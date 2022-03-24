/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#[cfg(target_os = "linux")]
mod libusb;

#[cfg(target_os = "android")]
mod android;

pub mod protocol;

use tokio::{sync::{oneshot, mpsc::Receiver}, task::JoinHandle};

use crate::{message_router::{RouterSender}, utils::{QuitReceiver, QuitSender, SendDownward}, config::{LogicConfig, EVENT_CHANNEL_SIZE}};


use std::{sync::{Arc}, fmt::Debug};

use super::data::{usb::{UsbDataConnectionSender, UsbDataConnectionReceiver, UsbDataConnectionBuilder}, DataSender, DataSenderBuilder, DataReceiverBuilder};


#[cfg(target_os = "linux")]
pub use self::libusb::{UsbPacketWrapper, UsbDataChannelCreator, UsbDataChannelReceiver};

#[cfg(target_os = "android")]
pub use self::android::{UsbPacketWrapper, UsbDataChannelCreator, UsbDataChannelReceiver};

pub trait UsbDataChannelCreatorI {
    fn build_sender(&self) -> (DataSenderBuilder, DataReceiverBuilder) {
        UsbDataConnectionBuilder::build_sender()
    }

    fn build_receiver(&self) -> (UsbDataConnectionSender, DataReceiverBuilder) {
        UsbDataConnectionBuilder::build_receiver()
    }
}


#[derive(Debug)]
pub enum UsbEvent {
    ReceiveAudioOverUsb(UsbDataConnectionSender),
    /// File descriptor or -1 if there is no USB accessory connected.
    AndroidUsbAccessoryFileDescriptor(i32),
    AndroidQuitAndroidUsbManager,
}

pub struct UsbManagerHandle {
    join_handle: JoinHandle<()>,
    quit_sender: QuitSender,
    usb_sender: SendDownward<UsbEvent>,
}

impl UsbManagerHandle {
    /// This will block until `ConnectionListener` quits.
    pub async fn quit(self) {
        self.quit_sender.send(()).unwrap();
        self.join_handle.await.unwrap();
    }

    pub async fn send_event(&mut self, event: UsbEvent) {
        self.usb_sender.send_down(event).await;
    }
}


pub struct UsbManager {
    r_sender: RouterSender,
    quit_receiver: QuitReceiver,
    config: Arc<LogicConfig>,
    usb_receiver: Receiver<UsbEvent>,
    usb_data_channel_receiver: UsbDataChannelReceiver,
}

impl UsbManager {
    pub fn start_task(
        r_sender: RouterSender,
        config: Arc<LogicConfig>,
        usb_data_channel_receiver: UsbDataChannelReceiver,
    ) -> UsbManagerHandle {
        let (quit_sender, quit_receiver) = oneshot::channel();

        let (usb_sender, usb_receiver) =
            tokio::sync::mpsc::channel::<UsbEvent>(EVENT_CHANNEL_SIZE);

        let usb_manager = Self {
            r_sender,
            quit_receiver,
            config,
            usb_receiver,
            usb_data_channel_receiver,
        };

        let task = async move {
            usb_manager.run().await;
        };

        let join_handle = tokio::spawn(task);

        UsbManagerHandle {
            join_handle, quit_sender, usb_sender: usb_sender.into(),
        }
    }

