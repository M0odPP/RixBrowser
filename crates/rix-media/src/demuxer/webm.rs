//! Pure-Rust EBML / WebM / Matroska demuxer and segment builder (supporting MSE clusters).

use std::collections::HashMap;

use crate::types::{
    AudioCodec, AudioTrackConfig, ContainerFormat, DemuxedMedia, EncodedPacket, MediaError,
    MseSample, PacketKind, TrackInfo, TrackKind, VideoCodec, VideoTrackConfig,
};

// Standard EBML / Matroska / WebM element IDs (full VINT-encoded IDs).
const ID_EBML: u32 = 0x1A45_DFA3;
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_INFO: u32 = 0x1549_A966;
const ID_TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const ID_DURATION: u32 = 0x4489;
const ID_TRACKS: u32 = 0x1654_AE6B;
const ID_TRACK_ENTRY: u32 = 0xAE;
const ID_TRACK_NUMBER: u32 = 0xD7;
const ID_TRACK_TYPE: u32 = 0x83;
const ID_DEFAULT_DURATION: u32 = 0x23_E383;
const ID_LANGUAGE: u32 = 0x22_B59C;
const ID_CODEC_ID: u32 = 0x86;
const ID_CODEC_PRIVATE: u32 = 0x63A2;
const ID_VIDEO: u32 = 0xE0;
const ID_PIXEL_WIDTH: u32 = 0xB0;
const ID_PIXEL_HEIGHT: u32 = 0xBA;
const ID_AUDIO: u32 = 0xE1;
const ID_SAMPLING_FREQUENCY: u32 = 0xB5;
const ID_CHANNELS: u32 = 0x9F;
const ID_CLUSTER: u32 = 0x1F43_B675;
const ID_CLUSTER_TIMESTAMP: u32 = 0xE7;
const ID_SIMPLE_BLOCK: u32 = 0xA3;
const ID_BLOCK_GROUP: u32 = 0xA0;
const ID_BLOCK: u32 = 0xA1;
const ID_BLOCK_DURATION: u32 = 0x9B;
const ID_REFERENCE_BLOCK: u32 = 0xFB;

/// Stateful WebM/Matroska header state for incremental MSE Cluster parsing.
#[derive(Debug, Clone)]
pub struct WebmInitState {
    /// Timestamp scale in nanoseconds per tick (WebM default: 1_000_000 ns = 1 ms).
    pub timestamp_scale_ns: u64,
    pub duration_us: Option<u64>,
    pub init_duration_s: Option<f64>,
    pub tracks: HashMap<u32, TrackInfo>,
    pub default_durations_us: HashMap<u32, u64>,
    pub default_durations_s: HashMap<u32, f64>,
    pub last_frame_delta_s: HashMap<u32, f64>,
    pub track_order: Vec<u32>,
}

impl Default for WebmInitState {
    fn default() -> Self {
        Self {
            timestamp_scale_ns: 1_000_000,
            duration_us: None,
            init_duration_s: None,
            tracks: HashMap::new(),
            default_durations_us: HashMap::new(),
            default_durations_s: HashMap::new(),
            last_frame_delta_s: HashMap::new(),
            track_order: Vec::new(),
        }
    }
}

/// Structural classification of an incremental WebM byte buffer for MSE parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebmBufferStatus {
    Complete { has_init: bool, has_cluster: bool },
    Partial { in_media_segment: bool },
    Corrupt,
}

