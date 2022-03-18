/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

mod cpp_bridge;
pub mod callback_mode;
pub mod normal_mode;

use log::{error, info, warn, debug};


use std::{thread::JoinHandle, sync::{atomic::{AtomicBool, Ordering}, mpsc::{Sender, Receiver, SyncSender, self, TrySendError}, Arc}, io::{Read, ErrorKind}, time::Duration, num::Wrapping, collections::VecDeque};

#[derive(Debug, Clone)]
pub struct OboeInfo {
    pub sample_rate: i32,
    pub frames_per_burst: i32,
}

use crate::{connection::data::{DataReceiver, MAX_PACKET_SIZE}, config::LogicConfig};

use self::normal_mode::OboeCppNormalMode;

use super::AudioEvent;


use callback_mode::OboeCppCallbackMode;

const OBOE_BUFFER_BURST_COUNT: i32 = 8;

const SAMPLE_BYTE_COUNT: usize = 2;
const AUDIO_CHANNEL_COUNT: usize = 2;
/// Frame is two samples currently.
const AUDIO_BLOCK_FRAME_COUNT: usize = 8;
const AUDIO_BLOCK_SAMPLE_COUNT: usize = AUDIO_BLOCK_FRAME_COUNT*AUDIO_CHANNEL_COUNT as usize;
const AUDIO_BLOCK_SIZE_IN_BYTES: usize = AUDIO_BLOCK_FRAME_COUNT*AUDIO_CHANNEL_COUNT*SAMPLE_BYTE_COUNT;
type AudioDataBlock = [i16; AUDIO_BLOCK_SAMPLE_COUNT];



pub enum OboeEvent {
    AudioEvent(AudioEvent),
    RequestQuit,
    CallbackEvent(CallbackModeEvent),
    UnderrunCheckTimer,
}

pub enum CallbackModeEvent {
    BufferingDone,
    DataReaderReadingError,
    DataReaderSendingError,
}

pub struct OboeThread {
    handle: JoinHandle<()>,
    sender: Sender<OboeEvent>,
}

impl OboeThread {
    pub fn new(config: Arc<LogicConfig>) -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();

