/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Connected device's state.

use std::{net::SocketAddr, sync::Arc, time::Instant};

use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    config::{ServerConfig, EVENT_CHANNEL_SIZE},
    audio::AudioEvent,
    message_router::RouterSender,
    utils::{
        Connection, ConnectionEvent, ConnectionHandle, ConnectionId, QuitReceiver, QuitSender,
        SendDownward,
    },
};

use super::{
    data::{DataConnection, DataConnectionEvent, DataConnectionHandle},
    protocol::{
        AudioFormat, AudioStreamInfo, ClientInfo, ClientMessage, ServerInfo, ServerMessage,
    },
    DeviceManagerInternalEvent,
};

/// Event to `DeviceStateTask`.
#[derive(Debug)]
pub enum DeviceEvent {
    DataConnection(DataConnectionEvent),
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
    address: SocketAddr,
    connection_handle: ConnectionHandle<ServerMessage>,
    ping_state: Option<Instant>,
    audio_out: Option<DataConnectionHandle>,
    r_sender: RouterSender,
    event_sender: mpsc::Sender<DeviceEvent>,
    event_receiver: mpsc::Receiver<DeviceEvent>,
    connection_receiver: mpsc::Receiver<ConnectionEvent<ClientMessage>>,
    config: Arc<ServerConfig>,
    client_info: Option<ClientInfo>,
}

impl DeviceStateTask {
    /// Start new `DeviceStateTask`.
    pub async fn task(
        id: ConnectionId,
        stream: TcpStream,
        address: SocketAddr,
        r_sender: RouterSender,
        config: Arc<ServerConfig>,
    ) -> DeviceStateTaskHandle {
        let (connection_sender, connection_receiver) =
            mpsc::channel::<ConnectionEvent<ClientMessage>>(EVENT_CHANNEL_SIZE);

        let (read_half, write_half) = stream.into_split();
        let connection_handle: ConnectionHandle<ServerMessage> =
            Connection::spawn_connection_task(id, read_half, write_half, connection_sender.into());

        let message = ServerMessage::ServerInfo(ServerInfo::new("Test server"));
        connection_handle.send_down(message).await;

        let (event_sender, event_receiver) = mpsc::channel::<DeviceEvent>(EVENT_CHANNEL_SIZE);

        let (quit_sender, quit_receiver) = oneshot::channel();

        let device_task = Self {
            id,
            connection_handle,
            r_sender,
            ping_state: None,
            address,
            audio_out: None,
            event_receiver,
            event_sender: event_sender.clone(),
            connection_receiver,
            config,
            client_info: None,
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
                        ConnectionEvent::ReadError(id, error) => {
                            eprintln!("Connection id {} read error {:?}", id, error);
                            break Some(WaitQuit);
                        }
                        ConnectionEvent::WriteError(id, error) => {
                            eprintln!("Connection id {} write error {:?}", id, error);
                            break Some(WaitQuit);
                        }
                        ConnectionEvent::Message(_, message) => {
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
            audio.quit().await;
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
            DeviceEvent::DataConnection(event) => {
                self.handle_data_connection_message(event).await;
            }
            DeviceEvent::SendPing => {
                if self.ping_state.is_none() {
                    self.connection_handle.send_down(ServerMessage::Ping).await;
                    self.ping_state = Some(Instant::now());
                }
            }
        }
    }

    /// Handle `ClientMessage`.
    async fn handle_client_message(&mut self, message: ClientMessage) {
        match message {
            ClientMessage::ClientInfo(info) => {
                println!("ClientInfo {:?}", info);

                self.client_info = Some(info);

                let handle = DataConnection::task(
                    self.connection_handle.id(),
                    self.event_sender.clone().into(),
                    self.address,
                );

                self.audio_out = Some(handle);
            }
            ClientMessage::Ping => {
                self.connection_handle
                    .send_down(ServerMessage::PingResponse)
                    .await;
            }
            ClientMessage::PingResponse => {
                if let Some(time) = self.ping_state.take() {
                    let time = Instant::now().duration_since(time).as_millis();
                    println!("Ping time {} ms", time);
                }
            }
            ClientMessage::AudioStreamPlayError(error) => {
                eprintln!("AudioStreamPlayError {:?}", error);
            }
        }
    }

    /// Handle `DataConnectionEvent`.
    pub async fn handle_data_connection_message(&mut self, message: DataConnectionEvent) {
        match message {
            DataConnectionEvent::NewConnection(handle) => {
                self.r_sender
                    .send_audio_server_event(AudioEvent::StartRecording {
                        send_handle: handle,
                        sample_rate: self.client_info.as_ref().unwrap().native_sample_rate as u32,
                    })
                    .await;
            }
            DataConnectionEvent::PortNumber(tcp_port) => {
                let mut sample_rate = if let Some(client_info) = &self.client_info {
                    assert!(
                        client_info.native_sample_rate == 44100
                            || client_info.native_sample_rate == 48000
                    );
                    client_info.native_sample_rate as u32
                } else {
                    return;
                };

                let format = if self.config.encode_opus {
                    sample_rate = 48000;
                    AudioFormat::Opus
                } else {
                    AudioFormat::Pcm
                };

                let info = AudioStreamInfo::new(format, 2u8, sample_rate, tcp_port);

                self.connection_handle
                    .send_down(ServerMessage::PlayAudioStream(info))
                    .await;
            }
            e @ DataConnectionEvent::TcpListenerBindError(_)
            | e @ DataConnectionEvent::GetPortNumberError(_)
            | e @ DataConnectionEvent::AcceptError(_)
            | e @ DataConnectionEvent::SendConnectionError(_) => {
                eprintln!("Error: {:?}", e);
            }
        }
    }
}
