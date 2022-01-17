/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Some utility types.

use bytes::BytesMut;
use serde::{de::DeserializeOwned, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use std::{
    convert::TryInto,
    fmt::{self, Debug},
    io::ErrorKind,
};

use crate::config::EVENT_CHANNEL_SIZE;

/// Handle for indicating running device data stream connections.
///
/// Drop this type with some connection.
pub type ConnectionShutdownWatch = mpsc::Sender<()>;

/// Sender only used for quit request message sending.
pub type QuitSender = oneshot::Sender<()>;

/// Receiver only used for quit request message receiving.
pub type QuitReceiver = oneshot::Receiver<()>;

/// Internally used ID number for device connections.
pub type ConnectionId = u64;

/// Wrapper for tokio::sync::mpsc::Sender for sending events upwards in the
/// task tree. This exist to make code more readable.
#[derive(Debug)]
pub struct SendUpward<T: fmt::Debug> {
    sender: mpsc::Sender<T>,
}

impl<T: fmt::Debug> SendUpward<T> {
    /// Panic if channel is broken.
    pub async fn send_up(&self, data: T) {
        self.sender.send(data).await.expect("Error: broken channel");
    }

    pub fn clone(&self) -> Self {
        self.sender.clone().into()
    }
}

impl<T: fmt::Debug> From<mpsc::Sender<T>> for SendUpward<T> {
    /// Convert `Sender` to `SendUpward`.
    fn from(sender: mpsc::Sender<T>) -> Self {
        Self { sender }
    }
}

/// Wrapper for tokio::sync::mpsc::Sender for sending events downwards in the
/// task tree. This exist to make code more readable.
#[derive(Debug)]
pub struct SendDownward<T: fmt::Debug> {
    sender: mpsc::Sender<T>,
}

impl<T: fmt::Debug> SendDownward<T> {
    /// Panic if channel is broken.
    pub async fn send_down(&self, data: T) {
        self.sender.send(data).await.expect("Error: broken channel");
    }
}

impl<T: fmt::Debug> From<mpsc::Sender<T>> for SendDownward<T> {
    /// Convert `Sender` to SendDownward`.
    fn from(sender: mpsc::Sender<T>) -> Self {
        Self { sender }
    }
}

/// Handle for `Connection` task which sends and receives messages.
///
/// Panic might happen if connection handle is dropped before the task
/// is closed.
pub struct ConnectionHandle<SendM: Debug> {
    id: ConnectionId,
    task_handle: JoinHandle<()>,
    sender: SendDownward<SendM>,
    quit_sender: oneshot::Sender<()>,
}

impl<M: Debug> ConnectionHandle<M> {
    /// Create new connection handle.
    pub fn new(
        id: ConnectionId,
        task_handle: JoinHandle<()>,
        sender: SendDownward<M>,
        quit_sender: oneshot::Sender<()>,
    ) -> Self {
        Self {
            id,
            task_handle,
            sender,
            quit_sender,
        }
    }

    /// Get connection id.
    pub fn id(&self) -> ConnectionId {
        self.id
    }

    /// Run quit. Returns when `Connection` task is closed.
    pub async fn quit(self) {
        self.quit_sender.send(()).unwrap();
        self.task_handle.await.unwrap();
    }

    /// Send message to the send queue.
    pub async fn send_down(&self, message: M) {
        self.sender.send_down(message).await
    }
}

/// Event from `Connection` task.
#[derive(Debug)]
pub enum ConnectionEvent<M> {
    /// Reading error occurred. No new messages will be received or sent. `Connection`
    /// task is waiting quit request.
    ReadError(ConnectionId, ReadError),
    /// Writing error occurred. No new messages will be received or sent. `Connection`
    /// task is waiting quit request.
    WriteError(ConnectionId, WriteError),
    /// New message from connections.
    Message(ConnectionId, M),
}

impl<M> ConnectionEvent<M> {
    /// Returns true if event is `ReadError` or `WriteError`.
    pub fn is_error(&self) -> bool {
        match self {
            Self::ReadError(_, _) | Self::WriteError(_, _) => true,
            Self::Message(_, _) => false,
        }
    }
}

/// `Connection` task reading errors.
#[derive(Debug)]
pub enum ReadError {
    Io(std::io::Error),
    Deserialize(serde_json::error::Error),
    /// Negative message size (-1 or less).
    MessageSize(i32),
}

/// `Connection` task writing errors.
#[derive(Debug)]
pub enum WriteError {
    Io(std::io::Error),
    Serialize(serde_json::error::Error),
    /// Serialized message byte count does not fit in `i32` data type.
    ///
    /// Returns serialized message (JSON data).
    MessageSize(String),
}

/// `Connection` task.
#[derive(Debug)]
pub struct Connection<
    R: AsyncReadExt + Unpin + Send + 'static,
    W: AsyncWriteExt + Unpin + Send + 'static,
    ReceiveM: DeserializeOwned + Debug + Send + 'static,
    SendM: Serialize + Debug + Send + 'static,
> {
    id: ConnectionId,
    read_half: R,
    write_half: W,
    sender: SendUpward<ConnectionEvent<ReceiveM>>,
    receiver: mpsc::Receiver<SendM>,
    quit_receiver: oneshot::Receiver<()>,
}

impl<
        R: AsyncReadExt + Unpin + Send + 'static,
        W: AsyncWriteExt + Unpin + Send + 'static,
        ReceiveM: DeserializeOwned + Debug + Send + 'static,
        SendM: Serialize + Debug + Send + 'static,
    > Connection<R, W, ReceiveM, SendM>
{
    /// Spawns new connection task.
    pub fn spawn_connection_task(
        id: ConnectionId,
        read_half: R,
        write_half: W,
        sender: SendUpward<ConnectionEvent<ReceiveM>>,
    ) -> ConnectionHandle<SendM> {
        let (event_sender, receiver) = mpsc::channel(EVENT_CHANNEL_SIZE);
        let (quit_sender, quit_receiver) = oneshot::channel();

        let connection = Self {
            id,
            sender,
            receiver,
            quit_receiver,
            read_half,
            write_half,
        };

        let task_handle = tokio::spawn(async move { connection.connection_task().await });

        ConnectionHandle::new(id, task_handle, event_sender.into(), quit_sender)
    }

    /// Creates new connection task future.
    pub async fn connection_task(mut self) {
        let buffer = BytesMut::new();
        let r_task = Self::read_message(buffer, self.read_half);
        tokio::pin!(r_task);

        // TODO: Use self.receiver as w_receiver instead crating a new channel?
        let (w_sender, w_receiver) = mpsc::channel(EVENT_CHANNEL_SIZE);
        let w_task = Self::handle_writing(w_receiver, self.write_half);
        tokio::pin!(w_task);

        // TODO: Use one select for checking quit messages. This can be done
        // when logic is moved to another future using for example an async
        // block.

        loop {
            tokio::select!(
                result = &mut self.quit_receiver => {
                    result.unwrap();
                    return;
                }
                event = self.receiver.recv() => {
                    tokio::select!(
                        result = w_sender.send(event.unwrap()) => result.unwrap(),
                        quit = &mut self.quit_receiver => return quit.unwrap(),
                    );
                    continue;
                }
                (result, buffer, read) = &mut r_task => {
                    r_task.set(Self::read_message(buffer, read));

                    match result {
                        Ok(message) => {
                            let event = ConnectionEvent::Message(self.id, message);
                            tokio::select!(
                                _ = self.sender.send_up(event) => (),
                                quit = &mut self.quit_receiver => return quit.unwrap(),
                            );
                        }

                        Err(e) => {
                            let event = ConnectionEvent::ReadError(self.id, e);
                            tokio::select!(
                                _ = self.sender.send_up(event) => (),
                                quit = &mut self.quit_receiver => return quit.unwrap(),
                            );
                            break;
                        }
                    }
                }
                error = &mut w_task => {
                    let event = ConnectionEvent::WriteError(self.id, error);
                    tokio::select!(
                        _ = self.sender.send_up(event) => (),
                        quit = &mut self.quit_receiver => return quit.unwrap(),
                    );
                    break;
                }
            );
        }

        self.quit_receiver.await.unwrap();
    }

    /// Creates new message reading future.
    async fn read_message(
        mut buffer: BytesMut,
        mut read_half: R,
    ) -> (Result<ReceiveM, ReadError>, BytesMut, R) {
        buffer.clear();

        let message_len = match read_half.read_i32().await.map_err(ReadError::Io) {
            Ok(len) => len,
            Err(e) => return (Err(e), buffer, read_half),
        };

        if message_len.is_negative() {
            return (Err(ReadError::MessageSize(message_len)), buffer, read_half);
        }

        let mut read_half_with_limit = read_half.take(message_len as u64);

        loop {
            match read_half_with_limit.read_buf(&mut buffer).await {
                Ok(0) => {
                    if buffer.len() == message_len as usize {
                        let message =
                            serde_json::from_slice(&buffer).map_err(ReadError::Deserialize);
                        return (message, buffer, read_half_with_limit.into_inner());
                    } else {
                        let error = std::io::Error::new(ErrorKind::UnexpectedEof, "");
                        return (
                            Err(ReadError::Io(error)),
                            buffer,
                            read_half_with_limit.into_inner(),
                        );
                    }
                }
                Ok(_) => (),
                Err(e) => {
                    return (
                        Err(ReadError::Io(e)),
                        buffer,
                        read_half_with_limit.into_inner(),
                    )
                }
            }
        }
    }

    /// Create new message writing future.
    async fn handle_writing(mut receiver: mpsc::Receiver<SendM>, mut write_half: W) -> WriteError {
        loop {
            let message = receiver.recv().await.unwrap();

            let data = match serde_json::to_string(&message) {
                Ok(data) => data,
                Err(e) => {
                    return WriteError::Serialize(e);
                }
            };

            let data_len: i32 = match data.len().try_into() {
                Ok(len) => len,
                Err(_) => {
                    return WriteError::MessageSize(data);
                }
            };

            let data_len = data_len.to_be_bytes();
            if let Err(e) = write_half.write_all(&data_len).await {
                return WriteError::Io(e);
            };

            if let Err(e) = write_half.write_all(data.as_bytes()).await {
                return WriteError::Io(e);
            };
        }
    }
}