/// Inspects top-level EBML/WebM elements in `bytes` to distinguish complete segments,
/// partial streamed clusters/headers, and corrupt byte streams.
pub fn inspect_webm_buffer(bytes: &[u8]) -> WebmBufferStatus {
    if bytes.is_empty() {
        return WebmBufferStatus::Complete {
            has_init: false,
            has_cluster: false,
        };
    }
    let mut pos = 0usize;
    let mut has_init = false;
    let mut has_cluster = false;
    let mut saw_known = false;
    while pos < bytes.len() {
        let Ok((id, id_len)) = read_ebml_id(&bytes[pos..]) else {
            return if saw_known {
                WebmBufferStatus::Partial {
                    in_media_segment: has_cluster,
                }
            } else {
                WebmBufferStatus::Corrupt
            };
        };
        let known = matches!(
            id,
            ID_EBML
                | ID_SEGMENT
                | ID_INFO
                | ID_TRACKS
                | ID_CLUSTER
                | 0x114D_9B74 // SeekHead
                | 0x1C53_BB6B // Cues
                | 0x1254_C367 // Tags
                | 0xEC        // Void
                | 0xBF // CRC-32
        );
        if !known {
            return WebmBufferStatus::Corrupt;
        }
        saw_known = true;
        let size_pos = pos + id_len;
        if size_pos >= bytes.len() {
            return WebmBufferStatus::Partial {
                in_media_segment: has_cluster || id == ID_CLUSTER,
            };
        }
        let Ok((size_opt, size_len)) = read_ebml_size(&bytes[size_pos..]) else {
            return WebmBufferStatus::Partial {
                in_media_segment: has_cluster || id == ID_CLUSTER,
            };
        };
        let body_start = size_pos + size_len;
        match size_opt {
            Some(sz) => {
                let end = body_start.saturating_add(sz);
                if end > bytes.len() {
                    if id == ID_SEGMENT {
                        // A streaming Segment with a declared total size larger than this chunk
                        // still has complete child Info/Tracks/Cluster elements inside!
                        let sub = inspect_webm_buffer(&bytes[body_start..]);
                        return match sub {
                            WebmBufferStatus::Complete {
                                has_init: si,
                                has_cluster: sc,
                            } => WebmBufferStatus::Complete {
                                has_init: has_init || si,
                                has_cluster: has_cluster || sc,
                            },
                            WebmBufferStatus::Partial { in_media_segment } => {
                                WebmBufferStatus::Partial {
                                    in_media_segment: in_media_segment || has_cluster,
                                }
                            }
                            WebmBufferStatus::Corrupt => WebmBufferStatus::Corrupt,
                        };
                    }
                    return WebmBufferStatus::Partial {
                        in_media_segment: has_cluster || id == ID_CLUSTER,
                    };
                }
                if id == ID_EBML || id == ID_TRACKS {
                    has_init = true;
                } else if id == ID_CLUSTER {
                    has_cluster = true;
                } else if id == ID_SEGMENT {
                    if let WebmBufferStatus::Complete {
                        has_init: si,
                        has_cluster: sc,
                    } = inspect_webm_buffer(&bytes[body_start..end])
                    {
                        has_init |= si;
                        has_cluster |= sc;
                    }
                }
                pos = end;
            }
            None => {
                if id == ID_SEGMENT {
                    let sub = inspect_webm_buffer(&bytes[body_start..]);
                    return match sub {
                        WebmBufferStatus::Complete {
                            has_init: si,
                            has_cluster: sc,
                        } => WebmBufferStatus::Complete {
                            has_init: has_init || si,
                            has_cluster: has_cluster || sc,
                        },
                        other => other,
                    };
                }
                if id == ID_CLUSTER {
                    has_cluster = true;
                }
                break;
            }
        }
    }
    WebmBufferStatus::Complete {
        has_init,
        has_cluster,
    }
}

impl WebmInitState {
    pub fn tracks(&self) -> Vec<TrackInfo> {
        self.track_order
            .iter()
            .filter_map(|id| self.tracks.get(id).cloned())
            .collect()
    }
}

/// Demuxes a complete WebM / Matroska byte buffer.
pub fn demux_webm(bytes: &[u8]) -> Result<DemuxedMedia, MediaError> {
    let mut state = WebmInitState::default();
    let mut packets = Vec::new();
    parse_webm_stream(bytes, &mut state, &mut packets)?;
    if state.tracks.is_empty() {
        return Err(MediaError::MalformedContainer(
            "WebM/Matroska stream has no recognized tracks".to_string(),
        ));
    }
    Ok(DemuxedMedia {
        container: ContainerFormat::WebM,
        duration_us: state.duration_us,
        tracks: state.tracks(),
        packets,
    })
}

/// Parses a WebM initialization segment (`EBML` + `Segment` + `Info` + `Tracks`).
pub fn parse_webm_init_segment(bytes: &[u8]) -> Result<WebmInitState, MediaError> {
    let mut state = WebmInitState::default();
    let mut packets = Vec::new();
    parse_webm_stream(bytes, &mut state, &mut packets)?;
    if state.tracks.is_empty() {
        return Err(MediaError::MalformedContainer(
            "WebM init segment contains no Tracks element".to_string(),
        ));
    }
    Ok(state)
}

/// Parses one or more WebM `Cluster` media segments (or combined init+clusters) using `state`.
pub fn parse_webm_media_segments(
    bytes: &[u8],
    state: &mut WebmInitState,
) -> Result<Vec<EncodedPacket>, MediaError> {
    let mut packets = Vec::new();
    parse_webm_stream(bytes, state, &mut packets)?;
    Ok(packets)
}

