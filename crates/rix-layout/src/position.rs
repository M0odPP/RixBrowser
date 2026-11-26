//! W3C CSS Positioned Layout Level 3 engine:
//! - True containing block identification (`position != static`, `transform`, `filter`, ICB)
//! - Out-of-flow (`position: absolute | fixed`) propagation with static position tracking
//! - Inset-Modified Containing Block (IMCB) with negative-IMCB clamping (CSSWG #11478)
//! - Auto-size stretch vs fit-content (`justify-self`/`align-self`, replaced, table),
//!   auto-margin resolution, writing-mode/direction axis mapping, and self-alignment
//! - Full `position: sticky` scroll-container and containing-block offset pass

use std::collections::HashMap;
use std::sync::Arc;

use rix_core::{Point, Rect, Size};
use rix_css::values_math::{RotateSpec, ScaleSpec, TranslateSpec};
use rix_css::{
    AlignValue, BoxSizingMode, ComputedStyle, DirectionMode, DisplayMode, LengthPercentageAuto,
    OverflowMode, PositionMode, SizeValue, WritingMode,
};
use rix_dom::NodeId;

use crate::box_tree::{BoxId, BoxKind};
use crate::fragment::{
    layout_box, measure_intrinsic_widths, resolve_size_with_intrinsic, BoxMetrics, ConstraintSpace,
    EdgeInsets, Fragment, FragmentKind, LayoutContext,
};

/// An out-of-flow (`position: absolute | fixed`) descendant propagating upward to its
/// true CSS containing block.
#[derive(Debug, Clone, Copy)]
pub struct PendingOof {
    pub box_id: BoxId,
    /// Hypothetical static position relative to the current ancestor's border-box origin.
    pub static_pos: Point,
    pub is_fixed: bool,
    /// True when `static_pos.x` anchors the box's physical right margin edge (horizontal RTL / vertical-rl).
    pub static_x_is_end: bool,
    /// True when `static_pos.y` anchors the box's physical bottom margin edge (vertical-lr/rl + RTL).
    pub static_y_is_end: bool,
    /// Optional CSS Flexbox L1 §4.1 static-position alignment hint when direct child of a flex container.
    pub flex_hint: Option<crate::flex::FlexAbsposHint>,
    /// Optional CSS Grid L1 §9 grid-area CB & static-position hint when direct child of a grid container.
    pub grid_hint: Option<crate::grid::GridAbsposHint>,
}

/// Containing block geometry passed to [`layout_out_of_flow_box`].
#[derive(Debug, Clone, Copy)]
pub struct ContainingBlockInfo {
    /// Containing block padding-box (or ICB) rectangle in the container fragment's border-box coords.
    pub rect: Rect,
    pub writing_mode: WritingMode,
    pub direction: DirectionMode,
}

/// Returns `true` if a box with `style` establishes a containing block for `position: fixed`
/// (and `position: absolute`) descendants per CSS Transforms L1 / Filter Effects L1 / Position L3.
#[must_use]
pub fn establishes_fixed_cb(style: &ComputedStyle, is_root: bool) -> bool {
    if is_root {
        return true;
    }
    style.effects.as_ref().is_some_and(|e| {
        !e.transform.is_empty()
            || e.transform_raw.is_some()
            || e.scale != ScaleSpec::None
            || e.rotate != RotateSpec::None
            || e.translate != TranslateSpec::None
            || !e.filter.is_empty()
            || (e.contain & (0x04 | 0x10) != 0)
    })
}

/// Returns `true` if a box with `style` establishes a containing block for `position: absolute`
/// descendants per CSS Positioned Layout L3 §2.1.
#[must_use]
pub fn establishes_abs_cb(style: &ComputedStyle, is_root: bool) -> bool {
    style.position != PositionMode::Static || establishes_fixed_cb(style, is_root)
}

/// Translates all pending out-of-flow static positions starting at `start_idx` by `(dx, dy)`
/// when a child fragment is positioned inside its parent container.
#[inline]
pub fn offset_pending_oof(pending: &mut [PendingOof], start_idx: usize, dx: f32, dy: f32) {
    for entry in &mut pending[start_idx..] {
        entry.static_pos.x += dx;
        entry.static_pos.y += dy;
        if let Some(ref mut fh) = entry.flex_hint {
            fh.content_rect.origin.x += dx;
            fh.content_rect.origin.y += dy;
        }
        if let Some(ref mut gh) = entry.grid_hint {
            gh.is_direct_child = false;
            gh.grid_area_cb.origin.x += dx;
            gh.grid_area_cb.origin.y += dy;
            gh.content_box_rect.origin.x += dx;
            gh.content_box_rect.origin.y += dy;
        }
    }
}

