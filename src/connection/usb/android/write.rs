/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::{sync::{atomic::{AtomicBool, Ordering}, mpsc::{Sender, Receiver, TryRecvError}}, thread::JoinHandle, time::Duration, net::TcpStream, io::{Write, ErrorKind, Read}};

use crate::connection::{data::{usb::{UsbDataConnectionSender, UsbDataConnectionReceiver}, DataSenderInterface}, usb::protocol::{UsbPacketOutType, USB_PACKET_MAX_DATA_SIZE, UsbPacketInType, USB_IN_PACKET_MAX_DATA_SIZE}};

use super::{accessory::{AccessoryReader, AccessoryWriter}, AndroidUsbEvent, UsbError};

use log::{error, warn};

pub static ACCESSORY_WRITER_THREAD_RUNNING: AtomicBool = AtomicBool::new(false);
pub static ACCESSORY_WRITER_THREAD_QUIT: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub enum WriterError {
    Usb(UsbError),
    JsonTcpDisconnected,
    JsonTcp(std::io::Error),
}

pub enum WriterEvent {
    SendAudio(UsbDataConnectionReceiver),
}

pub struct WriterThread {
    handle: JoinHandle<()>,
    writer_sender: Sender<WriterEvent>,
}

impl WriterThread {
    pub fn new(sender: Sender<AndroidUsbEvent>, accessory_writer: AccessoryWriter, json_stream: TcpStream) -> Self {
        if ACCESSORY_WRITER_THREAD_RUNNING.swap(true, Ordering::SeqCst) {
            panic!("Only one accessory writer thread can be running at the same time.");
        }

        let (writer_sender, receiver) = std::sync::mpsc::channel();

        let handle = std::thread::spawn(move || {
            ACCESSORY_WRITER_THREAD_QUIT.store(false, Ordering::Relaxed);

            WriterLogic::new(accessory_writer, sender, json_stream, receiver).run();
        });

        Self {
            handle,
            writer_sender,
        }
    }

    pub fn send_writer_event(&mut self, event: WriterEvent) {
        self.writer_sender.send(event).unwrap();
    }

    pub fn quit(self) {
        ACCESSORY_WRITER_THREAD_QUIT.store(true, Ordering::Relaxed);
        log::info!("ACCESSORY_WRITER_THREAD_QUIT start");
        self.handle.join().unwrap();
        log::info!("ACCESSORY_WRITER_THREAD_QUIT ready");
        ACCESSORY_WRITER_THREAD_RUNNING.store(false, Ordering::SeqCst);
    }
}


struct WriterLogic {
    accessory_writer: AccessoryWriter,
    sender: Sender<AndroidUsbEvent>,
    json_stream: TcpStream,
    writer_receiver: Receiver<WriterEvent>,
}

impl WriterLogic {
    fn new(
        accessory_writer: AccessoryWriter,
        sender: Sender<AndroidUsbEvent>,
        json_stream: TcpStream,
        writer_receiver: Receiver<WriterEvent>,
    ) -> Self {
        Self {
            accessory_writer,
            sender,
            json_stream,
            writer_receiver,
        }
    }

    fn run(mut self) {
        let mut buffer = [0; USB_IN_PACKET_MAX_DATA_SIZE as usize];

        loop {
            match self.writer_receiver.try_recv() {
                Ok(WriterEvent::SendAudio(receiver)) => {
                    unimplemented!()
                }
                Err(TryRecvError::Disconnected) => panic!("Receiver<WriterEvent> disconnected"),
                Err(TryRecvError::Empty) => (),
            }

            match self.json_stream.read(&mut buffer) {
                Ok(0) => {
                    self.sender.send(AndroidUsbEvent::WriterError(WriterError::JsonTcpDisconnected)).unwrap();
                    return self.wait_quit();
                }
                Ok(size) => {
                    let new_data = &buffer[..size];
                    if let Err(e) = self.accessory_writer.write_packet(UsbPacketInType::JsonStream, new_data) {
                        self.sender.send(AndroidUsbEvent::WriterError(WriterError::Usb(e))).unwrap();
                        return self.wait_quit();
                    }
                }
                Err(e) => {
                    if e.kind() == ErrorKind::WouldBlock {
                        if let Err(e) = self.accessory_writer.write_packet(UsbPacketInType::Empty, &[]) {
                            self.sender.send(AndroidUsbEvent::WriterError(WriterError::Usb(e))).unwrap();
                            return self.wait_quit();
                        }
                    } else {
                        self.sender.send(AndroidUsbEvent::WriterError(WriterError::JsonTcp(e))).unwrap();
                        return self.wait_quit();
                    }
                }
            }

            if ACCESSORY_WRITER_THREAD_QUIT.load(Ordering::Relaxed) {
                return;
            }
        }
    }

    fn wait_quit(self) {
        loop {
            std::thread::sleep(Duration::from_millis(1));
            if ACCESSORY_WRITER_THREAD_QUIT.load(Ordering::Relaxed) {
                return;
            }
        }
    }
}
