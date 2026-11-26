//! CSS 2.1 Appendix E stacking-context traversal and hierarchical binary
//! `DisplayList` + `SpatialTree` painter for RixBrowser.
//!
//! Painting order strictly follows CSS 2.1 Appendix E within every stacking context:
//! 1. Background, box-shadow, and borders of the stacking-context root.
//! 2. Child stacking contexts with negative `z-index` (sorted ascending).
//! 3. In-flow non-inline-level non-positioned descendants (blocks, tables).
//! 4. Non-positioned floats.
//! 5. In-flow inline-level non-positioned descendants (line boxes, split inline boxes,
//!    glyph runs, atomic inlines).
//! 6. Positioned descendants with `z-index: auto | 0` and non-positioned stacking contexts.
//! 7. Child stacking contexts with positive `z-index` (sorted ascending).

use std::collections::HashSet;

use rix_core::{Point, Rect, Size, Vec2};
use rix_css::{
    AnimationDirectionMode, AnimationFillModeKind, AnimationPlayStateMode, BackgroundClipMode,
    BackgroundImageValue, BackgroundSizeValue, ClipPathValue, ClipRadius, ColorRgba, ComputedStyle,
    CssGradientStop, DisplayMode, EvalContext, FilterOp as CssFilterOp, KeyframesRule,
    LengthPercentage, LengthPercentageAuto, LineStyle, ListStylePositionMode, ListStyleTypeMode,
    MixBlendMode, OverflowMode, PositionMode, PropertyIdKind, PropertyValue, TextDecorationLine,
    TimingFunction, TransformOp, VisibilityMode, ZIndexValue,
};
use rix_ipc::{FontResourceId, ImageId, PathResourceId, PipelineId, ScrollId, SpatialNodeId};
use rix_render::animations::{
    AnimDirection, AnimFillMode, AnimationTarget, CompositorAnimation, OpacityKeyframe,
    TimingCurve, TransformKeyframe,
};
use rix_render::display_list::{
    AnimationId, BlendMode, ClipPathShape, ColorStop, DisplayItem, DisplayList,
    FilterOp as RenderFilterOp, GlyphInstance, ImageSampling, Rgba8, SpatialNodeKind,
    StickyMargins,
};
use rix_render::resources::parse_svg_path;
use rix_render::ResourceRegistry;
use rix_text::{FontDatabase, FontId};
use smallvec::SmallVec;
use vello::kurbo::Affine;

use crate::assets::AssetRegistry;
use crate::box_tree::FormControlKind;
use crate::fragment::{Fragment, FragmentKind, FragmentTree};

/// Paints a [`FragmentTree`] into a hierarchical [`DisplayList`] in CSS 2.1 Appendix E
/// stacking-context order while registering required fonts, decoded images, and SVG paths
/// in `resources`.
pub fn paint_fragment_tree(
    tree: &FragmentTree,
    font_db: &FontDatabase,
    assets: &AssetRegistry,
    pipeline: PipelineId,
    resources: &mut ResourceRegistry,
    dark_mode: bool,
) -> DisplayList {
    paint_fragment_tree_with_keyframes(
        tree,
        font_db,
        assets,
        &[],
        tree.content_bounds.size,
        pipeline,
        resources,
        dark_mode,
    )
}

/// Paints a [`FragmentTree`] with `@keyframes` compositor animation extraction and
/// viewport-aware root [`SpatialNodeKind::ScrollFrame`] creation for `position: sticky`.
#[allow(clippy::too_many_arguments)]
pub fn paint_fragment_tree_with_keyframes(
    tree: &FragmentTree,
    font_db: &FontDatabase,
    assets: &AssetRegistry,
    keyframes: &[&KeyframesRule],
    viewport: Size,
    pipeline: PipelineId,
    resources: &mut ResourceRegistry,
    dark_mode: bool,
) -> DisplayList {
    let mut list = DisplayList::new(tree.content_bounds);
    let Some(root) = &tree.root else {
        return list;
    };

    let needs_root_scroll = tree.content_bounds.size.height > viewport.height + 1.0
        || tree.content_bounds.size.width > viewport.width + 1.0
        || fragment_has_sticky_descendant(root);
    let (root_spatial, root_scroll_parent, next_scroll_id) = if needs_root_scroll {
        let vp_w = if viewport.width > 0.0 {
            viewport.width
        } else {
            tree.content_bounds.size.width
        };
        let vp_h = if viewport.height > 0.0 {
            viewport.height
        } else {
            tree.content_bounds.size.height
        };
        let scroll_node = list.spatial_tree.add_node(
            Some(SpatialNodeId(0)),
            SpatialNodeKind::ScrollFrame {
                external_scroll_id: ScrollId(1),
                content_rect: Rect::new(
                    0.0,
                    0.0,
                    tree.content_bounds.size.width.max(vp_w),
                    tree.content_bounds.size.height.max(vp_h),
                ),
                viewport_rect: Rect::new(0.0, 0.0, vp_w, vp_h),
                current_offset: Vec2::ZERO,
            },
        );
        (scroll_node, Some(scroll_node), 2)
    } else {
        (SpatialNodeId(0), None, 1)
    };

    if needs_root_scroll {
        list.push(DisplayItem::PushStackingContext {
            spatial: root_spatial,
            bounds: Rect::new(
                0.0,
                0.0,
                tree.content_bounds.size.width.max(viewport.width),
                tree.content_bounds.size.height.max(viewport.height),
            ),
            opacity: 1.0,
            mix_blend: BlendMode::Normal,
            filters: SmallVec::new(),
        });
    }

    let mut painter = Painter {
        list: &mut list,
        font_db,
        assets,
        keyframes,
        pipeline,
        resources,
        dark_mode,
        registered_fonts: Vec::new(),
        registered_images: HashSet::new(),
        next_scroll_id,
        next_animation_id: 1,
    };

    painter.paint_stacking_context(
        root,
        Point::ZERO,
        tree.content_bounds,
        root_spatial,
        root_scroll_parent,
    );
    if needs_root_scroll {
        list.push(DisplayItem::PopStackingContext);
    }
    list
}

fn fragment_has_sticky_descendant(frag: &Fragment) -> bool {
    if frag.style.position == PositionMode::Sticky {
        return true;
    }
    frag.children.iter().any(fragment_has_sticky_descendant)
}

struct Painter<'a> {
    list: &'a mut DisplayList,
    font_db: &'a FontDatabase,
    assets: &'a AssetRegistry,
    keyframes: &'a [&'a KeyframesRule],
    pipeline: PipelineId,
    resources: &'a mut ResourceRegistry,
    dark_mode: bool,
    registered_fonts: Vec<(FontId, FontResourceId)>,
    registered_images: HashSet<ImageId>,
    next_scroll_id: u64,
    next_animation_id: u64,
}

#[derive(Clone, Copy)]
struct CollectedStackingChild<'f> {
    frag: &'f Fragment,
    parent_origin: Point,
    container_rect: Rect,
    z_index: i32,
    order: usize,
}

impl<'a> Painter<'a> {
    fn register_font(&mut self, font_id: FontId) -> Option<FontResourceId> {
        if let Some(&(_, res_id)) = self.registered_fonts.iter().find(|(id, _)| *id == font_id) {
            return Some(res_id);
        }
        let font = self.font_db.get_font(font_id)?;
        let res_id = self.resources.register_rix_font(font);
        self.registered_fonts.push((font_id, res_id));
        Some(res_id)
    }

    fn ensure_image_registered(&mut self, image_id: ImageId) -> bool {
        if self.registered_images.contains(&image_id) {
            return true;
        }
        let Some(decoded) = self.assets.get_by_id(image_id) else {
            return false;
        };
        if self
            .resources
            .register_rgba8(
                self.pipeline,
                image_id,
                decoded.width,
                decoded.height,
                (*decoded.rgba).clone(),
            )
            .is_ok()
        {
            self.registered_images.insert(image_id);
            true
        } else {
            false
        }
    }

