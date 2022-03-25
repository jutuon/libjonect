/* This Source Code Form is subject to the terms of the Mozilla Public
* License, v. 2.0. If a copy of the MPL was not distributed with this
* file, You can obtain one at https://mozilla.org/MPL/2.0/. */



use log::{error};

use std::{sync::{atomic::{Ordering, AtomicU32, AtomicBool}, mpsc::{Receiver}}, process::abort, io::ErrorKind};


use crate::{audio::oboe::AUDIO_BLOCK_SAMPLE_COUNT, connection::data::{DataReceiver, MAX_PACKET_SIZE}, config::{RAW_PCM_AUDIO_UDP_DATA_SIZE_IN_BYTES, RAW_PCM_AUDIO_UDP_DATA_SIZE_IN_SAMPLES, PCM_AUDIO_PACKET_SIZE_IN_BYTES}};

use super::{AudioDataBlock, AUDIO_CHANNEL_COUNT, OBOE_BUFFER_BURST_COUNT, cpp_bridge::{OBOE_CPP_IS_RUNNING, start_oboe_in_callback_mode, oboe_request_start, close_oboe, STATUS_ERROR, check_underruns}, OboeInfo};


static mut CALLBACK_STATE: Option<CallbackState> = None;

static RECV_BLOCKING_COUNT: AtomicU32 = AtomicU32::new(0);

static CALLBACK_ERROR_DETECTED: AtomicBool = AtomicBool::new(false);

enum BufferAndReceiver {
    AudioDataBlock(Receiver<AudioDataBlock>, AudioDataBlock),
    // Note that MAX_PACKET_SIZE % 2 != 0, but this should not be a problem as
    // audio data is 16-bit samples and sending code should send complete audio
    // frames.
    DirectReceiver {
        receiver: DataReceiver,
        conversion_buffer: [u8; PCM_AUDIO_PACKET_SIZE_IN_BYTES],
        buffer: [i16; RAW_PCM_AUDIO_UDP_DATA_SIZE_IN_SAMPLES],
        stream_started: bool,
    },
}

struct CallbackState {
    pcm_data_receiver: BufferAndReceiver,
    samples_written: usize,
}

impl CallbackState {
    fn new(pcm_data_receiver: PcmReceiver) -> Self {
        Self {
            pcm_data_receiver: pcm_data_receiver.with_buffer(),
            samples_written: 0,
        }
    }

    fn all_current_data_written(&self) -> bool {
        match self.pcm_data_receiver {
            BufferAndReceiver::AudioDataBlock(_, _) => self.samples_written == AUDIO_BLOCK_SAMPLE_COUNT,
            BufferAndReceiver::DirectReceiver{ .. } => self.samples_written == RAW_PCM_AUDIO_UDP_DATA_SIZE_IN_SAMPLES,
        }
    }

    /// Max length for returned sample is `sample_request`.
    fn get_new_data(&mut self, sample_request: usize) -> Result<&[i16], ()> {
        if self.all_current_data_written() {
            match &mut self.pcm_data_receiver {
                BufferAndReceiver::AudioDataBlock(receiver, buffer) => {
                    *buffer = match receiver.recv() {
                        Ok(data) => {
                            self.samples_written = 0;
                            data
                        },
                        Err(_) => {
                            return Err(());
                        }
                    };
                }
                BufferAndReceiver::DirectReceiver { receiver, conversion_buffer, buffer, stream_started } => {
                    let data = match receiver.recv_packet(conversion_buffer.as_mut_slice()) {
                        Ok(0) => {
                            return Err(());
                        }
                        Ok(size) => &conversion_buffer[..size],
                        Err(e) => {
                            match e.kind() {
                                ErrorKind::WouldBlock => {
                                    &[]
                                }
                                _ => {
                                    return Err(());
                                }
                            }
                        }
                    };

                    if data.is_empty() && !*stream_started {
                        // Buffer contains silence currently.
                    } else if data.is_empty() {
                        // This is probably a data sending bug.
                        return Err(());
                    } else {
                        if !*stream_started {
                            *stream_started = true;
                            match receiver.set_nonblocking(false) {
                                Ok(()) => (),
                                Err(_) => return Err(()),
                            }
                        }

                        // Direct mode is only enabled for reliable connections.

                        let (_packet_counter_bytes, audio_data) = data.split_at(4);

                        for (target, sample_bytes) in buffer.as_mut_slice().iter_mut().zip(audio_data.chunks_exact(2)) {
                            match sample_bytes.try_into() {
                                Ok(sample) => {
                                    *target = i16::from_le_bytes(sample);
                                }
                                Err(_) => {
                                    abort()
                                }
                            }
                        }
                    }

                    self.samples_written = 0;
                }
            }
        }

        let current_data = match &self.pcm_data_receiver {
            BufferAndReceiver::AudioDataBlock(_, buffer) => buffer.as_slice(),
            BufferAndReceiver::DirectReceiver { buffer, .. } => buffer.as_slice(),
        };

        let (_, unwritten_data) = current_data.split_at(self.samples_written);

        let max_sample_count = unwritten_data.len().min(sample_request);
        let (response_data, _) = unwritten_data.split_at(max_sample_count);

        self.samples_written += response_data.len();

        Ok(response_data)
    }
}

