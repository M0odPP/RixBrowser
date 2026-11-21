use crate::error::{IpcError, Result};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{de::DeserializeOwned, Serialize};

pub const HEADER_SIZE: usize = 8;
pub const PROTOCOL_VERSION: u8 = 1;
pub const MAX_CONTROL_FRAME_BYTES: usize = 4 * 1024 * 1024;
pub const RECOMMENDED_SHMEM_THRESHOLD_BYTES: usize = 64 * 1024;

pub const FLAG_NONE: u32 = 0;
pub const FLAG_HAS_FDS: u32 = 1 << 8;
pub const KNOWN_FLAGS_MASK: u32 = FLAG_HAS_FDS;

const VERSION_MASK: u32 = 0xFF;
const INITIAL_BUFFER_CAPACITY: usize = 8192;
const COMPACT_BUFFER_THRESHOLD: usize = 256 * 1024;
const MIN_READ_SPARE_BYTES: usize = 1024;
const READ_RESERVE_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub payload_len: u32,
    pub version: u8,
    pub flags: u32,
}

impl FrameHeader {
    pub fn new(payload_len: u32, flags: u32) -> Result<Self> {
        Self::with_max(payload_len, flags, MAX_CONTROL_FRAME_BYTES)
    }

    pub fn with_max(payload_len: u32, flags: u32, max_frame_bytes: usize) -> Result<Self> {
        if (flags & !KNOWN_FLAGS_MASK) != 0 {
            return Err(IpcError::UnexpectedFlags(flags));
        }
        let size = payload_len as usize;
        if size > max_frame_bytes {
            return Err(IpcError::FrameTooLarge {
                size,
                max: max_frame_bytes,
            });
        }
        Ok(Self {
            payload_len,
            version: PROTOCOL_VERSION,
            flags,
        })
    }

    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut out = [0u8; HEADER_SIZE];
        let flags_and_ver = (self.flags & !VERSION_MASK) | u32::from(self.version);
        out[0..4].copy_from_slice(&self.payload_len.to_le_bytes());
        out[4..8].copy_from_slice(&flags_and_ver.to_le_bytes());
        out
    }

    pub fn decode(bytes: [u8; HEADER_SIZE], max_frame_bytes: usize) -> Result<Self> {
        let payload_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let flags_and_ver = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let version = (flags_and_ver & VERSION_MASK) as u8;
        let flags = flags_and_ver & !VERSION_MASK;

        if version != PROTOCOL_VERSION {
            return Err(IpcError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                actual: version,
            });
        }
        if (flags & !KNOWN_FLAGS_MASK) != 0 {
            return Err(IpcError::UnexpectedFlags(flags));
        }
        let size = payload_len as usize;
        if size > max_frame_bytes {
            return Err(IpcError::FrameTooLarge {
                size,
                max: max_frame_bytes,
            });
        }

        Ok(Self {
            payload_len,
            version,
            flags,
        })
    }
}

pub fn encode_message<T: Serialize>(
    msg: &T,
    flags: u32,
    max_frame_bytes: usize,
    out: &mut BytesMut,
) -> Result<usize> {
    let payload = postcard::to_stdvec(msg)?;
    if payload.len() > max_frame_bytes {
        return Err(IpcError::FrameTooLarge {
            size: payload.len(),
            max: max_frame_bytes,
        });
    }
    let payload_len = u32::try_from(payload.len()).map_err(|_| IpcError::FrameTooLarge {
        size: payload.len(),
        max: max_frame_bytes,
    })?;
    let header = FrameHeader::with_max(payload_len, flags, max_frame_bytes)?;

    if payload.len() > RECOMMENDED_SHMEM_THRESHOLD_BYTES {
        tracing::debug!(
            payload_bytes = payload.len(),
            threshold_bytes = RECOMMENDED_SHMEM_THRESHOLD_BYTES,
            "control frame payload exceeds shmem threshold; consider transferring via ShmemRef"
        );
    }

    let total_len = HEADER_SIZE + payload.len();
    out.reserve(total_len);
    out.put_slice(&header.encode());
    out.put_slice(&payload);
    Ok(total_len)
}

pub fn decode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T> {
    Ok(postcard::from_bytes(payload)?)
}