/// Parses one or more WebM `Cluster` media segments into high-precision [`MseSample`]s,
/// inferring missing `SimpleBlock` frame durations from consecutive same-track timestamps.
pub fn parse_webm_mse_segments(
    bytes: &[u8],
    state: &mut WebmInitState,
) -> Result<Vec<MseSample>, MediaError> {
    let mut packets = Vec::new();
    parse_webm_stream(bytes, state, &mut packets)?;
    let mut samples: Vec<MseSample> = packets
        .into_iter()
        .map(|p| {
            let video_dims = state.tracks.get(&p.track_id).and_then(|t| match &t.kind {
                TrackKind::Video(v) => Some((v.width, v.height)),
                TrackKind::Audio(_) => None,
            });
            let dur_s = if p.duration_us > 0 {
                p.duration_us as f64 / 1_000_000.0
            } else {
                state
                    .default_durations_s
                    .get(&p.track_id)
                    .copied()
                    .unwrap_or(0.0)
            };
            MseSample {
                track_id: p.track_id,
                kind: p.kind,
                data: p.data,
                pts_s: p.pts_us as f64 / 1_000_000.0,
                dts_s: p.dts_us as f64 / 1_000_000.0,
                duration_s: dur_s,
                is_keyframe: p.is_keyframe,
                video_dims,
            }
        })
        .collect();

    // Infer missing SimpleBlock durations from the next frame's PTS on the same track.
    let mut indices_by_track: HashMap<u32, Vec<usize>> = HashMap::new();
    for (idx, s) in samples.iter().enumerate() {
        indices_by_track.entry(s.track_id).or_default().push(idx);
    }
    for (tid, idxs) in indices_by_track {
        for win in idxs.windows(2) {
            let cur = win[0];
            let next = win[1];
            let delta = samples[next].pts_s - samples[cur].pts_s;
            if delta > 0.0 && delta < 1.0 {
                state.last_frame_delta_s.insert(tid, delta);
                if samples[cur].duration_s <= 0.0 {
                    samples[cur].duration_s = delta;
                }
            }
        }
        if let Some(&last_idx) = idxs.last() {
            if samples[last_idx].duration_s <= 0.0 {
                let default_kind_dur = match samples[last_idx].kind {
                    PacketKind::Video => 1.0 / 30.0,
                    PacketKind::Audio => 0.020,
                };
                let fallback = state
                    .last_frame_delta_s
                    .get(&tid)
                    .copied()
                    .or_else(|| state.default_durations_s.get(&tid).copied())
                    .unwrap_or(default_kind_dur);
                samples[last_idx].duration_s = fallback;
            }
        }
    }

    Ok(samples)
}

fn parse_webm_stream(
    bytes: &[u8],
    state: &mut WebmInitState,
    packets: &mut Vec<EncodedPacket>,
) -> Result<(), MediaError> {
    let mut pos = 0usize;
    while pos < bytes.len() {
        let Some((id, body, next_pos)) = read_element(bytes, pos)? else {
            break;
        };
        match id {
            ID_EBML => {}
            ID_SEGMENT => {
                parse_segment_children(body, state, packets)?;
            }
            ID_INFO => parse_info(body, state)?,
            ID_TRACKS => parse_tracks(body, state)?,
            ID_CLUSTER => parse_cluster(body, state, packets)?,
            _ => {}
        }
        pos = next_pos;
    }
    Ok(())
}

fn parse_segment_children(
    segment_body: &[u8],
    state: &mut WebmInitState,
    packets: &mut Vec<EncodedPacket>,
) -> Result<(), MediaError> {
    let mut pos = 0usize;
    while pos < segment_body.len() {
        let Some((id, body, next_pos)) = read_element(segment_body, pos)? else {
            break;
        };
        match id {
            ID_INFO => parse_info(body, state)?,
            ID_TRACKS => parse_tracks(body, state)?,
            ID_CLUSTER => parse_cluster(body, state, packets)?,
            _ => {}
        }
        pos = next_pos;
    }
    Ok(())
}

