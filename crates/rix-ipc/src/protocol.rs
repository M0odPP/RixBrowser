pub use rix_core::{NavigationTiming, Point, ProcessId, Rect, Size, TabId, Vec2, WindowId};
use serde::{Deserialize, Serialize};

macro_rules! define_id {
    ($name:ident, $inner:ty) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        pub struct $name(pub $inner);
    };
}

define_id!(FrameId, u64);
define_id!(NavigationId, u64);
define_id!(PipelineId, u64);
define_id!(RequestId, u64);
define_id!(StreamId, u64);
define_id!(WebSocketId, u64);
define_id!(ImageId, u64);
define_id!(FontResourceId, u64);
define_id!(PathResourceId, u64);
define_id!(SurfaceId, u64);
define_id!(ScrollId, u64);
define_id!(SpatialNodeId, u32);
define_id!(ShmemId, u64);
define_id!(MessagePortId, u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShmemRef {
    pub id: ShmemId,
    pub offset: u32,
    pub len: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ViewportMetrics {
    pub logical_size: Size,
    pub device_pixel_ratio: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoadState {
    Started,
    Committed,
    DomInteractive,
    Complete,
    Failed { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HttpMethodKind {
    Get,
    Post,
    Head,
    Put,
    Delete,
    Patch,
    Options,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FetchMode {
    Cors,
    NoCors,
    SameOrigin,
    Navigate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CredentialsMode {
    Omit,
    SameOrigin,
    Include,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CursorIcon {
    #[default]
    Default,
    Pointer,
    Text,
    Crosshair,
    Move,
    NotAllowed,
    Grab,
    Grabbing,
    Wait,
    Progress,
    ResizeNs,
    ResizeEw,
    ResizeNesw,
    ResizeNwse,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KeyModifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButtonKind {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    Other(u16),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RoutedInputEvent {
    MouseMove {
        position: Point,
        modifiers: KeyModifiers,
    },
    MouseDown {
        position: Point,
        button: MouseButtonKind,
        click_count: u8,
        modifiers: KeyModifiers,
    },
    MouseUp {
        position: Point,
        button: MouseButtonKind,
        modifiers: KeyModifiers,
    },
    Wheel {
        position: Point,
        delta_x: f32,
        delta_y: f32,
        modifiers: KeyModifiers,
    },
    KeyDown {
        key: String,
        code: String,
        repeat: bool,
        modifiers: KeyModifiers,
    },
    KeyUp {
        key: String,
        code: String,
        modifiers: KeyModifiers,
    },
    Focus {
        focused: bool,
    },
    Paste {
        text: String,
    },
    Copy,
    Cut,
    SelectAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct FrameTelemetry {
    pub frame_seq: u64,
    pub style_ms: f32,
    pub layout_ms: f32,
    pub paint_ms: f32,
    pub ipc_bytes: u32,
    pub dom_nodes: u32,
    pub resident_memory_kb: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MissingFeatureKind {
    CssProperty,
    CssAtRule,
    CssSelector,
    WebApi,
    MediaCodec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    Rgba8Unorm,
    Bgra8Unorm,
    R8Unorm,
    Rg8Unorm,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct PaintStats {
    pub display_items: u32,
    pub spatial_nodes: u32,
    pub build_ms: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScrollNodeUpdate {
    pub id: SpatialNodeId,
    pub scroll_id: ScrollId,
    pub content_rect: Rect,
    pub viewport_rect: Rect,
    pub offset: Point,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ScrollTreeUpdate {
    pub nodes: Vec<ScrollNodeUpdate>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BrowserToContent {
    CreatePipeline {
        pipeline: PipelineId,
        tab: TabId,
        viewport: ViewportMetrics,
    },
    DestroyPipeline {
        pipeline: PipelineId,
    },
    CommitNavigation {
        pipeline: PipelineId,
        nav_id: NavigationId,
        final_url: String,
        mime_type: String,
        stream_id: StreamId,
    },
    ViewportChanged {
        pipeline: PipelineId,
        viewport: ViewportMetrics,
    },
    InputEvent {
        pipeline: PipelineId,
        event: RoutedInputEvent,
    },
    ScrollOffsetSync {
        pipeline: PipelineId,
        node_id: SpatialNodeId,
        offset: Point,
    },
    BeginFrame {
        pipeline: PipelineId,
        frame_seq: u64,
        vsync_ns: u64,
    },
    LoadDocument {
        pipeline: PipelineId,
        url: String,
        html: String,
        sandbox: Option<String>,
    },
    EvalScript {
        pipeline: PipelineId,
        seq: u64,
        source: String,
    },
    DeliverPostMessage {
        target_pipeline: PipelineId,
        source_pipeline: PipelineId,
        data_json: String,
        origin: String,
        transfer_ports: Vec<MessagePortId>,
    },
    DeliverPortMessage {
        target_pipeline: PipelineId,
        port_id: MessagePortId,
        data_json: String,
        transfer_ports: Vec<MessagePortId>,
    },
    ChildFrameBound {
        parent_pipeline: PipelineId,
        frame_node_id: u64,
        child_pipeline: PipelineId,
        surface_id: SurfaceId,
        origin: String,
        is_cross_origin: bool,
    },
    ChildFrameCrashed {
        parent_pipeline: PipelineId,
        frame_node_id: u64,
        child_pipeline: PipelineId,
        surface_id: SurfaceId,
    },
    RequestDisplayList {
        pipeline: PipelineId,
        epoch: u64,
    },
    ThemeChanged {
        dark: bool,
    },
    Ping {
        seq: u64,
    },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContentToBrowser {
    PipelineReady {
        pipeline: PipelineId,
    },
    TitleChanged {
        pipeline: PipelineId,
        title: String,
    },
    FaviconChanged {
        pipeline: PipelineId,
        url: String,
    },
    LoadStateChanged {
        pipeline: PipelineId,
        state: LoadState,
    },
    NavigationRequested {
        pipeline: PipelineId,
        url: String,
        method: HttpMethodKind,
        form_body: Option<Vec<u8>>,
        content_type: Option<String>,
    },
    CursorChanged {
        pipeline: PipelineId,
        cursor: CursorIcon,
    },
    FocusChanged {
        pipeline: PipelineId,
        editable: bool,
        selection_text: Option<String>,
    },
    RequestAnimationFrame {
        pipeline: PipelineId,
    },
    TelemetrySample {
        pipeline: PipelineId,
        sample: FrameTelemetry,
    },
    MissingFeatureReported {
        pipeline: PipelineId,
        kind: MissingFeatureKind,
        name: String,
    },
    CreateChildFrame {
        parent_pipeline: PipelineId,
        frame_node_id: u64,
        surface_id: SurfaceId,
        url: String,
        srcdoc: Option<String>,
        sandbox: Option<String>,
        width: u32,
        height: u32,
    },
    NavigateChildFrame {
        parent_pipeline: PipelineId,
        frame_node_id: u64,
        url: String,
        srcdoc: Option<String>,
        sandbox: Option<String>,
    },
    RemoveChildFrame {
        parent_pipeline: PipelineId,
        frame_node_id: u64,
    },
    PostMessageToFrame {
        source_pipeline: PipelineId,
        target_pipeline: PipelineId,
        data_json: String,
        source_origin: String,
        target_origin: String,
        transfer_ports: Vec<MessagePortId>,
    },
    MessagePortSend {
        source_pipeline: PipelineId,
        port_id: MessagePortId,
        data_json: String,
        transfer_ports: Vec<MessagePortId>,
    },
    MessagePortClose {
        port_id: MessagePortId,
    },
    ScriptResult {
        pipeline: PipelineId,
        seq: u64,
        ok: bool,
        value: String,
    },
    DisplayListReady {
        pipeline: PipelineId,
        epoch: u64,
        bytes: Vec<u8>,
    },
    Pong {
        seq: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContentToNetwork {
    FetchStart {
        request_id: RequestId,
        url: String,
        method: HttpMethodKind,
        headers: Vec<(String, String)>,
        body: Option<ShmemRef>,
        mode: FetchMode,
        credentials: CredentialsMode,
        initiator_origin: String,
    },
    FetchCancel {
        request_id: RequestId,
    },
    WebSocketOpen {
        ws_id: WebSocketId,
        url: String,
        protocols: Vec<String>,
        origin: String,
    },
    WebSocketSend {
        ws_id: WebSocketId,
        opcode: u8,
        payload: Vec<u8>,
    },
    WebSocketClose {
        ws_id: WebSocketId,
        code: u16,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NetworkToContent {
    ResponseHead {
        request_id: RequestId,
        status: u16,
        final_url: String,
        headers: Vec<(String, String)>,
        redirected: bool,
    },
    ResponseChunk {
        request_id: RequestId,
        bytes: Vec<u8>,
    },
    ResponseEnd {
        request_id: RequestId,
        timing: NavigationTiming,
    },
    ResponseError {
        request_id: RequestId,
        error: String,
    },
    WebSocketOpened {
        ws_id: WebSocketId,
        protocol: String,
    },
    WebSocketMessage {
        ws_id: WebSocketId,
        opcode: u8,
        payload: Vec<u8>,
    },
    WebSocketClosed {
        ws_id: WebSocketId,
        code: u16,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContentToCompositor {
    SubmitFrame {
        pipeline: PipelineId,
        epoch: u64,
        display_list: ShmemRef,
        scroll_tree: ScrollTreeUpdate,
        stats: PaintStats,
    },
    RegisterImage {
        image_id: ImageId,
        width: u32,
        height: u32,
        format: PixelFormat,
        shmem: ShmemRef,
    },
    ReleaseImage {
        image_id: ImageId,
    },
    RemovePipeline {
        pipeline: PipelineId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum CompositorToContent {
    BeginFrame {
        pipeline: PipelineId,
        frame_seq: u64,
        vsync_ns: u64,
    },
    ScrollOffsetSync {
        pipeline: PipelineId,
        node_id: SpatialNodeId,
        offset: Point,
    },
    FrameAck {
        pipeline: PipelineId,
        epoch: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{decode_payload, encode_message, FLAG_NONE, MAX_CONTROL_FRAME_BYTES};
    use bytes::BytesMut;

    fn roundtrip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(val: &T) {
        let mut buf = BytesMut::new();
        encode_message(val, FLAG_NONE, MAX_CONTROL_FRAME_BYTES, &mut buf).expect("encode");
        let decoded: T = decode_payload(&buf[crate::frame::HEADER_SIZE..]).expect("decode");
        assert_eq!(&decoded, val);
    }

    #[test]
    fn all_protocol_enums_roundtrip() {
        roundtrip(&BrowserToContent::CommitNavigation {
            pipeline: PipelineId(1),
            nav_id: NavigationId(2),
            final_url: "https://example.org/".to_string(),
            mime_type: "text/html".to_string(),
            stream_id: StreamId(3),
        });
        roundtrip(&BrowserToContent::InputEvent {
            pipeline: PipelineId(1),
            event: RoutedInputEvent::KeyDown {
                key: "Enter".to_string(),
                code: "Enter".to_string(),
                repeat: false,
                modifiers: KeyModifiers {
                    ctrl: true,
                    ..KeyModifiers::default()
                },
            },
        });
        roundtrip(&ContentToBrowser::NavigationRequested {
            pipeline: PipelineId(1),
            url: "https://example.org/submit".to_string(),
            method: HttpMethodKind::Post,
            form_body: Some(b"a=1&b=2".to_vec()),
            content_type: Some("application/x-www-form-urlencoded".to_string()),
        });
        roundtrip(&ContentToBrowser::MissingFeatureReported {
            pipeline: PipelineId(1),
            kind: MissingFeatureKind::CssProperty,
            name: "backdrop-filter".to_string(),
        });
        roundtrip(&ContentToNetwork::FetchStart {
            request_id: RequestId(9),
            url: "https://api.example.org/data".to_string(),
            method: HttpMethodKind::Get,
            headers: vec![("accept".to_string(), "application/json".to_string())],
            body: Some(ShmemRef {
                id: ShmemId(4),
                offset: 0,
                len: 128,
            }),
            mode: FetchMode::Cors,
            credentials: CredentialsMode::SameOrigin,
            initiator_origin: "https://example.org".to_string(),
        });
        roundtrip(&NetworkToContent::ResponseEnd {
            request_id: RequestId(9),
            timing: NavigationTiming {
                time_origin_ms: 1700000000000.0,
                fetch_start_ms: 1.0,
                request_start_ms: 5.0,
                response_start_ms: 25.0,
                response_end_ms: 40.0,
                redirect_start_ms: Some(0.5),
                redirect_end_ms: Some(1.0),
                redirect_count: 1,
            },
        });
        roundtrip(&ContentToCompositor::SubmitFrame {
            pipeline: PipelineId(1),
            epoch: 42,
            display_list: ShmemRef {
                id: ShmemId(8),
                offset: 64,
                len: 4096,
            },
            scroll_tree: ScrollTreeUpdate {
                nodes: vec![ScrollNodeUpdate {
                    id: SpatialNodeId(0),
                    scroll_id: ScrollId(1),
                    content_rect: Rect::new(0.0, 0.0, 1280.0, 4000.0),
                    viewport_rect: Rect::new(0.0, 0.0, 1280.0, 720.0),
                    offset: Point::new(0.0, 120.0),
                }],
            },
            stats: PaintStats {
                display_items: 250,
                spatial_nodes: 3,
                build_ms: 1.25,
            },
        });
        roundtrip(&CompositorToContent::FrameAck {
            pipeline: PipelineId(1),
            epoch: 42,
        });
        roundtrip(&BrowserToContent::DeliverPostMessage {
            target_pipeline: PipelineId(2),
            source_pipeline: PipelineId(1),
            data_json: r#"{"hello":"oopif"}"#.to_string(),
            origin: "https://parent.example.org".to_string(),
            transfer_ports: vec![MessagePortId(7)],
        });
        roundtrip(&ContentToBrowser::CreateChildFrame {
            parent_pipeline: PipelineId(1),
            frame_node_id: 42,
            surface_id: SurfaceId(99),
            url: "https://child.example.com/embed".to_string(),
            srcdoc: None,
            sandbox: Some("allow-scripts".to_string()),
            width: 300,
            height: 150,
        });
        roundtrip(&ContentToBrowser::PostMessageToFrame {
            source_pipeline: PipelineId(1),
            target_pipeline: PipelineId(2),
            data_json: r#""ping""#.to_string(),
            source_origin: "https://parent.example.org".to_string(),
            target_origin: "https://child.example.com".to_string(),
            transfer_ports: vec![MessagePortId(11)],
        });
    }
}
