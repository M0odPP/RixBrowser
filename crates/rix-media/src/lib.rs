#![deny(unsafe_code)]
//! Pure-Rust media container demuxers, video/audio decoders, MSE, and WebAudio DSP for RixBrowser.

pub mod audio;
pub mod demuxer;
pub mod mse;
pub mod types;
pub mod video;

pub use audio::{
    build_audio_codec_registry, create_audio_decoder, decode_container_audio, AudioDecoder,
};
pub use demuxer::{demux_bytes, detect_container};
pub use mse::{
    is_type_supported, AppendMode, AppendResult, CodedFrame, EndOfStreamError, MediaSourceEngine,
    MseError, ReadyState, SourceBufferEngine, TimeRanges, DEFAULT_SOURCE_BUFFER_QUOTA_BYTES,
};
pub use types::{
    AudioCodec, AudioFrame, AudioTrackConfig, ColorMatrix, ColorRange, ContainerFormat,
    DemuxedMedia, EncodedPacket, MediaError, PacketKind, PcmSampleFormat, PixelFormat, TrackInfo,
    TrackKind, VideoCodec, VideoFrame, VideoTrackConfig,
};
pub use video::{
    create_video_decoder, Av1VideoDecoder, H264VideoDecoder, VideoDecoder, Vp8VideoDecoder,
    Vp9VideoDecoder,
};