fn parse_info(info_body: &[u8], state: &mut WebmInitState) -> Result<(), MediaError> {
    let mut raw_duration: Option<f64> = None;
    let mut pos = 0usize;
    while pos < info_body.len() {
        let Some((id, body, next_pos)) = read_element(info_body, pos)? else {
            break;
        };
        match id {
            ID_TIMESTAMP_SCALE => {
                let scale = read_be_uint(body);
                if scale > 0 {
                    state.timestamp_scale_ns = scale;
                }
            }
            ID_DURATION => {
                raw_duration = read_be_float(body);
            }
            _ => {}
        }
        pos = next_pos;
    }
    if let Some(dur_ticks) = raw_duration {
        if dur_ticks.is_finite() && dur_ticks >= 0.0 {
            let dur_s = dur_ticks * (state.timestamp_scale_ns as f64) / 1_000_000_000.0;
            if dur_s > 0.0 {
                state.init_duration_s = Some(dur_s);
            }
            let us = (dur_ticks * (state.timestamp_scale_ns as f64) / 1_000.0).round();
            if us >= 0.0 {
                state.duration_us = Some(us as u64);
            }
        }
    }
    Ok(())
}

fn parse_tracks(tracks_body: &[u8], state: &mut WebmInitState) -> Result<(), MediaError> {
    let mut pos = 0usize;
    while pos < tracks_body.len() {
        let Some((id, body, next_pos)) = read_element(tracks_body, pos)? else {
            break;
        };
        if id == ID_TRACK_ENTRY {
            parse_track_entry(body, state)?;
        }
        pos = next_pos;
    }
    Ok(())
}

fn parse_track_entry(entry_body: &[u8], state: &mut WebmInitState) -> Result<(), MediaError> {
    let mut track_number = 0u32;
    let mut track_type = 0u64;
    let mut codec_id = String::new();
    let mut codec_private = Vec::new();
    let mut language = None;
    let mut default_dur_ns = 0u64;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut sample_rate = 48_000u32;
    let mut channels = 2u16;

    let mut pos = 0usize;
    while pos < entry_body.len() {
        let Some((id, body, next_pos)) = read_element(entry_body, pos)? else {
            break;
        };
        match id {
            ID_TRACK_NUMBER => track_number = read_be_uint(body) as u32,
            ID_TRACK_TYPE => track_type = read_be_uint(body),
            ID_DEFAULT_DURATION => default_dur_ns = read_be_uint(body),
            ID_LANGUAGE => {
                if let Ok(s) = std::str::from_utf8(body) {
                    let trimmed = s.trim_end_matches('\0');
                    if !trimmed.is_empty() {
                        language = Some(trimmed.to_string());
                    }
                }
            }
            ID_CODEC_ID => {
                if let Ok(s) = std::str::from_utf8(body) {
                    codec_id = s.trim_end_matches('\0').to_string();
                }
            }
            ID_CODEC_PRIVATE => {
                codec_private = body.to_vec();
            }
            ID_VIDEO => {
                let mut vpos = 0usize;
                while vpos < body.len() {
                    let Some((vid, vbody, vnext)) = read_element(body, vpos)? else {
                        break;
                    };
                    match vid {
                        ID_PIXEL_WIDTH => width = read_be_uint(vbody) as u32,
                        ID_PIXEL_HEIGHT => height = read_be_uint(vbody) as u32,
                        _ => {}
                    }
                    vpos = vnext;
                }
            }
            ID_AUDIO => {
                let mut apos = 0usize;
                while apos < body.len() {
                    let Some((aid, abody, anext)) = read_element(body, apos)? else {
                        break;
                    };
                    match aid {
                        ID_SAMPLING_FREQUENCY => {
                            if let Some(sr) = read_be_float(abody) {
                                if sr > 0.0 && sr.is_finite() {
                                    sample_rate = sr.round() as u32;
                                }
                            }
                        }
                        ID_CHANNELS => {
                            channels = (read_be_uint(abody) as u16).max(1);
                        }
                        _ => {}
                    }
                    apos = anext;
                }
            }
            _ => {}
        }
        pos = next_pos;
    }

    if track_number == 0 {
        return Ok(());
    }

    let kind = if track_type == 1 {
        let Some(codec) = VideoCodec::from_codec_str(&codec_id) else {
            return Ok(());
        };
        TrackKind::Video(VideoTrackConfig {
            codec,
            width,
            height,
        })
    } else if track_type == 2 {
        let Some(codec) = AudioCodec::from_codec_str(&codec_id) else {
            return Ok(());
        };
        TrackKind::Audio(AudioTrackConfig {
            codec,
            sample_rate,
            channels,
            pcm_format: None,
        })
    } else {
        return Ok(());
    };

    let timescale = (1_000_000_000u64 / state.timestamp_scale_ns.max(1)).max(1) as u32;
    let info = TrackInfo {
        id: track_number,
        kind,
        timescale,
        duration_us: state.duration_us,
        language,
        extra_data: codec_private,
    };

    if !state.tracks.contains_key(&track_number) {
        state.track_order.push(track_number);
    }
    state.tracks.insert(track_number, info);
    if default_dur_ns > 0 {
        state
            .default_durations_us
            .insert(track_number, default_dur_ns / 1_000);
        state
            .default_durations_s
            .insert(track_number, (default_dur_ns as f64) / 1_000_000_000.0);
    }
    Ok(())
}

