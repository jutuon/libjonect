

pub mod utils;
pub mod config;
pub mod audio;
pub mod device;
pub mod message_router;
pub mod ui;

use crate::{
    config::ServerConfig,
        audio::AudioManager, device::DeviceManager, message_router::Router, ui::UiConnectionManager,

};

use tokio::signal;

use tokio::runtime::Runtime;

/// Async server code.
pub struct AsyncServer {
    config: std::sync::Arc<ServerConfig>,
}

impl AsyncServer {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config: config.into(),
        }
    }

    /// Future for main server task.
    pub async fn run(&mut self) {
        // Init message routing.

        let (router, r_sender, device_manager_receiver, ui_receiver, audio_receiver) =
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

        let mut ctrl_c_listener_enabled = true;

        loop {
            tokio::select! {
                quit_request = signal::ctrl_c(), if ctrl_c_listener_enabled => {
                    match quit_request {
                        Ok(()) => {
                            dm_quit_sender.send(()).unwrap();
                            ui_quit_sender.send(()).unwrap();
                            audio_quit_sender.send(()).unwrap();
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

        // And finally close router.

        r_quit_sender.send(()).unwrap();
        router_task_handle.await.unwrap();
    }
}

/// Start server.
pub struct Server;

impl Server {
    /// Starts server. Blocks until server is closed.
    pub fn run(config: ServerConfig) {
        let rt = match Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("{}", e);
                return;
            }
        };

        let mut server = AsyncServer::new(config);

        rt.block_on(server.run());
    }
}
