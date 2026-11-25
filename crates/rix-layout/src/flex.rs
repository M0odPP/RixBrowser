//! Native pure-Rust W3C CSS Flexible Box Layout Module Level 1 & CSS Box Alignment Level 3
//! layout engine.
//!
//! Implements:
//! - Unified 2D physical axis mapping across all 5 `writing-mode`s × 2 `direction`s ×
//!   4 `flex-direction`s × 3 `flex-wrap`s
//! - §9.2 Flex base size & hypothetical main size (including `aspect-ratio` transfer and
//!   §4.5 automatic minimum size)
//! - §9.3 Multi-line collection (`nowrap`, `wrap`, `wrap-reverse`) with `row-gap`/`column-gap`
//! - §9.7 Exact iterative freeze loop for resolving flexible lengths (`flex-grow`, `flex-shrink`)
//! - §9.4–§9.6 Cross-axis sizing, `align-content`, `justify-content`, `align-items`/`align-self`
//!   (`safe`/`unsafe`, `left`/`right`, `start`/`end`, `self-start`/`self-end`, `first/last baseline`)
//! - §9.8 Flex container first & last baseline computation
//! - §4.1 Absolutely-positioned flex child static position (`FlexAbsposHint`)
//! - §4.4 `visibility: collapse` flex item handling

use std::sync::Arc;

use rix_core::{Point, Rect};
use rix_css::{
    AlignValue, BoxSizingMode, ComputedStyle, DirectionMode, FlexDirectionMode, FlexWrapMode,
    LengthPercentage, LengthPercentageAuto, OverflowMode, PositionMode, SizeValue, VisibilityMode,
    WritingMode,
};

use crate::bfc::apply_relative_offset;
use crate::box_tree::{BoxId, BoxKind, ReplacedKind};
use crate::fragment::{
    layout_box, measure_intrinsic_widths, resolve_size_with_intrinsic, BoxMetrics, ConstraintSpace,
    EdgeInsets, Fragment, FragmentKind, LayoutContext,
};
use crate::position::{offset_pending_oof, resolve_oof_for_container, PendingOof};

/// Unified physical-axis descriptor for a flex container combining `writing-mode`, `direction`,
/// `flex-direction`, and `flex-wrap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlexAxes {
    /// `true` if main axis is physical X, `false` if physical Y.
    pub main_is_x: bool,
    /// `true` if main-start is at the physical max edge (`+X` -> `-X` or `+Y` -> `-Y`).
    pub main_reversed: bool,
    /// `true` if cross-start is at the physical max edge.
    pub cross_reversed: bool,
    /// `true` if the container's writing-mode start on the main axis is at the physical max edge
    /// (independent of `row-reverse`/`column-reverse` — used for `start`/`end`/`safe`).
    pub wm_main_reversed: bool,
    /// `true` if the container's writing-mode start on the cross axis is at the physical max edge
    /// (independent of `wrap-reverse` — used for `start`/`end`/`safe`).
    pub wm_cross_reversed: bool,
}

impl FlexAxes {
    /// Computes exact physical axis directions from `(writing_mode, direction, flex_direction, flex_wrap)`.
    #[must_use]
    pub const fn compute(
        wm: WritingMode,
        dir: DirectionMode,
        flex_dir: FlexDirectionMode,
        flex_wrap: FlexWrapMode,
    ) -> Self {
        let is_rtl = matches!(dir, DirectionMode::Rtl);
        let (inline_is_x, inline_rev, block_rev) = match wm {
            WritingMode::HorizontalTb => (true, is_rtl, false),
            WritingMode::VerticalRl | WritingMode::SidewaysRl => (false, is_rtl, true),
            WritingMode::VerticalLr => (false, is_rtl, false),
            WritingMode::SidewaysLr => (false, !is_rtl, false),
        };
        let (main_is_x, main_rev, wm_main_rev, mut cross_rev, wm_cross_rev) = match flex_dir {
            FlexDirectionMode::Row => (inline_is_x, inline_rev, inline_rev, block_rev, block_rev),
            FlexDirectionMode::RowReverse => {
                (inline_is_x, !inline_rev, inline_rev, block_rev, block_rev)
            }
            FlexDirectionMode::Column => {
                (!inline_is_x, block_rev, block_rev, inline_rev, inline_rev)
            }
            FlexDirectionMode::ColumnReverse => {
                (!inline_is_x, !block_rev, block_rev, inline_rev, inline_rev)
            }
        };
        if matches!(flex_wrap, FlexWrapMode::WrapReverse) {
            cross_rev = !cross_rev;
        }
        Self {
            main_is_x,
            main_reversed: main_rev,
            cross_reversed: cross_rev,
            wm_main_reversed: wm_main_rev,
            wm_cross_reversed: wm_cross_rev,
        }
    }

    /// Returns whether the item's own writing-mode start along the cross axis is physically reversed.
    #[must_use]
    pub const fn item_cross_reversed(self, item_wm: WritingMode, item_dir: DirectionMode) -> bool {
        let is_rtl = matches!(item_dir, DirectionMode::Rtl);
        let (inline_is_x, inline_rev, block_rev) = match item_wm {
            WritingMode::HorizontalTb => (true, is_rtl, false),
            WritingMode::VerticalRl | WritingMode::SidewaysRl => (false, is_rtl, true),
            WritingMode::VerticalLr => (false, is_rtl, false),
            WritingMode::SidewaysLr => (false, !is_rtl, false),
        };
        let cross_is_x = !self.main_is_x;
        if cross_is_x == inline_is_x {
            inline_rev
        } else {
            block_rev
        }
    }
}

/// Static position alignment metadata attached to a [`PendingOof`] whose layout parent is a
/// CSS Flexbox container (CSS Flexbox L1 §4.1).
#[derive(Debug, Clone, Copy)]
pub struct FlexAbsposHint {
    /// Flex container's content-box rect in its border-box coordinates (translated upward as ancestors place it).
    pub content_rect: Rect,
    pub axes: FlexAxes,
    pub justify_content: AlignValue,
    pub align_items: AlignValue,
    pub align_self: AlignValue,
}

impl FlexAbsposHint {
    /// Computes the hypothetical static margin-box top-left position `(x, y)` of an abspos
    /// child with outer margin-box size `(outer_w, outer_h)` in the same coordinate space as
    /// `self.content_rect`.
    #[must_use]
    pub fn compute_static_pos(
        &self,
        outer_w: f32,
        outer_h: f32,
        item_wm: WritingMode,
        item_dir: DirectionMode,
    ) -> Point {
        let (cb_main, cb_cross, item_main, item_cross) = if self.axes.main_is_x {
            (
                self.content_rect.size.width,
                self.content_rect.size.height,
                outer_w,
                outer_h,
            )
        } else {
            (
                self.content_rect.size.height,
                self.content_rect.size.width,
                outer_h,
                outer_w,
            )
        };

        let main_off = align_single_in_span(
            cb_main,
            item_main,
            self.justify_content,
            self.axes.main_is_x,
            self.axes.main_reversed,
            self.axes.wm_main_reversed,
            self.axes.wm_main_reversed,
            true,
        );

        let eff_cross = match self.align_self {
            AlignValue::Auto => self.align_items,
            other => other,
        };
        let item_self_rev = self.axes.item_cross_reversed(item_wm, item_dir);
        let cross_off = align_single_in_span(
            cb_cross,
            item_cross,
            eff_cross,
            !self.axes.main_is_x,
            self.axes.cross_reversed,
            self.axes.wm_cross_reversed,
            item_self_rev,
            false,
        );

        let (px, py) = if self.axes.main_is_x {
            (
                self.content_rect.origin.x + main_off,
                self.content_rect.origin.y + cross_off,
            )
        } else {
            (
                self.content_rect.origin.x + cross_off,
                self.content_rect.origin.y + main_off,
            )
        };
        Point::new(px, py)
    }
}

/// Computes the physical offset from the physical min edge (`left` or `top`) of a span of length
/// `container_len` for a single box of length `item_len` under CSS Box Alignment L3.
#[allow(clippy::too_many_arguments)]
#[must_use]
fn align_single_in_span(
    container_len: f32,
    item_len: f32,
    align: AlignValue,
    axis_is_x: bool,
    flex_rev: bool,
    wm_rev: bool,
    self_rev: bool,
    _is_main_content_dist: bool,
) -> f32 {
    let free = container_len - item_len;
    let is_safe = align.is_safe();
    if is_safe && free < 0.0 {
        return if wm_rev { free } else { 0.0 };
    }
    let flex_start_off = if flex_rev { free } else { 0.0 };
    let flex_end_off = if flex_rev { 0.0 } else { free };
    let wm_start_off = if wm_rev { free } else { 0.0 };
    let wm_end_off = if wm_rev { 0.0 } else { free };
    let self_start_off = if self_rev { free } else { 0.0 };
    let self_end_off = if self_rev { 0.0 } else { free };

    match align.base() {
        AlignValue::Normal
        | AlignValue::Auto
        | AlignValue::Stretch
        | AlignValue::FlexStart
        | AlignValue::SpaceBetween => flex_start_off,
        AlignValue::FlexEnd => flex_end_off,
        AlignValue::Start => wm_start_off,
        AlignValue::End => wm_end_off,
        AlignValue::SelfStart => self_start_off,
        AlignValue::SelfEnd => self_end_off,
        AlignValue::Left => {
            if axis_is_x {
                0.0
            } else {
                wm_start_off
            }
        }
        AlignValue::Right => {
            if axis_is_x {
                free
            } else {
                wm_start_off
            }
        }
        AlignValue::Center | AlignValue::SpaceAround | AlignValue::SpaceEvenly => free * 0.5,
        AlignValue::Baseline => wm_start_off,
        AlignValue::LastBaseline => wm_end_off,
        _ => flex_start_off,
    }
}

