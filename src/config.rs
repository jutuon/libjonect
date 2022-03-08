/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! App configuration constants and command line argument parsing.

use std::net::{SocketAddr};

/// Size for event channel buffers.
pub const EVENT_CHANNEL_SIZE: usize = 32;

/// Socket address for device JSON connections.
pub const DEVICE_SOCKET_ADDRESS: &str = "0.0.0.0:8080";

/// Socket address for data connections.
pub const AUDIO_DATA_SOCKET_ADDRESS: &str = "0.0.0.0:8082";

pub const DATA_PORT_UDP_SEND_ADDRESS: &str = "0.0.0.0:8082";
pub const DATA_PORT_UDP_RECEIVE_ADDRESS: &str = "0.0.0.0:8083";

/// Socket address UI connection.
pub const UI_SOCKET_ADDRESS: &str = "127.0.0.1:8081";

pub const DATA_PORT_TCP: u16 = 8082;
pub const DATA_PORT_UDP_SEND: u16 = 8082;
pub const DATA_PORT_UDP_RECEIVE: u16 = 8083;
pub const JSON_PORT: u16 = 8080;

// 240 is 120 16-bit samples per channel. That is minimum frame size for Opus at
// 48 kHz. This value is just for sending raw PCM, but lets use the same size
// for simplicity.
pub const RAW_PCM_AUDIO_UDP_DATA_SIZE_IN_BYTES: usize = 240 * 2;

#[derive(Debug, Clone)]
pub struct LogicConfig {
    pub pa_source_name: Option<String>,
    pub encode_opus: bool,
    pub enable_connection_listening: bool,
    pub enable_ping: bool,
    pub connect_address: Option<SocketAddr>,
    pub enable_udp_audio_data_sending: bool,
}