    fn paint_stacking_context<'f>(
        &mut self,
        frag: &'f Fragment,
        parent_origin: Point,
        container_rect: Rect,
        parent_spatial: SpatialNodeId,
        scroll_parent: Option<SpatialNodeId>,
    ) {
        let abs_rect = frag.absolute_border_rect(parent_origin);
        let style = &frag.style;

        let mut active_spatial = parent_spatial;
        let mut active_scroll_parent = scroll_parent;
        let mut pushed_stacking_layer = false;
        let mut pushed_clip_path = false;
        let mut pushed_clip = false;

        // 1. Sticky spatial node (`position: sticky`) with true containing-block travel bounds.
        if style.position == PositionMode::Sticky {
            if let Some(sp) = active_scroll_parent {
                let eval = EvalContext::default();
                let inset = &style.box_model.inset;
                let margins = StickyMargins {
                    top: match &inset.top {
                        LengthPercentageAuto::Value(lp) => {
                            Some(lp.resolve_px(container_rect.size.height, &eval))
                        }
                        LengthPercentageAuto::Auto => None,
                    },
                    bottom: match &inset.bottom {
                        LengthPercentageAuto::Value(lp) => {
                            Some(lp.resolve_px(container_rect.size.height, &eval))
                        }
                        LengthPercentageAuto::Auto => None,
                    },
                    left: match &inset.left {
                        LengthPercentageAuto::Value(lp) => {
                            Some(lp.resolve_px(container_rect.size.width, &eval))
                        }
                        LengthPercentageAuto::Auto => None,
                    },
                    right: match &inset.right {
                        LengthPercentageAuto::Value(lp) => {
                            Some(lp.resolve_px(container_rect.size.width, &eval))
                        }
                        LengthPercentageAuto::Auto => None,
                    },
                };
                let max_travel_x = (container_rect.max_x() - abs_rect.max_x()).max(0.0);
                let max_travel_y = (container_rect.max_y() - abs_rect.max_y()).max(0.0);
                active_spatial = self.list.spatial_tree.add_node(
                    Some(active_spatial),
                    SpatialNodeKind::StickyFrame {
                        bounds: Rect::new(
                            abs_rect.min_x(),
                            abs_rect.min_y(),
                            max_travel_x,
                            max_travel_y,
                        ),
                        margins,
                        scroll_parent: sp,
                    },
                );
            }
        }

        // 2. Transform spatial node (`transform: ...` + `transform-origin: ...` + `@keyframes` transform tracks)
        let mut animated_opacity_ids: SmallVec<[AnimationId; 2]> = SmallVec::new();
        let mut has_transform_anim = false;
        if let Some(effects) = &style.effects {
            let eval = EvalContext::default();
            let base_matrix = if effects.transform.is_empty() {
                [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]
            } else {
                compose_transform_matrix(&effects.transform, abs_rect)
            };
            let origin = Point::new(
                abs_rect.min_x()
                    + effects
                        .transform_origin
                        .x
                        .resolve_px(abs_rect.size.width, &eval),
                abs_rect.min_y()
                    + effects
                        .transform_origin
                        .y
                        .resolve_px(abs_rect.size.height, &eval),
            );

            // Check if any CSS `@keyframes` animation on this element animates `transform` or `opacity`.
            let mut transform_anim_tracks: Vec<(AnimationId, CompositorAnimation)> = Vec::new();
            for spec in &effects.animations {
                if spec.name.as_str().eq_ignore_ascii_case("none") {
                    continue;
                }
                let Some(rule) = self
                    .keyframes
                    .iter()
                    .rev()
                    .find(|k| k.name.as_str() == spec.name.as_str())
                else {
                    continue;
                };
                let tf_kfs = extract_transform_keyframes(rule, abs_rect, base_matrix);
                if !tf_kfs.is_empty() {
                    let id = AnimationId(self.next_animation_id);
                    self.next_animation_id += 1;
                    transform_anim_tracks.push((
                        id,
                        CompositorAnimation {
                            id,
                            start_time_ms: None,
                            duration_ms: spec.duration_ms,
                            delay_ms: spec.delay_ms,
                            iteration_count: spec.iteration_count,
                            direction: to_anim_direction(spec.direction),
                            fill_mode: to_anim_fill(spec.fill_mode),
                            paused: spec.play_state == AnimationPlayStateMode::Paused,
                            timing: to_timing_curve(&spec.timing),
                            target: AnimationTarget::Transform {
                                spatial: SpatialNodeId(0), // patched right after node allocation
                                keyframes: tf_kfs,
                            },
                        },
                    ));
                }
                let op_kfs = extract_opacity_keyframes(rule, style.opacity);
                if !op_kfs.is_empty() {
                    let id = AnimationId(self.next_animation_id);
                    self.next_animation_id += 1;
                    animated_opacity_ids.push(id);
                    self.list.animations.push(CompositorAnimation {
                        id,
                        start_time_ms: None,
                        duration_ms: spec.duration_ms,
                        delay_ms: spec.delay_ms,
                        iteration_count: spec.iteration_count,
                        direction: to_anim_direction(spec.direction),
                        fill_mode: to_anim_fill(spec.fill_mode),
                        paused: spec.play_state == AnimationPlayStateMode::Paused,
                        timing: to_timing_curve(&spec.timing),
                        target: AnimationTarget::Opacity { keyframes: op_kfs },
                    });
                }
            }

            if !effects.transform.is_empty() || !transform_anim_tracks.is_empty() {
                has_transform_anim = !transform_anim_tracks.is_empty();
                let first_anim_id = transform_anim_tracks.first().map(|(id, _)| *id);
                active_spatial = self.list.spatial_tree.add_node(
                    Some(active_spatial),
                    SpatialNodeKind::Transform {
                        matrix: base_matrix,
                        origin,
                        animation_id: first_anim_id,
                    },
                );
                for (_, mut track) in transform_anim_tracks {
                    if let AnimationTarget::Transform { spatial, .. } = &mut track.target {
                        *spatial = active_spatial;
                    }
                    self.list.animations.push(track);
                }
            }
        }

        // 3. Finite stacking-context layer (`opacity < 1`, `filter`, `backdrop-filter`, `mix-blend-mode`, animation, or new spatial node).
        let mut filters = extract_render_filters(style, abs_rect, self.dark_mode);
        let backdrop_filters = extract_render_backdrop_filters(style, abs_rect, self.dark_mode);
        for anim_id in animated_opacity_ids {
            filters.push(RenderFilterOp::AnimatedOpacity(anim_id));
        }
        let mix_blend = style.effects.as_ref().map_or(BlendMode::Normal, |e| {
            to_render_blend_mode(e.mix_blend_mode)
        });
        let needs_layer = style.opacity < 0.999
            || !filters.is_empty()
            || !backdrop_filters.is_empty()
            || mix_blend != BlendMode::Normal
            || has_transform_anim
            || active_spatial != parent_spatial;
        if needs_layer {
            let finite_bounds = Rect::new(
                abs_rect.origin.x + frag.overflow_rect.origin.x,
                abs_rect.origin.y + frag.overflow_rect.origin.y,
                frag.overflow_rect.size.width.max(abs_rect.size.width),
                frag.overflow_rect.size.height.max(abs_rect.size.height),
            );
            if !finite_bounds.is_empty() {
                self.list.push(DisplayItem::PushStackingContext {
                    spatial: active_spatial,
                    bounds: finite_bounds,
                    opacity: style.opacity.clamp(0.0, 1.0),
                    mix_blend,
                    filters,
                });
                pushed_stacking_layer = true;
            }
        }

        // 3b. CSS `clip-path` and CSS 2.1 `clip: rect(...)` (clips background, border, AND descendants).
        if let Some(effects) = &style.effects {
            if let Some(shape) = resolve_clip_path(&effects.clip_path, abs_rect) {
                self.list.push(DisplayItem::PushClipPath {
                    spatial: active_spatial,
                    shape,
                });
                pushed_clip_path = true;
            }
            if matches!(style.position, PositionMode::Absolute | PositionMode::Fixed) {
                if let Some(cr) = &effects.clip_rect {
                    let eval = EvalContext {
                        font_size_px: style.text.font_size_px,
                        ..EvalContext::default()
                    };
                    let top = cr
                        .top
                        .as_ref()
                        .map_or(0.0, |v| v.resolve_px(abs_rect.size.height, &eval));
                    let left = cr
                        .left
                        .as_ref()
                        .map_or(0.0, |v| v.resolve_px(abs_rect.size.width, &eval));
                    let right = cr.right.as_ref().map_or(abs_rect.size.width, |v| {
                        v.resolve_px(abs_rect.size.width, &eval)
                    });
                    let bottom = cr.bottom.as_ref().map_or(abs_rect.size.height, |v| {
                        v.resolve_px(abs_rect.size.height, &eval)
                    });
                    let clip_r = Rect::new(
                        abs_rect.min_x() + left,
                        abs_rect.min_y() + top,
                        (right - left).max(0.0),
                        (bottom - top).max(0.0),
                    );
                    self.list.push(DisplayItem::PushClip {
                        spatial: active_spatial,
                        rect: clip_r,
                        radii: [0.0; 4],
                    });
                    pushed_clip = true;
                }
            }
        }

        // 3c. W3C Filter Effects Level 2 `backdrop-filter` (filters rendered backdrop behind border box).
        if !backdrop_filters.is_empty() && !abs_rect.is_empty() {
            self.list.push(DisplayItem::BackdropFilter {
                bounds: abs_rect,
                radii: resolve_radii(style, abs_rect),
                filters: backdrop_filters,
            });
        }

        // Step 1 (Appendix E): Paint background, shadows, and borders of this root.
        self.paint_box_decorations(frag, abs_rect, active_spatial);

        // 4. Overflow clip / ScrollFrame for children.
        let clips_x = style.overflow.x != OverflowMode::Visible;
        let clips_y = style.overflow.y != OverflowMode::Visible;
        let is_scroll_container =
            matches!(style.overflow.x, OverflowMode::Scroll | OverflowMode::Auto)
                || matches!(style.overflow.y, OverflowMode::Scroll | OverflowMode::Auto);
        let mut pushed_overflow_clip = false;
        if clips_x || clips_y {
            let clip_rect = Rect::new(
                abs_rect.min_x() + frag.border.left,
                abs_rect.min_y() + frag.border.top,
                (abs_rect.size.width - frag.border.horizontal()).max(0.0),
                (abs_rect.size.height - frag.border.vertical()).max(0.0),
            );
            if is_scroll_container {
                let scroll_id = ScrollId(self.next_scroll_id);
                self.next_scroll_id += 1;
                let scroll_spatial = self.list.spatial_tree.add_node(
                    Some(active_spatial),
                    SpatialNodeKind::ScrollFrame {
                        external_scroll_id: scroll_id,
                        content_rect: Rect::new(
                            clip_rect.min_x(),
                            clip_rect.min_y(),
                            frag.overflow_rect.size.width.max(clip_rect.size.width),
                            frag.overflow_rect.size.height.max(clip_rect.size.height),
                        ),
                        viewport_rect: clip_rect,
                        current_offset: Vec2::ZERO,
                    },
                );
                active_spatial = scroll_spatial;
                active_scroll_parent = Some(scroll_spatial);
            }
            let radii = resolve_radii(style, clip_rect);
            self.list.push(DisplayItem::PushClip {
                spatial: active_spatial,
                rect: clip_rect,
                radii,
            });
            pushed_overflow_clip = true;
        }

        // Paint intrinsic replaced content (Image, SVG, FormControl, TextRun) of this fragment itself.
        self.paint_leaf_content(frag, abs_rect);

        // Collect descendant stacking contexts belonging to THIS stacking context.
        let mut neg_z: Vec<CollectedStackingChild<'f>> = Vec::new();
        let mut zero_z: Vec<CollectedStackingChild<'f>> = Vec::new();
        let mut pos_z: Vec<CollectedStackingChild<'f>> = Vec::new();
        let mut order_counter = 0usize;
        collect_stacking_children(
            frag,
            abs_rect.origin,
            abs_rect,
            &mut neg_z,
            &mut zero_z,
            &mut pos_z,
            &mut order_counter,
        );

        // Step 2 (Appendix E): Child stacking contexts with negative z-index (ascending,
        // tied by pre-order DOM tree order via `box_id` so out-of-flow `::before` children
        // paint strictly in document order rather than OOF append order).
        neg_z.sort_by_key(|c| (c.z_index, c.frag.box_id.0, c.order));
        for child in neg_z {
            self.paint_stacking_context(
                child.frag,
                child.parent_origin,
                child.container_rect,
                active_spatial,
                active_scroll_parent,
            );
        }

        // Step 3 (Appendix E): In-flow non-inline-level non-positioned descendants (blocks, tables).
        self.paint_flow_phase(
            frag,
            abs_rect.origin,
            active_spatial,
            active_scroll_parent,
            FlowPhase::Blocks,
        );

        // Step 4 (Appendix E): Non-positioned floats.
        self.paint_flow_phase(
            frag,
            abs_rect.origin,
            active_spatial,
            active_scroll_parent,
            FlowPhase::Floats,
        );

        // Step 5 (Appendix E): In-flow inline-level descendants (line boxes, text, inline boxes, atomic inlines).
        self.paint_flow_phase(
            frag,
            abs_rect.origin,
            active_spatial,
            active_scroll_parent,
            FlowPhase::Inlines,
        );

        // Step 6 (Appendix E): Positioned z-index: auto | 0 and non-positioned stacking contexts
        // in pre-order DOM tree order (so `::before { position: absolute }` stays behind sibling
        // `position: relative` content).
        zero_z.sort_by_key(|c| (c.frag.box_id.0, c.order));
        for child in zero_z {
            self.paint_stacking_context(
                child.frag,
                child.parent_origin,
                child.container_rect,
                active_spatial,
                active_scroll_parent,
            );
        }

        // Step 7 (Appendix E): Child stacking contexts with positive z-index (ascending).
        pos_z.sort_by_key(|c| (c.z_index, c.frag.box_id.0, c.order));
        for child in pos_z {
            self.paint_stacking_context(
                child.frag,
                child.parent_origin,
                child.container_rect,
                active_spatial,
                active_scroll_parent,
            );
        }

        if pushed_overflow_clip {
            self.list.push(DisplayItem::PopClip);
        }
        if pushed_clip {
            self.list.push(DisplayItem::PopClip);
        }
        if pushed_clip_path {
            self.list.push(DisplayItem::PopClip);
        }
        if pushed_stacking_layer {
            self.list.push(DisplayItem::PopStackingContext);
        }
    }

    fn paint_flow_phase(
        &mut self,
        parent: &Fragment,
        parent_origin: Point,
        spatial: SpatialNodeId,
        scroll_parent: Option<SpatialNodeId>,
        phase: FlowPhase,
    ) {
        let mut list_ordinal: usize = 0;
        for child in parent.children.iter() {
            let is_list_item = matches!(
                child.style.display,
                DisplayMode::ListItem | DisplayMode::FlowRootListItem
            );
            if is_list_item {
                list_ordinal += 1;
            }
            // Skip child stacking contexts & positioned elements (handled in Steps 2, 6, 7).
            if creates_stacking_context(child) || is_positioned(child) {
                continue;
            }
            let child_abs = child.absolute_border_rect(parent_origin);
            match phase {
                FlowPhase::Blocks => {
                    if child.is_float || is_inline_level_fragment(child) {
                        continue;
                    }
                    self.paint_non_stacking_subtree(
                        child,
                        child_abs,
                        spatial,
                        scroll_parent,
                        FlowPhase::Blocks,
                    );
                }
                FlowPhase::Floats => {
                    if child.is_float {
                        // A float paints itself as a pseudo-stacking context (blocks, floats, inlines inside).
                        self.paint_stacking_context(
                            child,
                            parent_origin,
                            child_abs,
                            spatial,
                            scroll_parent,
                        );
                    } else if !is_inline_level_fragment(child) {
                        let clips = child.style.overflow.x != OverflowMode::Visible
                            || child.style.overflow.y != OverflowMode::Visible;
                        if clips {
                            let clip_rect = Rect::new(
                                child_abs.min_x() + child.border.left,
                                child_abs.min_y() + child.border.top,
                                (child_abs.size.width - child.border.horizontal()).max(0.0),
                                (child_abs.size.height - child.border.vertical()).max(0.0),
                            );
                            self.list.push(DisplayItem::PushClip {
                                spatial,
                                rect: clip_rect,
                                radii: resolve_radii(&child.style, clip_rect),
                            });
                        }
                        self.paint_flow_phase(
                            child,
                            child_abs.origin,
                            spatial,
                            scroll_parent,
                            FlowPhase::Floats,
                        );
                        if clips {
                            self.list.push(DisplayItem::PopClip);
                        }
                    }
                }
                FlowPhase::Inlines => {
                    if child.is_float {
                        continue;
                    }
                    if is_inline_level_fragment(child) {
                        self.paint_inline_subtree(child, child_abs, spatial, scroll_parent);
                    } else {
                        if is_list_item {
                            self.paint_list_marker(child, child_abs, list_ordinal);
                        }
                        let clips = child.style.overflow.x != OverflowMode::Visible
                            || child.style.overflow.y != OverflowMode::Visible;
                        if clips {
                            let clip_rect = Rect::new(
                                child_abs.min_x() + child.border.left,
                                child_abs.min_y() + child.border.top,
                                (child_abs.size.width - child.border.horizontal()).max(0.0),
                                (child_abs.size.height - child.border.vertical()).max(0.0),
                            );
                            self.list.push(DisplayItem::PushClip {
                                spatial,
                                rect: clip_rect,
                                radii: resolve_radii(&child.style, clip_rect),
                            });
                        }
                        self.paint_flow_phase(
                            child,
                            child_abs.origin,
                            spatial,
                            scroll_parent,
                            FlowPhase::Inlines,
                        );
                        if clips {
                            self.list.push(DisplayItem::PopClip);
                        }
                    }
                }
            }
        }
    }

    fn paint_non_stacking_subtree(
        &mut self,
        frag: &Fragment,
        abs_rect: Rect,
        spatial: SpatialNodeId,
        scroll_parent: Option<SpatialNodeId>,
        phase: FlowPhase,
    ) {
        self.paint_box_decorations(frag, abs_rect, spatial);

        let clips = frag.style.overflow.x != OverflowMode::Visible
            || frag.style.overflow.y != OverflowMode::Visible;
        if clips {
            let clip_rect = Rect::new(
                abs_rect.min_x() + frag.border.left,
                abs_rect.min_y() + frag.border.top,
                (abs_rect.size.width - frag.border.horizontal()).max(0.0),
                (abs_rect.size.height - frag.border.vertical()).max(0.0),
            );
            self.list.push(DisplayItem::PushClip {
                spatial,
                rect: clip_rect,
                radii: resolve_radii(&frag.style, clip_rect),
            });
        }

        self.paint_leaf_content(frag, abs_rect);
        self.paint_flow_phase(frag, abs_rect.origin, spatial, scroll_parent, phase);

        if clips {
            self.list.push(DisplayItem::PopClip);
        }
    }

    fn paint_inline_subtree(
        &mut self,
        frag: &Fragment,
        abs_rect: Rect,
        spatial: SpatialNodeId,
        scroll_parent: Option<SpatialNodeId>,
    ) {
        match &frag.kind {
            FragmentKind::LineBox { .. } => {
                for ch in frag.children.iter() {
                    if creates_stacking_context(ch) || is_positioned(ch) {
                        continue;
                    }
                    let ch_abs = ch.absolute_border_rect(abs_rect.origin);
                    self.paint_inline_subtree(ch, ch_abs, spatial, scroll_parent);
                }
            }
            FragmentKind::InlineBox { .. } => {
                self.paint_box_decorations(frag, abs_rect, spatial);
            }
            FragmentKind::TextRun(_) => {
                // Text runs inherit their parent element's computed style for typography only;
                // never re-paint box backgrounds, borders, shadows, or outlines per text run.
                self.paint_leaf_content(frag, abs_rect);
            }
            FragmentKind::Image { .. }
            | FragmentKind::Svg { .. }
            | FragmentKind::FormControl { .. }
            | FragmentKind::ExternalSurface { .. } => {
                self.paint_box_decorations(frag, abs_rect, spatial);
                self.paint_leaf_content(frag, abs_rect);
            }
            FragmentKind::Box => {
                // Atomic inline (`inline-block`, `inline-flex`, `inline-grid`, `inline-table`)
                // paints atomically like a pseudo-stacking context per CSS 2.1 Appendix E.
                self.paint_stacking_context(
                    frag,
                    Point::new(
                        abs_rect.origin.x - frag.border_rect.origin.x,
                        abs_rect.origin.y - frag.border_rect.origin.y,
                    ),
                    abs_rect,
                    spatial,
                    scroll_parent,
                );
            }
        }
    }

    fn paint_list_marker(&mut self, frag: &Fragment, abs_rect: Rect, ordinal: usize) {
        if frag.style.visibility != VisibilityMode::Visible || abs_rect.is_empty() {
            return;
        }
        let lst = frag.style.text.list_style_type;
        if lst == ListStyleTypeMode::None {
            return;
        }
        let color = frag.style.text.color;
        if color.a == 0 {
            return;
        }
        let font_size = frag.style.text.font_size_px.max(1.0);
        let baseline_y = abs_rect.min_y()
            + frag
                .baseline
                .unwrap_or(frag.border.top + frag.padding.top + font_size * 0.85);
        let is_inside = frag.style.text.list_style_position == ListStylePositionMode::Inside;
        let anchor_x = if is_inside {
            abs_rect.min_x() + frag.border.left + frag.padding.left + font_size * 0.35
        } else {
            abs_rect.min_x() - font_size * 0.6
        };
        let rgba = to_rgba8(color);

        match lst {
            ListStyleTypeMode::Disc => {
                let d = (font_size * 0.34).clamp(3.0, 8.0);
                let r = d * 0.5;
                let cy = baseline_y - font_size * 0.32;
                let rect = Rect::new(anchor_x - r, cy - r, d, d);
                self.list.push(DisplayItem::Rect {
                    bounds: rect,
                    color: rgba,
                    radii: [r; 4],
                });
            }
            ListStyleTypeMode::Circle => {
                let d = (font_size * 0.34).clamp(3.0, 8.0);
                let r = d * 0.5;
                let cy = baseline_y - font_size * 0.32;
                let rect = Rect::new(anchor_x - r, cy - r, d, d);
                self.list.push(DisplayItem::Border {
                    bounds: rect,
                    widths: [1.0; 4],
                    colors: [rgba; 4],
                    radii: [r; 4],
                });
            }
            ListStyleTypeMode::Square => {
                let d = (font_size * 0.32).clamp(3.0, 8.0);
                let r = d * 0.5;
                let cy = baseline_y - font_size * 0.32;
                let rect = Rect::new(anchor_x - r, cy - r, d, d);
                self.list.push(DisplayItem::Rect {
                    bounds: rect,
                    color: rgba,
                    radii: [0.0; 4],
                });
            }
            _ => {
                let marker_text = format_list_marker(lst, ordinal);
                if marker_text.is_empty() {
                    return;
                }
                let font_id = crate::ifc::resolve_font_for_style(self.font_db, None, &frag.style);
                let Some(font_res_id) = self.register_font(font_id) else {
                    return;
                };
                let shaper = rix_text::TextShaper::new(self.font_db);
                let runs = shaper.shape_text(&marker_text, font_id, font_size);
                let total_w: f32 = runs.iter().map(|r| r.width).sum();
                if total_w <= 0.0 {
                    return;
                }
                let start_x = if is_inside {
                    abs_rect.min_x() + frag.border.left + frag.padding.left
                } else {
                    abs_rect.min_x() - total_w - font_size * 0.25
                };
                let mut pen_x = start_x;
                let mut instances = Vec::new();
                for run in runs {
                    for g in run.glyphs {
                        instances.push(GlyphInstance {
                            glyph_id: g.glyph_id,
                            x: pen_x + g.x_offset,
                            y: baseline_y - g.y_offset,
                        });
                        pen_x += g.x_advance;
                    }
                }
                if !instances.is_empty() {
                    let bounds =
                        Rect::new(start_x, baseline_y - font_size, total_w, font_size * 1.25);
                    self.list
                        .push_glyph_run(bounds, font_res_id, font_size, rgba, &instances);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_svg_asset_in_rect(
        &mut self,
        svg: &crate::assets::DecodedSvg,
        abs_rect: Rect,
        spatial: SpatialNodeId,
        size: &BackgroundSizeValue,
        pos_x: &LengthPercentage,
        pos_y: &LengthPercentage,
        tint_override: Option<ColorRgba>,
    ) {
        if abs_rect.is_empty() || svg.paths.is_empty() {
            return;
        }
        let vb = svg.view_box.map_or_else(
            || {
                Rect::new(
                    0.0,
                    0.0,
                    svg.intrinsic_size.width.max(1.0),
                    svg.intrinsic_size.height.max(1.0),
                )
            },
            |v| Rect::new(v.min_x, v.min_y, v.width.max(1.0), v.height.max(1.0)),
        );
        let sx = f64::from(abs_rect.size.width / vb.size.width);
        let sy = f64::from(abs_rect.size.height / vb.size.height);
        let eval = EvalContext::default();
        let nat_w = f64::from(svg.intrinsic_size.width.max(1.0));
        let nat_h = f64::from(svg.intrinsic_size.height.max(1.0));
        let (scale_x, scale_y) = match size {
            BackgroundSizeValue::Cover => {
                let s = sx.max(sy);
                (s, s)
            }
            BackgroundSizeValue::Contain => {
                let s = sx.min(sy);
                (s, s)
            }
            BackgroundSizeValue::Auto => {
                // CSS Backgrounds L3 §3.9: `background-size: auto` renders at the SVG's intrinsic
                // pixel dimensions (`width`/`height` attributes, falling back to `viewBox` size).
                let s = (nat_w / f64::from(vb.size.width)).min(nat_h / f64::from(vb.size.height));
                (s, s)
            }
            BackgroundSizeValue::Explicit(w_opt, h_opt) => {
                let (tw, th) = match (w_opt, h_opt) {
                    (Some(w), Some(h)) => (
                        f64::from(w.resolve_px(abs_rect.size.width, &eval).max(0.0)),
                        f64::from(h.resolve_px(abs_rect.size.height, &eval).max(0.0)),
                    ),
                    (Some(w), None) => {
                        let rw = f64::from(w.resolve_px(abs_rect.size.width, &eval).max(0.0));
                        (rw, rw * (nat_h / nat_w))
                    }
                    (None, Some(h)) => {
                        let rh = f64::from(h.resolve_px(abs_rect.size.height, &eval).max(0.0));
                        (rh * (nat_w / nat_h), rh)
                    }
                    (None, None) => (nat_w, nat_h),
                };
                if tw <= 0.0 || th <= 0.0 {
                    return;
                }
                (
                    tw / f64::from(vb.size.width),
                    th / f64::from(vb.size.height),
                )
            }
        };
        let rendered_w = vb.size.width * scale_x as f32;
        let rendered_h = vb.size.height * scale_y as f32;
        let off_x = pos_x.resolve_px(abs_rect.size.width - rendered_w, &eval);
        let off_y = pos_y.resolve_px(abs_rect.size.height - rendered_h, &eval);
        let tx = -f64::from(vb.origin.x) * scale_x + f64::from(off_x);
        let ty = -f64::from(vb.origin.y) * scale_y + f64::from(off_y);
        let xf = Affine::translate((tx, ty)) * Affine::scale_non_uniform(scale_x, scale_y);
        let stroke_scale = ((scale_x + scale_y) * 0.5) as f32;
        let needs_clip = rendered_w > abs_rect.size.width + 0.5
            || rendered_h > abs_rect.size.height + 0.5
            || off_x < -0.5
            || off_y < -0.5;
        if needs_clip {
            self.list.push(DisplayItem::PushClip {
                spatial,
                rect: abs_rect,
                radii: [0.0; 4],
            });
        }

        for entry in &svg.paths {
            let Ok(mut bez) = parse_svg_path(&entry.path_d) else {
                continue;
            };
            let elem_xf = xf * Affine::new(entry.transform);
            bez.apply_affine(elem_xf);
            let coeffs = elem_xf.as_coeffs();
            let path_id = hash_svg_path_id(&entry.path_d, rendered_w, rendered_h, &coeffs);
            self.resources.register_path(path_id, bez);
            let fill = match tint_override {
                Some(tint) => entry.fill.map(|_| to_rgba8(tint)),
                None => entry.fill.map(to_rgba8),
            };
            let stroke = match tint_override {
                Some(tint) => entry
                    .stroke
                    .map(|(_, w)| (to_rgba8(tint), (w * stroke_scale).max(0.5))),
                None => entry
                    .stroke
                    .map(|(c, w)| (to_rgba8(c), (w * stroke_scale).max(0.5))),
            };
            self.list.push(DisplayItem::VectorPath {
                bounds: abs_rect,
                path_id,
                fill,
                stroke,
            });
        }
        if needs_clip {
            self.list.push(DisplayItem::PopClip);
        }
    }

    fn paint_box_decorations(&mut self, frag: &Fragment, abs_rect: Rect, spatial: SpatialNodeId) {
        if frag.style.visibility != VisibilityMode::Visible || abs_rect.is_empty() {
            return;
        }
        let style = &frag.style;
        let radii = resolve_radii(style, abs_rect);
        let eval = EvalContext::default();

        // Outset box shadows behind background.
        if let Some(effects) = &style.effects {
            for shadow in &effects.box_shadow {
                if !shadow.inset {
                    let color = shadow.color.resolve(style.text.color, self.dark_mode);
                    if color.a > 0 {
                        self.list.push(DisplayItem::BoxShadow {
                            bounds: abs_rect,
                            offset: Vec2::new(
                                shadow.offset_x.resolve_px(abs_rect.size.width, &eval),
                                shadow.offset_y.resolve_px(abs_rect.size.height, &eval),
                            ),
                            blur_radius: shadow
                                .blur
                                .resolve_px(abs_rect.size.width, &eval)
                                .max(0.0),
                            spread_radius: shadow.spread.resolve_px(abs_rect.size.width, &eval),
                            color: to_rgba8(color),
                            radii,
                            inset: false,
                        });
                    }
                }
            }
        }

        // CSS Backgrounds L4 §3.7: `background-clip: text` clips the element's background to the
        // foreground text glyphs (never painting a solid/gradient box rectangle behind the text).
        let is_bg_clip_text = style.visual.background_clip == BackgroundClipMode::Text;

        // Solid background fill (or CSS Masking L1 `mask-image: url(...)` tinted vector icon).
        let bg = style.visual.background_color;
        if let BackgroundImageValue::Url(mask_url) = &style.visual.mask_image {
            if bg.a > 0 {
                if let Some(svg) = self.assets.get_svg_by_src(mask_url.as_str()).cloned() {
                    let center = LengthPercentage::Percent(0.5);
                    self.paint_svg_asset_in_rect(
                        &svg,
                        abs_rect,
                        spatial,
                        &BackgroundSizeValue::Contain,
                        &center,
                        &center,
                        Some(bg),
                    );
                }
            }
        } else if bg.a > 0 && !is_bg_clip_text {
            self.list.push(DisplayItem::Rect {
                bounds: abs_rect,
                color: to_rgba8(bg),
                radii,
            });
        }

        // Multi-layer `background-image` (`url(...)`, `linear-gradient(...)`, `radial-gradient(...)`)
        // painted bottom-to-top per CSS Backgrounds Level 3 (last listed layer paints first).
        if !is_bg_clip_text {
            for layer in style.visual.background_layers.iter().rev() {
                match &layer.image {
                    BackgroundImageValue::None => {}
                    BackgroundImageValue::LinearGradient { angle_deg, stops } => {
                        let (start, end) = linear_gradient_endpoints(abs_rect, *angle_deg);
                        let grad_len = (end.x - start.x).hypot(end.y - start.y).max(1.0);
                        let resolved = resolve_gradient_stops(
                            stops,
                            grad_len,
                            style.text.color,
                            self.dark_mode,
                        );
                        if resolved.len() >= 2 {
                            self.list.push(DisplayItem::LinearGradient {
                                bounds: abs_rect,
                                start,
                                end,
                                stops: resolved,
                                radii,
                            });
                        }
                    }
                    BackgroundImageValue::RadialGradient {
                        shape_circle,
                        cx,
                        cy,
                        stops,
                    } => {
                        let center = Point::new(
                            abs_rect.min_x() + cx.resolve_px(abs_rect.size.width, &eval),
                            abs_rect.min_y() + cy.resolve_px(abs_rect.size.height, &eval),
                        );
                        let dx = (center.x - abs_rect.min_x())
                            .abs()
                            .max((abs_rect.max_x() - center.x).abs());
                        let dy = (center.y - abs_rect.min_y())
                            .abs()
                            .max((abs_rect.max_y() - center.y).abs());
                        let radius = if *shape_circle {
                            (dx * dx + dy * dy).sqrt().max(1.0)
                        } else {
                            dx.max(dy).max(1.0)
                        };
                        let resolved =
                            resolve_gradient_stops(stops, radius, style.text.color, self.dark_mode);
                        if resolved.len() >= 2 {
                            self.list.push(DisplayItem::RadialGradient {
                                bounds: abs_rect,
                                center,
                                radius,
                                stops: resolved,
                                radii,
                            });
                        }
                    }
                    BackgroundImageValue::Url(url) => {
                        let effective_size =
                            style.visual.background_size.as_ref().unwrap_or(&layer.size);
                        let px = if matches!(layer.pos_x, LengthPercentage::Zero)
                            && !matches!(style.visual.background_position.0, LengthPercentage::Zero)
                        {
                            &style.visual.background_position.0
                        } else {
                            &layer.pos_x
                        };
                        let py = if matches!(layer.pos_y, LengthPercentage::Zero)
                            && !matches!(style.visual.background_position.1, LengthPercentage::Zero)
                        {
                            &style.visual.background_position.1
                        } else {
                            &layer.pos_y
                        };
                        if let Some(decoded) = self.assets.get_by_src(url.as_str()) {
                            let img_id = decoded.image_id;
                            let iw = decoded.width.max(1) as f32;
                            let ih = decoded.height.max(1) as f32;
                            if self.ensure_image_registered(img_id) {
                                let img_bounds = match effective_size {
                                    BackgroundSizeValue::Cover => {
                                        let scale = (abs_rect.size.width / iw)
                                            .max(abs_rect.size.height / ih);
                                        let dw = iw * scale;
                                        let dh = ih * scale;
                                        Rect::new(
                                            abs_rect.min_x()
                                                + px.resolve_px(abs_rect.size.width - dw, &eval),
                                            abs_rect.min_y()
                                                + py.resolve_px(abs_rect.size.height - dh, &eval),
                                            dw,
                                            dh,
                                        )
                                    }
                                    BackgroundSizeValue::Contain => {
                                        let scale = (abs_rect.size.width / iw)
                                            .min(abs_rect.size.height / ih);
                                        let dw = iw * scale;
                                        let dh = ih * scale;
                                        Rect::new(
                                            abs_rect.min_x()
                                                + px.resolve_px(abs_rect.size.width - dw, &eval),
                                            abs_rect.min_y()
                                                + py.resolve_px(abs_rect.size.height - dh, &eval),
                                            dw,
                                            dh,
                                        )
                                    }
                                    BackgroundSizeValue::Explicit(w_opt, h_opt) => {
                                        let (dw, dh) = match (w_opt, h_opt) {
                                            (Some(w), Some(h)) => (
                                                w.resolve_px(abs_rect.size.width, &eval).max(0.0),
                                                h.resolve_px(abs_rect.size.height, &eval).max(0.0),
                                            ),
                                            (Some(w), None) => {
                                                let rw = w
                                                    .resolve_px(abs_rect.size.width, &eval)
                                                    .max(0.0);
                                                (rw, rw * (ih / iw))
                                            }
                                            (None, Some(h)) => {
                                                let rh = h
                                                    .resolve_px(abs_rect.size.height, &eval)
                                                    .max(0.0);
                                                (rh * (iw / ih), rh)
                                            }
                                            (None, None) => (iw, ih),
                                        };
                                        Rect::new(
                                            abs_rect.min_x()
                                                + px.resolve_px(abs_rect.size.width - dw, &eval),
                                            abs_rect.min_y()
                                                + py.resolve_px(abs_rect.size.height - dh, &eval),
                                            dw,
                                            dh,
                                        )
                                    }
                                    BackgroundSizeValue::Auto => abs_rect,
                                };
                                // CSS Backgrounds L3 §3.7: background images are always clipped to
                                // the element's background-clip box (default border-box `abs_rect`).
                                let needs_bg_clip = img_bounds.min_x() < abs_rect.min_x() - 0.5
                                    || img_bounds.min_y() < abs_rect.min_y() - 0.5
                                    || img_bounds.max_x() > abs_rect.max_x() + 0.5
                                    || img_bounds.max_y() > abs_rect.max_y() + 0.5;
                                if needs_bg_clip {
                                    self.list.push(DisplayItem::PushClip {
                                        spatial,
                                        rect: abs_rect,
                                        radii,
                                    });
                                }
                                self.list.push(DisplayItem::Image {
                                    bounds: img_bounds,
                                    image_id: img_id,
                                    sampling: ImageSampling::Bilinear,
                                    radii: if needs_bg_clip { [0.0; 4] } else { radii },
                                });
                                if needs_bg_clip {
                                    self.list.push(DisplayItem::PopClip);
                                }
                            }
                        } else if let Some(svg) = self.assets.get_svg_by_src(url.as_str()).cloned()
                        {
                            self.paint_svg_asset_in_rect(
                                &svg,
                                abs_rect,
                                spatial,
                                effective_size,
                                px,
                                py,
                                None,
                            );
                        }
                    }
                }
            }
        }

        // Inset box shadows above background, below borders.
        if let Some(effects) = &style.effects {
            for shadow in &effects.box_shadow {
                if shadow.inset {
                    let color = shadow.color.resolve(style.text.color, self.dark_mode);
                    if color.a > 0 {
                        self.list.push(DisplayItem::BoxShadow {
                            bounds: abs_rect,
                            offset: Vec2::new(
                                shadow.offset_x.resolve_px(abs_rect.size.width, &eval),
                                shadow.offset_y.resolve_px(abs_rect.size.height, &eval),
                            ),
                            blur_radius: shadow
                                .blur
                                .resolve_px(abs_rect.size.width, &eval)
                                .max(0.0),
                            spread_radius: shadow.spread.resolve_px(abs_rect.size.width, &eval),
                            color: to_rgba8(color),
                            radii,
                            inset: true,
                        });
                    }
                }
            }
        }

        // Borders (respecting split inline-box left/right borders already zeroed on frag.border).
        let widths = [
            frag.border.top,
            frag.border.right,
            frag.border.bottom,
            frag.border.left,
        ];
        if widths.iter().any(|&w| w > 0.0) {
            let bc = &style.visual.border_color;
            let colors = [
                to_rgba8(bc.top.resolve(style.text.color, self.dark_mode)),
                to_rgba8(bc.right.resolve(style.text.color, self.dark_mode)),
                to_rgba8(bc.bottom.resolve(style.text.color, self.dark_mode)),
                to_rgba8(bc.left.resolve(style.text.color, self.dark_mode)),
            ];
            self.list.push(DisplayItem::Border {
                bounds: abs_rect,
                widths,
                colors,
                radii,
            });
        }

        // CSS outline (rendered around border box expanded by outline-offset + outline-width).
        if style.visual.outline_width > 0.0
            && !matches!(
                style.visual.outline_style,
                LineStyle::None | LineStyle::Hidden
            )
        {
            let offset = style
                .visual
                .outline_offset
                .resolve_px(abs_rect.size.width, &eval);
            let ow = style.visual.outline_width;
            let expand = offset + ow;
            let outline_rect = Rect::new(
                abs_rect.min_x() - expand,
                abs_rect.min_y() - expand,
                (abs_rect.size.width + expand * 2.0).max(0.0),
                (abs_rect.size.height + expand * 2.0).max(0.0),
            );
            let oc = to_rgba8(
                style
                    .visual
                    .outline_color
                    .resolve(style.text.color, self.dark_mode),
            );
            let outline_radii = radii.map(|r| if r > 0.0 { r + expand.max(0.0) } else { 0.0 });
            self.list.push(DisplayItem::Border {
                bounds: outline_rect,
                widths: [ow; 4],
                colors: [oc; 4],
                radii: outline_radii,
            });
        }
    }

    fn paint_leaf_content(&mut self, frag: &Fragment, abs_rect: Rect) {
        if frag.style.visibility != VisibilityMode::Visible {
            return;
        }
        let content_abs = Rect::new(
            abs_rect.min_x() + frag.content_rect.origin.x,
            abs_rect.min_y() + frag.content_rect.origin.y,
            frag.content_rect.size.width,
            frag.content_rect.size.height,
        );

        match &frag.kind {
            FragmentKind::TextRun(run) => {
                if run.glyphs.is_empty() {
                    return;
                }
                // CSS Backgrounds L4 §3.7: When `background-clip: text` is active with a gradient
                // or opaque background, paint the glyphs using the background clip fill color
                // (especially when author CSS sets `color: transparent` or `-webkit-text-fill-color`).
                let effective_color =
                    if frag.style.visual.background_clip == BackgroundClipMode::Text {
                        extract_text_clip_color(&frag.style, self.dark_mode).unwrap_or(run.color)
                    } else {
                        run.color
                    };
                if effective_color.a == 0 {
                    return;
                }
                let baseline_y = abs_rect.min_y() + run.baseline_y;
                let color = to_rgba8(effective_color);

                // Emit glyphs grouped by each glyph's true fallback `font_id` so multi-script runs
                // (Latin + Arabic + CJK + Emoji) ALWAYS use the exact font whose glyph IDs were shaped!
                let mut span_start = 0usize;
                while span_start < run.glyphs.len() {
                    let span_font = run.glyphs[span_start].font_id;
                    let mut span_end = span_start + 1;
                    while span_end < run.glyphs.len() && run.glyphs[span_end].font_id == span_font {
                        span_end += 1;
                    }
                    if let Some(font_res_id) = self.register_font(span_font) {
                        let instances: Vec<GlyphInstance> = run.glyphs[span_start..span_end]
                            .iter()
                            .map(|g| GlyphInstance {
                                glyph_id: g.glyph_id,
                                x: abs_rect.min_x() + g.x,
                                y: baseline_y + g.y,
                            })
                            .collect();
                        self.list.push_glyph_run(
                            abs_rect,
                            font_res_id,
                            run.font_size,
                            color,
                            &instances,
                        );
                    }
                    span_start = span_end;
                }

                // Paint `text-decoration` lines (underline, overline, line-through).
                if !run.decoration.is_empty() && run.width > 0.0 {
                    let deco_color = frag
                        .style
                        .visual
                        .text_decoration_color
                        .map_or(color, |c| to_rgba8(c.resolve(run.color, self.dark_mode)));
                    let thickness = (run.font_size / 14.0).clamp(1.0, 2.5);
                    if run.decoration.contains(TextDecorationLine::UNDERLINE) {
                        self.list.push(DisplayItem::Rect {
                            bounds: Rect::new(
                                abs_rect.min_x(),
                                baseline_y + thickness * 1.2,
                                run.width,
                                thickness,
                            ),
                            color: deco_color,
                            radii: [0.0; 4],
                        });
                    }
                    if run.decoration.contains(TextDecorationLine::LINE_THROUGH) {
                        self.list.push(DisplayItem::Rect {
                            bounds: Rect::new(
                                abs_rect.min_x(),
                                baseline_y - run.font_size * 0.3,
                                run.width,
                                thickness,
                            ),
                            color: deco_color,
                            radii: [0.0; 4],
                        });
                    }
                    if run.decoration.contains(TextDecorationLine::OVERLINE) {
                        self.list.push(DisplayItem::Rect {
                            bounds: Rect::new(
                                abs_rect.min_x(),
                                abs_rect.min_y(),
                                run.width,
                                thickness,
                            ),
                            color: deco_color,
                            radii: [0.0; 4],
                        });
                    }
                }
            }
            FragmentKind::Image { image_id, .. } => {
                if content_abs.is_empty() {
                    return;
                }
                if let Some(img_id) = *image_id {
                    if self.ensure_image_registered(img_id) {
                        self.list.push(DisplayItem::Image {
                            bounds: content_abs,
                            image_id: img_id,
                            sampling: ImageSampling::Bilinear,
                            radii: resolve_radii(&frag.style, content_abs),
                        });
                    }
                }
            }
            FragmentKind::ExternalSurface { surface_id, .. } => {
                if content_abs.is_empty() {
                    return;
                }
                self.list.push(DisplayItem::ExternalSurface {
                    bounds: content_abs,
                    surface_id: *surface_id,
                    radii: resolve_radii(&frag.style, content_abs),
                });
            }
            FragmentKind::Svg { view_box, paths } => {
                if content_abs.is_empty() || paths.is_empty() {
                    return;
                }
                let sx = if view_box.size.width > 0.0 {
                    f64::from(content_abs.size.width / view_box.size.width)
                } else {
                    1.0
                };
                let sy = if view_box.size.height > 0.0 {
                    f64::from(content_abs.size.height / view_box.size.height)
                } else {
                    1.0
                };
                let scale = sx.min(sy);
                let tx = -f64::from(view_box.origin.x) * scale
                    + f64::from(content_abs.size.width - view_box.size.width * scale as f32) * 0.5;
                let ty = -f64::from(view_box.origin.y) * scale
                    + f64::from(content_abs.size.height - view_box.size.height * scale as f32)
                        * 0.5;
                let xf = Affine::translate((tx, ty)) * Affine::scale(scale);

                for entry in paths {
                    let Ok(mut bez) = parse_svg_path(&entry.path_d) else {
                        continue;
                    };
                    let elem_xf = xf * Affine::new(entry.transform);
                    bez.apply_affine(elem_xf);
                    let path_id = hash_svg_path_id(
                        &entry.path_d,
                        content_abs.size.width,
                        content_abs.size.height,
                        &entry.transform,
                    );
                    self.resources.register_path(path_id, bez);
                    self.list.push(DisplayItem::VectorPath {
                        bounds: content_abs,
                        path_id,
                        fill: entry.fill.map(to_rgba8),
                        stroke: entry
                            .stroke
                            .map(|(c, w)| (to_rgba8(c), (w * scale as f32).max(0.5))),
                    });
                }
            }
            FragmentKind::FormControl {
                kind,
                text,
                placeholder,
            } => {
                self.paint_form_control(frag, abs_rect, content_abs, *kind, text, placeholder);
            }
            FragmentKind::Box | FragmentKind::InlineBox { .. } | FragmentKind::LineBox { .. } => {}
        }
    }

    fn paint_form_control(
        &mut self,
        frag: &Fragment,
        abs_rect: Rect,
        content_abs: Rect,
        kind: FormControlKind,
        text: &str,
        placeholder: &str,
    ) {
        if abs_rect.is_empty() {
            return;
        }
        // Background and border are already painted from `ComputedStyle` (including `UA_CSS`
        // defaults) in `paint_box_decorations`, so author rules like `background: transparent;
        // border: 0;` are respected faithfully instead of painting a hardcoded white box!
        let display_text = if !text.is_empty() { text } else { placeholder };
        if display_text.is_empty() {
            return;
        }
        let font_id = crate::ifc::resolve_font_for_style(self.font_db, None, &frag.style);
        let font_size = frag.style.text.font_size_px.max(11.0);
        let shaper = rix_text::TextShaper::new(self.font_db);
        let runs = shaper.shape_text(display_text, font_id, font_size);
        let text_w: f32 = runs.iter().map(|r| r.width).sum();
        let start_x = match kind {
            FormControlKind::Button => {
                content_abs.min_x() + ((content_abs.size.width - text_w) * 0.5).max(0.0)
            }
            _ => content_abs.min_x() + 6.0,
        };
        let baseline_y = content_abs.min_y() + (content_abs.size.height + font_size * 0.72) * 0.5;
        let color = if text.is_empty() {
            Rgba8::new(130, 136, 148, 255)
        } else {
            to_rgba8(frag.style.text.color)
        };
        let mut pen_x = start_x;
        for run in runs {
            if run.glyphs.is_empty() {
                continue;
            }
            let mut instances = Vec::with_capacity(run.glyphs.len());
            for g in run.glyphs {
                instances.push(GlyphInstance {
                    glyph_id: g.glyph_id,
                    x: pen_x + g.x_offset,
                    y: baseline_y - g.y_offset,
                });
                pen_x += g.x_advance;
            }
            // Register each shaped run's actual fallback `run.font_id` so glyph IDs ALWAYS match
            // the font face that produced them (never garbling fallback glyphs into another font!).
            if let Some(font_res_id) = self.register_font(run.font_id) {
                self.list
                    .push_glyph_run(content_abs, font_res_id, font_size, color, &instances);
            }
        }
    }
}

#[derive(Clone, Copy)]
enum FlowPhase {
    Blocks,
    Floats,
    Inlines,
}

fn collect_stacking_children<'f>(
    parent: &'f Fragment,
    parent_origin: Point,
    container_rect: Rect,
    neg_z: &mut Vec<CollectedStackingChild<'f>>,
    zero_z: &mut Vec<CollectedStackingChild<'f>>,
    pos_z: &mut Vec<CollectedStackingChild<'f>>,
    order: &mut usize,
) {
    for child in parent.children.iter() {
        let child_abs = child.absolute_border_rect(parent_origin);
        if creates_stacking_context(child) || is_positioned(child) {
            let z = match child.style.z_index {
                ZIndexValue::Auto => 0,
                ZIndexValue::Integer(n) => n,
            };
            let entry = CollectedStackingChild {
                frag: child,
                parent_origin,
                container_rect,
                z_index: z,
                order: *order,
            };
            *order += 1;
            if creates_stacking_context(child) && z < 0 {
                neg_z.push(entry);
            } else if creates_stacking_context(child) && z > 0 {
                pos_z.push(entry);
            } else {
                zero_z.push(entry);
            }
            // Do NOT descend past a child stacking context — its own descendants belong to it!
            continue;
        }
        collect_stacking_children(
            child,
            child_abs.origin,
            child_abs,
            neg_z,
            zero_z,
            pos_z,
            order,
        );
    }
}

#[inline]
fn is_positioned(frag: &Fragment) -> bool {
    frag.style.position != PositionMode::Static
}

#[inline]
fn creates_stacking_context(frag: &Fragment) -> bool {
    let s = &frag.style;
    if s.opacity < 0.999 {
        return true;
    }
    if matches!(s.position, PositionMode::Fixed | PositionMode::Sticky) {
        return true;
    }
    if s.position != PositionMode::Static && s.z_index != ZIndexValue::Auto {
        return true;
    }
    if let Some(effects) = &s.effects {
        if !effects.transform.is_empty()
            || !effects.filter.is_empty()
            || !effects.animations.is_empty()
            || effects.clip_path != ClipPathValue::None
            || effects.mix_blend_mode != MixBlendMode::Normal
        {
            return true;
        }
    }
    false
}

#[inline]
fn is_inline_level_fragment(frag: &Fragment) -> bool {
    matches!(
        frag.kind,
        FragmentKind::LineBox { .. }
            | FragmentKind::InlineBox { .. }
            | FragmentKind::TextRun(_)
            | FragmentKind::Image { .. }
            | FragmentKind::Svg { .. }
            | FragmentKind::FormControl { .. }
            | FragmentKind::ExternalSurface { .. }
    )
}

#[inline]
fn to_rgba8(c: ColorRgba) -> Rgba8 {
    Rgba8::new(c.r, c.g, c.b, c.a)
}

fn resolve_radii(style: &ComputedStyle, rect: Rect) -> [f32; 4] {
    let eval = EvalContext::default();
    let basis = rect.size.width.min(rect.size.height);
    let max_r = basis * 0.5;
    let br = &style.visual.border_radius;
    [
        br.top_left.x.resolve_px(basis, &eval).clamp(0.0, max_r),
        br.top_right.x.resolve_px(basis, &eval).clamp(0.0, max_r),
        br.bottom_right.x.resolve_px(basis, &eval).clamp(0.0, max_r),
        br.bottom_left.x.resolve_px(basis, &eval).clamp(0.0, max_r),
    ]
}

/// Multiplies two column-major 4x4 homogeneous transform matrices `a * b`.
fn mat4_mul(a: &[f64; 16], b: &[f64; 16]) -> [f64; 16] {
    let mut out = [0.0f64; 16];
    for col in 0..4 {
        for row in 0..4 {
            let mut sum = 0.0;
            for k in 0..4 {
                sum += a[k * 4 + row] * b[col * 4 + k];
            }
            out[col * 4 + row] = sum;
        }
    }
    out
}

const MAT4_IDENTITY: [f64; 16] = [
    1.0, 0.0, 0.0, 0.0, // col 0
    0.0, 1.0, 0.0, 0.0, // col 1
    0.0, 0.0, 1.0, 0.0, // col 2
    0.0, 0.0, 0.0, 1.0, // col 3
];

/// Composes a full CSS 2D/3D `transform` operation list in column-major 4x4
/// homogeneous space and projects the resulting plane transform to a 2D affine
/// matrix `[a, b, c, d, e, f]` suitable for Vello/Kurbo composition.
pub(crate) fn compose_transform_matrix(ops: &[TransformOp], rect: Rect) -> [f32; 6] {
    let eval = EvalContext::default();
    let mut m = MAT4_IDENTITY;
    for op in ops {
        let next = match op {
            TransformOp::Translate(tx, ty) => {
                let x = f64::from(tx.resolve_px(rect.size.width, &eval));
                let y = f64::from(ty.resolve_px(rect.size.height, &eval));
                [
                    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, x, y, 0.0, 1.0,
                ]
            }
            TransformOp::Translate3d(tx, ty, tz) => {
                let x = f64::from(tx.resolve_px(rect.size.width, &eval));
                let y = f64::from(ty.resolve_px(rect.size.height, &eval));
                let z = f64::from(*tz);
                [
                    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, x, y, z, 1.0,
                ]
            }
            TransformOp::TranslateZ(tz) => [
                1.0,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
                0.0,
                0.0,
                f64::from(*tz),
                1.0,
            ],
            TransformOp::Scale(sx, sy) => [
                f64::from(*sx),
                0.0,
                0.0,
                0.0,
                0.0,
                f64::from(*sy),
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
            ],
            TransformOp::Scale3d(sx, sy, sz) => [
                f64::from(*sx),
                0.0,
                0.0,
                0.0,
                0.0,
                f64::from(*sy),
                0.0,
                0.0,
                0.0,
                0.0,
                f64::from(*sz),
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
            ],
            TransformOp::ScaleZ(sz) => [
                1.0,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
                0.0,
                0.0,
                0.0,
                f64::from(*sz),
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
            ],
            TransformOp::RotateDeg(deg) => {
                let (s, c) = f64::from(deg.to_radians()).sin_cos();
                [
                    c, s, 0.0, 0.0, -s, c, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
                ]
            }
            TransformOp::RotateXDeg(deg) => {
                let (s, c) = f64::from(deg.to_radians()).sin_cos();
                [
                    1.0, 0.0, 0.0, 0.0, 0.0, c, s, 0.0, 0.0, -s, c, 0.0, 0.0, 0.0, 0.0, 1.0,
                ]
            }
            TransformOp::RotateYDeg(deg) => {
                let (s, c) = f64::from(deg.to_radians()).sin_cos();
                [
                    c, 0.0, -s, 0.0, 0.0, 1.0, 0.0, 0.0, s, 0.0, c, 0.0, 0.0, 0.0, 0.0, 1.0,
                ]
            }
            TransformOp::Rotate3d(rx, ry, rz, deg) => {
                let len =
                    (f64::from(*rx).powi(2) + f64::from(*ry).powi(2) + f64::from(*rz).powi(2))
                        .sqrt();
                if len < 1e-6 {
                    MAT4_IDENTITY
                } else {
                    let (ux, uy, uz) = (
                        f64::from(*rx) / len,
                        f64::from(*ry) / len,
                        f64::from(*rz) / len,
                    );
                    let (s, c) = f64::from(deg.to_radians()).sin_cos();
                    let t = 1.0 - c;
                    [
                        t * ux * ux + c,
                        t * ux * uy + uz * s,
                        t * ux * uz - uy * s,
                        0.0,
                        t * ux * uy - uz * s,
                        t * uy * uy + c,
                        t * uy * uz + ux * s,
                        0.0,
                        t * ux * uz + uy * s,
                        t * uy * uz - ux * s,
                        t * uz * uz + c,
                        0.0,
                        0.0,
                        0.0,
                        0.0,
                        1.0,
                    ]
                }
            }
            TransformOp::SkewDeg(ax, ay) => [
                1.0,
                f64::from(ay.to_radians().tan()),
                0.0,
                0.0,
                f64::from(ax.to_radians().tan()),
                1.0,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
            ],
            TransformOp::Perspective(d) => {
                let inv = if *d > 1e-3 { -1.0 / f64::from(*d) } else { 0.0 };
                [
                    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, inv, 0.0, 0.0, 0.0, 1.0,
                ]
            }
            TransformOp::Matrix(arr) => [
                f64::from(arr[0]),
                f64::from(arr[1]),
                0.0,
                0.0,
                f64::from(arr[2]),
                f64::from(arr[3]),
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
                f64::from(arr[4]),
                f64::from(arr[5]),
                0.0,
                1.0,
            ],
            TransformOp::Matrix3d(arr) => arr.map(f64::from),
        };
        m = mat4_mul(&m, &next);
    }
    let w = if m[15].abs() > 1e-6 { m[15] } else { 1.0 };
    [
        (m[0] / w) as f32,
        (m[1] / w) as f32,
        (m[4] / w) as f32,
        (m[5] / w) as f32,
        (m[12] / w) as f32,
        (m[13] / w) as f32,
    ]
}

fn extract_render_filter_ops(
    ops: &[CssFilterOp],
    style: &ComputedStyle,
    rect: Rect,
    dark_mode: bool,
) -> SmallVec<[RenderFilterOp; 2]> {
    let mut out = SmallVec::new();
    let eval = style.text.eval_ctx;
    for op in ops {
        match op {
            CssFilterOp::Blur(lp) => {
                out.push(RenderFilterOp::Blur(lp.resolve_px(0.0, &eval).max(0.0)));
            }
            CssFilterOp::Opacity(a) => out.push(RenderFilterOp::Opacity(*a)),
            CssFilterOp::Brightness(b) => out.push(RenderFilterOp::Brightness(*b)),
            CssFilterOp::Contrast(c) => out.push(RenderFilterOp::Contrast(*c)),
            CssFilterOp::Grayscale(g) => out.push(RenderFilterOp::Grayscale(*g)),
            CssFilterOp::Saturate(s) => out.push(RenderFilterOp::Saturate(*s)),
            CssFilterOp::Invert(i) => out.push(RenderFilterOp::Invert(*i)),
            CssFilterOp::Sepia(s) => out.push(RenderFilterOp::Sepia(*s)),
            CssFilterOp::HueRotateDeg(d) => out.push(RenderFilterOp::HueRotateDeg(*d)),
            CssFilterOp::DropShadow {
                offset_x,
                offset_y,
                blur,
                color,
            } => {
                let c = color.resolve(style.text.color, dark_mode);
                out.push(RenderFilterOp::DropShadow {
                    offset: Vec2::new(
                        offset_x.resolve_px(rect.size.width, &eval),
                        offset_y.resolve_px(rect.size.height, &eval),
                    ),
                    blur_radius: blur.resolve_px(rect.size.width, &eval).max(0.0),
                    color: to_rgba8(c),
                });
            }
            CssFilterOp::Url(_) => {}
        }
    }
    out
}

fn extract_render_filters(
    style: &ComputedStyle,
    rect: Rect,
    dark_mode: bool,
) -> SmallVec<[RenderFilterOp; 2]> {
    match &style.effects {
        Some(effects) if !effects.filter.is_empty() => {
            extract_render_filter_ops(&effects.filter, style, rect, dark_mode)
        }
        _ => SmallVec::new(),
    }
}

fn extract_render_backdrop_filters(
    style: &ComputedStyle,
    rect: Rect,
    dark_mode: bool,
) -> SmallVec<[RenderFilterOp; 2]> {
    match &style.effects {
        Some(effects) if !effects.backdrop_filter.is_empty() => {
            extract_render_filter_ops(&effects.backdrop_filter, style, rect, dark_mode)
        }
        _ => SmallVec::new(),
    }
}

fn to_render_blend_mode(m: MixBlendMode) -> BlendMode {
    match m {
        MixBlendMode::Normal => BlendMode::Normal,
        MixBlendMode::Multiply => BlendMode::Multiply,
        MixBlendMode::Screen => BlendMode::Screen,
        MixBlendMode::Overlay => BlendMode::Overlay,
        MixBlendMode::Darken => BlendMode::Darken,
        MixBlendMode::Lighten => BlendMode::Lighten,
        MixBlendMode::ColorDodge => BlendMode::ColorDodge,
        MixBlendMode::ColorBurn => BlendMode::ColorBurn,
        MixBlendMode::HardLight => BlendMode::HardLight,
        MixBlendMode::SoftLight => BlendMode::SoftLight,
        MixBlendMode::Difference => BlendMode::Difference,
        MixBlendMode::Exclusion => BlendMode::Exclusion,
        MixBlendMode::Hue => BlendMode::Hue,
        MixBlendMode::Saturation => BlendMode::Saturation,
        MixBlendMode::Color => BlendMode::Color,
        MixBlendMode::Luminosity => BlendMode::Luminosity,
    }
}

fn resolve_clip_radius(r: &ClipRadius, basis: f32, closest: f32, farthest: f32) -> f32 {
    let eval = EvalContext::default();
    match r {
        ClipRadius::Length(lp) => lp.resolve_px(basis, &eval).max(0.0),
        ClipRadius::ClosestSide => closest.max(0.0),
        ClipRadius::FarthestSide => farthest.max(0.0),
    }
}

fn resolve_clip_path(cp: &ClipPathValue, rect: Rect) -> Option<ClipPathShape> {
    let eval = EvalContext::default();
    match cp {
        ClipPathValue::None => None,
        ClipPathValue::Inset {
            top,
            right,
            bottom,
            left,
            radii,
        } => {
            let t = top.resolve_px(rect.size.height, &eval);
            let r = right.resolve_px(rect.size.width, &eval);
            let b = bottom.resolve_px(rect.size.height, &eval);
            let l = left.resolve_px(rect.size.width, &eval);
            let w = (rect.size.width - l - r).max(0.0);
            let h = (rect.size.height - t - b).max(0.0);
            let basis = w.min(h);
            let resolved_radii = [
                radii[0].resolve_px(basis, &eval).max(0.0),
                radii[1].resolve_px(basis, &eval).max(0.0),
                radii[2].resolve_px(basis, &eval).max(0.0),
                radii[3].resolve_px(basis, &eval).max(0.0),
            ];
            Some(ClipPathShape::Inset {
                rect: Rect::new(rect.min_x() + l, rect.min_y() + t, w, h),
                radii: resolved_radii,
            })
        }
        ClipPathValue::Circle { radius, cx, cy } => {
            let lx = cx.resolve_px(rect.size.width, &eval);
            let ly = cy.resolve_px(rect.size.height, &eval);
            let center = Point::new(rect.min_x() + lx, rect.min_y() + ly);
            let dx_min = lx.abs().min((rect.size.width - lx).abs());
            let dy_min = ly.abs().min((rect.size.height - ly).abs());
            let dx_max = lx.abs().max((rect.size.width - lx).abs());
            let dy_max = ly.abs().max((rect.size.height - ly).abs());
            let basis = ((rect.size.width.powi(2) + rect.size.height.powi(2)) * 0.5).sqrt();
            let rad = resolve_clip_radius(radius, basis, dx_min.min(dy_min), dx_max.max(dy_max));
            Some(ClipPathShape::Circle {
                center,
                radius: rad,
            })
        }
        ClipPathValue::Ellipse { rx, ry, cx, cy } => {
            let lx = cx.resolve_px(rect.size.width, &eval);
            let ly = cy.resolve_px(rect.size.height, &eval);
            let center = Point::new(rect.min_x() + lx, rect.min_y() + ly);
            let dx_min = lx.abs().min((rect.size.width - lx).abs());
            let dy_min = ly.abs().min((rect.size.height - ly).abs());
            let dx_max = lx.abs().max((rect.size.width - lx).abs());
            let dy_max = ly.abs().max((rect.size.height - ly).abs());
            Some(ClipPathShape::Ellipse {
                center,
                rx: resolve_clip_radius(rx, rect.size.width, dx_min, dx_max),
                ry: resolve_clip_radius(ry, rect.size.height, dy_min, dy_max),
            })
        }
        ClipPathValue::Polygon { fill_odd, points } => {
            if points.len() < 3 {
                return None;
            }
            let resolved = points
                .iter()
                .map(|(px, py)| {
                    Point::new(
                        rect.min_x() + px.resolve_px(rect.size.width, &eval),
                        rect.min_y() + py.resolve_px(rect.size.height, &eval),
                    )
                })
                .collect();
            Some(ClipPathShape::Polygon {
                fill_odd: *fill_odd,
                points: resolved,
            })
        }
    }
}

fn extract_text_clip_color(style: &ComputedStyle, dark_mode: bool) -> Option<ColorRgba> {
    for layer in &style.visual.background_layers {
        let stops = match &layer.image {
            BackgroundImageValue::LinearGradient { stops, .. }
            | BackgroundImageValue::RadialGradient { stops, .. } => stops.as_slice(),
            _ => continue,
        };
        if !stops.is_empty() {
            // Prefer the midpoint stop (or first opaque stop) as a crisp representative fill.
            let mid = stops.len() / 2;
            let cand = stops[mid].color.resolve(style.text.color, dark_mode);
            if cand.a > 0 {
                return Some(cand);
            }
            for s in stops {
                let c = s.color.resolve(style.text.color, dark_mode);
                if c.a > 0 {
                    return Some(c);
                }
            }
        }
    }
    if style.visual.background_color.a > 0 {
        Some(style.visual.background_color)
    } else {
        None
    }
}

fn resolve_gradient_stops(
    stops: &[CssGradientStop],
    grad_len: f32,
    current_color: ColorRgba,
    dark_mode: bool,
) -> SmallVec<[ColorStop; 4]> {
    if stops.is_empty() {
        return SmallVec::new();
    }
    let eval = EvalContext::default();
    let n = stops.len();
    let len = grad_len.max(1.0);

    // CSS Images L3 §3.4.3 (Color Stop Fixup):
    // 1. Resolve explicit `<length-percentage>` stop positions against the gradient line length.
    let mut positions: SmallVec<[Option<f32>; 8]> = stops
        .iter()
        .map(|s| {
            s.position
                .as_ref()
                .map(|lp| lp.resolve_px(len, &eval) / len)
        })
        .collect();

    // 2. Default first stop to 0% and last stop to 100% if unspecified.
    if positions[0].is_none() {
        positions[0] = Some(0.0);
    }
    if n > 1 && positions[n - 1].is_none() {
        positions[n - 1] = Some(1.0);
    }

    // 3. Enforce monotonic non-decreasing stop positions (`calc(100% - 1px), color 0` -> hard stop).
    let mut max_so_far = positions[0].unwrap_or(0.0);
    for p in positions.iter_mut().skip(1).flatten() {
        if *p < max_so_far {
            *p = max_so_far;
        } else {
            max_so_far = *p;
        }
    }

    // 4. Linearly distribute any runs of unspecified stop positions.
    let mut i = 0;
    while i < n {
        if positions[i].is_none() {
            let start = i - 1;
            let mut end = i + 1;
            while end < n && positions[end].is_none() {
                end += 1;
            }
            let p0 = positions[start].unwrap_or(0.0);
            let p1 = positions[end].unwrap_or(1.0);
            let span = (end - start) as f32;
            for k in (start + 1)..end {
                positions[k] = Some(p0 + (p1 - p0) * ((k - start) as f32 / span));
            }
            i = end;
        } else {
            i += 1;
        }
    }

    let mut out: SmallVec<[ColorStop; 4]> = SmallVec::with_capacity(n + 2);
    for (idx, stop) in stops.iter().enumerate() {
        let color = to_rgba8(stop.color.resolve(current_color, dark_mode));
        let offset = positions[idx].unwrap_or(0.0).clamp(0.0, 1.0);
        out.push(ColorStop { offset, color });
    }
    // CSS Images L3 §3.4.3: Before the first stop the gradient has the color of the first stop,
    // and after the last stop it has the color of the last stop. Explicit 0.0 / 1.0 clamping
    // stops ensure GPU ramp builders (e.g. vello_encoding) preserve shifted hard-stop transitions
    // like `linear-gradient(180deg, transparent calc(100% - 1px), red 0)`.
    if let Some(first) = out.first().copied() {
        if first.offset > 0.0 {
            out.insert(
                0,
                ColorStop {
                    offset: 0.0,
                    color: first.color,
                },
            );
        }
    }
    if let Some(last) = out.last().copied() {
        if last.offset < 1.0 {
            out.push(ColorStop {
                offset: 1.0,
                color: last.color,
            });
        }
    }
    out
}

fn linear_gradient_endpoints(rect: Rect, angle_deg: f32) -> (Point, Point) {
    let rad = angle_deg.to_radians();
    let dx = rad.sin();
    let dy = -rad.cos();
    let half_len = (rect.size.width * dx.abs() + rect.size.height * dy.abs()) * 0.5;
    let cx = rect.min_x() + rect.size.width * 0.5;
    let cy = rect.min_y() + rect.size.height * 0.5;
    (
        Point::new(cx - dx * half_len, cy - dy * half_len),
        Point::new(cx + dx * half_len, cy + dy * half_len),
    )
}

fn to_timing_curve(t: &TimingFunction) -> TimingCurve {
    match t {
        TimingFunction::Linear => TimingCurve::Linear,
        TimingFunction::Ease => TimingCurve::Ease,
        TimingFunction::EaseIn => TimingCurve::EaseIn,
        TimingFunction::EaseOut => TimingCurve::EaseOut,
        TimingFunction::EaseInOut => TimingCurve::EaseInOut,
        TimingFunction::CubicBezier(x1, y1, x2, y2) => {
            TimingCurve::CubicBezier(*x1 as f32, *y1 as f32, *x2 as f32, *y2 as f32)
        }
        TimingFunction::Steps(c, pos) => TimingCurve::Steps(
            *c,
            matches!(
                pos,
                rix_css::StepPositionMode::Start
                    | rix_css::StepPositionMode::JumpStart
                    | rix_css::StepPositionMode::JumpBoth
            ),
        ),
        TimingFunction::LinearStops(_) => TimingCurve::Linear,
    }
}

fn to_anim_direction(d: AnimationDirectionMode) -> AnimDirection {
    match d {
        AnimationDirectionMode::Normal => AnimDirection::Normal,
        AnimationDirectionMode::Reverse => AnimDirection::Reverse,
        AnimationDirectionMode::Alternate => AnimDirection::Alternate,
        AnimationDirectionMode::AlternateReverse => AnimDirection::AlternateReverse,
    }
}

fn to_anim_fill(f: AnimationFillModeKind) -> AnimFillMode {
    match f {
        AnimationFillModeKind::None => AnimFillMode::None,
        AnimationFillModeKind::Forwards => AnimFillMode::Forwards,
        AnimationFillModeKind::Backwards => AnimFillMode::Backwards,
        AnimationFillModeKind::Both => AnimFillMode::Both,
    }
}

fn extract_transform_keyframes(
    rule: &KeyframesRule,
    rect: Rect,
    base_matrix: [f32; 6],
) -> Vec<TransformKeyframe> {
    let mut kfs = Vec::new();
    for step in &rule.steps {
        for decl in &step.declarations {
            if decl.property == PropertyIdKind::Transform {
                if let PropertyValue::Transform(ops) = &decl.value {
                    let matrix = compose_transform_matrix(ops, rect);
                    for &off in &step.offsets {
                        kfs.push(TransformKeyframe {
                            offset: off.clamp(0.0, 1.0),
                            matrix,
                        });
                    }
                }
            }
        }
    }
    if kfs.is_empty() {
        return kfs;
    }
    kfs.sort_by(|a, b| a.offset.total_cmp(&b.offset));
    if kfs.first().is_some_and(|k| k.offset > 1e-4) {
        kfs.insert(
            0,
            TransformKeyframe {
                offset: 0.0,
                matrix: base_matrix,
            },
        );
    }
    if kfs.last().is_some_and(|k| k.offset < 1.0 - 1e-4) {
        kfs.push(TransformKeyframe {
            offset: 1.0,
            matrix: base_matrix,
        });
    }
    kfs
}

fn extract_opacity_keyframes(rule: &KeyframesRule, base_opacity: f32) -> Vec<OpacityKeyframe> {
    let mut kfs = Vec::new();
    for step in &rule.steps {
        for decl in &step.declarations {
            if decl.property == PropertyIdKind::Opacity {
                if let PropertyValue::Number(n) = &decl.value {
                    let opacity = n.clamp(0.0, 1.0);
                    for &off in &step.offsets {
                        kfs.push(OpacityKeyframe {
                            offset: off.clamp(0.0, 1.0),
                            opacity,
                        });
                    }
                }
            }
        }
    }
    if kfs.is_empty() {
        return kfs;
    }
    kfs.sort_by(|a, b| a.offset.total_cmp(&b.offset));
    if kfs.first().is_some_and(|k| k.offset > 1e-4) {
        kfs.insert(
            0,
            OpacityKeyframe {
                offset: 0.0,
                opacity: base_opacity.clamp(0.0, 1.0),
            },
        );
    }
    if kfs.last().is_some_and(|k| k.offset < 1.0 - 1e-4) {
        kfs.push(OpacityKeyframe {
            offset: 1.0,
            opacity: base_opacity.clamp(0.0, 1.0),
        });
    }
    kfs
}

fn format_list_marker(mode: ListStyleTypeMode, ordinal: usize) -> String {
    let n = ordinal.max(1);
    match mode {
        ListStyleTypeMode::Decimal => format!("{n}."),
        ListStyleTypeMode::DecimalLeadingZero => {
            if n < 10 {
                format!("0{n}.")
            } else {
                format!("{n}.")
            }
        }
        ListStyleTypeMode::LowerAlpha => format!("{}.", to_alphabetic(n, false)),
        ListStyleTypeMode::UpperAlpha => format!("{}.", to_alphabetic(n, true)),
        ListStyleTypeMode::LowerRoman => format!("{}.", to_roman(n, false)),
        ListStyleTypeMode::UpperRoman => format!("{}.", to_roman(n, true)),
        ListStyleTypeMode::DisclosureOpen => "\u{25BE}".to_string(),
        ListStyleTypeMode::DisclosureClosed => "\u{25B8}".to_string(),
        _ => String::new(),
    }
}

fn to_alphabetic(mut n: usize, upper: bool) -> String {
    let mut chars = Vec::new();
    let base = if upper { b'A' } else { b'a' };
    while n > 0 {
        n -= 1;
        chars.push((base + (n % 26) as u8) as char);
        n /= 26;
    }
    chars.into_iter().rev().collect()
}

fn to_roman(mut n: usize, upper: bool) -> String {
    if n > 3999 {
        return n.to_string();
    }
    const TABLE: &[(usize, &str)] = &[
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut out = String::new();
    for &(val, sym) in TABLE {
        while n >= val {
            out.push_str(sym);
            n -= val;
        }
    }
    if upper {
        out.to_ascii_uppercase()
    } else {
        out
    }
}

fn hash_svg_path_id(d: &str, w: f32, h: f32, transform: &[f64; 6]) -> PathResourceId {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in w.to_bits().to_le_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    for b in h.to_bits().to_le_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    for &c in transform {
        for b in c.to_bits().to_le_bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    for &b in d.as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    PathResourceId(hash.max(1))
}
