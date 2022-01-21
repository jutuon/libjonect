/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

pub mod utils;
pub mod config;
pub mod audio;
pub mod device;
pub mod message_router;
pub mod ui;
pub mod connection;

use crate::{
    config::LogicConfig,
        audio::AudioManager, device::DeviceManager, message_router::Router, ui::UiConnectionManager,

};

use connection::ConnectionManager;
use tokio::signal;

use tokio::runtime::Runtime;
use utils::{QuitReceiver, QuitSender};

use log::{error, info};


pub struct AsyncLogic {
    config: std::sync::Arc<LogicConfig>,
}

impl AsyncLogic {
    pub fn new(config: LogicConfig) -> Self {
        Self {
            config: config.into(),
        }
    }

    pub async fn run(&mut self, mut logic_quit_receiver: QuitReceiver) {
        // Init message routing.

        let (router, mut r_sender, device_manager_receiver, ui_receiver, audio_receiver, cm_receiver) =
            Router::new();
        let (r_quit_sender, r_quit_receiver) = tokio::sync::oneshot::channel();

        let router_task_handle = tokio::spawn(router.run(r_quit_receiver));

        // Init other components.

        let (audio_task_handle, audio_quit_sender) =
            AudioManager::task(r_sender.clone(), audio_receiver, self.config.clone());
        let (dm_task_handle, dm_quit_sender) = DeviceManager::task(
            r_sender.clone(),
            device_manager_receiver,
            self.config.clone(),
        );
        let (ui_task_handle, ui_quit_sender) =
            UiConnectionManager::task(r_sender.clone(), ui_receiver);

        let (cm_task_handle, cm_quit_sender) = ConnectionManager::start_task(
            r_sender.clone(),
            cm_receiver,
            self.config.clone(),
        );

        let mut ctrl_c_listener_enabled = true;

        // TODO: remove this?
        if self.config.enable_connection_listening {
            r_sender.send_connection_manager_event(connection::ConnectionManagerEvent::StartTcpListener).await;
        }

        if let Some(address) = self.config.connect_address {
            r_sender.send_connection_manager_event(connection::ConnectionManagerEvent::ConnectTo { address }).await;
        }

        loop {
            tokio::select! {
                quit_request = signal::ctrl_c(), if ctrl_c_listener_enabled => {
                    match quit_request {
                        Ok(()) => {
                            break;
                        }
                        Err(e) => {
                            ctrl_c_listener_enabled = false;
                            error!("Failed to listen CTRL+C. Error: {}", e);
                        }
                    }
                }
                result = &mut logic_quit_receiver => {
                    result.unwrap();
                    break;
                }
            }
        }

        dm_quit_sender.send(()).unwrap();
        ui_quit_sender.send(()).unwrap();
        audio_quit_sender.send(()).unwrap();
        cm_quit_sender.send(()).unwrap();

        // Quit started. Wait all components to close.

        audio_task_handle.await.unwrap();
        dm_task_handle.await.unwrap();
        ui_task_handle.await.unwrap();
        cm_task_handle.await.unwrap();

        // And finally close router.

        r_quit_sender.send(()).unwrap();
        router_task_handle.await.unwrap();
    }
}


pub struct Logic;

impl Logic {
    /// Starts main logic. Blocks until main logic is closed.
    pub fn run(config: LogicConfig, quit_receiver: Option<QuitReceiver>) {
        let rt = match Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                error!("{}", e);
                return;
            }
        };

        let (sender, receiver) = match quit_receiver {
            Some(receiver) => {
                (None, receiver)
            }
            None => {
                let (sender, receiver) = Self::create_quit_notification_channel();
                (Some(sender), receiver)
            },
        };

        let mut logic = AsyncLogic::new(config);

        rt.block_on(logic.run(receiver));

        drop(sender);
    }

    pub fn create_quit_notification_channel() -> (QuitSender, QuitReceiver) {
        tokio::sync::oneshot::channel()
    }
}
