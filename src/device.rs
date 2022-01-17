/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Connect devices (clients) to the server.

pub mod data;
pub mod protocol;
pub mod state;

use tokio::{
    net::TcpListener,
    sync::{oneshot},
    task::JoinHandle,
};

use std::{
    collections::HashMap,
    fmt::Debug,
    io::{self},
    sync::Arc,
    time::Duration,
};

use crate::{
    config::{self, ServerConfig},
    utils::{ConnectionId, QuitReceiver, QuitSender},
};

use self::{
    state::{DeviceEvent, DeviceStateTask, DeviceStateTaskHandle},
};

use super::{
    message_router::{MessageReceiver, RouterSender},
    ui::UiEvent,
};

/// TCP socket errors which disables TCP support.
#[derive(Debug)]
pub enum TcpSupportError {
    ListenerCreationError(io::Error),
    AcceptError(io::Error),
}

/// `DeviceManager`'s internal event type.
#[derive(Debug)]
enum DeviceManagerInternalEvent {
    PublicEvent(DeviceManagerEvent),
    RemoveConnection(ConnectionId),
}

/// Wrapper for `DeviceManagerInternalEvent`.
///
/// This makes possible sending public `DeviceManagerEvent`s from other code
/// while keeping `DeviceManagerInternalEvent` private.
#[derive(Debug)]
pub struct DmEvent {
    value: DeviceManagerInternalEvent,
}

impl From<DeviceManagerEvent> for DmEvent {
    fn from(e: DeviceManagerEvent) -> Self {
        Self {
            value: DeviceManagerInternalEvent::PublicEvent(e),
        }
    }
}

impl From<DeviceManagerInternalEvent> for DmEvent {
    fn from(value: DeviceManagerInternalEvent) -> Self {
        Self { value }
    }
}

/// Device manager events.
#[derive(Debug)]
pub enum DeviceManagerEvent {
    RunDeviceConnectionPing,
}

/// Logic for connecting devices (clients) to the server.
pub struct DeviceManager {
    r_sender: RouterSender,
    receiver: MessageReceiver<DmEvent>,
    next_connection_id: u64,
    connections: HashMap<ConnectionId, DeviceStateTaskHandle>,
    tcp_listener_enabled: bool,
    config: Arc<ServerConfig>,
}

impl DeviceManager {
    /// Start new `DeviceManager` task.
    pub fn task(
        r_sender: RouterSender,
        receiver: MessageReceiver<DmEvent>,
        config: Arc<ServerConfig>,
    ) -> (JoinHandle<()>, QuitSender) {
        let (quit_sender, quit_receiver) = oneshot::channel();

        let dm = Self {
            r_sender,
            receiver,
            next_connection_id: 0,
            connections: HashMap::new(),
            tcp_listener_enabled: true,
            config,
        };

        let task = async move {
            dm.run(quit_receiver).await;
        };

        (tokio::spawn(task), quit_sender)
    }

    /// Run device manager logic.
    pub async fn run(mut self, mut quit_receiver: QuitReceiver) {
        let listener = match TcpListener::bind(config::DEVICE_SOCKET_ADDRESS).await {
            Ok(listener) => listener,
            Err(e) => {
                let e = TcpSupportError::ListenerCreationError(e);
                let e = UiEvent::TcpSupportDisabledBecauseOfError(e);

                tokio::select! {
                    result = &mut quit_receiver => return result.unwrap(),
                    _ = self.r_sender.send_ui_event(e) => (),
                };

                // Wait quit message.
                quit_receiver.await.unwrap();
                return;
            }
        };

        let mut ping_timer = tokio::time::interval(Duration::from_secs(1));

        // TODO: Use single quit select and move other logic to async block.

        loop {
            tokio::select! {
                result = &mut quit_receiver => break result.unwrap(),
                result = listener.accept(), if self.tcp_listener_enabled => {
                    tokio::select! {
                        result = &mut quit_receiver => break result.unwrap(),
                        _ = self.handle_tcp_listener_accept(result) => (),
                    };
                }
                event = self.receiver.recv() => {
                    tokio::select! {
                        result = &mut quit_receiver => break result.unwrap(),
                        _ = self.handle_dm_event(event) => (),
                    };
                }
                _ = ping_timer.tick() => {
                    tokio::select! {
                        result = &mut quit_receiver => break result.unwrap(),
                        _ = self.handle_ping_timer_tick() => (),
                    };
                }
            }
        }

        // Quit

        for connection in self.connections.into_values() {
            connection.quit().await;
        }
    }

    /// Handle TCP listener accept.
    pub async fn handle_tcp_listener_accept(
        &mut self,
        result: std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)>,
    ) {
        match result {
            Ok((stream, address)) => {
                let id = self.next_connection_id;
                self.next_connection_id = match id.checked_add(1) {
                    Some(new_next_id) => new_next_id,
                    None => {
                        eprintln!("Warning: Couldn't handle a new connection because there is no new connection ID numbers.");
                        return;
                    }
                };

                let device_state = DeviceStateTask::task(
                    id,
                    stream,
                    address,
                    self.r_sender.clone(),
                    self.config.clone(),
                )
                .await;

                self.connections.insert(id, device_state);
            }
            Err(e) => {
                let e = TcpSupportError::AcceptError(e);
                let e = UiEvent::TcpSupportDisabledBecauseOfError(e);
                self.r_sender.send_ui_event(e).await;

                self.tcp_listener_enabled = false;
            }
        }
    }

    /// Handle `DmEvent`.
    pub async fn handle_dm_event(&mut self, event: DmEvent) {
        match event.value {
            DeviceManagerInternalEvent::PublicEvent(event) => match event {
                DeviceManagerEvent::RunDeviceConnectionPing => {
                    for connection in self.connections.values_mut() {
                        connection.send(DeviceEvent::SendPing).await;
                    }
                }
            },
            DeviceManagerInternalEvent::RemoveConnection(id) => {
                self.connections.remove(&id).unwrap().quit().await
            }
        }
    }

    /// Handle ping timer tick.
    pub async fn handle_ping_timer_tick(&mut self) {
        for connection in self.connections.values_mut() {
            connection.send(DeviceEvent::SendPing).await;
        }
    }
}
