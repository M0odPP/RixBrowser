use crate::animations::{AnimationSample, CompositorAnimation};
use crate::apz::{apply_scroll_wheel, ScrollDelta, ScrollSyncQueue};
use crate::display_list::{
    AnimationId, BlendMode, DisplayItem, DisplayList, FilterOp, SpatialNodeKind,
};
use crate::gpu_filters::BackdropFilterPass;
use crate::resources::ResourceRegistry;
use crate::scene_encoder::{
    encode_display_list, encode_display_list_ranges, scale_corner_radii, transform_aabb,
    EncodeOptions, EncodeStats, DEFAULT_VIEWPORT_OVERSCAN_PX,
};
use crate::strip_cache::{StripCacheStats, StripSceneCache, DEFAULT_STRIP_HEIGHT_PX};
use rix_core::{Point, Rect, Size};
use rix_ipc::{CompositorToContent, PipelineId, SpatialNodeId, SurfaceId};
use smallvec::SmallVec;
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::Arc;
use vello::kurbo::{Affine, Circle, Line, Rect as KRect, RoundedRect, RoundedRectRadii, Stroke};
use vello::peniko::{Blob, Color, Fill, ImageAlphaType, ImageBrush, ImageData, ImageFormat};
use vello::Scene;

/// Tracks exact dirty viewport rectangles across frames for partial-damage compositing
/// and guarantees `None` (0 Hz idle) when nothing changed.
#[derive(Debug, Clone, Copy, Default)]
pub struct DamageTracker {
    dirty_rect: Option<Rect>,
    full_viewport: bool,
    last_frame: Option<Rect>,
}

impl DamageTracker {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            dirty_rect: None,
            full_viewport: false,
            last_frame: None,
        }
    }

    pub fn mark_full(&mut self) {
        self.full_viewport = true;
    }

    pub fn mark_rect(&mut self, rect: Rect) {
        if rect.is_empty() || self.full_viewport {
            return;
        }
        self.dirty_rect = Some(match self.dirty_rect {
            Some(prev) => rect_union(prev, rect),
            None => rect,
        });
    }

    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.full_viewport || self.dirty_rect.is_some()
    }

    /// Consumes pending damage, clips it to `viewport`, records it as `last_frame`, and returns it.
    pub fn take_frame_damage(&mut self, viewport: Rect) -> Option<Rect> {
        let out = if self.full_viewport {
            (!viewport.is_empty()).then_some(viewport)
        } else {
            self.dirty_rect
                .and_then(|r| {
                    if viewport.is_empty() {
                        Some(r)
                    } else {
                        r.intersect(&viewport)
                    }
                })
                .filter(|r| !r.is_empty())
        };
        self.dirty_rect = None;
        self.full_viewport = false;
        self.last_frame = out;
        out
    }

    pub fn clear_idle(&mut self) {
        self.last_frame = None;
    }

    #[must_use]
    pub const fn last_frame_damage(&self) -> Option<Rect> {
        self.last_frame
    }
}

/// Statistics for a layer's retained [`RetainedLayerTree`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayerTreeStats {
    pub static_segments: usize,
    pub promoted_layers: usize,
    pub fast_recomposes: u64,
}

/// Single GPU `backdrop-filter` pass followed by the overlay scene painted on top of it.
pub struct BackdropCompositeStep {
    pub pass: BackdropFilterPass,
    pub overlay_scene: Scene,
}

/// Multi-stage GPU composition plan emitted when any visible layer contains `DisplayItem::BackdropFilter`.
pub struct BackdropCompositePlan {
    pub base_scene: Scene,
    pub steps: Vec<BackdropCompositeStep>,
}

struct PromotedSubLayer {
    scene: Scene,
    spatial: SpatialNodeId,
    local_bounds: Rect,
    base_opacity: f32,
    opacity_anim: Option<AnimationId>,
    mix_blend: BlendMode,
    last_world_bounds: Rect,
}

enum LayerTreeSegment {
    Static(Scene),
    Promoted(PromotedSubLayer),
}

struct RetainedLayerTree {
    segments: Vec<LayerTreeSegment>,
    static_segments: usize,
    promoted_layers: usize,
    fast_recomposes: u64,
}

