/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! User interface communication protocol and server code for it.

use serde::{Deserialize, Serialize};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    config::{self, EVENT_CHANNEL_SIZE},
    device::DeviceManagerEvent,
    utils::{Connection, ConnectionEvent, ConnectionHandle, QuitReceiver, QuitSender},
};

use super::{
    connection::tcp::ListenerError,
    message_router::{MessageReceiver, RouterSender},
};

/// UI message from server to UI.
#[derive(Debug, Deserialize, Serialize)]
pub enum UiProtocolFromServerToUi {
    Message(String),
}

/// UI message from UI to server.
#[derive(Debug, Deserialize, Serialize)]
pub enum UiProtocolFromUiToServer {
    NotificationTest,
    RunDeviceConnectionPing,
}

/// Event to `UiConnectionManager`.
#[derive(Debug)]
pub enum UiEvent {
    TcpSupportDisabledBecauseOfError(ListenerError),
}

/// Quit reason for `UiConnectionManager::handle_connection`.
enum QuitReason {
    QuitRequest,
    ConnectionError,
}

/// Logic for handling new UI connections.
pub struct UiConnectionManager {
    server_sender: RouterSender,
    ui_receiver: MessageReceiver<UiEvent>,
    quit_receiver: QuitReceiver,
}

impl UiConnectionManager {
    /// Start new `UiConnectionManager` task.
    pub fn task(
        server_sender: RouterSender,
        ui_receiver: MessageReceiver<UiEvent>,
    ) -> (JoinHandle<()>, QuitSender) {
        let (quit_sender, quit_receiver) = oneshot::channel();

        let cm = Self {
            server_sender,
            ui_receiver,
            quit_receiver,
        };

        let task = async move {
            cm.run().await;
        };

        let handle = tokio::spawn(task);

        (handle, quit_sender)
    }

    /// Run `UiConnectionManager` logic.
    async fn run(mut self) {
        let listener = match TcpListener::bind(config::UI_SOCKET_ADDRESS).await {
            Ok(listener) => listener,
            Err(e) => {
                eprintln!("UI connection disabled. Error: {:?}", e);
                self.quit_receiver.await.unwrap();
                return;
            }
        };

        loop {
            tokio::select! {
                event = &mut self.quit_receiver => return event.unwrap(),
                listener_result = listener.accept() => {
                    let socket = match listener_result {
                        Ok((socket, _)) => socket,
                        Err(e) => {
                            eprintln!("Error: {:?}", e);
                            continue;
                        }
                    };

                    match Self::handle_connection(
                        &mut self.server_sender,
                        &mut self.ui_receiver,
                        &mut self.quit_receiver,
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
        mut server_sender: &mut RouterSender,
        ui_receiver: &mut MessageReceiver<UiEvent>,
        mut quit_receiver: &mut QuitReceiver,
        connection: TcpStream,
    ) -> QuitReason {
        let (sender, mut connections_receiver) =
            mpsc::channel::<ConnectionEvent<UiProtocolFromUiToServer>>(EVENT_CHANNEL_SIZE);

        let connection_handle: ConnectionHandle<UiProtocolFromUiToServer> =
            Connection::spawn_connection_task(connection, sender.into());

        tokio::pin!(ui_receiver);

        let quit_reason = loop {
            tokio::select! {
                event = &mut quit_receiver => {
                    event.unwrap();
                    break QuitReason::QuitRequest;
                },
                message = ui_receiver.recv() => {
                    match message {
                        UiEvent::TcpSupportDisabledBecauseOfError(error) => {
                            eprintln!("TCP support disabled {:?}", error);
                            continue;
                        }
                    }
                }
                event = connections_receiver.recv() => {
                    match event.unwrap() {
                        ConnectionEvent::ReadError(error) => {
                            eprintln!("UI connection read error {:?}", error);
                            break QuitReason::ConnectionError;
                        }
                        ConnectionEvent::WriteError(error) => {
                            eprintln!("UI connection write error {:?}", error);
                            break QuitReason::ConnectionError;
                        }
                        ConnectionEvent::Message(message) => {
                            let sender = &mut server_sender;
                            let handle_message = async move {
                                match message {
                                    UiProtocolFromUiToServer::NotificationTest => {
                                        println!("UI notification");
                                    }
                                    UiProtocolFromUiToServer::RunDeviceConnectionPing => {
                                        sender.send_device_manager_event(DeviceManagerEvent::RunDeviceConnectionPing).await;
                                    }
                                }
                            };
                            tokio::select! {
                                result = &mut quit_receiver => {
                                    result.unwrap();
                                    break QuitReason::QuitRequest;
                                }
                                _ = handle_message => (),
                            };
                        }
                    }
                }

            }
        };

        connection_handle.quit().await;
        quit_reason
    }
}
