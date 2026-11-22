//! Zero-copy planar YUV420p / NV12 frame transport over `rix-ipc` [`TripleBufferShmem`].
//!
//! Layout inside each triple-buffer slot (fixed 64-byte little-endian header + tight/strided planes):
//! - `[0..4]`   magic `b"RYUV"`
//! - `[4..8]`   version (`1u32`)
//! - `[8..12]`  width (`u32`)
//! - `[12..16]` height (`u32`)
//! - `[16..20]` pixel_format (`0 = Yuv420p`, `1 = Nv12`)
//! - `[20..24]` color_matrix (`0 = Bt709`, `1 = Bt601`)
//! - `[24..28]` color_range (`0 = Limited`, `1 = Full`)
//! - `[28..32]` y_stride (`u32`)
//! - `[32..36]` u_stride (`u32`)
//! - `[36..40]` v_stride (`u32`)
//! - `[40..44]` y_len (`u32`)
//! - `[44..48]` u_len (`u32`)
//! - `[48..52]` v_len (`u32`)
//! - `[52..56]` flags (`bit 0 = is_keyframe`)
//! - `[56..64]` pts_us (`i64`)
//! - `[64..]`   `y_plane`, `u_plane`, `v_plane` bytes back-to-back

use rix_ipc::{TripleBufferConsumer, TripleBufferProducer, TripleBufferShmem};

use crate::types::{ColorMatrix, ColorRange, MediaError, PixelFormat, VideoFrame};

/// 4-byte binary header magic for shared-memory planar YUV frames.
pub const YUV_SHMEM_MAGIC: [u8; 4] = *b"RYUV";
/// Binary header version.
pub const YUV_SHMEM_VERSION: u32 = 1;
/// Fixed header byte size preceding the Y/U/V planes.
pub const YUV_SHMEM_HEADER_BYTES: usize = 64;

/// Computes the triple-buffer slot capacity (in bytes) required to hold a planar YUV420p/NV12
/// frame up to `max_width x max_height`.
#[must_use]
pub fn yuv_shmem_slot_capacity(max_width: u32, max_height: u32) -> usize {
    let w = max_width as usize;
    let h = max_height as usize;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    // 4:2:0 uses `w * h + 2 * cw * ch` bytes; add small alignment slack for padded decoder strides.
    YUV_SHMEM_HEADER_BYTES + (w + 64) * h + 2 * (cw + 64) * ch
}

/// Zero-copy borrowed view of a planar YUV420p or NV12 frame living directly inside shared memory.
#[derive(Debug, Clone, Copy)]
pub struct BorrowedYuvFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub pixel_format: PixelFormat,
    pub color_matrix: ColorMatrix,
    pub color_range: ColorRange,
    pub y_stride: u32,
    pub u_stride: u32,
    pub v_stride: u32,
    pub pts_us: i64,
    pub is_keyframe: bool,
    pub y_plane: &'a [u8],
    pub u_plane: &'a [u8],
    pub v_plane: &'a [u8],
}

/// Encodes `frame` into `dst` (clearing and reusing `dst`'s allocation) with the 64-byte `RYUV` header.
pub fn encode_video_frame_to_slot(frame: &VideoFrame, dst: &mut Vec<u8>) {
    let total =
        YUV_SHMEM_HEADER_BYTES + frame.y_plane.len() + frame.u_plane.len() + frame.v_plane.len();
    dst.clear();
    dst.reserve(total);

    let pix_fmt: u32 = match frame.pixel_format {
        PixelFormat::Yuv420p => 0,
        PixelFormat::Nv12 => 1,
    };
    let matrix: u32 = match frame.color_matrix {
        ColorMatrix::Bt709 => 0,
        ColorMatrix::Bt601 => 1,
    };
    let range: u32 = match frame.color_range {
        ColorRange::Limited => 0,
        ColorRange::Full => 1,
    };
    let flags: u32 = u32::from(frame.is_keyframe);

    dst.extend_from_slice(&YUV_SHMEM_MAGIC);
    dst.extend_from_slice(&YUV_SHMEM_VERSION.to_le_bytes());
    dst.extend_from_slice(&frame.width.to_le_bytes());
    dst.extend_from_slice(&frame.height.to_le_bytes());
    dst.extend_from_slice(&pix_fmt.to_le_bytes());
    dst.extend_from_slice(&matrix.to_le_bytes());
    dst.extend_from_slice(&range.to_le_bytes());
    dst.extend_from_slice(&frame.y_stride.to_le_bytes());
    dst.extend_from_slice(&frame.u_stride.to_le_bytes());
    dst.extend_from_slice(&frame.v_stride.to_le_bytes());
    dst.extend_from_slice(&(frame.y_plane.len() as u32).to_le_bytes());
    dst.extend_from_slice(&(frame.u_plane.len() as u32).to_le_bytes());
    dst.extend_from_slice(&(frame.v_plane.len() as u32).to_le_bytes());
    dst.extend_from_slice(&flags.to_le_bytes());
    dst.extend_from_slice(&frame.pts_us.to_le_bytes());
    debug_assert_eq!(dst.len(), YUV_SHMEM_HEADER_BYTES);

    dst.extend_from_slice(&frame.y_plane);
    dst.extend_from_slice(&frame.u_plane);
    dst.extend_from_slice(&frame.v_plane);
}

