use crate::display_list::{BlendMode, DisplayItem, DisplayList, SpatialNodeKind};
use crate::resources::ResourceRegistry;
use crate::scene_encoder::{
    encode_display_list_ranges, transform_aabb, EncodeOptions, EncodeStats,
};
use rix_core::Rect;
use rix_ipc::{PipelineId, SpatialNodeId};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use vello::kurbo::{Affine, Rect as KurboRect};
use vello::peniko::Fill;
use vello::Scene;

pub const DEFAULT_STRIP_HEIGHT_PX: f32 = 512.0;
pub const MIN_STRIP_HEIGHT_PX: f32 = 512.0;
pub const MAX_STRIP_HEIGHT_PX: f32 = 1024.0;
pub const DEFAULT_MAX_CACHED_STRIPS: usize = 24;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StripCacheStats {
    pub active: bool,
    pub strip_height_px: u32,
    pub indexed_strips: usize,
    pub cached_strips: usize,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub strips_evicted: u64,
    pub visible_strips_appended: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeRelation {
    RootFixed,
    RootScrolled,
    StickyOfRoot,
}

struct OpenWrapper {
    open_idx: usize,
    skip_in_strips: bool,
    emitted_in_fixed: bool,
    emitted_in_dynamic: bool,
    emitted_in_strips: HashSet<i32>,
    prev_spatial: SpatialNodeId,
}

struct CachedStrip {
    scene: Scene,
    last_used_frame: u64,
}

pub struct StripIndex {
    pub root_scroll_id: SpatialNodeId,
    pub strip_height: f32,
    pub horizontal_band: (f32, f32),
    pub fixed_ranges: Vec<Range<usize>>,
    pub dynamic_ranges: Vec<Range<usize>>,
    pub strip_ranges: HashMap<i32, Vec<Range<usize>>>,
    pub has_viewport_clip_wrapper: bool,
}

impl StripIndex {
    pub fn build(
        list: &DisplayList,
        root_scroll_id: SpatialNodeId,
        strip_height: f32,
    ) -> Option<Self> {
        let strip_height = strip_height.clamp(MIN_STRIP_HEIGHT_PX, MAX_STRIP_HEIGHT_PX);
        let root_node = list.spatial_tree.get(root_scroll_id)?;
        let SpatialNodeKind::ScrollFrame {
            content_rect,
            viewport_rect,
            ..
        } = &root_node.kind
        else {
            return None;
        };
        if viewport_rect.is_empty() || content_rect.size.height <= viewport_rect.size.height * 2.0 {
            return None;
        }

        let relations = classify_nodes(list, root_scroll_id);
        let content_transforms: Vec<Affine> = list
            .spatial_tree
            .nodes
            .iter()
            .map(|n| {
                list.spatial_tree
                    .resolve_world_transform_with_override(n.id, Some(root_scroll_id))
            })
            .collect();

        let lookup_relation = |id: SpatialNodeId| -> NodeRelation {
            list.spatial_tree
                .nodes
                .get(id.0 as usize)
                .filter(|n| n.id == id)
                .and_then(|_| relations.get(id.0 as usize).copied())
                .unwrap_or(NodeRelation::RootFixed)
        };
        let lookup_transform = |id: SpatialNodeId| -> Affine {
            list.spatial_tree
                .nodes
                .get(id.0 as usize)
                .filter(|n| n.id == id)
                .and_then(|_| content_transforms.get(id.0 as usize).copied())
                .unwrap_or(Affine::IDENTITY)
        };

        let mut index = Self {
            root_scroll_id,
            strip_height,
            horizontal_band: (
                content_rect.min_x().min(viewport_rect.min_x()) - 512.0,
                content_rect.max_x().max(viewport_rect.max_x()) + 512.0,
            ),
            fixed_ranges: Vec::new(),
            dynamic_ranges: Vec::new(),
            strip_ranges: HashMap::new(),
            has_viewport_clip_wrapper: false,
        };

        let mut current_spatial = SpatialNodeId(0);
        let mut wrappers: Vec<OpenWrapper> = Vec::new();
        let mut idx = 0usize;

        while idx < list.items.len() {
            match &list.items[idx] {
                DisplayItem::PushStackingContext {
                    spatial,
                    bounds,
                    opacity,
                    mix_blend,
                    filters,
                } => {
                    let rel = lookup_relation(*spatial);
                    let is_passthrough = *opacity >= 0.9999
                        && *mix_blend == BlendMode::Normal
                        && filters.is_empty()
                        && bounds.size.height > strip_height * 1.5
                        && matches!(rel, NodeRelation::RootScrolled | NodeRelation::RootFixed);
                    if is_passthrough {
                        wrappers.push(OpenWrapper {
                            open_idx: idx,
                            skip_in_strips: false,
                            emitted_in_fixed: false,
                            emitted_in_dynamic: false,
                            emitted_in_strips: HashSet::new(),
                            prev_spatial: current_spatial,
                        });
                        current_spatial = *spatial;
                        idx += 1;
                    } else {
                        let end = find_balanced_end(&list.items, idx, true);
                        let world = transform_aabb(*bounds, lookup_transform(*spatial));
                        index.record_range(
                            rel,
                            &mut wrappers,
                            idx..end,
                            world.min_y(),
                            world.max_y(),
                        );
                        idx = end;
                    }
                }
                DisplayItem::PopStackingContext => {
                    if let Some(w) = wrappers.pop() {
                        current_spatial = w.prev_spatial;
                        index.close_wrapper(w, idx);
                    }
                    idx += 1;
                }
                DisplayItem::PushClip { spatial, rect, .. } => {
                    let rel = lookup_relation(*spatial);
                    let matches_vp = rel == NodeRelation::RootFixed
                        && (rect.min_x() - viewport_rect.min_x()).abs() < 1.0
                        && (rect.min_y() - viewport_rect.min_y()).abs() < 1.0
                        && (rect.size.width - viewport_rect.size.width).abs() < 1.0
                        && (rect.size.height - viewport_rect.size.height).abs() < 1.0;
                    let is_passthrough = matches_vp
                        || (rel == NodeRelation::RootScrolled
                            && rect.size.height > strip_height * 1.5);
                    if is_passthrough {
                        if matches_vp {
                            index.has_viewport_clip_wrapper = true;
                        }
                        wrappers.push(OpenWrapper {
                            open_idx: idx,
                            skip_in_strips: matches_vp,
                            emitted_in_fixed: false,
                            emitted_in_dynamic: false,
                            emitted_in_strips: HashSet::new(),
                            prev_spatial: current_spatial,
                        });
                        current_spatial = *spatial;
                        idx += 1;
                    } else {
                        let end = find_balanced_end(&list.items, idx, false);
                        let world = transform_aabb(*rect, lookup_transform(*spatial));
                        index.record_range(
                            rel,
                            &mut wrappers,
                            idx..end,
                            world.min_y(),
                            world.max_y(),
                        );
                        idx = end;
                    }
                }
                DisplayItem::PopClip => {
                    if let Some(w) = wrappers.pop() {
                        current_spatial = w.prev_spatial;
                        index.close_wrapper(w, idx);
                    }
                    idx += 1;
                }
                item => {
                    let rel = lookup_relation(current_spatial);
                    if let Some(bounds) = item_local_bounds(item) {
                        let world = transform_aabb(bounds, lookup_transform(current_spatial));
                        index.record_range(
                            rel,
                            &mut wrappers,
                            idx..idx + 1,
                            world.min_y(),
                            world.max_y(),
                        );
                    }
                    idx += 1;
                }
            }
        }

        Some(index)
    }

    fn record_range(
        &mut self,
        rel: NodeRelation,
        wrappers: &mut [OpenWrapper],
        range: Range<usize>,
        min_y: f32,
        max_y: f32,
    ) {
        match rel {
            NodeRelation::RootFixed => {
                for w in wrappers.iter_mut() {
                    if !w.emitted_in_fixed {
                        push_merged_range(&mut self.fixed_ranges, w.open_idx..w.open_idx + 1);
                        w.emitted_in_fixed = true;
                    }
                }
                push_merged_range(&mut self.fixed_ranges, range);
            }
            NodeRelation::StickyOfRoot => {
                for w in wrappers.iter_mut() {
                    if !w.emitted_in_dynamic {
                        push_merged_range(&mut self.dynamic_ranges, w.open_idx..w.open_idx + 1);
                        w.emitted_in_dynamic = true;
                    }
                }
                push_merged_range(&mut self.dynamic_ranges, range);
            }
            NodeRelation::RootScrolled => {
                let first_strip = (min_y / self.strip_height).floor() as i32;
                let last_strip = ((max_y - 0.001) / self.strip_height).floor() as i32;
                if last_strip < first_strip {
                    return;
                }
                for strip_id in first_strip..=last_strip {
                    let bucket = self.strip_ranges.entry(strip_id).or_default();
                    for w in wrappers.iter_mut() {
                        if !w.skip_in_strips && !w.emitted_in_strips.contains(&strip_id) {
                            push_merged_range(bucket, w.open_idx..w.open_idx + 1);
                            w.emitted_in_strips.insert(strip_id);
                        }
                    }
                    push_merged_range(bucket, range.clone());
                }
            }
        }
    }

    fn close_wrapper(&mut self, wrapper: OpenWrapper, pop_idx: usize) {
        if wrapper.emitted_in_fixed {
            push_merged_range(&mut self.fixed_ranges, pop_idx..pop_idx + 1);
        }
        if wrapper.emitted_in_dynamic {
            push_merged_range(&mut self.dynamic_ranges, pop_idx..pop_idx + 1);
        }
        if !wrapper.skip_in_strips {
            for strip_id in wrapper.emitted_in_strips {
                if let Some(bucket) = self.strip_ranges.get_mut(&strip_id) {
                    push_merged_range(bucket, pop_idx..pop_idx + 1);
                }
            }
        }
    }
}

pub struct StripSceneCache {
    index: StripIndex,
    fixed_scene: Option<Scene>,
    strips: HashMap<i32, CachedStrip>,
    max_cached_strips: usize,
    tick: u64,
    cache_hits: u64,
    cache_misses: u64,
    strips_evicted: u64,
    last_visible_strips: usize,
}

impl StripSceneCache {
    pub fn try_new(list: &DisplayList, strip_height: f32) -> Option<Self> {
        let root_scroll_id = crate::apz::find_root_scroll_node(list)?;
        let index = StripIndex::build(list, root_scroll_id, strip_height)?;
        Some(Self {
            index,
            fixed_scene: None,
            strips: HashMap::new(),
            max_cached_strips: DEFAULT_MAX_CACHED_STRIPS,
            tick: 0,
            cache_hits: 0,
            cache_misses: 0,
            strips_evicted: 0,
            last_visible_strips: 0,
        })
    }

    pub fn root_scroll_id(&self) -> SpatialNodeId {
        self.index.root_scroll_id
    }

    pub fn invalidate_vertical_span(&mut self, min_y: f32, max_y: f32) {
        let first = (min_y / self.index.strip_height).floor() as i32;
        let last = (max_y / self.index.strip_height).floor() as i32;
        for strip_id in first..=last {
            self.strips.remove(&strip_id);
        }
    }

    pub fn stats(&self) -> StripCacheStats {
        StripCacheStats {
            active: true,
            strip_height_px: self.index.strip_height as u32,
            indexed_strips: self.index.strip_ranges.len(),
            cached_strips: self.strips.len(),
            cache_hits: self.cache_hits,
            cache_misses: self.cache_misses,
            strips_evicted: self.strips_evicted,
            visible_strips_appended: self.last_visible_strips,
        }
    }

    /// Composes ONLY the 2–4 visible strips at the current scroll offset into `target_scene`.
    pub fn compose_visible(
        &mut self,
        pipeline: PipelineId,
        list: &DisplayList,
        resources: &mut ResourceRegistry,
        base_viewport: Option<Rect>,
        target_scene: &mut Scene,
    ) -> EncodeStats {
        let mut total_stats = EncodeStats::default();
        let Some(scroll_node) = list.spatial_tree.get(self.index.root_scroll_id) else {
            return total_stats;
        };
        let SpatialNodeKind::ScrollFrame {
            viewport_rect,
            current_offset,
            ..
        } = &scroll_node.kind
        else {
            return total_stats;
        };

        self.tick = self.tick.wrapping_add(1);
        let effective_vp = base_viewport
            .and_then(|vp| vp.intersect(viewport_rect))
            .unwrap_or(*viewport_rect);
        if effective_vp.is_empty() {
            return total_stats;
        }
        let vis_min_y = effective_vp.min_y() + current_offset.y;
        let vis_max_y = effective_vp.max_y() + current_offset.y;
        let first_strip = (vis_min_y / self.index.strip_height).floor() as i32;
        let last_strip = ((vis_max_y - 0.001) / self.index.strip_height).floor() as i32;

        let scroll_transform =
            Affine::translate((-f64::from(current_offset.x), -f64::from(current_offset.y)));
        let vp_kurbo = KurboRect::new(
            f64::from(effective_vp.min_x()),
            f64::from(effective_vp.min_y()),
            f64::from(effective_vp.max_x()),
            f64::from(effective_vp.max_y()),
        );
        target_scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &vp_kurbo);

        let mut appended = 0usize;
        for strip_id in first_strip..=last_strip {
            let Some(ranges) = self.index.strip_ranges.get(&strip_id) else {
                continue;
            };
            if ranges.is_empty() {
                continue;
            }
            let cached = if let Some(entry) = self.strips.get_mut(&strip_id) {
                entry.last_used_frame = self.tick;
                self.cache_hits += 1;
                entry
            } else {
                self.cache_misses += 1;
                let band_y0 = strip_id as f32 * self.index.strip_height;
                let band_x0 = self.index.horizontal_band.0;
                let band_w = (self.index.horizontal_band.1 - band_x0).max(1.0);
                let band_rect = Rect::new(band_x0, band_y0, band_w, self.index.strip_height);

                let mut strip_scene = Scene::new();
                let strip_clip = KurboRect::new(
                    f64::from(band_rect.min_x()),
                    f64::from(band_rect.min_y()),
                    f64::from(band_rect.max_x()),
                    f64::from(band_rect.max_y()),
                );
                // Strict strip clip guarantees zero double-blending at strip seams.
                strip_scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &strip_clip);
                let opts = EncodeOptions {
                    cull_rect: Some(band_rect),
                    viewport_clip: Some(band_rect),
                    base_transform: Affine::IDENTITY,
                    override_scroll_zero: Some(self.index.root_scroll_id),
                };
                let stats = encode_display_list_ranges(
                    pipeline,
                    list,
                    ranges,
                    resources,
                    &opts,
                    &mut strip_scene,
                );
                strip_scene.pop_layer();

                total_stats.items_visited += stats.items_visited;
                total_stats.items_encoded += stats.items_encoded;
                total_stats.items_culled += stats.items_culled;
                total_stats.subtrees_culled += stats.subtrees_culled;

                self.evict_lru_if_full();
                self.strips.insert(
                    strip_id,
                    CachedStrip {
                        scene: strip_scene,
                        last_used_frame: self.tick,
                    },
                );
                self.strips.get_mut(&strip_id).expect("just inserted")
            };

            target_scene.append(&cached.scene, Some(scroll_transform));
            appended += 1;
        }
        target_scene.pop_layer();
        self.last_visible_strips = appended;

        if !self.index.dynamic_ranges.is_empty() {
            let opts = EncodeOptions {
                cull_rect: Some(effective_vp),
                viewport_clip: Some(effective_vp),
                base_transform: Affine::IDENTITY,
                override_scroll_zero: None,
            };
            let dyn_stats = encode_display_list_ranges(
                pipeline,
                list,
                &self.index.dynamic_ranges,
                resources,
                &opts,
                target_scene,
            );
            total_stats.items_visited += dyn_stats.items_visited;
            total_stats.items_encoded += dyn_stats.items_encoded;
        }

        if !self.index.fixed_ranges.is_empty() {
            let fixed = self.fixed_scene.get_or_insert_with(|| {
                let mut scene = Scene::new();
                let opts = EncodeOptions {
                    cull_rect: base_viewport,
                    viewport_clip: base_viewport,
                    base_transform: Affine::IDENTITY,
                    override_scroll_zero: None,
                };
                encode_display_list_ranges(
                    pipeline,
                    list,
                    &self.index.fixed_ranges,
                    resources,
                    &opts,
                    &mut scene,
                );
                scene
            });
            target_scene.append(fixed, None);
        }

        total_stats
    }

    fn evict_lru_if_full(&mut self) {
        while self.strips.len() >= self.max_cached_strips {
            let Some(&oldest_id) = self
                .strips
                .iter()
                .min_by_key(|(_, strip)| strip.last_used_frame)
                .map(|(id, _)| id)
            else {
                break;
            };
            self.strips.remove(&oldest_id);
            self.strips_evicted += 1;
        }
    }
}