/// Drains and lays out all pending out-of-flow descendants from `ctx.pending_oof[start_idx..]`
/// whose containing block is this container, appending their fragments to `placed_children`.
pub fn resolve_oof_for_container(
    ctx: &mut LayoutContext<'_>,
    start_idx: usize,
    container_style: &ComputedStyle,
    cb_rect: Rect,
    is_root: bool,
    parent_space: ConstraintSpace,
    placed_children: &mut Vec<Fragment>,
) {
    let is_abs_cb = establishes_abs_cb(container_style, is_root);
    let is_fixed_cb = establishes_fixed_cb(container_style, is_root);
    if !is_abs_cb && !is_fixed_cb {
        return;
    }

    let mut mine = Vec::new();
    let mut keep = Vec::new();
    for entry in ctx.pending_oof.drain(start_idx..) {
        let capture = if entry.is_fixed {
            is_fixed_cb
        } else {
            is_abs_cb
        };
        if capture {
            mine.push(entry);
        } else {
            keep.push(entry);
        }
    }
    ctx.pending_oof.extend(keep);

    if mine.is_empty() {
        return;
    }
    let cb = ContainingBlockInfo {
        rect: cb_rect,
        writing_mode: container_style.text.writing_mode,
        direction: container_style.text.direction,
    };
    for entry in mine {
        let frag = layout_out_of_flow_box(ctx, entry, cb, parent_space);
        placed_children.push(frag);
    }
}

