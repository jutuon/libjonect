/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Find Android device in USB accessory mode

use rusb::{DeviceHandle, Context, Device, Direction};

use log::{info, error};

use crate::connection::usb::protocol::{UsbPacketOutType, UsbPacket, UsbPacketInType};

use super::{UsbError, USB_TIMEOUT};

pub const ACCESSORY_MODE_VENDOR_ID: u16 = 0x18d1;

pub const ACCESSORY_MODE_PRODUCT_IDS: &[u16] = &[0x2d00, 0x2d01];

pub const ACCESSORY_MODE_AND_ADB_PRODUCT_ID: u16 = 0x2d01;



pub struct AndroidUsbAccessory {
    handle: DeviceHandle<Context>,
    endpoint_in: u8,
    endpoint_out: u8,
    buffer: [u8; 512],
    receive_buffer: [u8; 64],
}

impl AndroidUsbAccessory {
    pub fn new(device: Device<Context>) -> Result<Option<Self>, UsbError> {
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
                        receive_buffer: [0; 64],
                    }))
                }
                _ => return Ok(None),
            }
        }

        Ok(None)
    }

    /// Max data size is 509 bytes.
    pub fn send_packet(&mut self, packet_type: UsbPacketOutType, data: &[u8]) -> Result<(), UsbError> {
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

            if size != self.buffer.len() {
                //Err(UsbError::WriteBulkByteCount { expected: self.buffer.len() , was: size })
                info!("Write bulk retry");
            } else {
                break Ok(())
            }
        }

    }

    pub fn send_usb_packet(&mut self, packet: UsbPacket) -> Result<(), UsbError> {
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

    pub fn receive_packet(&mut self) -> Result<(UsbPacketInType, &[u8]), UsbError> {
        //info!("Read bulk starts.");

        loop {
            let size = self.handle.read_bulk(self.endpoint_in, &mut self.receive_buffer, USB_TIMEOUT)
                .map_err(UsbError::ReadBulkFailed)?;

            //info!("Read bulk size: {size}");

            if size != self.receive_buffer.len() {
                //Err(UsbError::ReadBulkByteCount { expected: self.buffer.len()
                //, was: size })
                info!("Read bulk retry {}", size);
            } else {
                let (header, data) = self.receive_buffer.split_at(3);

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
}

impl Drop for AndroidUsbAccessory {
    fn drop(&mut self) {
        if let Err(e) = self.handle.reset() {
            error!("USB reset error: {e}");
        }
    }
}
