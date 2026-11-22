//! Pure-Rust audio decoders for Opus, AAC-LC, Vorbis, MP3, FLAC, and PCM.

use std::io::Cursor;

use symphonia::core::audio::Channels;
use symphonia::core::codecs::audio::well_known as audio_wk;
use symphonia::core::codecs::audio::{
    AudioCodecParameters, AudioDecoder as SymphoniaAudioDecoder, AudioDecoderOptions,
};
use symphonia::core::codecs::registry::CodecRegistry;
use symphonia::core::common::Tier;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::packet::PacketRef;
use symphonia::core::units::{Duration, Timestamp};

use crate::demuxer::detect_container;
use crate::types::{
    AudioCodec, AudioFrame, AudioTrackConfig, ContainerFormat, EncodedPacket, MediaError,
    PcmSampleFormat, TrackInfo, TrackKind,
};

/// Unified pure-Rust audio decoder trait.
pub trait AudioDecoder: Send {
    /// Decodes one elementary audio packet into zero or more interleaved `f32` [`AudioFrame`]s.
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Vec<AudioFrame>, MediaError>;

    /// Resets internal decoder state (for seeking or discontinuity).
    fn reset(&mut self);
}

/// Builds a Symphonia [`CodecRegistry`] with all enabled pure-Rust codecs plus safe `symphonia-opus`.
pub fn build_audio_codec_registry() -> CodecRegistry {
    let mut registry = CodecRegistry::new();
    symphonia::default::register_enabled_codecs(&mut registry);
    registry.register_audio_decoder_at_tier::<symphonia_opus::OpusDecoder>(Tier::Fallback);
    registry
}

/// Creates a pure-Rust [`AudioDecoder`] matching the given audio track metadata.
pub fn create_audio_decoder(track: &TrackInfo) -> Result<Box<dyn AudioDecoder>, MediaError> {
    let TrackKind::Audio(cfg) = &track.kind else {
        return Err(MediaError::InvalidConfig(
            "expected an audio track for audio decoder creation".to_string(),
        ));
    };

    match cfg.codec {
        AudioCodec::Opus => Ok(Box::new(PureOpusDecoder::new(cfg)?)),
        AudioCodec::Pcm => Ok(Box::new(RawPcmDecoder::new(cfg))),
        AudioCodec::Aac | AudioCodec::Vorbis | AudioCodec::Mp3 | AudioCodec::Flac => Ok(Box::new(
            SymphoniaPacketAudioDecoder::new(cfg, &track.extra_data)?,
        )),
    }
}

/// Decodes a complete audio container buffer (WAV, Ogg/Opus, Ogg/Vorbis, MP3, FLAC, MP4/AAC, WebM/Opus)
/// directly to interleaved `f32` [`AudioFrame`]s. Used both for container playback and `AudioContext.decodeAudioData`.
pub fn decode_container_audio(
    bytes: &[u8],
    format_hint: Option<ContainerFormat>,
) -> Result<Vec<AudioFrame>, MediaError> {
    let detected = format_hint.or_else(|| detect_container(bytes));

    // Try direct Symphonia probe + decode first (handles WAV, Ogg/Opus, Ogg/Vorbis, MP3, FLAC, MP4, MKV).
    if let Ok(frames) = decode_container_via_symphonia(bytes, detected) {
        if !frames.is_empty() {
            return Ok(frames);
        }
    }

    // Fallback to our own MP4/WebM demuxers + packet decoders (covers MSE fMP4/WebM segments).
    let demuxed = crate::demuxer::demux_bytes(bytes, detected)?;
    let audio_track = demuxed
        .tracks
        .iter()
        .find(|t| matches!(t.kind, TrackKind::Audio(_)))
        .ok_or_else(|| MediaError::AudioDecode("container has no audio track".to_string()))?;

    let mut decoder = create_audio_decoder(audio_track)?;
    let mut frames = Vec::new();
    for pkt in &demuxed.packets {
        if pkt.track_id == audio_track.id {
            frames.extend(decoder.decode(pkt)?);
        }
    }
    Ok(frames)
}

