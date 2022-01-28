/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

mod cpp_bridge;

use log::{error, info, warn};


use std::{thread::JoinHandle, sync::{atomic::{AtomicBool, Ordering}, mpsc::{Sender, Receiver, SyncSender, self, TrySendError}}, io::Read, time::Duration};

use crate::connection::tcp::TcpSendHandle;

use self::cpp_bridge::OboeInfo;

use super::AudioEvent;


use cpp_bridge::OboeCppThread;

const OBOE_BUFFER_BURST_COUNT: i32 = 2;

const SAMPLE_BYTE_COUNT: usize = 2;
const AUDIO_CHANNEL_COUNT: usize = 2;
/// Frame is two samples currently.
const AUDIO_BLOCK_FRAME_COUNT: usize = 64;
const AUDIO_BLOCK_SAMPLE_COUNT: usize = AUDIO_BLOCK_FRAME_COUNT*AUDIO_CHANNEL_COUNT as usize;
const AUDIO_BLOCK_SIZE_IN_BYTES: usize = AUDIO_BLOCK_FRAME_COUNT*AUDIO_CHANNEL_COUNT*SAMPLE_BYTE_COUNT;
type AudioDataBlock = [i16; AUDIO_BLOCK_SAMPLE_COUNT];



pub enum OboeEvent {
    AudioEvent(AudioEvent),
    RequestQuit,
    BufferingDone {
        initial_buffer_frame_count: i32,
        initial_data: Vec<i16>,
    },
    DataReaderReadingError,
    DataReaderSendingError,
    OboeError(String),
    UnderrunCheckTimer,
}

pub struct OboeThread {
    handle: JoinHandle<()>,
    sender: Sender<OboeEvent>,
}

impl OboeThread {
    pub fn new() -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();

        let s = sender.clone();
        let handle = std::thread::spawn(move || {
            OboeLogic::new(s, receiver).run();
        });

        Self {
            handle,
            sender,
        }
    }

    pub fn send_event(&mut self, event: AudioEvent) {
        self.sender.send(OboeEvent::AudioEvent(event)).unwrap();
    }

    pub fn send_underrun_check_timer_tick(&mut self) {
        self.sender.send(OboeEvent::UnderrunCheckTimer).unwrap();
    }

    pub fn quit(self) {
        self.sender.send(OboeEvent::RequestQuit).unwrap();
        self.handle.join().unwrap();
    }
}

struct OboeLogic {
    sender: Sender<OboeEvent>,
    receiver: Receiver<OboeEvent>,
    data_reader_thread: Option<(DataReaderThread, OboeInfo)>,
    pcm_data_receiver: Option<Receiver<AudioDataBlock>>,
    oboe_cpp_thread: Option<OboeCppThread>,
}

impl OboeLogic {
    fn new(sender: Sender<OboeEvent>, receiver: Receiver<OboeEvent>) -> Self {
        Self {
            sender,
            receiver,
            data_reader_thread: None,
            pcm_data_receiver: None,
            oboe_cpp_thread: None,
        }
    }

    fn run(mut self) {
        loop {
            match self.receiver.recv().unwrap() {
                OboeEvent::RequestQuit => {
                    break;
                }
                OboeEvent::AudioEvent(audio_event) => {
                    self.handle_audio_event(audio_event)
                }
                OboeEvent::BufferingDone {
                    initial_buffer_frame_count,
                    initial_data,
                } => {
                    let handle = OboeCppThread::new(
                        self.pcm_data_receiver.take().unwrap(),
                        self.data_reader_thread.as_ref().unwrap().1.clone(),
                        self.sender.clone(),
                        initial_buffer_frame_count,
                        initial_data,
                    );
                    self.oboe_cpp_thread = Some(handle);
                }

                OboeEvent::OboeError(error) => {
                    error!("{}", error);

                    self.quit_data_reader_and_oboe();
                }
                OboeEvent::DataReaderSendingError => {
                    error!("OboeEvent::DataReaderSendingError");

                    self.quit_data_reader_and_oboe();
                }
                OboeEvent::DataReaderReadingError => {
                    error!("OboeEvent::DataReaderReadingError");

                    self.quit_data_reader_and_oboe();
                }
                OboeEvent::UnderrunCheckTimer => {
                    if let Some(oboe_cpp) = self.oboe_cpp_thread.as_mut() {
                        oboe_cpp.check_underruns();
                    }
                }
            }
        }

        self.quit_data_reader_and_oboe();
    }

    fn quit_data_reader_and_oboe(&mut self) {
        if let Some((thread, _)) = self.data_reader_thread.take() {
            thread.quit();
            info!("data_reader_thread quit");
            // TODO: Send data connection disconnect request?
        }

        if let Some(thread) = self.oboe_cpp_thread.take() {
            thread.quit();
            info!("oboe_cpp_thread quit");
        }
    }

    fn handle_audio_event(&mut self, audio_event: AudioEvent) {
        match audio_event {
            AudioEvent::PlayAudio {
                mut send_handle,
                decode_opus,
                frames_per_burst,
                sample_rate,
             } => {
                if let Err(e) = send_handle.set_blocking() {
                    error!("Error: {e}");
                    return;
                }

                // Buffer size is 64 KiB. TODO: update size comment?
                let (pcm_data_sender, pcm_data_receiver) = mpsc::sync_channel(1024);

                let data_reader = DataReaderThread::new(
                    self.sender.clone(),
                    send_handle,
                    pcm_data_sender,
                    decode_opus,
                    frames_per_burst,
                );

                let oboe_info = OboeInfo {
                    sample_rate: if decode_opus { 48000 } else { sample_rate },
                    frames_per_burst,
                };

                self.data_reader_thread = Some((data_reader, oboe_info));
                self.pcm_data_receiver = Some(pcm_data_receiver);
            }

            AudioEvent::StartRecording { .. } |
            AudioEvent::StopRecording |
            AudioEvent::Message(_)  => (),
        }
    }
}

