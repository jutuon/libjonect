/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::{sync::{atomic::{AtomicBool, Ordering}, mpsc::{Sender, Receiver, TryRecvError}}, thread::JoinHandle, time::Duration, net::TcpStream, io::{Write, ErrorKind}};

use crate::{connection::{data::{usb::UsbDataConnectionSender, DataSenderInterface}, usb::protocol::UsbPacketOutType}, utils::android::config_thread_priority};

use super::{accessory::AccessoryReader, AndroidUsbEvent, UsbError};

use log::{error, warn};

pub static ACCESSORY_READER_THREAD_RUNNING: AtomicBool = AtomicBool::new(false);
pub static ACCESSORY_READER_THREAD_QUIT: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub enum ReaderError {
    Usb(UsbError),
    JsonTcpDisconnected,
    JsonTcp(std::io::Error),
}

pub enum ReaderEvent {
    ReadAudio(UsbDataConnectionSender),
}

pub struct ReaderThread {
    handle: JoinHandle<()>,
    reader_sender: Sender<ReaderEvent>,
}

impl ReaderThread {
    pub fn new(sender: Sender<AndroidUsbEvent>, accessory_reader: AccessoryReader, json_stream: TcpStream) -> Self {
        if ACCESSORY_READER_THREAD_RUNNING.swap(true, Ordering::SeqCst) {
            panic!("Only one accessory reader thread can be running at the same time.");
        }

        let (reader_sender, receiver) = std::sync::mpsc::channel();

        let handle = std::thread::spawn(move || {
            ACCESSORY_READER_THREAD_QUIT.store(false, Ordering::Relaxed);

            ReaderLogic::new(accessory_reader, sender, json_stream, receiver).run();
        });

        Self {
            handle,
            reader_sender,
        }
    }

    pub fn send_reader_event(&mut self, event: ReaderEvent) {
        self.reader_sender.send(event).unwrap();
    }

    pub fn quit(self) {
        ACCESSORY_READER_THREAD_QUIT.store(true, Ordering::Relaxed);
        log::info!("ACCESSORY_READER_THREAD_QUIT start");
        self.handle.join().unwrap();
        log::info!("ACCESSORY_READER_THREAD_QUIT ready");
        ACCESSORY_READER_THREAD_RUNNING.store(false, Ordering::SeqCst);
    }
}


struct ReaderLogic {
    accessory_reader: AccessoryReader,
    sender: Sender<AndroidUsbEvent>,
    json_stream: TcpStream,
    reader_receiver: Receiver<ReaderEvent>,
}

impl ReaderLogic {
    fn new(
        accessory_reader: AccessoryReader,
        sender: Sender<AndroidUsbEvent>,
        json_stream: TcpStream,
        reader_receiver: Receiver<ReaderEvent>,
    ) -> Self {
        Self {
            accessory_reader,
            sender,
            json_stream,
            reader_receiver,
        }
    }

    fn run(mut self) {
        config_thread_priority("USB accessory reading logic");

        let mut audio_sender: Option<UsbDataConnectionSender> = None;

        loop {
            match self.reader_receiver.try_recv() {
                Ok(ReaderEvent::ReadAudio(sender)) => {
                    audio_sender = Some(sender);
                }
                Err(TryRecvError::Disconnected) => panic!("Receiver<ReaderEvent> disconnected"),
                Err(TryRecvError::Empty) => (),
            }

            let (packet_type, mut data) = match self.accessory_reader.read_packet() {
                Ok(value) => value,
                Err(e) => {
                    self.sender.send(AndroidUsbEvent::ReaderError(ReaderError::Usb(e))).unwrap();
                    return self.wait_quit();
                }
            };

            match packet_type {
                UsbPacketOutType::Empty => (),
                UsbPacketOutType::JsonStream => {
                    loop {
                        match self.json_stream.write(data) {
                            Ok(0) => {
                                self.sender.send(AndroidUsbEvent::ReaderError(ReaderError::JsonTcpDisconnected)).unwrap();
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
                                    self.sender.send(AndroidUsbEvent::ReaderError(ReaderError::JsonTcp(e))).unwrap();
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
                    } else {
                        // TODO: Send JSON message to the connected device when
                        // data can be sent.
                        // warn!("Audio data dropped.");
                    }
                },
                UsbPacketOutType::NextIsWrite => (),
            }

            if ACCESSORY_READER_THREAD_QUIT.load(Ordering::Relaxed) {
                return;
            }
        }
    }

    fn wait_quit(self) {
        loop {
            std::thread::sleep(Duration::from_millis(1));
            if ACCESSORY_READER_THREAD_QUIT.load(Ordering::Relaxed) {
                return;
            }
        }
    }
}