impl RetainedLayerTree {
    fn try_build(
        pipeline: PipelineId,
        list: &DisplayList,
        resources: &mut ResourceRegistry,
        base_opts: &EncodeOptions,
    ) -> Option<Self> {
        if list.animations.is_empty() || list.items.is_empty() {
            return None;
        }
        let mut segments: Vec<LayerTreeSegment> = Vec::new();
        let mut static_segments = 0usize;
        let mut promoted_layers = 0usize;
        let mut static_start = 0usize;
        let mut idx = 0usize;
        let mut depth = 0usize;

        while idx < list.items.len() {
            match &list.items[idx] {
                DisplayItem::PushStackingContext {
                    spatial,
                    bounds,
                    opacity,
                    mix_blend,
                    filters,
                } if depth == 0 => {
                    let end_idx = find_matching_stacking_pop(&list.items, idx);
                    let spatial_is_anim = matches!(
                        list.spatial_tree.get(*spatial).map(|n| &n.kind),
                        Some(SpatialNodeKind::Transform {
                            animation_id: Some(_),
                            ..
                        })
                    );
                    let opacity_anim = filters.iter().find_map(|f| match *f {
                        FilterOp::AnimatedOpacity(id) => Some(id),
                        _ => None,
                    });
                    let has_complex_filters = filters
                        .iter()
                        .any(|f| !matches!(f, FilterOp::AnimatedOpacity(_) | FilterOp::Opacity(_)));
                    let has_backdrop = list.items[idx..=end_idx.min(list.items.len() - 1)]
                        .iter()
                        .any(|it| matches!(it, DisplayItem::BackdropFilter { .. }));

                    if (spatial_is_anim || opacity_anim.is_some())
                        && !has_complex_filters
                        && !has_backdrop
                        && end_idx < list.items.len()
                    {
                        if static_start < idx {
                            let mut st_scene = Scene::new();
                            encode_display_list_ranges(
                                pipeline,
                                list,
                                std::slice::from_ref(&(static_start..idx)),
                                resources,
                                base_opts,
                                &mut st_scene,
                            );
                            segments.push(LayerTreeSegment::Static(st_scene));
                            static_segments += 1;
                        }
                        let mut sub_scene = Scene::new();
                        if idx + 1 < end_idx {
                            let local_opts = EncodeOptions {
                                cull_rect: None,
                                viewport_clip: None,
                                base_transform: Affine::IDENTITY,
                                override_scroll_zero: Some(*spatial),
                                opacity_overrides: HashMap::new(),
                            };
                            encode_display_list_ranges(
                                pipeline,
                                list,
                                std::slice::from_ref(&((idx + 1)..end_idx)),
                                resources,
                                &local_opts,
                                &mut sub_scene,
                            );
                        }
                        let mut base_opacity = opacity.clamp(0.0, 1.0);
                        for op in filters {
                            if let FilterOp::Opacity(a) = *op {
                                base_opacity *= a.clamp(0.0, 1.0);
                            }
                        }
                        let world_tf = list.spatial_tree.resolve_world_transform(*spatial);
                        let last_world_bounds = transform_aabb(*bounds, world_tf);
                        segments.push(LayerTreeSegment::Promoted(PromotedSubLayer {
                            scene: sub_scene,
                            spatial: *spatial,
                            local_bounds: *bounds,
                            base_opacity,
                            opacity_anim,
                            mix_blend: *mix_blend,
                            last_world_bounds,
                        }));
                        promoted_layers += 1;
                        idx = end_idx + 1;
                        static_start = idx;
                        continue;
                    }
                    depth += 1;
                }
                DisplayItem::PushStackingContext { .. } => depth += 1,
                DisplayItem::PopStackingContext => depth = depth.saturating_sub(1),
                _ => {}
            }
            idx += 1;
        }

        if promoted_layers == 0 {
            return None;
        }
        if static_start < list.items.len() {
            let mut st_scene = Scene::new();
            encode_display_list_ranges(
                pipeline,
                list,
                std::slice::from_ref(&(static_start..list.items.len())),
                resources,
                base_opts,
                &mut st_scene,
            );
            segments.push(LayerTreeSegment::Static(st_scene));
            static_segments += 1;
        }
        Some(Self {
            segments,
            static_segments,
            promoted_layers,
            fast_recomposes: 0,
        })
    }

    fn compose_into(
        &mut self,
        list: &DisplayList,
        animated_opacities: &HashMap<AnimationId, f32>,
        viewport_clip: Option<Rect>,
        target: &mut Scene,
    ) {
        for seg in &self.segments {
            match seg {
                LayerTreeSegment::Static(scene) => target.append(scene, None),
                LayerTreeSegment::Promoted(sub) => {
                    let alpha = sub
                        .opacity_anim
                        .and_then(|id| animated_opacities.get(&id).copied())
                        .unwrap_or(sub.base_opacity)
                        .clamp(0.0, 1.0);
                    if alpha <= 0.0 || sub.local_bounds.is_empty() {
                        continue;
                    }
                    let world_tf = list.spatial_tree.resolve_world_transform(sub.spatial);
                    let world_bounds = transform_aabb(sub.local_bounds, world_tf);
                    let clipped = match viewport_clip {
                        Some(vp) => world_bounds.intersect(&vp).unwrap_or(Rect::ZERO),
                        None => world_bounds,
                    };
                    if clipped.is_empty() {
                        continue;
                    }
                    let clip_rect = KRect::new(
                        f64::from(clipped.min_x()),
                        f64::from(clipped.min_y()),
                        f64::from(clipped.max_x()),
                        f64::from(clipped.max_y()),
                    );
                    target.push_layer(
                        Fill::NonZero,
                        sub.mix_blend.to_vello(),
                        alpha,
                        Affine::IDENTITY,
                        &clip_rect,
                    );
                    target.append(&sub.scene, Some(world_tf));
                    target.pop_layer();
                }
            }
        }
        self.fast_recomposes += 1;
    }

    fn stats(&self) -> LayerTreeStats {
        LayerTreeStats {
            static_segments: self.static_segments,
            promoted_layers: self.promoted_layers,
            fast_recomposes: self.fast_recomposes,
        }
    }
}

fn find_matching_stacking_pop(items: &[DisplayItem], start: usize) -> usize {
    let mut depth = 1usize;
    let mut idx = start + 1;
    while idx < items.len() {
        match &items[idx] {
            DisplayItem::PushStackingContext { .. } => depth += 1,
            DisplayItem::PopStackingContext => {
                depth -= 1;
                if depth == 0 {
                    return idx;
                }
            }
            _ => {}
        }
        idx += 1;
    }
    items.len()
}

fn rect_union(a: Rect, b: Rect) -> Rect {
    if a.is_empty() {
        return b;
    }
    if b.is_empty() {
        return a;
    }
    let min_x = a.min_x().min(b.min_x());
    let min_y = a.min_y().min(b.min_y());
    let max_x = a.max_x().max(b.max_x());
    let max_y = a.max_y().max(b.max_y());
    Rect::new(
        min_x,
        min_y,
        (max_x - min_x).max(0.0),
        (max_y - min_y).max(0.0),
    )
}

const MAX_EXTERNAL_SURFACE_DEPTH: usize = 16;

#[derive(Debug, Clone, Copy)]
struct ExternalSurfacePlacement {
    surface_id: SurfaceId,
    spatial_id: SpatialNodeId,
    bounds: Rect,
    radii: [f32; 4],
}

struct LiveExternalSurface {
    width: u32,
    height: u32,
    scene: Scene,
}

