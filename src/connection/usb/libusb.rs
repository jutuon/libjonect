/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Libusb code

mod android;
mod accessory;

use std::{thread::JoinHandle, sync::{mpsc::{Sender, Receiver, TryRecvError, RecvError, RecvTimeoutError}, Arc}, time::{Duration, Instant}, ffi::CString, net::TcpStream, io::{Read, ErrorKind, Write}};

use log::{info, error};
use rusb::{Context, UsbContext, Device, Direction, RequestType, DeviceHandle};

use crate::{config::{LogicConfig, self}, message_router::RouterSender, connection::data::{DataReceiver, usb::{UsbDataConnectionReceiver, UsbDataConnectionSender}, EmptyReceiver}};

use self::{accessory::AndroidUsbAccessory, android::AndroidUsbDevice};

use super::{UsbEvent, protocol::{UsbPacketOutType, USB_PACKET_MAX_DATA_SIZE, UsbPacketInType, UsbPacket}, UsbDataChannelCreatorI};

const KNOWN_ANDROID_VENDOR_IDS: &[u16] = &[
    0x0e8d, // Nokia 2.2
    0x2a70, // OnePlus 5
];

const USB_TIMEOUT: Duration = Duration::from_millis(500);

pub struct UsbDataChannelReceiver {
    sender: Sender<UsbPacketWrapper>,
    receiver: Receiver<UsbPacketWrapper>,
}

pub struct UsbDataChannelCreator {
    sender: Sender<UsbPacketWrapper>,
}

impl UsbDataChannelCreator {
    pub fn new() -> (UsbDataChannelCreator, UsbDataChannelReceiver) {
        let (sender, receiver) = std::sync::mpsc::channel();

        let receiver = UsbDataChannelReceiver {
            sender: sender.clone(),
            receiver,
        };

        let sender = UsbDataChannelCreator {
            sender,
        };

        (sender, receiver)
    }
}

impl UsbDataChannelCreatorI for UsbDataChannelCreator {
    fn build_sender(&self) -> (crate::connection::data::DataSenderBuilder, crate::connection::data::DataReceiverBuilder) {
        let usb_sender = Box::new(UsbDataConnectionSender::new(self.sender.clone()));

        let usb_receiver = Box::new(EmptyReceiver);

        (usb_sender, usb_receiver)
    }
}


#[derive(Debug)]
pub struct UsbPacketWrapper {
    packet: LibUsbEvent,
}

impl UsbPacketWrapper {
    fn event(self) -> LibUsbEvent {
        self.packet
    }

    pub fn new() -> UsbPacketWrapper {
        Self {
            packet: LibUsbEvent::SendAudioData(UsbPacket::new()),
        }
    }

    pub fn set_size(&mut self, size: u16) {
        if let LibUsbEvent::SendAudioData(packet) = &mut self.packet {
            packet.set_size(size)
        }
    }

    pub fn data(&self) -> &[u8] {
        if let LibUsbEvent::SendAudioData(packet) = &self.packet {
            packet.data()
        } else {
            &[]
        }
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        if let LibUsbEvent::SendAudioData(packet) = &mut self.packet {
            packet.data_mut()
        } else {
            &mut []
        }
    }
}

impl From<LibUsbEvent> for UsbPacketWrapper {
    fn from(e: LibUsbEvent) -> Self {
        Self {
            packet: e
        }
    }
}


#[derive(Debug)]
pub enum UsbError {
    ReadControlFailed(rusb::Error),
    WriteControlFailed(rusb::Error),
    AndroidStartAccessoryModeUnexpectedWriteResult(usize),
    DeviceIterationFailed(rusb::Error),
    DeviceDescriptorRequestFailed(rusb::Error),
    DeviceOpenFailed(rusb::Error),
    GetConfigurationFailed(rusb::Error),
    SetConfigurationFailed(rusb::Error),
    GetConfigDescriptorFailed(rusb::Error),
    UnsupportedEndpointMaxPacketSize(u16),
    ClaimInterfaceFailed(rusb::Error),
    ReadBulkFailed(rusb::Error),
    ReadBulkByteCount{ expected: usize, was: usize},
    WriteBulkFailed(rusb::Error),
    WriteBulkByteCount{ expected: usize, was: usize},
    UnknownInPacket(u8),
}

#[derive(Debug)]
enum LibUsbEvent {
    RequestQuit,
    PollUsbDevices,
    JsonPollIfConnected,
    UsbEvent(UsbEvent),
    SendAudioData(UsbPacket),
}

pub struct LibUsbThread {
    handle: JoinHandle<()>,
    sender: Sender<UsbPacketWrapper>,
}