/// Computes `(initial_offset, between_gap)` in flex-start -> flex-end logical order along an axis
/// for `count` items/lines whose total occupied size (excluding CSS `gap`) is `content_sum` with
/// base `gap`.
#[allow(clippy::too_many_arguments)]
fn distribute_content_positions(
    container_len: f32,
    content_sum: f32,
    gap: f32,
    count: usize,
    align: AlignValue,
    axis_is_x: bool,
    flex_rev: bool,
    wm_rev: bool,
) -> (f32, f32) {
    if count == 0 {
        return (0.0, gap);
    }
    let total_gaps = gap * count.saturating_sub(1) as f32;
    let free = container_len - content_sum - total_gaps;
    let is_safe = align.is_safe();

    // Logical start offset (from flex-start edge toward flex-end).
    let wm_start_logical = if wm_rev == flex_rev { 0.0 } else { free };
    let wm_end_logical = if wm_rev == flex_rev { free } else { 0.0 };

    if is_safe && free < 0.0 {
        return (wm_start_logical, gap);
    }

    let base = align.base();
    if free <= 0.0 || count == 1 {
        let off = match base {
            AlignValue::Normal
            | AlignValue::Auto
            | AlignValue::Stretch
            | AlignValue::FlexStart
            | AlignValue::Baseline
            | AlignValue::LastBaseline => 0.0,
            AlignValue::SpaceBetween => {
                // CSS Box Alignment L3 §5.3: `space-between` fallback is `safe flex-start` / `start`.
                if free < 0.0 {
                    wm_start_logical
                } else {
                    0.0
                }
            }
            AlignValue::SpaceAround | AlignValue::SpaceEvenly => {
                // CSS Box Alignment L3 §5.3: `space-around` / `space-evenly` fallback is `safe center`.
                if free < 0.0 {
                    wm_start_logical
                } else {
                    free * 0.5
                }
            }
            AlignValue::FlexEnd => free,
            AlignValue::Start => wm_start_logical,
            AlignValue::End => wm_end_logical,
            AlignValue::SelfStart => wm_start_logical,
            AlignValue::SelfEnd => wm_end_logical,
            AlignValue::Left => {
                if axis_is_x {
                    if flex_rev {
                        free
                    } else {
                        0.0
                    }
                } else {
                    wm_start_logical
                }
            }
            AlignValue::Right => {
                if axis_is_x {
                    if flex_rev {
                        0.0
                    } else {
                        free
                    }
                } else {
                    wm_start_logical
                }
            }
            AlignValue::Center => free * 0.5,
            _ => 0.0,
        };
        return (off, gap);
    }

    match base {
        AlignValue::SpaceBetween => (0.0, gap + free / (count - 1) as f32),
        AlignValue::SpaceAround => {
            let slot = free / count as f32;
            (slot * 0.5, gap + slot)
        }
        AlignValue::SpaceEvenly => {
            let slot = free / (count + 1) as f32;
            (slot, gap + slot)
        }
        AlignValue::FlexEnd => (free, gap),
        AlignValue::Start | AlignValue::SelfStart => (wm_start_logical, gap),
        AlignValue::End | AlignValue::SelfEnd => (wm_end_logical, gap),
        AlignValue::Left => {
            let off = if axis_is_x {
                if flex_rev {
                    free
                } else {
                    0.0
                }
            } else {
                wm_start_logical
            };
            (off, gap)
        }
        AlignValue::Right => {
            let off = if axis_is_x {
                if flex_rev {
                    0.0
                } else {
                    free
                }
            } else {
                wm_start_logical
            };
            (off, gap)
        }
        AlignValue::Center => (free * 0.5, gap),
        _ => (0.0, gap),
    }
}

