/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use log::error;

use std::sync::atomic::AtomicBool;

/// Returns STATUS_ERROR or STATUS_OK.
type WriteDataCallback = extern "C" fn(
    audio_data: *mut i16,
    num_frames: i32) -> StatusValue;

type StatusValue = i32;
pub const STATUS_OK: i32 = 0;
pub const STATUS_ERROR: i32 = -1;

#[link(name = "jonect_android_cpp", kind = "dylib")]
extern "C" {
    pub fn start_oboe_in_callback_mode(
        callback: WriteDataCallback,
        sample_rate: i32,
        frames_per_burst: i32,
        buffer_capacity_in_frames: i32,
    ) -> StatusValue;

    pub fn start_oboe_in_write_mode(
        sample_rate: i32,
        frames_per_burst: i32,
        buffer_capacity_in_frames: i32,
    ) -> StatusValue;

    pub fn oboe_request_start();

    /// Returns STATUS_ERROR or count of frames which Oboe wrote to the current stream.
    pub fn oboe_write_data(
        data: *const i16,
        data_frame_count: i32,
    ) -> i32;

    /// Returns STATUS_ERROR or XRunCount which is not negative.
    pub fn oboe_get_x_run_count() -> i32;


    pub fn close_oboe();
}

/// Make sure that only one instance of Oboe C++ code is running.
pub static OBOE_CPP_IS_RUNNING: AtomicBool = AtomicBool::new(false);


pub fn check_underruns(previous_underrun_count: &mut Option<i32>) {
    let underrun_count = unsafe {
        oboe_get_x_run_count()
    };

    if underrun_count == STATUS_ERROR {
        if previous_underrun_count.take().is_some() {
            error!("Underrun check failed. It is now disabled.");
        }

        return;
    }

    if let Some(previous_count) = previous_underrun_count.as_mut() {
        if underrun_count > *previous_count {
            error!("Underrun count: {}", underrun_count);
            *previous_count = underrun_count;
        }
    }
}
