/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! PulseAudio audio stream code.

use log::{error,info, warn};

use std::{
    convert::TryInto,
    io::{ErrorKind, Write}, num::Wrapping,
};

use audiopus::{coder::Encoder, SampleRate};
use bytes::{Buf, BytesMut};

use pulse::{context::Context, def::BufferAttr, sample::Spec, stream::Stream};

use crate::{audio::pulseaudio::state::PAEvent, connection::data::{DataSender, DataSenderBuilder, MAX_PACKET_SIZE}, config::RAW_PCM_AUDIO_UDP_DATA_SIZE_IN_BYTES};

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
    record: Option<(Stream, DataSender)>,
    sender: EventToAudioServerSender,
    /// If false do not read PCM data from PulseAudio.
    enable_recording: bool,
    /// If true send `PAEvent::StreamManagerQuitReady` when recording stream
    /// closes.
    quit_requested: bool,
    /// Encoder input buffer.
    raw_pcm_buffer: Vec<u8>,
    encoder: Option<Encoder>,
    /// Encoder input buffer.
    encoder_buffer: Vec<i16>,
    /// Opus documentation recommends size 4000.
    encoder_output_buffer: Box<[u8; 4000]>,
    audio_packet_drop_count: u64,
    audio_packet_counter: Wrapping<u32>,
    audio_packet_buffer: Vec<u8>,
}

impl PAStreamManager {
    /// Create new `PAStreamManager`.
    pub fn new(sender: EventToAudioServerSender) -> Self {
        Self {
            record: None,
            sender,
            enable_recording: true,
            quit_requested: false,
            raw_pcm_buffer: Vec::new(),
            encoder: None,
            encoder_buffer: Vec::new(),
            encoder_output_buffer: Box::new([0; 4000]),
            audio_packet_drop_count: 0,
            audio_packet_counter: Wrapping(0),
            audio_packet_buffer: Vec::new(),
        }
    }

    /// Start recording.
    pub fn request_start_record_stream(
        &mut self,
        context: &mut Context,
        source_name: Option<String>,
        send_handle: DataSenderBuilder,
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

        self.record = Some((stream, send_handle.build()));
        self.enable_recording = true;
        self.audio_packet_drop_count = 0;
        self.audio_packet_counter = Wrapping(0);
        self.audio_packet_buffer.clear();
        self.raw_pcm_buffer.clear();
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
                if self.audio_packet_drop_count != 0 {
                    info!("Audio packet drop count: {}", self.audio_packet_drop_count);
                } else {
                    warn!("Audio packet drop count: {}", self.audio_packet_drop_count);
                }
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
        send_handle: &mut DataSender,
        audio_packet_drop_count: &mut u64,
        audio_packet_counter: &mut Wrapping<u32>,
        audio_packet_buffer: &mut Vec<u8>,
    ) -> Result<(), StreamError> {
        assert!(data.len() <= MAX_PACKET_SIZE);

        audio_packet_buffer.clear();

        let packet_number = audio_packet_counter.0;
        *audio_packet_counter += Wrapping(1);

        audio_packet_buffer.extend_from_slice(&packet_number.to_be_bytes());
        audio_packet_buffer.extend_from_slice(data);

        match send_handle.send_packet(audio_packet_buffer) {
            Ok(()) => (),
            Err(e) => {
                if ErrorKind::WouldBlock == e.kind() {
                    *audio_packet_drop_count += 1;
                } else {
                    return Err(StreamError::SocketError(e));
                }
            }
        }

        Ok(())
    }

    /// Encode PCM data and send encoded audio forward.
    fn handle_raw_pcm_data(
        data: &[u8],
        raw_audio_data_buffer: &mut Vec<u8>,
        send_handle: &mut DataSender,
        audio_packet_drop_count: &mut u64,
        audio_packet_counter: &mut Wrapping<u32>,
        audio_packet_buffer: &mut Vec<u8>,
    ) -> Result<(), StreamError> {
        if data.len() % 2 != 0 {
            return Err(StreamError::NotEnoughBytesForOneSample);
        }

        for byte in data {
            raw_audio_data_buffer.push(*byte);

            if raw_audio_data_buffer.len() == RAW_PCM_AUDIO_UDP_DATA_SIZE_IN_BYTES {

                Self::handle_data(
                    raw_audio_data_buffer,
                    send_handle,
                    audio_packet_drop_count,
                    audio_packet_counter,
                    audio_packet_buffer,
                )?;

                raw_audio_data_buffer.clear();
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
        send_handle: &mut DataSender,
        audio_packet_drop_count: &mut u64,
        audio_packet_counter: &mut Wrapping<u32>,
        audio_packet_buffer: &mut Vec<u8>,
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

                Self::handle_data(
                    &encoder_output_buffer[..size],
                    send_handle,
                    audio_packet_drop_count,
                    audio_packet_counter,
                    audio_packet_buffer,
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
                            send_handle,
                            &mut self.audio_packet_drop_count,
                            &mut self.audio_packet_counter,
                            &mut self.audio_packet_buffer,
                        )
                    } else {
                        Self::handle_raw_pcm_data(
                            data,
                            &mut self.raw_pcm_buffer,
                            send_handle,
                            &mut self.audio_packet_drop_count,
                            &mut self.audio_packet_counter,
                            &mut self.audio_packet_buffer,
                        )
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
