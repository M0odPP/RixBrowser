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

use rix_core::{Point, Rect, Vec2};
use rix_css::{
    ColorRgba, ComputedStyle, EvalContext, FilterOp as CssFilterOp, LengthPercentageAuto,
    LineStyle, OverflowMode, PositionMode, TextDecorationLine, TransformOp, VisibilityMode,
    ZIndexValue,
};
use rix_ipc::{FontResourceId, ImageId, PathResourceId, PipelineId, ScrollId, SpatialNodeId};
use rix_render::display_list::{
    BlendMode, DisplayItem, DisplayList, FilterOp as RenderFilterOp, GlyphInstance, ImageSampling,
    Rgba8, SpatialNodeKind, StickyMargins,
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
    let mut list = DisplayList::new(tree.content_bounds);
    let Some(root) = &tree.root else {
        return list;
    };

    let mut painter = Painter {
        list: &mut list,
        font_db,
        assets,
        pipeline,
        resources,
        dark_mode,
        registered_fonts: Vec::new(),
        registered_images: HashSet::new(),
        next_scroll_id: 1,
    };

    let root_spatial = SpatialNodeId(0);
    painter.paint_stacking_context(root, Point::ZERO, root_spatial, None);
    list
}

struct Painter<'a> {
    list: &'a mut DisplayList,
    font_db: &'a FontDatabase,
    assets: &'a AssetRegistry,
    pipeline: PipelineId,
    resources: &'a mut ResourceRegistry,
    dark_mode: bool,
    registered_fonts: Vec<(FontId, FontResourceId)>,
    registered_images: HashSet<ImageId>,
    next_scroll_id: u64,
}

