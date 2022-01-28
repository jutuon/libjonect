/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */


use log::{error, info};

use std::{sync::{atomic::{AtomicBool, Ordering, AtomicI32}, mpsc::{Receiver, Sender}, Mutex}, process::abort, thread::JoinHandle, ffi::CStr, vec, collections::VecDeque};

use lazy_static::lazy_static;

use crate::audio::oboe::AUDIO_BLOCK_SAMPLE_COUNT;

use super::{AudioDataBlock, AUDIO_BLOCK_FRAME_COUNT, OboeEvent, AUDIO_CHANNEL_COUNT, OBOE_BUFFER_BURST_COUNT};

type WriteDataCallback = extern "C" fn(
    audio_data: *mut i16,
    num_frames: i32,
    updateUnderrunCount: i32) -> u8;

type WaitQuitFunction = extern "C" fn();

type SendErrorFunction = extern "C" fn(error_message: *const std::os::raw::c_char);
type SendUnderrunCountErrorFunction = extern "C" fn(error: i32);


#[link(name = "jonect_android_cpp", kind = "dylib")]
extern "C" {
    fn oboe_cpp_code(
        callback: WriteDataCallback,
        wait_quit: WaitQuitFunction,
        send_error: SendErrorFunction,
        send_underrun_count_error: SendUnderrunCountErrorFunction,
        sample_rate: i32,
        frames_per_burst: i32,
        frames_per_data_callback: std::os::raw::c_int,
        buffer_capacity_in_frames: i32,
        initial_data: *const i16,
        initial_data_frame_count: i32,
    );
}

lazy_static! {
    static ref EVENT_SENDER: Mutex<Option<Sender<OboeEvent>>> = Mutex::new(None);
    static ref OBOE_QUIT_NOTIFICATION: Mutex<Option<Receiver<()>>> = Mutex::new(None);
}

static OBOE_CPP_IS_RUNNING: AtomicBool = AtomicBool::new(false);
static mut PCM_DATA_RECEIVER: Option<Receiver<AudioDataBlock>> = None;


#[derive(Debug, Clone)]
pub struct OboeInfo {
    pub sample_rate: i32,
    pub frames_per_burst: i32,
}

pub struct OboeCppThread {
    handle: JoinHandle<()>,
    quit_sender: Sender<()>,
    previous_underrun_count: Option<i32>,
}

impl OboeCppThread {
    pub fn new(
        pcm_receiver: Receiver<AudioDataBlock>,
        oboe_info: OboeInfo,
        sender: Sender<OboeEvent>,
        initial_buffer_frame_count: i32,
        initial_data: Vec<i16>,
    ) -> Self {
        if OBOE_CPP_IS_RUNNING.swap(true, Ordering::SeqCst) {
            panic!("Only one OboeCppThread can be running at the same time.");
        }

        let (quit_sender, quit_receiver) = std::sync::mpsc::channel();

        UNDERRUN_COUNT.store(0, Ordering::Relaxed);
        UNDERRUN_ERROR.store(0, Ordering::Relaxed);

        let mut lock = EVENT_SENDER.lock().unwrap();
        *lock = Some(sender);
        drop(lock);

        let mut lock = OBOE_QUIT_NOTIFICATION.lock().unwrap();
        *lock = Some(quit_receiver);
        drop(lock);

        unsafe {
            UNWRITTEN_AUDIO_DATA = Some(VecDeque::from([0; AUDIO_BLOCK_SAMPLE_COUNT]));
        }

        info!("initial_buffer_frame_count: {}, initial_data lenght: {}", initial_buffer_frame_count, initial_data.len());

        let handle = std::thread::spawn(move || {
            unsafe {
                PCM_DATA_RECEIVER = Some(pcm_receiver);
                oboe_cpp_code(
                    write_data,
                    wait_oboe_quit,
                    send_error,
                    send_underrun_error,
                    oboe_info.sample_rate,
                    oboe_info.frames_per_burst,
                    AUDIO_BLOCK_FRAME_COUNT.try_into().unwrap(),
                    OBOE_BUFFER_BURST_COUNT * oboe_info.frames_per_burst,
                    initial_data.as_ptr(),
                    initial_buffer_frame_count,
                );
            }

            drop(initial_data);
        });

        Self {
            handle,
            quit_sender,
            previous_underrun_count: Some(0),
        }
    }

    pub fn quit(self) {
        self.quit_sender.send(()).unwrap();
        self.handle.join().unwrap();
        OBOE_CPP_IS_RUNNING.store(false, Ordering::SeqCst);
    }

