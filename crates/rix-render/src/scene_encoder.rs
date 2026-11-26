use crate::display_list::{AnimationId, ClipPathShape, DisplayItem, DisplayList, FilterOp, Rgba8};
use crate::gpu_filters::ColorFilterMatrix;
use crate::resources::ResourceRegistry;
use rix_core::Rect;
use rix_ipc::{PipelineId, SpatialNodeId};
use smallvec::SmallVec;
use std::collections::HashMap;
use std::ops::Range;
use vello::kurbo::{
    Affine, BezPath, Circle, Ellipse, Rect as KurboRect, RoundedRect, RoundedRectRadii, Shape,
    Stroke,
};
use vello::peniko::{Fill, Gradient, ImageBrush, InterpolationAlphaSpace};
use vello::{Glyph, Scene};

pub const DEFAULT_VIEWPORT_OVERSCAN_PX: f32 = 128.0;

#[derive(Debug, Clone)]
pub struct EncodeOptions {
    pub cull_rect: Option<Rect>,
    pub viewport_clip: Option<Rect>,
    pub base_transform: Affine,
    pub override_scroll_zero: Option<SpatialNodeId>,
    pub opacity_overrides: HashMap<AnimationId, f32>,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            cull_rect: None,
            viewport_clip: None,
            base_transform: Affine::IDENTITY,
            override_scroll_zero: None,
            opacity_overrides: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ActiveFilterFrame {
    color_matrix: ColorFilterMatrix,
    blur_radius: f32,
}

impl Default for ActiveFilterFrame {
    fn default() -> Self {
        Self {
            color_matrix: ColorFilterMatrix::IDENTITY,
            blur_radius: 0.0,
        }
    }
}

impl ActiveFilterFrame {
    fn apply_color(&self, c: Rgba8) -> Rgba8 {
        self.color_matrix.apply_rgba(c)
    }

