/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

pub mod io_traits;
pub mod tcp;

use log::{warn, error};

use tokio::{
    net::{TcpStream},
    sync::{oneshot},
    task::JoinHandle, io::{AsyncWrite, AsyncRead},
};

use std::{
    collections::HashMap,
    fmt::Debug,
    io::{self},
    sync::Arc,
    net::{SocketAddr, IpAddr},
};

use crate::{
    config::{LogicConfig},
    utils::{ConnectionId, QuitReceiver, QuitSender}, connection::tcp::ConnectionListener, device::DeviceManagerEvent,
};

use self::tcp::{ListenerError, ConnectionListenerHandle, TcpSendConnection};

use super::{
    message_router::{MessageReceiver, RouterSender},
    ui::UiEvent,
};


#[derive(Debug)]
pub enum ConnectionManagerEvent {
    /// Starts TCP listener task. JSON and data connections will be listened.
    StartTcpListener,
    /// Stop TCP listener task.
    StopTcpListener,
    /// Connect to another Jonect instance which is listening new connections.
    ConnectTo { address: SocketAddr },
    /// Connect to another Jonect instance's data port.
    ConnectToData { id: ConnectionId },
    RemoveDataConnections { id: ConnectionId },
    Internal(CmInternalEventWrapper),
}

impl From<CmInternalEvent> for ConnectionManagerEvent {
    fn from(e: CmInternalEvent) -> Self {
        Self::Internal(CmInternalEventWrapper(e))
    }
}

#[derive(Debug)]
pub struct CmInternalEventWrapper(CmInternalEvent);


#[derive(Debug)]
enum CmInternalEvent {
    ListenerError(ListenerError),
    NewJsonStream(TcpStream, SocketAddr),
    NewDataStream(TcpStream, SocketAddr),
}


pub struct ConnectionManager {
    r_sender: RouterSender,
    receiver: MessageReceiver<ConnectionManagerEvent>,
    next_json_connection_id: ConnectionId,
    data_connections: HashMap<ConnectionId, (IpAddr, Vec<TcpSendConnection>)>,
    config: Arc<LogicConfig>,

}

impl ConnectionManager {
    pub fn start_task(
        r_sender: RouterSender,
        receiver: MessageReceiver<ConnectionManagerEvent>,
        config: Arc<LogicConfig>,
    ) -> (JoinHandle<()>, QuitSender) {
        let (quit_sender, quit_receiver) = oneshot::channel();

        let dm = Self {
            r_sender,
            receiver,
            next_json_connection_id: 0,
            data_connections: HashMap::new(),
            config,
        };

        let task = async move {
            dm.run(quit_receiver).await;
        };

        (tokio::spawn(task), quit_sender)
    }

    pub async fn run(mut self, mut quit_receiver: QuitReceiver) {

        let mut listener: Option<ConnectionListenerHandle> = None;

        loop {
            tokio::select! {
                result = &mut quit_receiver => break result.unwrap(),
                event = self.receiver.recv() => {
                    match event {
                        // Prevent race conditions related to starting and stopping new tasks.
                        // Otherwise task handle might be dropped.
                        ConnectionManagerEvent::StartTcpListener => {
                            listener = Some(ConnectionListener::start_task(self.r_sender.clone()));
                        }
                        ConnectionManagerEvent::StopTcpListener => {
                            if let Some(handle) = listener.take() {
                                handle.quit().await
                            }
                        }
                        event => {
                            tokio::select! {
                                result = &mut quit_receiver => break result.unwrap(),
                                _ = self.handle_cm_event(event) => (),
                            };
                        }
                    }
                }
            }
        }

        // Quit

        if let Some(handle) = listener.take() {
            handle.quit().await
        }

        for (ip, connections) in self.data_connections.into_values() {
            for data_connection in connections {
                data_connection.wait_quit().await
            }
        }
    }

