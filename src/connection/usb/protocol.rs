/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */


pub const USB_PACKET_MAX_DATA_SIZE: u16 = 512-3;

pub const USB_IN_PACKET_MAX_DATA_SIZE: u16 = 64 - 3;

pub const USB_EMPTY_PACKET: u8 = 1;

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum UsbPacketOutType {
    Empty = USB_EMPTY_PACKET,
    /// Normal JSON message stream.
    JsonStream,
    /// Normal audio data stream.
    AudioData,
    NextIsWrite,
}


impl From<UsbPacketOutType> for u8 {
    fn from(packet_type: UsbPacketOutType) -> Self {
        packet_type as u8
    }
}


#[derive(Debug)]
#[repr(u8)]
pub enum UsbPacketInType {
    Empty = USB_EMPTY_PACKET,
    /// Normal JSON message stream.
    JsonStream,
}

impl From<UsbPacketInType> for u8 {
    fn from(packet_type: UsbPacketInType) -> Self {
        packet_type as u8
    }
}

pub struct UsbPacket {
    data: [u8; 512],
}

impl std::fmt::Debug for UsbPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsbPacket").finish()
    }
}

impl UsbPacket {
    pub fn new() -> UsbPacket {
        let mut data = [0; 512];
        data[0] = USB_EMPTY_PACKET;

        Self {
            data,
        }
    }

    pub fn set_packet_type(&mut self, packet_type: u8) {
        self.data[0] = packet_type;
    }

    pub fn raw(&self) -> &[u8; 512] {
        &self.data
    }

    /// Max size is USB_PACKET_MAX_DATA_SIZE.
    pub fn set_size(&mut self, size: u16) {
        assert!(size <= USB_PACKET_MAX_DATA_SIZE);

        let [byte1, byte2] = size.to_be_bytes();
        self.data[1] = byte1;
        self.data[2] = byte2;
    }

    pub fn data(&self) -> &[u8] {
        let size = u16::from_be_bytes([self.data[1], self.data[2]]) as usize;
        let (_, packet_data) = self.data.split_at(3);
        &packet_data[..size]
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        let size = u16::from_be_bytes([self.data[1], self.data[2]]) as usize;
        let (_, packet_data) = self.data.split_at_mut(3);
        &mut packet_data[..size]
    }
}