    fn combine(parent: Self, ops: &[FilterOp]) -> Self {
        let mut next = parent;
        let step_matrix = ColorFilterMatrix::from_filters(ops, false);
        next.color_matrix = next.color_matrix.then(step_matrix);
        for op in ops {
            if let FilterOp::Blur(r) = *op {
                next.blur_radius += r.max(0.0);
            }
        }
        next
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EncodeStats {
    pub items_visited: u32,
    pub items_encoded: u32,
    pub items_culled: u32,
    pub subtrees_culled: u32,
}

/// Proportionally scales corner radii per W3C CSS Backgrounds & Borders Level 3 §5.5.
#[inline]
pub fn scale_corner_radii(width: f64, height: f64, radii: [f32; 4]) -> [f64; 4] {
    let [tl, tr, br, bl] = radii.map(|r| f64::from(r).max(0.0));
    let mut factor = 1.0f64;
    let top_sum = tl + tr;
    if top_sum > width && top_sum > 0.0 {
        factor = factor.min(width / top_sum);
    }
    let bottom_sum = bl + br;
    if bottom_sum > width && bottom_sum > 0.0 {
        factor = factor.min(width / bottom_sum);
    }
    let left_sum = tl + bl;
    if left_sum > height && left_sum > 0.0 {
        factor = factor.min(height / left_sum);
    }
    let right_sum = tr + br;
    if right_sum > height && right_sum > 0.0 {
        factor = factor.min(height / right_sum);
    }
    [tl * factor, tr * factor, br * factor, bl * factor]
}

#[inline]
pub(crate) fn transform_aabb(rect: Rect, affine: Affine) -> Rect {
    if rect.is_empty() {
        return Rect::ZERO;
    }
    let corners = [
        (f64::from(rect.min_x()), f64::from(rect.min_y())),
        (f64::from(rect.max_x()), f64::from(rect.min_y())),
        (f64::from(rect.max_x()), f64::from(rect.max_y())),
        (f64::from(rect.min_x()), f64::from(rect.max_y())),
    ];
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for (x, y) in corners {
        let p = affine * vello::kurbo::Point::new(x, y);
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    if !min_x.is_finite() || !min_y.is_finite() || !max_x.is_finite() || !max_y.is_finite() {
        return Rect::ZERO;
    }
    Rect::new(
        min_x as f32,
        min_y as f32,
        (max_x - min_x).max(0.0) as f32,
        (max_y - min_y).max(0.0) as f32,
    )
}

#[derive(Clone, Copy)]
enum LayerFrame {
    Stacking { prev_spatial: SpatialNodeId },
    Clip { prev_spatial: SpatialNodeId },
}

fn to_kurbo_rect(rect: Rect) -> KurboRect {
    KurboRect::new(
        f64::from(rect.min_x()),
        f64::from(rect.min_y()),
        f64::from(rect.max_x()),
        f64::from(rect.max_y()),
    )
}

fn rounded_rect(rect: Rect, radii: [f32; 4]) -> RoundedRect {
    let [tl, tr, br, bl] = scale_corner_radii(
        f64::from(rect.size.width),
        f64::from(rect.size.height),
        radii,
    );
    RoundedRect::new(
        f64::from(rect.min_x()),
        f64::from(rect.min_y()),
        f64::from(rect.max_x()),
        f64::from(rect.max_y()),
        RoundedRectRadii::new(tl, tr, br, bl),
    )
}

fn fill_shape(
    scene: &mut Scene,
    transform: Affine,
    brush: vello::peniko::Color,
    bounds: Rect,
    radii: [f32; 4],
) {
    if radii.iter().any(|&r| r > 0.0) {
        let shape = rounded_rect(bounds, radii);
        scene.fill(Fill::NonZero, transform, brush, None, &shape);
    } else {
        let shape = to_kurbo_rect(bounds);
        scene.fill(Fill::NonZero, transform, brush, None, &shape);
    }
}

fn skip_balanced_stacking_subtree(items: &[DisplayItem], start: usize) -> usize {
    let mut depth = 1usize;
    let mut idx = start + 1;
    while idx < items.len() {
        match &items[idx] {
            DisplayItem::PushStackingContext { .. } => depth += 1,
            DisplayItem::PopStackingContext => {
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

fn skip_balanced_clip_subtree(items: &[DisplayItem], start: usize) -> usize {
    let mut depth = 1usize;
    let mut idx = start + 1;
    while idx < items.len() {
        match &items[idx] {
            DisplayItem::PushClip { .. } | DisplayItem::PushClipPath { .. } => depth += 1,
            DisplayItem::PopClip => {
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

pub fn encode_display_list(
    pipeline: PipelineId,
    list: &DisplayList,
    resources: &mut ResourceRegistry,
    options: &EncodeOptions,
    scene: &mut Scene,
) -> EncodeStats {
    if list.items.is_empty() {
        return EncodeStats::default();
    }
    let full = 0..list.items.len();
    encode_display_list_ranges(
        pipeline,
        list,
        std::slice::from_ref(&full),
        resources,
        options,
        scene,
    )
}

pub fn encode_display_list_ranges(
    pipeline: PipelineId,
    list: &DisplayList,
    ranges: &[Range<usize>],
    resources: &mut ResourceRegistry,
    options: &EncodeOptions,
    scene: &mut Scene,
) -> EncodeStats {
    if ranges.is_empty() || list.items.is_empty() {
        return EncodeStats::default();
    }
    let mut stats = EncodeStats::default();
    let mut spatial_transforms: Vec<Affine> = Vec::with_capacity(list.spatial_tree.nodes.len());
    for node in &list.spatial_tree.nodes {
        spatial_transforms.push(
            options.base_transform
                * list
                    .spatial_tree
                    .resolve_world_transform_with_override(node.id, options.override_scroll_zero),
        );
    }

    let lookup_transform = |id: SpatialNodeId| -> Affine {
        list.spatial_tree
            .nodes
            .get(id.0 as usize)
            .filter(|n| n.id == id)
            .and_then(|_| spatial_transforms.get(id.0 as usize).copied())
            .unwrap_or_else(|| {
                options.base_transform
                    * list
                        .spatial_tree
                        .resolve_world_transform_with_override(id, options.override_scroll_zero)
            })
    };

    let mut current_spatial = SpatialNodeId(0);
    let mut current_transform = lookup_transform(current_spatial);
    let mut layer_stack: SmallVec<[LayerFrame; 8]> = SmallVec::new();
    let mut world_clip_stack: SmallVec<[Rect; 8]> = SmallVec::new();
    let mut filter_stack: SmallVec<[ActiveFilterFrame; 8]> =
        smallvec::smallvec![ActiveFilterFrame::default()];
    if let Some(vp) = options.viewport_clip {
        world_clip_stack.push(vp);
    }

    let effective_world_cull = |clip_stack: &[Rect]| -> Option<Rect> {
        match (clip_stack.last().copied(), options.cull_rect) {
            (Some(a), Some(b)) => Some(a.intersect(&b).unwrap_or(Rect::ZERO)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    };

    let mut range_idx = 0usize;
    let mut idx = ranges[0].start;
    loop {
        while range_idx < ranges.len() {
            let r_start = ranges[range_idx].start;
            let r_end = ranges[range_idx].end.min(list.items.len());
            if idx < r_start {
                idx = r_start;
            }
            if idx < r_end {
                break;
            }
            range_idx += 1;
        }
        if range_idx >= ranges.len() || idx >= list.items.len() {
            break;
        }

        stats.items_visited += 1;
        match &list.items[idx] {
            DisplayItem::PushStackingContext {
                spatial,
                bounds,
                opacity,
                mix_blend,
                filters,
            } => {
                let next_transform = lookup_transform(*spatial);
                let mut effective_alpha = opacity.clamp(0.0, 1.0);
                let mut extra_blur_inflate = 0.0f32;
                for op in filters {
                    match *op {
                        FilterOp::Opacity(a) => effective_alpha *= a.clamp(0.0, 1.0),
                        FilterOp::AnimatedOpacity(anim_id) => {
                            if let Some(&ov) = options.opacity_overrides.get(&anim_id) {
                                effective_alpha = ov.clamp(0.0, 1.0);
                            }
                        }
                        FilterOp::Blur(r) => extra_blur_inflate += r.max(0.0) * 2.5,
                        FilterOp::DropShadow {
                            offset,
                            blur_radius,
                            ..
                        } => {
                            extra_blur_inflate = extra_blur_inflate.max(
                                blur_radius.max(0.0) * 2.5 + offset.x.abs().max(offset.y.abs()),
                            );
                        }
                        FilterOp::Brightness(_)
                        | FilterOp::Contrast(_)
                        | FilterOp::Grayscale(_)
                        | FilterOp::Saturate(_)
                        | FilterOp::Invert(_)
                        | FilterOp::Sepia(_)
                        | FilterOp::HueRotateDeg(_) => {}
                    }
                }
                let expanded_bounds = if extra_blur_inflate > 0.0 {
                    bounds.inflate(extra_blur_inflate)
                } else {
                    *bounds
                };
                let world_bounds = transform_aabb(expanded_bounds, next_transform);
                let clipped_world = match world_clip_stack.last() {
                    Some(c) => world_bounds.intersect(c).unwrap_or(Rect::ZERO),
                    None => world_bounds,
                };
                let layer_clip = match options.cull_rect {
                    Some(c) => clipped_world.intersect(&c).unwrap_or(Rect::ZERO),
                    None => clipped_world,
                };

                if bounds.is_empty() || layer_clip.is_empty() || effective_alpha <= 0.0 {
                    stats.subtrees_culled += 1;
                    idx = skip_balanced_stacking_subtree(&list.items, idx);
                    continue;
                }

                // Render CSS `filter: drop-shadow(...)` behind the stacking context via Vello GPU blur.
                let parent_filter = *filter_stack.last().unwrap_or(&ActiveFilterFrame::default());
                for op in filters {
                    if let FilterOp::DropShadow {
                        offset,
                        blur_radius,
                        color,
                    } = *op
                    {
                        if !color.is_transparent() {
                            let shadow_color = parent_filter.apply_color(color);
                            let shadow_rect = to_kurbo_rect(bounds.translate(offset));
                            let std_dev = (f64::from(blur_radius.max(0.0)) * 0.5).max(0.5);
                            scene.draw_blurred_rounded_rect(
                                next_transform,
                                shadow_rect,
                                shadow_color.to_vello_color(),
                                0.0,
                                std_dev,
                            );
                        }
                    }
                }

                // Always pass real finite bounds intersected with viewport/active clips (Spec 02 §2.2).
                let finite_layer_clip = to_kurbo_rect(layer_clip);
                scene.push_layer(
                    Fill::NonZero,
                    mix_blend.to_vello(),
                    effective_alpha,
                    Affine::IDENTITY,
                    &finite_layer_clip,
                );
                layer_stack.push(LayerFrame::Stacking {
                    prev_spatial: current_spatial,
                });
                world_clip_stack.push(clipped_world);
                filter_stack.push(ActiveFilterFrame::combine(parent_filter, filters));
                current_spatial = *spatial;
                current_transform = next_transform;
                stats.items_encoded += 1;
            }
            DisplayItem::PopStackingContext => {
                if let Some(LayerFrame::Stacking { prev_spatial }) = layer_stack.pop() {
                    scene.pop_layer();
                    world_clip_stack.pop();
                    if filter_stack.len() > 1 {
                        filter_stack.pop();
                    }
                    current_spatial = prev_spatial;
                    current_transform = lookup_transform(prev_spatial);
                    stats.items_encoded += 1;
                }
            }
            DisplayItem::PushClipPath { spatial, shape } => {
                let next_transform = lookup_transform(*spatial);
                let local_bb = shape.bounding_rect();
                let world_bounds = transform_aabb(local_bb, next_transform);
                let clipped_world = match world_clip_stack.last() {
                    Some(c) => world_bounds.intersect(c).unwrap_or(Rect::ZERO),
                    None => world_bounds,
                };
                let visible_after_cull = options
                    .cull_rect
                    .map(|c| clipped_world.intersects(&c))
                    .unwrap_or(true);

                if local_bb.is_empty() || clipped_world.is_empty() || !visible_after_cull {
                    stats.subtrees_culled += 1;
                    idx = skip_balanced_clip_subtree(&list.items, idx);
                    continue;
                }

                match shape {
                    ClipPathShape::Inset { rect, radii } => {
                        if radii.iter().any(|&r| r > 0.0) {
                            let clip_shape = rounded_rect(*rect, *radii);
                            scene.push_clip_layer(Fill::NonZero, next_transform, &clip_shape);
                        } else {
                            let clip_shape = to_kurbo_rect(*rect);
                            scene.push_clip_layer(Fill::NonZero, next_transform, &clip_shape);
                        }
                    }
                    ClipPathShape::Circle { center, radius } => {
                        let c = Circle::new(
                            (f64::from(center.x), f64::from(center.y)),
                            f64::from(radius.max(0.0)),
                        );
                        scene.push_clip_layer(Fill::NonZero, next_transform, &c);
                    }
                    ClipPathShape::Ellipse { center, rx, ry } => {
                        let e = Ellipse::new(
                            (f64::from(center.x), f64::from(center.y)),
                            (f64::from(rx.max(0.0)), f64::from(ry.max(0.0))),
                            0.0,
                        );
                        scene.push_clip_layer(Fill::NonZero, next_transform, &e);
                    }
                    ClipPathShape::Polygon { fill_odd, points } => {
                        let mut path = BezPath::new();
                        if let Some(first) = points.first() {
                            path.move_to((f64::from(first.x), f64::from(first.y)));
                            for p in points.iter().skip(1) {
                                path.line_to((f64::from(p.x), f64::from(p.y)));
                            }
                            path.close_path();
                        }
                        let fill = if *fill_odd {
                            Fill::EvenOdd
                        } else {
                            Fill::NonZero
                        };
                        scene.push_clip_layer(fill, next_transform, &path);
                    }
                }
                layer_stack.push(LayerFrame::Clip {
                    prev_spatial: current_spatial,
                });
                world_clip_stack.push(clipped_world);
                current_spatial = *spatial;
                current_transform = next_transform;
                stats.items_encoded += 1;
            }
            DisplayItem::PushClip {
                spatial,
                rect,
                radii,
            } => {
                let next_transform = lookup_transform(*spatial);
                let world_bounds = transform_aabb(*rect, next_transform);
                let clipped_world = match world_clip_stack.last() {
                    Some(c) => world_bounds.intersect(c).unwrap_or(Rect::ZERO),
                    None => world_bounds,
                };
                let visible_after_cull = options
                    .cull_rect
                    .map(|c| clipped_world.intersects(&c))
                    .unwrap_or(true);

                if rect.is_empty() || clipped_world.is_empty() || !visible_after_cull {
                    stats.subtrees_culled += 1;
                    idx = skip_balanced_clip_subtree(&list.items, idx);
                    continue;
                }

                if radii.iter().any(|&r| r > 0.0) {
                    let clip_shape = rounded_rect(*rect, *radii);
                    scene.push_clip_layer(Fill::NonZero, next_transform, &clip_shape);
                } else {
                    let clip_shape = to_kurbo_rect(*rect);
                    scene.push_clip_layer(Fill::NonZero, next_transform, &clip_shape);
                }
                layer_stack.push(LayerFrame::Clip {
                    prev_spatial: current_spatial,
                });
                world_clip_stack.push(clipped_world);
                current_spatial = *spatial;
                current_transform = next_transform;
                stats.items_encoded += 1;
            }
            DisplayItem::PopClip => {
                if let Some(LayerFrame::Clip { prev_spatial }) = layer_stack.pop() {
                    scene.pop_layer();
                    world_clip_stack.pop();
                    current_spatial = prev_spatial;
                    current_transform = lookup_transform(prev_spatial);
                    stats.items_encoded += 1;
                }
            }
            DisplayItem::Rect {
                bounds,
                color,
                radii,
            } => {
                if bounds.is_empty() || color.is_transparent() {
                    idx += 1;
                    continue;
                }
                let af = *filter_stack.last().unwrap_or(&ActiveFilterFrame::default());
                let cull_bounds = if af.blur_radius > 0.0 {
                    bounds.inflate(af.blur_radius * 2.5)
                } else {
                    *bounds
                };
                if is_culled(
                    cull_bounds,
                    current_transform,
                    effective_world_cull(&world_clip_stack),
                ) {
                    stats.items_culled += 1;
                    idx += 1;
                    continue;
                }
                let filtered = af.apply_color(*color);
                if af.blur_radius > 0.05 {
                    let avg_r = f64::from(radii.iter().copied().sum::<f32>() * 0.25);
                    scene.draw_blurred_rounded_rect(
                        current_transform,
                        to_kurbo_rect(*bounds),
                        filtered.to_vello_color(),
                        avg_r,
                        f64::from(af.blur_radius),
                    );
                } else {
                    fill_shape(
                        scene,
                        current_transform,
                        filtered.to_vello_color(),
                        *bounds,
                        *radii,
                    );
                }
                stats.items_encoded += 1;
            }
            DisplayItem::LinearGradient {
                bounds,
                start,
                end,
                stops,
                radii,
            } => {
                if bounds.is_empty() || stops.is_empty() {
                    idx += 1;
                    continue;
                }
                if is_culled(
                    *bounds,
                    current_transform,
                    effective_world_cull(&world_clip_stack),
                ) {
                    stats.items_culled += 1;
                    idx += 1;
                    continue;
                }
                let af = *filter_stack.last().unwrap_or(&ActiveFilterFrame::default());
                let peniko_stops: SmallVec<[(f32, vello::peniko::Color); 4]> = stops
                    .iter()
                    .map(|s| (s.offset, af.apply_color(s.color).to_vello_color()))
                    .collect();
                let brush = Gradient::new_linear(
                    (f64::from(start.x), f64::from(start.y)),
                    (f64::from(end.x), f64::from(end.y)),
                )
                .with_interpolation_alpha_space(InterpolationAlphaSpace::Premultiplied)
                .with_stops(peniko_stops.as_slice());
                if radii.iter().any(|&r| r > 0.0) {
                    let shape = rounded_rect(*bounds, *radii);
                    scene.fill(Fill::NonZero, current_transform, &brush, None, &shape);
                } else {
                    let shape = to_kurbo_rect(*bounds);
                    scene.fill(Fill::NonZero, current_transform, &brush, None, &shape);
                }
                stats.items_encoded += 1;
            }
            DisplayItem::RadialGradient {
                bounds,
                center,
                radius,
                stops,
                radii,
            } => {
                if bounds.is_empty() || stops.is_empty() || *radius <= 0.0 {
                    idx += 1;
                    continue;
                }
                if is_culled(
                    *bounds,
                    current_transform,
                    effective_world_cull(&world_clip_stack),
                ) {
                    stats.items_culled += 1;
                    idx += 1;
                    continue;
                }
                let af = *filter_stack.last().unwrap_or(&ActiveFilterFrame::default());
                let peniko_stops: SmallVec<[(f32, vello::peniko::Color); 4]> = stops
                    .iter()
                    .map(|s| (s.offset, af.apply_color(s.color).to_vello_color()))
                    .collect();
                let brush = Gradient::new_radial(
                    (f64::from(center.x), f64::from(center.y)),
                    radius.max(0.5),
                )
                .with_interpolation_alpha_space(InterpolationAlphaSpace::Premultiplied)
                .with_stops(peniko_stops.as_slice());
                if radii.iter().any(|&r| r > 0.0) {
                    let shape = rounded_rect(*bounds, *radii);
                    scene.fill(Fill::NonZero, current_transform, &brush, None, &shape);
                } else {
                    let shape = to_kurbo_rect(*bounds);
                    scene.fill(Fill::NonZero, current_transform, &brush, None, &shape);
                }
                stats.items_encoded += 1;
            }
            DisplayItem::Border {
                bounds,
                widths,
                colors,
                radii,
            } => {
                if bounds.is_empty() || widths.iter().all(|&w| w <= 0.0) {
                    idx += 1;
                    continue;
                }
                if is_culled(
                    *bounds,
                    current_transform,
                    effective_world_cull(&world_clip_stack),
                ) {
                    stats.items_culled += 1;
                    idx += 1;
                    continue;
                }
                let af = *filter_stack.last().unwrap_or(&ActiveFilterFrame::default());
                let filtered_colors = colors.map(|c| af.apply_color(c));
                encode_border(
                    scene,
                    current_transform,
                    *bounds,
                    *widths,
                    filtered_colors,
                    *radii,
                );
                stats.items_encoded += 1;
            }
            DisplayItem::BoxShadow {
                bounds,
                offset,
                blur_radius,
                spread_radius,
                color,
                radii,
                inset,
            } => {
                if bounds.is_empty() || color.is_transparent() {
                    idx += 1;
                    continue;
                }
                let margin = blur_radius.max(0.0) * 1.5 + spread_radius.abs();
                let shadow_bounds = bounds.translate(*offset).inflate(margin);
                if is_culled(
                    shadow_bounds,
                    current_transform,
                    effective_world_cull(&world_clip_stack),
                ) {
                    stats.items_culled += 1;
                    idx += 1;
                    continue;
                }
                let af = *filter_stack.last().unwrap_or(&ActiveFilterFrame::default());
                encode_box_shadow(
                    scene,
                    current_transform,
                    *bounds,
                    *offset,
                    *blur_radius,
                    *spread_radius,
                    af.apply_color(*color),
                    *radii,
                    *inset,
                );
                stats.items_encoded += 1;
            }
            DisplayItem::GlyphRun {
                bounds,
                font_id,
                font_size,
                color,
                glyphs,
            } => {
                if *font_size <= 0.0 || color.is_transparent() || glyphs.is_empty() {
                    idx += 1;
                    continue;
                }
                if is_culled(
                    *bounds,
                    current_transform,
                    effective_world_cull(&world_clip_stack),
                ) {
                    stats.items_culled += 1;
                    idx += 1;
                    continue;
                }
                let start = glyphs.start as usize;
                let end = (glyphs.end as usize).min(list.glyphs.len());
                if start >= end {
                    idx += 1;
                    continue;
                }
                let af = *filter_stack.last().unwrap_or(&ActiveFilterFrame::default());
                let glyph_color = af.apply_color(*color);
                if let Some(font) = resources.get_font(*font_id) {
                    scene
                        .draw_glyphs(&font.font)
                        .font_size(*font_size)
                        .normalized_coords(&font.normalized_coords)
                        .hint(true)
                        .transform(current_transform)
                        .brush(glyph_color.to_vello_color())
                        .draw(
                            Fill::NonZero,
                            list.glyphs[start..end].iter().map(|g| Glyph {
                                id: g.glyph_id,
                                x: g.x,
                                y: g.y,
                            }),
                        );
                    stats.items_encoded += 1;
                }
            }
            DisplayItem::Image {
                bounds,
                image_id,
                sampling,
                radii,
            } => {
                if bounds.is_empty() {
                    idx += 1;
                    continue;
                }
                if is_culled(
                    *bounds,
                    current_transform,
                    effective_world_cull(&world_clip_stack),
                ) {
                    stats.items_culled += 1;
                    idx += 1;
                    continue;
                }
                if let Some(image_data) = resources.get_image(pipeline, *image_id) {
                    draw_image_into_bounds(
                        scene,
                        current_transform,
                        *bounds,
                        image_data,
                        *sampling,
                        *radii,
                    );
                    stats.items_encoded += 1;
                }
            }
            DisplayItem::VectorPath {
                bounds,
                path_id,
                fill,
                stroke,
            } => {
                if bounds.is_empty() {
                    idx += 1;
                    continue;
                }
                if is_culled(
                    *bounds,
                    current_transform,
                    effective_world_cull(&world_clip_stack),
                ) {
                    stats.items_culled += 1;
                    idx += 1;
                    continue;
                }
                if let Some(path) = resources.get_path(*path_id) {
                    let path_transform = current_transform
                        * Affine::translate((f64::from(bounds.min_x()), f64::from(bounds.min_y())));
                    if let Some(fill_color) = fill {
                        scene.fill(
                            Fill::NonZero,
                            path_transform,
                            fill_color.to_vello_color(),
                            None,
                            path,
                        );
                    }
                    if let Some((stroke_color, stroke_w)) = stroke {
                        if *stroke_w > 0.0 {
                            let st = Stroke::new(f64::from(*stroke_w));
                            scene.stroke(
                                &st,
                                path_transform,
                                stroke_color.to_vello_color(),
                                None,
                                path,
                            );
                        }
                    }
                    stats.items_encoded += 1;
                }
            }
            DisplayItem::ExternalSurface {
                bounds,
                surface_id,
                radii,
            } => {
                if bounds.is_empty() {
                    idx += 1;
                    continue;
                }
                if is_culled(
                    *bounds,
                    current_transform,
                    effective_world_cull(&world_clip_stack),
                ) {
                    stats.items_culled += 1;
                    idx += 1;
                    continue;
                }
                if let Some(image_data) = resources.get_external_surface(*surface_id) {
                    draw_image_into_bounds(
                        scene,
                        current_transform,
                        *bounds,
                        image_data,
                        crate::display_list::ImageSampling::Bilinear,
                        *radii,
                    );
                    stats.items_encoded += 1;
                }
            }
            DisplayItem::BackdropFilter { .. } => {
                // Executed as a GPU-to-GPU WGSL pass between pre/post scenes in `VelloRenderer`.
            }
        }
        idx += 1;
    }

    // Balance any truncated layer frames cleanly.
    while layer_stack.pop().is_some() {
        scene.pop_layer();
    }

    stats
}

#[inline]
fn is_culled(local_bounds: Rect, transform: Affine, cull: Option<Rect>) -> bool {
    let Some(visible) = cull else {
        return false;
    };
    !transform_aabb(local_bounds, transform).intersects(&visible)
}

fn draw_image_into_bounds(
    scene: &mut Scene,
    transform: Affine,
    bounds: Rect,
    image: vello::peniko::ImageData,
    sampling: crate::display_list::ImageSampling,
    radii: [f32; 4],
) {
    if image.width == 0 || image.height == 0 {
        return;
    }
    let sx = f64::from(bounds.size.width) / f64::from(image.width);
    let sy = f64::from(bounds.size.height) / f64::from(image.height);
    let img_transform = transform
        * Affine::translate((f64::from(bounds.min_x()), f64::from(bounds.min_y())))
        * Affine::scale_non_uniform(sx, sy);
    let brush = ImageBrush::new(image).with_quality(sampling.to_vello_quality());
    if radii.iter().any(|&r| r > 0.0) {
        let clip_shape = rounded_rect(bounds, radii);
        scene.push_clip_layer(Fill::NonZero, transform, &clip_shape);
        scene.draw_image(&brush, img_transform);
        scene.pop_layer();
    } else {
        scene.draw_image(&brush, img_transform);
    }
}

fn encode_border(
    scene: &mut Scene,
    transform: Affine,
    bounds: Rect,
    widths: [f32; 4],
    colors: [Rgba8; 4],
    radii: [f32; 4],
) {
    let uniform_color = colors.iter().all(|&c| c == colors[0]);
    if uniform_color && !colors[0].is_transparent() {
        let color = colors[0].to_vello_color();
        let [tl, tr, br, bl] = scale_corner_radii(
            f64::from(bounds.size.width),
            f64::from(bounds.size.height),
            radii,
        );
        let x0 = f64::from(bounds.min_x());
        let y0 = f64::from(bounds.min_y());
        let x1 = f64::from(bounds.max_x());
        let y1 = f64::from(bounds.max_y());
        let outer = RoundedRect::new(x0, y0, x1, y1, RoundedRectRadii::new(tl, tr, br, bl));

        let top_w = f64::from(widths[0].max(0.0));
        let right_w = f64::from(widths[1].max(0.0));
        let bottom_w = f64::from(widths[2].max(0.0));
        let left_w = f64::from(widths[3].max(0.0));

        let in_x0 = (x0 + left_w).min(x1);
        let in_y0 = (y0 + top_w).min(y1);
        let in_x1 = (x1 - right_w).max(in_x0);
        let in_y1 = (y1 - bottom_w).max(in_y0);
        let inner = RoundedRect::new(
            in_x0,
            in_y0,
            in_x1,
            in_y1,
            RoundedRectRadii::new(
                (tl - top_w.max(left_w)).max(0.0),
                (tr - top_w.max(right_w)).max(0.0),
                (br - bottom_w.max(right_w)).max(0.0),
                (bl - bottom_w.max(left_w)).max(0.0),
            ),
        );
        let mut ring = outer.to_path(0.1);
        ring.extend(inner.path_elements(0.1));
        scene.fill(Fill::EvenOdd, transform, color, None, &ring);
        return;
    }

    // Per-side edge quads when border colors differ.
    let [top, right, bottom, left] = widths.map(|w| w.max(0.0));
    let x0 = bounds.min_x();
    let y0 = bounds.min_y();
    let w = bounds.size.width;
    let h = bounds.size.height;
    let sides = [
        (Rect::new(x0, y0, w, top.min(h)), colors[0]),
        (
            Rect::new((x0 + w - right).max(x0), y0, right.min(w), h),
            colors[1],
        ),
        (
            Rect::new(x0, (y0 + h - bottom).max(y0), w, bottom.min(h)),
            colors[2],
        ),
        (Rect::new(x0, y0, left.min(w), h), colors[3]),
    ];
    for (edge_rect, color) in sides {
        if !edge_rect.is_empty() && !color.is_transparent() {
            scene.fill(
                Fill::NonZero,
                transform,
                color.to_vello_color(),
                None,
                &to_kurbo_rect(edge_rect),
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_box_shadow(
    scene: &mut Scene,
    transform: Affine,
    bounds: Rect,
    offset: rix_core::Vec2,
    blur_radius: f32,
    spread_radius: f32,
    color: Rgba8,
    radii: [f32; 4],
    inset: bool,
) {
    let vello_color = color.to_vello_color();
    if inset {
        let clip_shape = rounded_rect(bounds, radii);
        scene.push_clip_layer(Fill::NonZero, transform, &clip_shape);
        let inner = Rect::new(
            bounds.min_x() + offset.x + spread_radius,
            bounds.min_y() + offset.y + spread_radius,
            (bounds.size.width - spread_radius * 2.0).max(0.0),
            (bounds.size.height - spread_radius * 2.0).max(0.0),
        );
        let pad = blur_radius.max(0.0) + spread_radius.abs() + 2.0;
        let outer = bounds.inflate(pad);
        let mut ring = to_kurbo_rect(outer).to_path(0.1);
        ring.extend(rounded_rect(inner, radii).path_elements(0.1));
        scene.fill(Fill::EvenOdd, transform, vello_color, None, &ring);
        scene.pop_layer();
        return;
    }

    let sx = bounds.min_x() + offset.x - spread_radius;
    let sy = bounds.min_y() + offset.y - spread_radius;
    let sw = (bounds.size.width + spread_radius * 2.0).max(1.0);
    let sh = (bounds.size.height + spread_radius * 2.0).max(1.0);
    let avg_radius = radii.iter().copied().sum::<f32>() * 0.25;
    let shadow_radius =
        f64::from((avg_radius + spread_radius).clamp(0.0, (sw.min(sh) * 0.5).max(0.0)));
    let std_dev = f64::from(blur_radius.max(0.0)) * 0.5;
    let shadow_rect = KurboRect::new(
        f64::from(sx),
        f64::from(sy),
        f64::from(sx + sw),
        f64::from(sy + sh),
    );
    // CSS Backgrounds L3 §7.1.2: An outset box-shadow is clipped out of the element's
    // border-box so transparent elements never fill their interior with shadow color.
    let blur_pad = blur_radius.max(0.0) * 3.0 + 4.0;
    let clip_min_x = bounds.min_x().min(sx - blur_pad) - 2.0;
    let clip_min_y = bounds.min_y().min(sy - blur_pad) - 2.0;
    let clip_max_x = bounds.max_x().max(sx + sw + blur_pad) + 2.0;
    let clip_max_y = bounds.max_y().max(sy + sh + blur_pad) + 2.0;
    let outer_clip = KurboRect::new(
        f64::from(clip_min_x),
        f64::from(clip_min_y),
        f64::from(clip_max_x),
        f64::from(clip_max_y),
    );
    let mut cutout = outer_clip.to_path(0.1);
    cutout.extend(rounded_rect(bounds, radii).path_elements(0.1));
    scene.push_clip_layer(Fill::EvenOdd, transform, &cutout);
    if std_dev > 0.01 {
        scene.draw_blurred_rounded_rect(
            transform,
            shadow_rect,
            vello_color,
            shadow_radius,
            std_dev,
        );
    } else {
        let adjusted_radii = radii.map(|r| (r + spread_radius).max(0.0));
        fill_shape(
            scene,
            transform,
            vello_color,
            Rect::new(sx, sy, sw, sh),
            adjusted_radii,
        );
    }
    scene.pop_layer();
}
