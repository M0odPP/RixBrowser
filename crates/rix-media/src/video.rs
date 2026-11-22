//! Pure-Rust full inter-frame video decoders for AV1 (`rav1d`), H.264 (with DPB reordering), VP8, and VP9.

use std::collections::VecDeque;

use crate::types::{
    ColorMatrix, ColorRange, EncodedPacket, MediaError, PixelFormat, TrackInfo, TrackKind,
    VideoCodec, VideoFrame,
};

/// Unified pure-Rust video decoder trait supporting keyframes, P-frames, and B-frames.
pub trait VideoDecoder: Send {
    /// Decodes a single elementary packet, returning zero or more display-ordered planar YUV frames.
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Vec<VideoFrame>, MediaError>;

    /// Drains any buffered / reordered frames at end-of-stream or seek boundaries.
    fn flush(&mut self) -> Result<Vec<VideoFrame>, MediaError>;

    /// Resets internal reference frame buffers and decoder state.
    fn reset(&mut self);
}

/// Instantiates the appropriate pure-Rust [`VideoDecoder`] for a demuxed video track.
pub fn create_video_decoder(track: &TrackInfo) -> Result<Box<dyn VideoDecoder>, MediaError> {
    let TrackKind::Video(cfg) = &track.kind else {
        return Err(MediaError::InvalidConfig(
            "expected a video track for video decoder creation".to_string(),
        ));
    };

    match cfg.codec {
        VideoCodec::Av1 => Ok(Box::new(Av1VideoDecoder::new()?)),
        VideoCodec::H264 => Ok(Box::new(H264VideoDecoder::new(&track.extra_data))),
        VideoCodec::Vp8 => Ok(Box::new(Vp8VideoDecoder::new())),
        VideoCodec::Vp9 => Ok(Box::new(Vp9VideoDecoder::new())),
    }
}

/// Copies a potentially strided 8-bit plane into a tightly packed plane (`width * height`).
fn copy_tight_plane_8(src: &[u8], stride: usize, width: usize, height: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height);
    for row in 0..height {
        let start = row * stride;
        let end = (start + width).min(src.len());
        if start < end {
            out.extend_from_slice(&src[start..end]);
            let copied = end - start;
            if copied < width {
                out.resize(out.len() + (width - copied), 0);
            }
        } else {
            out.resize(out.len() + width, 0);
        }
    }
    out
}

/// Downshifts a 10-bit or 12-bit little-endian `u16` plane into a tightly packed 8-bit plane.
fn copy_tight_plane_hbd_to_8(
    src_bytes: &[u8],
    stride_bytes: usize,
    width: usize,
    height: usize,
    shift: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height);
    for row in 0..height {
        let row_start = row * stride_bytes;
        for col in 0..width {
            let off = row_start + col * 2;
            let sample = if off + 1 < src_bytes.len() {
                u16::from_le_bytes([src_bytes[off], src_bytes[off + 1]])
            } else {
                0
            };
            out.push((sample >> shift).min(255) as u8);
        }
    }
    out
}

