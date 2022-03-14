use std::{thread::JoinHandle, sync::{mpsc::{Sender, Receiver, TryRecvError, RecvError}, Arc}, time::Duration, ffi::CString, net::TcpStream, io::{Read, ErrorKind, Write}};

use log::{info, error};
use rusb::{Context, UsbContext, Device, Direction, RequestType, DeviceHandle};

use crate::{config::{LogicConfig, self}, message_router::RouterSender, connection::data::{DataReceiver, usb::{UsbDataConnectionReceiver, UsbDataConnectionSender}}};

use super::{UsbEvent, UsbPacketOutType, USB_PACKET_MAX_DATA_SIZE, UsbPacketInType, UsbPacket};

const KNOWN_ANDROID_VENDOR_IDS: &[u16] = &[
    0x0e8d,
];

const ACCESSORY_MODE_PRODUCT_IDS: &[u16] = &[0x2d00, 0x2d01];

const ACCESSORY_MODE_AND_ADB_PRODUCT_ID: u16 = 0x2d01;

const ACCESSORY_MODE_VENDOR_ID: u16 = 0x18d1;

const USB_TIMEOUT: Duration = Duration::from_millis(20000);

enum LibUsbEvent {
    RequestQuit,
    PollUsbDevices,
    JsonPollIfConnected,
    UsbEvent(UsbEvent),
}

pub struct LibUsbThread {
    handle: JoinHandle<()>,
    sender: Sender<LibUsbEvent>,
}

impl LibUsbThread {
    pub async fn start(r_sender: RouterSender, config: Arc<LogicConfig>) -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();

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
        self.sender.send(LibUsbEvent::UsbEvent(event)).unwrap();
    }

    pub fn poll_usb_devices(&mut self) {
        self.sender.send(LibUsbEvent::PollUsbDevices).unwrap();
    }

    pub fn json_poll_if_connected(&mut self) {
        self.sender.send(LibUsbEvent::JsonPollIfConnected).unwrap();
    }

    pub fn quit(self) {
        self.sender.send(LibUsbEvent::RequestQuit).unwrap();
        self.handle.join().unwrap();
    }
}


struct LibUsbLogic {
    sender: Sender<LibUsbEvent>,
    receiver: Receiver<LibUsbEvent>,
    config: Arc<LogicConfig>,
    r_sender: RouterSender,
}

impl LibUsbLogic {
    fn new(
        r_sender: RouterSender,
        sender: Sender<LibUsbEvent>,
        receiver: Receiver<LibUsbEvent>,
        config: Arc<LogicConfig>
    ) -> Self {
        Self {
            r_sender,
            sender,
            receiver,
            config,
        }
    }

