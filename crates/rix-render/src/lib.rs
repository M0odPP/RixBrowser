#![deny(unsafe_code)]

pub mod compositor;
pub mod display_list;
pub mod renderer;
pub mod resources;
pub mod scene_encoder;

pub use compositor::{Compositor, LayerRole, RetainedLayer};
pub use display_list::{
    AnimationId, BlendMode, ColorStop, DisplayItem, DisplayList, FilterOp, GlyphInstance,
    ImageSampling, Rgba8, SpatialNode, SpatialNodeKind, SpatialTree, StickyMargins,
};
pub use renderer::{RenderError, VelloRenderer};
pub use resources::{ResourceRegistry, DEFAULT_IMAGE_CACHE_BUDGET_BYTES};
pub use scene_encoder::{encode_display_list, scale_corner_radii, EncodeOptions, EncodeStats};
pub use smallvec::smallvec;
pub use vello::peniko::Color;
pub use vello::Scene;