        let s = sender.clone();
        let handle = std::thread::spawn(move || {
            config_thread_priority("OboeThread");
            OboeLogic::new(s, receiver, config).run();
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

/// Normal mode does not work currently for some reason. Writing to the stream
/// does not work as zero is returned from the Oboe method.
pub struct NormalModeState {
    oboe_info: OboeInfo,
    pub normal_oboe: OboeCppNormalMode,
    decode_opus: bool,
    data_stream: DataReceiver,
    buffering_counter: Option<usize>,
    initial_buffer_block_count: usize,
    initial_buffer_frame_count: usize,
}

impl NormalModeState {
    fn new(
        oboe_info: OboeInfo,
        normal_oboe: OboeCppNormalMode,
        decode_opus: bool,
        data_stream: DataReceiver,
    ) -> Self {
        let initial_buffer_block_count: usize = (OBOE_BUFFER_BURST_COUNT*oboe_info.frames_per_burst/(AUDIO_BLOCK_FRAME_COUNT as i32)) as usize;
        let initial_buffer_frame_count: usize = initial_buffer_block_count*(AUDIO_BLOCK_FRAME_COUNT);
        info!("Oboe audio: initial_buffer_block_count: {}", initial_buffer_block_count);
        info!("Oboe audio: initial_buffer_frame_count: {}", initial_buffer_frame_count);

        Self {
            oboe_info,
            normal_oboe,
            decode_opus,
            data_stream,
            buffering_counter: None,
            initial_buffer_block_count,
            initial_buffer_frame_count,
        }
    }

    fn handle_data_reading_and_writing(&mut self) -> Result<(), ()> {

        unimplemented!();

        /*
        if self.buffering_counter.is_some() {
            let mut buffer = vec![0u8; AUDIO_CHANNEL_COUNT * SAMPLE_BYTE_COUNT * self.oboe_info.frames_per_burst as usize];
            let mut buffer_i16 = vec![0i16; AUDIO_CHANNEL_COUNT * self.oboe_info.frames_per_burst as usize];

            // TODO: Timeout for reading?
            match self.data_stream.read_exact(&mut buffer) {
                Ok(_) => (),
                Err(e) => {
                    error!("Data socket reading error: {}", e);
                    return Err(());
                }
            }

            for (sample_bytes, sample) in buffer.chunks_exact(2).zip(buffer_i16.iter_mut()) {
                *sample = i16::from_le_bytes(sample_bytes.try_into().unwrap());
            }

            self.normal_oboe.write_data(&buffer_i16)?;
            self.normal_oboe.request_start();
            self.buffering_counter.take();

            Ok(())
        } else {
            const FRAME_COUNT: usize = 32;
            let mut buffer = [0u8; AUDIO_CHANNEL_COUNT * SAMPLE_BYTE_COUNT * FRAME_COUNT];
            let mut buffer_i16 = [0i16; AUDIO_CHANNEL_COUNT * FRAME_COUNT];


            // TODO: Timeout for reading?
            match self.data_stream.read_exact(&mut buffer) {
                Ok(_) => (),
                Err(e) => {
                    error!("Data socket reading error: {}", e);
                    return Err(());
                }
            }

            for (sample_bytes, sample) in buffer.chunks_exact(2).zip(buffer_i16.iter_mut()) {
                *sample = i16::from_le_bytes(sample_bytes.try_into().unwrap());
            }

            self.normal_oboe.write_data(&buffer_i16)?;

            if let Some(counter) = self.buffering_counter.as_mut() {
                *counter += FRAME_COUNT;

                if *counter >= self.initial_buffer_frame_count {
                    self.buffering_counter = None;
                    //self.normal_oboe.request_start();
                }
            }

            Ok(())
        }
     */
    }

}

pub enum OboeMode {
    Callback {
        oboe_info: OboeInfo,
        data_reader_thread: DataReaderThread,
        callback_oboe: OboeCppCallbackMode,
    },
    Normal {
        state: NormalModeState,
    }
}

impl OboeMode {
    fn quit(self) {
        match self {
            OboeMode::Callback {
                data_reader_thread,
                callback_oboe,
                ..
            } => {
                data_reader_thread.quit();
                info!("data_reader_thread quit");
                // TODO: Send data connection disconnect request?

                callback_oboe.quit();
            }
            OboeMode::Normal {
                state,
                ..
            } => {
                state.normal_oboe.quit();
            }
        }
    }

    fn request_start(&mut self) {
        match self {
            OboeMode::Callback { callback_oboe, ..} => {
                callback_oboe.request_start();
            }
            OboeMode::Normal { state, .. } => {
                state.normal_oboe.request_start();
            }
        }
    }

    fn check_underruns(&mut self) {
        match self {
            OboeMode::Callback { callback_oboe, ..} => {
                callback_oboe.check_underruns();
            }
            OboeMode::Normal { state, .. } => {
                state.normal_oboe.check_underruns();
            }
        }
    }
}

struct StopAudio;

struct OboeLogic {
    sender: Sender<OboeEvent>,
    receiver: Receiver<OboeEvent>,
    oboe_mode: Option<OboeMode>,
    config: Arc<LogicConfig>,
}

impl OboeLogic {
    fn new(sender: Sender<OboeEvent>, receiver: Receiver<OboeEvent>, config: Arc<LogicConfig>) -> Self {
        Self {
            sender,
            receiver,
            oboe_mode: None,
            config,
        }
    }

    fn run(mut self) {
        loop {
            let event = match self.oboe_mode.as_mut() {
                Some(OboeMode::Normal {
                    state,
                    ..
                }) => {
                    if state.handle_data_reading_and_writing().is_err() {
                        self.quit_data_reader_and_oboe();
                    }

                    match self.receiver.try_recv() {
                        Ok(event) => event,
                        Err(mpsc::TryRecvError::Empty) => continue,
                        Err(mpsc::TryRecvError::Disconnected) => panic!("TryRecvError::Disconnected"),
                    }
                }
                _ => self.receiver.recv().unwrap(),
            };

            match event {
                OboeEvent::RequestQuit => {
                    break;
                }
                OboeEvent::AudioEvent(audio_event) => {
                    if let Some(StopAudio) = self.handle_audio_event(audio_event) {
                        self.quit_data_reader_and_oboe();
                    }
                }
                OboeEvent::CallbackEvent(event) => {
                    match event {
                        CallbackModeEvent::DataReaderSendingError => {
                            error!("OboeEvent::DataReaderSendingError");

                            self.quit_data_reader_and_oboe();
                        }
                        CallbackModeEvent::DataReaderReadingError => {
                            error!("OboeEvent::DataReaderReadingError");

                            self.quit_data_reader_and_oboe();
                        }
                        CallbackModeEvent::BufferingDone => {
                            if let Some(oboe) = self.oboe_mode.as_mut() {
                                oboe.request_start();
                            }
                        }
                    }
                }

                OboeEvent::UnderrunCheckTimer => {
                    if let Some(oboe) = self.oboe_mode.as_mut() {
                        oboe.check_underruns();
                    }
                }
            }
        }

        self.quit_data_reader_and_oboe();
    }

    fn quit_data_reader_and_oboe(&mut self) {
        if let Some(mode) = self.oboe_mode.take() {
            mode.quit();
        }
    }

    fn handle_audio_event(&mut self, audio_event: AudioEvent) -> Option<StopAudio> {
        match audio_event {
            AudioEvent::PlayAudio {
                mut send_handle,
                decode_opus,
                sample_rate,
                android_info,
             } => {
                // TODO: Opus decoding.
                assert!(!decode_opus);

                let frames_per_burst = android_info.unwrap().frames_per_burst;

                if self.oboe_mode.is_some() {
                    error!("Can not play multiple audio streams simultaneously.");
                    return None;
                }

                if let Err(e) = send_handle.set_timeout(Some(Duration::from_millis(10))) {
                    error!("Error: {e}");
                    return None;
                }

                let send_handle = send_handle.build();

                let oboe_info = OboeInfo {
                    sample_rate: if decode_opus { 48000 } else { sample_rate },
                    frames_per_burst,
                };

                let callback_mode = true;

                if callback_mode {
                    // Buffer size is 64 KiB. TODO: update size comment?
                    let (pcm_data_sender, pcm_data_receiver) = mpsc::sync_channel(1024);

                    let data_reader_thread = DataReaderThread::new(
                        self.sender.clone(),
                        send_handle,
                        pcm_data_sender,
                        decode_opus,
                        frames_per_burst,
                        self.config.clone(),
                    );

                    let callback_oboe = OboeCppCallbackMode::new(pcm_data_receiver, oboe_info.clone());

                    self.oboe_mode = Some(OboeMode::Callback {
                        callback_oboe,
                        data_reader_thread,
                        oboe_info,
                    });

                } else {
                    let normal_oboe = OboeCppNormalMode::new(oboe_info.clone());

                    self.oboe_mode = Some(OboeMode::Normal {
                        state: NormalModeState::new(oboe_info, normal_oboe, decode_opus, send_handle),
                    });
                }

                None
            }
            AudioEvent::StopPlayingAudio => {
                Some(StopAudio)
            }
            AudioEvent::StartRecording { .. } |
            AudioEvent::StopRecording |
            AudioEvent::Message(_)  => None,
        }
    }
}

static DATA_READER_THREAD_QUIT: AtomicBool = AtomicBool::new(false);
static DATA_READER_THREAD_IS_RUNNING: AtomicBool = AtomicBool::new(false);


pub struct DataReaderThread {
    handle: JoinHandle<()>,
    config: Arc<LogicConfig>,
}

impl DataReaderThread {
    fn new(
            sender: Sender<OboeEvent>,
            data: DataReceiver,
            pcm_data_queue: SyncSender<AudioDataBlock>,
            decode_opus: bool,
            frames_per_burst: i32,
            config: Arc<LogicConfig>,
        ) -> Self {
        if DATA_READER_THREAD_IS_RUNNING.swap(true, Ordering::SeqCst) {
            panic!("Only one data reader thread can be running at the same time.");
        }

        let print_audio_packet = config.print_first_audio_packet_bytes;
        let handle = std::thread::spawn(move || {
            config_thread_priority("DataReaderThread");
            DATA_READER_THREAD_QUIT.store(false, Ordering::Relaxed);

            let mut packet_loss_counter: u64 = 0;
            Self::data_reading_logic(sender, data, pcm_data_queue, decode_opus, frames_per_burst, &mut packet_loss_counter, print_audio_packet);
            info!("Packet loss counter: {packet_loss_counter}");
        });

        Self {
            handle,
            config,
        }
    }

    fn quit(self) {
        DATA_READER_THREAD_QUIT.store(true, Ordering::Relaxed);
        self.handle.join().unwrap();
        DATA_READER_THREAD_IS_RUNNING.store(false, Ordering::SeqCst);
    }

    fn data_reading_logic(
        sender: Sender<OboeEvent>,
        mut data_handle: DataReceiver,
        pcm_data_queue: SyncSender<AudioDataBlock>,
        decode_opus: bool,
        frames_per_burst: i32,
        packet_loss_counter: &mut u64,
        print_first_audio_packet: bool,
    ) {
        // TODO: Opus decoding.

        let mut packet_buffer = vec![0u8; MAX_PACKET_SIZE];
        let mut audio_packet_manager = AudioPacketManager::new();

        let mut audio_data_buffer: VecDeque<i16> = VecDeque::with_capacity(AUDIO_BLOCK_SIZE_IN_BYTES*8);

        let mut buffer_i16 = [0i16; AUDIO_BLOCK_SAMPLE_COUNT];

        let mut try_send_counter: u32 = 0;
        let mut try_send_warning = true;

        let initial_buffer_block_count: i32 = OBOE_BUFFER_BURST_COUNT*frames_per_burst/(AUDIO_BLOCK_FRAME_COUNT as i32);
        let initial_buffer_frame_count: i32 = initial_buffer_block_count*(AUDIO_BLOCK_FRAME_COUNT as i32);
        info!("Oboe audio: initial_buffer_block_count: {}", initial_buffer_block_count);
        info!("Oboe audio: initial_buffer_frame_count: {}", initial_buffer_frame_count);

        let mut buffering_counter: Option<i32> = Some(0);

        loop {
            let packet_data: &[u8] = match data_handle.recv_packet(&mut packet_buffer) {
                Ok(0) => {
                    sender.send(OboeEvent::CallbackEvent(CallbackModeEvent::DataReaderReadingError)).unwrap();
                    error!("Data receiving error: disconnected");
                    Self::wait_quit();
                    return;
                }
                Ok(size) => &packet_buffer[..size],
                Err(e) => {
                    match e.kind() {
                        ErrorKind::WouldBlock => {
                            if DATA_READER_THREAD_QUIT.load(Ordering::Relaxed) {
                                return;
                            }

                            continue;
                        }
                        _ => {
                            sender.send(OboeEvent::CallbackEvent(CallbackModeEvent::DataReaderReadingError)).unwrap();
                            error!("Data receiving error: {e}");
                            Self::wait_quit();
                            return;
                        }
                    }
                }
            };

            let (packet_counter_bytes, audio_data) = packet_data.split_at(4);
            let packet_counter = u32::from_be_bytes(packet_counter_bytes.try_into().unwrap());

            if print_first_audio_packet && packet_counter == 0 {
                debug!("Packet number bytes: {packet_counter_bytes:?}");
                debug!("Audio bytes: {audio_data:?}");
            }

            if !data_handle.is_reliable_connection() {
                // TODO: Fix check_packet. There is some bug at least with USB
                // accessory mode.
                match audio_packet_manager.check_packet(packet_counter) {
                    PacketStatus::Normal => (),
                    PacketStatus::Discard => continue,
                    PacketStatus::PacketLoss(packet_loss) => {
                        *packet_loss_counter += packet_loss as u64;
                        info!("Packet loss counter: {packet_loss_counter}");
                        // TODO: Handle packet loss.
                    }
                }
            }

            assert!(audio_data.len() % 2 == 0);

            audio_data_buffer.extend(audio_data.chunks_exact(2).map(|sample_bytes| {
                i16::from_le_bytes(sample_bytes.try_into().unwrap())
            }));

            while audio_data_buffer.len() >= AUDIO_BLOCK_SAMPLE_COUNT {
                for target in buffer_i16.iter_mut() {
                    *target = audio_data_buffer.pop_front().unwrap();
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
                                // Oboe audio stream is probably broken. Quit
                                // data reading using event.
                                sender.send(OboeEvent::CallbackEvent(CallbackModeEvent::DataReaderSendingError)).unwrap();

                                // Read packets and wait quit.
                                Self::read_packets_and_wait_quit(data_handle);
                                return
                            }
                        }
                    }
                }

                if let Some(counter) = buffering_counter.as_mut() {
                    *counter += 1;

                    if *counter == initial_buffer_block_count {
                        sender.send(OboeEvent::CallbackEvent(CallbackModeEvent::BufferingDone)).unwrap();

                        buffering_counter = None;
                    }
                }

                if DATA_READER_THREAD_QUIT.load(Ordering::Relaxed) {
                    return;
                }
            }
        }
    }

