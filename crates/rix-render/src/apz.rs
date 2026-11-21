use crate::display_list::{DisplayList, SpatialNodeKind, SpatialTree};
use rix_core::{Point, Rect, Vec2};
use rix_ipc::{CompositorToContent, PipelineId, ScrollId, SpatialNodeId};
use std::collections::BTreeMap;
use vello::kurbo::Affine;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ScrollDelta {
    pub dx: f32,
    pub dy: f32,
}

impl ScrollDelta {
    pub const fn new(dx: f32, dy: f32) -> Self {
        Self { dx, dy }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollOutcome {
    pub scrolled: bool,
    pub pipeline: PipelineId,
    pub node_id: SpatialNodeId,
    pub scroll_id: ScrollId,
    pub new_offset: Vec2,
    pub remaining_delta: Vec2,
}

/// Clamps a scroll offset within `[0, max(content - viewport, 0)]`.
#[inline]
pub fn clamp_scroll_offset(content_rect: Rect, viewport_rect: Rect, offset: Vec2) -> Vec2 {
    let max_x = (content_rect.size.width - viewport_rect.size.width).max(0.0);
    let max_y = (content_rect.size.height - viewport_rect.size.height).max(0.0);
    let x = if offset.x.is_finite() {
        offset.x.clamp(0.0, max_x)
    } else {
        0.0
    };
    let y = if offset.y.is_finite() {
        offset.y.clamp(0.0, max_y)
    } else {
        0.0
    };
    Vec2::new(x, y)
}

fn ancestor_depth(tree: &SpatialTree, id: SpatialNodeId) -> usize {
    let mut depth = 0usize;
    let mut cur = tree.get(id).and_then(|n| n.parent);
    while let Some(pid) = cur {
        if depth >= tree.nodes.len() {
            break;
        }
        depth += 1;
        cur = tree.get(pid).and_then(|n| n.parent);
    }
    depth
}

/// Computes the screen/world-space clipped viewport bounds of a `SpatialNodeId` (excluding the node's own scroll offset).
pub(crate) fn scroll_frame_world_viewport(
    tree: &SpatialTree,
    node_id: SpatialNodeId,
    viewport_rect: Rect,
) -> Option<Rect> {
    let mut chain = Vec::new();
    let mut cur = tree.get(node_id).and_then(|n| n.parent);
    let mut steps = 0usize;
    while let Some(pid) = cur {
        if steps >= tree.nodes.len() {
            break;
        }
        steps += 1;
        let Some(pnode) = tree.get(pid) else {
            break;
        };
        chain.push(pnode);
        cur = pnode.parent;
    }

    let mut parent_world = Affine::IDENTITY;
    let mut clip: Option<Rect> = None;
    for pnode in chain.into_iter().rev() {
        if let SpatialNodeKind::ScrollFrame {
            viewport_rect: parent_vp,
            ..
        } = &pnode.kind
        {
            let world_vp = crate::scene_encoder::transform_aabb(*parent_vp, parent_world);
            clip = Some(match clip {
                Some(prev) => prev.intersect(&world_vp).unwrap_or(Rect::ZERO),
                None => world_vp,
            });
        }
        parent_world *= tree.local_transform(pnode);
    }

    let self_world_vp = crate::scene_encoder::transform_aabb(viewport_rect, parent_world);
    Some(match clip {
        Some(c) => self_world_vp.intersect(&c).unwrap_or(Rect::ZERO),
        None => self_world_vp,
    })
}

/// Returns matching `ScrollFrame` node IDs whose world viewport contains `point`, ordered deepest-first.
pub fn hit_test_scroll_chain(tree: &SpatialTree, point: Point) -> Vec<SpatialNodeId> {
    let mut hits: Vec<(usize, usize, SpatialNodeId)> = Vec::new();
    for (idx, node) in tree.nodes.iter().enumerate() {
        let SpatialNodeKind::ScrollFrame { viewport_rect, .. } = &node.kind else {
            continue;
        };
        let Some(world_vp) = scroll_frame_world_viewport(tree, node.id, *viewport_rect) else {
            continue;
        };
        if !world_vp.is_empty() && world_vp.contains(point) {
            hits.push((ancestor_depth(tree, node.id), idx, node.id));
        }
    }
    // Deepest nested first; break ties by later node order in the spatial tree.
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    hits.into_iter().map(|(_, _, id)| id).collect()
}

/// Applies wheel/trackpad `delta` directly onto `tree` in place (zero `DisplayList` cloning),
/// chaining unconsumed delta up the nested `ScrollFrame` chain.
pub fn apply_scroll_wheel(
    pipeline: PipelineId,
    tree: &mut SpatialTree,
    point: Point,
    delta: ScrollDelta,
) -> Vec<ScrollOutcome> {
    let chain = hit_test_scroll_chain(tree, point);
    let mut rem_x = if delta.dx.is_finite() { delta.dx } else { 0.0 };
    let mut rem_y = if delta.dy.is_finite() { delta.dy } else { 0.0 };
    let mut outcomes = Vec::new();

    for node_id in chain {
        if rem_x.abs() <= 1e-4 && rem_y.abs() <= 1e-4 {
            break;
        }
        let Some(node) = tree.get_mut(node_id) else {
            continue;
        };
        let SpatialNodeKind::ScrollFrame {
            content_rect,
            viewport_rect,
            external_scroll_id,
            current_offset,
        } = &mut node.kind
        else {
            continue;
        };
        let prev = *current_offset;
        let clamped_prev = clamp_scroll_offset(*content_rect, *viewport_rect, prev);
        let desired = Vec2::new(clamped_prev.x + rem_x, clamped_prev.y + rem_y);
        let next = clamp_scroll_offset(*content_rect, *viewport_rect, desired);

        let consumed_x = next.x - clamped_prev.x;
        let consumed_y = next.y - clamped_prev.y;
        *current_offset = next;
        rem_x -= consumed_x;
        rem_y -= consumed_y;

        let scrolled = (next.x - prev.x).abs() > 1e-4 || (next.y - prev.y).abs() > 1e-4;
        outcomes.push(ScrollOutcome {
            scrolled,
            pipeline,
            node_id,
            scroll_id: *external_scroll_id,
            new_offset: next,
            remaining_delta: Vec2::new(rem_x, rem_y),
        });
    }

    outcomes
}

/// Identifies a pipeline's root `ScrollFrame` when eligible for vertical strip caching.
pub(crate) fn find_root_scroll_node(list: &DisplayList) -> Option<SpatialNodeId> {
    list.spatial_tree.nodes.iter().find_map(|node| {
        if node.parent != Some(SpatialNodeId(0)) && node.parent.is_some() {
            return None;
        }
        if matches!(node.kind, SpatialNodeKind::ScrollFrame { .. }) {
            Some(node.id)
        } else {
            None
        }
    })
}

/// Coalesces multiple compositor scroll updates within one VSync window so Content receives
/// at most one `ScrollOffsetSync` per scrolled `(PipelineId, SpatialNodeId)` per VSync.
#[derive(Debug, Default)]
pub struct ScrollSyncQueue {
    pending: BTreeMap<(PipelineId, SpatialNodeId), Point>,
}

impl ScrollSyncQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, pipeline: PipelineId, node_id: SpatialNodeId, offset: Vec2) {
        self.pending
            .insert((pipeline, node_id), Point::new(offset.x, offset.y));
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn drain_vsync_syncs(&mut self) -> Vec<CompositorToContent> {
        let drained = std::mem::take(&mut self.pending);
        drained
            .into_iter()
            .map(
                |((pipeline, node_id), offset)| CompositorToContent::ScrollOffsetSync {
                    pipeline,
                    node_id,
                    offset,
                },
            )
            .collect()
    }
}
