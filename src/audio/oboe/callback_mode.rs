/* This Source Code Form is subject to the terms of the Mozilla Public
* License, v. 2.0. If a copy of the MPL was not distributed with this
* file, You can obtain one at https://mozilla.org/MPL/2.0/. */



use log::{error};

use std::{sync::{atomic::{Ordering, AtomicU32}, mpsc::{Receiver}}, process::abort};


use crate::audio::oboe::AUDIO_BLOCK_SAMPLE_COUNT;

use super::{AudioDataBlock, AUDIO_CHANNEL_COUNT, OBOE_BUFFER_BURST_COUNT, cpp_bridge::{OBOE_CPP_IS_RUNNING, start_oboe_in_callback_mode, oboe_request_start, close_oboe, STATUS_ERROR, check_underruns}, OboeInfo};


static mut CALLBACK_STATE: Option<CallbackState> = None;

static RECV_BLOCKING_COUNT: AtomicU32 = AtomicU32::new(0);

struct CallbackState {
    pcm_data_receiver: Receiver<AudioDataBlock>,
    current_data: AudioDataBlock,
    samples_written: usize,
}

impl CallbackState {
    fn new(pcm_data_receiver: Receiver<AudioDataBlock>) -> Self {
        Self {
            pcm_data_receiver,
            current_data: [0i16; AUDIO_BLOCK_SAMPLE_COUNT],
            samples_written: 0,
        }
    }

    fn all_current_data_written(&self) -> bool {
        self.samples_written == AUDIO_BLOCK_SAMPLE_COUNT
    }

    /// Max length for returned sample is `sample_request`.
    fn get_new_data(&mut self, sample_request: usize) -> Result<&[i16], ()> {
        if self.all_current_data_written() {
            /*
            self.current_data = loop {
                match self.pcm_data_receiver.try_recv() {
                    Ok(data) => {
                        self.samples_written = 0;
                        break data;
                    },
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        RECV_BLOCKING_COUNT.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        return Err(());
                    }
                };
            }
            */

            self.current_data = match self.pcm_data_receiver.recv() {
                Ok(data) => {
                    self.samples_written = 0;
                    data
                },
                Err(_) => {
                    return Err(());
                }
            };
        }

        let (_, unwritten_data) = self.current_data.split_at(self.samples_written);

        let max_sample_count = unwritten_data.len().min(sample_request);
        let (response_data, _) = unwritten_data.split_at(max_sample_count);

        self.samples_written += response_data.len();

        Ok(response_data)
    }
}


pub struct OboeCppCallbackMode {
    previous_underrun_count: Option<i32>,
    previous_blocking_count: Option<u32>,
}

impl OboeCppCallbackMode {
    pub fn new(
        pcm_data_receiver: Receiver<AudioDataBlock>,
        oboe_info: OboeInfo,
    ) -> Self {
        if OBOE_CPP_IS_RUNNING.swap(true, Ordering::SeqCst) {
            panic!("Only one OboeCppThread can be running at the same time.");
        }

        RECV_BLOCKING_COUNT.store(0, Ordering::Relaxed);

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
