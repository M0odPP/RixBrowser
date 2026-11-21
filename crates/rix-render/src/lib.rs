#![deny(unsafe_code)]

pub mod apz;
pub mod compositor;
pub mod display_list;
pub mod renderer;
pub mod resources;
pub mod scene_encoder;
pub mod strip_cache;

pub use apz::{
    apply_scroll_wheel, clamp_scroll_offset, hit_test_scroll_chain, ScrollDelta, ScrollOutcome,
    ScrollSyncQueue,
};
pub use compositor::{Compositor, LayerRole, RetainedLayer};
pub use display_list::{
    AnimationId, BlendMode, ColorStop, DisplayItem, DisplayList, FilterOp, GlyphInstance,
    ImageSampling, Rgba8, SpatialNode, SpatialNodeKind, SpatialTree, StickyMargins,
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
pub use vello::Scene;
