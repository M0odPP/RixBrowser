//! Pure-Rust ISO BMFF (standard MP4 and fragmented fMP4 / MSE) demuxer and segment muxer.

use std::collections::HashMap;

use crate::types::{
    AudioCodec, AudioTrackConfig, ContainerFormat, DemuxedMedia, EncodedPacket, MediaError,
    MseSample, PacketKind, TrackInfo, TrackKind, VideoCodec, VideoTrackConfig,
};

/// Track defaults from `mvex/trex` and `moov/trak` persisted across fMP4 media segments.
#[derive(Debug, Clone)]
pub struct Mp4TrackState {
    pub info: TrackInfo,
    pub default_sample_description_index: u32,
    pub default_sample_duration: u32,
    pub default_sample_size: u32,
    pub default_sample_flags: u32,
    pub next_decode_time: u64,
    /// Raw ISO BMFF `edts/elst` entries `(segment_duration_mvhd_ticks, media_time_mdhd_ticks)`.
    pub elst_entries: Vec<(u64, i64)>,
    /// Precomputed presentation offset in seconds derived from leading empty/trim `elst` edits.
    pub elst_offset_s: f64,
}

/// Stateful initialization header state for incremental MSE fMP4 segment parsing.
#[derive(Debug, Clone, Default)]
pub struct Mp4InitState {
    pub movie_timescale: u32,
    pub duration_us: Option<u64>,
    pub init_duration_s: Option<f64>,
    pub tracks: HashMap<u32, Mp4TrackState>,
    pub track_order: Vec<u32>,
}

/// Structural classification of an incremental fMP4 byte buffer for MSE parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mp4BufferStatus {
    Complete {
        has_moov: bool,
        has_moof: bool,
    },
    CompletePrefix {
        split_at: usize,
        has_moov: bool,
        has_moof: bool,
        trailing_in_media: bool,
    },
    Partial {
        in_media_segment: bool,
    },
    Corrupt,
}

fn is_valid_mp4_fourcc(kind: &[u8; 4]) -> bool {
    kind.iter().all(|&b| (0x20..=0x7E).contains(&b))
}

/// Inspects top-level ISO BMFF boxes in `buf` to distinguish complete segments, partial streamed
/// appends (including split `moof`/`mdat` pairs), and corrupt byte streams.
pub fn inspect_mp4_buffer(buf: &[u8]) -> Mp4BufferStatus {
    if buf.is_empty() {
        return Mp4BufferStatus::Complete {
            has_moov: false,
            has_moof: false,
        };
    }
    let mut off = 0usize;
    let mut last_complete_end = 0usize;
    let mut prefix_has_moov = false;
    let mut prefix_has_moof = false;
    let mut has_moov = false;
    let mut has_moof = false;
    let mut pending_moof_without_mdat = false;
    let mut saw_known_box = false;

    while off < buf.len() {
        let rem = buf.len() - off;
        if rem < 8 {
            if last_complete_end > 0 && (prefix_has_moov || prefix_has_moof) {
                return Mp4BufferStatus::CompletePrefix {
                    split_at: last_complete_end,
                    has_moov: prefix_has_moov,
                    has_moof: prefix_has_moof,
                    trailing_in_media: pending_moof_without_mdat,
                };
            }
            return if saw_known_box {
                Mp4BufferStatus::Partial {
                    in_media_segment: has_moof || pending_moof_without_mdat,
                }
            } else {
                Mp4BufferStatus::Corrupt
            };
        }
        let size32 = u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        let kind = [buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]];
        if !is_valid_mp4_fourcc(&kind) {
            return Mp4BufferStatus::Corrupt;
        }
        let known = matches!(
            &kind,
            b"ftyp"
                | b"styp"
                | b"moov"
                | b"moof"
                | b"mdat"
                | b"sidx"
                | b"free"
                | b"skip"
                | b"uuid"
                | b"mfra"
                | b"emsg"
                | b"prft"
                | b"pdin"
                | b"meta"
                | b"ssix"
                | b"wide"
        );
        if !known {
            return Mp4BufferStatus::Corrupt;
        }
        saw_known_box = true;

        let (hdr_len, box_size) = if size32 == 1 {
            if rem < 16 {
                let trailing_in_media =
                    pending_moof_without_mdat || &kind == b"moof" || &kind == b"mdat";
                if last_complete_end > 0 && (prefix_has_moov || prefix_has_moof) {
                    return Mp4BufferStatus::CompletePrefix {
                        split_at: last_complete_end,
                        has_moov: prefix_has_moov,
                        has_moof: prefix_has_moof,
                        trailing_in_media,
                    };
                }
                return Mp4BufferStatus::Partial {
                    in_media_segment: has_moof || trailing_in_media,
                };
            }
            let sz64 = u64::from_be_bytes([
                buf[off + 8],
                buf[off + 9],
                buf[off + 10],
                buf[off + 11],
                buf[off + 12],
                buf[off + 13],
                buf[off + 14],
                buf[off + 15],
            ]);
            let Ok(sz) = usize::try_from(sz64) else {
                return Mp4BufferStatus::Corrupt;
            };
            (16usize, sz)
        } else if size32 == 0 {
            (8usize, rem)
        } else {
            (8usize, size32 as usize)
        };

        if box_size < hdr_len {
            return Mp4BufferStatus::Corrupt;
        }
        if off.saturating_add(box_size) > buf.len() {
            let trailing_in_media =
                pending_moof_without_mdat || &kind == b"moof" || &kind == b"mdat";
            if last_complete_end > 0 && (prefix_has_moov || prefix_has_moof) {
                return Mp4BufferStatus::CompletePrefix {
                    split_at: last_complete_end,
                    has_moov: prefix_has_moov,
                    has_moof: prefix_has_moof,
                    trailing_in_media,
                };
            }
            return Mp4BufferStatus::Partial {
                in_media_segment: has_moof || trailing_in_media,
            };
        }
        match &kind {
            b"moov" => {
                has_moov = true;
                if !pending_moof_without_mdat {
                    last_complete_end = off + box_size;
                    prefix_has_moov = true;
                }
            }
            b"moof" => {
                has_moof = true;
                pending_moof_without_mdat = true;
            }
            b"mdat" => {
                pending_moof_without_mdat = false;
                last_complete_end = off + box_size;
                if has_moof {
                    prefix_has_moof = true;
                }
            }
            _ => {
                if !pending_moof_without_mdat {
                    last_complete_end = off + box_size;
                }
            }
        }
        off += box_size;
    }

    if pending_moof_without_mdat {
        if last_complete_end > 0 && (prefix_has_moov || prefix_has_moof) {
            return Mp4BufferStatus::CompletePrefix {
                split_at: last_complete_end,
                has_moov: prefix_has_moov,
                has_moof: prefix_has_moof,
                trailing_in_media: true,
            };
        }
        return Mp4BufferStatus::Partial {
            in_media_segment: true,
        };
    }
    Mp4BufferStatus::Complete { has_moov, has_moof }
}

impl Mp4InitState {
    /// Returns all initialized tracks in container declaration order.
    pub fn tracks(&self) -> Vec<TrackInfo> {
        self.track_order
            .iter()
            .filter_map(|id| self.tracks.get(id).map(|t| t.info.clone()))
            .collect()
    }
}

/// Demuxes a complete standard or fragmented MP4 byte buffer.
pub fn demux_mp4(bytes: &[u8]) -> Result<DemuxedMedia, MediaError> {
    let mut state = Mp4InitState::default();
    let mut packets = Vec::new();
    let mut saw_moov = false;
    let mut pos = 0usize;

    while pos < bytes.len() {
        let Some(bx) = read_box_header(bytes, pos)? else {
            break;
        };
        let body = &bytes[bx.body_start..bx.box_end];
        match &bx.kind {
            b"moov" => {
                saw_moov = true;
                parse_moov(bytes, body, &mut state, &mut packets)?;
            }
            b"moof" => {
                parse_moof(bytes, bx.box_start, body, &mut state, &mut packets)?;
            }
            _ => {}
        }
        pos = bx.box_end;
    }

    if !saw_moov {
        return Err(MediaError::MalformedContainer(
            "MP4 stream is missing a `moov` box".to_string(),
        ));
    }

    packets.sort_by_key(|p| (p.dts_us, p.pts_us, p.track_id));

    Ok(DemuxedMedia {
        container: ContainerFormat::Mp4,
        duration_us: state.duration_us,
        tracks: state.tracks(),
        packets,
    })
}