/// Extracts intrinsic aspect ratio `width / height` from CSS `aspect-ratio` or replaced element
/// intrinsic dimensions.
fn box_aspect_ratio(
    ctx: &LayoutContext<'_>,
    box_id: BoxId,
    style: &ComputedStyle,
    eval: &rix_css::EvalContext,
) -> Option<f32> {
    if let Some((num, den)) = style
        .effects
        .as_ref()
        .and_then(|e| e.aspect_ratio_math.as_ref())
    {
        let n = num.eval_scalar(eval, None);
        let d = den.as_ref().map_or(1.0, |x| x.eval_scalar(eval, None));
        let r = n / d;
        if r.is_finite() && r > 0.0 {
            return Some(r as f32);
        }
    }
    match &ctx.tree.get(box_id).kind {
        BoxKind::Replaced(ReplacedKind::Image { intrinsic_size, .. })
        | BoxKind::Replaced(ReplacedKind::Svg { intrinsic_size, .. })
        | BoxKind::Replaced(ReplacedKind::ExternalSurface { intrinsic_size, .. }) => {
            if intrinsic_size.width > 0.0 && intrinsic_size.height > 0.0 {
                Some(intrinsic_size.width / intrinsic_size.height)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Extracts natural `(width, height)` for replaced elements with intrinsic dimensions.
fn replaced_natural_size(ctx: &LayoutContext<'_>, box_id: BoxId) -> Option<(f32, f32)> {
    match &ctx.tree.get(box_id).kind {
        BoxKind::Replaced(ReplacedKind::Image { intrinsic_size, .. })
        | BoxKind::Replaced(ReplacedKind::Svg { intrinsic_size, .. })
        | BoxKind::Replaced(ReplacedKind::ExternalSurface { intrinsic_size, .. }) => {
            Some((intrinsic_size.width, intrinsic_size.height))
        }
        _ => None,
    }
}

struct FlexItemState {
    box_id: BoxId,
    style: Arc<ComputedStyle>,
    pb_main: f32,
    pb_cross: f32,
    margin_main_start: f32,
    margin_main_end: f32,
    margin_cross_start: f32,
    margin_cross_end: f32,
    margin_main_start_auto: bool,
    margin_main_end_auto: bool,
    margin_cross_start_auto: bool,
    margin_cross_end_auto: bool,
    flex_grow: f32,
    flex_shrink: f32,
    flex_base_size: f32,         // inner (content) main size
    hypothetical_main_size: f32, // clamped inner main size
    target_main_size: f32,       // solved inner main size
    min_main: f32,               // inner min main size
    max_main: f32,               // inner max main size
    min_cross: f32,              // inner min cross size
    max_cross: f32,              // inner max cross size
    spec_cross: Option<f32>,     // specified inner cross size if definite
    aspect_ratio: Option<f32>,   // width / height
    content_min_w: f32,          // raw content min-content width
    content_max_w: f32,          // raw content max-content width
    had_def_cross_pass1: bool,   // whether pass-1 layout already gave item a definite cross size
    align_self: AlignValue,
    collapsed: bool,
    frozen: bool,
    // Layout outputs after cross sizing:
    outer_cross_size: f32,
    used_cross_border: f32,
    first_baseline: f32, // distance from cross-start border edge (in horizontal-tb: top)
    last_baseline: f32,  // distance from cross-end border edge (in horizontal-tb: bottom)
    frag: Option<Fragment>,
    oof_range: (usize, usize),
}

impl FlexItemState {
    #[inline]
    fn outer_hypo_main(&self) -> f32 {
        if self.collapsed {
            return 0.0;
        }
        self.hypothetical_main_size + self.pb_main + self.margin_main_start + self.margin_main_end
    }

    #[inline]
    fn target_border_main(&self) -> f32 {
        if self.collapsed {
            return 0.0;
        }
        (self.target_main_size + self.pb_main).max(0.0)
    }

    #[inline]
    fn outer_target_main(&self) -> f32 {
        if self.collapsed {
            return 0.0;
        }
        self.target_border_main() + self.margin_main_start + self.margin_main_end
    }
}

struct FlexLine {
    item_indices: Vec<usize>,
    cross_size: f32,
    max_first_ascent: f32,
    max_last_ascent: f32,
}

#[inline]
fn resolve_gap(lp: &LengthPercentage, basis: Option<f32>, eval: &rix_css::EvalContext) -> f32 {
    if lp.has_percentage() && basis.is_none() {
        return 0.0;
    }
    lp.resolve_px(basis.unwrap_or(0.0), eval).max(0.0)
}

#[inline]
fn to_content(v: f32, pb: f32, is_bb: bool) -> f32 {
    if is_bb {
        (v - pb).max(0.0)
    } else {
        v.max(0.0)
    }
}

/// Measures raw content `(min_content_width, max_content_width)` of a flex container's children
/// without clamping to the container's own `width`/`min-width`/`max-width` or adding padding/border.
pub(crate) fn measure_flex_content_intrinsic_widths(
    ctx: &mut LayoutContext<'_>,
    container_id: BoxId,
    space: ConstraintSpace,
) -> (f32, f32) {
    let b = ctx.tree.get(container_id);
    let style = Arc::clone(&b.style);
    let eval = ctx.eval_context_for(&style, &space);

    let fg = style.flex_grid.as_deref();
    let flex_dir = fg.map_or(FlexDirectionMode::Row, |f| f.flex_direction);
    let flex_wrap = fg.map_or(FlexWrapMode::NoWrap, |f| f.flex_wrap);
    let axes = FlexAxes::compute(
        style.text.writing_mode,
        style.text.direction,
        flex_dir,
        flex_wrap,
    );
    let is_nowrap = flex_wrap == FlexWrapMode::NoWrap;
    let col_gap = fg.map_or(0.0, |f| {
        resolve_gap(&f.column_gap, Some(space.available_width), &eval)
    });

    let children = b.children.clone();
    let mut min_c = 0.0f32;
    let mut max_c = 0.0f32;
    let mut count = 0usize;

    for cid in children {
        let cb = ctx.tree.get(cid);
        if cb.is_out_of_flow {
            continue;
        }
        let cstyle = Arc::clone(&cb.style);
        let ceval = ctx.eval_context_for(&cstyle, &space);
        let cm = BoxMetrics::resolve_with_eval(&cstyle, &space, ceval);
        let cpb_w = cm.padding.horizontal() + cm.border.horizontal();
        let cbb = cstyle.box_sizing == BoxSizingMode::BorderBox;
        let (mut cmin, mut cmax) = measure_intrinsic_widths(ctx, cid, space);
        if axes.main_is_x {
            let cfg = cstyle.flex_grid.as_deref();
            let shrink = cfg.map_or(1.0, |f| f.flex_shrink);
            let grow = cfg.map_or(0.0, |f| f.flex_grow);
            // CSS Flexbox §9.9.1: consider definite flex-basis as hypothetical contribution.
            let def_fb = cfg.and_then(|f| match &f.flex_basis {
                LengthPercentageAuto::Value(lp) if !lp.has_percentage() => {
                    Some(to_content(lp.resolve_px(0.0, &ceval).max(0.0), cpb_w, cbb) + cpb_w)
                }
                _ => None,
            });
            if let Some(fb) = def_fb {
                let clamped_fb = fb.min(cm.max_width + cpb_w).max(cm.min_width + cpb_w);
                if grow == 0.0 {
                    cmax = cmax.min(clamped_fb);
                } else {
                    cmax = cmax.max(clamped_fb);
                }
                if shrink == 0.0 {
                    cmin = cmin.max(clamped_fb);
                } else if matches!(cstyle.box_model.min_width, SizeValue::Length(_)) {
                    cmin = cm.min_width + cpb_w;
                } else {
                    let (raw_cmin, _) =
                        crate::fragment::measure_content_intrinsic_widths(ctx, cid, space);
                    cmin = (raw_cmin + cpb_w).min(clamped_fb).max(cm.min_width + cpb_w);
                }
            }
        }
        let item_min = cmin + cm.margin.horizontal();
        let item_max = cmax + cm.margin.horizontal();
        if axes.main_is_x {
            if is_nowrap {
                min_c += item_min;
            } else {
                min_c = min_c.max(item_min);
            }
            max_c += item_max;
            count += 1;
        } else {
            min_c = min_c.max(item_min);
            max_c = max_c.max(item_max);
        }
    }
    if axes.main_is_x && count > 1 {
        let g = col_gap * (count - 1) as f32;
        max_c += g;
        if is_nowrap {
            min_c += g;
        }
    }
    (min_c, max_c.max(min_c))
}

/// Measures the `(min_content_width, max_content_width)` border-box intrinsic widths of a flex
/// container per CSS Flexbox L1 §9.9.
pub fn measure_flex_intrinsic_widths(
    ctx: &mut LayoutContext<'_>,
    container_id: BoxId,
    space: ConstraintSpace,
) -> (f32, f32) {
    let b = ctx.tree.get(container_id);
    let style = Arc::clone(&b.style);
    let eval = ctx.eval_context_for(&style, &space);
    let metrics = BoxMetrics::resolve_with_eval(&style, &space, eval);
    let pb_w = metrics.padding.horizontal() + metrics.border.horizontal();
    let is_bb = style.box_sizing == BoxSizingMode::BorderBox;

    let (min_c, max_c) = measure_flex_content_intrinsic_widths(ctx, container_id, space);
    let avail_c = (space.available_width - metrics.margin.horizontal() - pb_w).max(0.0);
    let stf = max_c.min(avail_c).max(min_c);
    let intr_fn = |basis: &rix_css::values_math::CalcSizeBasis| -> f32 {
        match basis {
            rix_css::values_math::CalcSizeBasis::MinContent => min_c,
            rix_css::values_math::CalcSizeBasis::MaxContent => max_c,
            rix_css::values_math::CalcSizeBasis::Stretch => avail_c,
            _ => stf,
        }
    };
    let eff_min = resolve_size_with_intrinsic(
        &style.box_model.min_width,
        Some(space.containing_block_width),
        pb_w,
        metrics.margin.horizontal(),
        is_bb,
        &eval,
        intr_fn,
    )
    .unwrap_or(metrics.min_width);
    let eff_max = resolve_size_with_intrinsic(
        &style.box_model.max_width,
        Some(space.containing_block_width),
        pb_w,
        metrics.margin.horizontal(),
        is_bb,
        &eval,
        intr_fn,
    )
    .unwrap_or(metrics.max_width);
    let spec_w = metrics.specified_width.or_else(|| {
        resolve_size_with_intrinsic(
            &style.box_model.width,
            Some(space.containing_block_width),
            pb_w,
            metrics.margin.horizontal(),
            is_bb,
            &eval,
            intr_fn,
        )
    });
    let final_min = spec_w.unwrap_or(min_c).min(eff_max).max(eff_min).max(0.0) + pb_w;
    let final_max = spec_w.unwrap_or(max_c).min(eff_max).max(eff_min).max(0.0) + pb_w;
    (final_min, final_max.max(final_min))
}

/// Lays out a CSS Flex (`display: flex | inline-flex`) container per W3C CSS Flexbox L1 §9.
pub fn layout_flex_container(
    ctx: &mut LayoutContext<'_>,
    container_id: BoxId,
    space: ConstraintSpace,
) -> Fragment {
    let container_box = ctx.tree.get(container_id);
    let style = Arc::clone(&container_box.style);
    let is_inline_flex = matches!(container_box.kind, BoxKind::InlineBlock { .. });
    let eval = ctx.eval_context_for(&style, &space);
    let metrics = BoxMetrics::resolve_with_eval(&style, &space, eval);
    let pb_w = metrics.padding.horizontal() + metrics.border.horizontal();
    let pb_h = metrics.padding.vertical() + metrics.border.vertical();
    let is_bb = style.box_sizing == BoxSizingMode::BorderBox;

    let fg = style.flex_grid.as_deref();
    let flex_dir = fg.map_or(FlexDirectionMode::Row, |f| f.flex_direction);
    let flex_wrap = fg.map_or(FlexWrapMode::NoWrap, |f| f.flex_wrap);
    let justify_content = fg.map_or(AlignValue::Normal, |f| f.justify_content);
    let align_items = fg.map_or(AlignValue::Normal, |f| f.align_items);
    let align_content = fg.map_or(AlignValue::Normal, |f| f.align_content);
    let axes = FlexAxes::compute(
        style.text.writing_mode,
        style.text.direction,
        flex_dir,
        flex_wrap,
    );

    // Separate in-flow items (sorted stably by CSS `order`) and direct out-of-flow children.
    let raw_children = container_box.children.clone();
    let mut in_flow: Vec<(i32, usize, BoxId)> = Vec::with_capacity(raw_children.len());
    let mut oof_children: Vec<BoxId> = Vec::new();
    for (idx, cid) in raw_children.into_iter().enumerate() {
        let cb = ctx.tree.get(cid);
        if cb.is_out_of_flow {
            oof_children.push(cid);
        } else {
            let order = cb.style.flex_grid.as_ref().map_or(0, |f| f.order);
            in_flow.push((order, idx, cid));
        }
    }
    in_flow.sort_by_key(|&(order, idx, _)| (order, idx));
    let ordered_ids: Vec<BoxId> = in_flow.into_iter().map(|(_, _, cid)| cid).collect();

    // Determine container definite / available width and height.
    // In vertical writing modes, `width: auto` is the block-size (content-sized from flex lines).
    let is_vert_wm = style.text.writing_mode != WritingMode::HorizontalTb;
    let shrink_w =
        is_inline_flex || container_box.is_float || container_box.is_out_of_flow || is_vert_wm;
    let avail_w_content = (space.available_width - metrics.margin.horizontal() - pb_w).max(0.0);
    let is_intr_kw = |sz: &SizeValue| {
        matches!(
            sz,
            SizeValue::MinContent
                | SizeValue::MaxContent
                | SizeValue::FitContent(_)
                | SizeValue::CalcSize(_)
        )
    };
    let need_intr_w = shrink_w
        || is_intr_kw(&style.box_model.width)
        || is_intr_kw(&style.box_model.min_width)
        || is_intr_kw(&style.box_model.max_width);
    let (intr_min_w, intr_max_w) = if need_intr_w {
        measure_flex_content_intrinsic_widths(ctx, container_id, space)
    } else {
        (0.0, 0.0)
    };
    let stf_w = intr_max_w.min(avail_w_content).max(intr_min_w);
    let intr_w_fn = |basis: &rix_css::values_math::CalcSizeBasis| -> f32 {
        match basis {
            rix_css::values_math::CalcSizeBasis::MinContent => intr_min_w,
            rix_css::values_math::CalcSizeBasis::MaxContent => intr_max_w,
            rix_css::values_math::CalcSizeBasis::Stretch => avail_w_content,
            _ => {
                if shrink_w {
                    stf_w
                } else {
                    avail_w_content
                }
            }
        }
    };
    let c_min_w = resolve_size_with_intrinsic(
        &style.box_model.min_width,
        Some(space.containing_block_width),
        pb_w,
        metrics.margin.horizontal(),
        is_bb,
        &eval,
        intr_w_fn,
    )
    .unwrap_or(metrics.min_width);
    let c_max_w = resolve_size_with_intrinsic(
        &style.box_model.max_width,
        Some(space.containing_block_width),
        pb_w,
        metrics.margin.horizontal(),
        is_bb,
        &eval,
        intr_w_fn,
    )
    .unwrap_or(metrics.max_width);
    let spec_w = metrics.specified_width.or_else(|| {
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
    let definite_w: Option<f32> = if let Some(sw) = spec_w {
        Some(sw.min(c_max_w).max(c_min_w).max(0.0))
    } else if !shrink_w {
        Some(avail_w_content.min(c_max_w).max(c_min_w).max(0.0))
    } else {
        None
    };

    let h_min_is_intr = is_intr_kw(&style.box_model.min_height);
    let h_max_is_intr = is_intr_kw(&style.box_model.max_height);
    let h_is_intr = is_intr_kw(&style.box_model.height);
    let definite_h: Option<f32> = if space.ignore_specified_height {
        None
    } else if let Some(ov_h) = space.definite_height_override {
        Some(
            ov_h.min(metrics.max_height)
                .max(metrics.min_height)
                .max(0.0),
        )
    } else if h_is_intr || h_min_is_intr || h_max_is_intr {
        None
    } else {
        metrics
            .specified_height
            .map(|h| h.min(metrics.max_height).max(metrics.min_height).max(0.0))
    };

    // Map container sizes & gaps to (main, cross).
    let (c_def_main, c_def_cross, c_min_main, c_max_main, c_min_cross, c_max_cross, c_avail_main) =
        if axes.main_is_x {
            (
                definite_w,
                definite_h,
                c_min_w,
                c_max_w,
                metrics.min_height,
                metrics.max_height,
                definite_w.unwrap_or(avail_w_content.min(c_max_w)),
            )
        } else {
            (
                definite_h,
                definite_w,
                metrics.min_height,
                metrics.max_height,
                c_min_w,
                c_max_w,
                definite_h.unwrap_or(metrics.max_height),
            )
        };

    let col_gap = fg.map_or(0.0, |f| resolve_gap(&f.column_gap, definite_w, &eval));
    let row_gap = fg.map_or(0.0, |f| resolve_gap(&f.row_gap, definite_h, &eval));
    let (main_gap, cross_gap) = if axes.main_is_x {
        (col_gap, row_gap)
    } else {
        (row_gap, col_gap)
    };

    let cb_for_items = ConstraintSpace {
        available_width: definite_w.unwrap_or(avail_w_content),
        available_height: definite_h,
        containing_block_width: definite_w.unwrap_or(avail_w_content),
        containing_block_height: definite_h.unwrap_or(space.containing_block_height),
        viewport_size: space.viewport_size,
        root_font_size_px: space.root_font_size_px,
        cb_inline_size: None,
        definite_width_override: None,
        definite_height_override: None,
        ignore_specified_height: false,
    };

    // §9.2: Determine flex base size and hypothetical main size for each item.
    let mut items: Vec<FlexItemState> = Vec::with_capacity(ordered_ids.len());
    for &cid in &ordered_ids {
        let cb = ctx.tree.get(cid);
        let istyle = Arc::clone(&cb.style);
        let ieval = ctx.eval_context_for(&istyle, &cb_for_items);
        let im = BoxMetrics::resolve_with_eval(&istyle, &cb_for_items, ieval);
        let ibb = istyle.box_sizing == BoxSizingMode::BorderBox;
        let ipb_w = im.padding.horizontal() + im.border.horizontal();
        let ipb_h = im.padding.vertical() + im.border.vertical();
        let ifg = istyle.flex_grid.as_deref();
        let grow = ifg.map_or(0.0, |f| f.flex_grow.max(0.0));
        let shrink = ifg.map_or(1.0, |f| f.flex_shrink.max(0.0));
        let align_self = match ifg.map_or(AlignValue::Auto, |f| f.align_self) {
            AlignValue::Auto => align_items,
            other => other,
        };
        let collapsed = istyle.visibility == VisibilityMode::Collapse;
        let ratio = box_aspect_ratio(ctx, cid, &istyle, &ieval);

        let m_left_auto = matches!(istyle.box_model.margin.left, LengthPercentageAuto::Auto);
        let m_right_auto = matches!(istyle.box_model.margin.right, LengthPercentageAuto::Auto);
        let m_top_auto = matches!(istyle.box_model.margin.top, LengthPercentageAuto::Auto);
        let m_bottom_auto = matches!(istyle.box_model.margin.bottom, LengthPercentageAuto::Auto);

        let (m_ms, m_me, m_ms_a, m_me_a) = if axes.main_is_x {
            if axes.main_reversed {
                (im.margin.right, im.margin.left, m_right_auto, m_left_auto)
            } else {
                (im.margin.left, im.margin.right, m_left_auto, m_right_auto)
            }
        } else if axes.main_reversed {
            (im.margin.bottom, im.margin.top, m_bottom_auto, m_top_auto)
        } else {
            (im.margin.top, im.margin.bottom, m_top_auto, m_bottom_auto)
        };
        let (m_cs, m_ce, m_cs_a, m_ce_a) = if axes.main_is_x {
            if axes.cross_reversed {
                (im.margin.bottom, im.margin.top, m_bottom_auto, m_top_auto)
            } else {
                (im.margin.top, im.margin.bottom, m_top_auto, m_bottom_auto)
            }
        } else if axes.cross_reversed {
            (im.margin.right, im.margin.left, m_right_auto, m_left_auto)
        } else {
            (im.margin.left, im.margin.right, m_left_auto, m_right_auto)
        };
        let (pb_main, pb_cross) = if axes.main_is_x {
            (ipb_w, ipb_h)
        } else {
            (ipb_h, ipb_w)
        };

        // Resolve specified cross size (needed for aspect-ratio transfer and cross sizing).
        let (raw_cross_sz, raw_min_cross, raw_max_cross, raw_main_sz, raw_min_main, raw_max_main) =
            if axes.main_is_x {
                (
                    &istyle.box_model.height,
                    &istyle.box_model.min_height,
                    &istyle.box_model.max_height,
                    &istyle.box_model.width,
                    &istyle.box_model.min_width,
                    &istyle.box_model.max_width,
                )
            } else {
                (
                    &istyle.box_model.width,
                    &istyle.box_model.min_width,
                    &istyle.box_model.max_width,
                    &istyle.box_model.height,
                    &istyle.box_model.min_height,
                    &istyle.box_model.max_height,
                )
            };

        let resolve_dim_opt = |sz: &SizeValue, basis: Option<f32>, pb: f32| -> Option<f32> {
            match sz {
                SizeValue::Length(lp) => {
                    if lp.has_percentage() && basis.is_none() {
                        None
                    } else {
                        Some(to_content(
                            lp.resolve_px(basis.unwrap_or(0.0), &ieval).max(0.0),
                            pb,
                            ibb,
                        ))
                    }
                }
                SizeValue::Stretch => basis.map(|b| (b - pb).max(0.0)),
                _ => None,
            }
        };

        let spec_cross = resolve_dim_opt(raw_cross_sz, c_def_cross, pb_cross);
        let min_cross = resolve_dim_opt(raw_min_cross, c_def_cross, pb_cross).unwrap_or(0.0);
        let max_cross =
            resolve_dim_opt(raw_max_cross, c_def_cross, pb_cross).unwrap_or(f32::INFINITY);
        let spec_main = resolve_dim_opt(raw_main_sz, c_def_main, pb_main);
        let max_main = resolve_dim_opt(raw_max_main, c_def_main, pb_main).unwrap_or(f32::INFINITY);

        // Compute raw content intrinsic widths along X (ignoring item's own width/min/max so
        // flex-shrink with `min-width: auto` can shrink below explicit `width`).
        let is_single_line = flex_wrap == FlexWrapMode::NoWrap;
        let stretched_def_cross = if is_single_line
            && spec_cross.is_none()
            && !m_cs_a
            && !m_ce_a
            && matches!(align_self.base(), AlignValue::Normal | AlignValue::Stretch)
        {
            c_def_cross.map(|dc| {
                (dc - m_cs - m_ce - pb_cross)
                    .max(0.0)
                    .min(max_cross)
                    .max(min_cross)
            })
        } else {
            None
        };
        let eff_def_cross = spec_cross.or(stretched_def_cross);
        let item_intr_space = if axes.main_is_x {
            if let Some(dc) = eff_def_cross {
                cb_for_items.with_available(cb_for_items.available_width, Some(dc))
            } else {
                cb_for_items
            }
        } else {
            cb_for_items
        };

        let nat_replaced = replaced_natural_size(ctx, cid);
        let is_replaced_box = matches!(ctx.tree.get(cid).kind, BoxKind::Replaced(_));
        let (content_min_w, content_max_w) = if let Some((nw, _)) = nat_replaced {
            (nw, nw)
        } else if is_replaced_box {
            let (iw, ix) = measure_intrinsic_widths(ctx, cid, item_intr_space);
            ((iw - ipb_w).max(0.0), (ix - ipb_w).max(0.0))
        } else {
            crate::fragment::measure_content_intrinsic_widths(ctx, cid, item_intr_space)
        };
        let intr_min_w_i = content_min_w + ipb_w;
        let intr_max_w_i = content_max_w + ipb_w;

        // Helper to measure item's raw content height at a given border-box width without leaking OOFs
        // or applying the item's own `height`/`min-height`/`max-height`.
        let measure_content_h = |ctx: &mut LayoutContext<'_>, probe_w: f32| -> f32 {
            let saved = ctx.pending_oof.len();
            let mut pspace = cb_for_items.with_available(probe_w.max(0.0), None);
            pspace.ignore_specified_height = true;
            let f = layout_box(ctx, cid, pspace);
            ctx.pending_oof.truncate(saved);
            (f.border_rect.size.height - ipb_h).max(0.0)
        };
        let col_probe_border_w = if let Some(sc) = eff_def_cross {
            sc.min(max_cross).max(min_cross) + pb_cross
        } else if let Some(cw) = c_def_cross {
            intr_max_w_i
                .min((cw - m_cs - m_ce).max(0.0))
                .max(intr_min_w_i)
        } else {
            intr_max_w_i
        };
        let mut cached_col_content_h: Option<f32> = None;
        let mut get_col_content_h = |ctx: &mut LayoutContext<'_>| -> f32 {
            if let Some(h) = cached_col_content_h {
                return h;
            }
            let h = if let Some((_, nh)) = nat_replaced {
                if let (Some(r), Some(dc)) = (ratio, eff_def_cross) {
                    dc / r
                } else {
                    nh
                }
            } else {
                let raw_h = measure_content_h(ctx, col_probe_border_w);
                if let Some(r) = ratio {
                    if raw_h == 0.0 && col_probe_border_w > pb_cross {
                        (col_probe_border_w - pb_cross) / r
                    } else {
                        raw_h
                    }
                } else {
                    raw_h
                }
            };
            cached_col_content_h = Some(h);
            h
        };

        // §9.2 step 3: Compute Flex Base Size (content-box main size).
        let fb_size = ifg.and_then(|f| f.flex_basis_size.as_ref());
        let fb_lpa = ifg
            .map(|f| &f.flex_basis)
            .unwrap_or(&LengthPercentageAuto::Auto);

        let definite_fb: Option<f32> = if let Some(sz) = fb_size {
            match sz {
                SizeValue::Content => None,
                SizeValue::MinContent => Some(if axes.main_is_x {
                    content_min_w
                } else {
                    get_col_content_h(ctx)
                }),
                SizeValue::MaxContent => Some(if axes.main_is_x {
                    content_max_w
                } else {
                    get_col_content_h(ctx)
                }),
                SizeValue::FitContent(lim) => Some(if axes.main_is_x {
                    let cap = lim
                        .as_ref()
                        .map_or((c_avail_main - pb_main).max(0.0), |lp| {
                            to_content(
                                lp.resolve_px(c_def_main.unwrap_or(0.0), &ieval).max(0.0),
                                pb_main,
                                ibb,
                            )
                        });
                    content_max_w.min(cap).max(content_min_w)
                } else {
                    get_col_content_h(ctx)
                }),
                SizeValue::CalcSize(cs) => {
                    let nat_c = if axes.main_is_x {
                        content_max_w
                    } else {
                        get_col_content_h(ctx)
                    };
                    let box_intr = |b: &rix_css::values_math::CalcSizeBasis| -> f32 {
                        let c = match b {
                            rix_css::values_math::CalcSizeBasis::MinContent if axes.main_is_x => {
                                content_min_w
                            }
                            rix_css::values_math::CalcSizeBasis::MaxContent if axes.main_is_x => {
                                content_max_w
                            }
                            rix_css::values_math::CalcSizeBasis::Stretch => {
                                (c_avail_main - m_ms - m_me - pb_main).max(0.0)
                            }
                            _ => nat_c,
                        };
                        if ibb {
                            c + pb_main
                        } else {
                            c
                        }
                    };
                    let raw = cs.eval_px(c_def_main, &ieval, &box_intr);
                    Some(to_content(raw, pb_main, ibb))
                }
                other => resolve_dim_opt(other, c_def_main, pb_main),
            }
        } else {
            match fb_lpa {
                LengthPercentageAuto::Auto => None,
                LengthPercentageAuto::Value(lp) => {
                    if lp.has_percentage() && c_def_main.is_none() {
                        None
                    } else {
                        Some(to_content(
                            lp.resolve_px(c_def_main.unwrap_or(0.0), &ieval).max(0.0),
                            pb_main,
                            ibb,
                        ))
                    }
                }
            }
        };

        let is_fb_content_explicit = matches!(fb_size, Some(SizeValue::Content));
        let flex_base_size = if let Some(dfb) = definite_fb {
            dfb
        } else if let Some(sm) = spec_main.filter(|_| !is_fb_content_explicit) {
            // `flex-basis: auto` with definite main size property.
            sm
        } else if let (Some(r), Some(dc)) = (ratio, eff_def_cross) {
            // §9.2 step 3B: transfer definite cross size (explicit or definite stretched) through intrinsic aspect ratio.
            let clamped_c = dc.min(max_cross).max(min_cross);
            if axes.main_is_x {
                clamped_c * r
            } else {
                clamped_c / r
            }
        } else if !axes.main_is_x {
            // Column flex item with auto/content height.
            get_col_content_h(ctx)
        } else {
            // Row flex item with auto/content width: max-content width.
            content_max_w
        };

        // §4.5: Implied Minimum Size of Flex Items (`min-main: auto` with `overflow == visible`).
        // CSS Sizing 3 §5.2.1: Compressible replaced form controls with percentage main size
        // resolve the percentage part to zero when computing automatic minimum size.
        let compressible_pct_min: Option<f32> = match (&ctx.tree.get(cid).kind, raw_main_sz) {
            (BoxKind::Replaced(ReplacedKind::FormControl { kind, .. }), SizeValue::Length(lp))
                if lp.has_percentage()
                    && matches!(
                        kind,
                        crate::box_tree::FormControlKind::TextInput
                            | crate::box_tree::FormControlKind::TextArea
                            | crate::box_tree::FormControlKind::Select
                    ) =>
            {
                Some(to_content(
                    lp.resolve_px(0.0, &ieval).max(0.0),
                    pb_main,
                    ibb,
                ))
            }
            _ => None,
        };
        let overflow_blocks_auto_min = if axes.main_is_x {
            istyle.overflow.x != OverflowMode::Visible
        } else {
            istyle.overflow.y != OverflowMode::Visible
        };
        let min_main =
            if let Some(explicit_min) = resolve_dim_opt(raw_min_main, c_def_main, pb_main) {
                explicit_min
            } else if matches!(raw_min_main, SizeValue::MinContent) {
                if axes.main_is_x {
                    content_min_w
                } else {
                    get_col_content_h(ctx)
                }
            } else if matches!(raw_min_main, SizeValue::MaxContent) {
                if axes.main_is_x {
                    content_max_w
                } else {
                    get_col_content_h(ctx)
                }
            } else if matches!(raw_min_main, SizeValue::Auto) && !overflow_blocks_auto_min {
                if let Some(comp_min) = compressible_pct_min {
                    comp_min.min(max_main).max(0.0)
                } else {
                    let content_sugg = if axes.main_is_x {
                        content_min_w
                    } else {
                        get_col_content_h(ctx)
                    };
                    let transferred_sugg = ratio.and_then(|r| {
                        let c_hi = spec_cross.map_or(max_cross, |s| s.min(max_cross));
                        if c_hi.is_finite() {
                            let t = if axes.main_is_x { c_hi * r } else { c_hi / r };
                            Some(t.min(max_main))
                        } else {
                            None
                        }
                    });
                    let mut auto_min = content_sugg.min(max_main);
                    if let Some(sm) = spec_main {
                        auto_min = auto_min.min(sm);
                    }
                    if let Some(ts) = transferred_sugg {
                        auto_min = auto_min.min(ts);
                    }
                    auto_min.max(0.0)
                }
            } else {
                0.0
            };

        // §9.2 step 4: Hypothetical main size clamps flex base size by [min_main, max_main],
        // plus transferred cross min/max constraints when item has an intrinsic aspect ratio
        // and no definite main/cross size.
        let (hypo_min_clamp, hypo_max_clamp) = if let Some(r) = ratio {
            if spec_cross.is_none() && definite_fb.is_none() && spec_main.is_none() {
                let t_min = if min_cross > 0.0 {
                    if axes.main_is_x {
                        min_cross * r
                    } else {
                        min_cross / r
                    }
                } else {
                    0.0
                };
                let t_max = if max_cross.is_finite() {
                    if axes.main_is_x {
                        max_cross * r
                    } else {
                        max_cross / r
                    }
                } else {
                    f32::INFINITY
                };
                (
                    min_main.max(t_min.min(max_main)),
                    max_main.min(t_max.max(min_main)),
                )
            } else {
                (min_main, max_main)
            }
        } else {
            (min_main, max_main)
        };
        let hypo_main = flex_base_size
            .min(hypo_max_clamp)
            .max(hypo_min_clamp)
            .max(0.0);

        items.push(FlexItemState {
            box_id: cid,
            style: istyle,
            pb_main,
            pb_cross,
            margin_main_start: m_ms,
            margin_main_end: m_me,
            margin_cross_start: m_cs,
            margin_cross_end: m_ce,
            margin_main_start_auto: m_ms_a,
            margin_main_end_auto: m_me_a,
            margin_cross_start_auto: m_cs_a,
            margin_cross_end_auto: m_ce_a,
            flex_grow: grow,
            flex_shrink: shrink,
            flex_base_size,
            hypothetical_main_size: hypo_main,
            target_main_size: hypo_main,
            min_main,
            max_main,
            min_cross,
            max_cross,
            spec_cross,
            aspect_ratio: ratio,
            content_min_w,
            content_max_w,
            had_def_cross_pass1: eff_def_cross.is_some(),
            align_self,
            collapsed,
            frozen: false,
            outer_cross_size: 0.0,
            used_cross_border: 0.0,
            first_baseline: 0.0,
            last_baseline: 0.0,
            frag: None,
            oof_range: (0, 0),
        });
    }

    // §9.3: Collect flex items into flex lines.
    let is_multi_line = flex_wrap != FlexWrapMode::NoWrap;
    let wrap_limit = if is_multi_line {
        c_def_main.unwrap_or(c_avail_main)
    } else {
        f32::INFINITY
    };

    let mut lines: Vec<FlexLine> = Vec::new();
    if !items.is_empty() {
        let mut cur_line: Vec<usize> = Vec::new();
        let mut line_main = 0.0f32;
        for (idx, item) in items.iter().enumerate() {
            let outer_m = item.outer_hypo_main();
            let gap_add = if cur_line.is_empty() { 0.0 } else { main_gap };
            if is_multi_line
                && !cur_line.is_empty()
                && line_main + gap_add + outer_m > wrap_limit + 0.01
            {
                lines.push(FlexLine {
                    item_indices: cur_line,
                    cross_size: 0.0,
                    max_first_ascent: 0.0,
                    max_last_ascent: 0.0,
                });
                cur_line = vec![idx];
                line_main = outer_m;
            } else {
                line_main += gap_add + outer_m;
                cur_line.push(idx);
            }
        }
        if !cur_line.is_empty() {
            lines.push(FlexLine {
                item_indices: cur_line,
                cross_size: 0.0,
                max_first_ascent: 0.0,
                max_last_ascent: 0.0,
            });
        }
    }

    // Determine container's resolved main inner size before resolving flexible lengths.
    let natural_main = lines
        .iter()
        .map(|line| {
            let sum: f32 = line
                .item_indices
                .iter()
                .map(|&i| items[i].outer_hypo_main())
                .sum();
            let vis = line
                .item_indices
                .iter()
                .filter(|&&i| !items[i].collapsed)
                .count();
            sum + main_gap * vis.saturating_sub(1) as f32
        })
        .fold(0.0f32, f32::max);
    let container_main = c_def_main
        .unwrap_or_else(|| natural_main.min(c_max_main).max(c_min_main))
        .max(0.0);

    // §9.7: Resolve Flexible Lengths on each line.
    for line in &lines {
        resolve_flexible_lengths(&mut items, &line.item_indices, container_main, main_gap);
    }

    // §9.4: Cross Size Determination — lay out every item at its final main size to get cross size & baselines.
    let my_oof_start = ctx.pending_oof.len();
    for line in &mut lines {
        for &idx in &line.item_indices {
            let (
                box_id,
                main_b,
                pb_m,
                pb_c,
                spec_c,
                min_c,
                max_c,
                ratio,
                m_cs,
                m_ce,
                cmin_w,
                cmax_w,
            ) = {
                let it = &items[idx];
                (
                    it.box_id,
                    it.target_border_main(),
                    it.pb_main,
                    it.pb_cross,
                    it.spec_cross,
                    it.min_cross,
                    it.max_cross,
                    it.aspect_ratio,
                    it.margin_cross_start,
                    it.margin_cross_end,
                    it.content_min_w,
                    it.content_max_w,
                )
            };
            let derived_cross_c = spec_c.or_else(|| {
                ratio.map(|r| {
                    let main_c = (main_b - pb_m).max(0.0);
                    let raw_c = if axes.main_is_x {
                        main_c / r
                    } else {
                        main_c * r
                    };
                    raw_c.min(max_c).max(min_c).max(0.0)
                })
            });
            // CSS Flexbox §9.4 step 7: determine hypothetical cross size by laying out each item
            // at its resolved main size; for column flex items with auto cross width, use fit-content
            // so `align-self: stretch` items don't inflate multi-line column cross sizes before step 11.
            let (item_avail_w, item_avail_h) = if axes.main_is_x {
                (main_b, derived_cross_c.map(|c| c + pb_c))
            } else {
                let w = if let Some(dc) = derived_cross_c {
                    dc + pb_c
                } else {
                    let avail_c =
                        (c_def_cross.unwrap_or(cb_for_items.available_width) - m_cs - m_ce - pb_c)
                            .max(0.0);
                    let fit_c = cmax_w.min(avail_c).max(cmin_w).min(max_c).max(min_c);
                    fit_c + pb_c
                };
                (w, Some(main_b))
            };
            let mut ispace = cb_for_items;
            ispace.available_width = item_avail_w.max(0.0);
            ispace.available_height = item_avail_h;
            // CSS Flexbox §9.8: a flex item's resolved main size in a definite-height column flex
            // (or explicit flex-basis/height), or definite cross size in a row flex, is definite for its children.
            ispace.definite_height_override = if axes.main_is_x {
                derived_cross_c
            } else if c_def_main.is_some()
                || items[idx].style.box_model.height != SizeValue::Auto
                || items[idx].style.flex_grid.as_deref().is_some_and(|f| {
                    !matches!(f.flex_basis, LengthPercentageAuto::Auto)
                        || f.flex_basis_size.is_some()
                })
            {
                Some((main_b - pb_m).max(0.0))
            } else {
                None
            };
            items[idx].had_def_cross_pass1 = derived_cross_c.is_some();

            let oof_s = ctx.pending_oof.len();
            let mut frag = layout_box(ctx, box_id, ispace);
            let oof_e = ctx.pending_oof.len();

            // Pin main axis to exact resolved flex main size.
            if axes.main_is_x {
                frag.border_rect.size.width = main_b;
                frag.content_rect.size.width = (main_b - pb_m).max(0.0);
                if let Some(dc) = derived_cross_c {
                    let clamped = dc.min(max_c).max(min_c).max(0.0);
                    frag.border_rect.size.height = clamped + pb_c;
                    frag.content_rect.size.height = clamped;
                } else {
                    let ch = (frag.border_rect.size.height - pb_c)
                        .max(0.0)
                        .min(max_c)
                        .max(min_c);
                    frag.border_rect.size.height = ch + pb_c;
                    frag.content_rect.size.height = ch;
                }
            } else {
                frag.border_rect.size.height = main_b;
                frag.content_rect.size.height = (main_b - pb_m).max(0.0);
                if let Some(dc) = derived_cross_c {
                    let clamped = dc.min(max_c).max(min_c).max(0.0);
                    frag.border_rect.size.width = clamped + pb_c;
                    frag.content_rect.size.width = clamped;
                } else {
                    let cw = (frag.border_rect.size.width - pb_c)
                        .max(0.0)
                        .min(max_c)
                        .max(min_c);
                    frag.border_rect.size.width = cw + pb_c;
                    frag.content_rect.size.width = cw;
                }
            }

            let cross_b = if axes.main_is_x {
                frag.border_rect.size.height
            } else {
                frag.border_rect.size.width
            };
            let (first_bl, last_bl) = if axes.main_is_x && !is_vert_wm {
                (
                    frag.baseline.unwrap_or(cross_b),
                    cross_b - frag.last_baseline.or(frag.baseline).unwrap_or(cross_b),
                )
            } else if !axes.main_is_x && is_vert_wm {
                let item_f = frag.baseline.unwrap_or(cross_b * 0.5);
                let item_l = frag
                    .last_baseline
                    .or(frag.baseline)
                    .unwrap_or(cross_b * 0.5);
                if axes.cross_reversed == axes.wm_cross_reversed {
                    (item_f, cross_b - item_l)
                } else {
                    (cross_b - item_f, item_l)
                }
            } else {
                (cross_b * 0.5, cross_b * 0.5)
            };

            let it = &mut items[idx];
            it.used_cross_border = cross_b;
            it.outer_cross_size = cross_b + m_cs + m_ce;
            it.first_baseline = first_bl;
            it.last_baseline = last_bl;
            it.frag = Some(frag);
            it.oof_range = (oof_s, oof_e);
        }

        // Compute line cross size per §9.4 step 8.
        let mut max_non_bl = 0.0f32;
        let mut max_f_asc = 0.0f32;
        let mut max_f_desc = 0.0f32;
        let mut has_f_bl = false;
        let mut max_l_asc = 0.0f32;
        let mut max_l_desc = 0.0f32;
        let mut has_l_bl = false;

        for &idx in &line.item_indices {
            let it = &items[idx];
            match it.align_self.base() {
                AlignValue::Baseline
                    if !it.margin_cross_start_auto && !it.margin_cross_end_auto =>
                {
                    has_f_bl = true;
                    let asc = it.margin_cross_start + it.first_baseline;
                    let desc = (it.used_cross_border - it.first_baseline) + it.margin_cross_end;
                    max_f_asc = max_f_asc.max(asc);
                    max_f_desc = max_f_desc.max(desc);
                }
                AlignValue::LastBaseline
                    if !it.margin_cross_start_auto && !it.margin_cross_end_auto =>
                {
                    has_l_bl = true;
                    let desc_from_end = it.margin_cross_end + it.last_baseline;
                    let above = (it.used_cross_border - it.last_baseline) + it.margin_cross_start;
                    max_l_asc = max_l_asc.max(desc_from_end);
                    max_l_desc = max_l_desc.max(above);
                }
                _ => {
                    max_non_bl = max_non_bl.max(it.outer_cross_size);
                }
            }
        }
        let mut line_cross = max_non_bl;
        if has_f_bl {
            line_cross = line_cross.max(max_f_asc + max_f_desc);
        }
        if has_l_bl {
            line_cross = line_cross.max(max_l_asc + max_l_desc);
        }
        line.cross_size = line_cross.max(0.0);
        line.max_first_ascent = max_f_asc;
        line.max_last_ascent = max_l_asc;
    }

    // §9.4 step 8 special case: single-line container with definite cross size clamps single line to container cross size.
    if !is_multi_line && lines.len() == 1 {
        if let Some(dc) = c_def_cross {
            lines[0].cross_size = dc.max(0.0);
        }
    }

    // §9.4 step 9: Handle `align-content: stretch` (and `normal`) for multi-line containers with definite cross size.
    let lines_cross_sum: f32 = lines.iter().map(|l| l.cross_size).sum::<f32>()
        + cross_gap * lines.len().saturating_sub(1) as f32;
    let resolve_intr_h_dim = |sz: &SizeValue, fallback: f32, nat_h: f32| -> f32 {
        resolve_size_with_intrinsic(
            sz,
            space.available_height,
            pb_h,
            metrics.margin.vertical(),
            is_bb,
            &eval,
            |_| nat_h,
        )
        .unwrap_or(fallback)
    };
    let (container_main, container_cross) = if axes.main_is_x {
        let nat_h = lines_cross_sum;
        let eff_min_h = if h_min_is_intr {
            resolve_intr_h_dim(&style.box_model.min_height, c_min_cross, nat_h)
        } else {
            c_min_cross
        };
        let eff_max_h = if h_max_is_intr {
            resolve_intr_h_dim(&style.box_model.max_height, c_max_cross, nat_h)
        } else {
            c_max_cross
        };
        let cc = if h_is_intr || h_min_is_intr || h_max_is_intr {
            let raw_h = if h_is_intr {
                resolve_intr_h_dim(&style.box_model.height, nat_h, nat_h)
            } else {
                metrics.specified_height.unwrap_or(nat_h)
            };
            raw_h.min(eff_max_h).max(eff_min_h).max(0.0)
        } else {
            c_def_cross
                .unwrap_or_else(|| lines_cross_sum.min(eff_max_h).max(eff_min_h))
                .max(0.0)
        };
        (container_main, cc)
    } else {
        let nat_h = natural_main;
        let eff_min_h = if h_min_is_intr {
            resolve_intr_h_dim(&style.box_model.min_height, c_min_main, nat_h)
        } else {
            c_min_main
        };
        let eff_max_h = if h_max_is_intr {
            resolve_intr_h_dim(&style.box_model.max_height, c_max_main, nat_h)
        } else {
            c_max_main
        };
        let cm = if h_is_intr || h_min_is_intr || h_max_is_intr {
            let raw_h = if h_is_intr {
                resolve_intr_h_dim(&style.box_model.height, nat_h, nat_h)
            } else {
                metrics.specified_height.unwrap_or(nat_h)
            };
            raw_h.min(eff_max_h).max(eff_min_h).max(0.0)
        } else {
            container_main
        };
        let cc = c_def_cross
            .unwrap_or_else(|| lines_cross_sum.min(c_max_cross).max(c_min_cross))
            .max(0.0);
        (cm, cc)
    };

    if is_multi_line
        && !lines.is_empty()
        && matches!(
            align_content.base(),
            AlignValue::Normal | AlignValue::Stretch
        )
    {
        let leftover = container_cross - lines_cross_sum;
        if leftover > 0.0 {
            let add = leftover / lines.len() as f32;
            for line in &mut lines {
                line.cross_size += add;
            }
        }
    }

    // §9.4 step 11: Stretch items with `align-self: stretch | normal`, auto cross size, and non-auto cross margins.
    for line in &lines {
        let l_cross = line.cross_size;
        for &idx in &line.item_indices {
            let should_stretch = {
                let it = &items[idx];
                !it.collapsed
                    && it.spec_cross.is_none()
                    && !it.margin_cross_start_auto
                    && !it.margin_cross_end_auto
                    && matches!(
                        it.align_self.base(),
                        AlignValue::Normal | AlignValue::Stretch
                    )
            };
            if !should_stretch {
                continue;
            }
            let (box_id, main_b, pb_c, min_c, max_c, m_cs, m_ce, had_def_c) = {
                let it = &items[idx];
                (
                    it.box_id,
                    it.target_border_main(),
                    it.pb_cross,
                    it.min_cross,
                    it.max_cross,
                    it.margin_cross_start,
                    it.margin_cross_end,
                    it.had_def_cross_pass1,
                )
            };
            let target_cross_c = (l_cross - m_cs - m_ce - pb_c)
                .max(0.0)
                .min(max_c)
                .max(min_c);
            let target_cross_b = target_cross_c + pb_c;
            let need_relayout_for_pct = !had_def_c && !ctx.tree.get(box_id).children.is_empty();
            if (target_cross_b - items[idx].used_cross_border).abs() > 0.25 || need_relayout_for_pct
            {
                // Re-layout item at definite stretched cross size so percentage cross descendants resolve accurately!
                let (rw, rh) = if axes.main_is_x {
                    (main_b, Some(target_cross_b))
                } else {
                    (target_cross_b, Some(main_b))
                };
                let saved_oof = ctx.pending_oof.len();
                let mut rspace = cb_for_items;
                rspace.available_width = rw.max(0.0);
                rspace.available_height = rh;
                rspace.definite_height_override = if axes.main_is_x {
                    Some(target_cross_c)
                } else if c_def_main.is_some()
                    || items[idx].style.box_model.height != SizeValue::Auto
                    || items[idx].style.flex_grid.as_deref().is_some_and(|f| {
                        !matches!(f.flex_basis, LengthPercentageAuto::Auto)
                            || f.flex_basis_size.is_some()
                    })
                {
                    Some((main_b - items[idx].pb_main).max(0.0))
                } else {
                    None
                };
                let mut rfrag = layout_box(ctx, box_id, rspace);
                // Replace old pending OOFs for this item cleanly.
                let new_oof_end = ctx.pending_oof.len();
                let (old_s, old_e) = items[idx].oof_range;
                if old_e > old_s {
                    // Only safe to discard new if old already captured them; keep new and drop old only if same span.
                    ctx.pending_oof.truncate(saved_oof);
                } else {
                    items[idx].oof_range = (saved_oof, new_oof_end);
                }

                let pb_m = items[idx].pb_main;
                if axes.main_is_x {
                    rfrag.border_rect.size.width = main_b;
                    rfrag.content_rect.size.width = (main_b - pb_m).max(0.0);
                    rfrag.border_rect.size.height = target_cross_b;
                    rfrag.content_rect.size.height = target_cross_c;
                } else {
                    rfrag.border_rect.size.height = main_b;
                    rfrag.content_rect.size.height = (main_b - pb_m).max(0.0);
                    rfrag.border_rect.size.width = target_cross_b;
                    rfrag.content_rect.size.width = target_cross_c;
                }
                items[idx].used_cross_border = target_cross_b;
                items[idx].outer_cross_size = target_cross_b + m_cs + m_ce;
                items[idx].frag = Some(rfrag);
            }
        }
    }

    // Final container border-box geometry & auto margins on container itself.
    let (final_content_w, final_content_h) = if axes.main_is_x {
        (container_main, container_cross)
    } else {
        (container_cross, container_main)
    };
    let border_w = final_content_w + pb_w;
    let border_h = final_content_h + pb_h;
    let content_origin_x = metrics.border.left + metrics.padding.left;
    let content_origin_y = metrics.border.top + metrics.padding.top;

    let (resolved_ml, resolved_mr) =
        if !shrink_w && (metrics.margin_left_auto || metrics.margin_right_auto) {
            let rem = (space.available_width - border_w).max(0.0);
            match (metrics.margin_left_auto, metrics.margin_right_auto) {
                (true, true) => (rem * 0.5, rem * 0.5),
                (true, false) => ((rem - metrics.margin.right).max(0.0), metrics.margin.right),
                (false, true) => (metrics.margin.left, (rem - metrics.margin.left).max(0.0)),
                (false, false) => (metrics.margin.left, metrics.margin.right),
            }
        } else {
            (metrics.margin.left, metrics.margin.right)
        };
    let resolved_margin = EdgeInsets {
        top: metrics.margin.top,
        right: resolved_mr,
        bottom: metrics.margin.bottom,
        left: resolved_ml,
    };

    // §9.6 step 16: Align flex lines along cross axis using `align-content` (multi-line only).
    let lines_sum_after_stretch: f32 = lines.iter().map(|l| l.cross_size).sum();
    let (mut line_cross_cursor, eff_cross_gap) = if is_multi_line && !lines.is_empty() {
        distribute_content_positions(
            container_cross,
            lines_sum_after_stretch,
            cross_gap,
            lines.len(),
            align_content,
            !axes.main_is_x,
            axes.cross_reversed,
            axes.wm_cross_reversed,
        )
    } else {
        (0.0, cross_gap)
    };

    // Place all items across all lines (§9.5 main-axis alignment & §9.6 cross-axis alignment).
    let mut placed_children: Vec<Fragment> = Vec::with_capacity(items.len() + oof_children.len());
    let mut container_first_baseline: Option<f32> = None;
    let mut container_last_baseline: Option<f32> = None;

    for (line_idx, line) in lines.iter().enumerate() {
        let l_cross = line.cross_size;
        let line_cross_pos = line_cross_cursor;
        line_cross_cursor += l_cross + eff_cross_gap;

        // Visible items for main-axis gap & alignment.
        let vis_count = line
            .item_indices
            .iter()
            .filter(|&&i| !items[i].collapsed)
            .count();
        let total_outer_main: f32 = line
            .item_indices
            .iter()
            .map(|&i| items[i].outer_target_main())
            .sum();
        let auto_main_count: usize = line
            .item_indices
            .iter()
            .filter(|&&i| !items[i].collapsed)
            .map(|&i| {
                usize::from(items[i].margin_main_start_auto)
                    + usize::from(items[i].margin_main_end_auto)
            })
            .sum();
        let free_main =
            container_main - total_outer_main - main_gap * vis_count.saturating_sub(1) as f32;

        let (mut main_cursor, eff_main_gap, per_auto_margin) =
            if free_main > 0.0 && auto_main_count > 0 {
                (0.0, main_gap, free_main / auto_main_count as f32)
            } else {
                let (off, g) = distribute_content_positions(
                    container_main,
                    total_outer_main,
                    main_gap,
                    vis_count,
                    justify_content,
                    axes.main_is_x,
                    axes.main_reversed,
                    axes.wm_main_reversed,
                );
                (off, g, 0.0)
            };

        let mut line_first_bl_shared: Option<f32> = None;
        let mut line_last_bl_shared: Option<f32> = None;
        let mut line_items_baselines: Vec<(f32, f32)> = Vec::with_capacity(line.item_indices.len());

        for &idx in &line.item_indices {
            let it = &mut items[idx];
            let m_ms = if it.margin_main_start_auto {
                per_auto_margin
            } else {
                it.margin_main_start
            };
            let m_me = if it.margin_main_end_auto {
                per_auto_margin
            } else {
                it.margin_main_end
            };
            let item_border_main = it.target_border_main();

            // Logical main border-box start from flex-start edge:
            let logical_main_start = main_cursor + m_ms;
            if !it.collapsed {
                main_cursor += m_ms + item_border_main + m_me + eff_main_gap;
            }
            // Convert logical main coordinate to physical offset from content-box min edge:
            let phys_main = if axes.main_reversed {
                container_main - logical_main_start - item_border_main
            } else {
                logical_main_start
            };

            // Cross-axis alignment within the line (`[line_cross_pos .. line_cross_pos + l_cross]`):
            let free_cross =
                l_cross - it.used_cross_border - it.margin_cross_start - it.margin_cross_end;
            let (m_cs, m_ce, logical_cross_in_line) =
                if it.margin_cross_start_auto || it.margin_cross_end_auto {
                    match (it.margin_cross_start_auto, it.margin_cross_end_auto) {
                        (true, true) => {
                            if free_cross >= 0.0 {
                                (free_cross * 0.5, free_cross * 0.5, free_cross * 0.5)
                            } else {
                                (0.0, free_cross, 0.0)
                            }
                        }
                        (true, false) => (
                            free_cross.max(0.0),
                            it.margin_cross_end,
                            free_cross.max(0.0),
                        ),
                        (false, true) => (
                            it.margin_cross_start,
                            free_cross.max(0.0),
                            it.margin_cross_start,
                        ),
                        (false, false) => unreachable!(),
                    }
                } else {
                    let mcs = it.margin_cross_start;
                    let mce = it.margin_cross_end;
                    let is_safe = it.align_self.is_safe();
                    let wm_same_as_flex = axes.wm_cross_reversed == axes.cross_reversed;
                    let item_self_rev = axes
                        .item_cross_reversed(it.style.text.writing_mode, it.style.text.direction);
                    let self_same_as_flex = item_self_rev == axes.cross_reversed;
                    let pos_in_line = if is_safe && free_cross < 0.0 {
                        if wm_same_as_flex {
                            mcs
                        } else {
                            l_cross - it.used_cross_border - mce
                        }
                    } else {
                        match it.align_self.base() {
                            AlignValue::Normal
                            | AlignValue::Auto
                            | AlignValue::Stretch
                            | AlignValue::FlexStart => mcs,
                            AlignValue::FlexEnd => l_cross - it.used_cross_border - mce,
                            AlignValue::Center => mcs + free_cross * 0.5,
                            AlignValue::Start => {
                                if wm_same_as_flex {
                                    mcs
                                } else {
                                    l_cross - it.used_cross_border - mce
                                }
                            }
                            AlignValue::End => {
                                if wm_same_as_flex {
                                    l_cross - it.used_cross_border - mce
                                } else {
                                    mcs
                                }
                            }
                            AlignValue::SelfStart => {
                                if self_same_as_flex {
                                    mcs
                                } else {
                                    l_cross - it.used_cross_border - mce
                                }
                            }
                            AlignValue::SelfEnd => {
                                if self_same_as_flex {
                                    l_cross - it.used_cross_border - mce
                                } else {
                                    mcs
                                }
                            }
                            AlignValue::Left => {
                                if !axes.main_is_x {
                                    if axes.cross_reversed {
                                        l_cross - it.used_cross_border - mce
                                    } else {
                                        mcs
                                    }
                                } else if wm_same_as_flex {
                                    mcs
                                } else {
                                    l_cross - it.used_cross_border - mce
                                }
                            }
                            AlignValue::Right => {
                                if !axes.main_is_x {
                                    if axes.cross_reversed {
                                        mcs
                                    } else {
                                        l_cross - it.used_cross_border - mce
                                    }
                                } else if wm_same_as_flex {
                                    mcs
                                } else {
                                    l_cross - it.used_cross_border - mce
                                }
                            }
                            AlignValue::Baseline => {
                                let off = (line.max_first_ascent - it.first_baseline).max(mcs);
                                line_first_bl_shared = Some(line_cross_pos + line.max_first_ascent);
                                off
                            }
                            AlignValue::LastBaseline => {
                                let from_end = (line.max_last_ascent - it.last_baseline).max(mce);
                                line_last_bl_shared =
                                    Some(line_cross_pos + l_cross - line.max_last_ascent);
                                l_cross - it.used_cross_border - from_end
                            }
                            _ => mcs,
                        }
                    };
                    (mcs, mce, pos_in_line)
                };

            let logical_cross_total = line_cross_pos + logical_cross_in_line;
            let phys_cross = if axes.cross_reversed {
                container_cross - logical_cross_total - it.used_cross_border
            } else {
                logical_cross_total
            };

            let (item_x, item_y) = if axes.main_is_x {
                (content_origin_x + phys_main, content_origin_y + phys_cross)
            } else {
                (content_origin_x + phys_cross, content_origin_y + phys_main)
            };

            let mut frag = it.frag.take().expect("flex item fragment laid out");
            // Update resolved physical margins on the fragment.
            frag.margin = if axes.main_is_x {
                let (ml, mr) = if axes.main_reversed {
                    (m_me, m_ms)
                } else {
                    (m_ms, m_me)
                };
                let (mt, mb) = if axes.cross_reversed {
                    (m_ce, m_cs)
                } else {
                    (m_cs, m_ce)
                };
                EdgeInsets {
                    top: mt,
                    right: mr,
                    bottom: mb,
                    left: ml,
                }
            } else {
                let (mt, mb) = if axes.main_reversed {
                    (m_me, m_ms)
                } else {
                    (m_ms, m_me)
                };
                let (ml, mr) = if axes.cross_reversed {
                    (m_ce, m_cs)
                } else {
                    (m_cs, m_ce)
                };
                EdgeInsets {
                    top: mt,
                    right: mr,
                    bottom: mb,
                    left: ml,
                }
            };
            frag.border_rect.origin = Point::new(item_x, item_y);
            let rel_cb_h = if definite_h.is_some() || final_content_h > 0.0 {
                Some(final_content_h)
            } else {
                None
            };
            let rel_space = cb_for_items.with_available(final_content_w, rel_cb_h);
            apply_relative_offset(&mut frag, rel_space);
            let (os, oe) = it.oof_range;
            if oe > os && oe <= ctx.pending_oof.len() {
                offset_pending_oof(
                    &mut ctx.pending_oof[..oe],
                    os,
                    frag.border_rect.origin.x,
                    frag.border_rect.origin.y,
                );
            }
            frag.compute_overflow_rect();

            let (f_bl, l_bl) = if is_vert_wm {
                let item_bs = if style.text.writing_mode == WritingMode::VerticalRl {
                    border_w - (frag.border_rect.origin.x + frag.border_rect.size.width)
                } else {
                    frag.border_rect.origin.x
                };
                let def_bl = frag.border_rect.size.width * 0.5;
                (
                    item_bs + frag.baseline.unwrap_or(def_bl),
                    item_bs + frag.last_baseline.or(frag.baseline).unwrap_or(def_bl),
                )
            } else {
                (
                    frag.border_rect.origin.y
                        + frag.baseline.unwrap_or(frag.border_rect.size.height),
                    frag.border_rect.origin.y
                        + frag
                            .last_baseline
                            .or(frag.baseline)
                            .unwrap_or(frag.border_rect.size.height),
                )
            };
            line_items_baselines.push((f_bl, l_bl));
            placed_children.push(frag);
        }

        // §9.8: Flex container first baseline (from visually-first line) & last baseline (from visually-last line).
        // Per CSSWG resolution, reversed main-axis directions (`row-reverse`, `column-reverse`) pick
        // the start-most item in writing-mode order (last in flex order) for first baseline.
        let rev_items = axes.main_reversed != axes.wm_main_reversed;
        let rev_lines = axes.cross_reversed != axes.wm_cross_reversed;
        let is_first_bl_line = if rev_lines {
            line_idx + 1 == lines.len()
        } else {
            line_idx == 0
        };
        let is_last_bl_line = if rev_lines {
            line_idx == 0
        } else {
            line_idx + 1 == lines.len()
        };
        let bs_pb = if is_vert_wm {
            if style.text.writing_mode == WritingMode::VerticalRl {
                metrics.border.right + metrics.padding.right
            } else {
                metrics.border.left + metrics.padding.left
            }
        } else {
            content_origin_y
        };
        let to_bs_shared = |logical_cross_bl: f32| -> f32 {
            if is_vert_wm {
                if axes.cross_reversed == axes.wm_cross_reversed {
                    bs_pb + logical_cross_bl
                } else {
                    border_w - (bs_pb + logical_cross_bl)
                }
            } else if axes.cross_reversed == axes.wm_cross_reversed {
                bs_pb + logical_cross_bl
            } else {
                border_h - (bs_pb + logical_cross_bl)
            }
        };
        if is_first_bl_line {
            container_first_baseline = line_first_bl_shared.map(to_bs_shared).or_else(|| {
                if rev_items {
                    line_items_baselines.last().map(|(f, _)| *f)
                } else {
                    line_items_baselines.first().map(|(f, _)| *f)
                }
            });
        }
        if is_last_bl_line {
            container_last_baseline = line_last_bl_shared.map(to_bs_shared).or_else(|| {
                if rev_items {
                    line_items_baselines.first().map(|(_, l)| *l)
                } else {
                    line_items_baselines.last().map(|(_, l)| *l)
                }
            });
        }
    }

    // §4.1: Push direct out-of-flow (`position: absolute | fixed`) children carrying `FlexAbsposHint`
    // so their static position resolves accurately against this flex container's content box & alignment!
    let content_rect_in_border = Rect::new(
        content_origin_x,
        content_origin_y,
        final_content_w,
        final_content_h,
    );
    for oof_id in oof_children {
        let oof_style = &ctx.tree.get(oof_id).style;
        let is_fixed = oof_style.position == PositionMode::Fixed;
        let oof_align_self = oof_style
            .flex_grid
            .as_ref()
            .map_or(AlignValue::Auto, |f| f.align_self);
        ctx.pending_oof.push(PendingOof {
            box_id: oof_id,
            static_pos: Point::new(content_origin_x, content_origin_y),
            is_fixed,
            static_x_is_end: false,
            static_y_is_end: false,
            flex_hint: Some(FlexAbsposHint {
                content_rect: content_rect_in_border,
                axes,
                justify_content,
                align_items,
                align_self: oof_align_self,
            }),
            grid_hint: None,
        });
    }

    resolve_oof_for_container(
        ctx,
        my_oof_start,
        &style,
        Rect::new(
            metrics.border.left,
            metrics.border.top,
            (border_w - metrics.border.horizontal()).max(0.0),
            (border_h - metrics.border.vertical()).max(0.0),
        ),
        false,
        space,
        &mut placed_children,
    );

    let is_scroll_box =
        style.overflow.x != OverflowMode::Visible || style.overflow.y != OverflowMode::Visible;
    let fallback_bl = if is_vert_wm { border_w * 0.5 } else { border_h };
    let clamp_bl = |v: f32| {
        if is_scroll_box {
            v.clamp(0.0, if is_vert_wm { border_w } else { border_h })
        } else {
            v
        }
    };
    let final_first_bl = Some(clamp_bl(container_first_baseline.unwrap_or(fallback_bl)));
    let final_last_bl = Some(clamp_bl(
        container_last_baseline
            .or(container_first_baseline)
            .unwrap_or(fallback_bl),
    ));

    let mut fragment = Fragment {
        box_id: container_id,
        dom_node: ctx.tree.get(container_id).dom_node,
        style,
        border_rect: Rect::new(0.0, 0.0, border_w, border_h),
        padding: metrics.padding,
        border: metrics.border,
        margin: resolved_margin,
        content_rect: content_rect_in_border,
        baseline: final_first_bl,
        last_baseline: final_last_bl,
        overflow_rect: Rect::new(0.0, 0.0, border_w, border_h),
        kind: FragmentKind::Box,
        children: Arc::new(placed_children),
        is_float: ctx.tree.get(container_id).is_float,
        is_out_of_flow: ctx.tree.get(container_id).is_out_of_flow,
        collapsed_margin_top: metrics.margin.top,
        collapsed_margin_bottom: metrics.margin.bottom,
        resolved_grid_tracks: None,
    };
    fragment.compute_overflow_rect();
    fragment
}

/// W3C CSS Flexbox Level 1 §9.7: Exact iterative freeze loop for resolving flexible lengths.
fn resolve_flexible_lengths(
    items: &mut [FlexItemState],
    line_indices: &[usize],
    container_main: f32,
    main_gap: f32,
) {
    let vis_count = line_indices
        .iter()
        .filter(|&&i| !items[i].collapsed)
        .count();
    let gaps_total = main_gap * vis_count.saturating_sub(1) as f32;
    let sum_hypo: f32 = line_indices
        .iter()
        .map(|&i| items[i].outer_hypo_main())
        .sum();
    let grow_mode = sum_hypo + gaps_total < container_main;

    // Step 2: Size inflexible items and freeze them.
    for &i in line_indices {
        let it = &mut items[i];
        if it.collapsed {
            it.target_main_size = 0.0;
            it.frozen = true;
            continue;
        }
        let factor = if grow_mode {
            it.flex_grow
        } else {
            it.flex_shrink
        };
        let inflexible = factor == 0.0
            || (grow_mode && it.flex_base_size > it.hypothetical_main_size)
            || (!grow_mode && it.flex_base_size < it.hypothetical_main_size);
        if inflexible {
            it.target_main_size = it.hypothetical_main_size;
            it.frozen = true;
        } else {
            it.target_main_size = it.flex_base_size;
            it.frozen = false;
        }
    }

    // Step 3: Calculate initial free space.
    let initial_free = container_main
        - gaps_total
        - line_indices
            .iter()
            .filter(|&&i| !items[i].collapsed)
            .map(|&i| {
                let it = &items[i];
                let inner = if it.frozen {
                    it.target_main_size
                } else {
                    it.flex_base_size
                };
                inner + it.pb_main + it.margin_main_start + it.margin_main_end
            })
            .sum::<f32>();

    // Step 4: Iterative freeze loop.
    for _ in 0..=line_indices.len() {
        let unfrozen_count = line_indices.iter().filter(|&&i| !items[i].frozen).count();
        if unfrozen_count == 0 {
            break;
        }
        let mut remaining_free = container_main
            - gaps_total
            - line_indices
                .iter()
                .filter(|&&i| !items[i].collapsed)
                .map(|&i| {
                    let it = &items[i];
                    let inner = if it.frozen {
                        it.target_main_size
                    } else {
                        it.flex_base_size
                    };
                    inner + it.pb_main + it.margin_main_start + it.margin_main_end
                })
                .sum::<f32>();

        let factor_sum: f32 = line_indices
            .iter()
            .filter(|&&i| !items[i].frozen)
            .map(|&i| {
                if grow_mode {
                    items[i].flex_grow
                } else {
                    items[i].flex_shrink
                }
            })
            .sum();

        if factor_sum < 1.0 {
            let scaled = initial_free * factor_sum;
            if scaled.abs() < remaining_free.abs() {
                remaining_free = scaled;
            }
        }

        if remaining_free != 0.0 {
            if grow_mode && factor_sum > 0.0 {
                for &i in line_indices {
                    if !items[i].frozen {
                        let ratio = items[i].flex_grow / factor_sum;
                        items[i].target_main_size =
                            items[i].flex_base_size + remaining_free * ratio;
                    }
                }
            } else if !grow_mode {
                let scaled_shrink_sum: f32 = line_indices
                    .iter()
                    .filter(|&&i| !items[i].frozen)
                    .map(|&i| items[i].flex_shrink * items[i].flex_base_size)
                    .sum();
                if scaled_shrink_sum > 0.0 {
                    for &i in line_indices {
                        if !items[i].frozen {
                            let scaled_f = items[i].flex_shrink * items[i].flex_base_size;
                            let ratio = scaled_f / scaled_shrink_sum;
                            items[i].target_main_size =
                                items[i].flex_base_size - remaining_free.abs() * ratio;
                        }
                    }
                }
            }
        }

        // Clamp min/max violations & accumulate total violation.
        let mut total_violation = 0.0f32;
        let mut violations: Vec<(usize, f32)> = Vec::new();
        for &i in line_indices {
            if items[i].frozen {
                continue;
            }
            let raw = items[i].target_main_size;
            let clamped = raw.min(items[i].max_main).max(items[i].min_main).max(0.0);
            let viol = clamped - raw;
            items[i].target_main_size = clamped;
            total_violation += viol;
            violations.push((i, viol));
        }

        if total_violation.abs() < 1e-4 {
            for &i in line_indices {
                items[i].frozen = true;
            }
            break;
        } else if total_violation > 0.0 {
            for (i, v) in violations {
                if v > 0.0 {
                    items[i].frozen = true;
                }
            }
        } else {
            for (i, v) in violations {
                if v < 0.0 {
                    items[i].frozen = true;
                }
            }
        }
    }
}