#[derive(Debug)]
pub struct FrameDecoder {
    buf: BytesMut,
    max_frame_bytes: usize,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::with_max_frame_bytes(MAX_CONTROL_FRAME_BYTES)
    }

    pub fn with_max_frame_bytes(max_frame_bytes: usize) -> Self {
        Self {
            buf: BytesMut::with_capacity(INITIAL_BUFFER_CAPACITY),
            max_frame_bytes,
        }
    }

    pub fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    pub fn buf_mut(&mut self) -> &mut BytesMut {
        if self.buf.capacity().saturating_sub(self.buf.len()) < MIN_READ_SPARE_BYTES {
            self.buf.reserve(READ_RESERVE_BYTES);
        }
        &mut self.buf
    }

    pub fn has_partial_frame(&self) -> bool {
        !self.buf.is_empty()
    }

    pub fn try_decode(&mut self) -> Result<Option<(FrameHeader, Bytes)>> {
        if self.buf.len() < HEADER_SIZE {
            return Ok(None);
        }

        let mut hdr_bytes = [0u8; HEADER_SIZE];
        hdr_bytes.copy_from_slice(&self.buf[..HEADER_SIZE]);
        let header = FrameHeader::decode(hdr_bytes, self.max_frame_bytes)?;

        let payload_len = header.payload_len as usize;
        let total_len = HEADER_SIZE
            .checked_add(payload_len)
            .ok_or(IpcError::FrameTooLarge {
                size: payload_len,
                max: self.max_frame_bytes,
            })?;

        if self.buf.len() < total_len {
            self.buf.reserve(total_len - self.buf.len());
            return Ok(None);
        }

        self.buf.advance(HEADER_SIZE);
        let payload = self.buf.split_to(payload_len).freeze();

        if self.buf.is_empty() && self.buf.capacity() > COMPACT_BUFFER_THRESHOLD {
            self.buf = BytesMut::with_capacity(INITIAL_BUFFER_CAPACITY);
        }

        Ok(Some((header, payload)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{BrowserToContent, PipelineId, ViewportMetrics};
    use rix_core::Size;

    #[test]
    fn header_encode_decode_roundtrip() {
        let header = FrameHeader::new(1024, FLAG_HAS_FDS).expect("valid header");
        let encoded = header.encode();
        let decoded = FrameHeader::decode(encoded, MAX_CONTROL_FRAME_BYTES).expect("valid decode");
        assert_eq!(decoded, header);
    }

    #[test]
    fn decode_rejects_version_mismatch() {
        let mut raw = FrameHeader::new(16, FLAG_NONE).expect("header").encode();
        raw[4] = 2; // version = 2
        let err = FrameHeader::decode(raw, MAX_CONTROL_FRAME_BYTES).unwrap_err();
        assert!(matches!(
            err,
            IpcError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                actual: 2
            }
        ));

        raw[4] = 0; // version = 0
        let err_zero = FrameHeader::decode(raw, MAX_CONTROL_FRAME_BYTES).unwrap_err();
        assert!(matches!(
            err_zero,
            IpcError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                actual: 0
            }
        ));
    }

    #[test]
    fn decode_rejects_unknown_flags() {
        let mut raw = FrameHeader::new(16, FLAG_NONE).expect("header").encode();
        let bad_flags_and_ver = (1u32 << 12) | u32::from(PROTOCOL_VERSION);
        raw[4..8].copy_from_slice(&bad_flags_and_ver.to_le_bytes());
        let err = FrameHeader::decode(raw, MAX_CONTROL_FRAME_BYTES).unwrap_err();
        assert!(matches!(err, IpcError::UnexpectedFlags(0x1000)));
    }

    #[test]
    fn decoder_rejects_oversized_header_before_payload_allocation() {
        let oversized = (MAX_CONTROL_FRAME_BYTES + 1) as u32;
        let mut raw = [0u8; HEADER_SIZE];
        raw[0..4].copy_from_slice(&oversized.to_le_bytes());
        raw[4..8].copy_from_slice(&u32::from(PROTOCOL_VERSION).to_le_bytes());

        let mut decoder = FrameDecoder::new();
        let cap_before = decoder.buf.capacity();
        decoder.buf_mut().extend_from_slice(&raw);

        let err = decoder.try_decode().unwrap_err();
        assert!(matches!(
            err,
            IpcError::FrameTooLarge {
                size,
                max: MAX_CONTROL_FRAME_BYTES
            } if size == MAX_CONTROL_FRAME_BYTES + 1
        ));
        assert_eq!(decoder.buf.capacity(), cap_before);
    }

    #[test]
    fn encode_rejects_oversized_payload() {
        let msg = vec![0xABu8; 256];
        let mut out = BytesMut::new();
        let err = encode_message(&msg, FLAG_NONE, 128, &mut out).unwrap_err();
        assert!(matches!(err, IpcError::FrameTooLarge { max: 128, .. }));
        assert!(out.is_empty());
    }

    #[test]
    fn decoder_handles_one_byte_at_a_time() {
        let msg = BrowserToContent::ViewportChanged {
            pipeline: PipelineId(7),
            viewport: ViewportMetrics {
                logical_size: Size::new(1440.0, 900.0),
                device_pixel_ratio: 2.0,
            },
        };
        let mut wire = BytesMut::new();
        encode_message(&msg, FLAG_NONE, MAX_CONTROL_FRAME_BYTES, &mut wire).expect("encode");

        let mut decoder = FrameDecoder::new();
        for &byte in &wire[..wire.len() - 1] {
            decoder.buf_mut().put_u8(byte);
            assert!(decoder.try_decode().expect("no decode error").is_none());
        }
        decoder.buf_mut().put_u8(wire[wire.len() - 1]);
        let (header, payload) = decoder
            .try_decode()
            .expect("decode")
            .expect("complete frame");
        assert_eq!(header.version, PROTOCOL_VERSION);
        let decoded: BrowserToContent = decode_payload(&payload).expect("postcard");
        assert_eq!(decoded, msg);
        assert!(!decoder.has_partial_frame());
    }

    #[test]
    fn decoder_handles_coalesced_frames() {
        let mut wire = BytesMut::new();
        let expected: Vec<BrowserToContent> = (0..10u64)
            .map(|seq| BrowserToContent::Ping { seq })
            .collect();
        for msg in &expected {
            encode_message(msg, FLAG_NONE, MAX_CONTROL_FRAME_BYTES, &mut wire).expect("encode");
        }

        let mut decoder = FrameDecoder::new();
        decoder.buf_mut().extend_from_slice(&wire);

        let mut actual = Vec::new();
        while let Some((_, payload)) = decoder.try_decode().expect("decode") {
            actual.push(decode_payload::<BrowserToContent>(&payload).expect("postcard"));
        }
        assert_eq!(actual, expected);
        assert!(!decoder.has_partial_frame());
    }
}
