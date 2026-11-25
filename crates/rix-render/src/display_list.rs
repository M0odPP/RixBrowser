use rix_core::{Point, Rect, Vec2};
use rix_ipc::{
    FontResourceId, ImageId, PathResourceId, ScrollId, SealedShmemBlob, SpatialNodeId, SurfaceId,
};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::ops::Range;
use thiserror::Error;
use vello::kurbo::Affine;
use vello::peniko::{self, Color, Compose, Mix};

#[derive(Error, Debug)]
pub enum DisplayListError {
    #[error("DisplayList binary codec error: {0}")]
    Codec(#[from] postcard::Error),
    #[error("DisplayList shared memory error: {0}")]
    Shmem(#[from] rix_ipc::IpcError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AnimationId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Rgba8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba8 {
    pub const TRANSPARENT: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };
    pub const BLACK: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };
    pub const WHITE: Self = Self {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };

    #[inline]
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    #[inline]
    pub fn from_f32(r: f32, g: f32, b: f32, a: f32) -> Self {
        let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        Self {
            r: to_u8(r),
            g: to_u8(g),
            b: to_u8(b),
            a: to_u8(a),
        }
    }

    #[inline]
    pub fn is_transparent(self) -> bool {
        self.a == 0
    }

    #[inline]
    pub fn to_vello_color(self) -> Color {
        Color::from_rgba8(self.r, self.g, self.b, self.a)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ColorStop {
    pub offset: f32,
    pub color: Rgba8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum BlendMode {
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Hue,
    Saturation,
    Color,
    Luminosity,
}

impl BlendMode {
    pub fn to_vello(self) -> peniko::BlendMode {
        let mix = match self {
            Self::Normal => Mix::Normal,
            Self::Multiply => Mix::Multiply,
            Self::Screen => Mix::Screen,
            Self::Overlay => Mix::Overlay,
            Self::Darken => Mix::Darken,
            Self::Lighten => Mix::Lighten,
            Self::ColorDodge => Mix::ColorDodge,
            Self::ColorBurn => Mix::ColorBurn,
            Self::HardLight => Mix::HardLight,
            Self::SoftLight => Mix::SoftLight,
            Self::Difference => Mix::Difference,
            Self::Exclusion => Mix::Exclusion,
            Self::Hue => Mix::Hue,
            Self::Saturation => Mix::Saturation,
            Self::Color => Mix::Color,
            Self::Luminosity => Mix::Luminosity,
        };
        peniko::BlendMode::new(mix, Compose::SrcOver)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum FilterOp {
    Blur(f32),
    Opacity(f32),
    Brightness(f32),
    Contrast(f32),
    Grayscale(f32),
    Saturate(f32),
    Invert(f32),
    Sepia(f32),
    HueRotateDeg(f32),
    DropShadow {
        offset: Vec2,
        blur_radius: f32,
        color: Rgba8,
    },
    /// Compositor-driven opacity animation track evaluated at VSync without rebuilding DisplayLists.
    AnimatedOpacity(AnimationId),
}

/// Geometry shape used by [`DisplayItem::PushClipPath`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClipPathShape {
    Inset { rect: Rect, radii: [f32; 4] },
    Circle { center: Point, radius: f32 },
    Ellipse { center: Point, rx: f32, ry: f32 },
    Polygon { fill_odd: bool, points: Vec<Point> },
}

impl ClipPathShape {
    #[must_use]
    pub fn bounding_rect(&self) -> Rect {
        match self {
            Self::Inset { rect, .. } => *rect,
            Self::Circle { center, radius } => {
                let r = radius.max(0.0);
                Rect::new(center.x - r, center.y - r, r * 2.0, r * 2.0)
            }
            Self::Ellipse { center, rx, ry } => {
                let rx = rx.max(0.0);
                let ry = ry.max(0.0);
                Rect::new(center.x - rx, center.y - ry, rx * 2.0, ry * 2.0)
            }
            Self::Polygon { points, .. } => {
                let Some(first) = points.first() else {
                    return Rect::ZERO;
                };
                let (mut min_x, mut min_y, mut max_x, mut max_y) =
                    (first.x, first.y, first.x, first.y);
                for p in points.iter().skip(1) {
                    min_x = min_x.min(p.x);
                    min_y = min_y.min(p.y);
                    max_x = max_x.max(p.x);
                    max_y = max_y.max(p.y);
                }
                Rect::new(
                    min_x,
                    min_y,
                    (max_x - min_x).max(0.0),
                    (max_y - min_y).max(0.0),
                )
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ImageSampling {
    #[default]
    Bilinear,
    Nearest,
    High,
}

impl ImageSampling {
    pub fn to_vello_quality(self) -> peniko::ImageQuality {
        match self {
            Self::Nearest => peniko::ImageQuality::Low,
            Self::Bilinear => peniko::ImageQuality::Medium,
            Self::High => peniko::ImageQuality::High,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct StickyMargins {
    pub top: Option<f32>,
    pub right: Option<f32>,
    pub bottom: Option<f32>,
    pub left: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpatialNode {
    pub id: SpatialNodeId,
    pub parent: Option<SpatialNodeId>,
    pub kind: SpatialNodeKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SpatialNodeKind {
    Root,
    ScrollFrame {
        content_rect: Rect,
        viewport_rect: Rect,
        external_scroll_id: ScrollId,
        current_offset: Vec2,
    },
    /// `bounds` defines the sticky box's natural top-left origin and maximum travel span `(width, height)`
    /// inside `scroll_parent` content coordinates.
    StickyFrame {
        bounds: Rect,
        margins: StickyMargins,
        scroll_parent: SpatialNodeId,
    },
    Transform {
        matrix: [f32; 6],
        origin: Point,
        animation_id: Option<AnimationId>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpatialTree {
    pub nodes: Vec<SpatialNode>,
}

impl Default for SpatialTree {
    fn default() -> Self {
        Self::new()
    }
}

impl SpatialTree {
    pub fn new() -> Self {
        Self {
            nodes: vec![SpatialNode {
                id: SpatialNodeId(0),
                parent: None,
                kind: SpatialNodeKind::Root,
            }],
        }
    }

    pub fn add_node(
        &mut self,
        parent: Option<SpatialNodeId>,
        kind: SpatialNodeKind,
    ) -> SpatialNodeId {
        let id = SpatialNodeId(self.nodes.len() as u32);
        self.nodes.push(SpatialNode { id, parent, kind });
        id
    }

    pub fn get(&self, id: SpatialNodeId) -> Option<&SpatialNode> {
        let direct = self.nodes.get(id.0 as usize).filter(|n| n.id == id);
        direct.or_else(|| self.nodes.iter().find(|n| n.id == id))
    }

    pub fn get_mut(&mut self, id: SpatialNodeId) -> Option<&mut SpatialNode> {
        if self.nodes.get(id.0 as usize).is_some_and(|n| n.id == id) {
            return self.nodes.get_mut(id.0 as usize);
        }
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    pub fn local_transform(&self, node: &SpatialNode) -> Affine {
        match &node.kind {
            SpatialNodeKind::Root => Affine::IDENTITY,
            SpatialNodeKind::ScrollFrame { current_offset, .. } => {
                Affine::translate((-f64::from(current_offset.x), -f64::from(current_offset.y)))
            }
            SpatialNodeKind::StickyFrame {
                bounds,
                margins,
                scroll_parent,
            } => {
                let Some(parent_node) = self.get(*scroll_parent) else {
                    return Affine::IDENTITY;
                };
                let SpatialNodeKind::ScrollFrame {
                    viewport_rect,
                    current_offset,
                    ..
                } = &parent_node.kind
                else {
                    return Affine::IDENTITY;
                };
                let mut dx = 0.0f32;
                let mut dy = 0.0f32;
                if let Some(top) = margins.top {
                    let target_y = current_offset.y + viewport_rect.origin.y + top;
                    let delta = (target_y - bounds.origin.y).max(0.0);
                    dy = delta.min(bounds.size.height.max(0.0));
                } else if let Some(bottom) = margins.bottom {
                    let target_y = current_offset.y + viewport_rect.max_y() - bottom;
                    let delta = (bounds.origin.y - target_y).max(0.0);
                    dy = -delta.min(bounds.size.height.max(0.0));
                }
                if let Some(left) = margins.left {
                    let target_x = current_offset.x + viewport_rect.origin.x + left;
                    let delta = (target_x - bounds.origin.x).max(0.0);
                    dx = delta.min(bounds.size.width.max(0.0));
                } else if let Some(right) = margins.right {
                    let target_x = current_offset.x + viewport_rect.max_x() - right;
                    let delta = (bounds.origin.x - target_x).max(0.0);
                    dx = -delta.min(bounds.size.width.max(0.0));
                }
                Affine::translate((f64::from(dx), f64::from(dy)))
            }
            SpatialNodeKind::Transform { matrix, origin, .. } => {
                let m = matrix.map(f64::from);
                let ox = f64::from(origin.x);
                let oy = f64::from(origin.y);
                Affine::translate((ox, oy)) * Affine::new(m) * Affine::translate((-ox, -oy))
            }
        }
    }

    pub fn resolve_world_transform(&self, id: SpatialNodeId) -> Affine {
        self.resolve_world_transform_with_override(id, None)
    }

    pub fn resolve_world_transform_with_override(
        &self,
        id: SpatialNodeId,
        zero_scroll_node: Option<SpatialNodeId>,
    ) -> Affine {
        let mut chain: SmallVec<[Affine; 8]> = SmallVec::new();
        let mut current = Some(id);
        let mut steps = 0usize;
        let limit = self.nodes.len();
        while let Some(node_id) = current {
            if steps >= limit {
                break;
            }
            steps += 1;
            let Some(node) = self.get(node_id) else {
                break;
            };
            let local = if zero_scroll_node == Some(node_id) {
                Affine::IDENTITY
            } else {
                self.local_transform(node)
            };
            chain.push(local);
            current = node.parent;
        }
        let mut world = Affine::IDENTITY;
        for transform in chain.into_iter().rev() {
            world *= transform;
        }
        world
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GlyphInstance {
    pub glyph_id: u32,
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DisplayItem {
    PushStackingContext {
        spatial: SpatialNodeId,
        bounds: Rect,
        opacity: f32,
        mix_blend: BlendMode,
        filters: SmallVec<[FilterOp; 2]>,
    },
    PopStackingContext,
    PushClip {
        spatial: SpatialNodeId,
        rect: Rect,
        radii: [f32; 4],
    },
    PushClipPath {
        spatial: SpatialNodeId,
        shape: ClipPathShape,
    },
    PopClip,
    Rect {
        bounds: Rect,
        color: Rgba8,
        radii: [f32; 4],
    },
    LinearGradient {
        bounds: Rect,
        start: Point,
        end: Point,
        stops: SmallVec<[ColorStop; 4]>,
        radii: [f32; 4],
    },
    RadialGradient {
        bounds: Rect,
        center: Point,
        radius: f32,
        stops: SmallVec<[ColorStop; 4]>,
        radii: [f32; 4],
    },
    Border {
        bounds: Rect,
        widths: [f32; 4],
        colors: [Rgba8; 4],
        radii: [f32; 4],
    },
    BoxShadow {
        bounds: Rect,
        offset: Vec2,
        blur_radius: f32,
        spread_radius: f32,
        color: Rgba8,
        radii: [f32; 4],
        inset: bool,
    },
    GlyphRun {
        bounds: Rect,
        font_id: FontResourceId,
        font_size: f32,
        color: Rgba8,
        glyphs: Range<u32>,
    },
    Image {
        bounds: Rect,
        image_id: ImageId,
        sampling: ImageSampling,
        radii: [f32; 4],
    },
    VectorPath {
        bounds: Rect,
        path_id: PathResourceId,
        fill: Option<Rgba8>,
        stroke: Option<(Rgba8, f32)>,
    },
    ExternalSurface {
        bounds: Rect,
        surface_id: SurfaceId,
        radii: [f32; 4],
    },
    /// GPU `backdrop-filter` pass applied to previously painted pixels behind `bounds`
    /// before this element's own translucent background, border, and children are drawn.
    BackdropFilter {
        bounds: Rect,
        radii: [f32; 4],
        filters: SmallVec<[FilterOp; 2]>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayList {
    pub spatial_tree: SpatialTree,
    pub items: Vec<DisplayItem>,
    pub glyphs: Vec<GlyphInstance>,
    pub content_bounds: Rect,
    #[serde(default)]
    pub animations: Vec<crate::animations::CompositorAnimation>,
}

impl Default for DisplayList {
    fn default() -> Self {
        Self::new(Rect::ZERO)
    }
}

impl DisplayList {
    pub fn new(content_bounds: Rect) -> Self {
        Self {
            spatial_tree: SpatialTree::new(),
            items: Vec::new(),
            glyphs: Vec::new(),
            content_bounds,
            animations: Vec::new(),
        }
    }

    pub fn push(&mut self, item: DisplayItem) {
        self.items.push(item);
    }

    pub fn push_glyph_run(
        &mut self,
        bounds: Rect,
        font_id: FontResourceId,
        font_size: f32,
        color: Rgba8,
        instances: &[GlyphInstance],
    ) {
        let start = self.glyphs.len() as u32;
        self.glyphs.extend_from_slice(instances);
        let end = self.glyphs.len() as u32;
        self.items.push(DisplayItem::GlyphRun {
            bounds,
            font_id,
            font_size,
            color,
            glyphs: start..end,
        });
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, DisplayListError> {
        Ok(postcard::to_stdvec(self)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DisplayListError> {
        Ok(postcard::from_bytes(bytes)?)
    }

    pub fn to_sealed_shmem(&self) -> Result<SealedShmemBlob, DisplayListError> {
        let bytes = self.to_bytes()?;
        Ok(SealedShmemBlob::from_bytes(&bytes)?)
    }

    pub fn from_sealed_shmem(blob: &SealedShmemBlob) -> Result<Self, DisplayListError> {
        Self::from_bytes(blob.as_slice())
    }
}
