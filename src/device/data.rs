/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Device data connection.

use std::net::SocketAddr;

use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    config::{self, EVENT_CHANNEL_SIZE},
    utils::{ConnectionId, ConnectionShutdownWatch, SendDownward, SendUpward},
};

use super::state::DeviceEvent;

/// Send data to connected device. Writing will be nonblocking. Drop this to close
/// `TcpSendConnection`.
#[derive(Debug)]
pub struct TcpSendHandle {
    tcp_stream: std::net::TcpStream,
    _shutdown_watch: ConnectionShutdownWatch,
}

impl std::io::Write for TcpSendHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.tcp_stream.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.tcp_stream.flush()
    }
}

/// Handle to `TcpSendHandle`.
#[derive(Debug)]
pub struct TcpSendConnection {
    shutdown_watch_receiver: mpsc::Receiver<()>,
}

impl TcpSendConnection {
    /// Create `TcpSendConnection` and `TcpSendHandle`.
    pub fn new(tcp_stream: std::net::TcpStream) -> Result<(Self, TcpSendHandle), std::io::Error> {
        tcp_stream.set_nonblocking(true)?;
        let (_shutdown_watch, shutdown_watch_receiver) = mpsc::channel::<()>(1);

        let handle = TcpSendHandle {
            tcp_stream,
            _shutdown_watch,
        };

        let connection = Self {
            shutdown_watch_receiver,
        };

        Ok((connection, handle))
    }

    /// Wait until `TcpSendHandle` is dropped.
    pub async fn wait_quit(mut self) {
        // TODO: Shutdown the socket here?

        let _ = self.shutdown_watch_receiver.recv().await;
    }
}

/// Events which `DataConnection` can send.
#[derive(Debug)]
pub enum DataConnectionEvent {
    TcpListenerBindError(std::io::Error),
    GetPortNumberError(std::io::Error),
    AcceptError(std::io::Error),
    SendConnectionError(std::io::Error),
    /// `DataConnection` is now waiting a new connection to this port.
    PortNumber(u16),
    /// Device is now connected. Use `TcpSendHandle` to send data to the device.
    NewConnection(TcpSendHandle),
}

impl From<DataConnectionEvent> for DeviceEvent {
    fn from(event: DataConnectionEvent) -> Self {
        DeviceEvent::DataConnection(event)
    }
}

/// Event from `Device` to `DataConnection`.
#[derive(Debug)]
pub enum DataConnectionEventFromDevice {
    Test,
}

/// Handle to `DataConnection` task.
pub struct DataConnectionHandle {
    command_connection_id: ConnectionId,
    task_handle: JoinHandle<()>,
    event_sender: SendDownward<DataConnectionEventFromDevice>,
    quit_sender: oneshot::Sender<()>,
}

impl DataConnectionHandle {
    pub fn id(&self) -> ConnectionId {
        self.command_connection_id
    }

    /// Send quit request to `DataConnection` and wait untill it is closed.
    pub async fn quit(self) {
        self.quit_sender.send(()).unwrap();
        self.task_handle.await.unwrap();
    }

    /// Send `DataConnectionEventFromDevice`.
    pub async fn send_down(&self, message: DataConnectionEventFromDevice) {
        self.event_sender.send_down(message).await
    }
}

/// Task for waiting data connection from the device.
pub struct DataConnection {
    command_connection_id: ConnectionId,
    sender: SendUpward<DeviceEvent>,
    receiver: mpsc::Receiver<DataConnectionEventFromDevice>,
    quit_receiver: oneshot::Receiver<()>,
    accept_from: SocketAddr,
    connection: Option<TcpSendConnection>,
}

impl DataConnection {
    /// Start new `DataConnection` task.
    pub fn task(
        command_connection_id: ConnectionId,
        sender: SendUpward<DeviceEvent>,
        accept_from: SocketAddr,
    ) -> DataConnectionHandle {
        let (event_sender, receiver) = mpsc::channel(EVENT_CHANNEL_SIZE);
        let (quit_sender, quit_receiver) = oneshot::channel();

        let manager = Self {
            command_connection_id,
            sender,
            receiver,
            quit_receiver,
            accept_from,
            connection: None,
        };

        let task_handle = tokio::spawn(manager.run());

        DataConnectionHandle {
            command_connection_id,
            task_handle,
            event_sender: event_sender.into(),
            quit_sender,
        }
    }

    // Run `DataConnection` logic.
    pub async fn run(mut self) {
        let audio_out = match TcpListener::bind(config::AUDIO_DATA_SOCKET_ADDRESS).await {
            Ok(listener) => listener,
            Err(e) => {
                let e = DataConnectionEvent::TcpListenerBindError(e);
                self.sender.send_up(e.into()).await;
                self.quit_receiver.await.unwrap();
                return;
            }
        };

        match audio_out.local_addr() {
            Ok(address) => {
                let event = DataConnectionEvent::PortNumber(address.port());
                self.sender.send_up(event.into()).await;
            }
            Err(e) => {
                let e = DataConnectionEvent::GetPortNumberError(e);
                self.sender.send_up(e.into()).await;
                self.quit_receiver.await.unwrap();
                return;
            }
        }

        loop {
            tokio::select! {
                result = &mut self.quit_receiver => break result.unwrap(),
                event = self.receiver.recv() => {
                    match event.unwrap() {
                        DataConnectionEventFromDevice::Test => (),
                    }
                }
                result = audio_out.accept() => {
                    match result {
                        Ok((connection, address)) => {
                            if address.ip() != self.accept_from.ip() {
                                continue;
                            }

                            let event = match connection.into_std().map(TcpSendConnection::new) {
                                Ok(Ok((connection, handle))) => {
                                    self.connection = Some(connection);
                                    DataConnectionEvent::NewConnection(handle)
                                }
                                Ok(Err(e)) | Err(e) => DataConnectionEvent::SendConnectionError(e),
                            };

                            self.sender.send_up(event.into()).await;
                        }
                        Err(e) => {
                            let e = DataConnectionEvent::AcceptError(e);
                            self.sender.send_up(e.into()).await;
                        }
                    }
                }
            }
        }

        if let Some(connection) = self.connection.take() {
            connection.wait_quit().await;
        }
    }
}