    pub fn check_underruns(&mut self) {
        let error = UNDERRUN_ERROR.load(Ordering::Relaxed);
        if error != 0 {
            if self.previous_underrun_count.take().is_some() {
                error!("Underrun check error: {}", error);
            }

            return;
        }

        let underrun_count = UNDERRUN_COUNT.load(Ordering::Relaxed);
        if let Some(previous_count) = self.previous_underrun_count.as_mut() {
            if underrun_count > *previous_count {
                error!("Underrun count: {}", underrun_count);
                *previous_count = underrun_count;
            }
        }
    }
}

pub static UNDERRUN_COUNT: AtomicI32 = AtomicI32::new(0);
pub static mut UNWRITTEN_AUDIO_DATA: Option<VecDeque<i16>> = None;

/// This code runs in a high priority thread. It probably is not the thread
/// created in OboeCppThread.
extern "C" fn write_data(
    audio_data: *mut i16,
    num_frames: i32,
    update_underrun_count: i32
) -> u8 {

    if update_underrun_count != 0 {
        UNDERRUN_COUNT.store(update_underrun_count, Ordering::Relaxed);
    }

    let receiver: &mut Receiver<AudioDataBlock> = unsafe {
        match PCM_DATA_RECEIVER.as_mut() {
            Some(receiver) => receiver,
            None => {
                abort();
            }
        }
    };

    let target_buffer: &mut [i16] = unsafe {
        std::slice::from_raw_parts_mut(audio_data, (num_frames * AUDIO_CHANNEL_COUNT as i32) as usize)
    };

    let mut remaining_target_buffer = target_buffer;

    let unwritten_data = unsafe {
        match UNWRITTEN_AUDIO_DATA.as_mut() {
            Some(data) => data,
            None => {
                abort();
            }
        }
    };

    if !unwritten_data.is_empty() {
        loop {
            match unwritten_data.pop_front() {
                Some(sample) => {
                    match remaining_target_buffer.split_first_mut() {
                        None => {
                            unwritten_data.push_front(sample);
                            // Continue playing audio.
                            return 0;
                        }
                        Some((target, next_target)) => {
                            remaining_target_buffer = next_target;
                            *target = sample;
                        }
                    }
                }
                None => {
                    if remaining_target_buffer.is_empty() {
                        // Continue playing audio.
                        return 0;
                    } else {
                        break;
                    }
                }
            }
        }
    }

    loop {
        let data = match receiver.recv() {
            Ok(data) => data,
            Err(_) => {
                // Stop calling this callback.
                return 1;
            }
        };

        let mut remaining_current_data = data.as_slice();

        loop {
            let split_at = remaining_current_data.len().min(remaining_target_buffer.len());

            if split_at == 0 {
                if remaining_target_buffer.is_empty() {
                    if !remaining_current_data.is_empty() {
                        for sample in remaining_current_data {
                            unwritten_data.push_back(*sample);
                        }
                    }

                    // Continue playing audio.
                    return 0;
                } else {
                    // Let's get new data for remaining_target_buffer.
                    break;
                }
            }

            let (first, second) = remaining_target_buffer.split_at_mut(split_at);
            remaining_target_buffer = second;

            let (first_data, second_data) = remaining_current_data.split_at(split_at);
            remaining_current_data = second_data;

            first.copy_from_slice(first_data);
        }

    }
}

extern "C" fn wait_oboe_quit() {
    let mut lock = match OBOE_QUIT_NOTIFICATION.lock() {
        Ok(lock) => lock,
        Err(_) => {
            abort();
        }
    };

    match lock.as_mut() {
        Some(receiver) => {
            match receiver.recv() {
                Ok(()) => (), // Quit
                Err(_) => {
                    abort();
                }
            }
        }
        None => {
            abort();
        }
    }
}

extern "C" fn send_error(message: *const std::os::raw::c_char) {
    let message = unsafe {
        CStr::from_ptr(message)
    };

    let message = message.to_string_lossy().to_string();

    let mut lock = match EVENT_SENDER.lock() {
        Ok(lock) => lock,
        Err(_) => {
            abort();
        }
    };

    match lock.as_mut() {
        Some(sender) => {
            match sender.send(OboeEvent::OboeError(message)) {
                Ok(()) => (),
                Err(_) => {
                    abort();
                }
            }
        }
        None => {
            abort();
        }
    }
}

pub static UNDERRUN_ERROR: AtomicI32 = AtomicI32::new(0);

extern "C" fn send_underrun_error(error: i32) {
    UNDERRUN_ERROR.store(error, Ordering::Relaxed);
}
