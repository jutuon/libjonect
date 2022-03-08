/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Audio code.

#[cfg(target_os = "linux")]
mod pulseaudio;

#[cfg(target_os = "android")]
mod oboe;

use log::debug;
use log::{info, error};

use std::io::ErrorKind;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use tokio::sync::oneshot;

use super::message_router::MessageReceiver;
use super::message_router::RouterSender;
use crate::config::LogicConfig;
use crate::connection::data::DataReceiverBuilder;
use crate::connection::data::DataSenderBuilder;
use crate::connection::data::MAX_PACKET_SIZE;
use crate::utils::QuitReceiver;
use crate::utils::QuitSender;


/// Event to `AudioManager`.
#[derive(Debug)]
pub enum AudioEvent {
    Message(String),
    StopRecording,
    StartRecording {
        send_handle: DataSenderBuilder,
        sample_rate: u32,
    },
    PlayAudio {
        send_handle: DataReceiverBuilder,
        sample_rate: i32,
        android_info: Option<PlayAudioEventAndroid>,
        decode_opus: bool,
    },
    StopPlayingAudio,
}

/// Android specific information for `AudioEvent::PlayAudio`.
#[derive(Debug, Clone, Copy)]
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
                            Self::handle_data_connection(self.quit_receiver, send_handle).await;
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
        mut connection: DataReceiverBuilder,
    ) {
        connection.set_timeout(Some(Duration::from_millis(1))).unwrap();
        let mut connection = connection.build();

        let mut timer = tokio::time::interval(Duration::from_millis(1));

        let mut buffer = vec![0; MAX_PACKET_SIZE];
        let mut bytes_per_second: u64 = 0;
        let mut data_count_time: Option<Instant> = None;

        let mut first_packet: bool = true;

        'main_loop: loop {
            tokio::select! {
                result = &mut quit_receiver => return result.unwrap(),
                _ = timer.tick() => {
                    'read_loop: loop {
                        match connection.recv_packet(&mut buffer) {
                            Ok(0) => break 'main_loop,
                            Ok(size) => {
                                let bytes = &buffer[..size];

                                // Minimum size should be 4 as packet counter is u32.
                                bytes_per_second += (size - 4) as u64;

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

                                if first_packet {
                                    first_packet = false;
                                    debug!("Audio packet number bytes: {:?}", &bytes[0..4]);
                                    debug!("Audio packet bytes: {:?}", &bytes[4..]);
                                }

                            }
                            Err(e) => {
                                match e.kind() {
                                    ErrorKind::WouldBlock => {
                                        break 'read_loop;
                                    }
                                    _ => {
                                        error!("Data connection error: {}", e);
                                        break 'main_loop;
                                    },
                                }
                            }
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