/// Parses an MSE fMP4 initialization segment (`ftyp` + `moov`) into reusable [`Mp4InitState`].
pub fn parse_mp4_init_segment(bytes: &[u8]) -> Result<Mp4InitState, MediaError> {
    let mut state = Mp4InitState::default();
    let mut dummy_packets = Vec::new();
    let mut saw_moov = false;
    let mut pos = 0usize;
    while pos < bytes.len() {
        let Some(bx) = read_box_header(bytes, pos)? else {
            break;
        };
        if &bx.kind == b"moov" {
            saw_moov = true;
            parse_moov(
                bytes,
                &bytes[bx.body_start..bx.box_end],
                &mut state,
                &mut dummy_packets,
            )?;
        }
        pos = bx.box_end;
    }
    if !saw_moov {
        return Err(MediaError::MalformedContainer(
            "MP4 init segment does not contain a `moov` box".to_string(),
        ));
    }
    Ok(state)
}

/// Parses one or more fMP4 media segments (`moof` + `mdat` pairs) using an existing [`Mp4InitState`].
/// Also accepts combined init+media buffers (updating `state` whenever a `moov` box appears).
pub fn parse_mp4_media_segments(
    bytes: &[u8],
    state: &mut Mp4InitState,
) -> Result<Vec<EncodedPacket>, MediaError> {
    let mut packets = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let Some(bx) = read_box_header(bytes, pos)? else {
            break;
        };
        let body = &bytes[bx.body_start..bx.box_end];
        match &bx.kind {
            b"moov" => {
                *state = Mp4InitState::default();
                parse_moov(bytes, body, state, &mut packets)?;
            }
            b"moof" => {
                parse_moof(bytes, bx.box_start, body, state, &mut packets)?;
            }
            _ => {}
        }
        pos = bx.box_end;
    }
    Ok(packets)
}

/// Parses one or more fMP4 media segments into high-precision [`MseSample`]s (preserving
/// sub-microsecond durations, `edts/elst` edit-list offsets, and per-frame video dimensions).
pub fn parse_mp4_mse_segments(
    bytes: &[u8],
    state: &mut Mp4InitState,
) -> Result<Vec<MseSample>, MediaError> {
    let mut samples = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let Some(bx) = read_box_header(bytes, pos)? else {
            break;
        };
        let body = &bytes[bx.body_start..bx.box_end];
        match &bx.kind {
            b"moov" => {
                *state = Mp4InitState::default();
                let mut stbl_pkts = Vec::new();
                parse_moov(bytes, body, state, &mut stbl_pkts)?;
                for pkt in stbl_pkts {
                    let vdims = state
                        .tracks
                        .get(&pkt.track_id)
                        .and_then(|t| match &t.info.kind {
                            TrackKind::Video(v) => Some((v.width, v.height)),
                            TrackKind::Audio(_) => None,
                        });
                    let elst_off = state
                        .tracks
                        .get(&pkt.track_id)
                        .map(|t| t.elst_offset_s)
                        .unwrap_or(0.0);
                    samples.push(MseSample {
                        track_id: pkt.track_id,
                        kind: pkt.kind,
                        data: pkt.data,
                        pts_s: pkt.pts_us as f64 / 1_000_000.0 + elst_off,
                        dts_s: pkt.dts_us as f64 / 1_000_000.0,
                        duration_s: pkt.duration_us as f64 / 1_000_000.0,
                        is_keyframe: pkt.is_keyframe,
                        video_dims: vdims,
                    });
                }
            }
            b"moof" => {
                parse_moof_mse(bytes, bx.box_start, body, state, &mut samples)?;
            }
            _ => {}
        }
        pos = bx.box_end;
    }
    Ok(samples)
}

#[derive(Debug, Clone, Copy)]
struct BoxSlice {
    kind: [u8; 4],
    box_start: usize,
    body_start: usize,
    box_end: usize,
}

fn read_box_header(buf: &[u8], offset: usize) -> Result<Option<BoxSlice>, MediaError> {
    if offset >= buf.len() {
        return Ok(None);
    }
    if buf.len() - offset < 8 {
        return Err(MediaError::MalformedContainer(
            "truncated MP4 box header".to_string(),
        ));
    }
    let size32 = u32::from_be_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ]);
    let kind = [
        buf[offset + 4],
        buf[offset + 5],
        buf[offset + 6],
        buf[offset + 7],
    ];

    let (header_len, box_size) = if size32 == 1 {
        if buf.len() - offset < 16 {
            return Err(MediaError::MalformedContainer(
                "truncated 64-bit MP4 box header".to_string(),
            ));
        }
        let size64 = u64::from_be_bytes([
            buf[offset + 8],
            buf[offset + 9],
            buf[offset + 10],
            buf[offset + 11],
            buf[offset + 12],
            buf[offset + 13],
            buf[offset + 14],
            buf[offset + 15],
        ]);
        let size_usize = usize::try_from(size64).map_err(|_| {
            MediaError::MalformedContainer("MP4 box size exceeds addressable range".to_string())
        })?;
        (16usize, size_usize)
    } else if size32 == 0 {
        (8usize, buf.len() - offset)
    } else {
        (8usize, size32 as usize)
    };

    if box_size < header_len || offset.saturating_add(box_size) > buf.len() {
        return Err(MediaError::MalformedContainer(format!(
            "MP4 box `{}` has invalid size {box_size}",
            String::from_utf8_lossy(&kind)
        )));
    }

    Ok(Some(BoxSlice {
        kind,
        box_start: offset,
        body_start: offset + header_len,
        box_end: offset + box_size,
    }))
}

fn read_u16(b: &[u8], off: usize) -> Result<u16, MediaError> {
    let slice = b
        .get(off..off + 2)
        .ok_or_else(|| MediaError::MalformedContainer("unexpected EOF reading u16".to_string()))?;
    Ok(u16::from_be_bytes([slice[0], slice[1]]))
}

