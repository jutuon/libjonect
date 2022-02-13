/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! PulseAudio audio stream code.

use log::{error,info};

use std::{
    convert::TryInto,
    io::{ErrorKind, Write},
};

use audiopus::{coder::Encoder, SampleRate};
use bytes::{Buf, BytesMut};

use pulse::{context::Context, def::BufferAttr, sample::Spec, stream::Stream};

use crate::{audio::pulseaudio::state::PAEvent, connection::tcp::TcpSendHandle};

use super::EventToAudioServerSender;

/// PulseAudio recording stream events.
#[derive(Debug)]
pub enum PARecordingStreamEvent {
    StateChange,
    Read(usize),
    Moved,
}

/// PulseAudio playback stream events.
#[derive(Debug)]
pub enum PAPlaybackStreamEvent {
    DataOverflow,
    DataUnderflow,
    StateChange,
}

/// Recording stream processing errors.
#[derive(Debug)]
pub enum StreamError {
    /// Sending data to the data stream failed.
    SocketError(std::io::Error),
    /// Make sure that PulseAudio returned enough bytes for one sample when
    /// encoding Opus.
    NotEnoughBytesForOneSample,
    /// Opus encoder error.
    EncoderError(audiopus::Error),
}

/// Logic for audio data streams.
pub struct PAStreamManager {
    record: Option<(Stream, TcpSendHandle)>,
    sender: EventToAudioServerSender,
    /// Buffer used to buffer data which could not sent forward using single
    /// write call.
    data_write_buffer: BytesMut,
    /// If false do not read PCM data from PulseAudio.
    enable_recording: bool,
    /// If true send `PAEvent::StreamManagerQuitReady` when recording stream
    /// closes.
    quit_requested: bool,
    encoder: Option<Encoder>,
    /// Encoder input buffer.
    encoder_buffer: Vec<i16>,
    /// Opus documentation recommends size 4000.
    encoder_output_buffer: Box<[u8; 4000]>,
}

impl PAStreamManager {
    /// Create new `PAStreamManager`.
    pub fn new(sender: EventToAudioServerSender) -> Self {
        Self {
            record: None,
            sender,
            data_write_buffer: BytesMut::new(),
            enable_recording: true,
            quit_requested: false,
            encoder: None,
            encoder_buffer: Vec::new(),
            encoder_output_buffer: Box::new([0; 4000]),
        }
    }

    /// Start recording.
    pub fn request_start_record_stream(
        &mut self,
        context: &mut Context,
        source_name: Option<String>,
        send_handle: TcpSendHandle,
        encode_opus: bool,
        sample_rate: u32,
    ) {
        let rate = if encode_opus {
            self.encoder = Some(
                Encoder::new(
                    SampleRate::Hz48000,
                    audiopus::Channels::Stereo,
                    audiopus::Application::Audio,
                )
                .unwrap(),
            );
            self.encoder_buffer.clear();
            48000
        } else {
            sample_rate
        };

        let spec = Spec {
            format: pulse::sample::Format::S16le,
            channels: 2,
            rate,
        };

        assert!(spec.is_valid(), "Stream data specification is invalid.");

        let mut stream = Stream::new(context, "Jonect recording stream", &spec, None)
            .expect("Stream creation error");

        // PulseAudio audio buffer settings.
        let buffer_settings = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            // channels * sample byte count * sample count
            // At 44100 Hz one millisecond is about 44,1 samples.
            fragsize: 2 * 2 * 32,
        };

        stream
            .connect_record(
                source_name.as_deref(),
                Some(&buffer_settings),
                pulse::stream::FlagSet::ADJUST_LATENCY,
            )
            .unwrap();

        let mut s = self.sender.clone();
        stream.set_moved_callback(Some(Box::new(move || {
            s.send_pa_record_stream_event(PARecordingStreamEvent::Moved);
        })));

        let mut s = self.sender.clone();
        stream.set_state_callback(Some(Box::new(move || {
            s.send_pa_record_stream_event(PARecordingStreamEvent::StateChange);
        })));

        let mut s = self.sender.clone();
        stream.set_read_callback(Some(Box::new(move |size| {
            s.send_pa_record_stream_event(PARecordingStreamEvent::Read(size));
        })));