    async fn handle_cm_event(&mut self, event: ConnectionManagerEvent) {
        match event {
            ConnectionManagerEvent::Internal(CmInternalEventWrapper(event)) => match event {
                CmInternalEvent::ListenerError(error) => {
                    let e = UiEvent::TcpSupportDisabledBecauseOfError(error);
                    self.r_sender.send_ui_event(e).await;
                }
                CmInternalEvent::NewJsonStream(stream, address) => {
                    let id = self.next_json_connection_id;
                    self.next_json_connection_id = match id.checked_add(1) {
                        Some(new_next_id) => new_next_id,
                        None => {
                            warn!("Warning: Couldn't handle a new connection because there is no new connection ID numbers.");
                            return;
                        }
                    };

                    self.data_connections.insert(id, (address.ip(), Vec::new()));

                    let e = DeviceManagerEvent::NewDeviceConnection(JsonConnection::new(stream, address, id));
                    self.r_sender.send_device_manager_event(e).await;
                }
                CmInternalEvent::NewDataStream(stream, address) => {
                    let mut data_connections = None;
                    for (id, (connection_address, connections)) in self.data_connections.iter_mut() {

                        if *connection_address == address.ip() {
                            data_connections = Some((id, connections));
                        }
                    }

                    let (id, data_connections) = if let Some((id, data_connections)) = data_connections {
                        (id, data_connections)
                    } else {
                        return;
                    };

                    match stream.into_std().map(TcpSendConnection::new) {
                        Ok(Ok((connection, handle))) => {
                            data_connections.push(connection);

                            let e = DeviceManagerEvent::NewDataConnection(*id, handle);
                            self.r_sender.send_device_manager_event(e).await;
                        }
                        Ok(Err(e)) | Err(e) => {
                            error!("Error: {:?}", e);
                        }
                    }
                }
            },
            ConnectionManagerEvent::ConnectTo { address } => {
                let stream = match TcpStream::connect(address).await {
                    Ok(stream) => stream,
                    Err(e) => {
                        error!("ConnectTo error: {}", e);
                        return;
                    }
                };

                self.r_sender.send_connection_manager_event(
                    CmInternalEvent::NewJsonStream(stream, address).into()
                ).await;
            }
            ConnectionManagerEvent::ConnectToData { id } => {
                let address = if let Some((ip, _)) = self.data_connections.get(&id) {
                    (*ip, crate::config::DATA_PORT).into()
                } else {
                    return;
                };

                let stream = match TcpStream::connect(address).await {
                    Ok(stream) => stream,
                    Err(e) => {
                        error!("ConnectToData error: {}", e);
                        return;
                    }
                };

                self.r_sender.send_connection_manager_event(
                    CmInternalEvent::NewDataStream(stream, address).into()
                ).await;
            }
            ConnectionManagerEvent::RemoveDataConnections { id } => {
                if let Some((ip, connections)) = self.data_connections.remove(&id) {
                    for data_connection in connections {
                        data_connection.wait_quit().await
                    }
                }
            }
            ConnectionManagerEvent::StartTcpListener | ConnectionManagerEvent::StopTcpListener => {
                panic!("Event handling error: StartTcpListener or StopTcpListener received.")
            }
        }
    }


}

#[derive(Debug)]
pub struct JsonConnection {
    tcp: TcpStream,
    address: SocketAddr,
    json_connection_id: ConnectionId,
}

impl JsonConnection {
    fn new(tcp: TcpStream, address: SocketAddr, json_connection_id: ConnectionId) -> Self {
        Self {
            address,
            json_connection_id,
            tcp,
        }
    }

    pub fn id(&self) -> u64 {
        self.json_connection_id
    }
}

impl AsyncRead for JsonConnection {
    fn poll_read(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.tcp).poll_read(cx, buf)
    }
}

impl AsyncWrite for JsonConnection {
    fn poll_write(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<Result<usize, io::Error>> {
        std::pin::Pin::new(&mut self.tcp).poll_write(cx, buf)
    }

    fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), io::Error>> {
        std::pin::Pin::new(&mut self.tcp).poll_shutdown(cx)
    }

    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), io::Error>> {
        std::pin::Pin::new(&mut self.tcp).poll_flush(cx)
    }
}
