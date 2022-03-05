/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

pub mod tcp;
pub mod data;

use log::{warn, error};

use serde::{Serialize, Deserialize};
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
    utils::{ConnectionId, QuitReceiver, QuitSender}, connection::tcp::ConnectionListener, device::DeviceManagerEvent, audio::{AudioEvent, PlayAudioEventAndroid},
};

use self::{tcp::{ListenerError, ConnectionListenerHandle}, data::tcp::TcpDataConnectionBuilder};

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
    ConnectToData {
        id: ConnectionId,
        next_resource_request: NextResourceRequest,
        data_connection_type: DataConnectionType,
    },
    RemoveDataConnections { id: ConnectionId },
    SetNextResourceRequest {
        id: ConnectionId,
        next_resource_request: NextResourceRequest,
        data_connection_type: DataConnectionType,
    },
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

pub struct ResourceHandle {
    connection_id: ConnectionId,
}

type AudioQuitEvent = AudioEvent;

pub struct ResourceManager {
    play_audio: Option<(ConnectionId, AudioQuitEvent)>,
    send_audio: Option<(ConnectionId, AudioQuitEvent)>,
}

impl ResourceManager {
    fn new() -> Self {
        Self {
            play_audio: None,
            send_audio: None,
        }
    }

    fn connect_play_audio(&mut self, id: ConnectionId) -> Result<(), ()> {
        if self.play_audio.is_some() {
            return Err(());
        }

        self.play_audio = Some((id, AudioEvent::StopPlayingAudio));

        Ok(())
    }

    fn connect_send_audio(&mut self, id: ConnectionId) -> Result<(), ()> {
        if self.send_audio.is_some() {
            return Err(());
        }

        self.send_audio = Some((id, AudioEvent::StopRecording));

        Ok(())
    }

