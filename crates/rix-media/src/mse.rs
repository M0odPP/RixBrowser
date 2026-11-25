//! W3C Media Source Extensions (MSE) core engine (`MediaSourceEngine`, `SourceBufferEngine`, `TimeRanges`).
//!
//! Pure-Rust, zero site-specific hardcodes, supporting ISO BMFF fMP4 and WebM/Matroska segments,
//! coalesced buffered range merging, coded-frame quota eviction, and seamless `changeType` codec switching.

use std::collections::HashMap;

use crate::demuxer::mp4::{
    inspect_mp4_buffer, parse_mp4_init_segment, parse_mp4_mse_segments, Mp4BufferStatus,
    Mp4InitState,
};
use crate::demuxer::webm::{
    inspect_webm_buffer, parse_webm_init_segment, parse_webm_mse_segments, WebmBufferStatus,
    WebmInitState,
};
use crate::types::{
    AudioCodec, AudioTrackConfig, ContainerFormat, EncodedPacket, MseSample, PacketKind, TrackInfo,
    TrackKind, VideoCodec,
};

/// Default per-`SourceBuffer` coded-frame quota for video buffers (64 MiB).
pub const DEFAULT_SOURCE_BUFFER_QUOTA_BYTES: usize = 64 * 1024 * 1024;
/// Default per-`SourceBuffer` coded-frame quota for audio-only buffers (4 MiB).
pub const DEFAULT_AUDIO_SOURCE_BUFFER_QUOTA_BYTES: usize = 4 * 1024 * 1024;

/// Tolerance (in seconds) when coalescing adjacent `TimeRanges` intervals (matches Chromium/Gecko).
const RANGE_MERGE_EPSILON_S: f64 = 0.002;

/// Normalized W3C `TimeRanges` holding sorted, disjoint, coalesced `[start, end]` intervals in seconds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimeRanges {
    ranges: Vec<(f64, f64)>,
}

impl TimeRanges {
    /// Creates an empty `TimeRanges`.
    pub fn new() -> Self {
        Self { ranges: Vec::new() }
    }

    /// Returns the number of disjoint time intervals.
    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    /// Returns true if there are no buffered intervals.
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Returns the start time (in seconds) of interval `index`, or `IndexSizeError` if out of bounds.
    pub fn start(&self, index: usize) -> Result<f64, MseError> {
        self.ranges
            .get(index)
            .map(|r| r.0)
            .ok_or_else(|| MseError::IndexSize(format!("TimeRanges index {index} out of bounds")))
    }

    /// Returns the end time (in seconds) of interval `index`, or `IndexSizeError` if out of bounds.
    pub fn end(&self, index: usize) -> Result<f64, MseError> {
        self.ranges
            .get(index)
            .map(|r| r.1)
            .ok_or_else(|| MseError::IndexSize(format!("TimeRanges index {index} out of bounds")))
    }

    /// Returns the raw sorted slices of `(start, end)` intervals.
    pub fn as_slice(&self) -> &[(f64, f64)] {
        &self.ranges
    }

    /// Inserts `[start, end]` and merges any overlapping or adjacent ranges (within epsilon).
    pub fn add_range(&mut self, start: f64, end: f64) {
        if !start.is_finite() || !end.is_finite() || end <= start {
            return;
        }
        self.ranges.push((start, end));
        self.coalesce();
    }

    /// Removes `[start, end)` from the interval set, splitting intervals when necessary.
    pub fn remove_range(&mut self, start: f64, end: f64) {
        if !start.is_finite() || !end.is_finite() || end <= start {
            return;
        }
        let mut next = Vec::with_capacity(self.ranges.len() + 1);
        for &(a, b) in &self.ranges {
            if b <= start || a >= end {
                next.push((a, b));
            } else {
                if a < start {
                    next.push((a, start));
                }
                if b > end {
                    next.push((end, b));
                }
            }
        }
        self.ranges = next;
    }

    /// Computes the intersection of two `TimeRanges` (used by `MediaSource.buffered` when open).
    pub fn intersect(&self, other: &Self) -> Self {
        let mut out = Self::new();
        for &(a0, a1) in &self.ranges {
            for &(b0, b1) in &other.ranges {
                let s = a0.max(b0);
                let e = a1.min(b1);
                if e > s {
                    out.add_range(s, e);
                }
            }
        }
        out
    }

    /// Extends the final range to `target_end` if non-empty (W3C MSE ended-state intersection rule).
    pub fn extend_last_to(&mut self, target_end: f64) {
        if let Some(last) = self.ranges.last_mut() {
            if target_end > last.1 {
                last.1 = target_end;
            }
        }
    }

    /// Returns the highest end timestamp across all ranges, or `0.0` if empty.
    pub fn highest_end(&self) -> f64 {
        self.ranges.last().map(|r| r.1).unwrap_or(0.0)
    }

    fn coalesce(&mut self) {
        self.ranges
            .sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));
        let mut merged: Vec<(f64, f64)> = Vec::with_capacity(self.ranges.len());
        for (s, e) in self.ranges.drain(..) {
            if let Some(last) = merged.last_mut() {
                if s <= last.1 + RANGE_MERGE_EPSILON_S {
                    if e > last.1 {
                        last.1 = e;
                    }
                    continue;
                }
            }
            merged.push((s, e));
        }
        self.ranges = merged;
    }
}

/// W3C MSE `AppendMode` enum (`"segments"` | `"sequence"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendMode {
    Segments,
    Sequence,
}

impl AppendMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Segments => "segments",
            Self::Sequence => "sequence",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "segments" => Some(Self::Segments),
            "sequence" => Some(Self::Sequence),
            _ => None,
        }
    }
}

/// W3C MSE `ReadyState` enum (`"closed"` | `"open"` | `"ended"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyState {
    Closed,
    Open,
    Ended,
}

impl ReadyState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Open => "open",
            Self::Ended => "ended",
        }
    }
}

/// W3C MSE `EndOfStreamError` enum (`"network"` | `"decode"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndOfStreamError {
    Network,
    Decode,
}

impl EndOfStreamError {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "network" => Some(Self::Network),
            "decode" => Some(Self::Decode),
            _ => None,
        }
    }
}

/// Typed DOMException-mappable errors returned by MSE operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MseError {
    TypeError(String),
    NotSupported(String),
    InvalidState(String),
    InvalidAccess(String),
    QuotaExceeded(String),
    IndexSize(String),
    DecodeError(String),
}

impl MseError {
    /// Returns the W3C WebIDL / DOMException error name.
    pub fn dom_name(&self) -> &'static str {
        match self {
            Self::TypeError(_) => "TypeError",
            Self::NotSupported(_) => "NotSupportedError",
            Self::InvalidState(_) => "InvalidStateError",
            Self::InvalidAccess(_) => "InvalidAccessError",
            Self::QuotaExceeded(_) => "QuotaExceededError",
            Self::IndexSize(_) => "IndexSizeError",
            Self::DecodeError(_) => "DataError",
        }
    }

    /// Returns the diagnostic error message.
    pub fn message(&self) -> &str {
        match self {
            Self::TypeError(m)
            | Self::NotSupported(m)
            | Self::InvalidState(m)
            | Self::InvalidAccess(m)
            | Self::QuotaExceeded(m)
            | Self::IndexSize(m)
            | Self::DecodeError(m) => m,
        }
    }
}

/// Byte stream format category supported by W3C MSE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MseByteStreamKind {
    Mp4,
    WebM,
    MpegAudio,
    AdtsAac,
}

/// Active asynchronous operation on a `SourceBuffer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PendingBufferOp {
    #[default]
    None,
    Append,
    Remove,
}