fn parse_cluster(
    cluster_body: &[u8],
    state: &WebmInitState,
    packets: &mut Vec<EncodedPacket>,
) -> Result<(), MediaError> {
    let mut cluster_ts: i64 = 0;
    let mut pos = 0usize;
    while pos < cluster_body.len() {
        let Some((id, body, next_pos)) = read_element(cluster_body, pos)? else {
            break;
        };
        match id {
            ID_CLUSTER_TIMESTAMP => {
                cluster_ts = read_be_uint(body) as i64;
            }
            ID_SIMPLE_BLOCK => {
                parse_block_payload(body, cluster_ts, None, None, true, state, packets)?;
            }
            ID_BLOCK_GROUP => {
                let mut block_body: Option<&[u8]> = None;
                let mut block_dur_ticks: Option<u64> = None;
                let mut has_ref_block = false;
                let mut bpos = 0usize;
                while bpos < body.len() {
                    let Some((bid, bsub, bnext)) = read_element(body, bpos)? else {
                        break;
                    };
                    match bid {
                        ID_BLOCK => block_body = Some(bsub),
                        ID_BLOCK_DURATION => block_dur_ticks = Some(read_be_uint(bsub)),
                        ID_REFERENCE_BLOCK => has_ref_block = true,
                        _ => {}
                    }
                    bpos = bnext;
                }
                if let Some(bb) = block_body {
                    parse_block_payload(
                        bb,
                        cluster_ts,
                        block_dur_ticks,
                        Some(!has_ref_block),
                        false,
                        state,
                        packets,
                    )?;
                }
            }
            _ => {}
        }
        pos = next_pos;
    }
    Ok(())
}

fn parse_block_payload(
    block: &[u8],
    cluster_ts: i64,
    block_dur_ticks: Option<u64>,
    block_group_keyframe: Option<bool>,
    is_simple_block: bool,
    state: &WebmInitState,
    packets: &mut Vec<EncodedPacket>,
) -> Result<(), MediaError> {
    let (track_num, vint_len) = read_ebml_vint_value(block)?;
    if block.len() < vint_len + 3 {
        return Err(MediaError::MalformedContainer(
            "truncated WebM block header".to_string(),
        ));
    }
    let track_id = track_num as u32;
    let Some(track) = state.tracks.get(&track_id) else {
        return Err(MediaError::MalformedContainer(format!(
            "WebM block references unknown track {track_id}"
        )));
    };
    let packet_kind = match &track.kind {
        TrackKind::Video(_) => PacketKind::Video,
        TrackKind::Audio(_) => PacketKind::Audio,
    };

    let rel_ts = i16::from_be_bytes([block[vint_len], block[vint_len + 1]]);
    let flags = block[vint_len + 2];
    let lacing = (flags >> 1) & 0x03;
    let is_keyframe = if packet_kind == PacketKind::Audio {
        true
    } else if is_simple_block {
        (flags & 0x80) != 0
    } else {
        block_group_keyframe.unwrap_or(false)
    };

    let payload = &block[vint_len + 3..];
    let frames = split_laced_frames(payload, lacing)?;
    let pts_ticks = cluster_ts + i64::from(rel_ts);
    let pts_us = (i128::from(pts_ticks) * i128::from(state.timestamp_scale_ns) / 1_000) as i64;
    let total_dur_us = block_dur_ticks
        .map(|d| (u128::from(d) * u128::from(state.timestamp_scale_ns) / 1_000) as u64)
        .or_else(|| state.default_durations_us.get(&track_id).copied())
        .unwrap_or(0);
    let per_frame_dur = if frames.is_empty() {
        0
    } else {
        total_dur_us / (frames.len() as u64)
    };

    for (idx, frame_bytes) in frames.into_iter().enumerate() {
        let frame_pts = pts_us + (idx as i64) * (per_frame_dur as i64);
        packets.push(EncodedPacket {
            track_id,
            kind: packet_kind,
            data: frame_bytes.to_vec(),
            pts_us: frame_pts,
            dts_us: frame_pts,
            duration_us: per_frame_dur,
            is_keyframe: is_keyframe && idx == 0,
        });
    }
    Ok(())
}

