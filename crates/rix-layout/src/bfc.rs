//! Block Formatting Context (BFC): CSS 2.1 collapsing vertical margins,
//! `float: left/right` exclusion bands, `clear`, BFC float containment,
//! `position: relative`, and out-of-flow `position: absolute | fixed` placement.

use std::sync::Arc;

use rix_core::{Point, Rect, Size};
use rix_css::{ClearMode, FloatMode, LengthPercentageAuto, PositionMode};

use crate::box_tree::{BoxId, BoxKind, LayoutBox};
use crate::fragment::{
    layout_box, measure_content_intrinsic_widths, resolve_size_with_intrinsic,
    size_value_needs_intrinsic, BoxMetrics, ConstraintSpace, EdgeInsets, Fragment, FragmentKind,
    LayoutContext,
};
use crate::ifc::layout_ifc;

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
    let shrink_to_fit = b.is_float
        || (b.is_out_of_flow && !oof_both_h_insets)
        || matches!(b.kind, BoxKind::InlineBlock { .. });
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

    let resolved_spec_w = metrics.specified_width.or_else(|| {
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

    let (content_width, resolved_margin_left, resolved_margin_right) =
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
    let child_space = space.with_available(content_width, metrics.specified_height);

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
    let mut out_of_flow_ids: Vec<BoxId> = Vec::new();
    let mut collapsed_top = resolved_margin.top;
    let mut collapsed_bottom = resolved_margin.bottom;

    let content_height = if children_ids.is_empty() {
        0.0
    } else if !has_in_flow_blocks {
        // Pure Inline Formatting Context inside this block container!
        for &cid in &children_ids {
            if ctx.tree.get(cid).is_out_of_flow {
                out_of_flow_ids.push(cid);
            }
        }
        let ifc_result = layout_ifc(
            ctx,
            box_id,
            child_space,
            &local_floats,
            Point::new(content_origin_x, content_origin_y),
        );
        placed_children.extend(ifc_result.fragments);
        ifc_result.height
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
                out_of_flow_ids.push(cid);
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
                placed_children.push(frag);
                continue;
            }

            // In-flow block-level child
            let child_clear = child_box.style.clear_mode;
            let child_is_bfc = child_box.establishes_bfc;

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
    let eff_min_h = resolve_size_with_intrinsic(
        &style.box_model.min_height,
        space.available_height,
        pb_h,
        metrics.margin.vertical(),
        is_bb,
        &eval,
        intr_h_fn,
    )
    .unwrap_or(metrics.min_height);
    let eff_max_h = resolve_size_with_intrinsic(
        &style.box_model.max_height,
        space.available_height,
        pb_h,
        metrics.margin.vertical(),
        is_bb,
        &eval,
        intr_h_fn,
    )
    .unwrap_or(metrics.max_height);
    let raw_content_h = metrics.specified_height.unwrap_or_else(|| {
        resolve_size_with_intrinsic(
            &style.box_model.height,
            space.available_height,
            pb_h,
            metrics.margin.vertical(),
            is_bb,
            &eval,
            intr_h_fn,
        )
        .unwrap_or(content_height)
    });
    let final_content_height = raw_content_h.min(eff_max_h).max(eff_min_h).max(0.0);
    let border_w = content_width + pb_w;
    let border_h = final_content_height + pb_h;

    // Layout out-of-flow (`position: absolute | fixed`) children against this container's padding box
    // (or viewport for `fixed`).
    let padding_box_w = content_width + metrics.padding.horizontal();
    let padding_box_h = final_content_height + metrics.padding.vertical();
    for oof_id in out_of_flow_ids {
        let oof_box = ctx.tree.get(oof_id);
        let is_fixed = oof_box.style.position == PositionMode::Fixed;
        let (cb_w, cb_h, base_x, base_y) = if is_fixed {
            (
                space.viewport_size.width,
                space.viewport_size.height,
                0.0,
                0.0,
            )
        } else {
            (
                padding_box_w,
                padding_box_h,
                metrics.border.left,
                metrics.border.top,
            )
        };
        let oof_frag = layout_out_of_flow(ctx, oof_box, space, cb_w, cb_h, base_x, base_y);
        placed_children.push(oof_frag);
    }

    let first_baseline = placed_children.iter().find_map(|c| {
        if c.is_out_of_flow || c.is_float {
            return None;
        }
        c.baseline.map(|b_y| c.border_rect.origin.y + b_y)
    });

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
        baseline: first_baseline.or(Some(border_h)),
        overflow_rect: Rect::new(0.0, 0.0, border_w, border_h),
        kind: FragmentKind::Box,
        children: Arc::new(placed_children),
        is_float: ctx.tree.get(box_id).is_float,
        is_out_of_flow: ctx.tree.get(box_id).is_out_of_flow,
        collapsed_margin_top: collapsed_top,
        collapsed_margin_bottom: collapsed_bottom,
    };
    fragment.compute_overflow_rect();
    fragment
}

