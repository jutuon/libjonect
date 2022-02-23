/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Audio code.

#[cfg(target_os = "linux")]
mod pulseaudio;

#[cfg(target_os = "android")]
mod oboe;

use log::{info, error};

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::io::AsyncReadExt;

use crate::connection::tcp::TcpSendHandle;
use super::message_router::MessageReceiver;
use super::message_router::RouterSender;
use crate::config::LogicConfig;
use crate::utils::QuitReceiver;
use crate::utils::QuitSender;


/// Event to `AudioManager`.
#[derive(Debug)]
pub enum AudioEvent {
    Message(String),
    StopRecording,
    StartRecording {
        send_handle: TcpSendHandle,
        sample_rate: u32,
    },
    PlayAudio {
        send_handle: TcpSendHandle,
        sample_rate: i32,
        android_info: Option<PlayAudioEventAndroid>,
        decode_opus: bool,
    }
}

/// Android specific information for `AudioEvent::PlayAudio`.
#[derive(Debug)]
pub struct PlayAudioEventAndroid {
    pub frames_per_burst: i32,
}

/// Handle audio recording requests.
pub struct AudioManager {
    r_sender: RouterSender,
    quit_receiver: QuitReceiver,
    audio_receiver: MessageReceiver<AudioEvent>,
    config: Arc<LogicConfig>,
}

impl AudioManager {
    /// Start new `AudioManager` task.
    pub fn task(
        r_sender: RouterSender,
        audio_receiver: MessageReceiver<AudioEvent>,
        config: Arc<LogicConfig>,
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
    #[cfg(target_os = "linux")]
    async fn run(mut self) {

        // TODO: remove this?
        if self.config.connect_address.is_some() {
            loop {
                tokio::select! {
                    result = &mut self.quit_receiver => break result.unwrap(),
                    event = self.audio_receiver.recv() => {
                        if let AudioEvent::PlayAudio { send_handle, .. } = event {
                            Self::handle_data_connection(self.quit_receiver, send_handle.tokio_tcp()).await;
                        }
                        // TODO: This does not work when device is disconnected
                        // and reconnected.
                        return;
                    }
                }
            }

            return;
        }

        let mut at = self::pulseaudio::PulseAudioThread::start(self.r_sender, self.config).await;

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

    /// Data connection handler task.
    async fn handle_data_connection(
        mut quit_receiver: oneshot::Receiver<()>,
        mut connection: TcpStream,
    ) {
        let mut buffer = [0; 1024];
        let mut bytes_per_second: u64 = 0;
        let mut data_count_time: Option<Instant> = None;

        loop {
            tokio::select! {
                result = &mut quit_receiver => return result.unwrap(),
                result = connection.read(&mut buffer) => {
                    match result {
                        Ok(0) => break,
                        Ok(size) => {
                            bytes_per_second += size as u64;

                            match data_count_time {
                                Some(time) => {
                                    let now = Instant::now();
                                    if now.duration_since(time) >= Duration::from_secs(1) {
                                        let speed = (bytes_per_second as f64) / 1024.0 / 1024.0;
                                        info!("Recording stream data speed: {} MiB/s", speed);
                                        bytes_per_second = 0;
                                        data_count_time = Some(Instant::now());
                                    }
                                }
                                None => {
                                    data_count_time = Some(Instant::now());
                                }
                            }
                        }
                        Err(e) => {
                            error!("Data connection error: {}", e);
                            break;
                        }
                    }
                }
            }
        }

        quit_receiver.await.unwrap();
    }

    /// Run `AudioManager` logic.
    #[cfg(target_os = "android")]
    async fn run(mut self) {
        let mut oboe_thread = self::oboe::OboeThread::new();

        let mut timer = tokio::time::interval(Duration::from_secs(1));

        loop {
            tokio::select! {
                result = &mut self.quit_receiver => break result.unwrap(),
                event = self.audio_receiver.recv() => {
                    oboe_thread.send_event(event)
                }
                event = timer.tick() => {
                    oboe_thread.send_underrun_check_timer_tick();
                }
            }
        }

        oboe_thread.quit()
    }

}
