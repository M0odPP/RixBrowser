#![deny(unsafe_code)]
//! Pure-Rust media container demuxers, video/audio decoders, MSE, and WebAudio DSP for RixBrowser.

pub mod audio;
pub mod audio_clock;
pub mod demuxer;
pub mod mse;
pub mod types;
pub mod video;
pub mod video_transport;
pub mod webaudio;

pub use audio::{
    build_audio_codec_registry, create_audio_decoder, decode_container_audio, AudioDecoder,
};
pub use audio_clock::{
    AudioOutputDevice, AudioOutputMode, AudioSampleQueue, AvSyncDecision, MediaClock,
    AV_SYNC_TOLERANCE_US,
};
pub use demuxer::{demux_bytes, detect_container};
pub use mse::{
    is_type_supported, AppendMode, AppendResult, CodedFrame, EndOfStreamError, MediaSourceEngine,
    MseError, PendingBufferOp, ReadyState, SourceBufferEngine, TimeRanges,
    DEFAULT_SOURCE_BUFFER_QUOTA_BYTES,
};
pub use types::{
    AudioCodec, AudioFrame, AudioTrackConfig, ColorMatrix, ColorRange, ContainerFormat,
    DemuxedMedia, EncodedPacket, MediaError, MseSample, PacketKind, PcmSampleFormat, PixelFormat,
    TrackInfo, TrackKind, VideoCodec, VideoFrame, VideoTrackConfig,
};
pub use video::{
    create_video_decoder, Av1VideoDecoder, H264VideoDecoder, VideoDecoder, Vp8VideoDecoder,
    Vp9VideoDecoder,
};
pub use video_transport::{
    create_video_frame_channel, decode_video_frame_from_slot, encode_video_frame_to_slot,
    yuv_shmem_slot_capacity, BorrowedYuvFrame, VideoFrameConsumer, VideoFrameProducer,
    YUV_SHMEM_HEADER_BYTES, YUV_SHMEM_MAGIC, YUV_SHMEM_VERSION,
};
pub use webaudio::{
    AnalyserState, AudioBuffer, AudioGraph, AudioNodeId, AudioNodeKind, AudioParam,
    BiquadFilterState, BiquadFilterType, OscillatorType, RENDER_QUANTUM_FRAMES,
};