impl LibUsbThread {
    pub async fn start(r_sender: RouterSender, config: Arc<LogicConfig>, receiver: UsbDataChannelReceiver) -> Self {
        let UsbDataChannelReceiver {
            sender,
            receiver,
        } = receiver;

        let s = sender.clone();
        let handle = std::thread::spawn(move || {
            LibUsbLogic::new(r_sender, s, receiver, config).run();
        });

        Self {
            handle,
            sender,
        }
    }

    pub fn send_event(&mut self, event: UsbEvent) {
        self.sender.send(LibUsbEvent::UsbEvent(event).into()).unwrap();
    }

    pub fn poll_usb_devices(&mut self) {
        self.sender.send(LibUsbEvent::PollUsbDevices.into()).unwrap();
    }

    pub fn json_poll_if_connected(&mut self) {
        self.sender.send(LibUsbEvent::JsonPollIfConnected.into()).unwrap();
    }

    pub fn quit(self) {
        self.sender.send(LibUsbEvent::RequestQuit.into()).unwrap();
        self.handle.join().unwrap();
    }
}

struct Quit;

enum Mode {
    PollUsbDevices,
    AccessoryConnected(AccessoryConnection),
}

impl Mode {
    fn update(self, logic: &mut LibUsbLogic, context: &Context) -> (Option<Quit>, Mode) {
        let mode = match self {
            Mode::PollUsbDevices  => {
                match logic.receiver.recv().unwrap().event() {
                    LibUsbEvent::RequestQuit => return (Some(Quit), self),
                    LibUsbEvent::PollUsbDevices => {
                        if Instant::now().duration_since(logic.poll_timer) >= Duration::from_secs(2) || logic.first_connect {
                            logic.first_connect = false;

                            if let Some(accessory) = LibUsbLogic::try_get_accessory_connection(context) {
                                Mode::AccessoryConnected(accessory)
                            } else {
                                self
                            }
                        } else {
                            self
                        }
                    }
                    LibUsbEvent::UsbEvent(_) |
                    LibUsbEvent::JsonPollIfConnected |
                    LibUsbEvent::SendAudioData(_) => self,
                }
            }
            Mode::AccessoryConnected(mut accessory) => {
                match logic.receiver.recv().unwrap().event() {
                    LibUsbEvent::RequestQuit => return (Some(Quit), Mode::AccessoryConnected(accessory)),
                    LibUsbEvent::PollUsbDevices => (),
                    LibUsbEvent::JsonPollIfConnected => {
                        match accessory.receive_next() {
                            Ok(()) => (),
                            Err(e) => {
                                error!("Error: {e:?}");
                                return (None, Mode::PollUsbDevices);
                            },
                        }

                        match accessory.update_connection() {
                            Ok(()) => (),
                            Err(e) => {
                                error!("Error: {e:?}");
                                return (None, Mode::PollUsbDevices);
                            },
                        }
                    }
                    LibUsbEvent::UsbEvent(event) => {
                        match event {
                            UsbEvent::ReceiveAudioOverUsb(sender) => {
                                unimplemented!()
                            }
                            UsbEvent::AndroidQuitAndroidUsbManager |
                            UsbEvent::AndroidUsbAccessoryFileDescriptor(_) => (),
                        }
                    }
                    LibUsbEvent::SendAudioData(data) => {
                        match accessory.send_audio_packet(data) {
                            Ok(()) => (),
                            Err(e) => {
                                error!("Error: {e:?}");
                                return (None, Mode::PollUsbDevices);
                            },
                        }
                    }
                }

                Mode::AccessoryConnected(accessory)
            }
        };

        (None, mode)
    }
}


struct LibUsbLogic {
    sender: Sender<UsbPacketWrapper>,
    receiver: Receiver<UsbPacketWrapper>,
    config: Arc<LogicConfig>,
    r_sender: RouterSender,
    poll_timer: Instant,
    first_connect: bool,
}

impl LibUsbLogic {
    fn new(
        r_sender: RouterSender,
        sender: Sender<UsbPacketWrapper>,
        receiver: Receiver<UsbPacketWrapper>,
        config: Arc<LogicConfig>
    ) -> Self {
        Self {
            r_sender,
            sender,
            receiver,
            config,
            poll_timer: Instant::now(),
            first_connect: true,
        }
    }

    fn run(mut self) {
        let context = Context::new().unwrap();

        for device in context.devices().unwrap().iter() {
            Self::print_usb_info(&device);
        }

        let mut mode = Mode::PollUsbDevices;

        loop {
            let previous_mode_poll = matches!(&mode, Mode::AccessoryConnected(_));

            let (quit, new_mode) = mode.update(&mut self, &context);
            mode = new_mode;

            if matches!(&mode, Mode::PollUsbDevices) && previous_mode_poll {
                info!("Accessory disconnected.");
                self.poll_timer = Instant::now();
            }

            if quit.is_some() {
                break;
            }
        }
    }

