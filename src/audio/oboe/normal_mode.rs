/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */



use log::{error};

use core::panic;
use std::{sync::{atomic::{Ordering}}};


use crate::audio::oboe::{cpp_bridge::start_oboe_in_write_mode};

use super::{AUDIO_CHANNEL_COUNT, OBOE_BUFFER_BURST_COUNT, OboeInfo, cpp_bridge::{check_underruns, close_oboe, OBOE_CPP_IS_RUNNING, oboe_write_data, STATUS_ERROR, oboe_request_start}};

pub struct OboeCppNormalMode {
    previous_underrun_count: Option<i32>,
}

impl OboeCppNormalMode {
    pub fn new(
        oboe_info: OboeInfo,
    ) -> Self {
        if OBOE_CPP_IS_RUNNING.swap(true, Ordering::SeqCst) {
            panic!("Only one OboeCppThread can be running at the same time.");
        }

        unsafe {
            start_oboe_in_write_mode(
                oboe_info.sample_rate,
                oboe_info.frames_per_burst,
                OBOE_BUFFER_BURST_COUNT * oboe_info.frames_per_burst,
            );
        }

        Self {
            previous_underrun_count: Some(0),
        }
    }

    pub fn request_start(&mut self) {
        unsafe {
            oboe_request_start();
        }
    }

    pub fn write_data(&mut self, data: &[i16]) -> Result<(), ()> {
        let mut remaining_frames_count: i32 = (data.len()/AUDIO_CHANNEL_COUNT).try_into().unwrap();

        loop {
            let frame_count = unsafe {
                oboe_write_data(data.as_ptr(), remaining_frames_count)
            };

            if frame_count == STATUS_ERROR {
                return Err(());
            }

            if frame_count == 0 {
                error!("Error: oboe_write_data returned 0.");
                return Err(());
            }

            remaining_frames_count -= frame_count;

            if remaining_frames_count < 0 {
                panic!("remaining_frames_count < 0");
            }

            if remaining_frames_count == 0 {
                return Ok(())
            }
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
    }
}
