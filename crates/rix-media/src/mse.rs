//! W3C Media Source Extensions (MSE) core engine (`MediaSourceEngine`, `SourceBufferEngine`, `TimeRanges`).
//!
//! Pure-Rust, zero site-specific hardcodes, supporting ISO BMFF fMP4 and WebM/Matroska segments,
//! coalesced buffered range merging, coded-frame quota eviction, and seamless `changeType` codec switching.

use crate::demuxer::detect_container;
use crate::demuxer::mp4::{parse_mp4_init_segment, parse_mp4_media_segments, Mp4InitState};
use crate::demuxer::webm::{parse_webm_init_segment, parse_webm_media_segments, WebmInitState};
use crate::types::{AudioCodec, ContainerFormat, EncodedPacket, PacketKind, TrackInfo, VideoCodec};

/// Default per-`SourceBuffer` coded-frame quota (64 MiB).
pub const DEFAULT_SOURCE_BUFFER_QUOTA_BYTES: usize = 64 * 1024 * 1024;

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

/// Parsed MSE MIME container + codec descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMimeType {
    pub essence: String,
    pub container: ContainerFormat,
    pub is_video_mime: bool,
    pub video_codecs: Vec<VideoCodec>,
    pub audio_codecs: Vec<AudioCodec>,
}

/// Parses and validates an MSE MIME type string according to W3C MSE `isTypeSupported` rules.
pub fn parse_supported_mime_type(mime: &str) -> Option<ParsedMimeType> {
    let trimmed = mime.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut parts = trimmed.split(';');
    let essence = parts.next()?.trim().to_ascii_lowercase();
    let (container, is_video_mime) = match essence.as_str() {
        "video/mp4" => (ContainerFormat::Mp4, true),
        "audio/mp4" => (ContainerFormat::Mp4, false),
        "video/webm" => (ContainerFormat::WebM, true),
        "audio/webm" => (ContainerFormat::WebM, false),
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
        let val = v.trim().trim_matches('"').trim_matches('\'').trim();
        if key == "codecs" {
            if codecs_param.is_some() || val.is_empty() {
                return None;
            }
            codecs_param = Some(val.to_string());
        } else if key == "profiles" || key == "bitrate" || key == "width" || key == "height" {
            // Accept standard optional media parameters.
        } else {
            return None;
        }
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
                // `audio/*` MIME types must not list video codecs.
                if !is_video_mime {
                    return None;
                }
                // Container-codec compatibility checks.
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
        is_video_mime,
        video_codecs,
        audio_codecs,
    })
}

/// W3C `MediaSource.isTypeSupported(type)` predicate.
pub fn is_type_supported(mime: &str) -> bool {
    parse_supported_mime_type(mime).is_some()
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
}