fn split_laced_frames(payload: &[u8], lacing: u8) -> Result<Vec<&[u8]>, MediaError> {
    if lacing == 0 {
        return Ok(vec![payload]);
    }
    if payload.is_empty() {
        return Err(MediaError::MalformedContainer(
            "empty laced WebM block".to_string(),
        ));
    }
    let frame_count = usize::from(payload[0]) + 1;
    let mut off = 1usize;
    let mut sizes = Vec::with_capacity(frame_count);

    match lacing {
        // Xiph lacing
        1 => {
            for _ in 0..(frame_count - 1) {
                let mut sz = 0usize;
                loop {
                    let b = *payload.get(off).ok_or_else(|| {
                        MediaError::MalformedContainer("truncated Xiph lace size".to_string())
                    })?;
                    off += 1;
                    sz += usize::from(b);
                    if b != 255 {
                        break;
                    }
                }
                sizes.push(sz);
            }
        }
        // Fixed-size lacing
        2 => {
            let rem = payload.len() - off;
            if !rem.is_multiple_of(frame_count) {
                return Err(MediaError::MalformedContainer(
                    "fixed-size WebM lace not divisible by frame count".to_string(),
                ));
            }
            let sz = rem / frame_count;
            let mut frames = Vec::with_capacity(frame_count);
            for i in 0..frame_count {
                let s = off + i * sz;
                frames.push(&payload[s..s + sz]);
            }
            return Ok(frames);
        }
        // EBML lacing
        3 => {
            let (first_sz, n) = read_ebml_vint_value(&payload[off..])?;
            off += n;
            let mut cur = first_sz as i64;
            sizes.push(first_sz as usize);
            for _ in 1..(frame_count - 1) {
                let (raw, m) = read_ebml_vint_value(&payload[off..])?;
                off += m;
                let bias = (1i64 << (7 * m - 1)) - 1;
                cur += (raw as i64) - bias;
                if cur < 0 {
                    return Err(MediaError::MalformedContainer(
                        "negative EBML laced frame size".to_string(),
                    ));
                }
                sizes.push(cur as usize);
            }
        }
        _ => return Ok(vec![payload]),
    }

    let mut frames = Vec::with_capacity(frame_count);
    for sz in sizes {
        let end = off.saturating_add(sz);
        if end > payload.len() {
            return Err(MediaError::MalformedContainer(
                "laced frame exceeds WebM block bounds".to_string(),
            ));
        }
        frames.push(&payload[off..end]);
        off = end;
    }
    frames.push(&payload[off..]);
    Ok(frames)
}

type EbmlElement<'a> = (u32, &'a [u8], usize);

fn read_element(buf: &[u8], offset: usize) -> Result<Option<EbmlElement<'_>>, MediaError> {
    if offset >= buf.len() {
        return Ok(None);
    }
    let (id, id_len) = read_ebml_id(&buf[offset..])?;
    let size_pos = offset + id_len;
    if size_pos >= buf.len() {
        return Err(MediaError::MalformedContainer(
            "truncated EBML element size".to_string(),
        ));
    }
    let (size_opt, size_len) = read_ebml_size(&buf[size_pos..])?;
    let body_start = size_pos + size_len;
    let body_end = match size_opt {
        Some(sz) => {
            let end = body_start.saturating_add(sz);
            if end > buf.len() {
                return Err(MediaError::MalformedContainer(
                    "EBML element size exceeds input buffer".to_string(),
                ));
            }
            end
        }
        // Unknown-size master element (streaming Segment or Cluster): extends to end of slice.
        None => buf.len(),
    };
    Ok(Some((id, &buf[body_start..body_end], body_end)))
}

fn read_ebml_id(slice: &[u8]) -> Result<(u32, usize), MediaError> {
    let first = *slice
        .first()
        .ok_or_else(|| MediaError::MalformedContainer("empty EBML id".to_string()))?;
    let width = first.leading_zeros() as usize + 1;
    if width == 0 || width > 4 || width > slice.len() {
        return Err(MediaError::MalformedContainer(
            "invalid EBML element ID".to_string(),
        ));
    }
    let mut id = 0u32;
    for &b in &slice[..width] {
        id = (id << 8) | u32::from(b);
    }
    Ok((id, width))
}