fn layout_out_of_flow(
    ctx: &mut LayoutContext<'_>,
    oof_box: &LayoutBox,
    parent_space: ConstraintSpace,
    cb_w: f32,
    cb_h: f32,
    base_x: f32,
    base_y: f32,
) -> Fragment {
    let style = &oof_box.style;
    let eval = parent_space.eval_context_for(style);
    let inset = &style.box_model.inset;
    let left = match &inset.left {
        LengthPercentageAuto::Auto => None,
        LengthPercentageAuto::Value(lp) => Some(lp.resolve_px(cb_w, &eval)),
    };
    let right = match &inset.right {
        LengthPercentageAuto::Auto => None,
        LengthPercentageAuto::Value(lp) => Some(lp.resolve_px(cb_w, &eval)),
    };
    let top = match &inset.top {
        LengthPercentageAuto::Auto => None,
        LengthPercentageAuto::Value(lp) => Some(lp.resolve_px(cb_h, &eval)),
    };
    let bottom = match &inset.bottom {
        LengthPercentageAuto::Auto => None,
        LengthPercentageAuto::Value(lp) => Some(lp.resolve_px(cb_h, &eval)),
    };

    // If both left & right are specified and width is auto (or calc-size with auto basis),
    // stretch available width between insets.
    let stretched_w = match (left, right, &style.box_model.width) {
        (Some(l), Some(r), rix_css::SizeValue::Auto) => Some((cb_w - l - r).max(0.0)),
        _ => None,
    };
    let calc_size_inset_w = match (left, right, &style.box_model.width) {
        (Some(l), Some(r), rix_css::SizeValue::CalcSize(_)) => Some((cb_w - l - r).max(0.0)),
        _ => None,
    };
    let stretched_h = match (top, bottom, &style.box_model.height) {
        (Some(t), Some(b), rix_css::SizeValue::Auto) => Some((cb_h - t - b).max(0.0)),
        _ => None,
    };
    let calc_size_inset_h = match (top, bottom, &style.box_model.height) {
        (Some(t), Some(b), rix_css::SizeValue::CalcSize(_)) => Some((cb_h - t - b).max(0.0)),
        _ => None,
    };

    let avail_w = stretched_w.or(calc_size_inset_w).unwrap_or(cb_w);
    let avail_h = stretched_h.or(calc_size_inset_h).or(Some(cb_h));
    let mut sub_space = parent_space.with_available(avail_w, avail_h);
    sub_space.containing_block_width = cb_w;
    sub_space.containing_block_height = cb_h;

    let mut frag = layout_box(ctx, oof_box.id, sub_space);
    if let Some(sw) = stretched_w {
        let pb_w = frag.padding.horizontal() + frag.border.horizontal();
        frag.border_rect.size.width = sw;
        frag.content_rect.size.width = (sw - pb_w).max(0.0);
    }
    if let Some(sh) = stretched_h {
        let pb_h = frag.padding.vertical() + frag.border.vertical();
        frag.border_rect.size.height = sh;
        frag.content_rect.size.height = (sh - pb_h).max(0.0);
    }

    let x = match (left, right) {
        (Some(l), _) => base_x + l + frag.margin.left,
        (None, Some(r)) => base_x + cb_w - r - frag.border_rect.size.width - frag.margin.right,
        (None, None) => base_x + frag.margin.left,
    };
    let y = match (top, bottom) {
        (Some(t), _) => base_y + t + frag.margin.top,
        (None, Some(b)) => base_y + cb_h - b - frag.border_rect.size.height - frag.margin.bottom,
        (None, None) => base_y + frag.margin.top,
    };
    frag.border_rect.origin = Point::new(x, y);
    frag
}

/// Applies CSS `position: relative` visual offset after normal-flow placement.
pub fn apply_relative_offset(frag: &mut Fragment, space: ConstraintSpace) {
    if frag.style.position != PositionMode::Relative {
        return;
    }
    let eval = space.eval_context_for(&frag.style);
    let inset = &frag.style.box_model.inset;
    let dx = match (&inset.left, &inset.right) {
        (LengthPercentageAuto::Value(l), _) => l.resolve_px(space.containing_block_width, &eval),
        (LengthPercentageAuto::Auto, LengthPercentageAuto::Value(r)) => {
            -r.resolve_px(space.containing_block_width, &eval)
        }
        _ => 0.0,
    };
    let dy = match (&inset.top, &inset.bottom) {
        (LengthPercentageAuto::Value(t), _) => t.resolve_px(space.containing_block_height, &eval),
        (LengthPercentageAuto::Auto, LengthPercentageAuto::Value(b)) => {
            -b.resolve_px(space.containing_block_height, &eval)
        }
        _ => 0.0,
    };
    frag.border_rect.origin.x += dx;
    frag.border_rect.origin.y += dy;
}
