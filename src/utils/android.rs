/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use log::{info, error};

/// Set thread priority for current thread.
pub fn config_thread_priority(name: &str) {
    let result = unsafe {
        // Set thread priority for current thread. Currently on Linux
        // libc::setpriority will set thread nice value but this might
        // change in the future. Alternative would be sched_setattr system
        // call. Value of Android API constant Process.THREAD_PRIORITY_AUDIO
        // is -16 and Process.THREAD_PRIORITY_URGENT_AUDIO is -19.
        libc::setpriority(libc::PRIO_PROCESS, 0, -19)
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
