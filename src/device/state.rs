/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Connected device's state.

use log::{info, error, debug};

use std::{sync::Arc, time::Instant};

use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    config::{LogicConfig, EVENT_CHANNEL_SIZE},
    audio::{AudioEvent, PlayAudioEventAndroid},
    message_router::RouterSender,
    utils::{
        Connection, ConnectionEvent, ConnectionHandle, ConnectionId, QuitReceiver, QuitSender,
        SendDownward,
    }, connection::{JsonConnection, ConnectionManagerEvent, DataConnectionType, NextResourceRequest}, ui::{ValueRequest, UiEvent, AndroidAudioInfo},
};

use super::{
    protocol::{
        AudioFormat, AudioStreamInfo, DeviceMessage, NativeSampleRate, UnsupportedFormat,
    },
    DeviceManagerInternalEvent,
};

/// Event to `DeviceStateTask`.
#[derive(Debug)]
pub enum DeviceEvent {
    SendPing,
    UiNativeSampleRate(AndroidAudioInfo),
    Disconnect,
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

enum QuitMode {
    QuitRequest,
    Disconnect,
    ConnectionError,
}

enum AudioInfoRequestMessages {
    Device(DeviceMessage),
    Connect(ConnectionManagerEvent),
}

impl AudioInfoRequestMessages {
    async fn handle(
        self,
        connection_handle: &mut ConnectionHandle<DeviceMessage>,
        r_sender: &mut RouterSender,
    ) {
        match self {
            AudioInfoRequestMessages::Connect(m) => {
                r_sender.send_connection_manager_event(m).await;
            }
            AudioInfoRequestMessages::Device(m) => {
                connection_handle.send_down(m).await;
            }
        }
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
    recording_native_sample_rate: Option<i32>,
    android_audio_info: ValueRequest<AndroidAudioInfo, AudioInfoRequestMessages>,
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
            recording_native_sample_rate: None,
            android_audio_info: ValueRequest::new(),
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
        let logic = async {
            self.r_sender.send_ui_event(UiEvent::ConnectionEstablished { connection_id: self.id }).await;

            loop {
                tokio::select! {
                    event = self.connection_receiver.recv() => {
                        match event.unwrap() {
                            ConnectionEvent::ReadError(error) => {
                                error!("Connection id {} read error {:?}", self.id, error);
                                break QuitMode::ConnectionError;
                            }
                            ConnectionEvent::WriteError(error) => {
                                error!("Connection id {} write error {:?}", self.id, error);
                                break QuitMode::ConnectionError;
                            }
                            ConnectionEvent::Message(message) => {
                                self.handle_client_message(message).await;
                            }
                        }
                    }
                    event = self.event_receiver.recv() => {
                        if let Some(quit_mode) = self.handle_device_event(event.unwrap()).await {
                            break quit_mode;
                        }
                    }
                }
            }
        };

        let quit_mode = tokio::select! {
            result = &mut quit_receiver => {
                result.unwrap();
                QuitMode::QuitRequest
            }
            quit_mode = logic => quit_mode,
        };

        if let Some(audio) = self.audio_out.take() {
            // TODO: remove this?,
            //audio.quit().await;
        }
        self.connection_handle.quit().await;

        if let QuitMode::ConnectionError = quit_mode {
            self.r_sender.send_ui_event(UiEvent::ConnectionError { connection_id: self.id }).await;
        }

        if let QuitMode::ConnectionError | QuitMode::Disconnect = quit_mode {
            self.r_sender
                .send_dm_internal_event(
                    DeviceManagerInternalEvent::RemoveConnection(self.id).into(),
                )
                .await;

            quit_receiver.await.unwrap()
        }
    }

    /// Handle `DeviceEvent`.
    async fn handle_device_event(&mut self, event: DeviceEvent) -> Option<QuitMode> {
        match event {
            DeviceEvent::SendPing => {
                if self.ping_state.is_none() {
                    self.connection_handle.send_down(DeviceMessage::Ping).await;
                    self.ping_state = Some(Instant::now());
                }
            }
            DeviceEvent::UiNativeSampleRate(native_sample_rate) => {
                for message in self.android_audio_info.set_value(native_sample_rate) {
                    message.handle(&mut self.connection_handle, &mut self.r_sender).await;
                }
            }
            DeviceEvent::Disconnect => {
                return Some(QuitMode::Disconnect);
            }
        }

        None
    }