fn classify_nodes(list: &DisplayList, root_scroll_id: SpatialNodeId) -> Vec<NodeRelation> {
    list.spatial_tree
        .nodes
        .iter()
        .map(|node| {
            let mut cur = Some(node.id);
            let mut steps = 0usize;
            let mut has_sticky = false;
            let mut under_scroll = false;
            while let Some(id) = cur {
                if steps >= list.spatial_tree.nodes.len() {
                    break;
                }
                steps += 1;
                if id == root_scroll_id {
                    under_scroll = true;
                }
                let Some(n) = list.spatial_tree.get(id) else {
                    break;
                };
                if let SpatialNodeKind::StickyFrame { scroll_parent, .. } = &n.kind {
                    has_sticky = true;
                    if *scroll_parent == root_scroll_id {
                        under_scroll = true;
                    }
                }
                cur = n.parent;
            }
            if has_sticky && under_scroll {
                NodeRelation::StickyOfRoot
            } else if under_scroll {
                NodeRelation::RootScrolled
            } else {
                NodeRelation::RootFixed
            }
        })
        .collect()
}

fn find_balanced_end(items: &[DisplayItem], start: usize, stacking: bool) -> usize {
    let mut depth = 1usize;
    let mut idx = start + 1;
    while idx < items.len() {
        match &items[idx] {
            DisplayItem::PushStackingContext { .. } if stacking => depth += 1,
            DisplayItem::PopStackingContext if stacking => {
                depth -= 1;
                if depth == 0 {
                    return idx + 1;
                }
            }
            DisplayItem::PushClip { .. } if !stacking => depth += 1,
            DisplayItem::PopClip if !stacking => {
                depth -= 1;
                if depth == 0 {
                    return idx + 1;
                }
            }
            _ => {}
        }
        idx += 1;
    }
    items.len()
}

