/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! USB data connection code.

use std::{net::{UdpSocket, IpAddr, ToSocketAddrs, SocketAddr}, time::Duration, io::{self, ErrorKind}, sync::mpsc::{Receiver, Sender, SendError, RecvTimeoutError, TryRecvError, RecvError}};

use crate::{connection::{data::MAX_PACKET_SIZE, usb::{protocol::{USB_PACKET_MAX_DATA_SIZE, UsbPacket}, UsbPacketWrapper}}, config::{DATA_PORT_UDP_SEND, DATA_PORT_UDP_SEND_ADDRESS, DATA_PORT_UDP_RECEIVE, DATA_PORT_UDP_RECEIVE_ADDRESS}};

use super::{DataSenderInterface, DataReceiverInterface, DataReceiverBuilderInterface, DataReceiverBuilder, DataSenderBuilderInterface, DataSenderBuilder};


#[derive(Debug)]
pub struct UsbDataConnectionBuilder;

impl UsbDataConnectionBuilder {
    pub fn build_sender() -> (DataSenderBuilder, DataReceiverBuilder) {
        let (sender, receiver) = std::sync::mpsc::channel();

        let usb_sender = Box::new(UsbDataConnectionSender {
            sender,
        });

        let usb_receiver = Box::new(UsbDataConnectionReceiver {
            receiver,
            timeout: None,
            nonbloking: false,
        });

        (usb_sender, usb_receiver)
    }

    pub fn build_receiver() -> (UsbDataConnectionSender, DataReceiverBuilder) {
        let (sender, receiver) = std::sync::mpsc::channel();

        let usb_sender = UsbDataConnectionSender {
            sender,
        };

        let usb_receiver = Box::new(UsbDataConnectionReceiver {
            receiver,
            timeout: None,
            nonbloking: false,
        });

        (usb_sender, usb_receiver)
    }
}


#[derive(Debug)]
pub struct UsbDataConnectionSender {
    sender: Sender<UsbPacketWrapper>,
}

impl UsbDataConnectionSender {
    pub fn new(sender: Sender<UsbPacketWrapper>) -> Self {
        Self {
            sender,
        }
    }
}

impl DataSenderBuilderInterface for UsbDataConnectionSender {
    fn is_reliable_connection(&self) -> bool {
        true
    }

    fn build(self: Box<Self>) -> Box<dyn DataSenderInterface> {
        self
    }
}

impl DataSenderInterface for UsbDataConnectionSender {
    fn is_reliable_connection(&self) -> bool {
        true
    }

    fn send_packet(&mut self, packet: &[u8]) -> Result<(), std::io::Error> {
        assert!(packet.len() <= USB_PACKET_MAX_DATA_SIZE as usize);

        let mut usb_packet = UsbPacketWrapper::new();
        usb_packet.set_size(packet.len() as u16);

        for (target, src) in usb_packet.data_mut().iter_mut().zip(packet.iter()) {
            *target = *src;
        }

        self.sender.send(usb_packet).map_err(|_| ErrorKind::BrokenPipe.into())
    }
}


#[derive(Debug)]
pub struct UsbDataConnectionReceiver {
    receiver: Receiver<UsbPacketWrapper>,
    timeout: Option<Duration>,
    nonbloking: bool,
}

impl UsbDataConnectionReceiver {
    pub fn try_recv(&mut self) -> Result<UsbPacketWrapper, TryRecvError> {
        self.receiver.try_recv()
    }

    pub fn recv(&mut self) -> Result<UsbPacketWrapper, RecvError> {
        self.receiver.recv()
    }

    pub fn raw_recv_mut(&mut self) -> &mut Receiver<UsbPacketWrapper> {
        &mut self.receiver
    }
}

impl DataReceiverBuilderInterface for UsbDataConnectionReceiver {
    fn is_reliable_connection(&self) -> bool {
        true
    }

    fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<(), std::io::Error> {
        self.timeout = timeout;
        Ok(())
    }

    fn build(self: Box<Self>) -> Box<dyn DataReceiverInterface> {
        self
    }
}

impl DataReceiverInterface for UsbDataConnectionReceiver {
    fn is_reliable_connection(&self) -> bool {
        true
    }

    fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<(), std::io::Error> {
        self.timeout = timeout;
        Ok(())
    }

    fn set_nonblocking(&mut self, nonblocking: bool) -> Result<(), std::io::Error> {
        self.nonbloking = nonblocking;
        Ok(())
    }

    fn recv_packet(&mut self, buffer: &mut [u8]) -> Result<usize, std::io::Error> {
        let packet = if self.nonbloking {
            self.receiver.try_recv().map_err(|e| {
                match e {
                    TryRecvError::Disconnected => ErrorKind::BrokenPipe,
                    TryRecvError::Empty => ErrorKind::WouldBlock,
                }
            })?
        } else if let Some(timeout) = self.timeout {
            self.receiver.recv_timeout(timeout).map_err(|e| {
                match e {
                    RecvTimeoutError::Disconnected => ErrorKind::BrokenPipe,
                    RecvTimeoutError::Timeout => ErrorKind::WouldBlock,
                }
            })?
        } else {
            self.receiver.recv().map_err(|_| ErrorKind::BrokenPipe)?
        };

        for (target, src) in buffer.iter_mut().zip(packet.data().iter()) {
            *target = *src;
        }

        Ok(packet.data().len())
    }
}
