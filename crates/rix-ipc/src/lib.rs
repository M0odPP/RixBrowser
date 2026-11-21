#![deny(unsafe_op_in_unsafe_fn)]
#![deny(unsafe_code)]

pub mod error;
pub mod frame;
pub mod protocol;
pub mod transport;

pub use error::{IpcError, Result};
pub use frame::{
    decode_payload, encode_message, FrameDecoder, FrameHeader, FLAG_HAS_FDS, FLAG_NONE,
    HEADER_SIZE, KNOWN_FLAGS_MASK, MAX_CONTROL_FRAME_BYTES, PROTOCOL_VERSION,
    RECOMMENDED_SHMEM_THRESHOLD_BYTES,
};
pub use protocol::{
    BrowserToContent, CompositorToContent, ContentToBrowser, ContentToCompositor, ContentToNetwork,
    CredentialsMode, CursorIcon, FetchMode, FontResourceId, FrameId, FrameTelemetry,
    HttpMethodKind, ImageId, KeyModifiers, LoadState, MissingFeatureKind, MouseButtonKind,
    NavigationId, NavigationTiming, NetworkToContent, PaintStats, PathResourceId, PipelineId,
    PixelFormat, Point, ProcessId, Rect, RequestId, RoutedInputEvent, ScrollId, ScrollNodeUpdate,
    ScrollTreeUpdate, ShmemId, ShmemRef, Size, SpatialNodeId, StreamId, SurfaceId, TabId,
    ViewportMetrics, WebSocketId, WindowId,
};
#[cfg(unix)]
pub use transport::{
    connect_unix, unix_pair, IpcListener, UnixIpcChannel, UnixIpcReceiver, UnixIpcSender,
};
pub use transport::{FramedChannel, FramedReceiver, FramedSender};