/// Parsed MSE MIME container + codec descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMimeType {
    pub essence: String,
    pub container: ContainerFormat,
    pub byte_stream_kind: MseByteStreamKind,
    pub generate_timestamps_flag: bool,
    pub is_video_mime: bool,
    pub video_codecs: Vec<VideoCodec>,
    pub audio_codecs: Vec<AudioCodec>,
}

/// Parses and validates an MSE MIME type string according to W3C MSE rules.
/// When `require_codecs_for_containers` is `true` (used by `MediaSource.isTypeSupported`),
/// MP4 and WebM MIME types must include a non-empty `codecs` parameter.
pub fn parse_supported_mime_type_with_mode(
    mime: &str,
    require_codecs_for_containers: bool,
) -> Option<ParsedMimeType> {
    let trimmed = mime.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut parts = trimmed.split(';');
    let essence = parts.next()?.trim().to_ascii_lowercase();
    let (container, byte_stream_kind, generate_timestamps_flag, is_video_mime) =
        match essence.as_str() {
            "video/mp4" => (ContainerFormat::Mp4, MseByteStreamKind::Mp4, false, true),
            "audio/mp4" => (ContainerFormat::Mp4, MseByteStreamKind::Mp4, false, false),
            "video/webm" => (ContainerFormat::WebM, MseByteStreamKind::WebM, false, true),
            "audio/webm" => (ContainerFormat::WebM, MseByteStreamKind::WebM, false, false),
            "audio/mpeg" => (
                ContainerFormat::Mp3,
                MseByteStreamKind::MpegAudio,
                true,
                false,
            ),
            "audio/aac" => (
                ContainerFormat::Mp3,
                MseByteStreamKind::AdtsAac,
                true,
                false,
            ),
            _ => return None,
        };

    let mut codecs_param: Option<String> = None;
    for raw_param in parts {
        let p = raw_param.trim();
        if p.is_empty() {
            continue;
        }
        let (k, v) = p.split_once('=')?;
        let key = k.trim().to_ascii_lowercase();
        let raw_v = v.trim();
        // Reject unmatched opening/closing quotes (e.g. `codecs="`).
        let val = if let Some(rest) = raw_v.strip_prefix('"') {
            rest.strip_suffix('"')?.trim()
        } else if let Some(rest) = raw_v.strip_prefix('\'') {
            rest.strip_suffix('\'')?.trim()
        } else {
            raw_v
        };
        if key == "codecs" {
            if generate_timestamps_flag || codecs_param.is_some() || val.is_empty() {
                return None;
            }
            codecs_param = Some(val.to_string());
        }
        // Per WHATWG MIME Sniffing / RFC 2045, unrecognized optional parameters are ignored.
    }

    if require_codecs_for_containers && !generate_timestamps_flag && codecs_param.is_none() {
        return None;
    }

    let mut video_codecs = Vec::new();
    let mut audio_codecs = Vec::new();

    if let Some(codecs_str) = codecs_param {
        for raw_codec in codecs_str.split(',') {
            let c = raw_codec.trim();
            if c.is_empty() {
                return None;
            }
            if let Some(vc) = VideoCodec::from_codec_str(c) {
                if !is_video_mime {
                    return None;
                }
                match (container, vc) {
                    (ContainerFormat::Mp4, VideoCodec::Vp8) => return None,
                    (ContainerFormat::WebM, VideoCodec::H264) => return None,
                    _ => {}
                }
                video_codecs.push(vc);
            } else {
                let ac = AudioCodec::from_codec_str(c)?;
                match (container, ac) {
                    (ContainerFormat::Mp4, AudioCodec::Vorbis) => return None,
                    (ContainerFormat::WebM, AudioCodec::Aac | AudioCodec::Mp3) => return None,
                    _ => {}
                }
                audio_codecs.push(ac);
            }
        }
        if video_codecs.is_empty() && audio_codecs.is_empty() {
            return None;
        }
    }

    Some(ParsedMimeType {
        essence,
        container,
        byte_stream_kind,
        generate_timestamps_flag,
        is_video_mime,
        video_codecs,
        audio_codecs,
    })
}

/// Parses and validates an MSE MIME type string allowing relaxed container MIME types without `codecs=`.
pub fn parse_supported_mime_type(mime: &str) -> Option<ParsedMimeType> {
    parse_supported_mime_type_with_mode(mime, false)
}

/// W3C `MediaSource.isTypeSupported(type)` predicate.
pub fn is_type_supported(mime: &str) -> bool {
    parse_supported_mime_type_with_mode(mime, true).is_some()
}

/// A single demuxed coded frame stored in a [`SourceBufferEngine`].
#[derive(Debug, Clone)]
pub struct CodedFrame {
    pub track_id: u32,
    pub kind: PacketKind,
    /// Adjusted presentation timestamp in seconds.
    pub pts_s: f64,
    /// Adjusted decode timestamp in seconds.
    pub dts_s: f64,
    /// Frame duration in seconds.
    pub duration_s: f64,
    pub is_keyframe: bool,
    pub data: Vec<u8>,
    /// Incremented whenever `changeType()` switches codec or a new init segment updates tracks.
    pub codec_generation: u64,
    /// Monotonic append-call batch ID so B-frames in the same append never evict each other.
    pub append_batch_id: u64,
    /// Decoded track video dimensions `(width, height)` when this frame came from a video track.
    pub video_dims: Option<(u32, u32)>,
}

impl CodedFrame {
    pub fn end_s(&self) -> f64 {
        self.pts_s + self.duration_s.max(0.0)
    }

    /// Converts back to an [`EncodedPacket`] (microseconds) for pure-Rust video/audio decoders.
    pub fn to_encoded_packet(&self) -> EncodedPacket {
        EncodedPacket {
            track_id: self.track_id,
            kind: self.kind,
            data: self.data.clone(),
            pts_us: (self.pts_s * 1_000_000.0).round() as i64,
            dts_us: (self.dts_s * 1_000_000.0).round() as i64,
            duration_us: (self.duration_s.max(0.0) * 1_000_000.0).round() as u64,
            is_keyframe: self.is_keyframe,
        }
    }
}

/// Outcome returned by [`SourceBufferEngine::append_buffer`].
#[derive(Debug, Clone, Default)]
pub struct AppendResult {
    pub init_segment_received: bool,
    pub init_duration_s: Option<f64>,
    pub frames_appended: usize,
    pub frames_evicted: usize,
    pub highest_end_timestamp: f64,
}

/// Pure-Rust MSE `SourceBuffer` engine storing coded frames, managing quota eviction,
/// buffered `TimeRanges` merging, and seamless `changeType` codec switching.
#[derive(Debug, Clone)]
pub struct SourceBufferEngine {
    pub mime_type: String,
    pub container: ContainerFormat,
    pub byte_stream_kind: MseByteStreamKind,
    pub generate_timestamps_flag: bool,
    pub mode: AppendMode,
    pub updating: bool,
    pub pending_op: PendingBufferOp,
    pub append_state_parsing_segment: bool,
    pub init_segment_received: bool,
    pub timestamp_offset: f64,
    pub append_window_start: f64,
    pub append_window_end: f64,
    pub quota_bytes: usize,
    pub buffered_bytes: usize,
    /// W3C MSE §3.5.4 `buffer full flag`: set when coded frames reach quota and eviction cannot
    /// free space behind `currentTime`, causing the next `appendBuffer()` to throw `QuotaExceededError`.
    pub buffer_full_flag: bool,
    pub buffered: TimeRanges,
    pub tracks: Vec<TrackInfo>,
    pub frames: Vec<CodedFrame>,
    pub codec_generation: u64,
    pub highest_end_timestamp: f64,
    /// Track count signature `(video_count, audio_count)` established by the first init segment
    /// (enforces W3C MSE §3.5.7 step 5: subsequent init segments cannot change track types/counts).
    first_init_track_signature: Option<(usize, usize)>,
    next_append_batch_id: u64,
    group_start_timestamp: Option<f64>,
    group_end_timestamp: f64,
    last_decode_timestamp: HashMap<u32, f64>,
    mpeg_pts_cursor_s: f64,
    mp4_state: Option<Mp4InitState>,
    webm_state: Option<WebmInitState>,
    pending_bytes: Vec<u8>,
}