fn push_merged_range(vec: &mut Vec<Range<usize>>, range: Range<usize>) {
    if range.is_empty() {
        return;
    }
    if let Some(last) = vec.last_mut() {
        if last.end == range.start {
            last.end = range.end;
            return;
        }
    }
    vec.push(range);
}

fn item_local_bounds(item: &DisplayItem) -> Option<Rect> {
    match item {
        DisplayItem::Rect { bounds, .. }
        | DisplayItem::LinearGradient { bounds, .. }
        | DisplayItem::Border { bounds, .. }
        | DisplayItem::GlyphRun { bounds, .. }
        | DisplayItem::Image { bounds, .. }
        | DisplayItem::VectorPath { bounds, .. }
        | DisplayItem::ExternalSurface { bounds, .. } => Some(*bounds),
        DisplayItem::BoxShadow {
            bounds,
            offset,
            blur_radius,
            spread_radius,
            inset,
            ..
        } => {
            if *inset {
                Some(*bounds)
            } else {
                let margin = blur_radius.max(0.0) * 1.5 + spread_radius.abs();
                Some(bounds.translate(*offset).inflate(margin))
            }
        }
        DisplayItem::PushStackingContext { .. }
        | DisplayItem::PopStackingContext
        | DisplayItem::PushClip { .. }
        | DisplayItem::PopClip => None,
    }
}
