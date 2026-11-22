//! Core media container, track, packet, and decoded frame types for `rix-media`.

use sha2::{Digest, Sha256};
use thiserror::Error;

/// Supported container formats for demuxing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContainerFormat {
    Mp4,
    WebM,
    Ogg,
    Wav,
    Mp3,
    Flac,
}

/// Supported video codecs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    Av1,
    H264,
    Vp8,
    Vp9,
}

impl VideoCodec {
    /// Parses an RFC 6381 / WebM / MIME codec string into a [`VideoCodec`].
    pub fn from_codec_str(codec: &str) -> Option<Self> {
        let lower = codec.trim().to_ascii_lowercase();
        if lower.starts_with("av01") || lower == "av1" || lower == "v_av1" {
            Some(Self::Av1)
        } else if lower.starts_with("avc1")
            || lower.starts_with("avc3")
            || lower == "h264"
            || lower == "v_mpeg4/iso/avc"
        {
            Some(Self::H264)
        } else if lower == "vp8" || lower.starts_with("vp08") || lower == "v_vp8" {
            Some(Self::Vp8)
        } else if lower == "vp9" || lower.starts_with("vp09") || lower == "v_vp9" {
            Some(Self::Vp9)
        } else {
            None
        }
    }
}

/// Supported audio codecs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    Opus,
    Aac,
    Vorbis,
    Mp3,
    Flac,
    Pcm,
}

impl AudioCodec {
    /// Parses an RFC 6381 / WebM / MIME codec string into an [`AudioCodec`].
    pub fn from_codec_str(codec: &str) -> Option<Self> {
        let lower = codec.trim().to_ascii_lowercase();
        if lower == "opus" || lower == "a_opus" {
            Some(Self::Opus)
        } else if lower.starts_with("mp4a.40") || lower == "aac" || lower == "a_aac" {
            Some(Self::Aac)
        } else if lower == "vorbis" || lower == "a_vorbis" {
            Some(Self::Vorbis)
        } else if lower == "mp3" || lower == "mp4a.6b" || lower == "a_mpeg/l3" {
            Some(Self::Mp3)
        } else if lower == "flac" || lower == "a_flac" {
            Some(Self::Flac)
        } else if lower.starts_with("pcm")
            || lower == "a_pcm/int/lit"
            || lower == "a_pcm/float/ieee"
        {
            Some(Self::Pcm)
        } else {
            None
        }
    }
}

/// Sample layout for raw PCM audio streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PcmSampleFormat {
    U8,
    I16Le,
    I24Le,
    I32Le,
    F32Le,
}

/// Video track configuration parsed from container metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoTrackConfig {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
}

/// Audio track configuration parsed from container metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTrackConfig {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u16,
    pub pcm_format: Option<PcmSampleFormat>,
}

/// Track media type and codec parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackKind {
    Video(VideoTrackConfig),
    Audio(AudioTrackConfig),
}

/// Demuxed elementary stream metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInfo {
    pub id: u32,
    pub kind: TrackKind,
    pub timescale: u32,
    pub duration_us: Option<u64>,
    pub language: Option<String>,
    /// Codec-specific initialization bytes (`avcC`, `av1C`, `vpcC`, `esds` ASC, `OpusHead`, etc.).
    pub extra_data: Vec<u8>,
}

/// Elementary packet media category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketKind {
    Video,
    Audio,
}

/// A demuxed encoded media packet ready for a video or audio decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedPacket {
    pub track_id: u32,
    pub kind: PacketKind,
    pub data: Vec<u8>,
    /// Presentation timestamp in microseconds.
    pub pts_us: i64,
    /// Decode timestamp in microseconds.
    pub dts_us: i64,
    /// Packet duration in microseconds.
    pub duration_us: u64,
    /// Whether this packet is a random-access point (keyframe / IDR / sync sample).
    pub is_keyframe: bool,
}

/// Planar pixel layout for decoded video frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 3-plane 8-bit 4:2:0 Y, U, V (`I420`).
    Yuv420p,
    /// 2-plane 8-bit 4:2:0 Y + interleaved UV (`NV12`).
    Nv12,
}

/// YUV-to-RGB color conversion matrix standard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorMatrix {
    Bt709,
    Bt601,
}

/// YUV signal quantization range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorRange {
    /// Video / studio range (`Y: 16..=235`, `UV: 16..=240`).
    Limited,
    /// Full / PC range (`0..=255`).
    Full,
}

/// Decoded planar video frame kept in native YUV420p or NV12 layout for zero-copy GPU upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub pixel_format: PixelFormat,
    pub color_matrix: ColorMatrix,
    pub color_range: ColorRange,
    pub y_plane: Vec<u8>,
    /// U plane (`Yuv420p`) or interleaved UV plane (`Nv12`).
    pub u_plane: Vec<u8>,
    /// V plane (`Yuv420p`) or empty (`Nv12`).
    pub v_plane: Vec<u8>,
    pub y_stride: u32,
    pub u_stride: u32,
    pub v_stride: u32,
    pub pts_us: i64,
    pub duration_us: u64,
    pub is_keyframe: bool,
}