    #[cfg(target_os = "linux")]
    async fn run(mut self) {
        use std::time::Duration;

        use tokio::net::TcpListener;

        use crate::{config, connection::{tcp::ListenerError, CmInternalEvent}};

        let usb_json_listener = match TcpListener::bind(config::USB_JSON_SOCKET_ADDRESS).await {
            Ok(listener) => listener,
            Err(e) => {
                let e = ListenerError::BindJsonSocket(e);
                let e = CmInternalEvent::UsbTcpListenerError(e).into();

                tokio::select! {
                    result = &mut self.quit_receiver => return result.unwrap(),
                    _ = self.r_sender.send_connection_manager_event(e) => (),
                };

                // Wait quit message.
                self.quit_receiver.await.unwrap();
                return;
            }
        };

        let mut usb_thread = self::libusb::LibUsbThread::start(self.r_sender.clone(), self.config, self.usb_data_channel_receiver).await;

        // TODO: Do not poll USB devices.
        let mut timer = tokio::time::interval(Duration::from_secs(2));

        let mut json_poll_timer = tokio::time::interval(Duration::from_millis(50));

        loop {
            tokio::select! {
                result = &mut self.quit_receiver => break result.unwrap(),
                event = self.usb_receiver.recv() => {
                    usb_thread.send_event(event.unwrap());
                }
                _ = timer.tick() => {
                    usb_thread.poll_usb_devices();
                }
                _ = json_poll_timer.tick() => {
                    usb_thread.json_poll_if_connected();
                }
                result = usb_json_listener.accept() => {
                    match result {
                        Ok((stream, address)) => {
                            self.r_sender.send_connection_manager_event(
                                CmInternalEvent::NewJsonStream { stream, address, usb: true }.into(),
                            ).await;
                        }
                        Err(error) => {
                            let e = ListenerError::AcceptJsonConnection(error);
                            self.r_sender.send_connection_manager_event(
                                CmInternalEvent::ListenerError(e).into()
                            ).await;

                            usb_thread.quit();
                            self.quit_receiver.await.unwrap();
                            return;
                        }
                    }
                },
            }
        }

        usb_thread.quit();
    }

    #[cfg(target_os = "android")]
    async fn run(mut self) {
        use std::time::Duration;

        use log::info;
        use tokio::net::TcpListener;

        use crate::{ui::UiEvent, connection::{usb::android::AndroidUsbThread, tcp::ListenerError, CmInternalEvent}, config};

        let mut android_usb: Option<AndroidUsbThread> = None;

        let usb_json_listener = match TcpListener::bind(config::USB_JSON_SOCKET_ADDRESS).await {
            Ok(listener) => listener,
            Err(e) => {
                let e = ListenerError::BindJsonSocket(e);
                let e = CmInternalEvent::UsbTcpListenerError(e).into();

                tokio::select! {
                    result = &mut self.quit_receiver => return result.unwrap(),
                    _ = self.r_sender.send_connection_manager_event(e) => (),
                };

                // Wait quit message.
                self.quit_receiver.await.unwrap();
                return;
            }
        };


        loop {
            tokio::select! {
                result = &mut self.quit_receiver => break result.unwrap(),
                event = self.usb_receiver.recv() => {
                    match event.unwrap() {
                        UsbEvent::AndroidUsbAccessoryFileDescriptor(fd) => {
                            if fd != -1 {
                                info!("Fd received: {}", fd);
                                if let Some(handle) = android_usb.take() {
                                    handle.quit();
                                }

                                match AndroidUsbThread::start(self.r_sender.clone(), self.config.clone(), fd) {
                                    Ok(thread) => {
                                        android_usb = Some(thread);
                                    }
                                    Err(()) => (),
                                }
                            }
                        }
                        UsbEvent::AndroidQuitAndroidUsbManager => {
                            if let Some(android_usb) = android_usb.take() {
                                android_usb.quit()
                            }
                        }
                        event => {
                            if let Some(usb_thread) = android_usb.as_mut() {
                                usb_thread.send_event(event);
                            }
                        }
                    }
                }
                result = usb_json_listener.accept() => {
                    match result {
                        Ok((stream, address)) => {
                            self.r_sender.send_connection_manager_event(
                                CmInternalEvent::NewJsonStream { stream, address, usb: true }.into(),
                            ).await;
                        }
                        Err(error) => {
                            let e = ListenerError::AcceptJsonConnection(error);
                            self.r_sender.send_connection_manager_event(
                                CmInternalEvent::ListenerError(e).into()
                            ).await;

                            break;
                        }
                    }
                },
            }
        }

        if let Some(android_usb) = android_usb.take() {
            android_usb.quit()
        }
    }
}