fn decode_container_via_symphonia(
    bytes: &[u8],
    container: Option<ContainerFormat>,
) -> Result<Vec<AudioFrame>, MediaError> {
    let codecs = build_audio_codec_registry();
    let cursor = Cursor::new(bytes.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

    let mut hint = Hint::new();
    if let Some(c) = container {
        let ext = match c {
            ContainerFormat::Mp4 => "mp4",
            ContainerFormat::WebM => "webm",
            ContainerFormat::Ogg => "ogg",
            ContainerFormat::Wav => "wav",
            ContainerFormat::Mp3 => "mp3",
            ContainerFormat::Flac => "flac",
        };
        hint.with_extension(ext);
    }

    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| MediaError::AudioDecode(format!("symphonia probe failed: {e}")))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| MediaError::AudioDecode("no default audio track".to_string()))?;
    let track_id = track.id;
    let time_base = track.time_base;
    let audio_params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| MediaError::AudioDecode("track lacks audio codec parameters".to_string()))?
        .clone();

    let mut decoder = codecs
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|e| MediaError::AudioDecode(format!("audio decoder init failed: {e}")))?;

    let mut out = Vec::new();
    loop {
        match format.next_packet() {
            Ok(Some(packet)) => {
                if packet.track_id != track_id {
                    continue;
                }
                let decoded = match decoder.decode(&packet) {
                    Ok(buf) => buf,
                    Err(SymphoniaError::DecodeError(_)) => continue,
                    Err(err) => {
                        return Err(MediaError::AudioDecode(format!(
                            "symphonia decode error: {err}"
                        )));
                    }
                };
                if decoded.frames() == 0 {
                    continue;
                }
                let sample_rate = decoded.spec().rate();
                let channels = decoded.spec().channels().count().max(1) as u16;
                let mut samples = Vec::with_capacity(decoded.samples_interleaved());
                decoded.copy_to_vec_interleaved(&mut samples);

                let pts_us = time_base
                    .map(|tb| tb.calc_time_saturating(packet.pts).as_micros() as i64)
                    .unwrap_or(0);
                let duration_us =
                    ((decoded.frames() as u64) * 1_000_000) / u64::from(sample_rate.max(1));

                out.push(AudioFrame {
                    sample_rate,
                    channels,
                    samples,
                    pts_us,
                    duration_us,
                });
            }
            Ok(None) => break,
            Err(SymphoniaError::IoError(err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
            }
            Err(err) => {
                return Err(MediaError::AudioDecode(format!(
                    "symphonia read error: {err}"
                )));
            }
        }
    }
    Ok(out)
}

/// Pure-Rust RFC 8251 Opus packet decoder backed by `opus-decoder` (`#![forbid(unsafe_code)]`).
pub struct PureOpusDecoder {
    inner: opus_decoder::OpusDecoder,
    sample_rate: u32,
    channels: u16,
    scratch: Vec<f32>,
}

impl PureOpusDecoder {
    pub fn new(cfg: &AudioTrackConfig) -> Result<Self, MediaError> {
        // RFC 6716 supports 8000, 12000, 16000, 24000, 48000 Hz output; default to 48 kHz.
        let sample_rate = match cfg.sample_rate {
            8_000 | 12_000 | 16_000 | 24_000 | 48_000 => cfg.sample_rate,
            _ => 48_000,
        };
        let channels = cfg.channels.clamp(1, 2);
        let inner = opus_decoder::OpusDecoder::new(sample_rate, usize::from(channels))
            .map_err(|e| MediaError::AudioDecode(format!("opus init failed: {e:?}")))?;
        let max_samples = inner.max_frame_size_per_channel() * usize::from(channels);
        Ok(Self {
            inner,
            sample_rate,
            channels,
            scratch: vec![0.0; max_samples],
        })
    }
}

impl AudioDecoder for PureOpusDecoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Vec<AudioFrame>, MediaError> {
        if packet.data.is_empty() {
            return Ok(Vec::new());
        }
        let frames_per_ch = self
            .inner
            .decode_float(&packet.data, &mut self.scratch, false)
            .map_err(|e| MediaError::AudioDecode(format!("opus decode failed: {e:?}")))?;
        let total_samples = frames_per_ch * usize::from(self.channels);
        let duration_us = ((frames_per_ch as u64) * 1_000_000) / u64::from(self.sample_rate.max(1));
        Ok(vec![AudioFrame {
            sample_rate: self.sample_rate,
            channels: self.channels,
            samples: self.scratch[..total_samples].to_vec(),
            pts_us: packet.pts_us,
            duration_us,
        }])
    }

    fn reset(&mut self) {
        if let Ok(fresh) =
            opus_decoder::OpusDecoder::new(self.sample_rate, usize::from(self.channels))
        {
            self.inner = fresh;
        }
    }
}

/// Pure-Rust Symphonia packet decoder for AAC-LC, Vorbis, MP3, and FLAC elementary streams.
pub struct SymphoniaPacketAudioDecoder {
    inner: Box<dyn SymphoniaAudioDecoder>,
}