impl VideoFrame {
    /// Computes a deterministic 64-bit FNV-1a checksum over the visible Y, U, and V pixels
    /// (excluding any trailing stride padding bytes).
    pub fn checksum_fnv1a64(&self) -> u64 {
        const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0100_0000_01b3;

        let mut hash = FNV_OFFSET;
        let mut feed = |slice: &[u8]| {
            for &b in slice {
                hash ^= u64::from(b);
                hash = hash.wrapping_mul(FNV_PRIME);
            }
        };

        let w = self.width as usize;
        let h = self.height as usize;
        let cw = w.div_ceil(2);
        let ch = h.div_ceil(2);

        for row in 0..h {
            let start = row * self.y_stride as usize;
            let end = (start + w).min(self.y_plane.len());
            if start < end {
                feed(&self.y_plane[start..end]);
            }
        }

        match self.pixel_format {
            PixelFormat::Yuv420p => {
                for row in 0..ch {
                    let start = row * self.u_stride as usize;
                    let end = (start + cw).min(self.u_plane.len());
                    if start < end {
                        feed(&self.u_plane[start..end]);
                    }
                }
                for row in 0..ch {
                    let start = row * self.v_stride as usize;
                    let end = (start + cw).min(self.v_plane.len());
                    if start < end {
                        feed(&self.v_plane[start..end]);
                    }
                }
            }
            PixelFormat::Nv12 => {
                let row_bytes = cw * 2;
                for row in 0..ch {
                    let start = row * self.u_stride as usize;
                    let end = (start + row_bytes).min(self.u_plane.len());
                    if start < end {
                        feed(&self.u_plane[start..end]);
                    }
                }
            }
        }

        hash
    }

    /// Computes a deterministic SHA-256 hex digest over the visible planar pixel bytes.
    pub fn checksum_sha256_hex(&self) -> String {
        let mut hasher = Sha256::new();
        let w = self.width as usize;
        let h = self.height as usize;
        let cw = w.div_ceil(2);
        let ch = h.div_ceil(2);

        for row in 0..h {
            let start = row * self.y_stride as usize;
            let end = (start + w).min(self.y_plane.len());
            if start < end {
                hasher.update(&self.y_plane[start..end]);
            }
        }
        match self.pixel_format {
            PixelFormat::Yuv420p => {
                for row in 0..ch {
                    let start = row * self.u_stride as usize;
                    let end = (start + cw).min(self.u_plane.len());
                    if start < end {
                        hasher.update(&self.u_plane[start..end]);
                    }
                }
                for row in 0..ch {
                    let start = row * self.v_stride as usize;
                    let end = (start + cw).min(self.v_plane.len());
                    if start < end {
                        hasher.update(&self.v_plane[start..end]);
                    }
                }
            }
            PixelFormat::Nv12 => {
                let row_bytes = cw * 2;
                for row in 0..ch {
                    let start = row * self.u_stride as usize;
                    let end = (start + row_bytes).min(self.u_plane.len());
                    if start < end {
                        hasher.update(&self.u_plane[start..end]);
                    }
                }
            }
        }

        let digest = hasher.finalize();
        let mut out = String::with_capacity(64);
        for byte in digest {
            use std::fmt::Write;
            let _ = write!(&mut out, "{byte:02x}");
        }
        out
    }

    /// Computes mean absolute error on the Y luma plane against another frame of the same size.
    pub fn luma_mae(&self, other: &Self) -> f64 {
        let w = self.width.min(other.width) as usize;
        let h = self.height.min(other.height) as usize;
        if w == 0 || h == 0 {
            return 0.0;
        }
        let mut total: u64 = 0;
        for row in 0..h {
            let a_off = row * self.y_stride as usize;
            let b_off = row * other.y_stride as usize;
            for col in 0..w {
                let a = self.y_plane.get(a_off + col).copied().unwrap_or(0);
                let b = other.y_plane.get(b_off + col).copied().unwrap_or(0);
                total += u64::from(a.abs_diff(b));
            }
        }
        total as f64 / (w * h) as f64
    }
}

/// Decoded interleaved floating-point PCM audio buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioFrame {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved `f32` samples in `[-1.0, 1.0]`.
    pub samples: Vec<f32>,
    pub pts_us: i64,
    pub duration_us: u64,
}

impl AudioFrame {
    /// Returns the number of multi-channel sample frames in this buffer.
    pub fn frame_count(&self) -> usize {
        let ch = usize::from(self.channels.max(1));
        self.samples.len() / ch
    }
}

/// Result of demuxing a container buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemuxedMedia {
    pub container: ContainerFormat,
    pub duration_us: Option<u64>,
    pub tracks: Vec<TrackInfo>,
    pub packets: Vec<EncodedPacket>,
}

/// Errors produced by `rix-media` demuxers, decoders, and MSE components.
#[derive(Debug, Error)]
pub enum MediaError {
    #[error("unsupported container or unrecognized header")]
    UnsupportedContainer,
    #[error("unsupported codec: {0}")]
    UnsupportedCodec(String),
    #[error("malformed container data: {0}")]
    MalformedContainer(String),
    #[error("audio decode error: {0}")]
    AudioDecode(String),
    #[error("video decode error: {0}")]
    VideoDecode(String),
    #[error("invalid media configuration: {0}")]
    InvalidConfig(String),
    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),
}