impl SourceBufferEngine {
    /// Creates a new `SourceBufferEngine` for a validated MIME type.
    pub fn new(mime: &str) -> Result<Self, MseError> {
        let parsed = parse_supported_mime_type(mime).ok_or_else(|| {
            MseError::NotSupported(format!("unsupported SourceBuffer MIME type: `{mime}`"))
        })?;
        let initial_mode = if parsed.generate_timestamps_flag {
            AppendMode::Sequence
        } else {
            AppendMode::Segments
        };
        Ok(Self {
            mime_type: mime.to_string(),
            container: parsed.container,
            byte_stream_kind: parsed.byte_stream_kind,
            generate_timestamps_flag: parsed.generate_timestamps_flag,
            mode: initial_mode,
            updating: false,
            pending_op: PendingBufferOp::None,
            append_state_parsing_segment: false,
            init_segment_received: false,
            timestamp_offset: 0.0,
            append_window_start: 0.0,
            append_window_end: f64::INFINITY,
            quota_bytes: if parsed.is_video_mime {
                DEFAULT_SOURCE_BUFFER_QUOTA_BYTES
            } else {
                DEFAULT_AUDIO_SOURCE_BUFFER_QUOTA_BYTES
            },
            buffered_bytes: 0,
            buffer_full_flag: false,
            buffered: TimeRanges::new(),
            tracks: Vec::new(),
            frames: Vec::new(),
            codec_generation: 1,
            highest_end_timestamp: 0.0,
            first_init_track_signature: None,
            next_append_batch_id: 1,
            group_start_timestamp: if parsed.generate_timestamps_flag {
                Some(0.0)
            } else {
                None
            },
            group_end_timestamp: 0.0,
            last_decode_timestamp: HashMap::new(),
            mpeg_pts_cursor_s: 0.0,
            mp4_state: None,
            webm_state: None,
            pending_bytes: Vec::new(),
        })
    }

    /// Sets `appendWindowStart`, enforcing W3C MSE range validation.
    pub fn set_append_window_start(&mut self, value: f64) -> Result<(), MseError> {
        if self.updating {
            return Err(MseError::InvalidState(
                "cannot modify appendWindowStart while updating".to_string(),
            ));
        }
        if !value.is_finite() || value < 0.0 || value >= self.append_window_end {
            return Err(MseError::TypeError(
                "appendWindowStart must be finite, >= 0, and < appendWindowEnd".to_string(),
            ));
        }
        self.append_window_start = value;
        Ok(())
    }

    /// Sets `appendWindowEnd`, enforcing W3C MSE range validation.
    pub fn set_append_window_end(&mut self, value: f64) -> Result<(), MseError> {
        if self.updating {
            return Err(MseError::InvalidState(
                "cannot modify appendWindowEnd while updating".to_string(),
            ));
        }
        if value.is_nan() || value <= self.append_window_start {
            return Err(MseError::TypeError(
                "appendWindowEnd must be > appendWindowStart and not NaN".to_string(),
            ));
        }
        self.append_window_end = value;
        Ok(())
    }

    /// Sets `timestampOffset`.
    pub fn set_timestamp_offset(&mut self, value: f64) -> Result<(), MseError> {
        if self.updating || self.append_state_parsing_segment {
            return Err(MseError::InvalidState(
                "cannot modify timestampOffset while updating or parsing a media segment"
                    .to_string(),
            ));
        }
        if !value.is_finite() {
            return Err(MseError::TypeError(
                "timestampOffset must be finite".to_string(),
            ));
        }
        self.timestamp_offset = value;
        if self.mode == AppendMode::Sequence {
            self.group_start_timestamp = Some(value);
        }
        Ok(())
    }

    /// Sets `mode` (`"segments"` | `"sequence"`).
    pub fn set_mode(&mut self, mode: AppendMode) -> Result<(), MseError> {
        if self.updating || self.append_state_parsing_segment {
            return Err(MseError::InvalidState(
                "cannot modify mode while updating or parsing a media segment".to_string(),
            ));
        }
        if self.generate_timestamps_flag && mode == AppendMode::Segments {
            return Err(MseError::TypeError(
                "cannot set mode to 'segments' when generate timestamps flag is true".to_string(),
            ));
        }
        self.mode = mode;
        if mode == AppendMode::Sequence {
            self.group_start_timestamp = Some(self.group_end_timestamp);
        }
        Ok(())
    }

    /// Aborts any active segment parser state and resets append window bounds per W3C MSE §3.2.
    /// Returns `Err(InvalidState)` if a `remove()` operation is currently in progress.
    pub fn abort_spec(&mut self) -> Result<bool, MseError> {
        if self.updating && self.pending_op == PendingBufferOp::Remove {
            return Err(MseError::InvalidState(
                "cannot call abort() while a remove() operation is in progress".to_string(),
            ));
        }
        let was_updating = self.updating;
        self.abort();
        Ok(was_updating)
    }

    /// Aborts any active segment parser state and resets append window bounds per W3C MSE §3.2.
    pub fn abort(&mut self) {
        self.updating = false;
        self.pending_op = PendingBufferOp::None;
        self.append_state_parsing_segment = false;
        self.pending_bytes.clear();
        self.append_window_start = 0.0;
        self.append_window_end = f64::INFINITY;
        self.group_start_timestamp = if self.mode == AppendMode::Sequence {
            Some(self.group_end_timestamp)
        } else {
            None
        };
    }

    /// Seamlessly switches the `SourceBuffer`'s container/codec type without discarding
    /// already-buffered frames (`codec_generation` increments for downstream decoder reinit).
    pub fn change_type(&mut self, new_mime: &str) -> Result<(), MseError> {
        if self.updating {
            return Err(MseError::InvalidState(
                "cannot call changeType while SourceBuffer is updating".to_string(),
            ));
        }
        let parsed = parse_supported_mime_type(new_mime).ok_or_else(|| {
            MseError::NotSupported(format!("unsupported changeType MIME: `{new_mime}`"))
        })?;
        self.mime_type = new_mime.to_string();
        self.container = parsed.container;
        self.byte_stream_kind = parsed.byte_stream_kind;
        self.generate_timestamps_flag = parsed.generate_timestamps_flag;
        if parsed.generate_timestamps_flag {
            self.mode = AppendMode::Sequence;
        }
        self.codec_generation = self.codec_generation.saturating_add(1);
        self.mp4_state = None;
        self.webm_state = None;
        self.pending_bytes.clear();
        self.append_state_parsing_segment = false;
        if self.mode == AppendMode::Sequence {
            self.group_start_timestamp = Some(self.group_end_timestamp);
        }
        Ok(())
    }

