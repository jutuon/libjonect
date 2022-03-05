/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use log::error;

use tokio::{
    net::{TcpListener, TcpStream},
    sync::{oneshot, mpsc},
    task::JoinHandle,
};

use std::{
    fmt::Debug,
    io::{self, Write, Read}, time::Duration,
};

use crate::{
    config::{self},
    utils::{QuitReceiver, QuitSender, ConnectionShutdownWatch},
};

use crate::{
    message_router::{RouterSender},
};

use super::{
    CmInternalEvent, data::{DataSenderInterface, DataReceiverInterface, DataReceiverBuilderInterface},
};

#[derive(Debug)]
pub enum ListenerError {
    BindJsonSocket(io::Error),
    BindDataSocket(io::Error),
    AcceptJsonConnection(io::Error),
    AcceptDataConnection(io::Error),
}

pub struct ConnectionListenerHandle {
    join_handle: JoinHandle<()>,
    quit_sender: QuitSender,
}

impl ConnectionListenerHandle {
    /// This will block until `ConnectionListener` quits.
    pub async fn quit(self) {
        self.quit_sender.send(()).unwrap();
        self.join_handle.await.unwrap();
    }
}

pub struct ConnectionListener {
    r_sender: RouterSender,
}

impl ConnectionListener {
    pub fn start_task(
        r_sender: RouterSender,
    ) -> ConnectionListenerHandle {
        let (quit_sender, quit_receiver) = oneshot::channel();

        let cl = Self {
            r_sender,
        };

        let task = async move {
            cl.run(quit_receiver).await;
        };

        ConnectionListenerHandle {
            join_handle: tokio::spawn(task),
            quit_sender,
        }
    }

    pub async fn run(mut self, mut quit_receiver: QuitReceiver) {
        let json_listener = match TcpListener::bind(config::DEVICE_SOCKET_ADDRESS).await {
            Ok(listener) => listener,
            Err(e) => {
                let e = ListenerError::BindJsonSocket(e);
                let e = CmInternalEvent::ListenerError(e).into();

                tokio::select! {
                    result = &mut quit_receiver => return result.unwrap(),
                    _ = self.r_sender.send_connection_manager_event(e) => (),
                };

                // Wait quit message.
                quit_receiver.await.unwrap();
                return;
            }
        };

        let data_listener = match TcpListener::bind(config::AUDIO_DATA_SOCKET_ADDRESS).await {
            Ok(listener) => listener,
            Err(e) => {
                let e = ListenerError::BindDataSocket(e);
                let e = CmInternalEvent::ListenerError(e).into();

                tokio::select! {
                    result = &mut quit_receiver => return result.unwrap(),
                    _ = self.r_sender.send_connection_manager_event(e) => (),
                };

                // Wait quit message.
                quit_receiver.await.unwrap();
                return;
            }
        };

        let logic = async {
            loop {
                tokio::select! {
                    result = json_listener.accept() => {
                        match result {
                            Ok((stream, address)) => {
                                self.r_sender.send_connection_manager_event(
                                    CmInternalEvent::NewJsonStream(stream, address).into(),
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
                    result = data_listener.accept() => {
                        match result {
                            Ok((stream, address)) => {
                                self.r_sender.send_connection_manager_event(
                                    CmInternalEvent::NewDataStream(stream, address).into(),
                                ).await;
                            }
                            Err(error) => {
                                let e = ListenerError::AcceptDataConnection(error);
                                self.r_sender.send_connection_manager_event(
                                    CmInternalEvent::ListenerError(e).into()
                                ).await;

                                break;
                            }
                        }
                    },
                };
            }

        };

        tokio::select! {
            result = &mut quit_receiver => result.unwrap(),
            _ = logic => {
                quit_receiver.await.unwrap();
            },
        }
    }
}


/*


/// Send data to connected device. Writing will be nonblocking. Drop this to close
/// `TcpSendConnection`.
#[derive(Debug)]
pub struct TcpSendHandle {
    tcp_stream: std::net::TcpStream,
    _shutdown_watch: ConnectionShutdownWatch,
}

impl TcpSendHandle {
    // TODO: remove this
    pub fn tokio_tcp(self) -> TcpStream {
        TcpStream::from_std(self.tcp_stream).unwrap()
    }

    pub fn set_blocking(&mut self) -> Result<(), io::Error> {
        self.tcp_stream.set_nonblocking(false)
    }
}

impl std::io::Write for TcpSendHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.tcp_stream.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.tcp_stream.flush()
    }
}

impl std::io::Read for TcpSendHandle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.tcp_stream.read(buf)
    }
}


/// Handle to `TcpSendHandle`.
#[derive(Debug)]
pub struct TcpSendConnection {
    shutdown_watch_receiver: mpsc::Receiver<()>,
    tcp_stream: std::net::TcpStream,
}

impl TcpSendConnection {
    /// Create `TcpSendConnection` and `TcpSendHandle`.
    pub fn new(tcp_stream: std::net::TcpStream) -> Result<(Self, TcpSendHandle), std::io::Error> {
        tcp_stream.set_nonblocking(true)?;
        let (_shutdown_watch, shutdown_watch_receiver) = mpsc::channel::<()>(1);

        let handle = TcpSendHandle {
            tcp_stream: tcp_stream.try_clone()?,
            _shutdown_watch,
        };

        let connection = Self {
            shutdown_watch_receiver,
            tcp_stream,
        };

        Ok((connection, handle))
    }

    /// Wait until `TcpSendHandle` is dropped.
    pub async fn wait_quit(mut self) {
        match self.tcp_stream.shutdown(std::net::Shutdown::Both) {
            Ok(()) => {
                // Wait until `TcpSendHandle` is dropped.
                let _ = self.shutdown_watch_receiver.recv().await;
            }
            Err(e) => {
                error!("TcpSendConnection error: {}", e);
            }
        }
    }
}

 */
