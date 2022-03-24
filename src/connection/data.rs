/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Data connection

pub mod tcp;
pub mod udp;
pub mod usb;

use std::{net::TcpStream, time::Duration, io::{Write, Read}, fmt::Debug};

pub trait DataSenderBuilderInterface: Debug + Send {
    fn is_reliable_connection(&self) -> bool;

    fn build(self: Box<Self>) -> Box<dyn DataSenderInterface>;
}

pub trait DataReceiverBuilderInterface: Debug + Send {
    fn is_reliable_connection(&self) -> bool;

    fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<(), std::io::Error>;

    fn build(self: Box<Self>) -> Box<dyn DataReceiverInterface>;
}

pub trait DataSenderInterface: Debug + Send {
    fn is_reliable_connection(&self) -> bool;

    /// Max packet lenght is 65 507 bytes (max packet length with IPv4 and UDP).
    ///
    /// If underlying data transfer method is USB then the max packet lenght is USB_PACKET_MAX_DATA_SIZE.
    ///
    /// Data sending might fail if underlying connection can detect disconnects.
    ///
    /// This might return io::ErrorKind::WouldBlock if underlying buffer is full.
    fn send_packet(&mut self, packet: &[u8]) -> Result<(), std::io::Error>;
}

pub trait DataReceiverInterface: Debug + Send {
    fn is_reliable_connection(&self) -> bool;

    fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<(), std::io::Error>;

    /// Max packet lenght is 65 507 bytes (max packet length with IPv4 and UDP).
    ///
    /// If underlying data transfer method is USB then the max packet lenght is USB_PACKET_MAX_DATA_SIZE.
    ///
    /// Data receiving might fail if underlying connection can detect disconnects.
    ///
    /// Returns ErrorKind::WouldBlock if timeout is set.
    /// Returns Ok(0) if connection is broken and it can be detected.
    fn recv_packet(&mut self, buffer: &mut [u8]) -> Result<usize, std::io::Error>;
}

pub const MAX_PACKET_SIZE: usize = 65_507;

pub type DataSenderBuilder = Box<dyn DataSenderBuilderInterface>;
pub type DataReceiverBuilder = Box<dyn DataReceiverBuilderInterface>;
pub type DataSender = Box<dyn DataSenderInterface>;
pub type DataReceiver = Box<dyn DataReceiverInterface>;


#[derive(Debug)]
pub struct EmptyReceiver;

impl DataReceiverBuilderInterface for EmptyReceiver {
    fn is_reliable_connection(&self) -> bool {
        false
    }

    fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<(), std::io::Error> {
        Ok(())
    }

    fn build(self: Box<Self>) -> Box<dyn DataReceiverInterface> {
        self
    }
}

impl DataReceiverInterface for EmptyReceiver {
    fn is_reliable_connection(&self) -> bool {
        false
    }

    fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<(), std::io::Error> {
        Ok(())
    }

    fn recv_packet(&mut self, buffer: &mut [u8]) -> Result<usize, std::io::Error> {
        Ok(0)
    }
}
