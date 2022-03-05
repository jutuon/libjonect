/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! TCP data connection code.

use std::{net::TcpStream, time::Duration, io::{Write, Read}};

use crate::connection::data::MAX_PACKET_SIZE;

use super::{DataSenderInterface, DataReceiverInterface, DataReceiverBuilderInterface, DataReceiverBuilder, DataSenderBuilder, DataSenderBuilderInterface};

#[derive(Debug)]
pub struct TcpDataConnectionBuilder {
    tcp_stream: TcpStream,
}

impl TcpDataConnectionBuilder {
    pub fn build_mode_receive(tcp_stream: std::net::TcpStream) -> DataReceiverBuilder {
        tcp_stream.set_nonblocking(false).unwrap();

        Box::new( Self {
            tcp_stream
        })
    }

    pub fn build_mode_send(tcp_stream: std::net::TcpStream) -> DataSenderBuilder {
        tcp_stream.set_nonblocking(true).unwrap();
        Box::new(Self {
            tcp_stream
        })
    }
}

impl DataReceiverBuilderInterface for TcpDataConnectionBuilder {
    fn is_reliable_connection(&self) -> bool {
        true
    }

    fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<(), std::io::Error> {
        self.tcp_stream.set_read_timeout(timeout)
    }

    fn build(self: Box<Self>) -> Box<dyn DataReceiverInterface> {
        Box::new(TcpDataConnection::new(self.tcp_stream))
    }
}

impl DataSenderBuilderInterface for TcpDataConnectionBuilder {
    fn is_reliable_connection(&self) -> bool {
        true
    }

    fn build(self: Box<Self>) -> Box<dyn DataSenderInterface> {
        Box::new(TcpDataConnection::new(self.tcp_stream))
    }
}

#[derive(Debug)]
struct TcpDataConnection {
    tcp_stream: std::net::TcpStream,
}

impl TcpDataConnection {
    pub fn new(mut tcp_stream: std::net::TcpStream) -> Self {
        Self {
            tcp_stream
        }
    }
}

impl DataSenderInterface for TcpDataConnection {
    fn is_reliable_connection(&self) -> bool {
        true
    }

    fn send_packet(&mut self, packet: &[u8]) -> Result<(), std::io::Error> {
        assert!(packet.len() <= MAX_PACKET_SIZE);

        let packet_size: u16 = packet.len().try_into().unwrap();

        // TODO: Do buffering?

        self.tcp_stream.write_all(&packet_size.to_be_bytes())?;
        self.tcp_stream.write_all(packet)?;

        Ok(())
    }
}

impl DataReceiverInterface for TcpDataConnection {
    fn is_reliable_connection(&self) -> bool {
        true
    }

    fn recv_packet(&mut self, buffer: &mut [u8]) -> Result<usize, std::io::Error> {
        let mut size_bytes = [0u8; 2];
        self.tcp_stream.read_exact(&mut size_bytes)?;

        let size = u16::from_be_bytes(size_bytes) as usize;
        let split_size = usize::min(size, buffer.len());

        let (packet_buffer, _) = buffer.split_at_mut(split_size);

        self.tcp_stream.read_exact(packet_buffer).map(|_| split_size)
    }
}
