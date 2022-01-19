/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Connected device's state.

use std::{sync::Arc, time::Instant};

use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    config::{LogicConfig, EVENT_CHANNEL_SIZE},
    audio::AudioEvent,
    message_router::RouterSender,
    utils::{
        Connection, ConnectionEvent, ConnectionHandle, ConnectionId, QuitReceiver, QuitSender,
        SendDownward,
    }, connection::{JsonConnection, tcp::TcpSendHandle, ConnectionManagerEvent},
};

use super::{
    protocol::{
        AudioFormat, AudioStreamInfo, DeviceMessage, NativeSampleRate,
    },
    DeviceManagerInternalEvent,
};

/// Event to `DeviceStateTask`.
#[derive(Debug)]
pub enum DeviceEvent {
    NewDataConnection(TcpSendHandle),
    SendPing,
}

/// Handle to `DeviceStateTask`.
pub struct DeviceStateTaskHandle {
    task_handle: JoinHandle<()>,
    event_sender: SendDownward<DeviceEvent>,
    quit_sender: QuitSender,
}

impl DeviceStateTaskHandle {
    /// Send quit request to the `DeviceStateTask` and waits until it is closed.
    pub async fn quit(self) {
        self.quit_sender.send(()).unwrap();
        self.task_handle.await.unwrap();
    }

    /// Send `DeviceEvent` to the `DeviceStateTask`.
    pub async fn send(&mut self, event: DeviceEvent) {
        self.event_sender.send_down(event).await;
    }
}

/// Logic for handling one connected device.
pub struct DeviceStateTask {
    id: ConnectionId,
    connection_handle: ConnectionHandle<DeviceMessage>,
    ping_state: Option<Instant>,
    audio_out: Option<()>,
    r_sender: RouterSender,
    event_sender: mpsc::Sender<DeviceEvent>,
    event_receiver: mpsc::Receiver<DeviceEvent>,
    connection_receiver: mpsc::Receiver<ConnectionEvent<DeviceMessage>>,
    config: Arc<LogicConfig>,
    native_sample_rate: Option<i32>,
}

impl DeviceStateTask {
    /// Start new `DeviceStateTask`.
    pub async fn task(
        connection: JsonConnection,
        r_sender: RouterSender,
        config: Arc<LogicConfig>,
    ) -> DeviceStateTaskHandle {

        let (connection_sender, connection_receiver) =
            mpsc::channel::<ConnectionEvent<DeviceMessage>>(EVENT_CHANNEL_SIZE);

        let id = connection.id();
        let connection_handle: ConnectionHandle<DeviceMessage> =
            Connection::spawn_connection_task(connection, connection_sender.into());

        // TODO: This should be removed in the future?
        if config.enable_connection_listening {
            connection_handle.send_down(DeviceMessage::GetNativeSampleRate).await;
        }

        let (event_sender, event_receiver) = mpsc::channel::<DeviceEvent>(EVENT_CHANNEL_SIZE);

        let (quit_sender, quit_receiver) = oneshot::channel();

        let device_task = Self {
            id,
            connection_handle,
            r_sender,
            ping_state: None,
            audio_out: None,
            event_receiver,
            event_sender: event_sender.clone(),
            connection_receiver,
            config,
            native_sample_rate: None,
        };

        let task_handle = tokio::spawn(device_task.run(quit_receiver));

        DeviceStateTaskHandle {
            quit_sender,
            event_sender: event_sender.into(),
            task_handle,
        }
    }