        self.data_write_buffer.clear();
        self.record = Some((stream, send_handle));
        self.enable_recording = true;
    }

    /// Handle `PARecordingStreamEvent::StateChange`.
    fn handle_recording_stream_state_change(&mut self) {
        use pulse::stream::State;

        let stream = &self.record.as_ref().unwrap().0;
        let state = stream.get_state();

        match state {
            State::Failed => {
                error!("Recording stream state: Failed.");
            }
            State::Terminated => {
                self.record = None;
                if self.quit_requested {
                    self.sender.send_pa(PAEvent::StreamManagerQuitReady);
                }
                info!("Recording stream state: Terminated.");
            }
            State::Ready => {
                info!("Recording from {:?}", stream.get_device_name());
            }
            _ => (),
        }
    }

    /// Send data to the output stream. Data which can not be sent using single
    /// write call will be buffered.
    fn handle_data(
        data: &[u8],
        data_write_buffer: &mut BytesMut,
        send_handle: &mut TcpSendHandle,
    ) -> Result<(), StreamError> {
        loop {
            // TODO: Add limit to the buffer.
            if data_write_buffer.has_remaining() {
                match send_handle.write(data_write_buffer.chunk()) {
                    Ok(count) => {
                        data_write_buffer.advance(count);
                    }
                    Err(e) => {
                        if ErrorKind::WouldBlock == e.kind() {
                            break;
                        } else {
                            return Err(StreamError::SocketError(e));
                        }
                    }
                }
            } else {
                break;
            }
        }

        match send_handle.write(data) {
            Ok(count) => {
                if count < data.len() {
                    let remaining_bytes = &data[count..];
                    data_write_buffer.extend_from_slice(remaining_bytes);
                    info!(
                        "Recording state: buffering {} bytes.",
                        remaining_bytes.len()
                    );
                }
            }
            Err(e) => {
                if ErrorKind::WouldBlock == e.kind() {
                    data_write_buffer.extend_from_slice(data);
                } else {
                    return Err(StreamError::SocketError(e));
                }
            }
        }

        Ok(())
    }

    /// Encode PCM data and send encoded audio forward.
    fn handle_data_with_encoding(
        data: &[u8],
        encoding_buffer: &mut Vec<i16>,
        encoder_output_buffer: &mut [u8; 4000],
        encoder: &mut Encoder,
        data_write_buffer: &mut BytesMut,
        send_handle: &mut TcpSendHandle,
    ) -> Result<(), StreamError> {
        if data.len() % 2 != 0 {
            return Err(StreamError::NotEnoughBytesForOneSample);
        }

        let sample_iterator = data.chunks_exact(2);

        for bytes in sample_iterator {
            let sample = i16::from_le_bytes(bytes.try_into().unwrap());
            encoding_buffer.push(sample);

            if encoding_buffer.len() == 240 {
                // 240 is 120 samples per channel. That is minimum frame size
                // for Opus at 48 kHz.

                let size = encoder
                    .encode(encoding_buffer, encoder_output_buffer)
                    .map_err(StreamError::EncoderError)?;

                encoding_buffer.clear();

                let protocol_size: i32 = size.try_into().unwrap();

                Self::handle_data(&protocol_size.to_be_bytes(), data_write_buffer, send_handle)?;
                Self::handle_data(
                    &encoder_output_buffer[..size],
                    data_write_buffer,
                    send_handle,
                )?;
            }
        }

        Ok(())
    }

    /// Handle `PARecordingStreamEvent::Read`.
    fn handle_recording_stream_read(&mut self) {
        let (r, ref mut send_handle) = self.record.as_mut().unwrap();

        if !self.enable_recording {
            return;
        }

        use pulse::stream::PeekResult;

        loop {
            let peek_result = r.peek().unwrap();
            match peek_result {
                PeekResult::Empty => {
                    break;
                }
                PeekResult::Data(data) => {
                    let result = if let Some(encoder) = &mut self.encoder {
                        Self::handle_data_with_encoding(
                            data,
                            &mut self.encoder_buffer,
                            &mut self.encoder_output_buffer,
                            encoder,
                            &mut self.data_write_buffer,
                            send_handle,
                        )
                    } else {
                        Self::handle_data(data, &mut self.data_write_buffer, send_handle)
                    };

                    match result {
                        Ok(()) => (),
                        Err(e) => {
                            error!("Audio streaming error: {:?}", e);
                            r.discard().unwrap();
                            self.enable_recording = false;
                            self.stop_recording();
                            return;
                        }
                    }
                }
                PeekResult::Hole(_) => (),
            }

            r.discard().unwrap();
        }
    }

    /// Handle `PARecordingStreamEvent`.
    pub fn handle_recording_stream_event(&mut self, event: PARecordingStreamEvent) {
        match event {
            PARecordingStreamEvent::StateChange => {
                self.handle_recording_stream_state_change();
            }
            PARecordingStreamEvent::Read(_) => {
                self.handle_recording_stream_read();
            }
            PARecordingStreamEvent::Moved => {
                if let Some(name) = self.record.as_ref().map(|(s, _)| s.get_device_name()) {
                    info!("Recording stream moved. Device name: {:?}", name);
                }
            }
        }
    }

    /// Disconnect recording stream.
    pub fn stop_recording(&mut self) {
        if let Some((stream, _)) = self.record.as_mut() {
            stream.disconnect().unwrap();
        }
    }

    /// Request quit. Event `PAEvent::StreamManagerQuitReady` will be sent when
    /// `PAStreamManager` can be dropped.
    pub fn request_quit(&mut self) {
        self.quit_requested = true;

        if let Some((stream, _)) = self.record.as_mut() {
            stream.disconnect().unwrap();
        } else {
            self.sender.send_pa(PAEvent::StreamManagerQuitReady);
        }
    }
}
