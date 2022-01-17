/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Audio code.

mod pulseaudio;

use std::sync::Arc;

use tokio::sync::oneshot;

use self::pulseaudio::PulseAudioThread;
use crate::device::data::TcpSendHandle;
use super::message_router::MessageReceiver;
use super::message_router::RouterSender;
use crate::config::ServerConfig;
use crate::utils::QuitReceiver;
use crate::utils::QuitSender;

pub use pulseaudio::EventToAudioServerSender;

/// Event to `AudioManager`.
#[derive(Debug)]
pub enum AudioEvent {
    Message(String),
    StopRecording,
    StartRecording {
        send_handle: TcpSendHandle,
        sample_rate: u32,
    },
}

/// Handle audio recording requests.
pub struct AudioManager {
    r_sender: RouterSender,
    quit_receiver: QuitReceiver,
    audio_receiver: MessageReceiver<AudioEvent>,
    config: Arc<ServerConfig>,
}

impl AudioManager {
    /// Start new `AudioManager` task.
    pub fn task(
        r_sender: RouterSender,
        audio_receiver: MessageReceiver<AudioEvent>,
        config: Arc<ServerConfig>,
    ) -> (tokio::task::JoinHandle<()>, QuitSender) {
        let (quit_sender, quit_receiver) = oneshot::channel();

        let audio_manager = Self {
            r_sender,
            audio_receiver,
            quit_receiver,
            config,
        };

        let task = async move {
            audio_manager.run().await;
        };

        let handle = tokio::spawn(task);

        (handle, quit_sender)
    }

    /// Run `AudioManager` logic.
    async fn run(mut self) {
        let mut at = PulseAudioThread::start(self.r_sender, self.config).await;

        loop {
            tokio::select! {
                result = &mut self.quit_receiver => break result.unwrap(),
                event = self.audio_receiver.recv() => {
                    at.send_event(event)
                }
            }
        }

        at.quit()
    }
}