    async fn request_disconnect_resources_for(&mut self, id: ConnectionId, r_sender: &mut RouterSender) {
        if let Some((resource_connection_id, event)) = self.play_audio.take() {
            if resource_connection_id == id {
                r_sender.send_audio_server_event(event).await;
            } else {
                self.play_audio = Some((resource_connection_id, event));
            }
        }

        if let Some((resource_connection_id, event)) = self.send_audio.take() {
            if resource_connection_id == id {
                r_sender.send_audio_server_event(event).await;
            } else {
                self.send_audio = Some((resource_connection_id, event));
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum NextResourceRequest {
    PlayAudio {
        sample_rate: i32,
        android_info: Option<PlayAudioEventAndroid>,
        decode_opus: bool,
    },
    SendAudio { sample_rate: u32 },
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
pub enum DataConnectionType {
    Tcp,
    Udp,
}

pub struct ConnectionResources {
    id: ConnectionId,
    ip: IpAddr,
    pending_data_connection_tcp: Option<std::net::TcpStream>,
    next_resource_request: Option<(NextResourceRequest, DataConnectionType)>,
}

impl ConnectionResources {
    fn new(id: ConnectionId, ip: IpAddr) -> Self {
        Self {
            id,
            ip,
            pending_data_connection_tcp: None,
            next_resource_request: None,
        }
    }

    fn ip(&self) -> IpAddr {
        self.ip
    }

    fn set_pending_tcp_connection(&mut self, tcp_stream: std::net::TcpStream) {
        self.pending_data_connection_tcp = Some(tcp_stream);
    }

    fn check_next_resource_request(&mut self, resource_manager: &mut ResourceManager) -> Option<AudioEvent> {
        match (self.next_resource_request, self.pending_data_connection_tcp.take()) {
            (Some((NextResourceRequest::SendAudio { sample_rate }, DataConnectionType::Tcp)), Some(stream)) => {
                if resource_manager.connect_send_audio(self.id).is_ok() {
                    Some(AudioEvent::StartRecording {
                        send_handle: TcpDataConnectionBuilder::build_mode_send(stream),
                        sample_rate,
                    })
                } else {
                    warn!("SendAudio resource is not available.");
                    None
                }
            }
            (
                Some((
                NextResourceRequest::PlayAudio {
                    sample_rate,
                    android_info,
                    decode_opus
                },
                DataConnectionType::Tcp
                )), Some(stream)
            ) => {
                if resource_manager.connect_play_audio(self.id).is_ok() {
                    Some(AudioEvent::PlayAudio {
                        send_handle: TcpDataConnectionBuilder::build_mode_receive(stream),
                        sample_rate,
                        android_info,
                        decode_opus,
                    })
                } else {
                    warn!("PlayAudio resource is not available.");
                    None
                }
            }
            (Some((_, DataConnectionType::Tcp)), None) => {
                None
            }
            (_, Some(stream)) => {
                self.pending_data_connection_tcp = Some(stream);
                None
            },
            _ => unimplemented!(),
        }


    }

    fn set_next_resource_request(
        &mut self,
        request: NextResourceRequest,
        data_connection: DataConnectionType
    ) {
        // TODO: What if there will be multiple resource requests at the same time.
        self.next_resource_request = Some((request, data_connection));
    }

    async fn send_quit_request(self, resource_manager: &mut ResourceManager, r_sender: &mut RouterSender) {
        resource_manager.request_disconnect_resources_for(self.id, r_sender).await;
    }
}

pub struct ConnectionManager {
    r_sender: RouterSender,
    receiver: MessageReceiver<ConnectionManagerEvent>,
    next_json_connection_id: ConnectionId,
    json_connections: HashMap<ConnectionId, ConnectionResources>,
    config: Arc<LogicConfig>,
    resource_manager: ResourceManager,
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
            json_connections: HashMap::new(),
            config,
            resource_manager: ResourceManager::new(),
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

        // Other components just drop connection related objects so there is no
        // need to close anything.
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

                    self.json_connections.insert(id, ConnectionResources::new(id, address.ip()));

                    let e = DeviceManagerEvent::NewDeviceConnection(JsonConnection::new(stream, address, id));
                    self.r_sender.send_device_manager_event(e).await;
                }
                CmInternalEvent::NewDataStream(stream, address) => {
                    let mut connection = None;
                    for (id, connection_resources) in self.json_connections.iter_mut() {
                        if connection_resources.ip() == address.ip() {
                            connection = Some((id, connection_resources));
                        }
                    }

                    let (id, connection_resources) = if let Some((id, connection_resources)) = connection {
                        (id, connection_resources)
                    } else {
                        return;
                    };

                    match stream.into_std() {
                        Ok(stream) => {
                            connection_resources.set_pending_tcp_connection(stream);
                            if let Some(event) = connection_resources.check_next_resource_request(&mut self.resource_manager) {
                                self.r_sender.send_audio_server_event(event).await;
                            }
                        }
                        Err(e) => {
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
            ConnectionManagerEvent::ConnectToData { id, next_resource_request, data_connection_type } => {
                // TODO: Currently the case where TCP data connection breaks but
                // TCP JSON connection still works is unhandled. Workaround for
                // this is manual connection restart as AudioManager does not
                // send event about broken TCP data connection to the
                // ConnectionManager.

                match data_connection_type {
                    DataConnectionType::Tcp => {
                        let address = if let Some(connection_resources) = self.json_connections.get_mut(&id) {
                            connection_resources.set_next_resource_request(next_resource_request, data_connection_type);
                            (connection_resources.ip(), crate::config::DATA_PORT).into()
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
                    },
                    DataConnectionType::Udp => {
                        unimplemented!()
                    },
                };
            }
            ConnectionManagerEvent::RemoveDataConnections { id } => {
                if let Some(connection_resources) = self.json_connections.remove(&id) {
                    connection_resources.send_quit_request(&mut self.resource_manager, &mut self.r_sender).await;
                }
            }
            ConnectionManagerEvent::StartTcpListener | ConnectionManagerEvent::StopTcpListener => {
                panic!("Event handling error: StartTcpListener or StopTcpListener received.")
            }
            ConnectionManagerEvent::SetNextResourceRequest {
                id, next_resource_request, data_connection_type
            } => {
                if let Some(connection_resources) = self.json_connections.get_mut(&id) {
                    connection_resources.set_next_resource_request(next_resource_request, data_connection_type);
                }
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