fn read_ebml_size(slice: &[u8]) -> Result<(Option<usize>, usize), MediaError> {
    let first = *slice
        .first()
        .ok_or_else(|| MediaError::MalformedContainer("empty EBML size".to_string()))?;
    let width = first.leading_zeros() as usize + 1;
    if width == 0 || width > 8 || width > slice.len() {
        return Err(MediaError::MalformedContainer(
            "invalid EBML element size VINT".to_string(),
        ));
    }
    let mask = if width == 8 {
        0u8
    } else {
        (1u8 << (8 - width)) - 1
    };
    let mut val = u64::from(first & mask);
    let mut all_ones = (first & mask) == mask;
    for &b in &slice[1..width] {
        val = (val << 8) | u64::from(b);
        if b != 0xff {
            all_ones = false;
        }
    }
    if all_ones {
        return Ok((None, width));
    }
    let sz = usize::try_from(val).map_err(|_| {
        MediaError::MalformedContainer("EBML element size exceeds usize".to_string())
    })?;
    Ok((Some(sz), width))
}

fn read_ebml_vint_value(slice: &[u8]) -> Result<(u64, usize), MediaError> {
    let (opt, width) = read_ebml_size(slice)?;
    Ok((opt.unwrap_or(0) as u64, width))
}

fn read_be_uint(bytes: &[u8]) -> u64 {
    let mut v = 0u64;
    for &b in bytes.iter().take(8) {
        v = (v << 8) | u64::from(b);
    }
    v
}

fn read_be_float(bytes: &[u8]) -> Option<f64> {
    match bytes.len() {
        4 => Some(f64::from(f32::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3],
        ]))),
        8 => Some(f64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Spec-compliant WebM builder helpers for tests & MSE fixtures
// ---------------------------------------------------------------------------

/// Input frame descriptor for [`mux_webm_cluster`].
#[derive(Debug, Clone)]
pub struct WebmBlockInput<'a> {
    pub track_id: u32,
    pub rel_timestamp_ms: i16,
    pub is_keyframe: bool,
    pub data: &'a [u8],
}

fn write_ebml_id(out: &mut Vec<u8>, id: u32) {
    let bytes = id.to_be_bytes();
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(3);
    out.extend_from_slice(&bytes[start..]);
}

fn write_ebml_vint(out: &mut Vec<u8>, value: u64) {
    if value < 0x7f {
        out.push((value as u8) | 0x80);
    } else if value < 0x3fff {
        let v = (value as u16) | 0x4000;
        out.extend_from_slice(&v.to_be_bytes());
    } else if value < 0x1f_ffff {
        let v = (value as u32) | 0x20_0000;
        let b = v.to_be_bytes();
        out.extend_from_slice(&b[1..4]);
    } else {
        let v = (value as u32) | 0x1000_0000;
        out.extend_from_slice(&v.to_be_bytes());
    }
}

fn ebml_elem(id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    write_ebml_id(&mut out, id);
    write_ebml_vint(&mut out, payload.len() as u64);
    out.extend_from_slice(payload);
    out
}

fn ebml_uint(id: u32, value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(7);
    ebml_elem(id, &bytes[start..])
}