pub enum PcmReceiver {
    AudioDataBlock(Receiver<AudioDataBlock>),
    DirectReceiver(DataReceiver),
}

impl PcmReceiver {
    fn with_buffer(self) -> BufferAndReceiver {
        match self {
            PcmReceiver::AudioDataBlock(r) => BufferAndReceiver::AudioDataBlock(r, [0i16; AUDIO_BLOCK_SAMPLE_COUNT]),
            PcmReceiver::DirectReceiver(receiver) => BufferAndReceiver::DirectReceiver{
                receiver,
                conversion_buffer: [0; PCM_AUDIO_PACKET_SIZE_IN_BYTES],
                buffer: [0i16; RAW_PCM_AUDIO_UDP_DATA_SIZE_IN_SAMPLES],
                stream_started: false,
            },
        }
    }
}

pub struct OboeCppCallbackMode {
    previous_underrun_count: Option<i32>,
    previous_blocking_count: Option<u32>,
}

impl OboeCppCallbackMode {
    pub fn new(
        pcm_data_receiver: PcmReceiver,
        oboe_info: OboeInfo,
    ) -> Self {
        if OBOE_CPP_IS_RUNNING.swap(true, Ordering::SeqCst) {
            panic!("Only one OboeCppThread can be running at the same time.");
        }

        RECV_BLOCKING_COUNT.store(0, Ordering::Relaxed);
        CALLBACK_ERROR_DETECTED.store(false, Ordering::Relaxed);

        unsafe {
            CALLBACK_STATE = Some(CallbackState::new(pcm_data_receiver));

            start_oboe_in_callback_mode(
                write_data,
                oboe_info.sample_rate,
                oboe_info.frames_per_burst,
                OBOE_BUFFER_BURST_COUNT * oboe_info.frames_per_burst,
            );
        }

        Self {
            previous_underrun_count: Some(0),
            previous_blocking_count: Some(0),
        }
    }

    pub fn request_start(&mut self) {
        unsafe {
            oboe_request_start();
        }
    }

    pub fn quit(self) {
        unsafe {
            close_oboe();
        }
        OBOE_CPP_IS_RUNNING.store(false, Ordering::SeqCst);
    }

    pub fn check_underruns(&mut self) {
        check_underruns(&mut self.previous_underrun_count);

        let blocking_count = RECV_BLOCKING_COUNT.load(Ordering::Relaxed);

        if let Some(previous) = self.previous_blocking_count.as_mut() {
            if blocking_count > *previous {
                error!("No new data count: {}", blocking_count);
                *previous = blocking_count;
            }
        }
    }

    pub fn error_occurred(&self) -> bool {
        CALLBACK_ERROR_DETECTED.load(Ordering::Relaxed)
    }
}


/// This code runs in a high priority thread. It probably is not the thread
/// created in OboeCppThread.
extern "C" fn write_data(
    audio_data: *mut i16,
    num_frames: i32,
) -> i32 {
    let mut remaining_target_buffer: &mut [i16] = unsafe {
        std::slice::from_raw_parts_mut(audio_data, (num_frames * AUDIO_CHANNEL_COUNT as i32) as usize)
    };

    let callback_state = unsafe {
        match CALLBACK_STATE.as_mut() {
            Some(data) => data,
            None => {
                abort();
            }
        }
    };

    loop {
        let new_data = match callback_state.get_new_data(remaining_target_buffer.len()) {
            Ok(value) => value,
            Err(()) => {
                // Stop calling this callback.
                CALLBACK_ERROR_DETECTED.store(true, Ordering::Relaxed);
                return STATUS_ERROR;
            }
        };

        let (target, next_write_target) = remaining_target_buffer.split_at_mut(new_data.len());
        remaining_target_buffer = next_write_target;

        target.copy_from_slice(new_data);

        if remaining_target_buffer.is_empty() {
            // Continue playing audio.
            return 0;
        }
    }
}