    /// Runs W3C MSE §3.5.4 Prepare Append quota eviction synchronously before `appendBuffer`.
    /// Evicts only coded frames strictly behind `current_playback_time_s`, clears `buffer_full_flag`
    /// if space drops below quota, and returns `Err(MseError::QuotaExceeded)` if `incoming_bytes > quota_bytes`
    /// or `buffer_full_flag` remains set.
    pub fn prepare_append_quota(
        &mut self,
        incoming_bytes: usize,
        current_playback_time_s: f64,
    ) -> Result<usize, MseError> {
        if incoming_bytes > self.quota_bytes {
            return Err(MseError::QuotaExceeded(format!(
                "append of {incoming_bytes} bytes exceeds SourceBuffer quota of {} bytes",
                self.quota_bytes
            )));
        }
        let mut evicted = 0usize;
        if self.buffer_full_flag || self.buffered_bytes >= self.quota_bytes {
            let needed = self
                .buffered_bytes
                .saturating_add(incoming_bytes)
                .saturating_sub(self.quota_bytes);
            evicted = self.evict_coded_frames(needed.max(1), current_playback_time_s);
            if self.buffered_bytes < self.quota_bytes {
                self.buffer_full_flag = false;
            }
        }
        if self.buffer_full_flag || self.buffered_bytes >= self.quota_bytes {
            return Err(MseError::QuotaExceeded(format!(
                "SourceBuffer quota ({} bytes) exceeded",
                self.quota_bytes
            )));
        }
        Ok(evicted)
    }

    /// Appends raw fMP4 or WebM initialization/media segment bytes, performing automatic
    /// coded-frame quota eviction relative to `current_playback_time_s` and updating `buffered`.
    pub fn append_buffer(
        &mut self,
        bytes: &[u8],
        current_playback_time_s: f64,
    ) -> Result<AppendResult, MseError> {
        if bytes.is_empty() {
            return Ok(AppendResult {
                highest_end_timestamp: self.highest_end_timestamp,
                ..Default::default()
            });
        }

        let frames_evicted = self.prepare_append_quota(bytes.len(), current_playback_time_s)?;

        self.pending_bytes.extend_from_slice(bytes);
        let input = std::mem::take(&mut self.pending_bytes);

        let mut result = AppendResult {
            frames_evicted,
            highest_end_timestamp: self.highest_end_timestamp,
            ..Default::default()
        };

        let mut samples = match self.byte_stream_kind {
            MseByteStreamKind::Mp4 => self.parse_mp4_chunk(&input, &mut result)?,
            MseByteStreamKind::WebM => self.parse_webm_chunk(&input, &mut result)?,
            MseByteStreamKind::MpegAudio => self.parse_mpeg_audio_chunk(&input, &mut result)?,
            MseByteStreamKind::AdtsAac => self.parse_adts_aac_chunk(&input, &mut result)?,
        };

        // Process decoded elementary samples through W3C MSE Coded Frame Processing Algorithm (§3.5.8).
        if self.mode == AppendMode::Sequence && !samples.is_empty() {
            samples.sort_by(|a, b| {
                a.dts_s
                    .total_cmp(&b.dts_s)
                    .then_with(|| a.pts_s.total_cmp(&b.pts_s))
            });
            if self.group_start_timestamp.is_none() {
                if let Some(first) = samples.first() {
                    let cand_dts = first.dts_s + self.timestamp_offset;
                    if let Some(&last_dts) = self.last_decode_timestamp.get(&first.track_id) {
                        let gap_limit = (first.duration_s * 2.0).max(0.1);
                        if cand_dts < last_dts - 1e-4 || cand_dts > last_dts + gap_limit {
                            self.group_start_timestamp = Some(self.group_end_timestamp);
                        }
                    }
                }
            }
            if let Some(group_start) = self.group_start_timestamp.take() {
                let earliest_raw_pts = samples
                    .iter()
                    .map(|s| s.pts_s)
                    .fold(f64::INFINITY, f64::min);
                if earliest_raw_pts.is_finite() {
                    self.timestamp_offset = group_start - earliest_raw_pts;
                }
            }
        }

        let batch_id = self.next_append_batch_id;
        self.next_append_batch_id = self.next_append_batch_id.wrapping_add(1);
        let mut needs_sort = false;
        let mut last_pushed_dts = self
            .frames
            .last()
            .map(|f| f.dts_s)
            .unwrap_or(f64::NEG_INFINITY);
        let mut max_existing_end_by_track: HashMap<u32, f64> = HashMap::new();
        for f in &self.frames {
            let e = max_existing_end_by_track
                .entry(f.track_id)
                .or_insert(f64::NEG_INFINITY);
            let fe = f.end_s();
            if fe > *e {
                *e = fe;
            }
        }

        for pkt in samples {
            let raw_pts_s = pkt.pts_s;
            let raw_dts_s = pkt.dts_s;
            let dur_s = pkt.duration_s.max(0.0);

            let pts_s = raw_pts_s + self.timestamp_offset;
            let dts_s = raw_dts_s + self.timestamp_offset;
            let frame_end = pts_s + dur_s;

            // Append window filtering (§3.5.8 step 1.11–1.12).
            if pts_s < self.append_window_start - 1e-9
                || pts_s >= self.append_window_end - 1e-12
                || frame_end > self.append_window_end + 1e-9
            {
                continue;
            }

            // W3C MSE §3.5.8 step 1.15: replace overlapping frames ONLY from prior append batches
            // (never evict sibling B-frames from the same segment!).
            let overlap_eps = (dur_s * 0.25).min(0.000_5);
            let track_max_end = max_existing_end_by_track
                .get(&pkt.track_id)
                .copied()
                .unwrap_or(f64::NEG_INFINITY);
            if pts_s < track_max_end - overlap_eps {
                self.frames.retain(|f| {
                    if f.track_id != pkt.track_id || f.append_batch_id == batch_id {
                        return true;
                    }
                    let overlaps =
                        f.pts_s >= pts_s - overlap_eps && f.pts_s < frame_end - overlap_eps;
                    if overlaps {
                        self.buffered_bytes = self.buffered_bytes.saturating_sub(f.data.len());
                        false
                    } else {
                        true
                    }
                });
            }
            if dts_s < last_pushed_dts {
                needs_sort = true;
            } else {
                last_pushed_dts = dts_s;
            }

            self.buffered_bytes = self.buffered_bytes.saturating_add(pkt.data.len());
            self.frames.push(CodedFrame {
                track_id: pkt.track_id,
                kind: pkt.kind,
                pts_s,
                dts_s,
                duration_s: dur_s,
                is_keyframe: pkt.is_keyframe,
                data: pkt.data,
                codec_generation: self.codec_generation,
                append_batch_id: batch_id,
                video_dims: pkt.video_dims,
            });
            self.last_decode_timestamp.insert(pkt.track_id, dts_s);
            result.frames_appended += 1;
            if frame_end > self.group_end_timestamp {
                self.group_end_timestamp = frame_end;
            }
            if frame_end > self.highest_end_timestamp {
                self.highest_end_timestamp = frame_end;
            }
        }

        // Enforce quota after inserting coded frames per W3C MSE §3.5.8 step 1.18:
        // attempt coded-frame eviction behind playback head; if still >= quota, set `buffer_full_flag`.
        if self.buffered_bytes > self.quota_bytes {
            let excess = self.buffered_bytes - self.quota_bytes;
            result.frames_evicted += self.evict_coded_frames(excess, current_playback_time_s);
            if self.buffered_bytes >= self.quota_bytes {
                self.buffer_full_flag = true;
            }
        }

        if needs_sort {
            self.frames.sort_by(|a, b| {
                a.dts_s
                    .total_cmp(&b.dts_s)
                    .then_with(|| a.pts_s.total_cmp(&b.pts_s))
            });
        }
        self.recompute_buffered_ranges();
        result.highest_end_timestamp = self.highest_end_timestamp;
        Ok(result)
    }

    fn validate_and_accept_init_tracks(
        &mut self,
        new_tracks: Vec<TrackInfo>,
    ) -> Result<(), MseError> {
        let mut video_cnt = 0usize;
        let mut audio_cnt = 0usize;
        for t in &new_tracks {
            match t.kind {
                TrackKind::Video(_) => video_cnt += 1,
                TrackKind::Audio(_) => audio_cnt += 1,
            }
        }
        let sig = (video_cnt, audio_cnt);
        if let Some(prev_sig) = self.first_init_track_signature {
            if prev_sig != sig {
                return Err(MseError::DecodeError(
                    "initialization segment track count or type does not match first init segment"
                        .to_string(),
                ));
            }
        } else {
            self.first_init_track_signature = Some(sig);
        }
        self.tracks = new_tracks;
        Ok(())
    }