    fn wait_quit() {
        loop {
            std::thread::sleep(Duration::from_millis(1));
            if DATA_READER_THREAD_QUIT.load(Ordering::Relaxed) {
                return;
            }
        }
    }

    fn read_packets_and_wait_quit(
        mut data_handle: DataReceiver,
    ) {
        loop {
            std::thread::sleep(Duration::from_millis(1));

            let mut buffer = [0];
            match data_handle.recv_packet(&mut buffer) {
                Ok(0) => {
                    error!("Data receiving error: disconnected");
                    break;
                }
                Ok(_) => (),
                Err(e) => {
                    error!("Data receiving error: {e}");
                    break;
                }
            };

            if DATA_READER_THREAD_QUIT.load(Ordering::Relaxed) {
                return;
            }
        }

        Self::wait_quit();
    }
}

/// Set thread priority for current thread.
fn config_thread_priority(name: &str) {
    let result = unsafe {
        // Set thread priority for current thread. Currently on Linux
        // libc::setpriority will set thread nice value but this might
        // change in the future. Alternative would be sched_setattr system
        // call. Value of Android API constant Process.THREAD_PRIORITY_AUDIO
        // is -16.
        libc::setpriority(libc::PRIO_PROCESS, 0, -16)
    };

    if result == -1 {
        error!("Setting thread priority failed.");
    }

    let get_result = unsafe {
        libc::getpriority(libc::PRIO_PROCESS, 0)
    };

    if get_result == -1 {
        error!("libc::getpriority returned -1 which might be error or not.");
    } else {
        info!("Thread priority for thread '{}' is now {}.", name, get_result);
    }
}

