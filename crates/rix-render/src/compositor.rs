use crate::display_list::DisplayList;
use crate::resources::ResourceRegistry;
use crate::scene_encoder::{encode_display_list, EncodeOptions, EncodeStats};
use rix_core::{Rect, Size};
use rix_ipc::PipelineId;
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
    encode_generation: u64,
    last_encode_stats: EncodeStats,
    dirty: bool,
}

impl RetainedLayer {
    pub fn encode_generation(&self) -> u64 {
        self.encode_generation
    }

    pub fn last_encode_stats(&self) -> EncodeStats {
        self.last_encode_stats
    }
}

pub struct Compositor {
    layers: BTreeMap<(i32, PipelineId), RetainedLayer>,
    pipeline_z: BTreeMap<PipelineId, i32>,
    pub resources: ResourceRegistry,
    viewport_size: Size,
    device_pixel_ratio: f32,
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
                existing.dirty = true;
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
                        encode_generation: 0,
                        last_encode_stats: EncodeStats::default(),
                        dirty: true,
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
        let encode_opts = EncodeOptions {
            cull_rect: None,
            viewport_clip: (!viewport_rect.is_empty()).then_some(viewport_rect),
            base_transform: Affine::IDENTITY,
        };
        let dpr_scale = Affine::scale(f64::from(self.device_pixel_ratio));

        for layer in self.layers.values_mut() {
            if layer.dirty || layer.cached_scene.is_none() {
                let mut scene = layer.cached_scene.take().unwrap_or_default();
                scene.reset();
                let stats = encode_display_list(
                    layer.pipeline,
                    &layer.display_list,
                    &mut self.resources,
                    &encode_opts,
                    &mut scene,
                );
                layer.cached_scene = Some(scene);
                layer.encode_generation += 1;
                layer.last_encode_stats = stats;
                layer.dirty = false;
            }
            if let Some(layer_scene) = layer.cached_scene.as_ref() {
                self.composed_scene.append(layer_scene, Some(dpr_scale));
            }
        }

        self.dirty = false;
        self.frames_rendered += 1;
    }
}