    fn run(mut self) {
        let context = Context::new().unwrap();

        for device in context.devices().unwrap().iter() {
            Self::print_usb_info(&device);
        }
        let mut flag = false;
        let mut current_accessory: Option<AccessoryConnection> = None;

        loop {
            let non_blocking_event_receiving = if let Some(accessory) = current_accessory.as_ref() {
                accessory.receive_audio.is_some()
            } else {
                false
            };

            let event = if non_blocking_event_receiving {
                match self.receiver.try_recv() {
                    Ok(event) => Some(event),
                    Err(TryRecvError::Disconnected) => {
                        panic!("Broken event channel.");
                    },
                    Err(TryRecvError::Empty) => None,
                }
            } else {
                Some(self.receiver.recv().unwrap())
            };

            if let Some(event) = event {
                match self.receiver.recv().unwrap() {
                    LibUsbEvent::RequestQuit => {
                        break;
                    }
                    LibUsbEvent::PollUsbDevices => {
                        match current_accessory.take() {
                            None => {
                                if flag {
                                    continue;
                                }
                                match Self::find_accessory_mode_usb_device(&context) {
                                    Ok(None) => {
                                        info!("No USB accessory devices detected.");
                                    },
                                    Ok(Some(accessory)) => {
                                        match AccessoryConnection::new(accessory) {
                                            Ok(connection) => {
                                                flag = true;
                                                current_accessory = Some(connection);
                                                continue;
                                            }
                                            Err(e) => {
                                                error!("AccessoryConnection creation failed: {e:?}");
                                                continue;
                                            }
                                        }
                                    }
                                    Err(e) => error!("Find USB accessory failed: {e:?}"),
                                }

                                match Self::find_android_usb_device(&context) {
                                    Ok(None) => {
                                        info!("No Android devices found.");
                                    },
                                    Ok(Some(())) => {
                                        info!("Android device found.");
                                    }
                                    Err(e) => error!("Find Android device failed: {e:?}"),
                                }
                            }
                            Some(accessory) => {
                                current_accessory = Some(accessory);
                            },
                        }
                    }
                    LibUsbEvent::JsonPollIfConnected => {
                        if let Some(mut accessory) = current_accessory.take() {
                            match accessory.receive_next() {
                                Ok(()) => {
                                    current_accessory = Some(accessory);
                                },
                                Err(e) => {
                                    error!("Error: {e:?}");
                                    accessory.quit();
                                },
                            }
                        }
                    }
                    LibUsbEvent::UsbEvent(event) => {
                        self.handle_usb_event(event, current_accessory.as_mut());
                    }
                }
            }

            if let Some(mut accessory) = current_accessory.take() {
                match accessory.update_connection() {
                    Ok(()) => {
                        current_accessory = Some(accessory);
                    },
                    Err(e) => {
                        error!("Error: {e:?}");
                        accessory.quit();
                    },
                }
            }
        }

        if let Some(accessory) = current_accessory {
            accessory.quit();
        }
    }

