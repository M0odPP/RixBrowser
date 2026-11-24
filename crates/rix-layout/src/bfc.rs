//! Block Formatting Context (BFC): CSS 2.1 collapsing vertical margins,
//! `float: left/right` exclusion bands, `clear`, BFC float containment,
//! `position: relative`, and out-of-flow `position: absolute | fixed` placement.

use std::sync::Arc;

use rix_core::{Point, Rect, Size};
use rix_css::{ClearMode, FloatMode, LengthPercentageAuto, PositionMode};

use crate::box_tree::{BoxId, BoxKind};
use crate::fragment::{
    layout_box, measure_content_intrinsic_widths, resolve_size_with_intrinsic,
    size_value_needs_intrinsic, BoxMetrics, ConstraintSpace, EdgeInsets, Fragment, FragmentKind,
    LayoutContext,
};
use crate::ifc::layout_ifc;
use crate::position::{offset_pending_oof, resolve_oof_for_container, PendingOof};

/// Single placed float exclusion rectangle in BFC content-area coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatBand {
    pub rect: Rect,
    pub is_left: bool,
}

/// Active float exclusion tracker for a Block Formatting Context.
#[derive(Debug, Default, Clone)]
pub struct FloatContext {
    floats: Vec<FloatBand>,
}

impl FloatContext {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.floats.is_empty()
    }

    /// Returns `(left_edge, right_edge)` available horizontally across vertical band `[y, y + height)`.
    #[must_use]
    pub fn available_band(&self, y: f32, height: f32, container_width: f32) -> (f32, f32) {
        let y_end = y + height.max(0.001);
        let mut left = 0.0f32;
        let mut right = container_width.max(0.0);
        for f in &self.floats {
            if f.rect.max_y() > y && f.rect.min_y() < y_end {
                if f.is_left {
                    left = left.max(f.rect.max_x());
                } else {
                    right = right.min(f.rect.min_x());
                }
            }
        }
        (left, right.max(left))
    }

    /// Returns the lowest `y` coordinate that satisfies `clear` (`left`, `right`, `both`).
    #[must_use]
    pub fn clear_y(&self, clear: ClearMode, current_y: f32) -> f32 {
        let mut y = current_y;
        for f in &self.floats {
            let matches = match clear {
                ClearMode::None => false,
                ClearMode::Left | ClearMode::InlineStart => f.is_left,
                ClearMode::Right | ClearMode::InlineEnd => !f.is_left,
                ClearMode::Both => true,
            };
            if matches {
                y = y.max(f.rect.max_y());
            }
        }
        y
    }

    /// Places a float margin-box of `size` starting at or below `start_y`, shifting
    /// downward past existing floats whenever the available band is too narrow.
    pub fn place_float(
        &mut self,
        is_left: bool,
        margin_box_size: Size,
        start_y: f32,
        container_width: f32,
    ) -> Point {
        let mut y = start_y;
        // CSS 2.1 §9.5.1 rule 5: top of float cannot be higher than any earlier float.
        if let Some(last) = self.floats.last() {
            y = y.max(last.rect.min_y());
        }

        loop {
            let (left_edge, right_edge) =
                self.available_band(y, margin_box_size.height.max(1.0), container_width);
            let avail = right_edge - left_edge;
            let fits = avail + 0.01 >= margin_box_size.width
                || (left_edge <= 0.01 && right_edge >= container_width - 0.01);
            if fits {
                let x = if is_left {
                    left_edge
                } else {
                    (right_edge - margin_box_size.width).max(left_edge)
                };
                let rect = Rect::new(x, y, margin_box_size.width, margin_box_size.height);
                self.floats.push(FloatBand { rect, is_left });
                return Point::new(x, y);
            }

            // Advance `y` to the bottom of the highest overlapping float.
            let next_y = self
                .floats
                .iter()
                .filter(|f| {
                    f.rect.max_y() > y && f.rect.min_y() < y + margin_box_size.height.max(1.0)
                })
                .map(|f| f.rect.max_y())
                .fold(f32::INFINITY, f32::min);
            if !next_y.is_finite() || next_y <= y {
                let x = if is_left {
                    0.0
                } else {
                    (container_width - margin_box_size.width).max(0.0)
                };
                let rect = Rect::new(x, y, margin_box_size.width, margin_box_size.height);
                self.floats.push(FloatBand { rect, is_left });
                return Point::new(x, y);
            }
            y = next_y;
        }
    }

    /// Lowest `max_y` of all floats in this BFC.
    #[must_use]
    pub fn max_float_bottom(&self) -> f32 {
        self.floats
            .iter()
            .map(|f| f.rect.max_y())
            .fold(0.0f32, f32::max)
    }

    /// Translates all float rectangles by `(dx, dy)` when passing into/out of a child BFC participant.
    #[must_use]
    pub fn translated(&self, dx: f32, dy: f32) -> Self {
        Self {
            floats: self
                .floats
                .iter()
                .map(|f| FloatBand {
                    rect: Rect::new(
                        f.rect.origin.x + dx,
                        f.rect.origin.y + dy,
                        f.rect.size.width,
                        f.rect.size.height,
                    ),
                    is_left: f.is_left,
                })
                .collect(),
        }
    }
}

