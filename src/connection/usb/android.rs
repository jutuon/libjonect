/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use core::panic;
use std::{os::unix::prelude::{RawFd}, thread::JoinHandle, sync::{Arc, mpsc::{Sender, Receiver}}, net::{TcpListener, ToSocketAddrs, TcpStream}, io::{Write, Read, ErrorKind}, time::Duration};

use log::{error, info};

use crate::{message_router::RouterSender, config::{LogicConfig, self}, connection::{ConnectionManagerEvent, data::{usb::UsbDataConnectionSender, DataSenderInterface}}};

use super::{UsbEvent, UsbPacketOutType, UsbPacketInType, USB_PACKET_MAX_DATA_SIZE};


pub enum AndroidUsbEvent {
    RequestQuit,
    UsbEvent(UsbEvent),
}

pub struct AndroidUsbThread {
    handle: JoinHandle<()>,
    sender: Sender<AndroidUsbEvent>,
}

impl AndroidUsbThread {
    pub fn start(r_sender: RouterSender, config: Arc<LogicConfig>, fd: RawFd) -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();

        let s = sender.clone();
        let handle = std::thread::spawn(move || {
            let usb_handle = AndroidUsbAccessoryHandle::new(fd);
            AndroidUsbManager::new(usb_handle, r_sender, receiver).run();
        });

        Self {
            handle,
            sender,
        }
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
    Read(nix::Error),
    Write(nix::Error),
    UnexpectedReadSize(usize),
    UnexpectedPacketOutType(u8),
    UnexpectedWriteSize(usize),
}

pub struct AndroidUsbAccessoryHandle {
    fd: RawFd,
    buffer: [u8; 512],
}

impl AndroidUsbAccessoryHandle {
    pub fn new(fd: RawFd) -> Self {
        Self {
            fd,
            buffer: [0; 512],
        }
    }

    pub fn read_packet(&mut self) -> Result<(UsbPacketOutType, &[u8]), UsbError> {
        //info!("Read start");
        let size = nix::unistd::read(self.fd, &mut self.buffer).map_err(UsbError::Read)?;
        if size != self.buffer.len() {
            return Err(UsbError::UnexpectedReadSize(size));
        }

        let (header, data) = self.buffer.split_at(3);

        let packet_type_byte = header[0];

        let packet_type = if UsbPacketOutType::Empty as u8 == packet_type_byte {
            UsbPacketOutType::Empty
        } else if UsbPacketOutType::JsonStream as u8 == packet_type_byte {
            UsbPacketOutType::JsonStream
        } else if UsbPacketOutType::AudioData as u8 == packet_type_byte {
            UsbPacketOutType::AudioData
        } else if UsbPacketOutType::NextIsWrite as u8 == packet_type_byte {
            UsbPacketOutType::NextIsWrite
        } else {
            return Err(UsbError::UnexpectedPacketOutType(packet_type_byte));
        };

        let packet_size: usize = u16::from_be_bytes([header[1], header[2]]) as usize;

        //info!("Read size {}, type: {:?}, type_byte: {}", size, packet_type, packet_type_byte);

        Ok((packet_type, &data[..packet_size]))
    }

    pub fn write_packet(&mut self, packet_type: UsbPacketInType, data: &[u8]) -> Result<(), UsbError> {
        //info!("Write packet_type {:?}", packet_type);
        let packet_type_byte = [packet_type as u8];
        let data_size: u16 = data.len().try_into().unwrap();
        let data_size_bytes = data_size.to_be_bytes();
        let packet_data = packet_type_byte.iter().chain(data_size_bytes.iter()).chain(data).chain(std::iter::repeat(&0));

        for (target, src) in self.buffer.iter_mut().zip(packet_data) {
            *target = *src;
        }

        let size = nix::unistd::write(self.fd, &self.buffer).map_err(UsbError::Write)?;
        if size != self.buffer.len() {
            return Err(UsbError::UnexpectedWriteSize(size));
        }

        //info!("Write size {}", size);

        Ok(())
    }
}


impl Drop for AndroidUsbAccessoryHandle {
    fn drop(&mut self) {
        if let Err(e) = nix::unistd::close(self.fd) {
            error!("AndroidUsbAccessoryHandle drop error: {e}");
        }
    }
}

pub enum UsbManagerError {
    Tcp(std::io::Error),
    Usb(UsbError),
}