/// Decodes a borrowed planar YUV frame directly from a shared-memory slot slice with zero copies.
#[must_use]
pub fn decode_video_frame_from_slot(slot: &[u8]) -> Option<BorrowedYuvFrame<'_>> {
    if slot.len() < YUV_SHMEM_HEADER_BYTES || slot[0..4] != YUV_SHMEM_MAGIC {
        return None;
    }
    let u32_at = |off: usize| -> u32 {
        u32::from_le_bytes([slot[off], slot[off + 1], slot[off + 2], slot[off + 3]])
    };
    if u32_at(4) != YUV_SHMEM_VERSION {
        return None;
    }
    let width = u32_at(8);
    let height = u32_at(12);
    let pixel_format = match u32_at(16) {
        0 => PixelFormat::Yuv420p,
        1 => PixelFormat::Nv12,
        _ => return None,
    };
    let color_matrix = match u32_at(20) {
        0 => ColorMatrix::Bt709,
        1 => ColorMatrix::Bt601,
        _ => return None,
    };
    let color_range = match u32_at(24) {
        0 => ColorRange::Limited,
        1 => ColorRange::Full,
        _ => return None,
    };
    let y_stride = u32_at(28);
    let u_stride = u32_at(32);
    let v_stride = u32_at(36);
    let y_len = u32_at(40) as usize;
    let u_len = u32_at(44) as usize;
    let v_len = u32_at(48) as usize;
    let flags = u32_at(52);
    let pts_us = i64::from_le_bytes([
        slot[56], slot[57], slot[58], slot[59], slot[60], slot[61], slot[62], slot[63],
    ]);

    let y_end = YUV_SHMEM_HEADER_BYTES.checked_add(y_len)?;
    let u_end = y_end.checked_add(u_len)?;
    let v_end = u_end.checked_add(v_len)?;
    if v_end > slot.len() {
        return None;
    }

    Some(BorrowedYuvFrame {
        width,
        height,
        pixel_format,
        color_matrix,
        color_range,
        y_stride,
        u_stride,
        v_stride,
        pts_us,
        is_keyframe: (flags & 1) != 0,
        y_plane: &slot[YUV_SHMEM_HEADER_BYTES..y_end],
        u_plane: &slot[y_end..u_end],
        v_plane: &slot[u_end..v_end],
    })
}

/// Producer endpoint writing planar YUV420p / NV12 frames into triple-buffered shared memory.
pub struct VideoFrameProducer {
    inner: TripleBufferProducer,
    scratch: Vec<u8>,
}

/// Consumer endpoint reading planar YUV420p / NV12 frames directly from triple-buffered shared memory.
pub struct VideoFrameConsumer {
    inner: TripleBufferConsumer,
}

/// Creates a connected [`VideoFrameProducer`] + [`VideoFrameConsumer`] pair backed by `memfd`
/// [`TripleBufferShmem`] sized for frames up to `max_width x max_height`.
pub fn create_video_frame_channel(
    max_width: u32,
    max_height: u32,
) -> Result<(VideoFrameProducer, VideoFrameConsumer), MediaError> {
    let cap = yuv_shmem_slot_capacity(max_width, max_height);
    let shmem = TripleBufferShmem::create(cap).map_err(|e| {
        MediaError::InvalidConfig(format!("triple-buffer shmem create failed: {e}"))
    })?;
    let (prod, cons) = shmem
        .into_pair()
        .map_err(|e| MediaError::InvalidConfig(format!("triple-buffer shmem pair failed: {e}")))?;
    Ok((
        VideoFrameProducer {
            inner: prod,
            scratch: Vec::with_capacity(cap),
        },
        VideoFrameConsumer { inner: cons },
    ))
}

impl VideoFrameProducer {
    /// Publishes a planar YUV420p / NV12 frame into the next shared-memory slot without any
    /// CPU YUV->RGBA conversion, returning the monotonic frame generation number.
    pub fn publish(&mut self, frame: &VideoFrame) -> Result<u64, MediaError> {
        encode_video_frame_to_slot(frame, &mut self.scratch);
        self.inner
            .write_frame(&self.scratch)
            .map_err(|e| MediaError::VideoDecode(format!("shmem publish failed: {e}")))
    }

    /// Returns the raw underlying [`TripleBufferProducer`].
    pub fn inner(&self) -> &TripleBufferProducer {
        &self.inner
    }
}

impl VideoFrameConsumer {
    /// Atomically acquires the latest published planar YUV frame bytes (returning `None` if no new frame).
    pub fn try_acquire_slot(&mut self) -> Option<&[u8]> {
        self.inner.try_acquire()
    }

    /// Atomically acquires and parses the latest published planar YUV frame zero-copy.
    pub fn try_acquire_frame(&mut self) -> Option<BorrowedYuvFrame<'_>> {
        let slot = self.inner.try_acquire()?;
        decode_video_frame_from_slot(slot)
    }

    /// Returns the monotonic generation counter of the last acquired frame.
    pub fn generation(&self) -> u64 {
        self.inner.generation()
    }
}