impl CodedFrame {
    pub fn end_s(&self) -> f64 {
        self.pts_s + self.duration_s.max(0.001)
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
    pub mode: AppendMode,
    pub updating: bool,
    pub timestamp_offset: f64,
    pub append_window_start: f64,
    pub append_window_end: f64,
    pub quota_bytes: usize,
    pub buffered_bytes: usize,
    pub buffered: TimeRanges,
    pub tracks: Vec<TrackInfo>,
    pub frames: Vec<CodedFrame>,
    pub codec_generation: u64,
    pub highest_end_timestamp: f64,
    group_start_timestamp: Option<f64>,
    group_end_timestamp: f64,
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
        Ok(Self {
            mime_type: mime.to_string(),
            container: parsed.container,
            mode: AppendMode::Segments,
            updating: false,
            timestamp_offset: 0.0,
            append_window_start: 0.0,
            append_window_end: f64::INFINITY,
            quota_bytes: DEFAULT_SOURCE_BUFFER_QUOTA_BYTES,
            buffered_bytes: 0,
            buffered: TimeRanges::new(),
            tracks: Vec::new(),
            frames: Vec::new(),
            codec_generation: 1,
            highest_end_timestamp: 0.0,
            group_start_timestamp: None,
            group_end_timestamp: 0.0,
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
        if self.updating {
            return Err(MseError::InvalidState(
                "cannot modify timestampOffset while updating".to_string(),
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
        if self.updating {
            return Err(MseError::InvalidState(
                "cannot modify mode while updating".to_string(),
            ));
        }
        self.mode = mode;
        if mode == AppendMode::Sequence {
            self.group_start_timestamp = Some(self.group_end_timestamp);
        }
        Ok(())
    }

    /// Aborts any active segment parser state and resets append window bounds per W3C MSE §3.2.
    pub fn abort(&mut self) {
        self.updating = false;
        self.pending_bytes.clear();
        self.append_window_start = 0.0;
        self.append_window_end = f64::INFINITY;
        self.group_start_timestamp = None;
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
        self.codec_generation = self.codec_generation.saturating_add(1);
        self.mp4_state = None;
        self.webm_state = None;
        self.pending_bytes.clear();
        if self.mode == AppendMode::Sequence {
            self.group_start_timestamp = Some(self.group_end_timestamp);
        }
        Ok(())
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

        // Single append larger than the entire buffer quota immediately fails with QuotaExceededError.
        if bytes.len() > self.quota_bytes {
            return Err(MseError::QuotaExceeded(format!(
                "append of {} bytes exceeds SourceBuffer quota of {} bytes",
                bytes.len(),
                self.quota_bytes
            )));
        }

        // Run coded-frame eviction before parsing if incoming bytes would exceed quota.
        let mut frames_evicted = 0usize;
        if self.buffered_bytes.saturating_add(bytes.len()) > self.quota_bytes {
            frames_evicted += self.evict_coded_frames(
                self.buffered_bytes
                    .saturating_add(bytes.len())
                    .saturating_sub(self.quota_bytes),
                current_playback_time_s,
            );
            if self.buffered_bytes.saturating_add(bytes.len()) > self.quota_bytes {
                return Err(MseError::QuotaExceeded(format!(
                    "SourceBuffer quota ({} bytes) exceeded after eviction",
                    self.quota_bytes
                )));
            }
        }

        self.pending_bytes.extend_from_slice(bytes);
        let input = std::mem::take(&mut self.pending_bytes);

        let mut result = AppendResult {
            frames_evicted,
            highest_end_timestamp: self.highest_end_timestamp,
            ..Default::default()
        };

        let packets = match self.container {
            ContainerFormat::Mp4 => self.parse_mp4_chunk(&input, &mut result)?,
            ContainerFormat::WebM => self.parse_webm_chunk(&input, &mut result)?,
            _ => {
                return Err(MseError::NotSupported(
                    "SourceBuffer container must be MP4 or WebM".to_string(),
                ));
            }
        };

        // Process decoded elementary packets through W3C MSE Coded Frame Processing Algorithm (§3.5.8).
        for pkt in packets {
            let raw_pts_s = pkt.pts_us as f64 / 1_000_000.0;
            let raw_dts_s = pkt.dts_us as f64 / 1_000_000.0;
            let dur_s = if pkt.duration_us > 0 {
                pkt.duration_us as f64 / 1_000_000.0
            } else {
                1.0 / 30.0
            };

            if self.mode == AppendMode::Sequence {
                if let Some(group_start) = self.group_start_timestamp.take() {
                    self.timestamp_offset = group_start - raw_pts_s;
                }
            }

            let pts_s = raw_pts_s + self.timestamp_offset;
            let dts_s = raw_dts_s + self.timestamp_offset;
            let frame_end = pts_s + dur_s;

            // Append window filtering (§3.5.8 step 1.11–1.12).
            if pts_s < self.append_window_start || frame_end > self.append_window_end {
                continue;
            }

            // Replace any overlapping frame on the same track near `pts_s`.
            let overlap_eps = 0.000_5;
            self.frames.retain(|f| {
                if f.track_id != pkt.track_id {
                    return true;
                }
                let overlaps = f.pts_s >= pts_s - overlap_eps && f.pts_s < frame_end - overlap_eps;
                if overlaps {
                    self.buffered_bytes = self.buffered_bytes.saturating_sub(f.data.len());
                    false
                } else {
                    true
                }
            });

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
            });
            result.frames_appended += 1;
            if frame_end > self.group_end_timestamp {
                self.group_end_timestamp = frame_end;
            }
            if frame_end > self.highest_end_timestamp {
                self.highest_end_timestamp = frame_end;
            }
        }

        // Enforce quota after inserting coded frames as well.
        if self.buffered_bytes > self.quota_bytes {
            let excess = self.buffered_bytes - self.quota_bytes;
            result.frames_evicted += self.evict_coded_frames(excess, current_playback_time_s);
            if self.buffered_bytes > self.quota_bytes {
                return Err(MseError::QuotaExceeded(
                    "SourceBuffer quota exceeded".to_string(),
                ));
            }
        }

        self.frames.sort_by(|a, b| {
            a.dts_s
                .total_cmp(&b.dts_s)
                .then_with(|| a.pts_s.total_cmp(&b.pts_s))
        });
        self.recompute_buffered_ranges();
        result.highest_end_timestamp = self.highest_end_timestamp;
        Ok(result)
    }

    fn parse_mp4_chunk(
        &mut self,
        input: &[u8],
        result: &mut AppendResult,
    ) -> Result<Vec<EncodedPacket>, MseError> {
        let has_init = contains_top_level_mp4_box(input, b"moov");
        let has_media = contains_top_level_mp4_box(input, b"moof");

        if !has_init && !has_media {
            // Validate magic header if present; otherwise reject corrupt media segments.
            if detect_container(input) != Some(ContainerFormat::Mp4) {
                return Err(MseError::DecodeError(
                    "appended bytes are not a valid ISO BMFF MP4 segment".to_string(),
                ));
            }
            // Pure `ftyp`/`styp`/`sidx` prefix: keep pending until `moov` or `moof` arrives.
            self.pending_bytes = input.to_vec();
            return Ok(Vec::new());
        }

        if has_init {
            let new_state =
                parse_mp4_init_segment(input).map_err(|e| MseError::DecodeError(e.to_string()))?;
            self.tracks = new_state.tracks();
            self.mp4_state = Some(new_state);
            result.init_segment_received = true;
            if !has_media {
                return Ok(Vec::new());
            }
        }

        let Some(state) = self.mp4_state.as_mut() else {
            return Err(MseError::DecodeError(
                "fMP4 media segment received before initialization segment".to_string(),
            ));
        };
        parse_mp4_media_segments(input, state).map_err(|e| MseError::DecodeError(e.to_string()))
    }

    fn parse_webm_chunk(
        &mut self,
        input: &[u8],
        result: &mut AppendResult,
    ) -> Result<Vec<EncodedPacket>, MseError> {
        let is_ebml_init = input.len() >= 4 && input[0..4] == [0x1a, 0x45, 0xdf, 0xa3];
        if is_ebml_init {
            let mut state =
                parse_webm_init_segment(input).map_err(|e| MseError::DecodeError(e.to_string()))?;
            self.tracks = state.tracks();
            result.init_segment_received = true;
            // A combined WebM init+Cluster buffer may already contain clusters; parse them too.
            let packets = parse_webm_media_segments(input, &mut state)
                .map_err(|e| MseError::DecodeError(e.to_string()))?;
            self.webm_state = Some(state);
            return Ok(packets);
        }

        let Some(state) = self.webm_state.as_mut() else {
            return Err(MseError::DecodeError(
                "WebM cluster received before EBML initialization segment".to_string(),
            ));
        };
        parse_webm_media_segments(input, state).map_err(|e| MseError::DecodeError(e.to_string()))
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
        if removed_any {
            self.recompute_buffered_ranges();
        }
        Ok(())
    }

    /// Evicts oldest GOPs behind `current_playback_time_s` (and then farthest future GOPs if needed)
    /// until at least `bytes_needed` are reclaimed.
    fn evict_coded_frames(&mut self, bytes_needed: usize, current_playback_time_s: f64) -> usize {
        if bytes_needed == 0 || self.frames.is_empty() {
            return 0;
        }
        let mut reclaimed = 0usize;
        let mut evicted_count = 0usize;

        // Pass 1: evict earliest frames strictly before playback head (`current_playback_time_s - 0.5s`).
        let safe_before = (current_playback_time_s - 0.5).max(0.0);
        let mut idx = 0usize;
        while idx < self.frames.len() && reclaimed < bytes_needed {
            if self.frames[idx].end_s() <= safe_before {
                let f = self.frames.remove(idx);
                reclaimed += f.data.len();
                self.buffered_bytes = self.buffered_bytes.saturating_sub(f.data.len());
                evicted_count += 1;
            } else {
                idx += 1;
            }
        }

        // Pass 2: if still over quota, evict from the front (oldest GOP) preserving remaining frames.
        while !self.frames.is_empty() && reclaimed < bytes_needed {
            let f = self.frames.remove(0);
            reclaimed += f.data.len();
            self.buffered_bytes = self.buffered_bytes.saturating_sub(f.data.len());
            evicted_count += 1;
        }

        if evicted_count > 0 {
            self.recompute_buffered_ranges();
        }
        evicted_count
    }

    fn recompute_buffered_ranges(&mut self) {
        let mut tr = TimeRanges::new();
        for f in &self.frames {
            tr.add_range(f.pts_s, f.end_s());
        }
        self.buffered = tr;
    }
}

fn contains_top_level_mp4_box(buf: &[u8], target: &[u8; 4]) -> bool {
    let mut off = 0usize;
    while off + 8 <= buf.len() {
        let sz = u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]) as usize;
        if &buf[off + 4..off + 8] == target {
            return true;
        }
        let step = if sz == 0 {
            break;
        } else if sz == 1 && off + 16 <= buf.len() {
            u64::from_be_bytes([
                buf[off + 8],
                buf[off + 9],
                buf[off + 10],
                buf[off + 11],
                buf[off + 12],
                buf[off + 13],
                buf[off + 14],
                buf[off + 15],
            ]) as usize
        } else {
            sz
        };
        if step < 8 || off.saturating_add(step) > buf.len() {
            break;
        }
        off += step;
    }
    false
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
        if !is_type_supported(mime) {
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

    /// Updates `duration` per W3C MSE Duration Change Algorithm (§2.4.6).
    pub fn set_duration(&mut self, new_duration: f64) -> Result<(), MseError> {
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
        let old_duration = self.duration;
        self.duration = new_duration;
        if old_duration.is_finite() && new_duration < old_duration {
            for sb in &mut self.source_buffers {
                let _ = sb.remove(new_duration, f64::INFINITY);
            }
        }
        Ok(())
    }

    /// Transitions `readyState` from `"open"` to `"ended"` (or signals a stream error).
    pub fn end_of_stream(&mut self, error: Option<EndOfStreamError>) -> Result<(), MseError> {
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
        if error.is_none() {
            let highest = self
                .source_buffers
                .iter()
                .map(|sb| sb.buffered.highest_end().max(sb.highest_end_timestamp))
                .fold(0.0f64, f64::max);
            if self.duration.is_nan() || highest > self.duration {
                self.duration = highest;
            }
        }
        Ok(())
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
        let active: Vec<&SourceBufferEngine> = self
            .source_buffers
            .iter()
            .filter(|sb| !sb.tracks.is_empty() || !sb.buffered.is_empty())
            .collect();
        if active.is_empty() {
            return TimeRanges::new();
        }
        let highest_end = active
            .iter()
            .map(|sb| sb.buffered.highest_end())
            .fold(0.0f64, f64::max);

        let mut iter = active.into_iter();
        let first = iter.next().unwrap();
        let mut acc = first.buffered.clone();
        if self.ready_state == ReadyState::Ended {
            acc.extend_last_to(highest_end);
        }
        for sb in iter {
            let mut cur = sb.buffered.clone();
            if self.ready_state == ReadyState::Ended {
                cur.extend_last_to(highest_end);
            }
            acc = acc.intersect(&cur);
        }
        acc
    }

    /// Synchronizes `duration` upward if a `SourceBuffer` appended frames beyond the current duration.
    pub fn sync_duration_after_append(&mut self, highest_end: f64) {
        if highest_end > 0.0 && (self.duration.is_nan() || highest_end > self.duration) {
            self.duration = highest_end;
        }
    }
}