#[derive(Debug, PartialEq)]
enum PacketStatus {
    Normal,
    PacketLoss(u32),
    Discard,
}

struct AudioPacketManager {
    expected_next_packet_count: u32,
    expected_next_counter_wrap_byte: bool,
}

impl AudioPacketManager {
    const WRAP_BYTE_MASK: u32 = 0b1000_0000_0000_0000;
    const MAX_COUNT: u32 = !Self::WRAP_BYTE_MASK;

    fn new() -> Self {
        Self {
            expected_next_packet_count: 0,
            expected_next_counter_wrap_byte: false,
        }
    }

    fn start_from(count: u32) -> Self {
        Self {
            expected_next_packet_count: count,
            expected_next_counter_wrap_byte: false,
        }
    }

    fn check_packet(&mut self, count: u32) -> PacketStatus {
        let wrap = count & Self::WRAP_BYTE_MASK != 0;
        let count = count & !Self::WRAP_BYTE_MASK;

        if self.expected_next_counter_wrap_byte == wrap {

            match count.cmp(&self.expected_next_packet_count) {
                std::cmp::Ordering::Equal => {
                    let next = count + 1;

                    let (expected_next_packet_count, expected_next_counter_wrap_byte) = if next == Self::WRAP_BYTE_MASK {
                        (0, !self.expected_next_counter_wrap_byte)
                    } else {
                        (next, self.expected_next_counter_wrap_byte)
                    };

                    self.expected_next_packet_count = expected_next_packet_count;
                    self.expected_next_counter_wrap_byte = expected_next_counter_wrap_byte;

                    PacketStatus::Normal
                }
                std::cmp::Ordering::Less => {
                    PacketStatus::Discard
                }
                std::cmp::Ordering::Greater => {
                    let packet_loss = count - self.expected_next_packet_count;
                    let next = count + 1;

                    let (expected_next_packet_count, expected_next_counter_wrap_byte) = if next == Self::WRAP_BYTE_MASK {
                        (0, !self.expected_next_counter_wrap_byte)
                    } else {
                        (next, self.expected_next_counter_wrap_byte)
                    };

                    self.expected_next_packet_count = expected_next_packet_count;
                    self.expected_next_counter_wrap_byte = expected_next_counter_wrap_byte;

                    PacketStatus::PacketLoss(packet_loss)
                }
            }
        } else {
            match count.cmp(&self.expected_next_packet_count) {
                std::cmp::Ordering::Less => {
                    let packet_loss = Self::MAX_COUNT - self.expected_next_packet_count + count;

                    self.expected_next_packet_count = count + 1;
                    self.expected_next_counter_wrap_byte = wrap;

                    PacketStatus::PacketLoss(packet_loss)
                }
                std::cmp::Ordering::Equal | std::cmp::Ordering::Greater => {
                    // Just assume that this is old packet.
                    PacketStatus::Discard
                }
            }
        }
    }
}

// TODO: Add more tests and check that current tests pass.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_packet_loss_1() {
        assert_eq!(AudioPacketManager::new().check_packet(0), PacketStatus::Normal);
    }

    #[test]
    fn test_packet_loss_1() {
        assert_eq!(AudioPacketManager::new().check_packet(1), PacketStatus::PacketLoss(1));
    }

    #[test]
    fn test_packet_loss_2() {
        assert_eq!(AudioPacketManager::new().check_packet(2), PacketStatus::PacketLoss(2));
    }

    #[test]
    fn test_discard_1() {
        assert_eq!(AudioPacketManager::new().check_packet(1), PacketStatus::PacketLoss(1));
    }
}