/// Builds a spec-compliant WebM initialization segment (`EBML` + unknown-size `Segment` header + `Info` + `Tracks`).
pub fn mux_webm_init_segment(tracks: &[TrackInfo], duration_ms: Option<f64>) -> Vec<u8> {
    let mut ebml_body = Vec::new();
    ebml_body.extend_from_slice(&ebml_uint(0x4286, 1)); // EBMLVersion
    ebml_body.extend_from_slice(&ebml_uint(0x42F7, 1)); // EBMLReadVersion
    ebml_body.extend_from_slice(&ebml_uint(0x42F2, 4)); // EBMLMaxIDLength
    ebml_body.extend_from_slice(&ebml_uint(0x42F3, 8)); // EBMLMaxSizeLength
    ebml_body.extend_from_slice(&ebml_elem(0x4282, b"webm")); // DocType
    ebml_body.extend_from_slice(&ebml_uint(0x4287, 4)); // DocTypeVersion
    ebml_body.extend_from_slice(&ebml_uint(0x4285, 2)); // DocTypeReadVersion
    let ebml = ebml_elem(ID_EBML, &ebml_body);

    let mut info_body = Vec::new();
    info_body.extend_from_slice(&ebml_uint(ID_TIMESTAMP_SCALE, 1_000_000));
    if let Some(dur) = duration_ms {
        info_body.extend_from_slice(&ebml_elem(ID_DURATION, &dur.to_be_bytes()));
    }
    let info = ebml_elem(ID_INFO, &info_body);

    let mut tracks_body = Vec::new();
    for track in tracks {
        let mut entry = Vec::new();
        entry.extend_from_slice(&ebml_uint(ID_TRACK_NUMBER, u64::from(track.id)));
        match &track.kind {
            TrackKind::Video(v) => {
                entry.extend_from_slice(&ebml_uint(ID_TRACK_TYPE, 1));
                let codec_id: &[u8] = match v.codec {
                    VideoCodec::Vp8 => b"V_VP8",
                    VideoCodec::Vp9 => b"V_VP9",
                    VideoCodec::Av1 => b"V_AV1",
                    VideoCodec::H264 => b"V_MPEG4/ISO/AVC",
                };
                entry.extend_from_slice(&ebml_elem(ID_CODEC_ID, codec_id));
                let mut vbody = Vec::new();
                vbody.extend_from_slice(&ebml_uint(ID_PIXEL_WIDTH, u64::from(v.width)));
                vbody.extend_from_slice(&ebml_uint(ID_PIXEL_HEIGHT, u64::from(v.height)));
                entry.extend_from_slice(&ebml_elem(ID_VIDEO, &vbody));
            }
            TrackKind::Audio(a) => {
                entry.extend_from_slice(&ebml_uint(ID_TRACK_TYPE, 2));
                let codec_id: &[u8] = match a.codec {
                    AudioCodec::Opus => b"A_OPUS",
                    AudioCodec::Vorbis => b"A_VORBIS",
                    AudioCodec::Aac => b"A_AAC",
                    AudioCodec::Flac => b"A_FLAC",
                    AudioCodec::Mp3 => b"A_MPEG/L3",
                    AudioCodec::Pcm => b"A_PCM/INT/LIT",
                };
                entry.extend_from_slice(&ebml_elem(ID_CODEC_ID, codec_id));
                let mut abody = Vec::new();
                abody.extend_from_slice(&ebml_elem(
                    ID_SAMPLING_FREQUENCY,
                    &(a.sample_rate as f32).to_be_bytes(),
                ));
                abody.extend_from_slice(&ebml_uint(ID_CHANNELS, u64::from(a.channels)));
                entry.extend_from_slice(&ebml_elem(ID_AUDIO, &abody));
            }
        }
        if !track.extra_data.is_empty() {
            entry.extend_from_slice(&ebml_elem(ID_CODEC_PRIVATE, &track.extra_data));
        }
        tracks_body.extend_from_slice(&ebml_elem(ID_TRACK_ENTRY, &entry));
    }
    let tracks_el = ebml_elem(ID_TRACKS, &tracks_body);

    // Emit Segment with unknown size (`0x01FFFFFFFFFFFFFF`) so clusters can be appended cleanly.
    let mut out = Vec::with_capacity(ebml.len() + 12 + info.len() + tracks_el.len());
    out.extend_from_slice(&ebml);
    write_ebml_id(&mut out, ID_SEGMENT);
    out.extend_from_slice(&[0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    out.extend_from_slice(&info);
    out.extend_from_slice(&tracks_el);
    out
}

/// Builds a spec-compliant WebM `Cluster` containing `SimpleBlock` entries.
pub fn mux_webm_cluster(cluster_timestamp_ms: u64, blocks: &[WebmBlockInput<'_>]) -> Vec<u8> {
    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&ebml_uint(ID_CLUSTER_TIMESTAMP, cluster_timestamp_ms));
    for blk in blocks {
        let mut payload = Vec::with_capacity(4 + blk.data.len());
        write_ebml_vint(&mut payload, u64::from(blk.track_id));
        payload.extend_from_slice(&blk.rel_timestamp_ms.to_be_bytes());
        payload.push(if blk.is_keyframe { 0x80 } else { 0x00 });
        payload.extend_from_slice(blk.data);
        cluster_body.extend_from_slice(&ebml_elem(ID_SIMPLE_BLOCK, &payload));
    }
    ebml_elem(ID_CLUSTER, &cluster_body)
}