    fn try_get_accessory_connection(context: &Context) -> Option<AccessoryConnection> {
        // TODO: If on going accessory connection process fails the USB device
        // should be reseted?

        match Self::find_accessory_mode_usb_device(&context) {
            Ok(None) => {
                info!("No USB accessory devices detected.");
            },
            Ok(Some(accessory)) => {
                match AccessoryConnection::new(accessory) {
                    Ok(connection) => {
                        return Some(connection);
                    }
                    Err(e) => {
                        error!("AccessoryConnection creation failed: {e:?}");
                    }
                }
            }
            Err(e) => {
                error!("Find USB accessory failed: {e:?}");
            },
        }

        match Self::find_android_usb_device(&context) {
            Ok(None) => {
                info!("No Android devices found.");
            },
            Ok(Some(())) => {
                info!("Android device found.");
            }
            Err(e) => {
                error!("Find Android device failed: {e:?}");
            },
        }

        None
    }

    fn find_android_usb_device(context: &Context) -> Result<Option<()>, UsbError> {
        for device in context.devices().map_err(UsbError::DeviceIterationFailed)?.iter() {
            let device_descriptor = device.device_descriptor().map_err(UsbError::DeviceDescriptorRequestFailed)?;

            if KNOWN_ANDROID_VENDOR_IDS.contains(&device_descriptor.vendor_id()) {

                let device_handle = device.open().map_err(UsbError::DeviceOpenFailed)?;

                let android_device = match AndroidUsbDevice::new(&device_handle)? {
                    Some(device) => device,
                    None => continue,
                };

                android_device.start_device_in_aoa_mode()?;

                return Ok(Some(()));
            }
        }

        Ok(None)
    }

    fn find_accessory_mode_usb_device(context: &Context) -> Result<Option<AndroidUsbAccessory>, UsbError> {
        for device in context.devices().map_err(UsbError::DeviceIterationFailed)?.iter() {
            if let Some(accessory) = AndroidUsbAccessory::new(device)? {
                return Ok(Some(accessory));
            }
        }

        Ok(None)
    }

    fn print_usb_info(device: &Device<Context>) {
        let device_info = device.device_descriptor().unwrap();
        info!("usb device: vendor_id: {:x}, product_id: {:x}", device_info.vendor_id(), device_info.product_id());
    }
}

#[derive(Debug)]
enum AccessoryConnectionError {
    Usb(UsbError),
    Tcp(std::io::Error),
    TcpDisconnected,
}

pub struct AccessoryConnection {
    accessory: AndroidUsbAccessory,
    json_stream: TcpStream,
    send_audio: Option<UsbDataConnectionSender>,
}

impl AccessoryConnection {
    fn new(accessory: AndroidUsbAccessory) -> Result<Self, AccessoryConnectionError> {
        // TODO: Use other connection method than TCP?
        let json_stream = TcpStream::connect(config::USB_JSON_SOCKET_ADDRESS).map_err(AccessoryConnectionError::Tcp)?;
        json_stream.set_nonblocking(true).map_err(AccessoryConnectionError::Tcp)?;

        Ok(Self {
            accessory,
            json_stream,
            send_audio: None,
        })
    }

    fn set_audio_sender(&mut self, sender: UsbDataConnectionSender) {
        self.send_audio = Some(sender);
    }

    fn send_audio_packet(&mut self, mut packet: UsbPacket) -> Result<(), AccessoryConnectionError> {
        packet.set_packet_type(UsbPacketOutType::AudioData as u8);
        self.accessory.send_usb_packet(packet).map_err(AccessoryConnectionError::Usb)?;
        Ok(())
    }

    fn update_connection(&mut self) -> Result<(), AccessoryConnectionError> {
        let mut buffer = [0; USB_PACKET_MAX_DATA_SIZE as usize];

        match self.json_stream.read(&mut buffer) {
            Ok(0) => {
                info!("TCP Disconnected");
                return Err(AccessoryConnectionError::TcpDisconnected);
            }
            Ok(size) => {
                let new_data = &buffer[..size];
                self.accessory.send_packet(UsbPacketOutType::JsonStream, new_data).map_err(AccessoryConnectionError::Usb)?;
            }
            Err(e) => {
                if e.kind() != ErrorKind::WouldBlock {
                    return Err(AccessoryConnectionError::Tcp(e));
                }
            }
        }

        Ok(())
    }

    fn receive_next(&mut self) -> Result<(), AccessoryConnectionError> {
        let (packet_type, mut data) = self.accessory.receive_packet().map_err(AccessoryConnectionError::Usb)?;

        match packet_type {
            UsbPacketInType::Empty => (),
            UsbPacketInType::JsonStream => {
                loop {
                    if data.is_empty() {
                        info!("data.is_empty()");
                        break;
                    }

                    match self.json_stream.write(data) {
                        Ok(0) => {
                            return Err(AccessoryConnectionError::TcpDisconnected);
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
                                return Err(AccessoryConnectionError::Tcp(e));
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
