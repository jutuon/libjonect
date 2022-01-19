/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Audio code.

mod pulseaudio;

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::io::AsyncReadExt;

use self::pulseaudio::PulseAudioThread;
use crate::connection::tcp::TcpSendHandle;
use super::message_router::MessageReceiver;
use super::message_router::RouterSender;
use crate::config::LogicConfig;
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
    PlayAudio {
        send_handle: TcpSendHandle,
    }
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
    async fn run(mut self) {

        // TODO: remove this?
        if self.config.connect_address.is_some() {
            loop {
                tokio::select! {
                    result = &mut self.quit_receiver => break result.unwrap(),
                    event = self.audio_receiver.recv() => {
                        if let AudioEvent::PlayAudio { send_handle } = event {
                            Self::handle_data_connection(self.quit_receiver, send_handle.tokio_tcp()).await;
                        }

                        return;
                    }
                }
            }

            return;
        }

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
                                        println!("Recording stream data speed: {} MiB/s", speed);
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
                            eprintln!("Data connection error: {}", e);
                            break;
                        }
                    }
                }
            }
        }

        quit_receiver.await.unwrap();
    }
}