/// CSS 2.1 §8.3.1 margin collapsing accumulator.
#[derive(Debug, Clone, Copy, Default)]
pub struct MarginCollapse {
    pos: f32,
    neg: f32,
}

impl MarginCollapse {
    #[inline]
    pub fn push(&mut self, margin: f32) {
        if margin >= 0.0 {
            self.pos = self.pos.max(margin);
        } else {
            self.neg = self.neg.min(margin);
        }
    }

    #[inline]
    #[must_use]
    pub fn resolved(self) -> f32 {
        self.pos + self.neg
    }
}

/// Combines two adjoining vertical margins per CSS 2.1 §8.3.1.
#[inline]
#[must_use]
pub fn collapse_margins(a: f32, b: f32) -> f32 {
    let mut acc = MarginCollapse::default();
    acc.push(a);
    acc.push(b);
    acc.resolved()
}

/// Lays out a block container box establishing (or participating in) a BFC.
pub fn layout_bfc_root(
    ctx: &mut LayoutContext<'_>,
    box_id: BoxId,
    space: ConstraintSpace,
) -> Fragment {
    let mut floats = FloatContext::new();
    layout_block_box(ctx, box_id, space, &mut floats)
}

/// Lays out a block-level or inline-block container within an active `FloatContext`.
pub fn layout_block_box(
    ctx: &mut LayoutContext<'_>,
    box_id: BoxId,
    space: ConstraintSpace,
    parent_floats: &mut FloatContext,
) -> Fragment {
    let b = ctx.tree.get(box_id);
    let style = Arc::clone(&b.style);
    let eval = ctx.eval_context_for(&style, &space);
    let metrics = BoxMetrics::resolve_with_eval(&style, &space, eval);
    let pb_w = metrics.padding.horizontal() + metrics.border.horizontal();
    let pb_h = metrics.padding.vertical() + metrics.border.vertical();

    // Resolve horizontal content width & auto margins.
    let oof_both_h_insets = b.is_out_of_flow
        && matches!(style.box_model.inset.left, LengthPercentageAuto::Value(_))
        && matches!(style.box_model.inset.right, LengthPercentageAuto::Value(_));
    let is_vert_box = style.text.writing_mode.is_vertical();
    let shrink_to_fit = b.is_float
        || (b.is_out_of_flow && !oof_both_h_insets)
        || matches!(b.kind, BoxKind::InlineBlock { .. })
        || (is_vert_box && space.definite_width_override.is_none());
    let is_bb = style.box_sizing == rix_css::BoxSizingMode::BorderBox;
    let avail_c = (space.available_width - metrics.margin.horizontal() - pb_w).max(0.0);

    let need_intr_w = shrink_to_fit
        || size_value_needs_intrinsic(&style.box_model.width)
        || size_value_needs_intrinsic(&style.box_model.min_width)
        || size_value_needs_intrinsic(&style.box_model.max_width);
    let (min_c, max_c) = if need_intr_w {
        measure_content_intrinsic_widths(ctx, box_id, space)
    } else {
        (0.0, 0.0)
    };
    let stf_c = max_c.min(avail_c).max(min_c);
    let intr_w_fn = |basis: &rix_css::values_math::CalcSizeBasis| -> f32 {
        match basis {
            rix_css::values_math::CalcSizeBasis::MinContent => min_c,
            rix_css::values_math::CalcSizeBasis::MaxContent => max_c,
            rix_css::values_math::CalcSizeBasis::FitContent => stf_c,
            rix_css::values_math::CalcSizeBasis::Stretch => avail_c,
            _ => {
                if shrink_to_fit {
                    stf_c
                } else {
                    avail_c
                }
            }
        }
    };
    let eff_min_w = resolve_size_with_intrinsic(
        &style.box_model.min_width,
        Some(space.containing_block_width),
        pb_w,
        metrics.margin.horizontal(),
        is_bb,
        &eval,
        intr_w_fn,
    )
    .unwrap_or(metrics.min_width);
    let eff_max_w = resolve_size_with_intrinsic(
        &style.box_model.max_width,
        Some(space.containing_block_width),
        pb_w,
        metrics.margin.horizontal(),
        is_bb,
        &eval,
        intr_w_fn,
    )
    .unwrap_or(metrics.max_width);
    let clamp_w = |w: f32| w.min(eff_max_w).max(eff_min_w).max(0.0);

    let resolved_spec_w = space
        .definite_width_override
        .or(metrics.specified_width)
        .or_else(|| {
            resolve_size_with_intrinsic(
                &style.box_model.width,
                Some(space.containing_block_width),
                pb_w,
                metrics.margin.horizontal(),
                is_bb,
                &eval,
                intr_w_fn,
            )
        });

    let (mut content_width, resolved_margin_left, resolved_margin_right) =
        if let Some(spec_w) = resolved_spec_w {
            let cw = clamp_w(spec_w);
            let remaining = (space.available_width - cw - pb_w).max(0.0);
            let (ml, mr) = match (metrics.margin_left_auto, metrics.margin_right_auto) {
                (true, true) => (remaining * 0.5, remaining * 0.5),
                (true, false) => (
                    (remaining - metrics.margin.right).max(0.0),
                    metrics.margin.right,
                ),
                (false, true) => (
                    metrics.margin.left,
                    (remaining - metrics.margin.left).max(0.0),
                ),
                (false, false) => (metrics.margin.left, metrics.margin.right),
            };
            (cw, ml, mr)
        } else if shrink_to_fit {
            (clamp_w(stf_c), metrics.margin.left, metrics.margin.right)
        } else {
            let cw = clamp_w(avail_c);
            let (ml, mr) = if metrics.margin_left_auto || metrics.margin_right_auto {
                let remaining = (space.available_width - cw - pb_w).max(0.0);
                match (metrics.margin_left_auto, metrics.margin_right_auto) {
                    (true, true) => (remaining * 0.5, remaining * 0.5),
                    (true, false) => (
                        (remaining - metrics.margin.right).max(0.0),
                        metrics.margin.right,
                    ),
                    (false, true) => (
                        metrics.margin.left,
                        (remaining - metrics.margin.left).max(0.0),
                    ),
                    (false, false) => (metrics.margin.left, metrics.margin.right),
                }
            } else {
                (metrics.margin.left, metrics.margin.right)
            };
            (cw, ml, mr)
        };

    let resolved_margin = EdgeInsets {
        top: metrics.margin.top,
        right: resolved_margin_right,
        bottom: metrics.margin.bottom,
        left: resolved_margin_left,
    };

    let content_origin_x = metrics.border.left + metrics.padding.left;
    let content_origin_y = metrics.border.top + metrics.padding.top;
    // Definite height for child `%` resolution: explicit `height`, stretched OOF (`top`+`bottom`),
    // or definite `width` combined with `aspect-ratio` (CSS Sizing L4 / Position L3 §5.1).
    let oof_both_v_insets = b.is_out_of_flow
        && matches!(style.box_model.inset.top, LengthPercentageAuto::Value(_))
        && matches!(style.box_model.inset.bottom, LengthPercentageAuto::Value(_));
    let is_table_cell = matches!(
        ctx.tree.get(box_id).kind,
        BoxKind::TableCell { .. } | BoxKind::AnonymousTableCell
    );
    // WHATWG Quirks Mode §3.3: The table cell height box sizing quirk treats CSS `height` on
    // `table-cell` elements as `border-box` in Quirks Mode regardless of `box-sizing`.
    let cell_spec_h = if is_table_cell
        && ctx.tree.quirks_mode == 2
        && style.box_sizing == rix_css::BoxSizingMode::ContentBox
    {
        metrics.specified_height.map(|h| (h - pb_h).max(0.0))
    } else {
        metrics.specified_height
    };
    let definite_height = if space.ignore_specified_height {
        None
    } else {
        space.definite_height_override.or(cell_spec_h).or_else(|| {
            if oof_both_v_insets && matches!(style.box_model.height, rix_css::SizeValue::Auto) {
                space.available_height.map(|ah| {
                    (ah - metrics.margin.vertical() - pb_h)
                        .max(0.0)
                        .min(metrics.max_height)
                        .max(metrics.min_height)
                })
            } else if resolved_spec_w.is_some()
                && matches!(style.box_model.height, rix_css::SizeValue::Auto)
            {
                style
                    .effects
                    .as_ref()
                    .and_then(|e| e.aspect_ratio_math.as_ref())
                    .and_then(|(num, den)| {
                        let n = num.eval_scalar(&eval, None);
                        let d = den.as_ref().map_or(1.0, |x| x.eval_scalar(&eval, None));
                        let ratio = n / d;
                        if ratio.is_finite() && ratio > 0.0 {
                            let box_w = if is_bb {
                                content_width + pb_w
                            } else {
                                content_width
                            };
                            let box_h = box_w / ratio as f32;
                            let ch = if is_bb {
                                (box_h - pb_h).max(0.0)
                            } else {
                                box_h
                            };
                            Some(ch.min(metrics.max_height).max(metrics.min_height).max(0.0))
                        } else {
                            None
                        }
                    })
            } else {
                None
            }
        })
    };
    let child_space = space.with_available(content_width, definite_height);

    let children_ids = ctx.tree.get(box_id).children.clone();
    let establishes_bfc = ctx.tree.get(box_id).establishes_bfc;

    // Own float context for BFC roots; otherwise borrow translated parent floats.
    let mut local_floats = if establishes_bfc {
        FloatContext::new()
    } else {
        parent_floats.translated(-content_origin_x, -content_origin_y)
    };

    let has_in_flow_blocks = children_ids.iter().any(|&cid| {
        let c = ctx.tree.get(cid);
        c.is_block_level() || c.is_float
    });

    let mut placed_children: Vec<Fragment> = Vec::with_capacity(children_ids.len());
    let my_oof_start = ctx.pending_oof.len();
    let mut collapsed_top = resolved_margin.top;
    let mut collapsed_bottom = resolved_margin.bottom;

    let content_height = if children_ids.is_empty() {
        0.0
    } else if !has_in_flow_blocks {
        // Pure Inline Formatting Context inside this block container!
        // `layout_ifc` records exact static positions for both direct and inline-nested OOF boxes.
        if is_vert_box {
            let inline_avail = definite_height
                .or_else(|| {
                    space
                        .available_height
                        .map(|ah| (ah - metrics.margin.vertical() - pb_h).max(0.0))
                })
                .unwrap_or(f32::INFINITY);
            let (vert_blk_w, vert_inline_h) =
                crate::ifc::measure_vertical_ifc_dims(ctx, box_id, child_space, inline_avail);
            let ifc_space = child_space.with_available(vert_inline_h.max(1.0), None);
            let mut ifc_result = layout_ifc(
                ctx,
                box_id,
                ifc_space,
                &local_floats,
                Point::new(content_origin_x, content_origin_y),
            );
            if resolved_spec_w.is_none() {
                content_width = clamp_w(vert_blk_w);
            }
            for lf in &mut ifc_result.fragments {
                lf.border_rect.size.width = vert_blk_w.min(content_width.max(vert_blk_w));
                lf.border_rect.size.height = vert_inline_h;
                lf.content_rect.size = lf.border_rect.size;
                lf.overflow_rect = Rect::new(
                    0.0,
                    0.0,
                    lf.border_rect.size.width,
                    lf.border_rect.size.height,
                );
            }
            placed_children.extend(ifc_result.fragments);
            vert_inline_h
        } else {
            let ifc_result = layout_ifc(
                ctx,
                box_id,
                child_space,
                &local_floats,
                Point::new(content_origin_x, content_origin_y),
            );
            placed_children.extend(ifc_result.fragments);
            ifc_result.height
        }
    } else {
        let can_collapse_top_with_first =
            !establishes_bfc && metrics.border.top == 0.0 && metrics.padding.top == 0.0;
        let can_collapse_bottom_with_last = !establishes_bfc
            && metrics.border.bottom == 0.0
            && metrics.padding.bottom == 0.0
            && metrics.specified_height.is_none();

        let mut cursor_y = 0.0f32;
        let mut prev_margin_bottom: Option<f32> = None;
        let mut saw_in_flow_child = false;

        for cid in children_ids {
            let child_box = ctx.tree.get(cid);
            if child_box.is_out_of_flow {
                let static_y = content_origin_y + cursor_y + prev_margin_bottom.unwrap_or(0.0);
                ctx.pending_oof.push(PendingOof {
                    box_id: cid,
                    static_pos: Point::new(content_origin_x, static_y),
                    is_fixed: child_box.style.position == PositionMode::Fixed,
                    flex_hint: None,
                    grid_hint: None,
                });
                continue;
            }

            if child_box.is_float {
                let clear = child_box.style.clear_mode;
                let mut float_y = cursor_y;
                if let Some(mb) = prev_margin_bottom {
                    float_y += mb;
                }
                if clear != ClearMode::None {
                    float_y = local_floats.clear_y(clear, float_y);
                }
                let is_left = matches!(
                    child_box.style.float_mode,
                    FloatMode::Left | FloatMode::InlineStart
                );
                let child_oof_start = ctx.pending_oof.len();
                let mut frag = layout_box(ctx, cid, child_space);
                let margin_box = Size::new(
                    frag.border_rect.size.width + frag.margin.horizontal(),
                    frag.border_rect.size.height + frag.margin.vertical(),
                );
                let pos = local_floats.place_float(is_left, margin_box, float_y, content_width);
                frag.border_rect.origin = Point::new(
                    content_origin_x + pos.x + frag.margin.left,
                    content_origin_y + pos.y + frag.margin.top,
                );
                apply_relative_offset(&mut frag, child_space);
                offset_pending_oof(
                    &mut ctx.pending_oof,
                    child_oof_start,
                    frag.border_rect.origin.x,
                    frag.border_rect.origin.y,
                );
                placed_children.push(frag);
                continue;
            }

            // In-flow block-level child
            let child_clear = child_box.style.clear_mode;
            let child_is_bfc = child_box.establishes_bfc;

            let child_oof_start = ctx.pending_oof.len();
            // Compute child layout; if child establishes a BFC next to floats, shrink available width if needed.
            let mut frag = if child_is_bfc && !local_floats.is_empty() {
                let probe_y = if child_clear != ClearMode::None {
                    local_floats.clear_y(child_clear, cursor_y)
                } else {
                    cursor_y
                };
                let (lx, rx) = local_floats.available_band(probe_y, 1.0, content_width);
                let band_w = (rx - lx).max(0.0);
                let sub_space = child_space.with_available(band_w, metrics.specified_height);
                layout_box(ctx, cid, sub_space)
            } else if matches!(
                child_box.kind,
                BoxKind::Block { .. } | BoxKind::AnonymousBlock
            ) && !child_is_bfc
                && !local_floats.is_empty()
            {
                let mut child_floats = local_floats.translated(0.0, -cursor_y);
                layout_block_box(ctx, cid, child_space, &mut child_floats)
            } else {
                layout_box(ctx, cid, child_space)
            };

            let child_top_margin = frag.collapsed_margin_top;
            let child_bottom_margin = frag.collapsed_margin_bottom;
            let is_empty_self_collapsing = frag.border_rect.size.height == 0.0
                && frag.border.vertical() == 0.0
                && frag.padding.vertical() == 0.0
                && frag.children.is_empty();

            if !saw_in_flow_child {
                saw_in_flow_child = true;
                if can_collapse_top_with_first && child_clear == ClearMode::None {
                    collapsed_top = collapse_margins(collapsed_top, child_top_margin);
                } else {
                    cursor_y += child_top_margin;
                }
            } else if let Some(prev_mb) = prev_margin_bottom {
                cursor_y += collapse_margins(prev_mb, child_top_margin);
            } else {
                cursor_y += child_top_margin;
            }

            if child_clear != ClearMode::None {
                cursor_y = local_floats.clear_y(child_clear, cursor_y);
            }

            let mut x_offset = frag.margin.left;
            if child_is_bfc && !local_floats.is_empty() {
                let (lx, _) = local_floats.available_band(
                    cursor_y,
                    frag.border_rect.size.height.max(1.0),
                    content_width,
                );
                x_offset = lx + frag.margin.left;
            }

            frag.border_rect.origin =
                Point::new(content_origin_x + x_offset, content_origin_y + cursor_y);
            apply_relative_offset(&mut frag, child_space);
            offset_pending_oof(
                &mut ctx.pending_oof,
                child_oof_start,
                frag.border_rect.origin.x,
                frag.border_rect.origin.y,
            );

            if is_empty_self_collapsing {
                let combined = collapse_margins(child_top_margin, child_bottom_margin);
                prev_margin_bottom = Some(match prev_margin_bottom {
                    Some(prev) => collapse_margins(prev, combined),
                    None => combined,
                });
            } else {
                cursor_y += frag.border_rect.size.height;
                prev_margin_bottom = Some(child_bottom_margin);
            }

            placed_children.push(frag);
        }

        // Trailing bottom margin of last in-flow child.
        if let Some(last_mb) = prev_margin_bottom {
            if can_collapse_bottom_with_last {
                collapsed_bottom = collapse_margins(collapsed_bottom, last_mb);
            } else {
                cursor_y += last_mb;
            }
        }

        if establishes_bfc {
            cursor_y = cursor_y.max(local_floats.max_float_bottom());
        } else {
            // Propagate newly added floats back to parent float context if non-BFC.
            for f in &local_floats.floats {
                let translated = FloatBand {
                    rect: Rect::new(
                        f.rect.origin.x + content_origin_x,
                        f.rect.origin.y + content_origin_y,
                        f.rect.size.width,
                        f.rect.size.height,
                    ),
                    is_left: f.is_left,
                };
                if !parent_floats.floats.contains(&translated) {
                    parent_floats.floats.push(translated);
                }
            }
        }
        cursor_y
    };

    let intr_h_fn = |basis: &rix_css::values_math::CalcSizeBasis| -> f32 {
        match basis {
            rix_css::values_math::CalcSizeBasis::Stretch => {
                space.available_height.map_or(content_height, |ah| {
                    (ah - metrics.margin.vertical() - pb_h).max(0.0)
                })
            }
            _ => content_height,
        }
    };
    let is_bb_h = is_bb || (is_table_cell && ctx.tree.quirks_mode == 2);
    let eff_min_h = resolve_size_with_intrinsic(
        &style.box_model.min_height,
        space.available_height,
        pb_h,
        metrics.margin.vertical(),
        is_bb_h,
        &eval,
        intr_h_fn,
    )
    .unwrap_or(metrics.min_height);
    let eff_max_h = resolve_size_with_intrinsic(
        &style.box_model.max_height,
        space.available_height,
        pb_h,
        metrics.margin.vertical(),
        is_bb_h,
        &eval,
        intr_h_fn,
    )
    .unwrap_or(metrics.max_height);
    let raw_content_h = definite_height.unwrap_or_else(|| {
        resolve_size_with_intrinsic(
            &style.box_model.height,
            space.available_height,
            pb_h,
            metrics.margin.vertical(),
            is_bb_h,
            &eval,
            intr_h_fn,
        )
        .unwrap_or(content_height)
    });
    let final_content_height = if space.ignore_specified_height {
        content_height.max(0.0)
    } else if is_table_cell {
        // CSS Tables L3: `height` on a `table-cell` acts as a minimum height for its content.
        raw_content_h
            .min(eff_max_h)
            .max(eff_min_h)
            .max(content_height)
            .max(0.0)
    } else {
        raw_content_h.min(eff_max_h).max(eff_min_h).max(0.0)
    };
    let border_w = content_width + pb_w;
    let border_h = final_content_height + pb_h;

    // Resolve out-of-flow (`position: absolute | fixed`) descendants whose true CSS containing
    // block is this box's padding box; all other pending OOFs propagate up to an ancestor CB.
    let padding_box_w = content_width + metrics.padding.horizontal();
    let padding_box_h = final_content_height + metrics.padding.vertical();
    resolve_oof_for_container(
        ctx,
        my_oof_start,
        &style,
        Rect::new(
            metrics.border.left,
            metrics.border.top,
            padding_box_w,
            padding_box_h,
        ),
        false,
        space,
        &mut placed_children,
    );

    let is_vert_wm = style.text.writing_mode != rix_css::WritingMode::HorizontalTb;
    let is_scroll_box = style.overflow.x != rix_css::OverflowMode::Visible
        || style.overflow.y != rix_css::OverflowMode::Visible;
    let (first_baseline, last_baseline) = if is_vert_wm {
        let bs_pb = if style.text.writing_mode == rix_css::WritingMode::VerticalRl {
            metrics.border.right + metrics.padding.right
        } else {
            metrics.border.left + metrics.padding.left
        };
        let fb = placed_children.iter().find_map(|c| {
            if c.is_out_of_flow || c.is_float {
                return None;
            }
            if c.style.text.writing_mode != rix_css::WritingMode::HorizontalTb
                && !c.children.is_empty()
            {
                let child_bs = if style.text.writing_mode == rix_css::WritingMode::VerticalRl {
                    border_w - (c.border_rect.origin.x + c.border_rect.size.width)
                } else {
                    c.border_rect.origin.x
                };
                c.baseline.map(|b| child_bs + b)
            } else {
                Some(bs_pb + c.border_rect.size.width * 0.5)
            }
        });
        let lb = placed_children.iter().rev().find_map(|c| {
            if c.is_out_of_flow || c.is_float {
                return None;
            }
            if c.style.text.writing_mode != rix_css::WritingMode::HorizontalTb
                && !c.children.is_empty()
            {
                let child_bs = if style.text.writing_mode == rix_css::WritingMode::VerticalRl {
                    border_w - (c.border_rect.origin.x + c.border_rect.size.width)
                } else {
                    c.border_rect.origin.x
                };
                c.last_baseline.or(c.baseline).map(|b| child_bs + b)
            } else {
                Some(bs_pb + c.border_rect.size.width * 0.5)
            }
        });
        let clamp_v = |v: f32| {
            if is_scroll_box {
                v.clamp(0.0, border_w)
            } else {
                v
            }
        };
        (
            Some(clamp_v(fb.unwrap_or(border_w * 0.5))),
            Some(clamp_v(lb.or(fb).unwrap_or(border_w * 0.5))),
        )
    } else {
        let fb = placed_children.iter().find_map(|c| {
            if c.is_out_of_flow || c.is_float {
                return None;
            }
            c.baseline.map(|b_y| c.border_rect.origin.y + b_y)
        });
        let lb = placed_children.iter().rev().find_map(|c| {
            if c.is_out_of_flow || c.is_float {
                return None;
            }
            c.last_baseline
                .or(c.baseline)
                .map(|b_y| c.border_rect.origin.y + b_y)
        });
        let clamp_h = |v: f32| {
            if is_scroll_box {
                v.clamp(0.0, border_h)
            } else {
                v
            }
        };
        (
            Some(clamp_h(fb.unwrap_or(border_h))),
            Some(clamp_h(lb.or(fb).unwrap_or(border_h))),
        )
    };

    let mut fragment = Fragment {
        box_id,
        dom_node: ctx.tree.get(box_id).dom_node,
        style,
        border_rect: Rect::new(0.0, 0.0, border_w, border_h),
        padding: metrics.padding,
        border: metrics.border,
        margin: resolved_margin,
        content_rect: Rect::new(
            content_origin_x,
            content_origin_y,
            content_width,
            final_content_height,
        ),
        baseline: first_baseline,
        last_baseline,
        overflow_rect: Rect::new(0.0, 0.0, border_w, border_h),
        kind: FragmentKind::Box,
        children: Arc::new(placed_children),
        is_float: ctx.tree.get(box_id).is_float,
        is_out_of_flow: ctx.tree.get(box_id).is_out_of_flow,
        collapsed_margin_top: collapsed_top,
        collapsed_margin_bottom: collapsed_bottom,
        resolved_grid_tracks: None,
    };
    fragment.compute_overflow_rect();
    fragment
}