/// Subsamples a 4:2:2 or 4:4:4 chroma plane to 4:2:0 so all output frames share uniform `Yuv420p`.
fn subsample_chroma_to_420(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
) -> Vec<u8> {
    if src_w == dst_w && src_h == dst_h {
        return src.to_vec();
    }
    let mut out = Vec::with_capacity(dst_w * dst_h);
    let sx = (src_w / dst_w.max(1)).max(1);
    let sy = (src_h / dst_h.max(1)).max(1);
    for y in 0..dst_h {
        let src_y = (y * sy).min(src_h.saturating_sub(1));
        for x in 0..dst_w {
            let src_x = (x * sx).min(src_w.saturating_sub(1));
            out.push(src.get(src_y * src_w + src_x).copied().unwrap_or(128));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 1. AV1 Decoder (`rav1d` via pure-Rust no-asm `rusty_av1d`)
// ---------------------------------------------------------------------------

/// Pure-Rust AV1 video decoder powered by `rav1d` (`rusty_av1d` safe Rust API).
pub struct Av1VideoDecoder {
    decoder: rusty_av1d::Decoder,
}

impl Av1VideoDecoder {
    /// Creates a new `rav1d` AV1 decoder with single-frame low-latency threading defaults.
    pub fn new() -> Result<Self, MediaError> {
        let mut settings = rusty_av1d::Settings::new();
        settings.set_n_threads(1);
        settings.set_max_frame_delay(1);
        let decoder = rusty_av1d::Decoder::with_settings(&settings)
            .map_err(|e| MediaError::VideoDecode(format!("rav1d init failed: {e:?}")))?;
        Ok(Self { decoder })
    }

    fn drain_pictures(&mut self, out: &mut Vec<VideoFrame>, fallback_keyframe: bool) {
        loop {
            match self.decoder.get_picture() {
                Ok(pic) => out.push(convert_rav1d_picture(&pic, fallback_keyframe)),
                Err(rusty_av1d::Rav1dError::TryAgain) => break,
                Err(_) => break,
            }
        }
    }
}

impl VideoDecoder for Av1VideoDecoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Vec<VideoFrame>, MediaError> {
        if packet.data.is_empty() {
            return Ok(Vec::new());
        }
        let mut frames = Vec::new();
        let data_box = packet.data.clone().into_boxed_slice();
        let mut res = self.decoder.send_data(
            data_box,
            None,
            Some(packet.pts_us),
            Some(packet.duration_us as i64),
        );

        loop {
            match res {
                Ok(()) => {
                    self.drain_pictures(&mut frames, packet.is_keyframe);
                    break;
                }
                Err(rusty_av1d::Rav1dError::TryAgain) => {
                    self.drain_pictures(&mut frames, packet.is_keyframe);
                    res = self.decoder.send_pending_data();
                }
                Err(err) => {
                    return Err(MediaError::VideoDecode(format!(
                        "rav1d send_data failed: {err:?}"
                    )));
                }
            }
        }

        Ok(frames)
    }

    fn flush(&mut self) -> Result<Vec<VideoFrame>, MediaError> {
        let mut frames = Vec::new();
        // First drain any pictures already queued in rav1d's frame delay buffer.
        self.drain_pictures(&mut frames, false);
        self.decoder.flush();
        self.drain_pictures(&mut frames, false);
        Ok(frames)
    }

    fn reset(&mut self) {
        self.decoder.flush();
    }
}

fn convert_rav1d_picture(pic: &rusty_av1d::Picture, is_keyframe: bool) -> VideoFrame {
    use rusty_av1d::pixel::{MatrixCoefficients, YUVRange};
    use rusty_av1d::{PixelLayout, PlanarImageComponent};

    let width = pic.width() as usize;
    let height = pic.height() as usize;
    let cw = width.div_ceil(2);
    let ch = height.div_ceil(2);
    let bpc = pic.bits_per_component().map(|b| b.0).unwrap_or(8);
    let hbd = pic.bit_depth() > 8;
    let shift = u32::from(bpc.saturating_sub(8));

    let y_stride = pic.stride(PlanarImageComponent::Y) as usize;
    let y_raw = pic.plane(PlanarImageComponent::Y);
    let y_plane = if hbd {
        copy_tight_plane_hbd_to_8(y_raw, y_stride, width, height, shift)
    } else {
        copy_tight_plane_8(y_raw, y_stride, width, height)
    };

    let (u_plane, v_plane) = match pic.pixel_layout() {
        PixelLayout::I400 => (vec![128u8; cw * ch], vec![128u8; cw * ch]),
        PixelLayout::I420 => {
            let us = pic.stride(PlanarImageComponent::U) as usize;
            let vs = pic.stride(PlanarImageComponent::V) as usize;
            let ur = pic.plane(PlanarImageComponent::U);
            let vr = pic.plane(PlanarImageComponent::V);
            if hbd {
                (
                    copy_tight_plane_hbd_to_8(ur, us, cw, ch, shift),
                    copy_tight_plane_hbd_to_8(vr, vs, cw, ch, shift),
                )
            } else {
                (
                    copy_tight_plane_8(ur, us, cw, ch),
                    copy_tight_plane_8(vr, vs, cw, ch),
                )
            }
        }
        PixelLayout::I422 => {
            let us = pic.stride(PlanarImageComponent::U) as usize;
            let vs = pic.stride(PlanarImageComponent::V) as usize;
            let ur = pic.plane(PlanarImageComponent::U);
            let vr = pic.plane(PlanarImageComponent::V);
            let (u_full, v_full) = if hbd {
                (
                    copy_tight_plane_hbd_to_8(ur, us, cw, height, shift),
                    copy_tight_plane_hbd_to_8(vr, vs, cw, height, shift),
                )
            } else {
                (
                    copy_tight_plane_8(ur, us, cw, height),
                    copy_tight_plane_8(vr, vs, cw, height),
                )
            };
            (
                subsample_chroma_to_420(&u_full, cw, height, cw, ch),
                subsample_chroma_to_420(&v_full, cw, height, cw, ch),
            )
        }
        PixelLayout::I444 => {
            let us = pic.stride(PlanarImageComponent::U) as usize;
            let vs = pic.stride(PlanarImageComponent::V) as usize;
            let ur = pic.plane(PlanarImageComponent::U);
            let vr = pic.plane(PlanarImageComponent::V);
            let (u_full, v_full) = if hbd {
                (
                    copy_tight_plane_hbd_to_8(ur, us, width, height, shift),
                    copy_tight_plane_hbd_to_8(vr, vs, width, height, shift),
                )
            } else {
                (
                    copy_tight_plane_8(ur, us, width, height),
                    copy_tight_plane_8(vr, vs, width, height),
                )
            };
            (
                subsample_chroma_to_420(&u_full, width, height, cw, ch),
                subsample_chroma_to_420(&v_full, width, height, cw, ch),
            )
        }
    };

    let color_matrix = match pic.matrix_coefficients() {
        MatrixCoefficients::BT470BG | MatrixCoefficients::ST170M => ColorMatrix::Bt601,
        _ => ColorMatrix::Bt709,
    };
    let color_range = match pic.color_range() {
        YUVRange::Full => ColorRange::Full,
        YUVRange::Limited => ColorRange::Limited,
    };

    VideoFrame {
        width: pic.width(),
        height: pic.height(),
        pixel_format: PixelFormat::Yuv420p,
        color_matrix,
        color_range,
        y_plane,
        u_plane,
        v_plane,
        y_stride: pic.width(),
        u_stride: cw as u32,
        v_stride: cw as u32,
        pts_us: pic.timestamp().unwrap_or(0),
        duration_us: pic.duration().max(0) as u64,
        is_keyframe,
    }
}

// ---------------------------------------------------------------------------
// 2. H.264 / AVC Decoder (`rusty_h264` + AVCC->Annex-B + DPB B-frame reorder)
// ---------------------------------------------------------------------------

struct ReorderEntry {
    poc: i32,
    decode_seq: u64,
    frame: VideoFrame,
}

/// Pure-Rust H.264 (AVC) video decoder supporting Baseline/Main/High profiles,
/// MP4 `avcC` parameter-set extraction, and POC-based DPB frame reordering for B-frames.
pub struct H264VideoDecoder {
    decoder: rusty_h264::Decoder,
    avcc_length_size: Option<usize>,
    avcc_header_annex_b: Vec<u8>,
    header_sent: bool,
    dpb: Vec<ReorderEntry>,
    max_reorder_frames: usize,
    decode_seq: u64,
}

impl H264VideoDecoder {
    /// Creates an H.264 decoder, parsing optional ISO BMFF `avcC` `extra_data` if supplied.
    pub fn new(extra_data: &[u8]) -> Self {
        let (avcc_length_size, avcc_header_annex_b) = parse_avcc_extra_data(extra_data);
        Self {
            decoder: rusty_h264::Decoder::new(),
            avcc_length_size,
            avcc_header_annex_b,
            header_sent: false,
            dpb: Vec::new(),
            max_reorder_frames: 4,
            decode_seq: 0,
        }
    }

    fn flush_dpb_all(&mut self, out: &mut Vec<VideoFrame>) {
        self.dpb.sort_by_key(|e| (e.poc, e.decode_seq));
        out.extend(self.dpb.drain(..).map(|e| e.frame));
    }

    fn drain_dpb_overflow(&mut self, out: &mut Vec<VideoFrame>) {
        if self.dpb.len() > self.max_reorder_frames {
            self.dpb.sort_by_key(|e| (e.poc, e.decode_seq));
            let emit_count = self.dpb.len() - self.max_reorder_frames;
            out.extend(self.dpb.drain(..emit_count).map(|e| e.frame));
        }
    }

    fn decode_single_au(
        &mut self,
        au_annex_b: &[u8],
        pts_us: i64,
        duration_us: u64,
        hint_keyframe: bool,
        out: &mut Vec<VideoFrame>,
    ) -> Result<(), MediaError> {
        let is_idr = h264_au_is_idr(au_annex_b);
        if is_idr && !self.dpb.is_empty() {
            // An IDR starts a new GOP: flush all prior GOP pictures in display (POC) order first.
            self.flush_dpb_all(out);
        }

        if let Some(yuv) = self
            .decoder
            .decode(au_annex_b)
            .map_err(|e| MediaError::VideoDecode(format!("h264 decode failed: {e:?}")))?
        {
            let poc = self.decoder.last_poc();
            let cw = yuv.width.div_ceil(2) as u32;
            let frame = VideoFrame {
                width: yuv.width as u32,
                height: yuv.height as u32,
                pixel_format: PixelFormat::Yuv420p,
                color_matrix: ColorMatrix::Bt709,
                color_range: ColorRange::Limited,
                y_plane: yuv.y,
                u_plane: yuv.u,
                v_plane: yuv.v,
                y_stride: yuv.width as u32,
                u_stride: cw,
                v_stride: cw,
                pts_us,
                duration_us,
                is_keyframe: is_idr || hint_keyframe,
            };
            self.decode_seq += 1;
            self.dpb.push(ReorderEntry {
                poc,
                decode_seq: self.decode_seq,
                frame,
            });
            self.drain_dpb_overflow(out);
        }
        Ok(())
    }
}

impl VideoDecoder for H264VideoDecoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Vec<VideoFrame>, MediaError> {
        if packet.data.is_empty() {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();

        // Feed MP4 `avcC` SPS/PPS once before the first slice packet.
        if !self.header_sent && !self.avcc_header_annex_b.is_empty() {
            self.header_sent = true;
            let _ = self.decoder.decode(&self.avcc_header_annex_b);
        }

        let annex_b = if is_annex_b(&packet.data) {
            packet.data.clone()
        } else if let Some(len_size) = self.avcc_length_size {
            avcc_packet_to_annex_b(&packet.data, len_size)?
        } else {
            // Try 4-byte AVCC conversion fallback if no start codes are present.
            avcc_packet_to_annex_b(&packet.data, 4)?
        };

        let access_units = split_h264_access_units(&annex_b);
        if access_units.is_empty() {
            self.decode_single_au(
                &annex_b,
                packet.pts_us,
                packet.duration_us,
                packet.is_keyframe,
                &mut out,
            )?;
        } else {
            let count = access_units.len() as u64;
            let per_au_dur = packet.duration_us.checked_div(count).unwrap_or(0);
            for (idx, au) in access_units.into_iter().enumerate() {
                let pts = packet.pts_us + (idx as i64) * (per_au_dur as i64);
                self.decode_single_au(
                    au,
                    pts,
                    per_au_dur,
                    packet.is_keyframe && idx == 0,
                    &mut out,
                )?;
            }
        }

        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<VideoFrame>, MediaError> {
        let mut out = Vec::new();
        self.flush_dpb_all(&mut out);
        Ok(out)
    }

    fn reset(&mut self) {
        self.decoder = rusty_h264::Decoder::new();
        self.header_sent = false;
        self.dpb.clear();
        self.decode_seq = 0;
    }
}

fn is_annex_b(data: &[u8]) -> bool {
    (data.len() >= 4 && data[0..4] == [0, 0, 0, 1]) || (data.len() >= 3 && data[0..3] == [0, 0, 1])
}

/// Returns true if an Annex-B H.264 access unit contains an IDR slice (NAL unit type 5).
pub fn h264_au_is_idr(annex_b: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 3 < annex_b.len() {
        let nal_start = if i + 4 < annex_b.len() && annex_b[i..i + 4] == [0, 0, 0, 1] {
            i + 4
        } else if annex_b[i..i + 3] == [0, 0, 1] {
            i + 3
        } else {
            i += 1;
            continue;
        };
        if (annex_b[nal_start] & 0x1f) == 5 {
            return true;
        }
        i = nal_start + 1;
    }
    false
}

/// Splits an Annex-B H.264 byte stream into individual picture access units.
pub fn split_h264_access_units(annex_b: &[u8]) -> Vec<&[u8]> {
    let mut nals: Vec<(usize, usize, u8, bool)> = Vec::new();
    let mut i = 0usize;
    while i + 3 < annex_b.len() {
        let (sc_start, nal_start) = if i + 4 < annex_b.len() && annex_b[i..i + 4] == [0, 0, 0, 1] {
            (i, i + 4)
        } else if annex_b[i..i + 3] == [0, 0, 1] {
            (i, i + 3)
        } else {
            i += 1;
            continue;
        };
        let nal_type = annex_b[nal_start] & 0x1f;
        // In H.264 Exp-Golomb, `first_mb_in_slice == 0` encodes as leading `1` bit (`0x80`).
        let first_mb_zero = annex_b.get(nal_start + 1).is_some_and(|&b| (b & 0x80) != 0);
        nals.push((sc_start, nal_start, nal_type, first_mb_zero));
        i = nal_start + 1;
    }

    if nals.is_empty() {
        return Vec::new();
    }

    let mut aus = Vec::new();
    let mut au_start = nals[0].0;
    let mut saw_vcl = false;

    for &(sc_start, _nal_start, nal_type, first_mb_zero) in &nals {
        let is_vcl = (1..=5).contains(&nal_type);
        let starts_new_au = saw_vcl && (matches!(nal_type, 6..=9) || (is_vcl && first_mb_zero));
        if starts_new_au {
            aus.push(&annex_b[au_start..sc_start]);
            au_start = sc_start;
            saw_vcl = false;
        }
        if is_vcl {
            saw_vcl = true;
        }
    }
    if au_start < annex_b.len() {
        aus.push(&annex_b[au_start..]);
    }
    aus
}

/// Parses ISO/IEC 14496-15 `avcC` decoder configuration record into `(nal_length_size, annex_b_sps_pps)`.
pub fn parse_avcc_extra_data(avcc: &[u8]) -> (Option<usize>, Vec<u8>) {
    if avcc.len() < 7 || avcc[0] != 1 {
        return (None, Vec::new());
    }
    let nal_length_size = usize::from((avcc[4] & 0x03) + 1);
    let mut annex_b = Vec::new();
    let num_sps = usize::from(avcc[5] & 0x1f);
    let mut off = 6usize;

    for _ in 0..num_sps {
        if off + 2 > avcc.len() {
            return (Some(nal_length_size), annex_b);
        }
        let len = usize::from(u16::from_be_bytes([avcc[off], avcc[off + 1]]));
        off += 2;
        if off + len > avcc.len() {
            return (Some(nal_length_size), annex_b);
        }
        annex_b.extend_from_slice(&[0, 0, 0, 1]);
        annex_b.extend_from_slice(&avcc[off..off + len]);
        off += len;
    }

    if off < avcc.len() {
        let num_pps = usize::from(avcc[off]);
        off += 1;
        for _ in 0..num_pps {
            if off + 2 > avcc.len() {
                break;
            }
            let len = usize::from(u16::from_be_bytes([avcc[off], avcc[off + 1]]));
            off += 2;
            if off + len > avcc.len() {
                break;
            }
            annex_b.extend_from_slice(&[0, 0, 0, 1]);
            annex_b.extend_from_slice(&avcc[off..off + len]);
            off += len;
        }
    }

    (Some(nal_length_size), annex_b)
}

/// Converts an AVCC length-prefixed sample payload into Annex-B start-code delimited NAL units.
pub fn avcc_packet_to_annex_b(data: &[u8], nal_length_size: usize) -> Result<Vec<u8>, MediaError> {
    if !(1..=4).contains(&nal_length_size) {
        return Err(MediaError::VideoDecode(format!(
            "invalid AVCC NAL length size {nal_length_size}"
        )));
    }
    let mut out = Vec::with_capacity(data.len() + 16);
    let mut off = 0usize;
    while off + nal_length_size <= data.len() {
        let mut nal_len = 0usize;
        for &b in &data[off..off + nal_length_size] {
            nal_len = (nal_len << 8) | usize::from(b);
        }
        off += nal_length_size;
        if nal_len == 0 {
            continue;
        }
        let end = off.saturating_add(nal_len);
        if end > data.len() {
            return Err(MediaError::VideoDecode(
                "AVCC NAL length exceeds packet bounds".to_string(),
            ));
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[off..end]);
        off = end;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 3. VP8 Decoder (`vp8-decoder` pure-Rust RFC 6386 keyframe + inter-frame)
// ---------------------------------------------------------------------------

/// Pure-Rust RFC 6386 VP8 decoder supporting full keyframe and inter-frame (P-frame) decoding.
pub struct Vp8VideoDecoder {
    decoder: vp8_decoder::Decoder,
}

impl Vp8VideoDecoder {
    pub fn new() -> Self {
        Self {
            decoder: vp8_decoder::Decoder::new(),
        }
    }
}

impl Default for Vp8VideoDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoDecoder for Vp8VideoDecoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Vec<VideoFrame>, MediaError> {
        if packet.data.is_empty() {
            return Ok(Vec::new());
        }
        let is_vp8_keyframe = (packet.data[0] & 0x01) == 0;
        match self.decoder.decode(&packet.data) {
            Ok(Some(pic)) => {
                let w = pic.width as usize;
                let h = pic.height as usize;
                let cw = pic.chroma_width() as usize;
                let ch = pic.chroma_height() as usize;
                let y_plane = copy_tight_plane_8(pic.y, pic.y_stride, w, h);
                let u_plane = copy_tight_plane_8(pic.u, pic.uv_stride, cw, ch);
                let v_plane = copy_tight_plane_8(pic.v, pic.uv_stride, cw, ch);
                Ok(vec![VideoFrame {
                    width: pic.width,
                    height: pic.height,
                    pixel_format: PixelFormat::Yuv420p,
                    color_matrix: ColorMatrix::Bt601,
                    color_range: ColorRange::Limited,
                    y_plane,
                    u_plane,
                    v_plane,
                    y_stride: pic.width,
                    u_stride: cw as u32,
                    v_stride: cw as u32,
                    pts_us: packet.pts_us,
                    duration_us: packet.duration_us,
                    is_keyframe: is_vp8_keyframe || packet.is_keyframe,
                }])
            }
            // Invisible alt-ref / reference-only frame updates internal DPB with no display output.
            Ok(None) => Ok(Vec::new()),
            Err(err) => Err(MediaError::VideoDecode(format!(
                "vp8 decode failed: {err:?}"
            ))),
        }
    }

    fn flush(&mut self) -> Result<Vec<VideoFrame>, MediaError> {
        Ok(Vec::new())
    }

    fn reset(&mut self) {
        self.decoder = vp8_decoder::Decoder::new();
    }
}

// ---------------------------------------------------------------------------
// 4. VP9 Decoder (`rusty_vp9` pure-Rust full inter-frame + alt-ref support)
// ---------------------------------------------------------------------------

/// Pure-Rust VP9 decoder supporting keyframes, inter-frames, superframes, and alt-ref frames.
pub struct Vp9VideoDecoder {
    decoder: rusty_vp9::Vp9Decoder,
    pending_meta: VecDeque<(u64, bool)>,
}

impl Vp9VideoDecoder {
    pub fn new() -> Self {
        Self {
            decoder: rusty_vp9::Vp9Decoder::new(),
            pending_meta: VecDeque::new(),
        }
    }

    fn drain_frames(&mut self) -> Result<Vec<VideoFrame>, MediaError> {
        let mut out = Vec::new();
        loop {
            match self.decoder.next_frame() {
                Ok(df) => {
                    let (duration_us, is_keyframe) =
                        self.pending_meta.pop_front().unwrap_or((0, false));
                    out.push(convert_vp9_frame(df, duration_us, is_keyframe));
                }
                Err(rusty_vp9::Error::Again) | Err(rusty_vp9::Error::Eof) => break,
                Err(err) => {
                    return Err(MediaError::VideoDecode(format!("vp9 decode failed: {err}")));
                }
            }
        }
        Ok(out)
    }
}

impl Default for Vp9VideoDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoDecoder for Vp9VideoDecoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Vec<VideoFrame>, MediaError> {
        if packet.data.is_empty() {
            return Ok(Vec::new());
        }
        self.pending_meta
            .push_back((packet.duration_us, packet.is_keyframe));
        self.decoder
            .push(&packet.data, Some(packet.pts_us))
            .map_err(|e| MediaError::VideoDecode(format!("vp9 push failed: {e}")))?;
        self.drain_frames()
    }

    fn flush(&mut self) -> Result<Vec<VideoFrame>, MediaError> {
        self.decoder.flush();
        self.drain_frames()
    }

    fn reset(&mut self) {
        self.decoder = rusty_vp9::Vp9Decoder::new();
        self.pending_meta.clear();
    }
}

fn convert_vp9_frame(
    df: rusty_vp9::DecodedFrame,
    duration_us: u64,
    is_keyframe: bool,
) -> VideoFrame {
    let w = df.width as usize;
    let h = df.height as usize;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let src_cw = if df.subsampling_x == 0 { w } else { cw };
    let src_ch = if df.subsampling_y == 0 { h } else { ch };
    let hbd = df.bit_depth > 8;
    let shift = df.bit_depth.saturating_sub(8);

    let y_stride = df.strides.first().copied().unwrap_or(w);
    let u_stride = df.strides.get(1).copied().unwrap_or(src_cw);
    let v_stride = df.strides.get(2).copied().unwrap_or(src_cw);

    let y_plane = if hbd {
        copy_tight_plane_hbd_to_8(&df.planes[0], y_stride, w, h, shift)
    } else {
        copy_tight_plane_8(&df.planes[0], y_stride, w, h)
    };
    let u_raw = if hbd {
        copy_tight_plane_hbd_to_8(&df.planes[1], u_stride, src_cw, src_ch, shift)
    } else {
        copy_tight_plane_8(&df.planes[1], u_stride, src_cw, src_ch)
    };
    let v_raw = if hbd {
        copy_tight_plane_hbd_to_8(&df.planes[2], v_stride, src_cw, src_ch, shift)
    } else {
        copy_tight_plane_8(&df.planes[2], v_stride, src_cw, src_ch)
    };

    let u_plane = subsample_chroma_to_420(&u_raw, src_cw, src_ch, cw, ch);
    let v_plane = subsample_chroma_to_420(&v_raw, src_cw, src_ch, cw, ch);

    VideoFrame {
        width: df.width,
        height: df.height,
        pixel_format: PixelFormat::Yuv420p,
        color_matrix: ColorMatrix::Bt709,
        color_range: ColorRange::Limited,
        y_plane,
        u_plane,
        v_plane,
        y_stride: df.width,
        u_stride: cw as u32,
        v_stride: cw as u32,
        pts_us: df.pts.unwrap_or(0),
        duration_us,
        is_keyframe,
    }
}