    /// Handle received `DeviceMessage`.
    async fn handle_client_message(&mut self, message: DeviceMessage) {
        debug!("Message: {:?}\n", message);
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

                self.recording_native_sample_rate = Some(native_sample_rate);

                let mut sample_rate = native_sample_rate as u32;

                let format = if self.config.encode_opus {
                    sample_rate = 48000;
                    AudioFormat::Opus
                } else {
                    AudioFormat::Pcm
                };

                let info = AudioStreamInfo::new(format, 2u8, sample_rate, 8082, DataConnectionType::Tcp);

                self.r_sender.send_connection_manager_event(ConnectionManagerEvent::SetNextResourceRequest {
                    id: self.id,
                    next_resource_request: NextResourceRequest::SendAudio {
                        sample_rate: self.recording_native_sample_rate.unwrap() as u32,
                    },
                    data_connection_type: DataConnectionType::Tcp,
                }).await;

                self.connection_handle
                    .send_down(DeviceMessage::PlayAudioStream(info))
                    .await;

                self.audio_out = Some(());
            }
            DeviceMessage::GetNativeSampleRate => {
                if cfg!(target_os = "android") {
                    let message = self.android_audio_info.request(|value| {
                            let m = NativeSampleRate::new(value.native_sample_rate);
                            let m = DeviceMessage::NativeSampleRate(m);
                            AudioInfoRequestMessages::Device(m)
                        }
                    );

                    if let Some(message) = message {
                        message.handle(&mut self.connection_handle, &mut self.r_sender).await;
                    } else {
                        self.r_sender.send_ui_event(crate::ui::UiEvent::GetNativeSampleRate {
                            who_sent_this: self.id,
                        }).await;
                    }
                } else {
                    // TODO: Get real NativeSampleRate from PulseAudio.
                    let m = NativeSampleRate::new(48000);
                    let m = DeviceMessage::NativeSampleRate(m);
                    let m = AudioInfoRequestMessages::Device(m);
                    m.handle(&mut self.connection_handle, &mut self.r_sender).await;
                }
            }
            DeviceMessage::Ping => {
                self.connection_handle
                    .send_down(DeviceMessage::PingResponse)
                    .await;
            }
            DeviceMessage::PingResponse => {
                if let Some(time) = self.ping_state.take() {
                    let time = Instant::now().duration_since(time).as_millis();
                    info!("Ping time {} ms", time);
                }
            }
            DeviceMessage::AudioStreamPlayError(error) => {
                error!("AudioStreamPlayError {:?}", error);
            }
            DeviceMessage::PlayAudioStream(info) => {
                let id = self.id;

                if info.channels != 2 {
                    error!("Non stereo audio streams are not supported.");
                    return;
                }

                match info.rate {
                    44100 | 48000 => (),
                    rate => {
                        error!("Unsupported audio stream sample rate {}.", rate);
                        return;
                    },
                };

                let decode_opus = match info.try_parse_audio_format() {
                    Ok(AudioFormat::Opus) => true,
                    Ok(AudioFormat::Pcm) => false,
                    Err(UnsupportedFormat) => return,
                };

                let android_info = if cfg!(target_os = "android") {
                    let frames_per_burst = self.android_audio_info
                    .current_value()
                    .as_ref()
                    .unwrap().frames_per_burst;

                    Some(PlayAudioEventAndroid {
                        frames_per_burst,
                    })
                } else {
                    None
                };

                let m = ConnectionManagerEvent::ConnectToData {
                    id,
                    next_resource_request: NextResourceRequest::PlayAudio {
                        sample_rate: info.rate as i32,
                        android_info,
                        decode_opus,
                    },
                    data_connection_type: DataConnectionType::Tcp,
                };
                let m = AudioInfoRequestMessages::Connect(m);

                if cfg!(target_os = "android") {
                    let message = self.android_audio_info.request(move |value| m);

                    if let Some(message) = message {
                        message.handle(&mut self.connection_handle, &mut self.r_sender).await;
                    } else {
                        self.r_sender.send_ui_event(crate::ui::UiEvent::GetNativeSampleRate {
                            who_sent_this: self.id,
                        }).await;
                    }
                } else {
                    m.handle(&mut self.connection_handle, &mut self.r_sender).await;
                }

            }
        }
    }
}