static DATA_READER_THREAD_QUIT: AtomicBool = AtomicBool::new(false);
static DATA_READER_THREAD_IS_RUNNING: AtomicBool = AtomicBool::new(false);


struct DataReaderThread {
    handle: JoinHandle<()>,
}

impl DataReaderThread {
    fn new(
            sender: Sender<OboeEvent>,
            data: TcpSendHandle,
            pcm_data_queue: SyncSender<AudioDataBlock>,
            decode_opus: bool,
            frames_per_burst: i32,
        ) -> Self {
        if DATA_READER_THREAD_IS_RUNNING.swap(true, Ordering::SeqCst) {
            panic!("Only one data reader thread can be running at the same time.");
        }

        let handle = std::thread::spawn(move || {
            DATA_READER_THREAD_QUIT.store(false, Ordering::Relaxed);
            Self::data_reading_logic(sender, data, pcm_data_queue, decode_opus, frames_per_burst);
        });

        Self {
            handle,
        }
    }

    fn quit(self) {
        DATA_READER_THREAD_QUIT.store(true, Ordering::Relaxed);
        self.handle.join().unwrap();
        DATA_READER_THREAD_IS_RUNNING.store(false, Ordering::SeqCst);
    }

    fn data_reading_logic(
        sender: Sender<OboeEvent>,
        mut data_handle: TcpSendHandle,
        pcm_data_queue: SyncSender<AudioDataBlock>,
        decode_opus: bool,
        frames_per_burst: i32,
    ) {
        // TODO: Opus decoding.

        let mut buffer = [0u8; AUDIO_BLOCK_SIZE_IN_BYTES];
        let mut buffer_i16 = [0i16; AUDIO_BLOCK_SAMPLE_COUNT];

        let mut try_send_counter: u32 = 0;
        let mut try_send_warning = true;

        let initial_buffer_block_count: i32 = OBOE_BUFFER_BURST_COUNT*frames_per_burst/(AUDIO_BLOCK_FRAME_COUNT as i32);
        let initial_buffer_frame_count: i32 = initial_buffer_block_count*(AUDIO_BLOCK_FRAME_COUNT as i32);
        info!("Oboe audio: initial_buffer_block_count: {}", initial_buffer_block_count);
        info!("Oboe audio: initial_buffer_frame_count: {}", initial_buffer_frame_count);

        let mut buffering_counter: Option<i32> = Some(0);

        /*

        let mut initial_data: Vec<i16> = Vec::with_capacity((initial_buffer_frame_count*AUDIO_CHANNEL_COUNT as i32).try_into().unwrap());

        for _ in 0..initial_buffer_block_count  {
            // TODO: Timeout for reading?
            match data_handle.read_exact(&mut buffer) {
                Ok(_) => (),
                Err(e) => {
                    error!("Data socket reading error: {}", e);
                    sender.send(OboeEvent::DataReaderReadingError).unwrap();
                    return
                }
            }

            for sample_bytes in buffer.chunks_exact(2) {
                let sample = i16::from_le_bytes(sample_bytes.try_into().unwrap());
                initial_data.push(sample);
            }
        }

        sender.send(OboeEvent::BufferingDone {
            initial_buffer_frame_count,
            initial_data,
        }).unwrap();

        */

        loop {
            // TODO: Timeout for reading?
            match data_handle.read_exact(&mut buffer) {
                Ok(_) => (),
                Err(e) => {
                    error!("Data socket reading error: {}", e);
                    sender.send(OboeEvent::DataReaderReadingError).unwrap();
                    return
                }
            }

            for (sample_bytes, sample) in buffer.chunks_exact(2).zip(buffer_i16.iter_mut()) {
                *sample = i16::from_le_bytes(sample_bytes.try_into().unwrap());
            }

            loop {
                match pcm_data_queue.try_send(buffer_i16) {
                    Ok(()) => {
                        try_send_counter = 0;
                        break;
                    },
                    Err(TrySendError::Disconnected(_)) => {
                        panic!("TrySendError::Disconnected should not be possible.");
                    }
                    Err(TrySendError::Full(_)) => {
                        if try_send_warning {
                            warn!("TrySendError::Full");
                            try_send_warning = false;
                        }

                        std::thread::sleep(Duration::from_millis(1));
                        try_send_counter += 1;

                        if try_send_counter == 500 {
                            // Oboe audio stream is probably broken.
                            sender.send(OboeEvent::DataReaderSendingError).unwrap();
                        }
                    }
                }
            };

            if let Some(counter) = buffering_counter.as_mut() {
                *counter += 1;

                if *counter == initial_buffer_block_count {
                    sender.send(OboeEvent::BufferingDone {
                        initial_buffer_frame_count: 0,
                        initial_data: Vec::new(),
                    }).unwrap();

                    buffering_counter = None;
                }
            }

            if DATA_READER_THREAD_QUIT.load(Ordering::Relaxed) {
                return;
            }
        }
    }
}