    fn parse_mp4_chunk(
        &mut self,
        input: &[u8],
        result: &mut AppendResult,
    ) -> Result<Vec<MseSample>, MseError> {
        match inspect_mp4_buffer(input) {
            Mp4BufferStatus::Corrupt => {
                self.append_state_parsing_segment = false;
                Err(MseError::DecodeError(
                    "appended bytes are not a valid ISO BMFF MP4 segment".to_string(),
                ))
            }
            Mp4BufferStatus::Partial { in_media_segment } => {
                if in_media_segment && self.mp4_state.is_none() {
                    self.append_state_parsing_segment = false;
                    return Err(MseError::DecodeError(
                        "fMP4 media segment received before initialization segment".to_string(),
                    ));
                }
                self.append_state_parsing_segment = in_media_segment;
                self.pending_bytes = input.to_vec();
                Ok(Vec::new())
            }
            Mp4BufferStatus::CompletePrefix {
                split_at,
                has_moov,
                has_moof,
                trailing_in_media,
            } => {
                let prefix = &input[..split_at];
                let trailing = &input[split_at..];
                if has_moov {
                    let new_state = parse_mp4_init_segment(prefix)
                        .map_err(|e| MseError::DecodeError(e.to_string()))?;
                    if new_state.tracks.is_empty() {
                        return Err(MseError::DecodeError(
                            "MP4 init segment contains no recognized tracks".to_string(),
                        ));
                    }
                    self.validate_and_accept_init_tracks(new_state.tracks())?;
                    result.init_duration_s = new_state.init_duration_s;
                    self.mp4_state = Some(new_state);
                    self.init_segment_received = true;
                    result.init_segment_received = true;
                }
                let samples = if has_moof {
                    let Some(state) = self.mp4_state.as_mut() else {
                        return Err(MseError::DecodeError(
                            "fMP4 media segment received before initialization segment".to_string(),
                        ));
                    };
                    parse_mp4_mse_segments(prefix, state)
                        .map_err(|e| MseError::DecodeError(e.to_string()))?
                } else {
                    Vec::new()
                };
                if trailing_in_media && self.mp4_state.is_none() {
                    self.append_state_parsing_segment = false;
                    return Err(MseError::DecodeError(
                        "fMP4 media segment received before initialization segment".to_string(),
                    ));
                }
                self.append_state_parsing_segment = trailing_in_media;
                self.pending_bytes = trailing.to_vec();
                Ok(samples)
            }
            Mp4BufferStatus::Complete { has_moov, has_moof } => {
                self.append_state_parsing_segment = false;
                if !has_moov && !has_moof {
                    self.pending_bytes = input.to_vec();
                    return Ok(Vec::new());
                }
                if has_moov {
                    let new_state = parse_mp4_init_segment(input)
                        .map_err(|e| MseError::DecodeError(e.to_string()))?;
                    if new_state.tracks.is_empty() {
                        return Err(MseError::DecodeError(
                            "MP4 init segment contains no recognized tracks".to_string(),
                        ));
                    }
                    self.validate_and_accept_init_tracks(new_state.tracks())?;
                    result.init_duration_s = new_state.init_duration_s;
                    self.mp4_state = Some(new_state);
                    self.init_segment_received = true;
                    result.init_segment_received = true;
                    if !has_moof {
                        return Ok(Vec::new());
                    }
                }
                let Some(state) = self.mp4_state.as_mut() else {
                    return Err(MseError::DecodeError(
                        "fMP4 media segment received before initialization segment".to_string(),
                    ));
                };
                parse_mp4_mse_segments(input, state)
                    .map_err(|e| MseError::DecodeError(e.to_string()))
            }
        }
    }

    fn parse_webm_chunk(
        &mut self,
        input: &[u8],
        result: &mut AppendResult,
    ) -> Result<Vec<MseSample>, MseError> {
        match inspect_webm_buffer(input) {
            WebmBufferStatus::Corrupt => {
                self.append_state_parsing_segment = false;
                Err(MseError::DecodeError(
                    "appended bytes are not a valid WebM segment".to_string(),
                ))
            }
            WebmBufferStatus::Partial { in_media_segment } => {
                if in_media_segment && self.webm_state.is_none() {
                    self.append_state_parsing_segment = false;
                    return Err(MseError::DecodeError(
                        "WebM cluster received before EBML initialization segment".to_string(),
                    ));
                }
                self.append_state_parsing_segment = in_media_segment;
                self.pending_bytes = input.to_vec();
                Ok(Vec::new())
            }
            WebmBufferStatus::Complete {
                has_init,
                has_cluster,
            } => {
                self.append_state_parsing_segment = false;
                let is_ebml_init =
                    has_init || (input.len() >= 4 && input[0..4] == [0x1a, 0x45, 0xdf, 0xa3]);
                if is_ebml_init {
                    let mut state = parse_webm_init_segment(input)
                        .map_err(|e| MseError::DecodeError(e.to_string()))?;
                    self.validate_and_accept_init_tracks(state.tracks())?;
                    result.init_duration_s = state.init_duration_s;
                    self.init_segment_received = true;
                    result.init_segment_received = true;
                    let samples = if has_cluster {
                        parse_webm_mse_segments(input, &mut state)
                            .map_err(|e| MseError::DecodeError(e.to_string()))?
                    } else {
                        Vec::new()
                    };
                    self.webm_state = Some(state);
                    return Ok(samples);
                }
                let Some(state) = self.webm_state.as_mut() else {
                    return Err(MseError::DecodeError(
                        "WebM cluster received before EBML initialization segment".to_string(),
                    ));
                };
                parse_webm_mse_segments(input, state)
                    .map_err(|e| MseError::DecodeError(e.to_string()))
            }
        }
    }

