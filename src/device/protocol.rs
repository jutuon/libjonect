/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Jonect command protocol
//!
//! # Data transfer protocol
//! 1. Send JSON message size as i32 (big endian).
//! 2. Send JSON message (UTF-8).
//!
//! # Protocol messages
//! TODO: update
//! 1. Server sends ServerMessage::ServerInfo message to the client.
//! 2. Client sends ClientMessage::ClientInfo message to the server.
//! 3. Client and server communicate with different protocol messages.
//!    There is no need to have a specific message sending order between
//!    the server and client because messages are processed as they
//!    are received.

use serde::{Deserialize, Serialize};

use crate::connection::DataConnectionType;

/// Available audio stream formats.
pub enum AudioFormat {
    // 16-bit little endian PCM samples.
    Pcm,
    // Opus encoded audio stream.
    Opus,
}

impl AudioFormat {
    /// Convert to string which is used in the JSON message.
    pub fn as_json_value(&self) -> &'static str {
        match self {
            Self::Pcm => "pcm-s16le",
            Self::Opus => "opus",
        }
    }
}


pub struct UnsupportedFormat;

impl TryFrom<&str> for AudioFormat {
    type Error = UnsupportedFormat;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let format = match value {
            "pcm-s16le" => Self::Pcm,
            "opus" => Self::Opus,
            _ => return Err(UnsupportedFormat),
        };

        Ok(format)
    }
}


/// Server informs the client about available audio stream.
#[derive(Debug, Serialize, Deserialize)]
pub struct AudioStreamInfo {
    /// Possible values:
    /// * pcm-s16le - 16-bit little endian PCM samples.
    /// * opus - Opus encoded audio stream.
    format: String,
    pub channels: u8,
    /// Sample rate.
    ///
    /// Possible values:
    /// * 44100
    /// * 48000
    pub rate: u32,
    /// Server TCP port for the audio stream.
    pub port: u16,
    pub data_connection_type: DataConnectionType,
}

impl AudioStreamInfo {
    // TODO: Use enum for sample rate.
    pub fn new(format: AudioFormat, channels: u8, rate: u32, port: u16, data_connection_type: DataConnectionType) -> Self {
        Self {
            format: format.as_json_value().to_string(),
            channels,
            rate,
            port,
            data_connection_type,
        }
    }

    pub fn try_parse_audio_format(&self) -> Result<AudioFormat, UnsupportedFormat> {
        self.format.as_str().try_into()
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NativeSampleRate {
    pub native_sample_rate: i32,
}

impl NativeSampleRate {
    pub fn new(native_sample_rate: i32) -> Self {
        Self {
            native_sample_rate
        }
    }
}

/// Message from client to server.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DeviceMessage {
    Ping,
    PingResponse,
    AudioStreamPlayError(String),
    PlayAudioStream(AudioStreamInfo),
    /// Request native sample rate of device which will receive this message.
    GetNativeSampleRate,
    /// Native sample rate of device which send this message.
    NativeSampleRate(NativeSampleRate),
    /// Device which handles PlayAudioStream message will send this after
    /// receiving PlayAudioStream.
    StartAudioStream,
}
