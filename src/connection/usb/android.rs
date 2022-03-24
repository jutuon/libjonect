/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

mod accessory;
mod read;
mod write;

use core::panic;
use std::{os::unix::prelude::{RawFd}, thread::JoinHandle, sync::{Arc, mpsc::{Sender, Receiver}}, net::{TcpListener, ToSocketAddrs, TcpStream}, io::{Write, Read, ErrorKind}, time::Duration};

use log::{error, info};

use crate::{message_router::RouterSender, config::{LogicConfig, self}, connection::{ConnectionManagerEvent, data::{usb::UsbDataConnectionSender, DataSenderInterface}}};

use self::{read::{ReaderError, ReaderThread}, write::{WriterError, WriterThread}, accessory::{Accessory, AccessoryReader, AccessoryWriter}};

use super::{UsbEvent, protocol::{UsbPacket, UsbPacketOutType, UsbPacketInType, USB_PACKET_MAX_DATA_SIZE}, UsbDataChannelCreatorI};


pub struct UsbDataChannelReceiver;

pub struct UsbDataChannelCreator;

impl UsbDataChannelCreator {
    pub fn new() -> (UsbDataChannelCreator, UsbDataChannelReceiver) {
        (UsbDataChannelCreator, UsbDataChannelReceiver)
    }
}

impl UsbDataChannelCreatorI for UsbDataChannelCreator {}



pub type UsbPacketWrapper = UsbPacket;

pub enum AndroidUsbEvent {
    RequestQuit,
    ReaderError(ReaderError),
    WriterError(WriterError),
    UsbEvent(UsbEvent),
}

pub struct AndroidUsbThread {
    handle: JoinHandle<()>,
    sender: Sender<AndroidUsbEvent>,
}

impl AndroidUsbThread {
    pub fn start(r_sender: RouterSender, config: Arc<LogicConfig>, fd: RawFd) -> Result<Self, ()> {
        let (sender, receiver) = std::sync::mpsc::channel();

        let (accessory_reader, accessory_writer) = match Accessory::new(fd) {
            Ok(accessory) => accessory,
            Err(e) => {
                error!("Error: {:?}", e);
                return Err(());
            }
        };

        let s = sender.clone();
        let handle = std::thread::spawn(move || {
            AndroidUsbManager::new(r_sender, receiver, s).run(accessory_reader, accessory_writer);
        });

        Ok(Self {
            handle,
            sender,
        })
    }

    pub fn send_event(&mut self, event: UsbEvent) {
        self.sender.send(AndroidUsbEvent::UsbEvent(event)).unwrap();
    }

    pub fn quit(self) {
        self.sender.send(AndroidUsbEvent::RequestQuit).unwrap();
        self.handle.join().unwrap();
    }
}


#[derive(Debug)]
pub enum UsbError {
    Dup(nix::Error),
    Read(nix::Error),
    Write(nix::Error),
    UnexpectedReadSize(usize),
    UnexpectedPacketOutType(u8),
    UnexpectedWriteSize(usize),
}

pub enum UsbManagerError {
    Tcp(std::io::Error),
    Usb(UsbError),
}


pub struct AndroidUsbManager {
    r_sender: RouterSender,
    receiver: Receiver<AndroidUsbEvent>,
    event_sender: Sender<AndroidUsbEvent>,
}

impl AndroidUsbManager {
    pub fn new(
        r_sender: RouterSender,
        receiver: Receiver<AndroidUsbEvent>,
        event_sender: Sender<AndroidUsbEvent>,
    ) -> Self {
        Self {
            r_sender,
            receiver,
            event_sender,
        }
    }

    pub fn run(
        self,
        accessory_reader: AccessoryReader,
        accessory_writer: AccessoryWriter,
    ) {
        let address = config::USB_JSON_SOCKET_ADDRESS.to_socket_addrs().unwrap().next().unwrap();
        let json_read = TcpStream::connect(address).unwrap();
        json_read.set_nonblocking(true).unwrap();

        let json_write = json_read.try_clone().unwrap();
        json_write.set_nonblocking(true).unwrap();

        let mut reader = ReaderThread::new(self.event_sender.clone(), accessory_reader, json_write);
        let writer = WriterThread::new(self.event_sender.clone(), accessory_writer, json_read);

        loop {
            match self.receiver.recv() {
                Ok(AndroidUsbEvent::RequestQuit) => break,
                Ok(AndroidUsbEvent::ReaderError(e)) => {
                    return self.wait_quit(reader, writer);
                }
                Ok(AndroidUsbEvent::WriterError(e)) => {
                    return self.wait_quit(reader, writer);
                }
                Ok(AndroidUsbEvent::UsbEvent(event)) => {
                    match event {
                        UsbEvent::AndroidUsbAccessoryFileDescriptor(_) => panic!("AndroidUsbAccessoryFileDescriptor should be already handled."),
                        UsbEvent::AndroidQuitAndroidUsbManager => panic!("AndroidQuitAndroidUsbManager should be already handled."),
                        UsbEvent::ReceiveAudioOverUsb(sender) => {
                            reader.send_reader_event(read::ReaderEvent::ReadAudio(sender));
                        }
                    }
                }
                Err(RevcError) => panic!("RevcError"),
            }
        }
    }

    fn wait_quit(mut self, reader: ReaderThread, writer: WriterThread) {
        self.r_sender.send_connection_manager_event_blocking(ConnectionManagerEvent::AndroidQuitAndroidUsbManager);

        reader.quit();
        writer.quit();

        loop {
            match self.receiver.recv().unwrap() {
                AndroidUsbEvent::RequestQuit => return,
                AndroidUsbEvent::UsbEvent(_) => (),
                AndroidUsbEvent::ReaderError(e) => {
                    error!("Error: {:?}", e);
                }
                AndroidUsbEvent::WriterError(e) => {
                    error!("Error: {:?}", e);
                }
            }
        }
    }
}
