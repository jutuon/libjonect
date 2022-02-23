/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! User interface communication protocol and server code for it.

use std::net::ToSocketAddrs;

use log::{error, info};

use serde::{Deserialize, Serialize};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    config::{self, EVENT_CHANNEL_SIZE},
    device::DeviceManagerEvent,
    utils::{Connection, ConnectionEvent, ConnectionHandle, QuitReceiver, QuitSender, ConnectionId}, connection::ConnectionManagerEvent,
};

use super::{
    connection::tcp::ListenerError,
    message_router::{MessageReceiver, RouterSender},
};

/// UI message from server to UI.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum UiProtocolFromServerToUi {
    Message(String),
    DeviceConnectionEstablished,
    DeviceConnectionDisconnected,
    DeviceConnectionDisconnectedWithError,
    AndroidGetNativeSampleRate {
        message_from: String,
    },
}

/// UI message from UI to server.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum UiProtocolFromUiToServer {
    NotificationTest,
    RunDeviceConnectionPing,
    DisconnectDevice,
    ConnectTo { ip_address: String },
    AndroidNativeSampleRate(AndroidAudioInfo),

}

#[derive(Debug, Deserialize, Serialize)]
pub struct AndroidAudioInfo {
    pub native_sample_rate: i32,
    pub frames_per_burst: i32,
    pub message_to: String,
}

/// Event to `UiConnectionManager`.
#[derive(Debug)]
pub enum UiEvent {
    TcpSupportDisabledBecauseOfError(ListenerError),
    GetNativeSampleRate { who_sent_this: ConnectionId },
    AllDevicesAreNowDisconnected,
    ConnectionError { connection_id: ConnectionId },
    ConnectionEstablished { connection_id: ConnectionId },
}

/// Quit reason for `UiConnectionManager::handle_connection`.
enum QuitReason {
    QuitRequest,
    ConnectionError,
}

/// Logic for handling new UI connections.
pub struct UiConnectionManager {
    r_sender: RouterSender,
    ui_receiver: MessageReceiver<UiEvent>,
}

impl UiConnectionManager {
    /// Start new `UiConnectionManager` task.
    pub fn task(
        server_sender: RouterSender,
        ui_receiver: MessageReceiver<UiEvent>,
    ) -> (JoinHandle<()>, QuitSender) {
        let (quit_sender, quit_receiver) = oneshot::channel();

        let cm = Self {
            r_sender: server_sender,
            ui_receiver,
        };

        let task = async move {
            cm.run(quit_receiver).await;
        };

        let handle = tokio::spawn(task);

        (handle, quit_sender)
    }

    /// Run `UiConnectionManager` logic.
    async fn run(mut self, mut quit_receiver: QuitReceiver) {
        let listener = match TcpListener::bind(config::UI_SOCKET_ADDRESS).await {
            Ok(listener) => listener,
            Err(e) => {
                error!("UI connection disabled. Error: {:?}", e);
                quit_receiver.await.unwrap();
                return;
            }
        };

        loop {
            tokio::select! {
                event = &mut quit_receiver => return event.unwrap(),
                message = self.ui_receiver.recv() => {
                    drop(message);
                }
                listener_result = listener.accept() => {
                    let socket = match listener_result {
                        Ok((socket, _)) => socket,
                        Err(e) => {
                            error!("Error: {:?}", e);
                            continue;
                        }
                    };

                    match self.handle_connection(
                        &mut quit_receiver,
                        socket,
                    ).await {
                        QuitReason::QuitRequest => return,
                        QuitReason::ConnectionError => (),
                    }
                }
            }
        }
    }

