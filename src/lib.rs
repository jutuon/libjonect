

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


pub struct AsyncLogic {
    config: std::sync::Arc<LogicConfig>,
}

impl AsyncLogic {
    pub fn new(config: LogicConfig) -> Self {
        Self {
            config: config.into(),
        }
    }

    /// Future for main server task.
    pub async fn run(&mut self) {
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
                            dm_quit_sender.send(()).unwrap();
                            ui_quit_sender.send(()).unwrap();
                            audio_quit_sender.send(()).unwrap();
                            cm_quit_sender.send(()).unwrap();
                            break;
                        }
                        Err(e) => {
                            ctrl_c_listener_enabled = false;
                            eprintln!("Failed to listen CTRL+C. Error: {}", e);
                        }
                    }
                }
            }
        }

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
    pub fn run(config: LogicConfig) {
        let rt = match Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("{}", e);
                return;
            }
        };

        let mut server = AsyncLogic::new(config);

        rt.block_on(server.run());
    }
}
