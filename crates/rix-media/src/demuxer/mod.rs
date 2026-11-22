//! Unified container detection and demuxing entry points for MP4/fMP4, WebM/Matroska, Ogg, WAV, MP3, and FLAC.

pub mod mp4;
pub mod symphonia_demux;
pub mod webm;

use crate::types::{ContainerFormat, DemuxedMedia, MediaError};

/// Detects the container format from magic header bytes.
pub fn detect_container(bytes: &[u8]) -> Option<ContainerFormat> {
    if bytes.len() >= 4 && bytes[0..4] == [0x1a, 0x45, 0xdf, 0xa3] {
        return Some(ContainerFormat::WebM);
    }
    if bytes.len() >= 4 && &bytes[0..4] == b"OggS" {
        return Some(ContainerFormat::Ogg);
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        return Some(ContainerFormat::Wav);
    }
    if bytes.len() >= 4 && &bytes[0..4] == b"fLaC" {
        return Some(ContainerFormat::Flac);
    }
    if bytes.len() >= 8
        && matches!(
            &bytes[4..8],
            b"ftyp" | b"moov" | b"moof" | b"styp" | b"sidx" | b"free" | b"wide" | b"mdat"
        )
    {
        return Some(ContainerFormat::Mp4);
    }
    if bytes.len() >= 3 && &bytes[0..3] == b"ID3" {
        return Some(ContainerFormat::Mp3);
    }
    // MPEG-1/2 Layer III frame sync (`11111111 111xxxxx` with valid layer bits `01`).
    if bytes.len() >= 2 && bytes[0] == 0xff && (bytes[1] & 0xe6) == 0xe2 {
        return Some(ContainerFormat::Mp3);
    }
    None
}

/// Demuxes any supported media container byte buffer into normalized tracks and packets.
pub fn demux_bytes(
    bytes: &[u8],
    format_hint: Option<ContainerFormat>,
) -> Result<DemuxedMedia, MediaError> {
    let container = format_hint
        .or_else(|| detect_container(bytes))
        .ok_or(MediaError::UnsupportedContainer)?;

    match container {
        ContainerFormat::Mp4 => mp4::demux_mp4(bytes),
        ContainerFormat::WebM => webm::demux_webm(bytes),
        ContainerFormat::Ogg => {
            symphonia_demux::demux_with_symphonia(bytes, container, Some("ogg"))
        }
        ContainerFormat::Wav => {
            symphonia_demux::demux_with_symphonia(bytes, container, Some("wav"))
        }
        ContainerFormat::Mp3 => {
            symphonia_demux::demux_with_symphonia(bytes, container, Some("mp3"))
        }
        ContainerFormat::Flac => {
            symphonia_demux::demux_with_symphonia(bytes, container, Some("flac"))
        }
    }
}