    /// Handle new UI connection.
    async fn handle_connection(
        &mut self,
        mut quit_receiver: &mut QuitReceiver,
        connection: TcpStream,
    ) -> QuitReason {
        let (sender, mut connections_receiver) =
            mpsc::channel::<ConnectionEvent<UiProtocolFromUiToServer>>(EVENT_CHANNEL_SIZE);

        let connection_handle: ConnectionHandle<UiProtocolFromServerToUi> =
            Connection::spawn_connection_task(connection, sender.into());

        let quit_reason = loop {
            tokio::select! {
                event = &mut quit_receiver => {
                    event.unwrap();
                    break QuitReason::QuitRequest;
                },
                message = self.ui_receiver.recv() => {
                    match message {
                        UiEvent::TcpSupportDisabledBecauseOfError(error) => {
                            info!("TCP support disabled {:?}", error);
                            continue;
                        }
                        UiEvent::GetNativeSampleRate { who_sent_this } => {
                            connection_handle.send_down(UiProtocolFromServerToUi::AndroidGetNativeSampleRate {
                                message_from: who_sent_this.to_string(),
                            }).await;
                        }
                        UiEvent::ConnectionError { .. } => {
                            connection_handle.send_down(UiProtocolFromServerToUi::DeviceConnectionDisconnectedWithError).await;
                        }
                        UiEvent::AllDevicesAreNowDisconnected => {
                            // TODO: Support displaying multiple devces in UI.
                            connection_handle.send_down(UiProtocolFromServerToUi::DeviceConnectionDisconnected).await;
                        }
                        UiEvent::ConnectionEstablished { .. } => {
                            connection_handle.send_down(UiProtocolFromServerToUi::DeviceConnectionEstablished).await;
                        }
                    }
                }
                event = connections_receiver.recv() => {
                    match event.unwrap() {
                        ConnectionEvent::ReadError(error) => {
                            error!("UI connection read error {:?}", error);
                            break QuitReason::ConnectionError;
                        }
                        ConnectionEvent::WriteError(error) => {
                            error!("UI connection write error {:?}", error);
                            break QuitReason::ConnectionError;
                        }
                        ConnectionEvent::Message(message) => {
                            tokio::select! {
                                result = &mut quit_receiver => {
                                    result.unwrap();
                                    break QuitReason::QuitRequest;
                                }
                                _ = self.handle_message(message) => (),
                            };
                        }
                    }
                }

            }
        };

        connection_handle.quit().await;
        quit_reason
    }

    async fn handle_message(
        &mut self,
        message: UiProtocolFromUiToServer,
    ) {
        match message {
            UiProtocolFromUiToServer::NotificationTest => {
                info!("UI notification");
            }
            UiProtocolFromUiToServer::RunDeviceConnectionPing => {
                self.r_sender.send_device_manager_event(
                    DeviceManagerEvent::RunDeviceConnectionPing
                ).await;
            }
            UiProtocolFromUiToServer::AndroidNativeSampleRate(audio_info) => {
                let id: ConnectionId = match audio_info.message_to.parse() {
                    Ok(id) => id,
                    Err(e) => {
                        error!("AndroidNativeSampleRate field message_to is not valid. Error: {}", e);
                        return;
                    }
                };

                let message = DeviceManagerEvent::UiNativeSampleRate(
                    id, audio_info
                );
                self.r_sender.send_device_manager_event(message).await;
            }
            UiProtocolFromUiToServer::ConnectTo { ip_address } => {
                let address_str = format!("{}:{}", ip_address, crate::config::JSON_PORT);
                match address_str.to_socket_addrs() {
                    Ok(address_iter) => {
                        if let Some(address) = address_iter.into_iter().next() {
                            self.r_sender.send_connection_manager_event(ConnectionManagerEvent::ConnectTo {
                                address,
                            }).await;
                        } else {
                            error!("Address parsing failed. Address: {}", address_str);
                        }
                    }
                    Err(e) => {
                        error!("Error: {}", e);
                    }
                }
            }
            UiProtocolFromUiToServer::DisconnectDevice => {
                self.r_sender.send_device_manager_event(DeviceManagerEvent::DisconnectAllDevices).await;
            }
        }
    }
}


pub struct ValueRequest<T: 'static, M> {
    value: Option<T>,
    request: Vec<Box<dyn FnOnce(&T) -> M + Send>>,
}

impl <T: 'static, M> ValueRequest<T, M> {
    pub fn new() -> Self {
        Self {
            value: None,
            request: Vec::new(),
        }
    }

    pub fn request<
        F: FnOnce(&T) -> M + 'static + Send,
    >(
        &mut self,
        create_message: F,
    ) -> Option<M> {
        if let Some(value) = self.value.as_ref() {
            Some((create_message)(value))
        } else {
            self.request.push(Box::new(create_message));
            None
        }
    }

    pub fn set_value(&mut self, value: T) -> impl Iterator<Item=M> + '_ {
        self.value = Some(value);
        let value = self.value.as_ref().unwrap();
        self.request.drain(..).map(move |handler| {
            (handler)(value)
        })
    }

    pub fn current_value(&self) -> &Option<T> {
        &self.value
    }
}
