/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! UDP data connection code.

use std::{net::{UdpSocket, IpAddr, ToSocketAddrs}, time::Duration, io};

use crate::{connection::data::MAX_PACKET_SIZE, config::{DATA_PORT_UDP_SEND, DATA_PORT_UDP_RECEIVE}};

use super::{DataSenderInterface, DataReceiverInterface, DataReceiverBuilderInterface, DataReceiverBuilder, DataSenderBuilderInterface, DataSenderBuilder};

pub struct UdpManager {
    udp_socket_send: Option<UdpSocket>,
    udp_socket_receive: Option<UdpSocket>,
}

impl UdpManager {
    pub fn new() -> Result<Self, UdpError> {
        Ok(Self {
            udp_socket_send: None,
            udp_socket_receive: None
        })
    }

    pub fn build_mode_receive(&mut self, json_connection_ip: IpAddr) -> Result<DataReceiverBuilder, UdpError> {
        let udp_socket_receive = match self.udp_socket_receive.take() {
            Some(socket) => socket,
            None => {
                let address  = ("127.0.0.1", DATA_PORT_UDP_RECEIVE).to_socket_addrs().unwrap().next().unwrap();
                UdpSocket::bind(address).map_err(UdpError::Bind)?
            }
        };

        // TODO: If UDP socket is used other place than AudioManager in the
        // future, there is possiblity for receiving data from same socket
        // multiple times.
        let socket = udp_socket_receive.try_clone().map_err(UdpError::Clone)?;
        let connection_address = (json_connection_ip, DATA_PORT_UDP_SEND).to_socket_addrs().unwrap().next().unwrap();
        socket.connect(connection_address).map_err(UdpError::Connect)?;

        self.udp_socket_receive = Some(udp_socket_receive);
        UdpDataConnectionBuilder::build_mode_receive(socket)
    }

    pub fn build_mode_send(&mut self, json_connection_ip: IpAddr) -> Result<DataSenderBuilder, UdpError> {
        let udp_socket_send = match self.udp_socket_send.take() {
            Some(socket) => socket,
            None => {
                let address  = ("127.0.0.1", DATA_PORT_UDP_SEND).to_socket_addrs().unwrap().next().unwrap();
                UdpSocket::bind(address).map_err(UdpError::Bind)?
            }
        };

        let socket = udp_socket_send.try_clone().map_err(UdpError::Clone)?;
        let connection_address = (json_connection_ip, DATA_PORT_UDP_RECEIVE).to_socket_addrs().unwrap().next().unwrap();
        socket.connect(connection_address).map_err(UdpError::Connect)?;

        self.udp_socket_send = Some(udp_socket_send);
        UdpDataConnectionBuilder::build_mode_send(socket)
    }
}

#[derive(Debug)]
pub enum UdpError {
    Bind(io::Error),
    SetNonblokingCall(io::Error),
    Clone(io::Error),
    Connect(io::Error),
}

#[derive(Debug)]
pub struct UdpDataConnectionBuilder {
    udp_socket: UdpSocket,
}

impl UdpDataConnectionBuilder {
    pub fn build_mode_receive(udp_socket: UdpSocket) -> Result<DataReceiverBuilder, UdpError> {
        udp_socket.set_nonblocking(false).map_err(UdpError::SetNonblokingCall)?;

        Ok(Box::new(Self {
            udp_socket
        }))
    }

    pub fn build_mode_send(udp_socket: UdpSocket) -> Result<DataSenderBuilder, UdpError> {
        udp_socket.set_nonblocking(false).map_err(UdpError::SetNonblokingCall)?;

        Ok(Box::new(Self {
            udp_socket
        }))
    }
}

impl DataReceiverBuilderInterface for UdpDataConnectionBuilder {
    fn is_reliable_connection(&self) -> bool {
        false
    }

    fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<(), std::io::Error> {
        self.udp_socket.set_read_timeout(timeout)
    }

    fn build(self: Box<Self>) -> Box<dyn DataReceiverInterface> {
        Box::new(UdpDataConnection::new(self.udp_socket))
    }
}

impl DataSenderBuilderInterface for UdpDataConnectionBuilder {
    fn is_reliable_connection(&self) -> bool {
        false
    }

    fn build(self: Box<Self>) -> Box<dyn DataSenderInterface> {
        Box::new(UdpDataConnection::new(self.udp_socket))
    }
}

#[derive(Debug)]
struct UdpDataConnection {
    udp_socket: std::net::UdpSocket,
}

impl UdpDataConnection {
    pub fn new(udp_socket: std::net::UdpSocket) -> Self {
        Self {
            udp_socket
        }
    }
}

impl DataSenderInterface for UdpDataConnection {
    fn is_reliable_connection(&self) -> bool {
        false
    }

    fn send_packet(&mut self, packet: &[u8]) -> Result<(), std::io::Error> {
        assert!(packet.len() <= MAX_PACKET_SIZE);

        self.udp_socket.send(packet)?;

        Ok(())
    }
}

impl DataReceiverInterface for UdpDataConnection {
    fn is_reliable_connection(&self) -> bool {
        false
    }

    fn recv_packet(&mut self, buffer: &mut [u8]) -> Result<usize, std::io::Error> {
        self.udp_socket.recv(buffer)
    }
}