enum ExternalSurfaceBinding {
    /// GPU-backed vector/texture scene (e.g. Canvas2D).
    Scene(Box<LiveExternalSurface>),
    /// GPU-resident `wgpu::Texture` (registered via `VelloRenderer::register_external_gpu_texture`)
    /// or RGBA frame backing WebGPU, WebGL2, and GPU Video external surfaces.
    Image {
        image: ImageData,
        width: u32,
        height: u32,
    },
    /// Out-of-process or nested child `<iframe>` pipeline composited directly into the slot.
    ChildPipeline {
        pipeline: PipelineId,
        width: u32,
        height: u32,
    },
    /// Inline crashed-frame visual rendered when an OOPIF child process terminates unexpectedly.
    Crashed {
        #[allow(dead_code)]
        label: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LayerRole {
    /// Embedded child iframe pipeline composited inside a parent `ExternalSurface` slot.
    Subframe,
    WebSurface,
    Chrome,
    Overlay,
}

impl LayerRole {
    pub const fn default_z_order(self) -> i32 {
        match self {
            Self::Subframe => 5,
            Self::WebSurface => 10,
            Self::Chrome => 100,
            Self::Overlay => 200,
        }
    }
}

pub struct RetainedLayer {
    pub pipeline: PipelineId,
    pub role: LayerRole,
    pub z_order: i32,
    pub epoch: u64,
    pub display_list: DisplayList,
    cached_scene: Option<Scene>,
    strip_cache: Option<StripSceneCache>,
    layer_tree: Option<RetainedLayerTree>,
    external_placements: Vec<ExternalSurfacePlacement>,
    animated_opacities: HashMap<AnimationId, f32>,
    has_backdrop_filters: bool,
    encode_generation: u64,
    last_encode_stats: EncodeStats,
    dirty: bool,
    strip_recompose_needed: bool,
    tree_recompose_needed: bool,
}

impl RetainedLayer {
    pub fn encode_generation(&self) -> u64 {
        self.encode_generation
    }

    pub fn last_encode_stats(&self) -> EncodeStats {
        self.last_encode_stats
    }

    pub fn strip_cache_stats(&self) -> Option<StripCacheStats> {
        self.strip_cache.as_ref().map(StripSceneCache::stats)
    }

    pub fn layer_tree_stats(&self) -> Option<LayerTreeStats> {
        self.layer_tree.as_ref().map(RetainedLayerTree::stats)
    }
}

pub struct Compositor {
    layers: BTreeMap<(i32, PipelineId), RetainedLayer>,
    pipeline_z: BTreeMap<PipelineId, i32>,
    pub resources: ResourceRegistry,
    external_bindings: HashMap<SurfaceId, ExternalSurfaceBinding>,
    viewport_size: Size,
    device_pixel_ratio: f32,
    strip_height_px: f32,
    scroll_sync_queue: ScrollSyncQueue,
    damage: DamageTracker,
    dirty: bool,
    frames_rendered: u64,
    composed_scene: Scene,
    backdrop_plan: Option<BackdropCompositePlan>,
}

impl Default for Compositor {
    fn default() -> Self {
        Self::new(Size::new(1280.0, 720.0), 1.0)
    }
}

impl Compositor {
    pub fn new(viewport_size: Size, device_pixel_ratio: f32) -> Self {
        Self {
            layers: BTreeMap::new(),
            pipeline_z: BTreeMap::new(),
            resources: ResourceRegistry::default(),
            external_bindings: HashMap::new(),
            viewport_size,
            device_pixel_ratio: device_pixel_ratio.max(0.5),
            strip_height_px: DEFAULT_STRIP_HEIGHT_PX,
            scroll_sync_queue: ScrollSyncQueue::new(),
            damage: DamageTracker::new(),
            dirty: false,
            frames_rendered: 0,
            composed_scene: Scene::new(),
            backdrop_plan: None,
        }
    }

    /// Presents a new frame for a GPU-backed `ExternalSurface` (e.g. Canvas2D) WITHOUT
    /// rebuilding any `DisplayList` or re-encoding any `RetainedLayer`.
    pub fn present_external_surface_scene(
        &mut self,
        surface_id: SurfaceId,
        width: u32,
        height: u32,
        scene: Scene,
    ) {
        self.external_bindings.insert(
            surface_id,
            ExternalSurfaceBinding::Scene(Box::new(LiveExternalSurface {
                width: width.max(1),
                height: height.max(1),
                scene,
            })),
        );
        self.dirty = true;
    }

    /// Presents a GPU-registered `ImageData` (zero-copy `wgpu::Texture` override from
    /// [`crate::VelloRenderer::register_external_gpu_texture`]) or RGBA image into an
    /// `ExternalSurface` slot WITHOUT rebuilding any `DisplayList` or re-encoding any layer.
    pub fn present_external_surface_image(&mut self, surface_id: SurfaceId, image: ImageData) {
        let width = image.width.max(1);
        let height = image.height.max(1);
        self.external_bindings.insert(
            surface_id,
            ExternalSurfaceBinding::Image {
                image,
                width,
                height,
            },
        );
        self.dirty = true;
    }

    /// Presents tightly packed RGBA8 pixels into an `ExternalSurface` slot WITHOUT
    /// rebuilding any `DisplayList` or re-encoding any `RetainedLayer`.
    pub fn present_external_surface_rgba(
        &mut self,
        surface_id: SurfaceId,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    ) {
        let w = width.max(1);
        let h = height.max(1);
        let image = ImageData {
            data: Blob::new(Arc::new(rgba)),
            format: ImageFormat::Rgba8,
            alpha_type: ImageAlphaType::Alpha,
            width: w,
            height: h,
        };
        self.present_external_surface_image(surface_id, image);
    }

    /// Binds an out-of-process or nested child `<iframe>` pipeline to an `ExternalSurface`
    /// slot in a parent document without requiring the parent DisplayList to rebuild when
    /// the child repaints.
    pub fn bind_external_surface_pipeline(
        &mut self,
        surface_id: SurfaceId,
        child_pipeline: PipelineId,
        width: u32,
        height: u32,
    ) {
        self.external_bindings.insert(
            surface_id,
            ExternalSurfaceBinding::ChildPipeline {
                pipeline: child_pipeline,
                width: width.max(1),
                height: height.max(1),
            },
        );
        self.dirty = true;
    }

    /// Unbinds a previously attached child iframe pipeline or external surface.
    pub fn unbind_external_surface(&mut self, surface_id: SurfaceId) -> bool {
        let removed = self.external_bindings.remove(&surface_id).is_some();
        if removed {
            self.dirty = true;
        }
        removed
    }

    /// Marks an OOPIF `ExternalSurface` as crashed so the compositor renders an inline
    /// crashed-frame indicator inside the iframe bounds while the parent document stays live.
    pub fn mark_external_surface_crashed(
        &mut self,
        surface_id: SurfaceId,
        label: impl Into<String>,
    ) {
        self.external_bindings.insert(
            surface_id,
            ExternalSurfaceBinding::Crashed {
                label: label.into(),
            },
        );
        self.dirty = true;
    }

    /// Marks every `ExternalSurface` currently bound to `crashed_pipeline` as crashed,
    /// releases the dead pipeline's retained layer & resources, and returns the affected
    /// surface identifiers.
    pub fn mark_pipeline_crashed(
        &mut self,
        crashed_pipeline: PipelineId,
        label: impl Into<String>,
    ) -> Vec<SurfaceId> {
        let label = label.into();
        let mut affected = Vec::new();
        for (&sid, binding) in &mut self.external_bindings {
            if matches!(
                binding,
                ExternalSurfaceBinding::ChildPipeline { pipeline, .. }
                    if *pipeline == crashed_pipeline
            ) {
                *binding = ExternalSurfaceBinding::Crashed {
                    label: label.clone(),
                };
                affected.push(sid);
            }
        }
        self.remove_pipeline(crashed_pipeline);
        if !affected.is_empty() {
            self.dirty = true;
        }
        affected
    }

    /// Returns `true` if `surface_id` is currently displaying an inline crashed-frame UI.
    #[must_use]
    pub fn external_surface_is_crashed(&self, surface_id: SurfaceId) -> bool {
        matches!(
            self.external_bindings.get(&surface_id),
            Some(ExternalSurfaceBinding::Crashed { .. })
        )
    }

    /// Removes a live external surface scene when its owning canvas is destroyed.
    pub fn remove_external_surface_scene(&mut self, surface_id: SurfaceId) -> bool {
        self.unbind_external_surface(surface_id)
    }

    pub fn viewport_size(&self) -> Size {
        self.viewport_size
    }

    pub fn device_pixel_ratio(&self) -> f32 {
        self.device_pixel_ratio
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn frames_rendered(&self) -> u64 {
        self.frames_rendered
    }

    pub fn last_composed_scene(&self) -> &Scene {
        &self.composed_scene
    }

    /// Returns the multi-stage GPU `backdrop-filter` composition plan for the most recent
    /// composed frame, or `None` when no visible layer uses `backdrop-filter` (fast path).
    #[must_use]
    pub fn backdrop_plan(&self) -> Option<&BackdropCompositePlan> {
        self.backdrop_plan.as_ref()
    }

    /// Returns the exact viewport damage rectangle rendered on the most recent frame,
    /// or `None` when the compositor was idle (0 Hz).
    #[must_use]
    pub const fn last_frame_damage(&self) -> Option<Rect> {
        self.damage.last_frame_damage()
    }

    pub fn strip_cache_stats(&self, pipeline: PipelineId) -> Option<StripCacheStats> {
        self.layer(pipeline)
            .and_then(RetainedLayer::strip_cache_stats)
    }

    pub fn layer_tree_stats(&self, pipeline: PipelineId) -> Option<LayerTreeStats> {
        self.layer(pipeline)
            .and_then(RetainedLayer::layer_tree_stats)
    }

    pub fn drain_scroll_syncs(&mut self) -> Vec<CompositorToContent> {
        self.scroll_sync_queue.drain_vsync_syncs()
    }

    pub fn set_viewport(&mut self, size: Size, dpr: f32) -> bool {
        let dpr = dpr.max(0.5);
        if (self.viewport_size.width - size.width).abs() < 1e-3
            && (self.viewport_size.height - size.height).abs() < 1e-3
            && (self.device_pixel_ratio - dpr).abs() < 1e-3
        {
            return false;
        }
        self.viewport_size = size;
        self.device_pixel_ratio = dpr;
        for layer in self.layers.values_mut() {
            layer.dirty = true;
        }
        self.damage.mark_full();
        self.dirty = true;
        true
    }

    pub fn submit_display_list(
        &mut self,
        pipeline: PipelineId,
        role: LayerRole,
        epoch: u64,
        display_list: DisplayList,
    ) {
        self.submit_display_list_with_z(
            pipeline,
            role,
            role.default_z_order(),
            epoch,
            display_list,
        );
    }

    pub fn submit_display_list_with_z(
        &mut self,
        pipeline: PipelineId,
        role: LayerRole,
        z_order: i32,
        epoch: u64,
        display_list: DisplayList,
    ) {
        if let Some(prev_z) = self.pipeline_z.insert(pipeline, z_order) {
            if prev_z != z_order {
                self.layers.remove(&(prev_z, pipeline));
            }
        }
        let key = (z_order, pipeline);
        let external_placements = extract_external_surface_placements(&display_list);
        let has_backdrop_filters = display_list
            .items
            .iter()
            .any(|it| matches!(it, DisplayItem::BackdropFilter { .. }));
        match self.layers.get_mut(&key) {
            Some(existing) => {
                existing.role = role;
                existing.epoch = epoch;
                existing.display_list = display_list;
                existing.strip_cache = None;
                existing.layer_tree = None;
                existing.external_placements = external_placements;
                existing.animated_opacities.clear();
                existing.has_backdrop_filters = has_backdrop_filters;
                existing.dirty = true;
                existing.strip_recompose_needed = false;
                existing.tree_recompose_needed = false;
            }
            None => {
                self.layers.insert(
                    key,
                    RetainedLayer {
                        pipeline,
                        role,
                        z_order,
                        epoch,
                        display_list,
                        cached_scene: None,
                        strip_cache: None,
                        layer_tree: None,
                        external_placements,
                        animated_opacities: HashMap::new(),
                        has_backdrop_filters,
                        encode_generation: 0,
                        last_encode_stats: EncodeStats::default(),
                        dirty: true,
                        strip_recompose_needed: false,
                        tree_recompose_needed: false,
                    },
                );
            }
        }
        self.damage.mark_full();
        self.dirty = true;
    }

    pub fn remove_pipeline(&mut self, pipeline: PipelineId) -> bool {
        let Some(z_order) = self.pipeline_z.remove(&pipeline) else {
            return false;
        };
        let removed = self.layers.remove(&(z_order, pipeline)).is_some();
        self.resources.release_pipeline_images(pipeline);
        if removed {
            self.dirty = true;
        }
        removed
    }

    pub fn layer(&self, pipeline: PipelineId) -> Option<&RetainedLayer> {
        let z = self.pipeline_z.get(&pipeline)?;
        self.layers.get(&(*z, pipeline))
    }

    /// Handles APZ wheel/trackpad scrolling directly inside the compositor without cloning DisplayLists.
    pub fn handle_wheel(&mut self, point: Point, delta_x: f32, delta_y: f32) -> bool {
        let delta = ScrollDelta::new(delta_x, delta_y);
        for layer in self.layers.values_mut().rev() {
            let outcomes = apply_scroll_wheel(
                layer.pipeline,
                &mut layer.display_list.spatial_tree,
                point,
                delta,
            );
            let scrolled_nodes: Vec<_> = outcomes.into_iter().filter(|o| o.scrolled).collect();
            if scrolled_nodes.is_empty() {
                continue;
            }

            let mut needs_full_reencode = layer.strip_cache.is_none();
            for outcome in &scrolled_nodes {
                self.scroll_sync_queue.record(
                    outcome.pipeline,
                    outcome.node_id,
                    outcome.new_offset,
                );
                if let Some(strip_cache) = layer.strip_cache.as_mut() {
                    if outcome.node_id != strip_cache.root_scroll_id() {
                        if let Some(span) = nested_scroll_content_y_span(
                            &layer.display_list,
                            outcome.node_id,
                            strip_cache.root_scroll_id(),
                        ) {
                            strip_cache.invalidate_vertical_span(span.0, span.1);
                        } else {
                            needs_full_reencode = true;
                        }
                    }
                }
            }

            if needs_full_reencode {
                layer.dirty = true;
            } else {
                // Fast APZ path: keep `layer.dirty == false` so `encode_generation` is preserved
                // and only the visible 2–4 strips are re-translated at compose time.
                layer.strip_recompose_needed = true;
            }
            self.damage.mark_full();
            self.dirty = true;
            return true;
        }
        false
    }

    /// Adds a compositor-driven animation or CSS `transition` track directly onto
    /// `pipeline`'s retained `DisplayList` (runs at 60–120 Hz without waking layout).
    pub fn add_animation(&mut self, pipeline: PipelineId, anim: CompositorAnimation) -> bool {
        let Some(&z) = self.pipeline_z.get(&pipeline) else {
            return false;
        };
        let Some(layer) = self.layers.get_mut(&(z, pipeline)) else {
            return false;
        };
        layer.display_list.animations.retain(|a| a.id != anim.id);
        layer.display_list.animations.push(anim);
        layer.strip_cache = None;
        layer.layer_tree = None;
        layer.dirty = true;
        layer.tree_recompose_needed = false;
        self.damage.mark_full();
        self.dirty = true;
        true
    }

    /// Evaluates all compositor-driven `transform` and `opacity` animations across all
    /// retained layers at VSync timestamp `now_ms`.
    ///
    /// Returns the number of still-running animations. Once all finite animations finish,
    /// this returns `0` and leaves `self.dirty == false` so the compositor stays 0 Hz idle.
    pub fn tick_animations(&mut self, now_ms: f64) -> usize {
        let mut active_count = 0usize;
        for layer in self.layers.values_mut() {
            if layer.display_list.animations.is_empty() {
                continue;
            }
            let mut layer_changed = false;
            let mut changed_spatials: SmallVec<[SpatialNodeId; 4]> = SmallVec::new();
            let mut changed_opacities: SmallVec<[AnimationId; 4]> = SmallVec::new();
            let mut samples = Vec::new();
            for anim in &mut layer.display_list.animations {
                let (sample, still_active) = anim.tick(now_ms);
                if still_active {
                    active_count += 1;
                }
                if let Some(s) = sample {
                    samples.push(s);
                }
            }
            for sample in samples {
                match sample {
                    AnimationSample::Transform { spatial, matrix } => {
                        if let Some(node) = layer.display_list.spatial_tree.get_mut(spatial) {
                            if let SpatialNodeKind::Transform { matrix: cur, .. } = &mut node.kind {
                                if cur
                                    .iter()
                                    .zip(matrix.iter())
                                    .any(|(a, b)| (a - b).abs() > 1e-4)
                                {
                                    *cur = matrix;
                                    layer_changed = true;
                                    changed_spatials.push(spatial);
                                }
                            }
                        }
                    }
                    AnimationSample::Opacity {
                        animation_id,
                        opacity,
                    } => {
                        let prev = layer.animated_opacities.get(&animation_id).copied();
                        if prev.is_none_or(|p| (p - opacity).abs() > 1e-4) {
                            layer.animated_opacities.insert(animation_id, opacity);
                            layer_changed = true;
                            changed_opacities.push(animation_id);
                        }
                    }
                }
            }
            if layer_changed {
                if let Some(tree) = layer.layer_tree.as_mut() {
                    let mut all_covered = true;
                    for &sp in &changed_spatials {
                        let mut found = false;
                        for seg in &mut tree.segments {
                            if let LayerTreeSegment::Promoted(sub) = seg {
                                if sub.spatial == sp {
                                    let tf = layer
                                        .display_list
                                        .spatial_tree
                                        .resolve_world_transform(sub.spatial);
                                    let next_bounds = transform_aabb(sub.local_bounds, tf);
                                    self.damage
                                        .mark_rect(rect_union(sub.last_world_bounds, next_bounds));
                                    sub.last_world_bounds = next_bounds;
                                    found = true;
                                }
                            }
                        }
                        if !found {
                            all_covered = false;
                        }
                    }
                    for &aid in &changed_opacities {
                        let mut found = false;
                        for seg in &mut tree.segments {
                            if let LayerTreeSegment::Promoted(sub) = seg {
                                if sub.opacity_anim == Some(aid) {
                                    self.damage.mark_rect(sub.last_world_bounds);
                                    found = true;
                                }
                            }
                        }
                        if !found {
                            all_covered = false;
                        }
                    }
                    if all_covered {
                        // Fast LayerTree path: keep `layer.dirty == false` so `encode_generation`
                        // stays constant across 60–120 FPS animation frames!
                        layer.tree_recompose_needed = true;
                        self.dirty = true;
                        continue;
                    }
                }
                layer.strip_cache = None;
                layer.layer_tree = None;
                layer.dirty = true;
                self.damage.mark_full();
                self.dirty = true;
            }
        }
        active_count
    }

    /// Re-encodes only dirty layers and composes the final scene. Returns `None` when idle.
    pub fn compose(&mut self) -> Option<&Scene> {
        if !self.dirty {
            self.damage.clear_idle();
            return None;
        }
        self.compose_inner();
        Some(&self.composed_scene)
    }

    /// Forces composition even if nothing changed since the last frame (for headless snapshots).
    pub fn compose_force(&mut self) -> &Scene {
        self.compose_inner();
        &self.composed_scene
    }

    fn compose_inner(&mut self) {
        self.composed_scene.reset();
        let viewport_rect = Rect::new(
            0.0,
            0.0,
            self.viewport_size.width,
            self.viewport_size.height,
        );
        let base_vp = (!viewport_rect.is_empty()).then_some(viewport_rect);
        let dpr_scale = Affine::scale(f64::from(self.device_pixel_ratio));

        // Map each subframe pipeline to its declared surface dimensions (if bound) so
        // subframe culling respects the iframe's own viewport instead of the root viewport.
        let mut subframe_viewports: HashMap<PipelineId, Rect> = HashMap::new();
        for binding in self.external_bindings.values() {
            if let ExternalSurfaceBinding::ChildPipeline {
                pipeline,
                width,
                height,
            } = *binding
            {
                subframe_viewports
                    .insert(pipeline, Rect::new(0.0, 0.0, width as f32, height as f32));
            }
        }

        // Pass 1: Re-encode only dirty layers (root layers AND embedded subframes).
        for layer in self.layers.values_mut() {
            let layer_vp = if layer.role == LayerRole::Subframe {
                subframe_viewports.get(&layer.pipeline).copied().or(base_vp)
            } else {
                base_vp
            };
            let encode_opts = EncodeOptions {
                cull_rect: layer_vp.map(|vp| vp.inflate(DEFAULT_VIEWPORT_OVERSCAN_PX)),
                viewport_clip: layer_vp,
                base_transform: Affine::IDENTITY,
                override_scroll_zero: None,
                opacity_overrides: layer.animated_opacities.clone(),
            };
            if layer.dirty || layer.cached_scene.is_none() {
                let mut scene = layer.cached_scene.take().unwrap_or_default();
                scene.reset();
                let use_strips = layer.display_list.animations.is_empty()
                    && layer.animated_opacities.is_empty()
                    && !layer.has_backdrop_filters;
                layer.strip_cache = if use_strips {
                    StripSceneCache::try_new(&layer.display_list, self.strip_height_px)
                } else {
                    None
                };
                layer.layer_tree =
                    if !layer.display_list.animations.is_empty() && !layer.has_backdrop_filters {
                        RetainedLayerTree::try_build(
                            layer.pipeline,
                            &layer.display_list,
                            &mut self.resources,
                            &encode_opts,
                        )
                    } else {
                        None
                    };
                let stats = if let Some(strip_cache) = layer.strip_cache.as_mut() {
                    strip_cache.compose_visible(
                        layer.pipeline,
                        &layer.display_list,
                        &mut self.resources,
                        layer_vp,
                        &mut scene,
                    )
                } else if let Some(tree) = layer.layer_tree.as_mut() {
                    tree.compose_into(
                        &layer.display_list,
                        &layer.animated_opacities,
                        layer_vp,
                        &mut scene,
                    );
                    EncodeStats {
                        items_visited: layer.display_list.items.len() as u32,
                        items_encoded: layer.display_list.items.len() as u32,
                        items_culled: 0,
                        subtrees_culled: 0,
                    }
                } else {
                    encode_display_list(
                        layer.pipeline,
                        &layer.display_list,
                        &mut self.resources,
                        &encode_opts,
                        &mut scene,
                    )
                };
                layer.cached_scene = Some(scene);
                layer.encode_generation += 1;
                layer.last_encode_stats = stats;
                layer.dirty = false;
                layer.strip_recompose_needed = false;
                layer.tree_recompose_needed = false;
            } else if layer.tree_recompose_needed {
                if let Some(tree) = layer.layer_tree.as_mut() {
                    let mut scene = layer.cached_scene.take().unwrap_or_default();
                    scene.reset();
                    tree.compose_into(
                        &layer.display_list,
                        &layer.animated_opacities,
                        layer_vp,
                        &mut scene,
                    );
                    layer.cached_scene = Some(scene);
                }
                layer.tree_recompose_needed = false;
            } else if layer.strip_recompose_needed {
                if let Some(strip_cache) = layer.strip_cache.as_mut() {
                    let mut scene = layer.cached_scene.take().unwrap_or_default();
                    scene.reset();
                    let stats = strip_cache.compose_visible(
                        layer.pipeline,
                        &layer.display_list,
                        &mut self.resources,
                        layer_vp,
                        &mut scene,
                    );
                    layer.cached_scene = Some(scene);
                    layer.last_encode_stats = stats;
                }
                layer.strip_recompose_needed = false;
            }
        }

        // Pass 2: Composite top-level layers in z-order, embedding any bound ExternalSurface
        // scenes, live child OOPIF pipelines (recursively), or inline crashed-frame UI.
        let root_keys: Vec<(i32, PipelineId)> = self
            .layers
            .iter()
            .filter(|(_, layer)| layer.role != LayerRole::Subframe)
            .map(|(&k, _)| k)
            .collect();
        let any_backdrop = root_keys
            .iter()
            .any(|k| self.layers.get(k).is_some_and(|l| l.has_backdrop_filters));
        let mut target = std::mem::take(&mut self.composed_scene);
        for key in &root_keys {
            if let Some(layer) = self.layers.get(key) {
                self.composite_layer_into(&mut target, layer, dpr_scale, 0);
            }
        }
        self.composed_scene = target;

        // Pass 2b: When any visible root layer uses `backdrop-filter`, split the composed
        // output into ordered GPU `BackdropCompositePlan` stages for true painter's order.
        if any_backdrop {
            let mut plan = BackdropCompositePlan {
                base_scene: Scene::new(),
                steps: Vec::new(),
            };
            let dpr = self.device_pixel_ratio;
            for key in &root_keys {
                let Some(layer) = self.layers.get(key) else {
                    continue;
                };
                if !layer.has_backdrop_filters {
                    let dst = plan
                        .steps
                        .last_mut()
                        .map_or(&mut plan.base_scene, |s| &mut s.overlay_scene);
                    self.composite_layer_into(dst, layer, dpr_scale, 0);
                    continue;
                }
                let encode_opts = EncodeOptions {
                    cull_rect: base_vp.map(|vp| vp.inflate(DEFAULT_VIEWPORT_OVERSCAN_PX)),
                    viewport_clip: base_vp,
                    base_transform: dpr_scale,
                    override_scroll_zero: None,
                    opacity_overrides: layer.animated_opacities.clone(),
                };
                let mut open_pushes: Vec<(usize, SpatialNodeId)> = Vec::new();
                let mut seg_start = 0usize;
                let items = &layer.display_list.items;
                for (i, item) in items.iter().enumerate() {
                    match item {
                        DisplayItem::PushStackingContext { spatial, .. }
                        | DisplayItem::PushClip { spatial, .. }
                        | DisplayItem::PushClipPath { spatial, .. } => {
                            open_pushes.push((i, *spatial));
                        }
                        DisplayItem::PopStackingContext | DisplayItem::PopClip => {
                            open_pushes.pop();
                        }
                        DisplayItem::BackdropFilter {
                            bounds,
                            radii,
                            filters,
                        } => {
                            let ranges = build_segment_ranges(&open_pushes, seg_start, i);
                            if !ranges.is_empty() {
                                let dst = plan
                                    .steps
                                    .last_mut()
                                    .map_or(&mut plan.base_scene, |s| &mut s.overlay_scene);
                                encode_display_list_ranges(
                                    layer.pipeline,
                                    &layer.display_list,
                                    &ranges,
                                    &mut self.resources,
                                    &encode_opts,
                                    dst,
                                );
                            }
                            let cur_spatial =
                                open_pushes.last().map_or(SpatialNodeId(0), |&(_, sp)| sp);
                            let world_tf = dpr_scale
                                * layer
                                    .display_list
                                    .spatial_tree
                                    .resolve_world_transform(cur_spatial);
                            let bounds_px = transform_aabb(*bounds, world_tf);
                            let radii_px = radii.map(|r| (r * dpr).max(0.0));
                            let scaled_filters: SmallVec<[FilterOp; 2]> = filters
                                .iter()
                                .map(|op| match *op {
                                    FilterOp::Blur(r) => FilterOp::Blur(r * dpr),
                                    other => other,
                                })
                                .collect();
                            plan.steps.push(BackdropCompositeStep {
                                pass: BackdropFilterPass {
                                    bounds_px,
                                    radii_px,
                                    filters: scaled_filters,
                                },
                                overlay_scene: Scene::new(),
                            });
                            seg_start = i + 1;
                        }
                        _ => {}
                    }
                }
                let tail_ranges = build_segment_ranges(&open_pushes, seg_start, items.len());
                if !tail_ranges.is_empty() {
                    let dst = plan
                        .steps
                        .last_mut()
                        .map_or(&mut plan.base_scene, |s| &mut s.overlay_scene);
                    encode_display_list_ranges(
                        layer.pipeline,
                        &layer.display_list,
                        &tail_ranges,
                        &mut self.resources,
                        &encode_opts,
                        dst,
                    );
                }
            }
            self.backdrop_plan = if plan.steps.is_empty() {
                None
            } else {
                Some(plan)
            };
        } else {
            self.backdrop_plan = None;
        }

        let _ = self.damage.take_frame_damage(viewport_rect);
        self.dirty = false;
        self.frames_rendered += 1;
    }

    fn composite_layer_into(
        &self,
        target: &mut Scene,
        layer: &RetainedLayer,
        base_tf: Affine,
        depth: usize,
    ) {
        if let Some(layer_scene) = layer.cached_scene.as_ref() {
            target.append(layer_scene, Some(base_tf));
        }
        if depth >= MAX_EXTERNAL_SURFACE_DEPTH {
            return;
        }
        for slot in &layer.external_placements {
            let Some(binding) = self.external_bindings.get(&slot.surface_id) else {
                continue;
            };
            if slot.bounds.is_empty() {
                continue;
            }
            let world_tf = base_tf
                * layer
                    .display_list
                    .spatial_tree
                    .resolve_world_transform(slot.spatial_id);
            let [tl, tr, br, bl] = scale_corner_radii(
                f64::from(slot.bounds.size.width),
                f64::from(slot.bounds.size.height),
                slot.radii,
            );
            let clip_rr = RoundedRect::new(
                f64::from(slot.bounds.min_x()),
                f64::from(slot.bounds.min_y()),
                f64::from(slot.bounds.max_x()),
                f64::from(slot.bounds.max_y()),
                RoundedRectRadii::new(tl, tr, br, bl),
            );
            target.push_clip_layer(Fill::NonZero, world_tf, &clip_rr);
            match binding {
                ExternalSurfaceBinding::Scene(ext) => {
                    let sx = f64::from(slot.bounds.size.width) / f64::from(ext.width);
                    let sy = f64::from(slot.bounds.size.height) / f64::from(ext.height);
                    let content_tf = world_tf
                        * Affine::translate((
                            f64::from(slot.bounds.min_x()),
                            f64::from(slot.bounds.min_y()),
                        ))
                        * Affine::scale_non_uniform(sx, sy);
                    target.append(&ext.scene, Some(content_tf));
                }
                ExternalSurfaceBinding::Image {
                    image,
                    width,
                    height,
                } => {
                    let sx = f64::from(slot.bounds.size.width) / f64::from(*width);
                    let sy = f64::from(slot.bounds.size.height) / f64::from(*height);
                    let content_tf = world_tf
                        * Affine::translate((
                            f64::from(slot.bounds.min_x()),
                            f64::from(slot.bounds.min_y()),
                        ))
                        * Affine::scale_non_uniform(sx, sy);
                    target.draw_image(&ImageBrush::new(image.clone()), content_tf);
                }
                ExternalSurfaceBinding::ChildPipeline { pipeline, .. } => {
                    if let Some(child_layer) = self.layer(*pipeline) {
                        let child_tf = world_tf
                            * Affine::translate((
                                f64::from(slot.bounds.min_x()),
                                f64::from(slot.bounds.min_y()),
                            ));
                        self.composite_layer_into(target, child_layer, child_tf, depth + 1);
                    }
                }
                ExternalSurfaceBinding::Crashed { .. } => {
                    encode_crashed_subframe_surface(target, world_tf, slot.bounds);
                }
            }
            target.pop_layer();
        }
    }
}

/// Renders an inline crashed-frame fallback surface directly inside an OOPIF's
/// `ExternalSurface` bounds so a dead child process never takes down or blanks the parent.
fn encode_crashed_subframe_surface(target: &mut Scene, world_tf: Affine, bounds: Rect) {
    let x0 = f64::from(bounds.min_x());
    let y0 = f64::from(bounds.min_y());
    let x1 = f64::from(bounds.max_x());
    let y1 = f64::from(bounds.max_y());
    let w = (x1 - x0).max(1.0);
    let h = (y1 - y0).max(1.0);

    // Dark slate background (`#1e2430`)
    let bg_rect = KRect::new(x0, y0, x1, y1);
    target.fill(
        Fill::NonZero,
        world_tf,
        Color::from_rgb8(30, 36, 48),
        None,
        &bg_rect,
    );

    // Crisp red-amber inset border (`#ef4444`)
    let border_inset = 1.0;
    let border_rect = KRect::new(
        x0 + border_inset,
        y0 + border_inset,
        (x1 - border_inset).max(x0 + border_inset),
        (y1 - border_inset).max(y0 + border_inset),
    );
    target.stroke(
        &Stroke::new(2.0),
        world_tf,
        Color::from_rgb8(239, 68, 68),
        None,
        &border_rect,
    );

    // Centered circular crash badge with exclamation mark
    let cx = x0 + w * 0.5;
    let cy = y0 + h * 0.5;
    let radius = (w.min(h) * 0.18).clamp(8.0, 28.0);
    let badge = Circle::new((cx, cy), radius);
    target.fill(
        Fill::NonZero,
        world_tf,
        Color::from_rgb8(220, 38, 38),
        None,
        &badge,
    );
    let stem = Line::new((cx, cy - radius * 0.45), (cx, cy + radius * 0.12));
    target.stroke(
        &Stroke::new((radius * 0.18).max(2.0)),
        world_tf,
        Color::WHITE,
        None,
        &stem,
    );
    let dot = Circle::new((cx, cy + radius * 0.42), (radius * 0.11).max(1.5));
    target.fill(Fill::NonZero, world_tf, Color::WHITE, None, &dot);
}

fn extract_external_surface_placements(list: &DisplayList) -> Vec<ExternalSurfacePlacement> {
    let mut out = Vec::new();
    let mut spatial_stack = vec![SpatialNodeId(0)];
    for item in &list.items {
        match item {
            DisplayItem::PushStackingContext { spatial, .. }
            | DisplayItem::PushClip { spatial, .. }
            | DisplayItem::PushClipPath { spatial, .. } => {
                spatial_stack.push(*spatial);
            }
            DisplayItem::PopStackingContext | DisplayItem::PopClip => {
                if spatial_stack.len() > 1 {
                    spatial_stack.pop();
                }
            }
            DisplayItem::ExternalSurface {
                bounds,
                surface_id,
                radii,
            } => out.push(ExternalSurfacePlacement {
                surface_id: *surface_id,
                spatial_id: *spatial_stack.last().unwrap_or(&SpatialNodeId(0)),
                bounds: *bounds,
                radii: *radii,
            }),
            _ => {}
        }
    }
    out
}

fn nested_scroll_content_y_span(
    list: &DisplayList,
    node_id: rix_ipc::SpatialNodeId,
    root_scroll_id: rix_ipc::SpatialNodeId,
) -> Option<(f32, f32)> {
    let node = list.spatial_tree.get(node_id)?;
    let SpatialNodeKind::ScrollFrame { viewport_rect, .. } = &node.kind else {
        return None;
    };
    let parent_transform = match node.parent {
        Some(pid) => list
            .spatial_tree
            .resolve_world_transform_with_override(pid, Some(root_scroll_id)),
        None => Affine::IDENTITY,
    };
    let world_vp = transform_aabb(*viewport_rect, parent_transform);
    Some((world_vp.min_y(), world_vp.max_y()))
}

fn build_segment_ranges(
    open_pushes: &[(usize, SpatialNodeId)],
    seg_start: usize,
    seg_end: usize,
) -> Vec<Range<usize>> {
    if seg_start >= seg_end {
        return Vec::new();
    }
    let mut ranges: Vec<Range<usize>> = Vec::with_capacity(open_pushes.len() + 1);
    for &(push_idx, _) in open_pushes {
        if push_idx < seg_start {
            ranges.push(push_idx..(push_idx + 1));
        }
    }
    ranges.push(seg_start..seg_end);
    ranges
}
