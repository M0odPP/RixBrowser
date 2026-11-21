use crate::apz::{apply_scroll_wheel, ScrollDelta, ScrollSyncQueue};
use crate::display_list::{DisplayList, SpatialNodeKind};
use crate::resources::ResourceRegistry;
use crate::scene_encoder::{
    encode_display_list, transform_aabb, EncodeOptions, EncodeStats, DEFAULT_VIEWPORT_OVERSCAN_PX,
};
use crate::strip_cache::{StripCacheStats, StripSceneCache, DEFAULT_STRIP_HEIGHT_PX};
use rix_core::{Point, Rect, Size};
use rix_ipc::{CompositorToContent, PipelineId};
use std::collections::BTreeMap;
use vello::kurbo::Affine;
use vello::Scene;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LayerRole {
    WebSurface,
    Chrome,
    Overlay,
}

impl LayerRole {
    pub const fn default_z_order(self) -> i32 {
        match self {
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
            viewport_size,
            device_pixel_ratio: device_pixel_ratio.max(0.5),
            strip_height_px: DEFAULT_STRIP_HEIGHT_PX,
            scroll_sync_queue: ScrollSyncQueue::new(),
            dirty: false,
            frames_rendered: 0,
            composed_scene: Scene::new(),
        }
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
        match self.layers.get_mut(&key) {
            Some(existing) => {
                existing.role = role;
                existing.epoch = epoch;
                existing.display_list = display_list;
                existing.strip_cache = None;
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
        let encode_opts = EncodeOptions {
            cull_rect: base_vp.map(|vp| vp.inflate(DEFAULT_VIEWPORT_OVERSCAN_PX)),
            viewport_clip: base_vp,
            base_transform: Affine::IDENTITY,
            override_scroll_zero: None,
        };
        let dpr_scale = Affine::scale(f64::from(self.device_pixel_ratio));

        for layer in self.layers.values_mut() {
            if layer.dirty || layer.cached_scene.is_none() {
                let mut scene = layer.cached_scene.take().unwrap_or_default();
                scene.reset();
                layer.strip_cache =
                    StripSceneCache::try_new(&layer.display_list, self.strip_height_px);
                let stats = if let Some(strip_cache) = layer.strip_cache.as_mut() {
                    strip_cache.compose_visible(
                        layer.pipeline,
                        &layer.display_list,
                        &mut self.resources,
                        base_vp,
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
                        base_vp,
                        &mut scene,
                    );
                    layer.cached_scene = Some(scene);
                    layer.last_encode_stats = stats;
                }
                layer.strip_recompose_needed = false;
            }
            if let Some(layer_scene) = layer.cached_scene.as_ref() {
                self.composed_scene.append(layer_scene, Some(dpr_scale));
            }
        }

        self.dirty = false;
        self.frames_rendered += 1;
    }
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