    fn parse_mpeg_audio_chunk(
        &mut self,
        input: &[u8],
        result: &mut AppendResult,
    ) -> Result<Vec<MseSample>, MseError> {
        let mut off = 0usize;
        let mut samples = Vec::new();
        self.append_state_parsing_segment = false;

        while off < input.len() {
            // Skip ID3v2 tags anywhere in the stream (partial ID3v2 tag stays WAITING_FOR_SEGMENT).
            if off + 3 <= input.len() && &input[off..off + 3] == b"ID3" {
                if off + 10 > input.len() {
                    self.pending_bytes = input[off..].to_vec();
                    break;
                }
                let sz = ((usize::from(input[off + 6] & 0x7f)) << 21)
                    | ((usize::from(input[off + 7] & 0x7f)) << 14)
                    | ((usize::from(input[off + 8] & 0x7f)) << 7)
                    | usize::from(input[off + 9] & 0x7f);
                let next = off + 10 + sz;
                if next > input.len() {
                    self.pending_bytes = input[off..].to_vec();
                    break;
                }
                off = next;
                continue;
            }
            // Skip Icecast ICY metadata headers (`ICY ...\r\n\r\n`).
            if off + 4 <= input.len() && &input[off..off + 4] == b"ICY " {
                if let Some(rel) = input[off..].windows(4).position(|w| w == b"\r\n\r\n") {
                    off += rel + 4;
                    continue;
                }
                self.pending_bytes = input[off..].to_vec();
                break;
            }
            // Skip ID3v1 tag (`TAG` + 125 bytes = 128 bytes).
            if off + 3 <= input.len() && &input[off..off + 3] == b"TAG" {
                if off + 128 > input.len() {
                    self.pending_bytes = input[off..].to_vec();
                    break;
                }
                off += 128;
                continue;
            }
            if off + 4 > input.len() {
                self.pending_bytes = input[off..].to_vec();
                self.append_state_parsing_segment = true;
                break;
            }
            if input[off] != 0xFF || (input[off + 1] & 0xE0) != 0xE0 {
                return Err(MseError::DecodeError(
                    "invalid MPEG audio frame sync header".to_string(),
                ));
            }
            let version_bits = (input[off + 1] >> 3) & 0x03;
            let layer_bits = (input[off + 1] >> 1) & 0x03;
            let bitrate_idx = ((input[off + 2] >> 4) & 0x0F) as usize;
            let sr_idx = ((input[off + 2] >> 2) & 0x03) as usize;
            let padding = ((input[off + 2] >> 1) & 0x01) as usize;
            let channel_mode = (input[off + 3] >> 6) & 0x03;

            if version_bits == 1
                || layer_bits == 0
                || bitrate_idx == 0
                || bitrate_idx == 15
                || sr_idx == 3
            {
                return Err(MseError::DecodeError(
                    "invalid MPEG audio header parameters".to_string(),
                ));
            }

            let is_mpeg1 = version_bits == 3;
            let sr_table: [u32; 3] = match version_bits {
                3 => [44_100, 48_000, 32_000],
                2 => [22_050, 24_000, 16_000],
                _ => [11_025, 12_000, 8_000],
            };
            let sample_rate = sr_table[sr_idx];

            let kbps: u32 = match (is_mpeg1, layer_bits) {
                (true, 3) => [
                    0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
                ][bitrate_idx],
                (true, 2) => [
                    0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
                ][bitrate_idx],
                (true, 1) => [
                    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
                ][bitrate_idx],
                (false, 3) => [
                    0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
                ][bitrate_idx],
                _ => [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160][bitrate_idx],
            };
            let samples_per_frame: u32 = match (is_mpeg1, layer_bits) {
                (_, 3) => 384,
                (false, 1) => 576,
                _ => 1152,
            };
            let frame_len = if layer_bits == 3 {
                (((12 * kbps * 1000) / sample_rate) + padding as u32) as usize * 4
            } else {
                let coeff = if !is_mpeg1 && layer_bits == 1 {
                    72
                } else {
                    144
                };
                ((coeff * kbps * 1000) / sample_rate) as usize + padding
            };
            if frame_len < 4 {
                return Err(MseError::DecodeError(
                    "invalid MPEG frame length".to_string(),
                ));
            }
            if off + frame_len > input.len() {
                self.pending_bytes = input[off..].to_vec();
                self.append_state_parsing_segment = true;
                break;
            }

            if !self.init_segment_received || self.first_init_track_signature.is_some() {
                let mp3_track = vec![TrackInfo {
                    id: 1,
                    kind: TrackKind::Audio(AudioTrackConfig {
                        codec: AudioCodec::Mp3,
                        sample_rate,
                        channels: if channel_mode == 3 { 1 } else { 2 },
                        pcm_format: None,
                    }),
                    timescale: sample_rate,
                    duration_us: None,
                    language: None,
                    extra_data: Vec::new(),
                }];
                self.validate_and_accept_init_tracks(mp3_track)?;
                if !self.init_segment_received {
                    self.init_segment_received = true;
                    result.init_segment_received = true;
                }
            }

            let dur_s = f64::from(samples_per_frame) / f64::from(sample_rate);
            let pts_s = self.mpeg_pts_cursor_s;
            self.mpeg_pts_cursor_s += dur_s;
            samples.push(MseSample {
                track_id: 1,
                kind: PacketKind::Audio,
                data: input[off..off + frame_len].to_vec(),
                pts_s,
                dts_s: pts_s,
                duration_s: dur_s,
                is_keyframe: true,
                video_dims: None,
            });
            off += frame_len;
        }
        Ok(samples)
    }

    fn parse_adts_aac_chunk(
        &mut self,
        input: &[u8],
        result: &mut AppendResult,
    ) -> Result<Vec<MseSample>, MseError> {
        const ADTS_SR: [u32; 13] = [
            96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
            8_000, 7_350,
        ];
        let mut off = 0usize;
        let mut samples = Vec::new();
        self.append_state_parsing_segment = false;
        while off + 7 <= input.len() {
            if input[off] != 0xFF || (input[off + 1] & 0xF0) != 0xF0 {
                return Err(MseError::DecodeError(
                    "invalid ADTS AAC syncword".to_string(),
                ));
            }
            let sr_idx = ((input[off + 2] >> 2) & 0x0F) as usize;
            let Some(&sample_rate) = ADTS_SR.get(sr_idx) else {
                return Err(MseError::DecodeError(
                    "invalid ADTS sample rate index".to_string(),
                ));
            };
            let channels =
                (((input[off + 2] & 0x01) << 2) | ((input[off + 3] >> 6) & 0x03)).max(1) as u16;
            let frame_len = ((usize::from(input[off + 3] & 0x03)) << 11)
                | (usize::from(input[off + 4]) << 3)
                | usize::from((input[off + 5] >> 5) & 0x07);
            if frame_len < 7 {
                return Err(MseError::DecodeError(
                    "invalid ADTS frame length".to_string(),
                ));
            }
            if off + frame_len > input.len() {
                self.pending_bytes = input[off..].to_vec();
                self.append_state_parsing_segment = true;
                break;
            }
            let aac_track = vec![TrackInfo {
                id: 1,
                kind: TrackKind::Audio(AudioTrackConfig {
                    codec: AudioCodec::Aac,
                    sample_rate,
                    channels,
                    pcm_format: None,
                }),
                timescale: sample_rate,
                duration_us: None,
                language: None,
                extra_data: Vec::new(),
            }];
            self.validate_and_accept_init_tracks(aac_track)?;
            if !self.init_segment_received {
                self.init_segment_received = true;
                result.init_segment_received = true;
            }
            let dur_s = 1024.0 / f64::from(sample_rate);
            let pts_s = self.mpeg_pts_cursor_s;
            self.mpeg_pts_cursor_s += dur_s;
            samples.push(MseSample {
                track_id: 1,
                kind: PacketKind::Audio,
                data: input[off..off + frame_len].to_vec(),
                pts_s,
                dts_s: pts_s,
                duration_s: dur_s,
                is_keyframe: true,
                video_dims: None,
            });
            off += frame_len;
        }
        Ok(samples)
    }

    /// Removes coded frames overlapping `[start, end)` per W3C MSE Coded Frame Removal Algorithm (§3.5.9).
    pub fn remove(&mut self, start: f64, end: f64) -> Result<(), MseError> {
        if self.updating {
            return Err(MseError::InvalidState(
                "cannot call remove while updating".to_string(),
            ));
        }
        if !start.is_finite() || start < 0.0 || end.is_nan() || end <= start {
            return Err(MseError::TypeError(
                "remove(start, end) requires finite start >= 0 and end > start".to_string(),
            ));
        }

        let mut removed_any = false;
        let mut drop_until_kf: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut kept = Vec::with_capacity(self.frames.len());
        let mut bytes_kept = 0usize;

        for frame in self.frames.drain(..) {
            let in_window = frame.pts_s >= start && frame.pts_s < end;
            if in_window {
                removed_any = true;
                drop_until_kf.insert(frame.track_id);
                continue;
            }
            if drop_until_kf.contains(&frame.track_id) {
                if frame.is_keyframe {
                    drop_until_kf.remove(&frame.track_id);
                } else {
                    // Drop dependent non-keyframes whose reference was removed.
                    continue;
                }
            }
            bytes_kept += frame.data.len();
            kept.push(frame);
        }

        self.frames = kept;
        self.buffered_bytes = bytes_kept;
        if self.buffered_bytes < self.quota_bytes {
            self.buffer_full_flag = false;
        }
        self.highest_end_timestamp = self
            .frames
            .iter()
            .map(CodedFrame::end_s)
            .fold(0.0f64, f64::max);
        if removed_any {
            self.recompute_buffered_ranges();
        }
        Ok(())
    }

