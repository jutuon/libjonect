/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Message router for sending messages between components.

use log::debug;

use tokio::sync::mpsc;

use crate::{config, utils, connection::{ConnectionManagerEvent}};

use super::{
    audio::AudioEvent,
    device::{DeviceManagerEvent, DmEvent},
    ui::UiEvent,
};

// Event to `Router`.
#[derive(Debug)]
pub enum RouterEvent {
    SendQuitRequest,
}

/// Message to be routed.
#[derive(Debug)]
pub enum RouterMessage {
    AudioServer(AudioEvent),
    DeviceManager(DmEvent),
    ConnectionManager(ConnectionManagerEvent),
    Ui(UiEvent),
    Router(RouterEvent),
}

/// Router for sending messages to different components.
#[derive(Debug)]
pub struct Router {
    r_receiver: mpsc::Receiver<RouterMessage>,
    audio_sender: mpsc::Sender<AudioEvent>,
    device_manager_sender: mpsc::Sender<DmEvent>,
    ui_sender: mpsc::Sender<UiEvent>,
    cm_sender: mpsc::Sender<ConnectionManagerEvent>,
}

impl Router {
    /// Create new `Router`.
    pub fn new() -> (
        Self,
        RouterSender,
        MessageReceiver<DmEvent>,
        MessageReceiver<UiEvent>,
        MessageReceiver<AudioEvent>,
        MessageReceiver<ConnectionManagerEvent>,
    ) {
        let (r_sender, r_receiver) = mpsc::channel(config::EVENT_CHANNEL_SIZE);

        let sender = RouterSender { sender: r_sender };

        let (device_manager_sender, device_manager_receiver) =
            mpsc::channel(config::EVENT_CHANNEL_SIZE);
        let (ui_sender, ui_receiver) = mpsc::channel(config::EVENT_CHANNEL_SIZE);
        let (audio_sender, audio_receiver) = mpsc::channel(config::EVENT_CHANNEL_SIZE);
        let (cm_sender, cm_receiver) = mpsc::channel(config::EVENT_CHANNEL_SIZE);

        let router = Self {
            r_receiver,
            audio_sender,
            device_manager_sender,
            ui_sender,
            cm_sender,
        };

        (
            router,
            sender,
            MessageReceiver::new(device_manager_receiver),
            MessageReceiver::new(ui_receiver),
            MessageReceiver::new(audio_receiver),
            MessageReceiver::new(cm_receiver),
        )
    }

    /// Run message routing code.
    pub async fn run(mut self, mut quit_receiver: utils::QuitReceiver) {
        // TODO: One select for &mut quit_receiver. Move other code to async
        // block.

        loop {
            tokio::select! {
                result = &mut quit_receiver => break result.unwrap(),
                Some(event) = self.r_receiver.recv() => {
                    debug!("Event: {:?}\n", event);
                    match event {
                        RouterMessage::AudioServer(event) => {
                            tokio::select! {
                                result = &mut quit_receiver => break result.unwrap(),
                                result = self.audio_sender.send(event) => result.unwrap(),
                            }
                        }
                        RouterMessage::DeviceManager(event) => {
                            tokio::select! {
                                result = &mut quit_receiver => break result.unwrap(),
                                result = self.device_manager_sender.send(event) => result.unwrap(),
                            }
                        }
                        RouterMessage::ConnectionManager(event) => {
                            tokio::select! {
                                result = &mut quit_receiver => break result.unwrap(),
                                result = self.cm_sender.send(event) => result.unwrap(),
                            }
                        }
                        RouterMessage::Ui(event) => {
                            tokio::select! {
                                result = &mut quit_receiver => break result.unwrap(),
                                result = self.ui_sender.send(event) => result.unwrap(),
                            }
                        }
                        RouterMessage::Router(RouterEvent::SendQuitRequest) => {
                            // TODO: Should router send quit message to components?
                        }
                    }
                }
            }
        }
    }
}

/// Send messages to different components.
#[derive(Debug, Clone)]
pub struct RouterSender {
    sender: mpsc::Sender<RouterMessage>,
}

impl RouterSender {
    /// Send message to `UiConnectionManager`.
    pub async fn send_ui_event(&mut self, event: UiEvent) {
        self.sender.send(RouterMessage::Ui(event)).await.unwrap()
    }

    /// Send message to `AudioManager`.
    pub async fn send_audio_server_event(&mut self, event: AudioEvent) {
        self.sender
            .send(RouterMessage::AudioServer(event))
            .await
            .unwrap()
    }

    /// Send message to `DeviceManager`.
    pub async fn send_device_manager_event(&mut self, event: DeviceManagerEvent) {
        self.sender
            .send(RouterMessage::DeviceManager(event.into()))
            .await
            .unwrap()
    }

    /// Send `DeviceManager`'s internal event to `DeviceManager`.
    pub async fn send_dm_internal_event(&mut self, event: DmEvent) {
        self.sender
            .send(RouterMessage::DeviceManager(event))
            .await
            .unwrap()
    }

    pub async fn send_connection_manager_event(&mut self, event: ConnectionManagerEvent) {
        self.sender
            .send(RouterMessage::ConnectionManager(event))
            .await
            .unwrap()
    }

    /// Send event to `Router`. This method will block.
    pub fn send_router_blocking(&mut self, event: RouterEvent) {
        self.sender
            .blocking_send(RouterMessage::Router(event))
            .unwrap()
    }

    pub fn send_connection_manager_event_blocking(&mut self, event: ConnectionManagerEvent) {
        self.sender
            .blocking_send(RouterMessage::ConnectionManager(event))
            .unwrap()
    }
}

/// Receive routed messages.
#[derive(Debug)]
pub struct MessageReceiver<T> {
    receiver: mpsc::Receiver<T>,
}

impl<T> MessageReceiver<T> {
    /// Create new `MessageReceiver`.
    pub fn new(receiver: mpsc::Receiver<T>) -> Self {
        Self { receiver }
    }

    pub async fn recv(&mut self) -> T {
        self.receiver.recv().await.unwrap()
    }
}
