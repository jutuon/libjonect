/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Connect devices (clients) to the server.

//pub mod data;
pub mod protocol;
pub mod state;

use tokio::{
    sync::{oneshot},
    task::JoinHandle,
};

use std::{
    collections::HashMap,
    fmt::Debug,
    sync::Arc,
    time::Duration,
};

use crate::{
    config::{LogicConfig},
    utils::{ConnectionId, QuitReceiver, QuitSender}, connection::{JsonConnection, ConnectionManagerEvent}, ui::{UiEvent, AndroidAudioInfo},
};

use self::{
    state::{DeviceEvent, DeviceStateTask, DeviceStateTaskHandle},
};

use super::{
    message_router::{MessageReceiver, RouterSender},
};

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
    NewDeviceConnection { json_connection: JsonConnection, usb: bool },
    UiNativeSampleRate(ConnectionId, AndroidAudioInfo),
    SendStartAudioStream(ConnectionId),
    DisconnectAllDevices,
}

/// Logic for connecting devices (clients) to the server.
pub struct DeviceManager {
    r_sender: RouterSender,
    receiver: MessageReceiver<DmEvent>,
    connections: HashMap<ConnectionId, DeviceStateTaskHandle>,
    config: Arc<LogicConfig>,
}

impl DeviceManager {
    /// Start new `DeviceManager` task.
    pub fn task(
        r_sender: RouterSender,
        receiver: MessageReceiver<DmEvent>,
        config: Arc<LogicConfig>,
    ) -> (JoinHandle<()>, QuitSender) {
        let (quit_sender, quit_receiver) = oneshot::channel();

        let dm = Self {
            r_sender,
            receiver,
            connections: HashMap::new(),
            config,
        };

        let task = async move {
            dm.run(quit_receiver).await;
        };

        (tokio::spawn(task), quit_sender)
    }

    /// Run device manager logic.
    pub async fn run(mut self, mut quit_receiver: QuitReceiver) {
        let mut ping_timer = tokio::time::interval(Duration::from_secs(10));

        // TODO: Use single quit select and move other logic to async block.

        loop {
            tokio::select! {
                result = &mut quit_receiver => break result.unwrap(),
                event = self.receiver.recv() => {
                    tokio::select! {
                        result = &mut quit_receiver => break result.unwrap(),
                        _ = self.handle_dm_event(event) => (),
                    };
                }
                _ = ping_timer.tick(), if self.config.enable_ping => {
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

    /// Handle `DmEvent`.
    pub async fn handle_dm_event(&mut self, event: DmEvent) {
        match event.value {
            DeviceManagerInternalEvent::PublicEvent(event) => match event {
                DeviceManagerEvent::RunDeviceConnectionPing => {
                    for connection in self.connections.values_mut() {
                        connection.send(DeviceEvent::SendPing).await;
                    }
                }
                DeviceManagerEvent::NewDeviceConnection { json_connection, usb } => {
                    let id = json_connection.id();

                    let device_state = DeviceStateTask::task(
                        json_connection,
                        usb,
                        self.r_sender.clone(),
                        self.config.clone(),
                    )
                    .await;

                    self.connections.insert(id, device_state);
                }
                DeviceManagerEvent::UiNativeSampleRate(id, native_sample_rate) => {
                    if let Some(device) = self.connections.get_mut(&id) {
                        device.send(DeviceEvent::UiNativeSampleRate(native_sample_rate)).await;
                    }
                }
                DeviceManagerEvent::SendStartAudioStream(id) => {
                    if let Some(device) = self.connections.get_mut(&id) {
                        device.send(DeviceEvent::SendStartAudioStream).await;
                    }
                }
                DeviceManagerEvent::DisconnectAllDevices => {
                    for (id, connection) in self.connections.iter_mut() {
                        connection.send(DeviceEvent::Disconnect).await;
                    }
                }
            },
            DeviceManagerInternalEvent::RemoveConnection(id) => {
                self.r_sender.send_connection_manager_event(
                    ConnectionManagerEvent::RemoveDataConnections{id}
                ).await;
                self.connections.remove(&id).unwrap().quit().await;

                if self.connections.is_empty() {
                    // TODO: It is possible that there is some data connections
                    // running when this event is sent.
                    self.r_sender.send_ui_event(UiEvent::AllDevicesAreNowDisconnected).await;
                }
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