    /// Computes W3C MSE §3.3.1 `SourceBuffer.buffered` as the intersection of per-track buffered
    /// ranges (extending each non-empty track's final range to `highest_end` when `ended == true`).
    pub fn buffered_ranges(&self, ended: bool) -> TimeRanges {
        let mut track_ids: Vec<u32> = self.tracks.iter().map(|t| t.id).collect();
        if track_ids.is_empty() {
            for f in &self.frames {
                if !track_ids.contains(&f.track_id) {
                    track_ids.push(f.track_id);
                }
            }
        }
        if track_ids.is_empty() {
            return TimeRanges::new();
        }

        let mut per_track: Vec<TimeRanges> = Vec::with_capacity(track_ids.len());
        let mut highest_end = 0.0f64;
        for tid in &track_ids {
            let mut tr = TimeRanges::new();
            let mut cur_group: Option<(f64, f64, f64)> = None; // (min_pts, max_end, last_dts_end)
            for f in &self.frames {
                if f.track_id != *tid {
                    continue;
                }
                let end = f.end_s();
                let dts_end = f.dts_s + f.duration_s.max(0.0);
                if end > highest_end {
                    highest_end = end;
                }
                // Group contiguous-DTS coded frames into one continuous buffered range per
                // W3C MSE §3.5.8 so out-of-order B-frame composition offsets never create fake gaps,
                // while still preserving real gaps left by SourceBuffer.remove().
                if let Some((ref mut g_start, ref mut g_end, ref mut g_dts_end)) = cur_group {
                    if f.dts_s <= *g_dts_end + RANGE_MERGE_EPSILON_S && f.dts_s >= *g_start - 1.0 {
                        if f.pts_s < *g_start {
                            *g_start = f.pts_s;
                        }
                        if end > *g_end {
                            *g_end = end;
                        }
                        if dts_end > *g_dts_end {
                            *g_dts_end = dts_end;
                        }
                        continue;
                    }
                    tr.add_range(*g_start, *g_end);
                }
                cur_group = Some((f.pts_s, end, dts_end));
            }
            if let Some((g_start, g_end, _)) = cur_group {
                tr.add_range(g_start, g_end);
            }
            if tr.is_empty() {
                return TimeRanges::new();
            }
            per_track.push(tr);
        }

        if ended {
            for tr in &mut per_track {
                tr.extend_last_to(highest_end);
            }
        }

        let mut iter = per_track.into_iter();
        let mut acc = iter.next().unwrap();
        for tr in iter {
            acc = acc.intersect(&tr);
        }
        acc
    }

    /// Highest coded-frame starting presentation timestamp (`pts_s`) across all tracks, if any.
    pub fn highest_coded_frame_pts(&self) -> Option<f64> {
        self.frames.iter().map(|f| f.pts_s).reduce(f64::max)
    }

    /// Highest coded-frame end timestamp across all tracks, or `0.0` if empty.
    pub fn highest_coded_frame_end(&self) -> f64 {
        self.frames
            .iter()
            .map(CodedFrame::end_s)
            .fold(0.0f64, f64::max)
    }

    /// Evicts oldest GOPs strictly behind `current_playback_time_s` per W3C MSE §3.5.12
    /// until at least `bytes_needed` are reclaimed (never evicting frames at/after `currentTime`).
    fn evict_coded_frames(&mut self, bytes_needed: usize, current_playback_time_s: f64) -> usize {
        if bytes_needed == 0 || self.frames.is_empty() || current_playback_time_s <= 0.0 {
            return 0;
        }
        let mut reclaimed = 0usize;
        let mut evicted_count = 0usize;

        // Evict earliest frames strictly before playback head (`current_playback_time_s`).
        let safe_before = current_playback_time_s.max(0.0);
        let mut idx = 0usize;
        while idx < self.frames.len() && reclaimed < bytes_needed {
            if self.frames[idx].end_s() <= safe_before + 1e-6 {
                let f = self.frames.remove(idx);
                reclaimed += f.data.len();
                self.buffered_bytes = self.buffered_bytes.saturating_sub(f.data.len());
                evicted_count += 1;
            } else {
                idx += 1;
            }
        }

        if evicted_count > 0 {
            self.recompute_buffered_ranges();
        }
        evicted_count
    }

    fn recompute_buffered_ranges(&mut self) {
        self.buffered = self.buffered_ranges(false);
    }
}

/// Pure-Rust W3C `MediaSource` state machine managing `SourceBufferEngine`s, `readyState`,
/// duration, live seekable ranges, and combined buffered `TimeRanges`.
#[derive(Debug, Clone)]
pub struct MediaSourceEngine {
    pub ready_state: ReadyState,
    pub duration: f64,
    pub source_buffers: Vec<SourceBufferEngine>,
    pub end_of_stream_error: Option<EndOfStreamError>,
    pub live_seekable_range: Option<(f64, f64)>,
}

impl Default for MediaSourceEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaSourceEngine {
    /// Creates a new `MediaSourceEngine` in the `"closed"` state with `duration = NaN`.
    pub fn new() -> Self {
        Self {
            ready_state: ReadyState::Closed,
            duration: f64::NAN,
            source_buffers: Vec::new(),
            end_of_stream_error: None,
            live_seekable_range: None,
        }
    }

    /// Opens this `MediaSource` (invoked when attached to an `HTMLMediaElement`).
    pub fn open(&mut self) -> bool {
        if self.ready_state == ReadyState::Closed {
            self.ready_state = ReadyState::Open;
            self.end_of_stream_error = None;
            true
        } else {
            false
        }
    }

    /// Closes this `MediaSource`, clearing all `SourceBuffer`s and resetting `duration = NaN`.
    pub fn close(&mut self) -> bool {
        if self.ready_state != ReadyState::Closed {
            self.ready_state = ReadyState::Closed;
            self.duration = f64::NAN;
            self.source_buffers.clear();
            self.live_seekable_range = None;
            true
        } else {
            false
        }
    }

    /// Adds a new `SourceBuffer` matching `mime` when `ready_state == Open`.
    pub fn add_source_buffer(&mut self, mime: &str) -> Result<usize, MseError> {
        if mime.trim().is_empty() {
            return Err(MseError::TypeError(
                "addSourceBuffer requires a non-empty MIME type".to_string(),
            ));
        }
        if parse_supported_mime_type(mime).is_none() {
            return Err(MseError::NotSupported(format!(
                "MIME type `{mime}` is not supported"
            )));
        }
        if self.ready_state != ReadyState::Open {
            return Err(MseError::InvalidState(
                "addSourceBuffer requires MediaSource.readyState === 'open'".to_string(),
            ));
        }
        let sb = SourceBufferEngine::new(mime)?;
        let idx = self.source_buffers.len();
        self.source_buffers.push(sb);
        Ok(idx)
    }

    /// Removes the `SourceBuffer` at `index`.
    pub fn remove_source_buffer(&mut self, index: usize) -> Result<SourceBufferEngine, MseError> {
        if index >= self.source_buffers.len() {
            return Err(MseError::NotSupported(
                "SourceBuffer does not belong to this MediaSource".to_string(),
            ));
        }
        Ok(self.source_buffers.remove(index))
    }