#[derive(Clone, Copy)]
struct CollectedStackingChild<'f> {
    frag: &'f Fragment,
    parent_origin: Point,
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
        parent_spatial: SpatialNodeId,
        scroll_parent: Option<SpatialNodeId>,
    ) {
        let abs_rect = frag.absolute_border_rect(parent_origin);
        let style = &frag.style;

        let mut active_spatial = parent_spatial;
        let mut active_scroll_parent = scroll_parent;
        let mut pushed_stacking_layer = false;
        let mut pushed_clip = false;

        // 1. Sticky spatial node (`position: sticky`)
        if style.position == PositionMode::Sticky {
            if let Some(sp) = active_scroll_parent {
                let eval = EvalContext::default();
                let inset = &style.box_model.inset;
                let margins = StickyMargins {
                    top: match &inset.top {
                        LengthPercentageAuto::Value(lp) => {
                            Some(lp.resolve_px(abs_rect.size.height, &eval))
                        }
                        LengthPercentageAuto::Auto => None,
                    },
                    bottom: match &inset.bottom {
                        LengthPercentageAuto::Value(lp) => {
                            Some(lp.resolve_px(abs_rect.size.height, &eval))
                        }
                        LengthPercentageAuto::Auto => None,
                    },
                    left: match &inset.left {
                        LengthPercentageAuto::Value(lp) => {
                            Some(lp.resolve_px(abs_rect.size.width, &eval))
                        }
                        LengthPercentageAuto::Auto => None,
                    },
                    right: match &inset.right {
                        LengthPercentageAuto::Value(lp) => {
                            Some(lp.resolve_px(abs_rect.size.width, &eval))
                        }
                        LengthPercentageAuto::Auto => None,
                    },
                };
                active_spatial = self.list.spatial_tree.add_node(
                    Some(active_spatial),
                    SpatialNodeKind::StickyFrame {
                        bounds: abs_rect,
                        margins,
                        scroll_parent: sp,
                    },
                );
            }
        }

        // 2. Transform spatial node (`transform: ...` + `transform-origin: ...`)
        let has_transform = style
            .effects
            .as_ref()
            .is_some_and(|e| !e.transform.is_empty());
        if has_transform {
            if let Some(effects) = &style.effects {
                let eval = EvalContext::default();
                let matrix = compose_transform_matrix(&effects.transform, abs_rect);
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
                active_spatial = self.list.spatial_tree.add_node(
                    Some(active_spatial),
                    SpatialNodeKind::Transform {
                        matrix,
                        origin,
                        animation_id: None,
                    },
                );
            }
        }

        // 3. Finite stacking-context layer (`opacity < 1`, `filter`, or new spatial node).
        let filters = extract_render_filters(style);
        let needs_layer =
            style.opacity < 0.999 || !filters.is_empty() || active_spatial != parent_spatial;
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
                    mix_blend: BlendMode::Normal,
                    filters,
                });
                pushed_stacking_layer = true;
            }
        }

        // Step 1 (Appendix E): Paint background, shadows, and borders of this root.
        self.paint_box_decorations(frag, abs_rect);

        // 4. Overflow clip / ScrollFrame for children.
        let clips_x = style.overflow.x != OverflowMode::Visible;
        let clips_y = style.overflow.y != OverflowMode::Visible;
        let is_scroll_container =
            matches!(style.overflow.x, OverflowMode::Scroll | OverflowMode::Auto)
                || matches!(style.overflow.y, OverflowMode::Scroll | OverflowMode::Auto);
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
            pushed_clip = true;
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
            &mut neg_z,
            &mut zero_z,
            &mut pos_z,
            &mut order_counter,
        );

        // Step 2 (Appendix E): Child stacking contexts with negative z-index (ascending).
        neg_z.sort_by_key(|c| (c.z_index, c.order));
        for child in neg_z {
            self.paint_stacking_context(
                child.frag,
                child.parent_origin,
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

        // Step 6 (Appendix E): Positioned z-index: auto | 0 and non-positioned stacking contexts.
        for child in zero_z {
            self.paint_stacking_context(
                child.frag,
                child.parent_origin,
                active_spatial,
                active_scroll_parent,
            );
        }

        // Step 7 (Appendix E): Child stacking contexts with positive z-index (ascending).
        pos_z.sort_by_key(|c| (c.z_index, c.order));
        for child in pos_z {
            self.paint_stacking_context(
                child.frag,
                child.parent_origin,
                active_spatial,
                active_scroll_parent,
            );
        }

        if pushed_clip {
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
        for child in parent.children.iter() {
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
                        self.paint_stacking_context(child, parent_origin, spatial, scroll_parent);
                    } else if !is_inline_level_fragment(child) {
                        self.paint_flow_phase(
                            child,
                            child_abs.origin,
                            spatial,
                            scroll_parent,
                            FlowPhase::Floats,
                        );
                    }
                }
                FlowPhase::Inlines => {
                    if child.is_float {
                        continue;
                    }
                    if is_inline_level_fragment(child) {
                        self.paint_inline_subtree(child, child_abs, spatial, scroll_parent);
                    } else {
                        self.paint_flow_phase(
                            child,
                            child_abs.origin,
                            spatial,
                            scroll_parent,
                            FlowPhase::Inlines,
                        );
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
        self.paint_box_decorations(frag, abs_rect);

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
                self.paint_box_decorations(frag, abs_rect);
            }
            FragmentKind::TextRun(_)
            | FragmentKind::Image { .. }
            | FragmentKind::Svg { .. }
            | FragmentKind::FormControl { .. }
            | FragmentKind::ExternalSurface { .. } => {
                self.paint_box_decorations(frag, abs_rect);
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
                    spatial,
                    scroll_parent,
                );
            }
        }
    }

    fn paint_box_decorations(&mut self, frag: &Fragment, abs_rect: Rect) {
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

        // Solid background fill.
        let bg = style.visual.background_color;
        if bg.a > 0 {
            self.list.push(DisplayItem::Rect {
                bounds: abs_rect,
                color: to_rgba8(bg),
                radii,
            });
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
                if run.glyphs.is_empty() || run.color.a == 0 {
                    return;
                }
                let Some(font_res_id) = self.register_font(run.font_id) else {
                    return;
                };
                let baseline_y = abs_rect.min_y() + run.baseline_y;
                let instances: Vec<GlyphInstance> = run
                    .glyphs
                    .iter()
                    .map(|g| GlyphInstance {
                        glyph_id: g.glyph_id,
                        x: abs_rect.min_x() + g.x,
                        y: baseline_y + g.y,
                    })
                    .collect();
                let color = to_rgba8(run.color);
                self.list
                    .push_glyph_run(abs_rect, font_res_id, run.font_size, color, &instances);

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
                    bez.apply_affine(xf);
                    let path_id = hash_svg_path_id(
                        &entry.path_d,
                        content_abs.size.width,
                        content_abs.size.height,
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
        // Provide clean fallback chrome if the author did not set explicit background/border.
        if frag.style.visual.background_color.a == 0 {
            let default_bg = match kind {
                FormControlKind::Button => Rgba8::new(236, 238, 242, 255),
                _ => Rgba8::new(255, 255, 255, 255),
            };
            self.list.push(DisplayItem::Rect {
                bounds: abs_rect,
                color: default_bg,
                radii: [4.0; 4],
            });
        }
        if frag.border.horizontal() == 0.0 && frag.border.vertical() == 0.0 {
            self.list.push(DisplayItem::Border {
                bounds: abs_rect,
                widths: [1.0; 4],
                colors: [Rgba8::new(160, 166, 178, 255); 4],
                radii: [4.0; 4],
            });
        }
        let display_text = if !text.is_empty() { text } else { placeholder };
        if display_text.is_empty() {
            return;
        }
        let font_id = crate::ifc::resolve_font_for_style(self.font_db, None, &frag.style);
        let Some(font_res_id) = self.register_font(font_id) else {
            return;
        };
        let font_size = frag.style.text.font_size_px.max(11.0);
        let shaper = rix_text::TextShaper::new(self.font_db);
        let runs = shaper.shape_text(display_text, font_id, font_size);
        let text_w: f32 = runs.iter().map(|r| r.width).sum();
        let start_x = match kind {
            FormControlKind::Button => {
                content_abs.min_x() + ((content_abs.size.width - text_w) * 0.5).max(4.0)
            }
            _ => content_abs.min_x() + 6.0,
        };
        let baseline_y = content_abs.min_y() + (content_abs.size.height + font_size * 0.72) * 0.5;
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
            let color = if text.is_empty() {
                Rgba8::new(130, 136, 148, 255)
            } else {
                to_rgba8(frag.style.text.color)
            };
            self.list
                .push_glyph_run(content_abs, font_res_id, font_size, color, &instances);
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
        collect_stacking_children(child, child_abs.origin, neg_z, zero_z, pos_z, order);
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
        if !effects.transform.is_empty() || !effects.filter.is_empty() {
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

fn extract_render_filters(style: &ComputedStyle) -> SmallVec<[RenderFilterOp; 2]> {
    let mut out = SmallVec::new();
    let eval = EvalContext::default();
    if let Some(effects) = &style.effects {
        for op in &effects.filter {
            match op {
                CssFilterOp::Blur(lp) => {
                    out.push(RenderFilterOp::Blur(lp.resolve_px(0.0, &eval).max(0.0)));
                }
                CssFilterOp::Opacity(a) => out.push(RenderFilterOp::Opacity(*a)),
                CssFilterOp::Brightness(b) => out.push(RenderFilterOp::Brightness(*b)),
                CssFilterOp::Contrast(c) => out.push(RenderFilterOp::Contrast(*c)),
                CssFilterOp::Grayscale(g) => out.push(RenderFilterOp::Grayscale(*g)),
                _ => {}
            }
        }
    }
    out
}

fn hash_svg_path_id(d: &str, w: f32, h: f32) -> PathResourceId {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in w.to_bits().to_le_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    for b in h.to_bits().to_le_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    for &b in d.as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    PathResourceId(hash.max(1))
}
