/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Find Android USB device

use std::ffi::CString;

use rusb::{Context, DeviceHandle, Direction, RequestType};

use log::{info, error};

use super::{UsbError, USB_TIMEOUT};

pub struct AndroidUsbDevice<'a> {
    handle: &'a DeviceHandle<Context>,
}

impl <'a> AndroidUsbDevice<'a> {
    pub fn new(handle: &'a DeviceHandle<Context>) -> Result<Option<Self>, UsbError> {
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

    pub fn start_device_in_aoa_mode(&self) -> Result<(), UsbError> {
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

    pub fn send_aoa_id_string(&self, string_id: AoaStringId, normal_text: &'static str) -> Result<(), UsbError> {
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