    /// Updates `duration` per W3C MSE Duration Change Algorithm (§2.4.6), returning `Ok(true)` if
    /// `duration` changed (so the caller can fire `durationchange` and seek if `currentTime > duration`).
    pub fn set_duration_spec(&mut self, mut new_duration: f64) -> Result<bool, MseError> {
        if new_duration.is_nan() || new_duration < 0.0 {
            return Err(MseError::TypeError(
                "MediaSource.duration must be non-negative and not NaN".to_string(),
            ));
        }
        if self.ready_state != ReadyState::Open {
            return Err(MseError::InvalidState(
                "cannot set duration unless MediaSource.readyState === 'open'".to_string(),
            ));
        }
        if self.source_buffers.iter().any(|sb| sb.updating) {
            return Err(MseError::InvalidState(
                "cannot set duration while a SourceBuffer is updating".to_string(),
            ));
        }

        // W3C MSE §2.4.6 step 3: throw InvalidStateError if new_duration is less than any
        // buffered coded frame's presentation start time (or any track's highest buffered start).
        let mut highest_frame_pts = 0.0f64;
        let mut has_frames = false;
        let mut highest_end = 0.0f64;
        for sb in &self.source_buffers {
            if let Some(pts) = sb.highest_coded_frame_pts() {
                has_frames = true;
                if pts > highest_frame_pts {
                    highest_frame_pts = pts;
                }
            }
            let end = sb.highest_coded_frame_end();
            if end > highest_end {
                highest_end = end;
            }
        }
        if has_frames && new_duration < highest_frame_pts - 1e-6 {
            return Err(MseError::InvalidState(
                "cannot set duration below highest buffered coded frame presentation time"
                    .to_string(),
            ));
        }
        // W3C MSE §2.4.6 step 4: clamp new_duration upward to highest buffered end time.
        if has_frames && new_duration < highest_end {
            new_duration = highest_end;
        }

        let old_duration = self.duration;
        if old_duration == new_duration {
            return Ok(false);
        }
        self.duration = new_duration;
        Ok(true)
    }

    /// Updates `duration` per W3C MSE Duration Change Algorithm (§2.4.6).
    pub fn set_duration(&mut self, new_duration: f64) -> Result<(), MseError> {
        self.set_duration_spec(new_duration).map(|_| ())
    }

    /// Transitions `readyState` from `"open"` to `"ended"` (or signals a stream error).
    /// Returns `Ok(true)` if `duration` changed as a result.
    pub fn end_of_stream_spec(
        &mut self,
        error: Option<EndOfStreamError>,
    ) -> Result<bool, MseError> {
        if self.ready_state != ReadyState::Open {
            return Err(MseError::InvalidState(
                "endOfStream requires MediaSource.readyState === 'open'".to_string(),
            ));
        }
        if self.source_buffers.iter().any(|sb| sb.updating) {
            return Err(MseError::InvalidState(
                "cannot call endOfStream while a SourceBuffer is updating".to_string(),
            ));
        }
        self.ready_state = ReadyState::Ended;
        self.end_of_stream_error = error;
        let mut duration_changed = false;
        if error.is_none() {
            let highest = self
                .source_buffers
                .iter()
                .map(SourceBufferEngine::highest_coded_frame_end)
                .fold(0.0f64, f64::max);
            if self.duration.is_nan() || (self.duration - highest).abs() > 1e-12 {
                self.duration = highest;
                duration_changed = true;
            }
        }
        Ok(duration_changed)
    }

    /// Transitions `readyState` from `"open"` to `"ended"` (or signals a stream error).
    pub fn end_of_stream(&mut self, error: Option<EndOfStreamError>) -> Result<(), MseError> {
        self.end_of_stream_spec(error).map(|_| ())
    }

    /// Re-opens an `"ended"` `MediaSource` automatically when `appendBuffer` or `remove` runs.
    pub fn reopen_if_ended(&mut self) -> bool {
        if self.ready_state == ReadyState::Ended {
            self.ready_state = ReadyState::Open;
            true
        } else {
            false
        }
    }

    /// Sets live seekable range per W3C MSE §2.2.
    pub fn set_live_seekable_range(&mut self, start: f64, end: f64) -> Result<(), MseError> {
        if self.ready_state != ReadyState::Open {
            return Err(MseError::InvalidState(
                "setLiveSeekableRange requires readyState === 'open'".to_string(),
            ));
        }
        if !start.is_finite() || !end.is_finite() || start < 0.0 || start > end {
            return Err(MseError::TypeError(
                "invalid live seekable range bounds".to_string(),
            ));
        }
        self.live_seekable_range = Some((start, end));
        Ok(())
    }

    /// Clears the live seekable range.
    pub fn clear_live_seekable_range(&mut self) -> Result<(), MseError> {
        if self.ready_state != ReadyState::Open {
            return Err(MseError::InvalidState(
                "clearLiveSeekableRange requires readyState === 'open'".to_string(),
            ));
        }
        self.live_seekable_range = None;
        Ok(())
    }

    /// Computes combined `HTMLMediaElement.buffered` across all active `SourceBuffer`s (W3C MSE §2.2).
    pub fn buffered(&self) -> TimeRanges {
        let is_ended = self.ready_state == ReadyState::Ended;
        let active: Vec<&SourceBufferEngine> = self
            .source_buffers
            .iter()
            .filter(|sb| {
                sb.init_segment_received || !sb.tracks.is_empty() || !sb.buffered.is_empty()
            })
            .collect();
        if active.is_empty() {
            return TimeRanges::new();
        }
        let mut ranges: Vec<TimeRanges> = Vec::with_capacity(active.len());
        let mut highest_end = 0.0f64;
        for sb in &active {
            let tr = sb.buffered_ranges(is_ended);
            if tr.is_empty() {
                return TimeRanges::new();
            }
            highest_end = highest_end.max(tr.highest_end());
            ranges.push(tr);
        }

        if is_ended {
            for tr in &mut ranges {
                tr.extend_last_to(highest_end);
            }
        }
        let mut iter = ranges.into_iter();
        let mut acc = iter.next().unwrap();
        for cur in iter {
            acc = acc.intersect(&cur);
        }
        acc
    }

    /// Computes `HTMLMediaElement.seekable` per W3C MSE §2.2 / HTML §4.8.11.
    pub fn seekable(&self) -> TimeRanges {
        let mut tr = TimeRanges::new();
        if self.duration.is_infinite() && self.duration.is_sign_positive() {
            let buf = self.buffered();
            if let Some((live_start, live_end)) = self.live_seekable_range {
                if let (Ok(buf_start), Some(&(_, buf_end))) = (buf.start(0), buf.as_slice().last())
                {
                    tr.add_range(live_start.min(buf_start), live_end.max(buf_end));
                } else {
                    tr.add_range(live_start, live_end);
                }
            } else {
                let end = buf.highest_end();
                if end > 0.0 {
                    tr.add_range(0.0, end);
                }
            }
        } else if self.duration.is_finite() && self.duration > 0.0 {
            tr.add_range(0.0, self.duration);
        }
        tr
    }

    /// Synchronizes `duration` upward if a `SourceBuffer` appended frames beyond the current duration,
    /// or initializes `duration` from an init segment when currently `NaN`.
    /// Returns `true` if `duration` changed.
    pub fn sync_duration_after_append_with_init(
        &mut self,
        init_duration_s: Option<f64>,
        highest_end: f64,
    ) -> bool {
        let old = self.duration;
        if let Some(id) = init_duration_s {
            if id.is_finite() && id > 0.0 && self.duration.is_nan() {
                self.duration = id;
            }
        }
        if highest_end > 0.0 && (self.duration.is_nan() || highest_end > self.duration) {
            self.duration = highest_end;
        }
        (old.is_nan() != self.duration.is_nan()) || (old - self.duration).abs() > 1e-12
    }

    /// Synchronizes `duration` upward if a `SourceBuffer` appended frames beyond the current duration.
    pub fn sync_duration_after_append(&mut self, highest_end: f64) {
        let _ = self.sync_duration_after_append_with_init(None, highest_end);
    }
}