    /// Run `DeviceStateTask`.
    pub async fn run(mut self, mut quit_receiver: QuitReceiver) {
        struct WaitQuit;

        let wait_quit: Option<WaitQuit> = loop {
            tokio::select! {
                result = &mut quit_receiver => {
                    result.unwrap();
                    break None;
                }
                event = self.connection_receiver.recv() => {
                    match event.unwrap() {
                        ConnectionEvent::ReadError(error) => {
                            eprintln!("Connection id {} read error {:?}", self.id, error);
                            break Some(WaitQuit);
                        }
                        ConnectionEvent::WriteError(error) => {
                            eprintln!("Connection id {} write error {:?}", self.id, error);
                            break Some(WaitQuit);
                        }
                        ConnectionEvent::Message(message) => {
                            tokio::select! {
                                result = &mut quit_receiver => {
                                    result.unwrap();
                                    break None;
                                }
                                _ = self.handle_client_message(message) => (),
                            }
                        }
                    }
                }
                event = self.event_receiver.recv() => {
                    tokio::select! {
                        result = &mut quit_receiver => {
                            result.unwrap();
                            break None;
                        }
                        _ = self.handle_device_event(event.unwrap()) => (),
                    }
                }
            }
        };

        if let Some(audio) = self.audio_out.take() {
            // TODO: remove this?,
            //audio.quit().await;
        }
        self.connection_handle.quit().await;

        if let Some(WaitQuit) = wait_quit {
            self.r_sender
                .send_dm_internal_event(
                    DeviceManagerInternalEvent::RemoveConnection(self.id).into(),
                )
                .await;

            quit_receiver.await.unwrap()
        }
    }

    /// Handle `DeviceEvent`.
    async fn handle_device_event(&mut self, event: DeviceEvent) {
        match event {
            DeviceEvent::NewDataConnection(send_handle) => {
                if self.audio_out.is_some() {
                    self.r_sender
                        .send_audio_server_event(AudioEvent::StartRecording {
                            send_handle,
                            sample_rate: self.native_sample_rate.unwrap() as u32,
                        })
                        .await;
                } else {
                    self.r_sender
                        .send_audio_server_event(AudioEvent::PlayAudio {
                            send_handle,
                        })
                        .await;
                }
            }
            DeviceEvent::SendPing => {
                if self.ping_state.is_none() {
                    self.connection_handle.send_down(DeviceMessage::Ping).await;
                    self.ping_state = Some(Instant::now());
                }
            }
        }
    }

    /// Handle received `DeviceMessage`.
    async fn handle_client_message(&mut self, message: DeviceMessage) {
        println!("Message: {:?}\n", message);
        match message {
            DeviceMessage::NativeSampleRate(NativeSampleRate { native_sample_rate }) => {
                if native_sample_rate == 0 {
                    // Audio playing is not supported.
                    return;
                }

                assert!(
                    native_sample_rate == 44100
                        || native_sample_rate == 48000
                );

                self.native_sample_rate = Some(native_sample_rate);

                let mut sample_rate = native_sample_rate as u32;

                let format = if self.config.encode_opus {
                    sample_rate = 48000;
                    AudioFormat::Opus
                } else {
                    AudioFormat::Pcm
                };

                let info = AudioStreamInfo::new(format, 2u8, sample_rate, 8082);

                self.connection_handle
                    .send_down(DeviceMessage::PlayAudioStream(info))
                    .await;

                self.audio_out = Some(());
            }
            DeviceMessage::GetNativeSampleRate => {
                self.connection_handle
                    .send_down(DeviceMessage::NativeSampleRate(NativeSampleRate::new(44100)))
                    .await;
            }
            DeviceMessage::Ping => {
                self.connection_handle
                    .send_down(DeviceMessage::PingResponse)
                    .await;
            }
            DeviceMessage::PingResponse => {
                if let Some(time) = self.ping_state.take() {
                    let time = Instant::now().duration_since(time).as_millis();
                    println!("Ping time {} ms", time);
                }
            }
            DeviceMessage::AudioStreamPlayError(error) => {
                eprintln!("AudioStreamPlayError {:?}", error);
            }
            DeviceMessage::PlayAudioStream(info) => {
                self.r_sender
                    .send_connection_manager_event(ConnectionManagerEvent::ConnectToData {
                        id: self.id
                    })
                    .await;
            }
        }
    }
}