impl SymphoniaPacketAudioDecoder {
    pub fn new(cfg: &AudioTrackConfig, extra_data: &[u8]) -> Result<Self, MediaError> {
        let codecs = build_audio_codec_registry();
        let codec_id = match cfg.codec {
            AudioCodec::Aac => audio_wk::CODEC_ID_AAC,
            AudioCodec::Vorbis => audio_wk::CODEC_ID_VORBIS,
            AudioCodec::Mp3 => audio_wk::CODEC_ID_MP3,
            AudioCodec::Flac => audio_wk::CODEC_ID_FLAC,
            AudioCodec::Opus => audio_wk::CODEC_ID_OPUS,
            AudioCodec::Pcm => audio_wk::CODEC_ID_PCM_S16LE,
        };

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(codec_id)
            .with_sample_rate(cfg.sample_rate.max(8_000))
            .with_channels(Channels::Discrete(cfg.channels.max(1)));
        if !extra_data.is_empty() {
            params.with_extra_data(extra_data.to_vec().into_boxed_slice());
        }

        let inner = codecs
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .map_err(|e| {
                MediaError::AudioDecode(format!(
                    "failed to instantiate {:?} decoder: {e}",
                    cfg.codec
                ))
            })?;

        Ok(Self { inner })
    }
}

impl AudioDecoder for SymphoniaPacketAudioDecoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Vec<AudioFrame>, MediaError> {
        if packet.data.is_empty() {
            return Ok(Vec::new());
        }
        let pref = PacketRef::new(
            packet.track_id,
            Timestamp::from(packet.pts_us),
            Duration::from(packet.duration_us),
            &packet.data,
        );
        let decoded = self
            .inner
            .decode_ref(&pref)
            .map_err(|e| MediaError::AudioDecode(format!("packet decode failed: {e}")))?;
        if decoded.frames() == 0 {
            return Ok(Vec::new());
        }
        let sample_rate = decoded.spec().rate();
        let channels = decoded.spec().channels().count().max(1) as u16;
        let mut samples = Vec::with_capacity(decoded.samples_interleaved());
        decoded.copy_to_vec_interleaved(&mut samples);
        let duration_us = ((decoded.frames() as u64) * 1_000_000) / u64::from(sample_rate.max(1));
        Ok(vec![AudioFrame {
            sample_rate,
            channels,
            samples,
            pts_us: packet.pts_us,
            duration_us,
        }])
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}

/// Direct raw PCM decoder supporting U8, S16LE, S24LE, S32LE, and F32LE interleaved streams.
pub struct RawPcmDecoder {
    sample_rate: u32,
    channels: u16,
    format: PcmSampleFormat,
}

impl RawPcmDecoder {
    pub fn new(cfg: &AudioTrackConfig) -> Self {
        Self {
            sample_rate: cfg.sample_rate.max(1),
            channels: cfg.channels.max(1),
            format: cfg.pcm_format.unwrap_or(PcmSampleFormat::I16Le),
        }
    }
}

impl AudioDecoder for RawPcmDecoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Vec<AudioFrame>, MediaError> {
        let samples: Vec<f32> = match self.format {
            PcmSampleFormat::U8 => packet
                .data
                .iter()
                .map(|&b| (f32::from(b) - 128.0) / 128.0)
                .collect(),
            PcmSampleFormat::I16Le => packet
                .data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&b| f32::from(i16::from_le_bytes(b)) / 32_768.0)
                .collect(),
            PcmSampleFormat::I24Le => packet
                .data
                .as_chunks::<3>()
                .0
                .iter()
                .map(|&b| {
                    let sign_ext = if (b[2] & 0x80) != 0 { 0xff } else { 0x00 };
                    let v = i32::from_le_bytes([b[0], b[1], b[2], sign_ext]);
                    (v as f32) / 8_388_608.0
                })
                .collect(),
            PcmSampleFormat::I32Le => packet
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&b| (i32::from_le_bytes(b) as f64 / 2_147_483_648.0) as f32)
                .collect(),
            PcmSampleFormat::F32Le => packet
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&b| f32::from_le_bytes(b))
                .collect(),
        };
        let frame_count = samples.len() / usize::from(self.channels);
        let duration_us = ((frame_count as u64) * 1_000_000) / u64::from(self.sample_rate);
        Ok(vec![AudioFrame {
            sample_rate: self.sample_rate,
            channels: self.channels,
            samples,
            pts_us: packet.pts_us,
            duration_us,
        }])
    }

    fn reset(&mut self) {}
}
