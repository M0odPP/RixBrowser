use crate::animations::{AnimationSample, CompositorAnimation};
use crate::apz::{apply_scroll_wheel, ScrollDelta, ScrollSyncQueue};
use crate::display_list::{AnimationId, DisplayItem, DisplayList, SpatialNodeKind};
use crate::resources::ResourceRegistry;
use crate::scene_encoder::{
    encode_display_list, scale_corner_radii, transform_aabb, EncodeOptions, EncodeStats,
    DEFAULT_VIEWPORT_OVERSCAN_PX,
};
use crate::strip_cache::{StripCacheStats, StripSceneCache, DEFAULT_STRIP_HEIGHT_PX};
use rix_core::{Point, Rect, Size};
use rix_ipc::{CompositorToContent, PipelineId, SpatialNodeId, SurfaceId};
use std::collections::{BTreeMap, HashMap};
use vello::kurbo::{Affine, Circle, Line, Rect as KRect, RoundedRect, RoundedRectRadii, Stroke};
use vello::peniko::{Color, Fill};
use vello::Scene;

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
    /// GPU-backed vector/texture scene (e.g. Canvas2D, WebGPU, Video).
    Scene(Box<LiveExternalSurface>),
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
    external_placements: Vec<ExternalSurfacePlacement>,
    animated_opacities: HashMap<AnimationId, f32>,
    encode_generation: u64,
    last_encode_stats: EncodeStats,
    dirty: bool,
    strip_recompose_needed: bool,
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
    dirty: bool,
    frames_rendered: u64,
    composed_scene: Scene,
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
            dirty: false,
            frames_rendered: 0,
            composed_scene: Scene::new(),
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

    pub fn strip_cache_stats(&self, pipeline: PipelineId) -> Option<StripCacheStats> {
        self.layer(pipeline)
            .and_then(RetainedLayer::strip_cache_stats)
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
        match self.layers.get_mut(&key) {
            Some(existing) => {
                existing.role = role;
                existing.epoch = epoch;
                existing.display_list = display_list;
                existing.strip_cache = None;
                existing.external_placements = external_placements;
                existing.animated_opacities.clear();
                existing.dirty = true;
                existing.strip_recompose_needed = false;
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
                        external_placements,
                        animated_opacities: HashMap::new(),
                        encode_generation: 0,
                        last_encode_stats: EncodeStats::default(),
                        dirty: true,
                        strip_recompose_needed: false,
                    },
                );
            }
        }
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
        layer.dirty = true;
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
                        }
                    }
                }
            }
            if layer_changed {
                layer.strip_cache = None;
                layer.dirty = true;
                self.dirty = true;
            }
        }
        active_count
    }

    /// Re-encodes only dirty layers and composes the final scene. Returns `None` when idle.
    pub fn compose(&mut self) -> Option<&Scene> {
        if !self.dirty {
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
                let use_strips =
                    layer.display_list.animations.is_empty() && layer.animated_opacities.is_empty();
                layer.strip_cache = if use_strips {
                    StripSceneCache::try_new(&layer.display_list, self.strip_height_px)
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
        let mut target = std::mem::take(&mut self.composed_scene);
        for key in root_keys {
            if let Some(layer) = self.layers.get(&key) {
                self.composite_layer_into(&mut target, layer, dpr_scale, 0);
            }
        }
        self.composed_scene = target;

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