#[derive(Debug, Clone, Copy)]
struct ImcbAxis {
    start: f32,
    size: f32,
    both_specified: bool,
    /// Anchor side when only one inset or static pos is active.
    anchor: AxisAnchor,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum AxisAnchor {
    Both,
    Start(f32),
    End(f32),
    Static(f32),
    StaticEnd(f32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedAlign {
    Start,
    End,
    Center,
    Stretch,
}

/// Maps CSS `AlignValue` to physical start/end/center/stretch along a physical axis,
/// accounting for whether the logical start edge corresponds to the physical max edge (`flipped`).
fn resolve_axis_align(val: AlignValue, flipped: bool) -> (ResolvedAlign, bool) {
    match val.base() {
        AlignValue::Auto | AlignValue::Normal => (
            if flipped {
                ResolvedAlign::End
            } else {
                ResolvedAlign::Start
            },
            true,
        ),
        AlignValue::Stretch => (ResolvedAlign::Stretch, true),
        AlignValue::Start | AlignValue::FlexStart | AlignValue::SelfStart => (
            if flipped {
                ResolvedAlign::End
            } else {
                ResolvedAlign::Start
            },
            false,
        ),
        AlignValue::End | AlignValue::FlexEnd | AlignValue::SelfEnd => (
            if flipped {
                ResolvedAlign::Start
            } else {
                ResolvedAlign::End
            },
            false,
        ),
        AlignValue::Left => (ResolvedAlign::Start, false),
        AlignValue::Right => (ResolvedAlign::End, false),
        AlignValue::Center => (ResolvedAlign::Center, false),
        _ => (
            if flipped {
                ResolvedAlign::End
            } else {
                ResolvedAlign::Start
            },
            false,
        ),
    }
}

/// Computes the Inset-Modified Containing Block (IMCB) along one physical axis
/// (X: `lo = left`, `hi = right`; Y: `lo = top`, `hi = bottom`) per CSS Position L3 §3.1
/// and CSSWG #11478 (negative-sized IMCB clamping).
fn compute_imcb_axis(
    cb_size: f32,
    lo_inset: Option<f32>,
    hi_inset: Option<f32>,
    static_pos: f32,
    static_is_end: bool,
    align: ResolvedAlign,
) -> ImcbAxis {
    match (lo_inset, hi_inset) {
        (Some(lo), Some(hi)) => {
            let raw = cb_size - lo - hi;
            if raw >= 0.0 {
                ImcbAxis {
                    start: lo,
                    size: raw,
                    both_specified: true,
                    anchor: AxisAnchor::Both,
                }
            } else {
                // Negative-sized IMCB: collapse size to 0 at alignment edge (CSSWG #11478).
                let start = match align {
                    ResolvedAlign::End => cb_size - hi,
                    ResolvedAlign::Center => (lo + (cb_size - hi)) * 0.5,
                    ResolvedAlign::Start | ResolvedAlign::Stretch => lo,
                };
                ImcbAxis {
                    start,
                    size: 0.0,
                    both_specified: true,
                    anchor: AxisAnchor::Both,
                }
            }
        }
        (Some(lo), None) => ImcbAxis {
            start: lo,
            size: (cb_size - lo).max(0.0),
            both_specified: false,
            anchor: AxisAnchor::Start(lo),
        },
        (None, Some(hi)) => ImcbAxis {
            start: 0.0,
            size: (cb_size - hi).max(0.0),
            both_specified: false,
            anchor: AxisAnchor::End(cb_size - hi),
        },
        (None, None) => {
            if static_is_end {
                let avail = if static_pos >= 0.0 && static_pos <= cb_size {
                    static_pos.max(0.0)
                } else {
                    cb_size.max(0.0)
                };
                ImcbAxis {
                    start: 0.0,
                    size: avail,
                    both_specified: false,
                    anchor: AxisAnchor::StaticEnd(static_pos),
                }
            } else {
                let avail = if static_pos >= 0.0 && static_pos <= cb_size {
                    (cb_size - static_pos).max(0.0)
                } else {
                    cb_size.max(0.0)
                };
                ImcbAxis {
                    start: static_pos,
                    size: avail,
                    both_specified: false,
                    anchor: AxisAnchor::Static(static_pos),
                }
            }
        }
    }
}

/// Solves final margins and physical coordinate along one axis inside an IMCB.
#[allow(clippy::too_many_arguments)]
fn solve_axis_position(
    imcb: ImcbAxis,
    box_size: f32,
    m_lo_auto: bool,
    m_hi_auto: bool,
    m_lo: f32,
    m_hi: f32,
    align: ResolvedAlign,
    is_inline_axis: bool,
    flipped: bool,
) -> (f32, f32, f32) {
    match imcb.anchor {
        AxisAnchor::Both => {
            let fixed_lo = if m_lo_auto { 0.0 } else { m_lo };
            let fixed_hi = if m_hi_auto { 0.0 } else { m_hi };
            let free = imcb.size - box_size - fixed_lo - fixed_hi;
            let (rl, rh) = match (m_lo_auto, m_hi_auto) {
                (true, true) => {
                    if free >= 0.0 || !is_inline_axis {
                        // CSS 2.1 §10.6.4 / Position L3: block-axis auto margins split even negative free space;
                        // inline-axis auto margins clamp start margin to 0 on overflow.
                        (free * 0.5, free * 0.5)
                    } else if flipped {
                        (free, 0.0)
                    } else {
                        (0.0, free)
                    }
                }
                (true, false) => (free, fixed_hi),
                (false, true) => (fixed_lo, free),
                (false, false) => (fixed_lo, fixed_hi),
            };
            let pos = if m_lo_auto || m_hi_auto {
                imcb.start + rl
            } else {
                match align {
                    ResolvedAlign::Start | ResolvedAlign::Stretch => imcb.start + rl,
                    ResolvedAlign::End => imcb.start + imcb.size - rh - box_size,
                    ResolvedAlign::Center => imcb.start + rl + free * 0.5,
                }
            };
            (pos, rl, rh)
        }
        AxisAnchor::Start(lo) => {
            let rl = if m_lo_auto { 0.0 } else { m_lo };
            let rh = if m_hi_auto { 0.0 } else { m_hi };
            (lo + rl, rl, rh)
        }
        AxisAnchor::End(hi_edge) => {
            let rl = if m_lo_auto { 0.0 } else { m_lo };
            let rh = if m_hi_auto { 0.0 } else { m_hi };
            (hi_edge - rh - box_size, rl, rh)
        }
        AxisAnchor::Static(st) => {
            let rl = if m_lo_auto { 0.0 } else { m_lo };
            let rh = if m_hi_auto { 0.0 } else { m_hi };
            (st + rl, rl, rh)
        }
        AxisAnchor::StaticEnd(st) => {
            let rl = if m_lo_auto { 0.0 } else { m_lo };
            let rh = if m_hi_auto { 0.0 } else { m_hi };
            (st - rh - box_size, rl, rh)
        }
    }
}

/// Lays out an out-of-flow (`position: absolute | fixed`) box against its true CSS containing
/// block `cb` per CSS Positioned Layout Level 3 §3.1 & §5.1.
pub fn layout_out_of_flow_box(
    ctx: &mut LayoutContext<'_>,
    entry: PendingOof,
    mut cb: ContainingBlockInfo,
    parent_space: ConstraintSpace,
) -> Fragment {
    // CSS Grid L1 §9.1: When a grid container IS the containing block for an abspos grid child,
    // the item's containing block is the resolved grid area between its specified lines/padding edges.
    if let Some(gh) = entry.grid_hint {
        if gh.is_direct_child {
            cb.rect = gh.grid_area_cb;
        }
    }
    let oof_box = ctx.tree.get(entry.box_id);
    let style = Arc::clone(&oof_box.style);
    let is_replaced = matches!(oof_box.kind, BoxKind::Replaced(_));
    let is_table = matches!(style.display, DisplayMode::Table | DisplayMode::InlineTable);

    let cb_w = cb.rect.size.width.max(0.0);
    let cb_h = cb.rect.size.height.max(0.0);
    let mut cb_space = parent_space;
    cb_space.available_width = cb_w;
    cb_space.available_height = Some(cb_h);
    cb_space.containing_block_width = cb_w;
    cb_space.containing_block_height = cb_h;
    cb_space.cb_inline_size = if cb.writing_mode.is_vertical() {
        Some(cb_h)
    } else {
        None
    };

    let eval = ctx.eval_context_for(&style, &cb_space);
    let metrics = BoxMetrics::resolve_with_eval(&style, &cb_space, eval);
    let pb_w = metrics.padding.horizontal() + metrics.border.horizontal();
    let pb_h = metrics.padding.vertical() + metrics.border.vertical();
    let is_bb = style.box_sizing == BoxSizingMode::BorderBox;

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

    let is_vert = cb.writing_mode.is_vertical();
    let is_rtl = cb.direction == DirectionMode::Rtl;
    let x_is_inline = !is_vert;
    let y_is_inline = is_vert;
    let x_flipped = if !is_vert {
        is_rtl
    } else {
        matches!(
            cb.writing_mode,
            WritingMode::VerticalRl | WritingMode::SidewaysRl
        )
    };
    let y_flipped = is_vert && is_rtl;

    let fg = style.flex_grid.as_deref();
    let justify_self = fg.map_or(AlignValue::Auto, |f| f.justify_self);
    let align_self = fg.map_or(AlignValue::Auto, |f| f.align_self);
    let (x_align_raw, y_align_raw) = if !is_vert {
        (justify_self, align_self)
    } else {
        (align_self, justify_self)
    };
    let (x_align, x_auto_stretch_align) = resolve_axis_align(x_align_raw, x_flipped);
    let (y_align, y_auto_stretch_align) = resolve_axis_align(y_align_raw, y_flipped);

    // Static position relative to CB padding-box origin.
    let static_rel_x = entry.static_pos.x - cb.rect.origin.x;
    let static_rel_y = entry.static_pos.y - cb.rect.origin.y;

    let imcb_x = compute_imcb_axis(
        cb_w,
        left,
        right,
        static_rel_x,
        entry.static_x_is_end,
        x_align,
    );
    let imcb_y = compute_imcb_axis(
        cb_h,
        top,
        bottom,
        static_rel_y,
        entry.static_y_is_end,
        y_align,
    );

    let m_left_auto = matches!(style.box_model.margin.left, LengthPercentageAuto::Auto);
    let m_right_auto = matches!(style.box_model.margin.right, LengthPercentageAuto::Auto);
    let m_top_auto = matches!(style.box_model.margin.top, LengthPercentageAuto::Auto);
    let m_bottom_auto = matches!(style.box_model.margin.bottom, LengthPercentageAuto::Auto);

    let fixed_mx = (if m_left_auto {
        0.0
    } else {
        metrics.margin.left
    }) + (if m_right_auto {
        0.0
    } else {
        metrics.margin.right
    });
    let fixed_my = (if m_top_auto { 0.0 } else { metrics.margin.top })
        + (if m_bottom_auto {
            0.0
        } else {
            metrics.margin.bottom
        });

    // CSS Position L3 §5.1: Auto sizing stretches across IMCB when both insets are non-auto,
    // alignment is normal/auto/stretch (and box is non-replaced, non-table).
    // When self-alignment is non-stretch (e.g. `center`, `start`, `end`), auto size becomes fit-content!
    let width_is_auto = matches!(style.box_model.width, SizeValue::Auto);
    let height_is_auto = matches!(style.box_model.height, SizeValue::Auto);

    let stretch_w =
        imcb_x.both_specified && width_is_auto && !is_replaced && !is_table && x_auto_stretch_align;
    let stretch_h = imcb_y.both_specified
        && height_is_auto
        && !is_replaced
        && !is_table
        && y_auto_stretch_align;

    let imcb_avail_w = (imcb_x.size - fixed_mx).max(0.0);
    let imcb_avail_h = (imcb_y.size - fixed_my).max(0.0);

    // Measure intrinsic width if we need fit-content or intrinsic min/max width.
    let fit_content_w = width_is_auto && !stretch_w && !is_replaced;
    let (min_w_c, max_w_c) = if fit_content_w {
        let (min_b, max_b) = measure_intrinsic_widths(ctx, entry.box_id, cb_space);
        ((min_b - pb_w).max(0.0), (max_b - pb_w).max(0.0))
    } else {
        (0.0, 0.0)
    };
    let stf_c = max_w_c.min((imcb_avail_w - pb_w).max(0.0)).max(min_w_c);
    let intr_w_fn = |basis: &rix_css::values_math::CalcSizeBasis| -> f32 {
        match basis {
            rix_css::values_math::CalcSizeBasis::MinContent => min_w_c,
            rix_css::values_math::CalcSizeBasis::MaxContent => max_w_c,
            rix_css::values_math::CalcSizeBasis::Stretch => (imcb_avail_w - pb_w).max(0.0),
            _ => stf_c,
        }
    };
    let eff_min_w = resolve_size_with_intrinsic(
        &style.box_model.min_width,
        Some(cb_w),
        pb_w,
        fixed_mx,
        is_bb,
        &eval,
        intr_w_fn,
    )
    .unwrap_or(metrics.min_width);
    let eff_max_w = resolve_size_with_intrinsic(
        &style.box_model.max_width,
        Some(cb_w),
        pb_w,
        fixed_mx,
        is_bb,
        &eval,
        intr_w_fn,
    )
    .unwrap_or(metrics.max_width);

    let sub_avail_w = if stretch_w {
        ((imcb_avail_w - pb_w).max(0.0).min(eff_max_w).max(eff_min_w) + pb_w + fixed_mx).max(0.0)
    } else if matches!(style.box_model.width, SizeValue::Stretch) {
        (imcb_avail_w + fixed_mx).max(0.0)
    } else if fit_content_w {
        (stf_c.min(eff_max_w).max(eff_min_w) + pb_w + fixed_mx).max(0.0)
    } else {
        cb_w
    };
    let sub_avail_h = if stretch_h || matches!(style.box_model.height, SizeValue::Stretch) {
        Some((imcb_avail_h + fixed_my).max(0.0))
    } else {
        Some(cb_h)
    };

    let mut sub_space = cb_space;
    sub_space.available_width = sub_avail_w;
    sub_space.available_height = sub_avail_h;
    sub_space.containing_block_width = cb_w;
    sub_space.containing_block_height = cb_h;
    let mut def_w_ov = if stretch_w {
        Some((imcb_avail_w - pb_w).max(0.0).min(eff_max_w).max(eff_min_w))
    } else {
        None
    };
    let mut def_h_ov = if stretch_h {
        Some(
            (imcb_avail_h - pb_h)
                .max(0.0)
                .min(metrics.max_height)
                .max(metrics.min_height),
        )
    } else {
        None
    };

    // CSS Sizing L4 §6.1 / CSS Position L3 §5.1: apply `aspect-ratio` to non-replaced
    // out-of-flow boxes when one axis is definite and the opposite axis is `auto`.
    let mut ratio_derived_w: Option<f32> = None;
    let mut ratio_derived_h: Option<f32> = None;
    if !is_replaced && !is_table {
        if let Some((num, den)) = style
            .effects
            .as_ref()
            .and_then(|e| e.aspect_ratio_math.as_ref())
        {
            let n = num.eval_scalar(&eval, None);
            let d = den.as_ref().map_or(1.0, |x| x.eval_scalar(&eval, None));
            let ratio = n / d;
            if ratio.is_finite() && ratio > 0.0 {
                let ratio_f = ratio as f32;
                let known_w = def_w_ov.or_else(|| {
                    metrics
                        .specified_width
                        .map(|w| w.min(eff_max_w).max(eff_min_w).max(0.0))
                });
                let known_h = def_h_ov.or_else(|| {
                    metrics
                        .specified_height
                        .map(|h| h.min(metrics.max_height).max(metrics.min_height).max(0.0))
                });
                if height_is_auto && !stretch_h {
                    if let Some(cw) = known_w {
                        let box_w = if is_bb { cw + pb_w } else { cw };
                        let box_h = box_w / ratio_f;
                        let ch = if is_bb {
                            (box_h - pb_h).max(0.0)
                        } else {
                            box_h
                        }
                        .min(metrics.max_height)
                        .max(metrics.min_height)
                        .max(0.0);
                        def_h_ov = Some(ch);
                        ratio_derived_h = Some(ch);
                    }
                } else if width_is_auto && !stretch_w {
                    if let Some(ch) = known_h {
                        let box_h = if is_bb { ch + pb_h } else { ch };
                        let box_w = box_h * ratio_f;
                        let cw = if is_bb {
                            (box_w - pb_w).max(0.0)
                        } else {
                            box_w
                        }
                        .min(eff_max_w)
                        .max(eff_min_w)
                        .max(0.0);
                        def_w_ov = Some(cw);
                        ratio_derived_w = Some(cw);
                        sub_space.available_width = (cw + pb_w + fixed_mx).max(0.0);
                    }
                }
            }
        }
    }
    sub_space.definite_width_override = def_w_ov;
    sub_space.definite_height_override = def_h_ov;

    let mut frag = layout_box(ctx, entry.box_id, sub_space);

    // Enforce exact stretched / ratio-derived border-box dimensions (clamped by min/max).
    if stretch_w {
        let cw = (imcb_avail_w - pb_w).max(0.0).min(eff_max_w).max(eff_min_w);
        let bw = cw + pb_w;
        frag.border_rect.size.width = bw;
        frag.content_rect.size.width = cw;
    } else if let Some(cw) = ratio_derived_w {
        frag.border_rect.size.width = cw + pb_w;
        frag.content_rect.size.width = cw;
    } else if fit_content_w && !is_table {
        // Ensure fit-content respects IMCB available width (even when both insets are set with non-stretch align).
        let cw = stf_c.min(eff_max_w).max(eff_min_w);
        let bw = cw + pb_w;
        if (frag.border_rect.size.width - bw).abs() > 0.5 && frag.border_rect.size.width > bw {
            frag.border_rect.size.width = bw;
            frag.content_rect.size.width = cw;
        }
    }

    if stretch_h {
        let ch = (imcb_avail_h - pb_h)
            .max(0.0)
            .min(metrics.max_height)
            .max(metrics.min_height);
        let bh = ch + pb_h;
        frag.border_rect.size.height = bh;
        frag.content_rect.size.height = ch;
    } else if let Some(ch) = ratio_derived_h {
        // Respect aspect-ratio height as minimum height while still allowing explicit min/max clamping.
        if frag.content_rect.size.height < ch {
            frag.border_rect.size.height = ch + pb_h;
            frag.content_rect.size.height = ch;
        }
    }

    // CSS Flexbox L1 §4.1: If this out-of-flow box is a direct child of a flex container,
    // refine its static position using its laid-out outer margin-box size and the flex container's
    // content box + `justify-content` / `align-self` alignment!
    let mut final_imcb_x = imcb_x;
    let mut final_imcb_y = imcb_y;
    if let Some(fh) = entry.flex_hint {
        let ml_stat = if m_left_auto {
            0.0
        } else {
            metrics.margin.left
        };
        let mr_stat = if m_right_auto {
            0.0
        } else {
            metrics.margin.right
        };
        let mt_stat = if m_top_auto { 0.0 } else { metrics.margin.top };
        let mb_stat = if m_bottom_auto {
            0.0
        } else {
            metrics.margin.bottom
        };
        let outer_w = frag.border_rect.size.width + ml_stat + mr_stat;
        let outer_h = frag.border_rect.size.height + mt_stat + mb_stat;
        let flex_static = fh.compute_static_pos(
            outer_w,
            outer_h,
            style.text.writing_mode,
            style.text.direction,
        );
        if matches!(
            final_imcb_x.anchor,
            AxisAnchor::Static(_) | AxisAnchor::StaticEnd(_)
        ) {
            final_imcb_x.anchor = AxisAnchor::Static(flex_static.x - cb.rect.origin.x);
        }
        if matches!(
            final_imcb_y.anchor,
            AxisAnchor::Static(_) | AxisAnchor::StaticEnd(_)
        ) {
            final_imcb_y.anchor = AxisAnchor::Static(flex_static.y - cb.rect.origin.y);
        }
    } else if let Some(gh) = entry.grid_hint {
        let ml_stat = if m_left_auto {
            0.0
        } else {
            metrics.margin.left
        };
        let mr_stat = if m_right_auto {
            0.0
        } else {
            metrics.margin.right
        };
        let mt_stat = if m_top_auto { 0.0 } else { metrics.margin.top };
        let mb_stat = if m_bottom_auto {
            0.0
        } else {
            metrics.margin.bottom
        };
        let outer_w = frag.border_rect.size.width + ml_stat + mr_stat;
        let outer_h = frag.border_rect.size.height + mt_stat + mb_stat;
        let grid_static = gh.compute_static_pos(
            outer_w,
            outer_h,
            style.text.writing_mode,
            style.text.direction,
        );
        if matches!(
            final_imcb_x.anchor,
            AxisAnchor::Static(_) | AxisAnchor::StaticEnd(_)
        ) {
            final_imcb_x.anchor = AxisAnchor::Static(grid_static.x - cb.rect.origin.x);
        }
        if matches!(
            final_imcb_y.anchor,
            AxisAnchor::Static(_) | AxisAnchor::StaticEnd(_)
        ) {
            final_imcb_y.anchor = AxisAnchor::Static(grid_static.y - cb.rect.origin.y);
        }
    }

    let (x_rel, ml, mr) = solve_axis_position(
        final_imcb_x,
        frag.border_rect.size.width,
        m_left_auto,
        m_right_auto,
        metrics.margin.left,
        metrics.margin.right,
        x_align,
        x_is_inline,
        x_flipped,
    );
    let (y_rel, mt, mb) = solve_axis_position(
        final_imcb_y,
        frag.border_rect.size.height,
        m_top_auto,
        m_bottom_auto,
        metrics.margin.top,
        metrics.margin.bottom,
        y_align,
        y_is_inline,
        y_flipped,
    );

    frag.margin = EdgeInsets {
        top: mt,
        right: mr,
        bottom: mb,
        left: ml,
    };
    frag.border_rect.origin = Point::new(cb.rect.origin.x + x_rel, cb.rect.origin.y + y_rel);
    frag.compute_overflow_rect();
    frag
}

/// Returns `true` if `mode` establishes a scroll container axis per CSS Overflow L3
/// (`auto`, `scroll`, or `hidden` — notably NOT `visible` or `clip`).
#[inline]
#[must_use]
pub const fn is_scroll_axis(mode: OverflowMode) -> bool {
    matches!(
        mode,
        OverflowMode::Auto | OverflowMode::Scroll | OverflowMode::Hidden
    )
}

#[derive(Debug, Clone, Copy)]
struct StickyContext {
    /// Nearest X scroll container's visible scrollport `[min_x, max_x]` in document coordinates.
    sp_x: (f32, f32),
    /// Nearest Y scroll container's visible scrollport `[min_y, max_y]` in document coordinates.
    sp_y: (f32, f32),
    /// Containing block's content box in document coordinates (for clamping sticky offset).
    cb_content: Rect,
    /// Active containing block's writing direction for overconstrained left/right tie-breaking.
    direction: DirectionMode,
}

/// Computes the CSS Position L3 §3.3 sticky-adjusted position along a single physical axis,
/// expanding the end edge (or start edge when `prefer_hi`) if the sticky view rectangle is
/// smaller than `box_size`.
#[allow(clippy::too_many_arguments)]
fn solve_sticky_axis(
    abs_pos: f32,
    box_size: f32,
    sp_lo: f32,
    sp_hi: f32,
    lo_inset: Option<f32>,
    hi_inset: Option<f32>,
    cb_lo: f32,
    cb_hi: f32,
    margin_lo: f32,
    margin_hi: f32,
    prefer_hi: bool,
) -> f32 {
    let mut sv_lo = lo_inset.map(|i| sp_lo + i);
    let mut sv_hi = hi_inset.map(|i| sp_hi - i);
    if let (Some(lo), Some(hi)) = (sv_lo, sv_hi) {
        if hi - lo < box_size {
            if prefer_hi {
                sv_lo = Some(hi - box_size);
            } else {
                sv_hi = Some(lo + box_size);
            }
        }
    }
    let mut target = abs_pos;
    if prefer_hi {
        if let Some(lo) = sv_lo {
            let cb_max = (cb_hi - box_size - margin_hi).max(abs_pos);
            if target < lo {
                target = lo.min(cb_max);
            }
        }
        if let Some(hi) = sv_hi {
            let stick_max = hi - box_size;
            let cb_min = (cb_lo + margin_lo).min(abs_pos);
            if target > stick_max {
                target = stick_max.max(cb_min);
            }
        }
    } else {
        if let Some(hi) = sv_hi {
            let stick_max = hi - box_size;
            let cb_min = (cb_lo + margin_lo).min(abs_pos);
            if target > stick_max {
                target = stick_max.max(cb_min);
            }
        }
        if let Some(lo) = sv_lo {
            let cb_max = (cb_hi - box_size - margin_hi).max(abs_pos);
            if target < lo {
                target = lo.min(cb_max);
            }
        }
    }
    target
}

/// Post-layout pass applying W3C CSS Positioned Layout L3 §3.3 `position: sticky` offsets
/// across the fragment tree given per-element `scroll_offsets` and viewport `root_scroll`.
pub fn apply_sticky_offsets(
    root: &mut Fragment,
    scroll_offsets: &HashMap<NodeId, Point>,
    root_scroll: Point,
    viewport: Size,
) {
    let root_cb = Rect::new(
        root.border_rect.origin.x + root.content_rect.origin.x,
        root.border_rect.origin.y + root.content_rect.origin.y,
        root.content_rect.size.width.max(viewport.width),
        root.content_rect.size.height.max(viewport.height),
    );
    let ctx = StickyContext {
        sp_x: (root_scroll.x, root_scroll.x + viewport.width),
        sp_y: (root_scroll.y, root_scroll.y + viewport.height),
        cb_content: root_cb,
        direction: root.style.text.direction,
    };
    apply_sticky_walk(root, Point::ZERO, ctx, scroll_offsets, true);
}

fn apply_sticky_walk(
    frag: &mut Fragment,
    parent_origin: Point,
    ctx: StickyContext,
    scroll_offsets: &HashMap<NodeId, Point>,
    is_root: bool,
) {
    let mut abs_x = parent_origin.x + frag.border_rect.origin.x;
    let mut abs_y = parent_origin.y + frag.border_rect.origin.y;

    if frag.style.position == PositionMode::Sticky {
        let eval = frag.style.text.eval_ctx;
        let inset = &frag.style.box_model.inset;
        let sp_w = (ctx.sp_x.1 - ctx.sp_x.0).max(0.0);
        let sp_h = (ctx.sp_y.1 - ctx.sp_y.0).max(0.0);

        let top = match &inset.top {
            LengthPercentageAuto::Auto => None,
            LengthPercentageAuto::Value(lp) => Some(lp.resolve_px(sp_h, &eval)),
        };
        let bottom = match &inset.bottom {
            LengthPercentageAuto::Auto => None,
            LengthPercentageAuto::Value(lp) => Some(lp.resolve_px(sp_h, &eval)),
        };
        let left = match &inset.left {
            LengthPercentageAuto::Auto => None,
            LengthPercentageAuto::Value(lp) => Some(lp.resolve_px(sp_w, &eval)),
        };
        let right = match &inset.right {
            LengthPercentageAuto::Auto => None,
            LengthPercentageAuto::Value(lp) => Some(lp.resolve_px(sp_w, &eval)),
        };

        let fw = frag.border_rect.size.width;
        let fh = frag.border_rect.size.height;
        let is_rtl = ctx.direction == DirectionMode::Rtl;

        let target_y = solve_sticky_axis(
            abs_y,
            fh,
            ctx.sp_y.0,
            ctx.sp_y.1,
            top,
            bottom,
            ctx.cb_content.min_y(),
            ctx.cb_content.max_y(),
            frag.margin.top,
            frag.margin.bottom,
            false,
        );
        let target_x = solve_sticky_axis(
            abs_x,
            fw,
            ctx.sp_x.0,
            ctx.sp_x.1,
            left,
            right,
            ctx.cb_content.min_x(),
            ctx.cb_content.max_x(),
            frag.margin.left,
            frag.margin.right,
            is_rtl,
        );

        let dx = target_x - abs_x;
        let dy = target_y - abs_y;
        if dx != 0.0 || dy != 0.0 {
            frag.border_rect.origin.x += dx;
            frag.border_rect.origin.y += dy;
            abs_x = target_x;
            abs_y = target_y;
        }
    }

    if frag.children.is_empty() {
        return;
    }

    // Build child StickyContext: block containers become the sticky containing block,
    // and each scroll-container axis (`overflow-x`/`overflow-y`: auto | scroll | hidden)
    // independently updates its corresponding sticky scrollport (skipping `visible` & `clip`).
    let mut child_ctx = ctx;
    let is_block_container = matches!(frag.kind, FragmentKind::Box);
    if is_block_container {
        child_ctx.cb_content = Rect::new(
            abs_x + frag.content_rect.origin.x,
            abs_y + frag.content_rect.origin.y,
            frag.content_rect.size.width,
            frag.content_rect.size.height,
        );
        child_ctx.direction = frag.style.text.direction;
    }

    if !is_root {
        let scroll_x = is_scroll_axis(frag.style.overflow.x);
        let scroll_y = is_scroll_axis(frag.style.overflow.y);
        if scroll_x || scroll_y {
            let scroll = frag
                .dom_node
                .and_then(|nid| scroll_offsets.get(&nid).copied())
                .unwrap_or(Point::ZERO);
            if scroll_x {
                let cx = abs_x + frag.content_rect.origin.x;
                let cw = frag.content_rect.size.width.max(0.0);
                child_ctx.sp_x = (cx + scroll.x, cx + scroll.x + cw);
                let scroll_w = (frag.overflow_rect.max_x() - frag.content_rect.origin.x).max(cw);
                child_ctx.cb_content.origin.x = cx;
                child_ctx.cb_content.size.width = scroll_w;
            }
            if scroll_y {
                let cy = abs_y + frag.content_rect.origin.y;
                let ch = frag.content_rect.size.height.max(0.0);
                child_ctx.sp_y = (cy + scroll.y, cy + scroll.y + ch);
                let scroll_h = (frag.overflow_rect.max_y() - frag.content_rect.origin.y).max(ch);
                child_ctx.cb_content.origin.y = cy;
                child_ctx.cb_content.size.height = scroll_h;
            }
        }
    }

    let my_origin = Point::new(abs_x, abs_y);
    let children = Arc::make_mut(&mut frag.children);
    for child in children.iter_mut() {
        apply_sticky_walk(child, my_origin, child_ctx, scroll_offsets, false);
    }
}