pub struct AndroidUsbManager {
    handle: AndroidUsbAccessoryHandle,
    r_sender: RouterSender,
    receiver: Receiver<AndroidUsbEvent>,
}

impl AndroidUsbManager {
    pub fn new(handle: AndroidUsbAccessoryHandle, r_sender: RouterSender, receiver: Receiver<AndroidUsbEvent>) -> Self {
        Self {
            handle,
            r_sender,
            receiver,
        }
    }

    pub fn run(mut self) {
        let address = config::USB_JSON_SOCKET_ADDRESS.to_socket_addrs().unwrap().next().unwrap();
        let mut json_connection = TcpStream::connect(address).unwrap();
        json_connection.set_nonblocking(true).unwrap();

        let mut write = false;

        let mut audio_sender: Option<UsbDataConnectionSender> = None;

        loop {
            loop {
                match self.receiver.try_recv() {
                    Ok(AndroidUsbEvent::RequestQuit) => return,
                    Ok(AndroidUsbEvent::UsbEvent(event)) => {
                        match event {
                            UsbEvent::AndroidUsbAccessoryFileDescriptor(_) => panic!("AndroidUsbAccessoryFileDescriptor should be already handled."),
                            UsbEvent::AndroidQuitAndroidUsbManager => panic!("AndroidQuitAndroidUsbManager should be already handled."),
                            UsbEvent::ReceiveAudioOverUsb(sender) => {
                                audio_sender = Some(sender);
                            }
                            UsbEvent::SendAudioOverUsb(_) => unimplemented!(),
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => panic!("TryRecvError::Disconnected"),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                }
            }

            if write {
                let mut buffer = [0; USB_PACKET_MAX_DATA_SIZE as usize];
                    match json_connection.read(&mut buffer) {
                        Ok(0) => {
                            error!("USB JSON TCP connection disconnected");
                            return self.wait_quit();
                        }
                        Ok(size) => {
                            let new_data = &buffer[..size];
                            if let Err(e) = self.handle.write_packet(UsbPacketInType::JsonStream, new_data) {
                                error!("Error: {:?}", e);
                                return self.wait_quit();
                            }
                        }
                        Err(e) => {
                            if e.kind() == ErrorKind::WouldBlock {
                                if let Err(e) = self.handle.write_packet(UsbPacketInType::Empty, &[]) {
                                    error!("Error: {:?}", e);
                                    return self.wait_quit();
                                }
                            } else {
                                error!("Error: {:?}", e);
                                return self.wait_quit();
                            }
                        }
                    }


                write = false;

            } else {
                let (packet_type, mut data) = match self.handle.read_packet() {
                    Ok(value) => value,
                    Err(e) => {
                        error!("Read packet error: {:?}", e);
                        return self.wait_quit();
                    }
                };

                match packet_type {
                    UsbPacketOutType::Empty => (),
                    UsbPacketOutType::JsonStream => {
                        loop {
                            match json_connection.write(data) {
                                Ok(0) => {
                                    error!("USB JSON TCP connection disconnected");
                                    return self.wait_quit();
                                }
                                Ok(count) => {
                                    let (_, non_written) = data.split_at(count);

                                    if non_written.is_empty() {
                                        break;
                                    } else {
                                        data = non_written;
                                    }
                                }
                                Err(e) => {
                                    if e.kind() == ErrorKind::WouldBlock {
                                        std::thread::sleep(Duration::from_millis(1));
                                    } else {
                                        error!("Error: {:?}", e);
                                        return self.wait_quit();
                                    }
                                }
                            }
                        }
                    },
                    UsbPacketOutType::AudioData => {
                        if let Some(sender) = audio_sender.as_mut() {
                            match sender.send_packet(data) {
                                Ok(()) => (),
                                Err(e) => {
                                    error!("Audio sending failed: {}", e);
                                    audio_sender.take();
                                }
                            }
                        }
                    },
                    UsbPacketOutType::NextIsWrite => write = true,
                }
            }
        }
    }

    fn wait_quit(&mut self) {
        self.r_sender.send_connection_manager_event_blocking(ConnectionManagerEvent::AndroidQuitAndroidUsbManager);

        loop {
            match self.receiver.recv().unwrap() {
                AndroidUsbEvent::RequestQuit => return,
                AndroidUsbEvent::UsbEvent(_) => (),
            }
        }
    }
}
