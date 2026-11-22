#![deny(unsafe_code)]

pub mod animations;
pub mod apz;
pub mod canvas2d;
pub mod compositor;
pub mod display_list;
pub mod gpu_context;
pub mod renderer;
pub mod resources;
pub mod scene_encoder;
pub mod strip_cache;

pub use animations::{
    AnimDirection, AnimFillMode, AnimationSample, AnimationTarget, CompositorAnimation,
    OpacityKeyframe, TimingCurve, TransformKeyframe,
};
pub use apz::{
    apply_scroll_wheel, clamp_scroll_offset, hit_test_scroll_chain, ScrollDelta, ScrollOutcome,
    ScrollSyncQueue,
};
pub use canvas2d::{
    encode_rgba_to_png, encode_rgba_to_png_data_url, parse_css_color, Canvas2DContext,
    CanvasFillRule, CanvasGradient, CanvasGradientKind, CanvasPaint, CanvasPattern,
    CanvasPatternRepeat, CanvasState, CanvasTextAlign, CanvasTextBaseline, CanvasTextMetrics,
};
pub use compositor::{Compositor, LayerRole, RetainedLayer};
pub use display_list::{
    AnimationId, BlendMode, ClipPathShape, ColorStop, DisplayItem, DisplayList, FilterOp,
    GlyphInstance, ImageSampling, Rgba8, SpatialNode, SpatialNodeKind, SpatialTree, StickyMargins,
};
pub use gpu_context::{
    create_headless_gpu_device, create_wgpu_instance, readback_wgpu_buffer_range,
    readback_wgpu_texture_rgba, request_device_from_adapter, request_headless_adapter,
    wait_for_gpu_idle, write_wgpu_buffer_mapped, SharedGpuDevice,
};
pub use renderer::{RenderError, VelloRenderer};
pub use resources::{ResourceRegistry, DEFAULT_IMAGE_CACHE_BUDGET_BYTES};
pub use scene_encoder::{
    encode_display_list, encode_display_list_ranges, scale_corner_radii, EncodeOptions,
    EncodeStats, DEFAULT_VIEWPORT_OVERSCAN_PX,
};
pub use smallvec::smallvec;
pub use strip_cache::{
    StripCacheStats, StripIndex, StripSceneCache, DEFAULT_MAX_CACHED_STRIPS,
    DEFAULT_STRIP_HEIGHT_PX,
};
pub use vello::peniko::Color;
pub use vello::peniko::ImageData as PenikoImageData;
pub use vello::wgpu;
pub use vello::Scene;
