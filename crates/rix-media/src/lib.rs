#![deny(unsafe_code)]
//! Pure-Rust media container demuxers, video/audio decoders, MSE, and WebAudio DSP for RixBrowser.

pub mod audio;
pub mod demuxer;
pub mod types;
pub mod video;

pub use audio::{
    build_audio_codec_registry, create_audio_decoder, decode_container_audio, AudioDecoder,
};
pub use demuxer::{demux_bytes, detect_container};
pub use types::{
    AudioCodec, AudioFrame, AudioTrackConfig, ColorMatrix, ColorRange, ContainerFormat,
    DemuxedMedia, EncodedPacket, MediaError, PacketKind, PcmSampleFormat, PixelFormat, TrackInfo,
    TrackKind, VideoCodec, VideoFrame, VideoTrackConfig,
};
pub use video::{
    create_video_decoder, Av1VideoDecoder, H264VideoDecoder, VideoDecoder, Vp8VideoDecoder,
    Vp9VideoDecoder,
};