/// Applies CSS `position: relative` visual offset after normal-flow placement.
pub fn apply_relative_offset(frag: &mut Fragment, space: ConstraintSpace) {
    if frag.style.position != PositionMode::Relative {
        return;
    }
    let eval = space.eval_context_for(&frag.style);
    let inset = &frag.style.box_model.inset;
    let is_rtl = frag.style.text.direction == rix_css::DirectionMode::Rtl;
    let dx = if is_rtl {
        match (&inset.left, &inset.right) {
            (_, LengthPercentageAuto::Value(r)) => {
                -r.resolve_px(space.containing_block_width, &eval)
            }
            (LengthPercentageAuto::Value(l), LengthPercentageAuto::Auto) => {
                l.resolve_px(space.containing_block_width, &eval)
            }
            _ => 0.0,
        }
    } else {
        match (&inset.left, &inset.right) {
            (LengthPercentageAuto::Value(l), _) => {
                l.resolve_px(space.containing_block_width, &eval)
            }
            (LengthPercentageAuto::Auto, LengthPercentageAuto::Value(r)) => {
                -r.resolve_px(space.containing_block_width, &eval)
            }
            _ => 0.0,
        }
    };
    let resolve_v = |v: &LengthPercentageAuto| -> Option<f32> {
        match v {
            LengthPercentageAuto::Auto => None,
            LengthPercentageAuto::Value(lp) => {
                if lp.has_percentage() && space.available_height.is_none() {
                    Some(0.0)
                } else {
                    Some(
                        lp.resolve_px(
                            space
                                .available_height
                                .unwrap_or(space.containing_block_height),
                            &eval,
                        ),
                    )
                }
            }
        }
    };
    let dy = match (resolve_v(&inset.top), resolve_v(&inset.bottom)) {
        (Some(t), _) => t,
        (None, Some(b)) => -b,
        _ => 0.0,
    };
    frag.border_rect.origin.x += dx;
    frag.border_rect.origin.y += dy;
}