    fn handle_usb_event(&mut self, event: UsbEvent, current_accessory: Option<&mut AccessoryConnection>) {
        match event {
            UsbEvent::ReceiveAudioOverUsb(sender) => {
                if let Some(accessory) = current_accessory {
                    unimplemented!()
                }
            }
            UsbEvent::SendAudioOverUsb(receiver) => {
                if let Some(accessory) = current_accessory {
                    accessory.set_audio_receiver(receiver);
                }
            }
        }
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
    receive_audio: Option<UsbDataConnectionReceiver>,
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
            receive_audio: None,
            send_audio: None,
        })
    }

    fn set_audio_receiver(&mut self, receiver: UsbDataConnectionReceiver) {
        self.receive_audio = Some(receiver);
    }

    fn set_audio_sender(&mut self, sender: UsbDataConnectionSender) {
        self.send_audio = Some(sender);
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

        // TODO: Use main loop event receiver also for data.
        if let Some(receiver) = self.receive_audio.as_mut() {
            match receiver.recv() {
                Ok(mut packet) => {
                    packet.set_packet_type(UsbPacketOutType::AudioData as u8);
                    self.accessory.send_usb_packet(packet).map_err(AccessoryConnectionError::Usb)?;

                }
                Err(RecvError) => {
                    self.receive_audio = None;
                    info!("USB audio receiver disconnected.");
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

    fn quit(self) {
        self.accessory.reset_usb_connection();
    }
}

#[derive(Debug)]
enum UsbError {
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

pub struct AndroidUsbDevice<'a> {
    handle: &'a DeviceHandle<Context>,
}

impl <'a> AndroidUsbDevice<'a> {

    fn new(handle: &'a DeviceHandle<Context>) -> Result<Option<Self>, UsbError> {
        let mut buffer = [0u8; 2];

        let size = handle.read_control(
            rusb::request_type(Direction::In, RequestType::Vendor, rusb::Recipient::Device),
            51,
            0,
            0,
            &mut buffer,
            USB_TIMEOUT,
        ).map_err(UsbError::ReadControlFailed)?;

        if size == 2 {
            let version = u16::from_le_bytes(buffer);
            info!("AOA version: {version}");
            if version == 1 || version == 2 {
                Ok(Some(Self {
                    handle
                }))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    fn start_device_in_aoa_mode(&self) -> Result<(), UsbError> {
        self.send_aoa_id_string(AoaStringId::ManufacturerName, "Jonect Developers")?;
        self.send_aoa_id_string(AoaStringId::ModelName, "Jonect")?;
        self.send_aoa_id_string(AoaStringId::Description, "Jonect speaker app")?;
        self.send_aoa_id_string(AoaStringId::Version, "0.1")?;
        self.send_aoa_id_string(AoaStringId::Uri, "https://github.com/jutuon/jonect")?;
        self.send_aoa_id_string(AoaStringId::SerialNumber, "1234")?;

        let size = self.handle.write_control(
            rusb::request_type(Direction::Out, RequestType::Vendor, rusb::Recipient::Device),
            53,
            0,
            0,
            &[],
            USB_TIMEOUT,
        ).map_err(UsbError::WriteControlFailed)?;

        Ok(())
    }

    fn send_aoa_id_string(&self, string_id: AoaStringId, normal_text: &'static str) -> Result<(), UsbError> {
        let text = CString::new(normal_text).unwrap();

        if text.as_bytes_with_nul().len() >= 255 {
            panic!("Too long string value: {}", normal_text);
        }

        let size = self.handle.write_control(
            rusb::request_type(Direction::Out, RequestType::Vendor, rusb::Recipient::Device),
            52,
            0,
            string_id as u16,
            text.as_bytes_with_nul(),
            USB_TIMEOUT,
        ).map_err(UsbError::WriteControlFailed)?;

        if size == text.as_bytes_with_nul().len() {
            Ok(())
        } else {
            Err(UsbError::AndroidStartAccessoryModeUnexpectedWriteResult(size))
        }
    }
}

#[repr(u8)]
pub enum AoaStringId {
    ManufacturerName = 0,
    ModelName,
    Description,
    Version,
    Uri,
    SerialNumber,
}

pub struct AndroidUsbAccessory {
    handle: DeviceHandle<Context>,
    endpoint_in: u8,
    endpoint_out: u8,
    buffer: [u8; 512],
}

impl AndroidUsbAccessory {
    fn new(device: Device<Context>) -> Result<Option<Self>, UsbError> {
        let device_descriptor = device.device_descriptor().map_err(UsbError::DeviceDescriptorRequestFailed)?;

        if device_descriptor.vendor_id() == ACCESSORY_MODE_VENDOR_ID &&
            ACCESSORY_MODE_PRODUCT_IDS.contains(&device_descriptor.product_id()) {

            let mut handle = device.open().map_err(UsbError::DeviceOpenFailed)?;

            if device_descriptor.product_id() == ACCESSORY_MODE_AND_ADB_PRODUCT_ID {
                let configuration = handle.active_configuration().map_err(UsbError::GetConfigurationFailed)?;
                info!("num counfigurations: {}", device_descriptor.num_configurations());
                info!("active configuration: {configuration}");
                if configuration != 1 {
                    handle.set_active_configuration(1).map_err(UsbError::SetConfigurationFailed)?;
                }
            }

            let config = device.active_config_descriptor().map_err(UsbError::GetConfigDescriptorFailed)?;

            for interface in config.interfaces() {
                info!("interfaces: {}", interface.number());
            }

            let mut interface_number = None;
            let mut endpoint_in = None;
            let mut endpoint_out = None;

            for interface in config.interfaces() {
                if interface.number() != 0 {
                    continue;
                }

                for descriptor in interface.descriptors() {
                    if descriptor.num_endpoints() != 2 {
                        continue;
                    }

                    for endpoint in descriptor.endpoint_descriptors() {
                        let address = endpoint.address();
                        info!("Endpoint address: {address}");
                        let max_packet_size = endpoint.max_packet_size();
                        if max_packet_size != 512 {
                            return Err(UsbError::UnsupportedEndpointMaxPacketSize(max_packet_size));
                        }

                        match endpoint.direction() {
                            Direction::In => endpoint_in = Some(address),
                            Direction::Out => endpoint_out = Some(address),
                        }
                    }
                }

                interface_number = Some(0);

                if interface_number.is_some() && endpoint_in.is_some() && endpoint_out.is_some() {
                    break
                }
            }

            match (interface_number, endpoint_in, endpoint_out) {
                (Some(interface), Some(endpoint_in), Some(endpoint_out)) => {
                    handle.claim_interface(interface).map_err(UsbError::ClaimInterfaceFailed)?;

                    info!("Interface number: {interface}, endpoint in: {endpoint_in}, endpoint out: {endpoint_out}");

                    return Ok(Some(Self {
                        handle,
                        endpoint_in,
                        endpoint_out,
                        buffer: [0; 512],
                    }))
                }
                _ => return Ok(None),
            }
        }

        Ok(None)
    }

    /// Max data size is 509 bytes.
    fn send_packet(&mut self, packet_type: UsbPacketOutType, data: &[u8]) -> Result<(), UsbError> {
        let packet_type_byte = [packet_type as u8];
        let data_size: u16 = data.len().try_into().unwrap();
        let data_size_bytes = data_size.to_be_bytes();
        let packet_data = packet_type_byte.iter().chain(data_size_bytes.iter()).chain(data).chain(std::iter::repeat(&0));

        for (target, src) in self.buffer.iter_mut().zip(packet_data) {
            *target = *src;
        }

        //info!("Write bulk starts. Send packet type: {packet_type:?}");

        loop {
            let size = self.handle.write_bulk(self.endpoint_out, &self.buffer, USB_TIMEOUT)
                .map_err(UsbError::WriteBulkFailed)?;

            //info!("Write bulk size: {size}");

            if size != self.buffer.len() {
                //Err(UsbError::WriteBulkByteCount { expected: self.buffer.len() , was: size })
                info!("Write bulk retry");
            } else {
                break Ok(())
            }
        }

    }

    fn send_usb_packet(&mut self, packet: UsbPacket) -> Result<(), UsbError> {
        loop {
            let size = self.handle.write_bulk(self.endpoint_out, packet.raw(), USB_TIMEOUT)
                .map_err(UsbError::WriteBulkFailed)?;

            if size != self.buffer.len() {
                info!("Write bulk retry");
            } else {
                break Ok(())
            }
        }
    }

    fn receive_packet(&mut self) -> Result<(UsbPacketInType, &[u8]), UsbError> {
        self.send_packet(UsbPacketOutType::NextIsWrite, &[])?;

        //info!("Read bulk starts.");

        loop {
            let size = self.handle.read_bulk(self.endpoint_in, &mut self.buffer, USB_TIMEOUT)
                .map_err(UsbError::ReadBulkFailed)?;

            //info!("Read bulk size: {size}");

            if size != self.buffer.len() {
                //Err(UsbError::ReadBulkByteCount { expected: self.buffer.len()
                //, was: size })
                info!("Read bulk retry");
            } else {
                let (header, data) = self.buffer.split_at(3);

                let packet = match header[0] {
                    1 => UsbPacketInType::Empty,
                    2 => UsbPacketInType::JsonStream,
                    packet_type => return Err(UsbError::UnknownInPacket(packet_type)),
                };

                //info!("Read bulk packet_type: {packet:?}");

                let packet_size: usize = u16::from_be_bytes([header[1], header[2]]) as usize;

                break Ok((packet, &data[..packet_size]))
            }
        }
    }

    fn reset_usb_connection(mut self) {
        if let Err(e) = self.handle.reset() {
            error!("USB reset error: {e}");
        }
    }
}