fn read_u32(b: &[u8], off: usize) -> Result<u32, MediaError> {
    let slice = b
        .get(off..off + 4)
        .ok_or_else(|| MediaError::MalformedContainer("unexpected EOF reading u32".to_string()))?;
    Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_i32(b: &[u8], off: usize) -> Result<i32, MediaError> {
    Ok(read_u32(b, off)? as i32)
}

fn read_u64(b: &[u8], off: usize) -> Result<u64, MediaError> {
    let slice = b
        .get(off..off + 8)
        .ok_or_else(|| MediaError::MalformedContainer("unexpected EOF reading u64".to_string()))?;
    Ok(u64::from_be_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

fn ticks_to_us_u64(ticks: u64, timescale: u32) -> u64 {
    if timescale == 0 {
        return 0;
    }
    ((u128::from(ticks) * 1_000_000u128) / u128::from(timescale)) as u64
}

fn ticks_to_us_i64(ticks: i64, timescale: u32) -> i64 {
    if timescale == 0 {
        return 0;
    }
    ((i128::from(ticks) * 1_000_000i128) / i128::from(timescale)) as i64
}

fn parse_moov(
    full_file: &[u8],
    moov_body: &[u8],
    state: &mut Mp4InitState,
    packets: &mut Vec<EncodedPacket>,
) -> Result<(), MediaError> {
    let mut trex_defaults: HashMap<u32, (u32, u32, u32, u32)> = HashMap::new();
    let mut mehd_duration_ticks: Option<u64> = None;
    let mut pos = 0usize;
    while pos < moov_body.len() {
        let Some(bx) = read_box_header(moov_body, pos)? else {
            break;
        };
        let body = &moov_body[bx.body_start..bx.box_end];
        match &bx.kind {
            b"mvhd" => {
                if !body.is_empty() {
                    let version = body[0];
                    if version == 0 && body.len() >= 20 {
                        let ts = read_u32(body, 12)?;
                        let dur = u64::from(read_u32(body, 16)?);
                        state.movie_timescale = ts;
                        if ts > 0 && dur > 0 && dur != u64::from(u32::MAX) {
                            state.duration_us = Some(ticks_to_us_u64(dur, ts));
                            state.init_duration_s = Some(dur as f64 / f64::from(ts));
                        }
                    } else if version == 1 && body.len() >= 32 {
                        let ts = read_u32(body, 20)?;
                        let dur = read_u64(body, 24)?;
                        state.movie_timescale = ts;
                        if ts > 0 && dur > 0 && dur != u64::MAX {
                            state.duration_us = Some(ticks_to_us_u64(dur, ts));
                            state.init_duration_s = Some(dur as f64 / f64::from(ts));
                        }
                    }
                }
            }
            b"mvex" => {
                parse_mvex(body, &mut trex_defaults, &mut mehd_duration_ticks)?;
            }
            b"trak" => {
                if let Some((track_info, elst_entries, stbl_packets)) = parse_trak(full_file, body)?
                {
                    let tid = track_info.id;
                    let (desc_idx, def_dur, def_size, def_flags) =
                        trex_defaults.get(&tid).copied().unwrap_or((1, 0, 0, 0));
                    if !state.tracks.contains_key(&tid) {
                        state.track_order.push(tid);
                    }
                    state.tracks.insert(
                        tid,
                        Mp4TrackState {
                            info: track_info,
                            default_sample_description_index: desc_idx,
                            default_sample_duration: def_dur,
                            default_sample_size: def_size,
                            default_sample_flags: def_flags,
                            next_decode_time: 0,
                            elst_entries,
                            elst_offset_s: 0.0,
                        },
                    );
                    packets.extend(stbl_packets);
                }
            }
            _ => {}
        }
        pos = bx.box_end;
    }

    if let Some(mehd_ticks) = mehd_duration_ticks {
        if state.movie_timescale > 0 && mehd_ticks > 0 {
            state.duration_us = Some(ticks_to_us_u64(mehd_ticks, state.movie_timescale));
            state.init_duration_s = Some(mehd_ticks as f64 / f64::from(state.movie_timescale));
        }
    }

    // Apply `trex` defaults in case `mvex` appeared after `trak` (the standard order in MP4!).
    for (tid, (desc_idx, def_dur, def_size, def_flags)) in trex_defaults {
        if let Some(track) = state.tracks.get_mut(&tid) {
            track.default_sample_description_index = desc_idx;
            track.default_sample_duration = def_dur;
            track.default_sample_size = def_size;
            track.default_sample_flags = def_flags;
        }
    }

    // Compute per-track `elst` edit-list presentation offsets once `movie_timescale` is known.
    let mvhd_ts = f64::from(state.movie_timescale.max(1));
    for track in state.tracks.values_mut() {
        let mdhd_ts = f64::from(track.info.timescale.max(1));
        let mut empty_s = 0.0f64;
        let mut trim_s = 0.0f64;
        let mut saw_media = false;
        for &(seg_dur, media_time) in &track.elst_entries {
            if media_time == -1 && !saw_media {
                empty_s += (seg_dur as f64) / mvhd_ts;
            } else if media_time >= 0 && !saw_media {
                trim_s = (media_time as f64) / mdhd_ts;
                saw_media = true;
            }
        }
        track.elst_offset_s = empty_s - trim_s;
    }

    Ok(())
}

fn parse_mvex(
    mvex_body: &[u8],
    trex_defaults: &mut HashMap<u32, (u32, u32, u32, u32)>,
    mehd_duration_ticks: &mut Option<u64>,
) -> Result<(), MediaError> {
    let mut pos = 0usize;
    while pos < mvex_body.len() {
        let Some(bx) = read_box_header(mvex_body, pos)? else {
            break;
        };
        let body = &mvex_body[bx.body_start..bx.box_end];
        match &bx.kind {
            b"mehd" if !body.is_empty() => {
                let version = body[0];
                if version == 0 && body.len() >= 8 {
                    let dur = u64::from(read_u32(body, 4)?);
                    if dur > 0 && dur != u64::from(u32::MAX) {
                        *mehd_duration_ticks = Some(dur);
                    }
                } else if version == 1 && body.len() >= 12 {
                    let dur = read_u64(body, 4)?;
                    if dur > 0 && dur != u64::MAX {
                        *mehd_duration_ticks = Some(dur);
                    }
                }
            }
            b"trex" if body.len() >= 24 => {
                let track_id = read_u32(body, 4)?;
                let desc_idx = read_u32(body, 8)?;
                let def_dur = read_u32(body, 12)?;
                let def_size = read_u32(body, 16)?;
                let def_flags = read_u32(body, 20)?;
                trex_defaults.insert(track_id, (desc_idx, def_dur, def_size, def_flags));
            }
            _ => {}
        }
        pos = bx.box_end;
    }
    Ok(())
}

#[derive(Default)]
struct StblTables {
    kind: Option<TrackKind>,
    extra_data: Vec<u8>,
    stts: Vec<(u32, u32)>,
    ctts: Vec<(u32, i32)>,
    stsc: Vec<(u32, u32, u32)>,
    stsz_default: u32,
    stsz_count: u32,
    stsz_sizes: Vec<u32>,
    chunk_offsets: Vec<u64>,
    stss: Vec<u32>,
    has_stss: bool,
}

type ParsedTrak = (TrackInfo, Vec<(u64, i64)>, Vec<EncodedPacket>);

fn parse_trak(full_file: &[u8], trak_body: &[u8]) -> Result<Option<ParsedTrak>, MediaError> {
    let mut track_id = 0u32;
    let mut timescale = 1_000u32;
    let mut duration_us = None;
    let mut language = None;
    let mut handler = [0u8; 4];
    let mut stbl = StblTables::default();
    let mut elst_entries: Vec<(u64, i64)> = Vec::new();

    let mut pos = 0usize;
    while pos < trak_body.len() {
        let Some(bx) = read_box_header(trak_body, pos)? else {
            break;
        };
        let body = &trak_body[bx.body_start..bx.box_end];
        match &bx.kind {
            b"tkhd" => {
                if !body.is_empty() {
                    let version = body[0];
                    if version == 0 && body.len() >= 16 {
                        track_id = read_u32(body, 12)?;
                    } else if version == 1 && body.len() >= 24 {
                        track_id = read_u32(body, 20)?;
                    }
                }
            }
            b"edts" => {
                parse_edts(body, &mut elst_entries)?;
            }
            b"mdia" => {
                parse_mdia(
                    body,
                    &mut timescale,
                    &mut duration_us,
                    &mut language,
                    &mut handler,
                    &mut stbl,
                )?;
            }
            _ => {}
        }
        pos = bx.box_end;
    }

    let Some(kind) = stbl.kind.clone() else {
        return Ok(None);
    };
    if track_id == 0 {
        track_id = 1;
    }
    let packet_kind = match &kind {
        TrackKind::Video(_) => PacketKind::Video,
        TrackKind::Audio(_) => PacketKind::Audio,
    };

    let packets = extract_stbl_packets(full_file, track_id, packet_kind, timescale, &stbl)?;
    let info = TrackInfo {
        id: track_id,
        kind,
        timescale,
        duration_us,
        language,
        extra_data: stbl.extra_data,
    };
    Ok(Some((info, elst_entries, packets)))
}

fn parse_edts(edts_body: &[u8], out: &mut Vec<(u64, i64)>) -> Result<(), MediaError> {
    let mut pos = 0usize;
    while pos < edts_body.len() {
        let Some(bx) = read_box_header(edts_body, pos)? else {
            break;
        };
        if &bx.kind == b"elst" {
            let body = &edts_body[bx.body_start..bx.box_end];
            if body.len() >= 8 {
                let version = body[0];
                let count = read_u32(body, 4)? as usize;
                let mut off = 8usize;
                for _ in 0..count {
                    if version == 0 {
                        if off + 12 > body.len() {
                            break;
                        }
                        let seg_dur = u64::from(read_u32(body, off)?);
                        let media_time = i64::from(read_i32(body, off + 4)?);
                        out.push((seg_dur, media_time));
                        off += 12;
                    } else if version == 1 {
                        if off + 20 > body.len() {
                            break;
                        }
                        let seg_dur = read_u64(body, off)?;
                        let media_time = read_u64(body, off + 8)? as i64;
                        out.push((seg_dur, media_time));
                        off += 20;
                    } else {
                        break;
                    }
                }
            }
        }
        pos = bx.box_end;
    }
    Ok(())
}

fn parse_mdia(
    mdia_body: &[u8],
    timescale: &mut u32,
    duration_us: &mut Option<u64>,
    language: &mut Option<String>,
    handler: &mut [u8; 4],
    stbl: &mut StblTables,
) -> Result<(), MediaError> {
    let mut pos = 0usize;
    while pos < mdia_body.len() {
        let Some(bx) = read_box_header(mdia_body, pos)? else {
            break;
        };
        let body = &mdia_body[bx.body_start..bx.box_end];
        match &bx.kind {
            b"mdhd" => {
                if !body.is_empty() {
                    let version = body[0];
                    if version == 0 && body.len() >= 22 {
                        *timescale = read_u32(body, 12)?;
                        let dur = u64::from(read_u32(body, 16)?);
                        if *timescale > 0 && dur > 0 && dur != u64::from(u32::MAX) {
                            *duration_us = Some(ticks_to_us_u64(dur, *timescale));
                        }
                        *language = decode_iso639(read_u16(body, 20)?);
                    } else if version == 1 && body.len() >= 34 {
                        *timescale = read_u32(body, 20)?;
                        let dur = read_u64(body, 24)?;
                        if *timescale > 0 && dur > 0 && dur != u64::MAX {
                            *duration_us = Some(ticks_to_us_u64(dur, *timescale));
                        }
                        *language = decode_iso639(read_u16(body, 32)?);
                    }
                }
            }
            b"hdlr" => {
                if body.len() >= 12 {
                    *handler = [body[8], body[9], body[10], body[11]];
                }
            }
            b"minf" => {
                let mut mpos = 0usize;
                while mpos < body.len() {
                    let Some(mbx) = read_box_header(body, mpos)? else {
                        break;
                    };
                    if &mbx.kind == b"stbl" {
                        parse_stbl(&body[mbx.body_start..mbx.box_end], stbl)?;
                    }
                    mpos = mbx.box_end;
                }
            }
            _ => {}
        }
        pos = bx.box_end;
    }
    Ok(())
}

fn decode_iso639(packed: u16) -> Option<String> {
    let c1 = ((packed >> 10) & 0x1f) as u8;
    let c2 = ((packed >> 5) & 0x1f) as u8;
    let c3 = (packed & 0x1f) as u8;
    if c1 == 0 || c2 == 0 || c3 == 0 {
        return None;
    }
    Some(
        [c1 + 0x60, c2 + 0x60, c3 + 0x60]
            .iter()
            .map(|&c| c as char)
            .collect(),
    )
}

fn parse_stbl(stbl_body: &[u8], out: &mut StblTables) -> Result<(), MediaError> {
    let mut pos = 0usize;
    while pos < stbl_body.len() {
        let Some(bx) = read_box_header(stbl_body, pos)? else {
            break;
        };
        let body = &stbl_body[bx.body_start..bx.box_end];
        match &bx.kind {
            b"stsd" => parse_stsd(body, out)?,
            b"stts" => {
                if body.len() >= 8 {
                    let count = read_u32(body, 4)? as usize;
                    let mut off = 8;
                    for _ in 0..count {
                        if off + 8 > body.len() {
                            break;
                        }
                        out.stts
                            .push((read_u32(body, off)?, read_u32(body, off + 4)?));
                        off += 8;
                    }
                }
            }
            b"ctts" => {
                if body.len() >= 8 {
                    let count = read_u32(body, 4)? as usize;
                    let mut off = 8;
                    for _ in 0..count {
                        if off + 8 > body.len() {
                            break;
                        }
                        out.ctts
                            .push((read_u32(body, off)?, read_i32(body, off + 4)?));
                        off += 8;
                    }
                }
            }
            b"stsc" => {
                if body.len() >= 8 {
                    let count = read_u32(body, 4)? as usize;
                    let mut off = 8;
                    for _ in 0..count {
                        if off + 12 > body.len() {
                            break;
                        }
                        out.stsc.push((
                            read_u32(body, off)?,
                            read_u32(body, off + 4)?,
                            read_u32(body, off + 8)?,
                        ));
                        off += 12;
                    }
                }
            }
            b"stsz" => {
                if body.len() >= 12 {
                    out.stsz_default = read_u32(body, 4)?;
                    out.stsz_count = read_u32(body, 8)?;
                    if out.stsz_default == 0 {
                        let mut off = 12;
                        for _ in 0..out.stsz_count {
                            if off + 4 > body.len() {
                                break;
                            }
                            out.stsz_sizes.push(read_u32(body, off)?);
                            off += 4;
                        }
                    }
                }
            }
            b"stco" => {
                if body.len() >= 8 {
                    let count = read_u32(body, 4)? as usize;
                    let mut off = 8;
                    for _ in 0..count {
                        if off + 4 > body.len() {
                            break;
                        }
                        out.chunk_offsets.push(u64::from(read_u32(body, off)?));
                        off += 4;
                    }
                }
            }
            b"co64" => {
                if body.len() >= 8 {
                    let count = read_u32(body, 4)? as usize;
                    let mut off = 8;
                    for _ in 0..count {
                        if off + 8 > body.len() {
                            break;
                        }
                        out.chunk_offsets.push(read_u64(body, off)?);
                        off += 8;
                    }
                }
            }
            b"stss" => {
                out.has_stss = true;
                if body.len() >= 8 {
                    let count = read_u32(body, 4)? as usize;
                    let mut off = 8;
                    for _ in 0..count {
                        if off + 4 > body.len() {
                            break;
                        }
                        out.stss.push(read_u32(body, off)?);
                        off += 4;
                    }
                }
            }
            _ => {}
        }
        pos = bx.box_end;
    }
    Ok(())
}

fn parse_stsd(body: &[u8], out: &mut StblTables) -> Result<(), MediaError> {
    if body.len() < 8 {
        return Ok(());
    }
    let entry_count = read_u32(body, 4)?;
    let mut off = 8usize;
    for _ in 0..entry_count {
        let Some(entry) = read_box_header(body, off)? else {
            break;
        };
        let ebody = &body[entry.body_start..entry.box_end];
        match &entry.kind {
            b"avc1" | b"avc3" | b"av01" | b"vp08" | b"vp09" => {
                let codec = match &entry.kind {
                    b"avc1" | b"avc3" => VideoCodec::H264,
                    b"av01" => VideoCodec::Av1,
                    b"vp08" => VideoCodec::Vp8,
                    b"vp09" => VideoCodec::Vp9,
                    _ => unreachable!(),
                };
                let (width, height) = if ebody.len() >= 28 {
                    (
                        u32::from(read_u16(ebody, 24)?),
                        u32::from(read_u16(ebody, 26)?),
                    )
                } else {
                    (0, 0)
                };
                out.kind = Some(TrackKind::Video(VideoTrackConfig {
                    codec,
                    width,
                    height,
                }));
                if ebody.len() > 78 {
                    extract_video_extra_data(&ebody[78..], &mut out.extra_data)?;
                }
                break;
            }
            b"mp4a" | b"Opus" | b"fLaC" | b".mp3" => {
                let codec = match &entry.kind {
                    b"mp4a" => AudioCodec::Aac,
                    b"Opus" => AudioCodec::Opus,
                    b"fLaC" => AudioCodec::Flac,
                    b".mp3" => AudioCodec::Mp3,
                    _ => unreachable!(),
                };
                let (channels, sample_rate) = if ebody.len() >= 28 {
                    let ch = read_u16(ebody, 16)?;
                    let sr = read_u32(ebody, 24)? >> 16;
                    (ch.max(1), if sr == 0 { 48_000 } else { sr })
                } else {
                    (2, 48_000)
                };
                out.kind = Some(TrackKind::Audio(AudioTrackConfig {
                    codec,
                    sample_rate,
                    channels,
                    pcm_format: None,
                }));
                if ebody.len() > 28 {
                    extract_audio_extra_data(&ebody[28..], codec, &mut out.extra_data)?;
                }
                break;
            }
            _ => {}
        }
        off = entry.box_end;
    }
    Ok(())
}

fn extract_video_extra_data(children: &[u8], out: &mut Vec<u8>) -> Result<(), MediaError> {
    let mut pos = 0usize;
    while pos < children.len() {
        let Some(bx) = read_box_header(children, pos)? else {
            break;
        };
        let body = &children[bx.body_start..bx.box_end];
        if matches!(&bx.kind, b"avcC" | b"av1C" | b"vpcC") {
            out.clear();
            out.extend_from_slice(body);
            return Ok(());
        }
        pos = bx.box_end;
    }
    Ok(())
}

fn extract_audio_extra_data(
    children: &[u8],
    codec: AudioCodec,
    out: &mut Vec<u8>,
) -> Result<(), MediaError> {
    let mut pos = 0usize;
    while pos < children.len() {
        let Some(bx) = read_box_header(children, pos)? else {
            break;
        };
        let body = &children[bx.body_start..bx.box_end];
        match (&bx.kind, codec) {
            (b"esds", AudioCodec::Aac) => {
                if let Some(asc) = parse_esds_audio_specific_config(body) {
                    out.clear();
                    out.extend_from_slice(&asc);
                    return Ok(());
                }
            }
            (b"dOps", AudioCodec::Opus) => {
                // Convert MP4 `dOps` (RFC 7845 / ISO BMFF Opus) to standard 19-byte `OpusHead`
                // so downstream Opus decoders receive uniform RFC 7845 headers.
                if body.len() >= 11 {
                    let channels = body[1];
                    let pre_skip = u16::from_be_bytes([body[2], body[3]]);
                    let input_rate = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
                    let output_gain = i16::from_be_bytes([body[8], body[9]]);
                    let mapping_family = body[10];
                    let mut head = Vec::with_capacity(19 + body.len().saturating_sub(11));
                    head.extend_from_slice(b"OpusHead");
                    head.push(1);
                    head.push(channels);
                    head.extend_from_slice(&pre_skip.to_le_bytes());
                    head.extend_from_slice(&input_rate.to_le_bytes());
                    head.extend_from_slice(&output_gain.to_le_bytes());
                    head.push(mapping_family);
                    if body.len() > 11 {
                        head.extend_from_slice(&body[11..]);
                    }
                    *out = head;
                    return Ok(());
                }
            }
            (b"dfLa", AudioCodec::Flac) if body.len() > 4 => {
                *out = body[4..].to_vec();
                return Ok(());
            }
            _ => {}
        }
        pos = bx.box_end;
    }
    Ok(())
}

fn parse_esds_audio_specific_config(esds_body: &[u8]) -> Option<Vec<u8>> {
    // FullBox header = 4 bytes (version + flags), followed by ISO 14496-1 descriptors.
    if esds_body.len() < 8 {
        return None;
    }
    let mut i = 4usize;
    while i < esds_body.len() {
        let tag = esds_body[i];
        i += 1;
        let (len, consumed) = read_mp4_descr_len(&esds_body[i..])?;
        i += consumed;
        if i + len > esds_body.len() {
            return None;
        }
        match tag {
            0x03 => {
                // ES_Descriptor: skip ES_ID(2) + flags(1) and optional fields, then recurse inside.
                if len < 3 {
                    return None;
                }
                let flags = esds_body[i + 2];
                let mut inner = i + 3;
                if (flags & 0x80) != 0 {
                    inner += 2;
                }
                if (flags & 0x40) != 0 {
                    let url_len = *esds_body.get(inner)? as usize;
                    inner += 1 + url_len;
                }
                if (flags & 0x20) != 0 {
                    inner += 2;
                }
                i = inner;
            }
            0x04 => {
                // DecoderConfigDescriptor: 13 fixed bytes before child DecoderSpecificInfo (0x05).
                if len < 13 {
                    return None;
                }
                i += 13;
            }
            0x05 => {
                return Some(esds_body[i..i + len].to_vec());
            }
            _ => {
                i += len;
            }
        }
    }
    None
}

fn read_mp4_descr_len(slice: &[u8]) -> Option<(usize, usize)> {
    let mut len = 0usize;
    let mut consumed = 0usize;
    for &byte in slice.iter().take(4) {
        consumed += 1;
        len = (len << 7) | usize::from(byte & 0x7f);
        if (byte & 0x80) == 0 {
            return Some((len, consumed));
        }
    }
    None
}

fn extract_stbl_packets(
    full_file: &[u8],
    track_id: u32,
    packet_kind: PacketKind,
    timescale: u32,
    stbl: &StblTables,
) -> Result<Vec<EncodedPacket>, MediaError> {
    let sample_count = stbl.stsz_count as usize;
    if sample_count == 0 || stbl.chunk_offsets.is_empty() || stbl.stsc.is_empty() {
        return Ok(Vec::new());
    }

    // Expand sample durations (`stts`) and composition offsets (`ctts`).
    let mut durations = Vec::with_capacity(sample_count);
    for &(cnt, delta) in &stbl.stts {
        for _ in 0..cnt {
            if durations.len() >= sample_count {
                break;
            }
            durations.push(delta);
        }
    }
    while durations.len() < sample_count {
        durations.push(durations.last().copied().unwrap_or(0));
    }

    let mut ctts_offsets = Vec::with_capacity(sample_count);
    for &(cnt, off) in &stbl.ctts {
        for _ in 0..cnt {
            if ctts_offsets.len() >= sample_count {
                break;
            }
            ctts_offsets.push(off);
        }
    }

    let mut packets = Vec::with_capacity(sample_count);
    let mut sample_idx = 0usize;
    let mut dts_ticks: i64 = 0;

    for (chunk_zero_idx, &chunk_offset) in stbl.chunk_offsets.iter().enumerate() {
        let chunk_one = (chunk_zero_idx + 1) as u32;
        let samples_in_chunk = samples_per_chunk(&stbl.stsc, chunk_one);
        let mut cur_offset = usize::try_from(chunk_offset).map_err(|_| {
            MediaError::MalformedContainer("MP4 chunk offset exceeds usize".to_string())
        })?;

        for _ in 0..samples_in_chunk {
            if sample_idx >= sample_count {
                break;
            }
            let size = if stbl.stsz_default != 0 {
                stbl.stsz_default as usize
            } else {
                stbl.stsz_sizes.get(sample_idx).copied().unwrap_or(0) as usize
            };
            let end = cur_offset.saturating_add(size);
            if end > full_file.len() {
                return Err(MediaError::MalformedContainer(
                    "MP4 sample offset/size out of bounds".to_string(),
                ));
            }
            let dur_ticks = durations[sample_idx];
            let cto = ctts_offsets.get(sample_idx).copied().unwrap_or(0);
            let pts_ticks = dts_ticks + i64::from(cto);
            let one_based = (sample_idx + 1) as u32;
            let is_keyframe = if stbl.has_stss {
                stbl.stss.binary_search(&one_based).is_ok()
            } else {
                true
            };

            packets.push(EncodedPacket {
                track_id,
                kind: packet_kind,
                data: full_file[cur_offset..end].to_vec(),
                pts_us: ticks_to_us_i64(pts_ticks, timescale),
                dts_us: ticks_to_us_i64(dts_ticks, timescale),
                duration_us: ticks_to_us_u64(u64::from(dur_ticks), timescale),
                is_keyframe,
            });

            cur_offset = end;
            dts_ticks += i64::from(dur_ticks);
            sample_idx += 1;
        }
    }

    Ok(packets)
}

fn samples_per_chunk(stsc: &[(u32, u32, u32)], chunk_one_based: u32) -> u32 {
    let mut spc = 1u32;
    for &(first_chunk, count, _) in stsc {
        if first_chunk <= chunk_one_based {
            spc = count;
        } else {
            break;
        }
    }
    spc
}

fn parse_moof(
    full_buf: &[u8],
    moof_start: usize,
    moof_body: &[u8],
    state: &mut Mp4InitState,
    packets: &mut Vec<EncodedPacket>,
) -> Result<(), MediaError> {
    let mut pos = 0usize;
    while pos < moof_body.len() {
        let Some(bx) = read_box_header(moof_body, pos)? else {
            break;
        };
        if &bx.kind == b"traf" {
            parse_traf(
                full_buf,
                moof_start,
                &moof_body[bx.body_start..bx.box_end],
                state,
                packets,
            )?;
        }
        pos = bx.box_end;
    }
    Ok(())
}

fn parse_traf(
    full_buf: &[u8],
    moof_start: usize,
    traf_body: &[u8],
    state: &mut Mp4InitState,
    packets: &mut Vec<EncodedPacket>,
) -> Result<(), MediaError> {
    let mut track_id = 0u32;
    let mut base_data_offset = moof_start as u64;
    let mut tfhd_def_dur: Option<u32> = None;
    let mut tfhd_def_size: Option<u32> = None;
    let mut tfhd_def_flags: Option<u32> = None;
    let mut tfdt_time: Option<u64> = None;
    let mut trun_bodies: Vec<&[u8]> = Vec::new();

    let mut pos = 0usize;
    while pos < traf_body.len() {
        let Some(bx) = read_box_header(traf_body, pos)? else {
            break;
        };
        let body = &traf_body[bx.body_start..bx.box_end];
        match &bx.kind {
            b"tfhd" => {
                if body.len() < 8 {
                    return Err(MediaError::MalformedContainer("truncated tfhd".to_string()));
                }
                let flags = read_u32(body, 0)? & 0x00ff_ffff;
                track_id = read_u32(body, 4)?;
                let mut off = 8usize;
                if (flags & 0x0000_0001) != 0 {
                    base_data_offset = read_u64(body, off)?;
                    off += 8;
                }
                if (flags & 0x0000_0002) != 0 {
                    off += 4; // sample_description_index
                }
                if (flags & 0x0000_0008) != 0 {
                    tfhd_def_dur = Some(read_u32(body, off)?);
                    off += 4;
                }
                if (flags & 0x0000_0010) != 0 {
                    tfhd_def_size = Some(read_u32(body, off)?);
                    off += 4;
                }
                if (flags & 0x0000_0020) != 0 {
                    tfhd_def_flags = Some(read_u32(body, off)?);
                }
            }
            b"tfdt" => {
                if !body.is_empty() {
                    let version = body[0];
                    if version == 0 && body.len() >= 8 {
                        tfdt_time = Some(u64::from(read_u32(body, 4)?));
                    } else if version == 1 && body.len() >= 12 {
                        tfdt_time = Some(read_u64(body, 4)?);
                    }
                }
            }
            b"trun" => {
                trun_bodies.push(body);
            }
            _ => {}
        }
        pos = bx.box_end;
    }

    let Some(track_state) = state.tracks.get_mut(&track_id) else {
        return Ok(());
    };
    let timescale = track_state.info.timescale.max(1);
    let packet_kind = match &track_state.info.kind {
        TrackKind::Video(_) => PacketKind::Video,
        TrackKind::Audio(_) => PacketKind::Audio,
    };

    let def_dur = tfhd_def_dur.unwrap_or(track_state.default_sample_duration);
    let def_size = tfhd_def_size.unwrap_or(track_state.default_sample_size);
    let def_flags = tfhd_def_flags.unwrap_or(track_state.default_sample_flags);

    let mut cur_dts = tfdt_time.unwrap_or(track_state.next_decode_time);

    for body in trun_bodies {
        if body.len() < 8 {
            return Err(MediaError::MalformedContainer("truncated trun".to_string()));
        }
        let version = body[0];
        let flags = read_u32(body, 0)? & 0x00ff_ffff;
        let sample_count = read_u32(body, 4)? as usize;
        let mut off = 8usize;

        let data_offset = if (flags & 0x0000_0001) != 0 {
            let v = read_i32(body, off)?;
            off += 4;
            v
        } else {
            0
        };
        let first_sample_flags = if (flags & 0x0000_0004) != 0 {
            let v = read_u32(body, off)?;
            off += 4;
            Some(v)
        } else {
            None
        };

        let mut sample_pos = if data_offset >= 0 {
            (base_data_offset as usize).saturating_add(data_offset as usize)
        } else {
            (base_data_offset as usize).saturating_sub(data_offset.unsigned_abs() as usize)
        };

        for idx in 0..sample_count {
            let dur = if (flags & 0x0000_0100) != 0 {
                let v = read_u32(body, off)?;
                off += 4;
                v
            } else {
                def_dur
            };
            let size = if (flags & 0x0000_0200) != 0 {
                let v = read_u32(body, off)?;
                off += 4;
                v as usize
            } else {
                def_size as usize
            };
            let sflags = if (flags & 0x0000_0400) != 0 {
                let v = read_u32(body, off)?;
                off += 4;
                v
            } else if idx == 0 {
                first_sample_flags.unwrap_or(def_flags)
            } else {
                def_flags
            };
            let cto: i64 = if (flags & 0x0000_0800) != 0 {
                let v = if version == 0 {
                    i64::from(read_u32(body, off)?)
                } else {
                    i64::from(read_i32(body, off)?)
                };
                off += 4;
                v
            } else {
                0
            };

            let end = sample_pos.saturating_add(size);
            if end > full_buf.len() {
                return Err(MediaError::MalformedContainer(
                    "fMP4 trun sample exceeds buffer length".to_string(),
                ));
            }

            // ISO 14496-12 §8.8.3.1: `sample_is_non_sync_sample` is bit 16 (`0x0001_0000`).
            let is_non_sync = (sflags & 0x0001_0000) != 0;
            let is_keyframe = packet_kind == PacketKind::Audio || !is_non_sync;

            let dts_i64 = cur_dts as i64;
            let pts_i64 = dts_i64 + cto;

            packets.push(EncodedPacket {
                track_id,
                kind: packet_kind,
                data: full_buf[sample_pos..end].to_vec(),
                pts_us: ticks_to_us_i64(pts_i64, timescale),
                dts_us: ticks_to_us_i64(dts_i64, timescale),
                duration_us: ticks_to_us_u64(u64::from(dur), timescale),
                is_keyframe,
            });

            sample_pos = end;
            cur_dts = cur_dts.saturating_add(u64::from(dur));
        }
    }

    track_state.next_decode_time = cur_dts;
    Ok(())
}

fn parse_moof_mse(
    full_buf: &[u8],
    moof_start: usize,
    moof_body: &[u8],
    state: &mut Mp4InitState,
    samples: &mut Vec<MseSample>,
) -> Result<(), MediaError> {
    let mut pos = 0usize;
    let mut saw_traf = false;
    while pos < moof_body.len() {
        let Some(bx) = read_box_header(moof_body, pos)? else {
            break;
        };
        if &bx.kind == b"traf" {
            saw_traf = true;
            parse_traf_mse(
                full_buf,
                moof_start,
                &moof_body[bx.body_start..bx.box_end],
                state,
                samples,
            )?;
        }
        pos = bx.box_end;
    }
    if !saw_traf {
        return Err(MediaError::MalformedContainer(
            "fMP4 moof contains no traf box".to_string(),
        ));
    }
    Ok(())
}

fn parse_traf_mse(
    full_buf: &[u8],
    moof_start: usize,
    traf_body: &[u8],
    state: &mut Mp4InitState,
    samples: &mut Vec<MseSample>,
) -> Result<(), MediaError> {
    let mut track_id = 0u32;
    let mut base_data_offset = moof_start as u64;
    let mut tfhd_def_dur: Option<u32> = None;
    let mut tfhd_def_size: Option<u32> = None;
    let mut tfhd_def_flags: Option<u32> = None;
    let mut tfdt_time: Option<u64> = None;
    let mut trun_bodies: Vec<&[u8]> = Vec::new();

    let mut pos = 0usize;
    while pos < traf_body.len() {
        let Some(bx) = read_box_header(traf_body, pos)? else {
            break;
        };
        let body = &traf_body[bx.body_start..bx.box_end];
        match &bx.kind {
            b"tfhd" => {
                if body.len() < 8 {
                    return Err(MediaError::MalformedContainer("truncated tfhd".to_string()));
                }
                let flags = read_u32(body, 0)? & 0x00ff_ffff;
                track_id = read_u32(body, 4)?;
                let mut off = 8usize;
                if (flags & 0x0000_0001) != 0 {
                    base_data_offset = read_u64(body, off)?;
                    off += 8;
                }
                if (flags & 0x0000_0002) != 0 {
                    off += 4;
                }
                if (flags & 0x0000_0008) != 0 {
                    tfhd_def_dur = Some(read_u32(body, off)?);
                    off += 4;
                }
                if (flags & 0x0000_0010) != 0 {
                    tfhd_def_size = Some(read_u32(body, off)?);
                    off += 4;
                }
                if (flags & 0x0000_0020) != 0 {
                    tfhd_def_flags = Some(read_u32(body, off)?);
                }
            }
            b"tfdt" => {
                if !body.is_empty() {
                    let version = body[0];
                    if version == 0 && body.len() >= 8 {
                        tfdt_time = Some(u64::from(read_u32(body, 4)?));
                    } else if version == 1 && body.len() >= 12 {
                        tfdt_time = Some(read_u64(body, 4)?);
                    }
                }
            }
            b"trun" => trun_bodies.push(body),
            _ => {}
        }
        pos = bx.box_end;
    }

    let Some(track_state) = state.tracks.get_mut(&track_id) else {
        return Err(MediaError::MalformedContainer(format!(
            "fMP4 traf references unknown track {track_id}"
        )));
    };
    let timescale = f64::from(track_state.info.timescale.max(1));
    let elst_offset_s = track_state.elst_offset_s;
    let (packet_kind, video_dims) = match &track_state.info.kind {
        TrackKind::Video(v) => (PacketKind::Video, Some((v.width, v.height))),
        TrackKind::Audio(_) => (PacketKind::Audio, None),
    };

    let def_dur = tfhd_def_dur.unwrap_or(track_state.default_sample_duration);
    let def_size = tfhd_def_size.unwrap_or(track_state.default_sample_size);
    let def_flags = tfhd_def_flags.unwrap_or(track_state.default_sample_flags);
    let mut cur_dts = tfdt_time.unwrap_or(track_state.next_decode_time);

    for body in trun_bodies {
        if body.len() < 8 {
            return Err(MediaError::MalformedContainer("truncated trun".to_string()));
        }
        let version = body[0];
        let flags = read_u32(body, 0)? & 0x00ff_ffff;
        let sample_count = read_u32(body, 4)? as usize;
        let mut off = 8usize;

        let data_offset = if (flags & 0x0000_0001) != 0 {
            let v = read_i32(body, off)?;
            off += 4;
            v
        } else {
            0
        };
        let first_sample_flags = if (flags & 0x0000_0004) != 0 {
            let v = read_u32(body, off)?;
            off += 4;
            Some(v)
        } else {
            None
        };

        let mut sample_pos = if data_offset >= 0 {
            (base_data_offset as usize).saturating_add(data_offset as usize)
        } else {
            (base_data_offset as usize).saturating_sub(data_offset.unsigned_abs() as usize)
        };

        for idx in 0..sample_count {
            let dur = if (flags & 0x0000_0100) != 0 {
                let v = read_u32(body, off)?;
                off += 4;
                v
            } else {
                def_dur
            };
            let size = if (flags & 0x0000_0200) != 0 {
                let v = read_u32(body, off)?;
                off += 4;
                v as usize
            } else {
                def_size as usize
            };
            let sflags = if (flags & 0x0000_0400) != 0 {
                let v = read_u32(body, off)?;
                off += 4;
                v
            } else if idx == 0 {
                first_sample_flags.unwrap_or(def_flags)
            } else {
                def_flags
            };
            let cto: i64 = if (flags & 0x0000_0800) != 0 {
                let v = if version == 0 {
                    i64::from(read_u32(body, off)?)
                } else {
                    i64::from(read_i32(body, off)?)
                };
                off += 4;
                v
            } else {
                0
            };

            let end = sample_pos.saturating_add(size);
            if end > full_buf.len() {
                return Err(MediaError::MalformedContainer(
                    "fMP4 trun sample exceeds buffer length".to_string(),
                ));
            }

            let is_non_sync = (sflags & 0x0001_0000) != 0;
            let is_keyframe = packet_kind == PacketKind::Audio || !is_non_sync;
            let dts_i64 = cur_dts as i64;
            let pts_i64 = dts_i64 + cto;

            samples.push(MseSample {
                track_id,
                kind: packet_kind,
                data: full_buf[sample_pos..end].to_vec(),
                pts_s: (pts_i64 as f64) / timescale + elst_offset_s,
                dts_s: (dts_i64 as f64) / timescale,
                duration_s: f64::from(dur) / timescale,
                is_keyframe,
                video_dims,
            });

            sample_pos = end;
            cur_dts = cur_dts.saturating_add(u64::from(dur));
        }
    }

    track_state.next_decode_time = cur_dts;
    Ok(())
}

// ---------------------------------------------------------------------------
// Spec-compliant Fragmented MP4 (fMP4) builder helpers for tests & MSE fixtures
// ---------------------------------------------------------------------------

/// Sample descriptor for [`mux_fmp4_media_segment`].
#[derive(Debug, Clone)]
pub struct Fmp4SampleInput<'a> {
    pub data: &'a [u8],
    pub duration_ticks: u32,
    pub composition_offset_ticks: i32,
    pub is_keyframe: bool,
}

fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let size = (8 + payload.len()) as u32;
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

fn mp4_full_box(kind: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut full = Vec::with_capacity(4 + payload.len());
    let vf = (u32::from(version) << 24) | (flags & 0x00ff_ffff);
    full.extend_from_slice(&vf.to_be_bytes());
    full.extend_from_slice(payload);
    mp4_box(kind, &full)
}

/// Builds a spec-compliant ISO BMFF fragmented MP4 initialization segment (`ftyp` + `moov` + `mvex`).
pub fn mux_fmp4_init_segment(tracks: &[TrackInfo], movie_timescale: u32) -> Vec<u8> {
    let mut ftyp_payload = Vec::new();
    ftyp_payload.extend_from_slice(b"isom");
    ftyp_payload.extend_from_slice(&512u32.to_be_bytes());
    ftyp_payload.extend_from_slice(b"isomiso5dashmp41");
    let ftyp = mp4_box(b"ftyp", &ftyp_payload);

    let mut mvhd_payload = vec![0u8; 92];
    mvhd_payload[8..12].copy_from_slice(&movie_timescale.to_be_bytes());
    // rate = 1.0 (0x00010000), volume = 1.0 (0x0100), identity matrix, next_track_id.
    mvhd_payload[16..20].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    mvhd_payload[20..22].copy_from_slice(&0x0100u16.to_be_bytes());
    let next_tid = tracks.iter().map(|t| t.id).max().unwrap_or(1) + 1;
    mvhd_payload[88..92].copy_from_slice(&next_tid.to_be_bytes());
    let mvhd = mp4_full_box(b"mvhd", 0, 0, &mvhd_payload);

    let mut moov_children = Vec::new();
    moov_children.extend_from_slice(&mvhd);

    let mut mvex_children = Vec::new();
    for track in tracks {
        moov_children.extend_from_slice(&build_fmp4_trak(track));
        let mut trex_payload = Vec::with_capacity(20);
        trex_payload.extend_from_slice(&track.id.to_be_bytes());
        trex_payload.extend_from_slice(&1u32.to_be_bytes());
        trex_payload.extend_from_slice(&0u32.to_be_bytes());
        trex_payload.extend_from_slice(&0u32.to_be_bytes());
        trex_payload.extend_from_slice(&0u32.to_be_bytes());
        mvex_children.extend_from_slice(&mp4_full_box(b"trex", 0, 0, &trex_payload));
    }
    moov_children.extend_from_slice(&mp4_box(b"mvex", &mvex_children));

    let moov = mp4_box(b"moov", &moov_children);
    let mut out = Vec::with_capacity(ftyp.len() + moov.len());
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    out
}

fn build_fmp4_trak(track: &TrackInfo) -> Vec<u8> {
    let mut tkhd_payload = vec![0u8; 80];
    tkhd_payload[8..12].copy_from_slice(&track.id.to_be_bytes());
    if let TrackKind::Video(v) = &track.kind {
        tkhd_payload[72..76].copy_from_slice(&(v.width << 16).to_be_bytes());
        tkhd_payload[76..80].copy_from_slice(&(v.height << 16).to_be_bytes());
    }
    let tkhd = mp4_full_box(b"tkhd", 0, 3, &tkhd_payload);

    let mut mdhd_payload = vec![0u8; 20];
    mdhd_payload[8..12].copy_from_slice(&track.timescale.max(1).to_be_bytes());
    let mdhd = mp4_full_box(b"mdhd", 0, 0, &mdhd_payload);

    let mut hdlr_payload = vec![0u8; 21];
    let hdlr_type = match &track.kind {
        TrackKind::Video(_) => b"vide",
        TrackKind::Audio(_) => b"soun",
    };
    hdlr_payload[4..8].copy_from_slice(hdlr_type);
    let hdlr = mp4_full_box(b"hdlr", 0, 0, &hdlr_payload);

    let sample_entry = build_sample_entry(track);
    let mut stsd_payload = Vec::with_capacity(4 + sample_entry.len());
    stsd_payload.extend_from_slice(&1u32.to_be_bytes());
    stsd_payload.extend_from_slice(&sample_entry);
    let stsd = mp4_full_box(b"stsd", 0, 0, &stsd_payload);

    let empty_count = 0u32.to_be_bytes();
    let stts = mp4_full_box(b"stts", 0, 0, &empty_count);
    let stsc = mp4_full_box(b"stsc", 0, 0, &empty_count);
    let stsz = mp4_full_box(b"stsz", 0, 0, &[0u8; 8]);
    let stco = mp4_full_box(b"stco", 0, 0, &empty_count);

    let mut stbl_body = Vec::new();
    stbl_body.extend_from_slice(&stsd);
    stbl_body.extend_from_slice(&stts);
    stbl_body.extend_from_slice(&stsc);
    stbl_body.extend_from_slice(&stsz);
    stbl_body.extend_from_slice(&stco);
    let stbl = mp4_box(b"stbl", &stbl_body);
    let minf = mp4_box(b"minf", &stbl);

    let mut mdia_body = Vec::new();
    mdia_body.extend_from_slice(&mdhd);
    mdia_body.extend_from_slice(&hdlr);
    mdia_body.extend_from_slice(&minf);
    let mdia = mp4_box(b"mdia", &mdia_body);

    let mut trak_body = Vec::new();
    trak_body.extend_from_slice(&tkhd);
    trak_body.extend_from_slice(&mdia);
    mp4_box(b"trak", &trak_body)
}

fn build_sample_entry(track: &TrackInfo) -> Vec<u8> {
    match &track.kind {
        TrackKind::Video(v) => {
            let mut body = vec![0u8; 78];
            body[6..8].copy_from_slice(&1u16.to_be_bytes()); // data_reference_index
            body[24..26].copy_from_slice(&(v.width as u16).to_be_bytes());
            body[26..28].copy_from_slice(&(v.height as u16).to_be_bytes());
            body[28..32].copy_from_slice(&0x0048_0000u32.to_be_bytes());
            body[32..36].copy_from_slice(&0x0048_0000u32.to_be_bytes());
            body[40..42].copy_from_slice(&1u16.to_be_bytes());
            body[74..76].copy_from_slice(&0x0018u16.to_be_bytes());
            body[76..78].copy_from_slice(&0xffffu16.to_be_bytes());
            if !track.extra_data.is_empty() {
                let cfg_tag = match v.codec {
                    VideoCodec::H264 => b"avcC",
                    VideoCodec::Av1 => b"av1C",
                    VideoCodec::Vp8 | VideoCodec::Vp9 => b"vpcC",
                };
                body.extend_from_slice(&mp4_box(cfg_tag, &track.extra_data));
            }
            let fourcc = match v.codec {
                VideoCodec::H264 => b"avc1",
                VideoCodec::Av1 => b"av01",
                VideoCodec::Vp8 => b"vp08",
                VideoCodec::Vp9 => b"vp09",
            };
            mp4_box(fourcc, &body)
        }
        TrackKind::Audio(a) => {
            let mut body = vec![0u8; 28];
            body[6..8].copy_from_slice(&1u16.to_be_bytes());
            body[16..18].copy_from_slice(&a.channels.to_be_bytes());
            body[18..20].copy_from_slice(&16u16.to_be_bytes());
            body[24..28].copy_from_slice(&(a.sample_rate << 16).to_be_bytes());
            if !track.extra_data.is_empty() {
                match a.codec {
                    AudioCodec::Aac => {
                        let asc = &track.extra_data;
                        let mut dsi = vec![0x05u8, asc.len() as u8];
                        dsi.extend_from_slice(asc);
                        let mut dcd = vec![0x04u8, (13 + dsi.len()) as u8, 0x40, 0x15];
                        dcd.extend_from_slice(&[0u8; 11]);
                        dcd.extend_from_slice(&dsi);
                        let mut esd = vec![0x03u8, (3 + dcd.len()) as u8, 0, 1, 0];
                        esd.extend_from_slice(&dcd);
                        body.extend_from_slice(&mp4_full_box(b"esds", 0, 0, &esd));
                    }
                    AudioCodec::Opus => {
                        let mut dops = vec![0u8, a.channels as u8, 0x01, 0x38];
                        dops.extend_from_slice(&a.sample_rate.to_be_bytes());
                        dops.extend_from_slice(&[0u8, 0u8, 0u8]);
                        body.extend_from_slice(&mp4_box(b"dOps", &dops));
                    }
                    _ => {}
                }
            }
            let fourcc = match a.codec {
                AudioCodec::Aac => b"mp4a",
                AudioCodec::Opus => b"Opus",
                AudioCodec::Flac => b"fLaC",
                AudioCodec::Mp3 => b".mp3",
                AudioCodec::Vorbis | AudioCodec::Pcm => b"mp4a",
            };
            mp4_box(fourcc, &body)
        }
    }
}

/// Builds a spec-compliant ISO BMFF fragmented MP4 media segment (`moof` + `mdat`).
pub fn mux_fmp4_media_segment(
    sequence_number: u32,
    track_id: u32,
    base_media_decode_time: u64,
    samples: &[Fmp4SampleInput<'_>],
) -> Vec<u8> {
    let mfhd = mp4_full_box(b"mfhd", 0, 0, &sequence_number.to_be_bytes());
    let tfhd = mp4_full_box(b"tfhd", 0, 0x02_0000, &track_id.to_be_bytes());
    let tfdt = mp4_full_box(b"tfdt", 1, 0, &base_media_decode_time.to_be_bytes());

    let mut mdat_payload = Vec::new();
    for s in samples {
        mdat_payload.extend_from_slice(s.data);
    }

    // Build `trun` v1 with data_offset | duration | size | flags | signed cto (0x0000_0f01).
    let trun_flags: u32 = 0x0000_0f01;
    let build_trun = |data_offset: i32| -> Vec<u8> {
        let mut p = Vec::with_capacity(8 + samples.len() * 16);
        p.extend_from_slice(&(samples.len() as u32).to_be_bytes());
        p.extend_from_slice(&data_offset.to_be_bytes());
        for s in samples {
            p.extend_from_slice(&s.duration_ticks.to_be_bytes());
            p.extend_from_slice(&(s.data.len() as u32).to_be_bytes());
            let sflags: u32 = if s.is_keyframe {
                0x0200_0000
            } else {
                0x0101_0000
            };
            p.extend_from_slice(&sflags.to_be_bytes());
            p.extend_from_slice(&s.composition_offset_ticks.to_be_bytes());
        }
        mp4_full_box(b"trun", 1, trun_flags, &p)
    };

    // First pass computes exact `moof` size so `data_offset = moof.len() + 8` points into `mdat` body.
    let probe_trun = build_trun(0);
    let mut probe_traf_body = Vec::new();
    probe_traf_body.extend_from_slice(&tfhd);
    probe_traf_body.extend_from_slice(&tfdt);
    probe_traf_body.extend_from_slice(&probe_trun);
    let probe_traf = mp4_box(b"traf", &probe_traf_body);
    let moof_size = 8 + mfhd.len() + probe_traf.len();

    let trun = build_trun((moof_size + 8) as i32);
    let mut traf_body = Vec::new();
    traf_body.extend_from_slice(&tfhd);
    traf_body.extend_from_slice(&tfdt);
    traf_body.extend_from_slice(&trun);
    let traf = mp4_box(b"traf", &traf_body);

    let mut moof_body = Vec::new();
    moof_body.extend_from_slice(&mfhd);
    moof_body.extend_from_slice(&traf);
    let moof = mp4_box(b"moof", &moof_body);
    let mdat = mp4_box(b"mdat", &mdat_payload);

    let mut out = Vec::with_capacity(moof.len() + mdat.len());
    out.extend_from_slice(&moof);
    out.extend_from_slice(&mdat);
    out
}
