/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::os::unix::prelude::RawFd;

use crate::connection::usb::protocol::{UsbPacketOutType, UsbPacketInType};

use super::UsbError;



pub struct Accessory;

impl Accessory {
    pub fn new(fd: RawFd) -> Result<(AccessoryReader, AccessoryWriter), UsbError> {
        let reader = AccessoryReader::new(fd);

        // If dup returns error the reader should run drop so fd will be closed.
        let write_fd = nix::unistd::dup(fd).map_err(UsbError::Dup)?;

        let writer = AccessoryWriter::new(write_fd);

        Ok((reader, writer))
    }
}


pub struct AccessoryReader {
    fd: RawFd,
    buffer: [u8; 512],
}

impl AccessoryReader {
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
}


impl Drop for AccessoryReader {
    fn drop(&mut self) {
        if let Err(e) = nix::unistd::close(self.fd) {
            log::error!("AccessoryReader drop error: {e}");
        }
    }
}

pub struct AccessoryWriter {
    fd: RawFd,
    buffer: [u8; 64],
}

impl AccessoryWriter {
    pub fn new(fd: RawFd) -> Self {
        Self {
            fd,
            buffer: [0; 64],
        }
    }

    pub fn write_packet(&mut self, packet_type: UsbPacketInType, data: &[u8]) -> Result<(), UsbError> {
        //log::info!("Write packet_type {:?}", packet_type);
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

impl Drop for AccessoryWriter {
    fn drop(&mut self) {
        if let Err(e) = nix::unistd::close(self.fd) {
            log::error!("AccessoryWriter drop error: {e}");
        }
    }
}
