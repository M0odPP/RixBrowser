//! Native pure-Rust W3C CSS Grid Layout Module Level 1 container & item layout engine.
//!
//! Pairs with [`crate::grid_tracks`] (explicit track expansion, line resolution, auto-placement,
//! §11 track sizing, and alignment math) to execute item intrinsic sizing (§6.6 & §11.5),
//! baseline-alignment shims (§10.5), two-pass column/row layout, out-of-flow §9 grid-area
//! containing blocks, and §7.2.6 resolved track listing for `getComputedStyle()`.

use std::collections::HashMap;
use std::sync::Arc;

use rix_core::{Point, Rect};
use rix_css::grid_css::GridTemplateTrackList;
use rix_css::{
    AlignValue, BoxSizingMode, ComputedStyle, DisplayMode, EvalContext, GridAutoFlowMode,
    GridPlacementValue, GridTemplateAreasValue, LengthPercentage, LengthPercentageAuto,
    PositionMode, SizeValue,
};

use crate::bfc::apply_relative_offset;
use crate::box_tree::{BoxId, BoxKind, ReplacedKind};
use crate::fragment::{
    layout_box, measure_intrinsic_widths, resolve_size_with_intrinsic, BoxMetrics, ConstraintSpace,
    EdgeInsets, Fragment, FragmentKind, LayoutContext, ResolvedGridTracks,
};
use crate::grid_tracks::{
    active_gaps_in_range, build_axis_tracks, compute_axis_baseline_metrics,
    compute_track_positions, expand_explicit_axis, format_resolved_tracks,
    mark_empty_auto_fit_tracks, place_grid_items, resolve_abspos_axis_edges, resolve_self_align,
    resolve_tracks, solve_grid_item_axis, span_track_size, track_has_percentage, GridItem,
    MaxSizing, ResolvedSelfAlign, SizingConstraint, Track,
};
pub use crate::grid_tracks::{GridAbsposHint, GridAxes};
use crate::position::{offset_pending_oof, resolve_oof_for_container, PendingOof};

// ---------------------------------------------------------------------------
// Item Intrinsic Contribution Measurement (CSS Grid L1 §6.6 & §11.5)
// ---------------------------------------------------------------------------

fn parse_aspect_ratio_val(style: &ComputedStyle, eval: &EvalContext) -> Option<f32> {
    if let Some((a, b_opt)) = style
        .effects
        .as_ref()
        .and_then(|e| e.aspect_ratio_math.as_ref())
    {
        let va = a.eval_scalar(eval, None);
        let vb = b_opt.as_ref().map_or(1.0, |b| b.eval_scalar(eval, None));
        if va.is_finite() && vb.is_finite() && va > 0.0 && vb > 0.0 {
            return Some((va / vb) as f32);
        }
    }
    None
}

fn replaced_intrinsic_dims(b: &crate::box_tree::LayoutBox) -> Option<(f32, f32)> {
    match &b.kind {
        BoxKind::Replaced(
            ReplacedKind::Image { intrinsic_size, .. }
            | ReplacedKind::Svg { intrinsic_size, .. }
            | ReplacedKind::ExternalSurface { intrinsic_size, .. },
        ) => Some((intrinsic_size.width, intrinsic_size.height)),
        _ => None,
    }
}

fn effective_aspect_ratio(b: &crate::box_tree::LayoutBox, eval: &EvalContext) -> Option<f32> {
    if let Some(ar) = parse_aspect_ratio_val(&b.style, eval) {
        return Some(ar);
    }
    if let Some((iw, ih)) = replaced_intrinsic_dims(b) {
        if iw > 0.0 && ih > 0.0 {
            return Some(iw / ih);
        }
    }
    None
}

fn measure_item_block_height(
    ctx: &mut LayoutContext<'_>,
    box_id: BoxId,
    avail_w: f32,
    avail_h: Option<f32>,
    space: ConstraintSpace,
    ignore_specified_h: bool,
) -> f32 {
    let mut probe = space.with_available(avail_w.max(0.0), avail_h);
    probe.ignore_specified_height = ignore_specified_h;
    let saved = ctx.pending_oof.len();
    let frag = layout_box(ctx, box_id, probe);
    ctx.pending_oof.truncate(saved);
    frag.border_rect.size.height
}

/// Computes `(min_contrib, min_content_contrib, max_content_contrib)` for `item` along `is_col`.
#[allow(clippy::too_many_arguments)]
fn compute_item_contributions(
    ctx: &mut LayoutContext<'_>,
    item: &GridItem,
    is_col: bool,
    axes: GridAxes,
    tracks: &[Track],
    other_tracks: Option<&[Track]>,
    other_gap: f32,
    container_space: ConstraintSpace,
) -> (f32, f32, f32) {
    let b_ref = ctx.tree.get(item.box_id);
    let style = Arc::clone(&b_ref.style);
    let is_replaced = matches!(b_ref.kind, BoxKind::Replaced(_));
    let eval = container_space.eval_context_for(&style);
    let ar = effective_aspect_ratio(b_ref, &eval);

    let (start, end) = if is_col {
        (item.col_start, item.col_end)
    } else {
        (item.row_start, item.row_end)
    };
    let spans_flex = tracks[start..end].iter().any(|t| t.max_fn.is_flexible());
    let fixed_max_sum: Option<f32> = {
        let mut s = 0.0f32;
        let mut all_fixed = true;
        for t in &tracks[start..end] {
            match t.max_fn {
                MaxSizing::Fixed(v) => s += v,
                _ => {
                    all_fixed = false;
                    break;
                }
            }
        }
        all_fixed.then_some(s)
    };

    // Known cross-axis span size (available during row pass after columns are resolved).
    let known_other_span = other_tracks.map(|ot| {
        let (os, oe) = if is_col {
            (item.row_start, item.row_end)
        } else {
            (item.col_start, item.col_end)
        };
        span_track_size(ot, os, oe, other_gap)
    });

    let axis_is_x = if is_col {
        axes.col_is_x
    } else {
        !axes.col_is_x
    };

    // Resolve item's padding, border, and non-auto margins (percentages resolve against
    // the known inline-axis span when resolving block-axis contributions, else 0 during intrinsic sizing).
    let mut metrics_space = container_space.with_available(
        if !axis_is_x {
            known_other_span.unwrap_or(container_space.available_width)
        } else {
            0.0
        },
        if axis_is_x { known_other_span } else { None },
    );
    if !axes.col_is_x {
        metrics_space.cb_inline_size = Some(if is_col {
            0.0
        } else {
            known_other_span.unwrap_or(0.0)
        });
    }
    let metrics = BoxMetrics::resolve_with_eval(&style, &metrics_space, eval);
    let is_bb = style.box_sizing == BoxSizingMode::BorderBox;

    let (pb_axis, margin_axis, pb_other, margin_other) = if axis_is_x {
        (
            metrics.padding.horizontal() + metrics.border.horizontal(),
            (if metrics.margin_left_auto {
                0.0
            } else {
                metrics.margin.left
            }) + (if metrics.margin_right_auto {
                0.0
            } else {
                metrics.margin.right
            }),
            metrics.padding.vertical() + metrics.border.vertical(),
            (if matches!(style.box_model.margin.top, LengthPercentageAuto::Auto) {
                0.0
            } else {
                metrics.margin.top
            }) + (if matches!(style.box_model.margin.bottom, LengthPercentageAuto::Auto) {
                0.0
            } else {
                metrics.margin.bottom
            }),
        )
    } else {
        (
            metrics.padding.vertical() + metrics.border.vertical(),
            (if matches!(style.box_model.margin.top, LengthPercentageAuto::Auto) {
                0.0
            } else {
                metrics.margin.top
            }) + (if matches!(style.box_model.margin.bottom, LengthPercentageAuto::Auto) {
                0.0
            } else {
                metrics.margin.bottom
            }),
            metrics.padding.horizontal() + metrics.border.horizontal(),
            (if metrics.margin_left_auto {
                0.0
            } else {
                metrics.margin.left
            }) + (if metrics.margin_right_auto {
                0.0
            } else {
                metrics.margin.right
            }),
        )
    };

    let (sz_val, min_sz_val, max_sz_val, other_sz_val, overflow_axis) = if axis_is_x {
        (
            &style.box_model.width,
            &style.box_model.min_width,
            &style.box_model.max_width,
            &style.box_model.height,
            style.overflow.x,
        )
    } else {
        (
            &style.box_model.height,
            &style.box_model.min_height,
            &style.box_model.max_height,
            &style.box_model.width,
            style.overflow.y,
        )
    };

    // Measure intrinsic border-box sizes along `axis_is_x`.
    let (intr_min_b, intr_max_b) = if axis_is_x {
        let (mn, mx) = measure_intrinsic_widths(ctx, item.box_id, metrics_space);
        (mn.max(pb_axis), mx.max(pb_axis))
    } else if style.text.writing_mode.is_vertical()
        && !is_replaced
        && !ctx
            .tree
            .get(item.box_id)
            .children
            .iter()
            .any(|&cid| ctx.tree.get(cid).is_block_level() || ctx.tree.get(cid).is_float)
        && !ctx.tree.get(item.box_id).children.is_empty()
    {
        // Vertical writing-mode inline content: physical Y is the item's inline axis!
        let (mn_c, mx_c) = crate::ifc::measure_ifc_inline_min_max(ctx, item.box_id, metrics_space);
        ((mn_c + pb_axis).max(pb_axis), (mx_c + pb_axis).max(pb_axis))
    } else {
        let probe_w = known_other_span
            .map(|s| (s - margin_other).max(0.0))
            .unwrap_or(container_space.available_width);
        let h = measure_item_block_height(ctx, item.box_id, probe_w, None, container_space, false);
        (h.max(pb_axis), h.max(pb_axis))
    };

    let resolve_size_border = |sv: &SizeValue| -> Option<f32> {
        match sv {
            SizeValue::Length(lp) if !lp.has_percentage() => {
                let raw = lp.resolve_px(0.0, &eval).max(0.0);
                Some(if is_bb {
                    raw.max(pb_axis)
                } else {
                    raw + pb_axis
                })
            }
            SizeValue::MinContent => Some(intr_min_b),
            SizeValue::MaxContent => Some(intr_max_b),
            SizeValue::FitContent(None) => Some(intr_max_b),
            SizeValue::FitContent(Some(lp)) if !lp.has_percentage() => {
                let raw = lp.resolve_px(0.0, &eval).max(0.0);
                let cap = if is_bb {
                    raw.max(pb_axis)
                } else {
                    raw + pb_axis
                };
                Some(intr_max_b.min(cap).max(intr_min_b))
            }
            _ => None,
        }
    };

    let def_border = match sz_val {
        SizeValue::Length(lp) if !lp.has_percentage() => {
            let raw = lp.resolve_px(0.0, &eval).max(0.0);
            Some(if is_bb {
                raw.max(pb_axis)
            } else {
                raw + pb_axis
            })
        }
        _ => None,
    };
    let min_border_explicit = resolve_size_border(min_sz_val);
    let max_border_explicit = resolve_size_border(max_sz_val);
    let min_is_auto = matches!(min_sz_val, SizeValue::Auto);

    // Transferred size through `aspect-ratio` if the opposite axis has a definite size.
    let transferred_border = ar.and_then(|ratio| {
        let other_content = match other_sz_val {
            SizeValue::Length(lp) if !lp.has_percentage() => {
                let raw = lp.resolve_px(0.0, &eval).max(0.0);
                Some(if is_bb {
                    (raw - pb_other).max(0.0)
                } else {
                    raw
                })
            }
            SizeValue::Length(lp) if known_other_span.is_some() => {
                let raw = lp.resolve_px(known_other_span.unwrap(), &eval).max(0.0);
                Some(if is_bb {
                    (raw - pb_other).max(0.0)
                } else {
                    raw
                })
            }
            SizeValue::Auto if known_other_span.is_some() && !is_replaced => None,
            _ => None,
        }?;
        let c = if axis_is_x {
            other_content * ratio
        } else {
            other_content / ratio
        };
        Some(c + pb_axis)
    });

    // Clamp helper using explicit min/max (ignoring unresolvable percentages).
    let clamp_border = |v: f32| -> f32 {
        let mut r = v;
        if let Some(mx) = max_border_explicit {
            r = r.min(mx);
        }
        if let Some(mn) = min_border_explicit {
            r = r.max(mn);
        }
        r.max(pb_axis)
    };

    let (min_c_b, max_c_b) = match sz_val {
        SizeValue::MinContent => (clamp_border(intr_min_b), clamp_border(intr_min_b)),
        SizeValue::MaxContent => (clamp_border(intr_max_b), clamp_border(intr_max_b)),
        SizeValue::FitContent(Some(lp)) if !lp.has_percentage() => {
            let raw = lp.resolve_px(0.0, &eval).max(0.0);
            let cap = if is_bb {
                raw.max(pb_axis)
            } else {
                raw + pb_axis
            };
            (
                clamp_border(intr_min_b),
                clamp_border(intr_max_b.min(cap).max(intr_min_b)),
            )
        }
        _ => {
            if let Some(db) = def_border {
                let c = clamp_border(db);
                (c, c)
            } else if let Some(tb) = transferred_border {
                let c = clamp_border(tb);
                (c, c)
            } else {
                (clamp_border(intr_min_b), clamp_border(intr_max_b))
            }
        }
    };

    let min_content_contrib = (min_c_b + margin_axis).max(0.0);
    let max_content_contrib = (max_c_b + margin_axis).max(0.0);

    // W3C CSS Grid L1 §11.5 & §6.6 Minimum Contribution:
    // If the item's computed preferred size neither behaves as `auto` nor depends on its
    // containing block in this axis, its minimum contribution IS its min-content contribution
    // (never clamped by spanned tracks' fixed max sizing functions).
    let pref_behaves_as_auto_or_cb = match sz_val {
        SizeValue::Auto
        | SizeValue::None
        | SizeValue::Content
        | SizeValue::Stretch
        | SizeValue::CalcSize(_) => transferred_border.is_none(),
        SizeValue::Length(lp) => lp.has_percentage(),
        SizeValue::FitContent(Some(lp)) => lp.has_percentage(),
        SizeValue::MinContent | SizeValue::MaxContent | SizeValue::FitContent(None) => false,
    };

    let min_contrib = if !pref_behaves_as_auto_or_cb {
        min_content_contrib
    } else if !min_is_auto {
        // Explicit `min-width`/`min-height` (non-auto): percentage resolves to 0 (+ pb) during
        // intrinsic sizing; non-percentage/intrinsic resolves directly and is never track-clamped.
        let b = min_border_explicit.unwrap_or(pb_axis).max(pb_axis);
        (b + margin_axis).max(0.0)
    } else if crate::position::is_scroll_axis(overflow_axis)
        || (spans_flex && end.saturating_sub(start) > 1)
    {
        // Scrollable overflow (`hidden`/`scroll`/`auto`, NOT `clip` or `visible`), or
        // CSS Grid L1 §6.6 / §11.5 step 3: automatic minimum size is 0 when spanning >1 track
        // and crossing a flexible track.
        (pb_axis + margin_axis).max(0.0)
    } else {
        // §6.6 Content-based Automatic Minimum Size (clamped by max-size and fixed track max sum).
        let mut m = intr_min_b;
        if let Some(tb) = transferred_border {
            m = m.min(tb);
        }
        if let Some(mx) = max_border_explicit {
            m = m.min(mx);
        }
        if !spans_flex {
            if let Some(fmax) = fixed_max_sum {
                let cap = (fmax - margin_axis).max(pb_axis);
                m = m.min(cap);
            }
        }
        (m.max(pb_axis) + margin_axis).max(0.0)
    };
    (min_contrib, min_content_contrib, max_content_contrib)
}

/// Adds CSS Box Alignment L3 §9 / CSS Grid L1 §10.5 baseline-alignment shims to intrinsic item
/// contributions before `resolve_tracks` runs along `is_col`.
#[allow(clippy::too_many_arguments)]
fn apply_baseline_shims_to_items(
    ctx: &mut LayoutContext<'_>,
    items: &mut [GridItem],
    is_col: bool,
    axes: GridAxes,
    container_align_items: AlignValue,
    other_tracks: Option<&[Track]>,
    other_gap: f32,
    space: ConstraintSpace,
) {
    let axis_is_x = if is_col {
        axes.col_is_x
    } else {
        !axes.col_is_x
    };
    let axis_rev = if is_col {
        axes.col_reversed
    } else {
        axes.row_reversed
    };

    // Identify single-span items participating in baseline alignment along `is_col`.
    let mut participants: Vec<(usize, usize, bool, bool, f32, f32)> = Vec::new();
    for (idx, it) in items.iter().enumerate() {
        let (s, e) = if is_col {
            (it.col_start, it.col_end)
        } else {
            (it.row_start, it.row_end)
        };
        if e != s + 1 {
            continue;
        }
        let item_box = ctx.tree.get(it.box_id);
        let style = Arc::clone(&item_box.style);
        let is_replaced = matches!(item_box.kind, BoxKind::Replaced(_));
        let ifg = style.flex_grid.as_deref();
        let raw = if is_col {
            ifg.map_or(AlignValue::Auto, |f| f.justify_self)
        } else {
            ifg.map_or(AlignValue::Auto, |f| f.align_self)
        };
        let eff = if raw.base() == AlignValue::Auto {
            container_align_items
        } else {
            raw
        };
        let item_rev =
            axes.item_axis_reversed(is_col, style.text.writing_mode, style.text.direction);
        let (resolved, _) = resolve_self_align(eff, axis_is_x, axis_rev, item_rev, is_replaced);
        let is_last = match resolved {
            ResolvedSelfAlign::Baseline => false,
            ResolvedSelfAlign::LastBaseline => true,
            _ => continue,
        };
        let (ms_auto, me_auto) = if is_col {
            axes.col_margins_auto(&style)
        } else {
            axes.row_margins_auto(&style)
        };
        if ms_auto || me_auto {
            continue;
        }

        // Probe intrinsic size & baseline of item.
        let known_other = other_tracks.map(|ot| {
            let (os, oe) = if is_col {
                (it.row_start, it.row_end)
            } else {
                (it.col_start, it.col_end)
            };
            span_track_size(ot, os, oe, other_gap)
        });
        let probe_w = if !axis_is_x {
            known_other.unwrap_or(space.available_width)
        } else {
            measure_intrinsic_widths(ctx, it.box_id, space).1
        };
        let probe_space = space.with_available(probe_w.max(0.0), None);
        let eval = probe_space.eval_context_for(&style);
        let im = BoxMetrics::resolve_with_eval(&style, &probe_space, eval);
        let saved = ctx.pending_oof.len();
        let frag = layout_box(ctx, it.box_id, probe_space);
        ctx.pending_oof.truncate(saved);
        let (ms_raw, me_raw) = if is_col {
            axes.col_margins(im.margin)
        } else {
            axes.row_margins(im.margin)
        };
        let (group_side, ascent, descent) = compute_axis_baseline_metrics(
            &style,
            frag.border_rect.size.width,
            frag.border_rect.size.height,
            frag.baseline,
            frag.last_baseline,
            &im,
            ms_raw,
            me_raw,
            axis_is_x,
            axis_rev,
            is_last,
        );
        participants.push((idx, s, is_last, group_side, ascent, descent));
    }

    if participants.len() < 2 {
        return;
    }

    let mut max_ascent: HashMap<(usize, bool, bool), f32> = HashMap::new();
    let mut max_descent: HashMap<(usize, bool, bool), f32> = HashMap::new();
    for &(_, track_idx, is_last, side, asc, desc) in &participants {
        let key = (track_idx, is_last, side);
        max_ascent
            .entry(key)
            .and_modify(|v| *v = v.max(asc))
            .or_insert(asc);
        max_descent
            .entry(key)
            .and_modify(|v| *v = v.max(desc))
            .or_insert(desc);
    }

    for &(idx, track_idx, is_last, side, asc, desc) in &participants {
        let key = (track_idx, is_last, side);
        let shim = if !is_last {
            (max_ascent[&key] - asc).max(0.0)
        } else {
            (max_descent[&key] - desc).max(0.0)
        };
        if shim > 0.0 {
            items[idx].min_contrib += shim;
            items[idx].min_content_contrib += shim;
            items[idx].max_content_contrib += shim;
        }
    }
}

// ---------------------------------------------------------------------------
// Intrinsic Width Measurement for Grid Containers
// ---------------------------------------------------------------------------

/// Computes `(min_content_width, max_content_width)` border-box sizes for a CSS Grid container.
pub fn measure_grid_intrinsic_widths(
    ctx: &mut LayoutContext<'_>,
    container_id: BoxId,
    space: ConstraintSpace,
) -> (f32, f32) {
    let b = ctx.tree.get(container_id);
    let style = Arc::clone(&b.style);
    let eval = space.eval_context_for(&style);
    let metrics = BoxMetrics::resolve_with_eval(&style, &space, eval);
    let pb_w = metrics.padding.horizontal() + metrics.border.horizontal();

    let (content_min, content_max) =
        measure_grid_content_intrinsic_widths(ctx, container_id, space);
    let is_bb = style.box_sizing == BoxSizingMode::BorderBox;
    let avail_c = (space.available_width - metrics.margin.horizontal() - pb_w).max(0.0);
    let stf = content_max.min(avail_c).max(content_min);
    let intr_fn = |basis: &rix_css::values_math::CalcSizeBasis| -> f32 {
        match basis {
            rix_css::values_math::CalcSizeBasis::MinContent => content_min,
            rix_css::values_math::CalcSizeBasis::MaxContent => content_max,
            rix_css::values_math::CalcSizeBasis::Stretch => avail_c,
            _ => stf,
        }
    };
    let min_w = resolve_size_with_intrinsic(
        &style.box_model.min_width,
        Some(space.containing_block_width),
        pb_w,
        metrics.margin.horizontal(),
        is_bb,
        &eval,
        intr_fn,
    )
    .unwrap_or(metrics.min_width);
    let max_w = resolve_size_with_intrinsic(
        &style.box_model.max_width,
        Some(space.containing_block_width),
        pb_w,
        metrics.margin.horizontal(),
        is_bb,
        &eval,
        intr_fn,
    )
    .unwrap_or(metrics.max_width);

    if let Some(sw) = metrics.specified_width.or_else(|| {
        resolve_size_with_intrinsic(
            &style.box_model.width,
            Some(space.containing_block_width),
            pb_w,
            metrics.margin.horizontal(),
            is_bb,
            &eval,
            intr_fn,
        )
    }) {
        let clamped = sw.min(max_w).max(min_w).max(0.0) + pb_w;
        return (clamped, clamped);
    }

    (
        content_min.min(max_w).max(min_w).max(0.0) + pb_w,
        content_max.min(max_w).max(min_w).max(0.0) + pb_w,
    )
}

fn measure_grid_content_intrinsic_widths(
    ctx: &mut LayoutContext<'_>,
    container_id: BoxId,
    space: ConstraintSpace,
) -> (f32, f32) {
    let b = ctx.tree.get(container_id);
    let style = Arc::clone(&b.style);
    let eval = space.eval_context_for(&style);
    let axes = GridAxes::compute(style.text.writing_mode, style.text.direction);
    let fg = style.flex_grid.as_deref();

    let in_flow_ids: Vec<BoxId> = b
        .children
        .iter()
        .copied()
        .filter(|&cid| !ctx.tree.get(cid).is_out_of_flow)
        .collect();

    let default_tracks = GridTemplateTrackList::None;
    let default_areas = GridTemplateAreasValue::default();
    let cols_list = fg.map_or(&default_tracks, |f| &f.grid_template_columns_list);
    let rows_list = fg.map_or(&default_tracks, |f| &f.grid_template_rows_list);
    let areas = fg.map_or(&default_areas, |f| &f.grid_template_areas);
    let auto_cols = fg.map_or(&[][..], |f| f.grid_auto_columns.as_slice());
    let auto_rows = fg.map_or(&[][..], |f| f.grid_auto_rows.as_slice());
    let auto_flow = fg.map_or(GridAutoFlowMode::Row, |f| f.grid_auto_flow);
    let col_gap = fg.map_or(0.0, |f| {
        if f.column_gap.has_percentage() {
            0.0
        } else {
            f.column_gap.resolve_px(0.0, &eval).max(0.0)
        }
    });
    let row_gap = fg.map_or(0.0, |f| {
        if f.row_gap.has_percentage() {
            0.0
        } else {
            f.row_gap.resolve_px(0.0, &eval).max(0.0)
        }
    });

    let exp_cols = expand_explicit_axis(
        cols_list, areas, auto_cols, false, None, None, None, col_gap, &eval, None,
    );
    let exp_rows = expand_explicit_axis(
        rows_list, areas, auto_rows, true, None, None, None, row_gap, &eval, None,
    );

    let mut placed = place_grid_items(ctx, &in_flow_ids, &exp_cols, &exp_rows, auto_flow, &eval);
    let is_col_x = axes.col_is_x;
    let (exp_x, auto_x, min_idx, total, gap_x) = if is_col_x {
        (
            &exp_cols,
            auto_cols,
            placed.col_min_idx,
            placed.col_total,
            col_gap,
        )
    } else {
        (
            &exp_rows,
            auto_rows,
            placed.row_min_idx,
            placed.row_total,
            row_gap,
        )
    };

    let mut tracks_min = build_axis_tracks(exp_x, auto_x, min_idx, total, None, &eval);
    let align_for_x = if is_col_x {
        fg.map_or(AlignValue::Normal, |f| f.justify_items)
    } else {
        fg.map_or(AlignValue::Normal, |f| f.align_items)
    };
    for it in &mut placed.items {
        let (mc, min_c, max_c) =
            compute_item_contributions(ctx, it, is_col_x, axes, &tracks_min, None, 0.0, space);
        it.min_contrib = mc;
        it.min_content_contrib = min_c;
        it.max_content_contrib = max_c;
    }
    apply_baseline_shims_to_items(
        ctx,
        &mut placed.items,
        is_col_x,
        axes,
        align_for_x,
        None,
        0.0,
        space,
    );

    let mut tracks_max = tracks_min.clone();
    resolve_tracks(
        &mut tracks_min,
        &placed.items,
        is_col_x,
        None,
        None,
        None,
        gap_x,
        AlignValue::Start,
        SizingConstraint::MinContent,
    );
    resolve_tracks(
        &mut tracks_max,
        &placed.items,
        is_col_x,
        None,
        None,
        None,
        gap_x,
        AlignValue::Start,
        SizingConstraint::MaxContent,
    );

    let min_w: f32 = tracks_min.iter().map(|t| t.base_size).sum::<f32>()
        + active_gaps_in_range(&tracks_min, 0, tracks_min.len(), gap_x);
    let max_w: f32 = tracks_max.iter().map(|t| t.base_size).sum::<f32>()
        + active_gaps_in_range(&tracks_max, 0, tracks_max.len(), gap_x);
    (min_w.max(0.0), max_w.max(min_w).max(0.0))
}

// ---------------------------------------------------------------------------
// Main Grid Container Layout Entry Point (W3C CSS Grid Level 1)
// ---------------------------------------------------------------------------

/// Lays out a CSS Grid container (`display: grid | inline-grid`) per W3C CSS Grid Level 1.
pub fn layout_grid_container(
    ctx: &mut LayoutContext<'_>,
    container_id: BoxId,
    space: ConstraintSpace,
) -> Fragment {
    let b = ctx.tree.get(container_id);
    let style = Arc::clone(&b.style);
    let dom_node = b.dom_node;
    let is_float = b.is_float;
    let is_out_of_flow = b.is_out_of_flow;
    let is_inline_grid = matches!(b.kind, BoxKind::InlineBlock { .. })
        || matches!(style.display, DisplayMode::InlineGrid);
    let child_ids: Vec<BoxId> = b.children.clone();

    let eval = space.eval_context_for(&style);
    let metrics = BoxMetrics::resolve_with_eval(&style, &space, eval);
    let pb_w = metrics.padding.horizontal() + metrics.border.horizontal();
    let pb_h = metrics.padding.vertical() + metrics.border.vertical();
    let is_bb = style.box_sizing == BoxSizingMode::BorderBox;
    let axes = GridAxes::compute(style.text.writing_mode, style.text.direction);

    let fg = style.flex_grid.as_deref();
    let default_tracks = GridTemplateTrackList::None;
    let default_areas = GridTemplateAreasValue::default();
    let cols_list = fg.map_or(&default_tracks, |f| &f.grid_template_columns_list);
    let rows_list = fg.map_or(&default_tracks, |f| &f.grid_template_rows_list);
    let areas = fg.map_or(&default_areas, |f| &f.grid_template_areas);
    let auto_cols = fg.map_or(&[][..], |f| f.grid_auto_columns.as_slice());
    let auto_rows = fg.map_or(&[][..], |f| f.grid_auto_rows.as_slice());
    let auto_flow = fg.map_or(GridAutoFlowMode::Row, |f| f.grid_auto_flow);
    let justify_content = fg.map_or(AlignValue::Normal, |f| f.justify_content);
    let align_content = fg.map_or(AlignValue::Normal, |f| f.align_content);
    let justify_items = fg.map_or(AlignValue::Normal, |f| f.justify_items);
    let align_items = fg.map_or(AlignValue::Normal, |f| f.align_items);

    let mut in_flow_ids = Vec::with_capacity(child_ids.len());
    let mut oof_ids = Vec::new();
    for cid in child_ids {
        if ctx.tree.get(cid).is_out_of_flow {
            oof_ids.push(cid);
        } else {
            in_flow_ids.push(cid);
        }
    }

    // Resolve container's definite/indefinite outer width & height.
    let shrink_w = is_inline_grid
        || is_float
        || is_out_of_flow
        || matches!(
            style.box_model.width,
            SizeValue::MinContent | SizeValue::MaxContent | SizeValue::FitContent(_)
        );

    let definite_w = if let Some(dw) = space.definite_width_override {
        Some(dw.min(metrics.max_width).max(metrics.min_width).max(0.0))
    } else if let Some(sw) = metrics.specified_width {
        Some(sw.min(metrics.max_width).max(metrics.min_width).max(0.0))
    } else if axes.col_is_x && matches!(style.box_model.width, SizeValue::Auto) && !shrink_w {
        let avail_c = (space.available_width - metrics.margin.horizontal() - pb_w).max(0.0);
        Some(
            avail_c
                .min(metrics.max_width)
                .max(metrics.min_width)
                .max(0.0),
        )
    } else {
        None
    };

    let definite_h = if space.ignore_specified_height {
        None
    } else if let Some(dh) = space.definite_height_override {
        Some(dh.min(metrics.max_height).max(metrics.min_height).max(0.0))
    } else {
        metrics
            .specified_height
            .map(|sh| sh.min(metrics.max_height).max(metrics.min_height).max(0.0))
    };

    let min_w_opt = (metrics.min_width > 0.0).then_some(metrics.min_width);
    let max_w_opt = metrics
        .max_width
        .is_finite()
        .then_some(min_w_opt.map_or(metrics.max_width, |mn| metrics.max_width.max(mn)));
    let min_h_opt = (metrics.min_height > 0.0).then_some(metrics.min_height);
    let max_h_opt = metrics
        .max_height
        .is_finite()
        .then_some(min_h_opt.map_or(metrics.max_height, |mn| metrics.max_height.max(mn)));

    let (def_col, min_col, max_col, def_row, min_row, max_row) = if axes.col_is_x {
        (
            definite_w, min_w_opt, max_w_opt, definite_h, min_h_opt, max_h_opt,
        )
    } else {
        (
            definite_h, min_h_opt, max_h_opt, definite_w, min_w_opt, max_w_opt,
        )
    };

    let resolve_gap = |lp: &LengthPercentage, basis: Option<f32>| -> f32 {
        if lp.has_percentage() {
            basis.map_or(0.0, |b| lp.resolve_px(b, &eval).max(0.0))
        } else {
            lp.resolve_px(0.0, &eval).max(0.0)
        }
    };
    let mut col_gap = fg.map_or(0.0, |f| resolve_gap(&f.column_gap, def_col));
    let mut row_gap = fg.map_or(0.0, |f| resolve_gap(&f.row_gap, def_row));

    // Probe single-item intrinsic size if an intrinsic auto-repeat is present (`repeat(auto-fill, auto)`).
    let probe_intrinsic_repeat = |ctx: &mut LayoutContext<'_>, is_col: bool| -> Option<f32> {
        let list = if is_col { cols_list } else { rows_list };
        if !list.has_intrinsic_auto_repeat() || in_flow_ids.is_empty() {
            return None;
        }
        let axis_is_x = if is_col {
            axes.col_is_x
        } else {
            !axes.col_is_x
        };
        let mut best = 0.0f32;
        for &cid in &in_flow_ids {
            let v = if axis_is_x {
                measure_intrinsic_widths(ctx, cid, space).1
            } else {
                measure_item_block_height(ctx, cid, space.available_width, None, space, false)
            };
            best = best.max(v);
        }
        (best > 0.0).then_some(best)
    };
    let col_intr_probe = probe_intrinsic_repeat(ctx, true);
    let row_intr_probe = probe_intrinsic_repeat(ctx, false);

    // 1. Expand explicit track lists and place all in-flow grid items (§7.2 & §8.5).
    let exp_cols = expand_explicit_axis(
        cols_list,
        areas,
        auto_cols,
        false,
        def_col,
        min_col,
        max_col,
        col_gap,
        &eval,
        col_intr_probe,
    );
    let exp_rows = expand_explicit_axis(
        rows_list,
        areas,
        auto_rows,
        true,
        def_row,
        min_row,
        max_row,
        row_gap,
        &eval,
        row_intr_probe,
    );
    let mut placed = place_grid_items(ctx, &in_flow_ids, &exp_cols, &exp_rows, auto_flow, &eval);

    // Build full track arrays and mark empty `auto-fit` tracks as collapsed (§7.2.3.2).
    let mut col_tracks = build_axis_tracks(
        &exp_cols,
        auto_cols,
        placed.col_min_idx,
        placed.col_total,
        def_col,
        &eval,
    );
    let mut row_tracks = build_axis_tracks(
        &exp_rows,
        auto_rows,
        placed.row_min_idx,
        placed.row_total,
        def_row,
        &eval,
    );
    mark_empty_auto_fit_tracks(&mut col_tracks, &placed.items, true);
    mark_empty_auto_fit_tracks(&mut row_tracks, &placed.items, false);

    let col_sizing_constraint = if def_col.is_some() {
        SizingConstraint::Definite(0)
    } else if axes.col_is_x && matches!(style.box_model.width, SizeValue::MinContent) {
        SizingConstraint::MinContent
    } else if axes.col_is_x && matches!(style.box_model.width, SizeValue::MaxContent) {
        SizingConstraint::MaxContent
    } else {
        SizingConstraint::Indefinite
    };

    // 2. Track Sizing Pass 1: Columns (inline axis)
    for it in &mut placed.items {
        let (mc, min_c, max_c) =
            compute_item_contributions(ctx, it, true, axes, &col_tracks, None, 0.0, space);
        it.min_contrib = mc;
        it.min_content_contrib = min_c;
        it.max_content_contrib = max_c;
    }
    apply_baseline_shims_to_items(
        ctx,
        &mut placed.items,
        true,
        axes,
        justify_items,
        None,
        0.0,
        space,
    );
    let eff_max_col = if def_col.is_none()
        && axes.col_is_x
        && shrink_w
        && matches!(style.box_model.width, SizeValue::Auto)
    {
        let avail_c = (space.available_width - metrics.margin.horizontal() - pb_w).max(0.0);
        let capped = max_col.map_or(avail_c, |m| m.min(avail_c));
        Some(min_col.map_or(capped, |mn| capped.max(mn)))
    } else {
        max_col
    };
    resolve_tracks(
        &mut col_tracks,
        &placed.items,
        true,
        def_col,
        min_col,
        eff_max_col,
        col_gap,
        justify_content,
        col_sizing_constraint,
    );

    // Resolve container column size (clamping shrink-to-fit against available width if `inline-grid` / `fit-content`).
    let used_cols_sum: f32 = col_tracks.iter().map(|t| t.base_size).sum::<f32>()
        + active_gaps_in_range(&col_tracks, 0, col_tracks.len(), col_gap);
    let final_col_size = if let Some(dc) = def_col {
        dc
    } else {
        let mut sz = used_cols_sum;
        if axes.col_is_x {
            let avail_c = (space.available_width - metrics.margin.horizontal() - pb_w).max(0.0);
            if matches!(style.box_model.width, SizeValue::FitContent(Some(_))) {
                if let SizeValue::FitContent(Some(lp)) = &style.box_model.width {
                    let lim = lp.resolve_px(space.containing_block_width, &eval).max(0.0);
                    sz = sz.min(lim);
                }
            } else if matches!(style.box_model.width, SizeValue::FitContent(None)) {
                sz = sz.min(avail_c);
            }
            if let Some(mx) = max_col {
                sz = sz.min(mx);
            }
            if let Some(mn) = min_col {
                sz = sz.max(mn);
            }
        } else {
            if let Some(mx) = max_col {
                sz = sz.min(mx);
            }
            if let Some(mn) = min_col {
                sz = sz.max(mn);
            }
        }
        sz.max(0.0)
    };

    // If column size was indefinite and percentage column tracks/gaps exist, re-resolve against `final_col_size` (§7.2.1).
    let has_pct_cols = col_tracks.iter().any(|t| track_has_percentage(&t.raw))
        || fg.is_some_and(|f| f.column_gap.has_percentage());
    if def_col.is_none() && has_pct_cols && final_col_size > 0.0 {
        col_gap = fg.map_or(0.0, |f| resolve_gap(&f.column_gap, Some(final_col_size)));
        for t in &mut col_tracks {
            t.refresh_fns(Some(final_col_size), &eval);
        }
        resolve_tracks(
            &mut col_tracks,
            &placed.items,
            true,
            Some(final_col_size),
            min_col,
            max_col,
            col_gap,
            justify_content,
            SizingConstraint::Definite(0),
        );
    }

    // 3. Track Sizing Pass 2: Rows (block axis) using resolved column track sizes
    for it in &mut placed.items {
        let (mc, min_c, max_c) = compute_item_contributions(
            ctx,
            it,
            false,
            axes,
            &row_tracks,
            Some(&col_tracks),
            col_gap,
            space,
        );
        it.min_contrib = mc;
        it.min_content_contrib = min_c;
        it.max_content_contrib = max_c;
    }
    apply_baseline_shims_to_items(
        ctx,
        &mut placed.items,
        false,
        axes,
        align_items,
        Some(&col_tracks),
        col_gap,
        space,
    );
    let row_sizing_constraint = if def_row.is_some() {
        SizingConstraint::Definite(0)
    } else {
        SizingConstraint::Indefinite
    };
    resolve_tracks(
        &mut row_tracks,
        &placed.items,
        false,
        def_row,
        min_row,
        max_row,
        row_gap,
        align_content,
        row_sizing_constraint,
    );

    let used_rows_sum: f32 = row_tracks.iter().map(|t| t.base_size).sum::<f32>()
        + active_gaps_in_range(&row_tracks, 0, row_tracks.len(), row_gap);
    let final_row_size = if let Some(dr) = def_row {
        dr
    } else {
        let mut sz = used_rows_sum;
        if let Some(mx) = max_row {
            sz = sz.min(mx);
        }
        if let Some(mn) = min_row {
            sz = sz.max(mn);
        }
        sz.max(0.0)
    };

    // Re-resolve percentage row tracks/gaps against `final_row_size` when container row size was indefinite (§7.2.1).
    let has_pct_rows = row_tracks.iter().any(|t| track_has_percentage(&t.raw))
        || fg.is_some_and(|f| f.row_gap.has_percentage());
    if def_row.is_none() && has_pct_rows && final_row_size > 0.0 {
        row_gap = fg.map_or(0.0, |f| resolve_gap(&f.row_gap, Some(final_row_size)));
        for t in &mut row_tracks {
            t.refresh_fns(Some(final_row_size), &eval);
        }
        resolve_tracks(
            &mut row_tracks,
            &placed.items,
            false,
            Some(final_row_size),
            min_row,
            max_row,
            row_gap,
            align_content,
            SizingConstraint::Definite(0),
        );
    }

    // 4. Content Alignment (§10.2–§10.3): compute logical track offsets
    let (col_starts, _col_lines, _) = compute_track_positions(
        &col_tracks,
        col_gap,
        final_col_size,
        justify_content,
        axes.col_is_x,
        axes.col_reversed,
    );
    let (row_starts, _row_lines, _) = compute_track_positions(
        &row_tracks,
        row_gap,
        final_row_size,
        align_content,
        !axes.col_is_x,
        axes.row_reversed,
    );

    let (content_w, content_h) = axes.to_physical(final_col_size, final_row_size);
    let border_w = content_w + pb_w;
    let border_h = content_h + pb_h;
    let content_origin_x = metrics.border.left + metrics.padding.left;
    let content_origin_y = metrics.border.top + metrics.padding.top;

    let oof_start = ctx.pending_oof.len();

    // 5. Lay out every in-flow grid item inside its resolved grid area (§10.4–§10.5).
    let mut col_first_baselines: HashMap<(usize, bool), f32> = HashMap::new();
    let mut col_last_baselines: HashMap<(usize, bool), f32> = HashMap::new();
    let mut row_first_baselines: HashMap<(usize, bool), f32> = HashMap::new();
    let mut row_last_baselines: HashMap<(usize, bool), f32> = HashMap::new();
    let mut item_align_info: Vec<(ResolvedSelfAlign, bool, ResolvedSelfAlign, bool, usize)> =
        Vec::with_capacity(placed.items.len());

    for it in &mut placed.items {
        let c_start = col_starts.get(it.col_start).copied().unwrap_or(0.0);
        let c_end = col_starts
            .get(it.col_end.saturating_sub(1))
            .copied()
            .unwrap_or(0.0)
            + col_tracks
                .get(it.col_end.saturating_sub(1))
                .map_or(0.0, |t| t.base_size);
        let r_start = row_starts.get(it.row_start).copied().unwrap_or(0.0);
        let r_end = row_starts
            .get(it.row_end.saturating_sub(1))
            .copied()
            .unwrap_or(0.0)
            + row_tracks
                .get(it.row_end.saturating_sub(1))
                .map_or(0.0, |t| t.base_size);
        let area_col_sz = (c_end - c_start).max(0.0);
        let area_row_sz = (r_end - r_start).max(0.0);
        it.area_col_start = c_start;
        it.area_col_size = area_col_sz;
        it.area_row_start = r_start;
        it.area_row_size = area_row_sz;

        let (area_w, area_h) = axes.to_physical(area_col_sz, area_row_sz);
        let item_box = ctx.tree.get(it.box_id);
        let item_style = Arc::clone(&item_box.style);
        let is_replaced = matches!(item_box.kind, BoxKind::Replaced(_));
        let item_eval = space.eval_context_for(&item_style);
        let ar = effective_aspect_ratio(item_box, &item_eval);

        let ifg = item_style.flex_grid.as_deref();
        let js_raw = ifg.map_or(AlignValue::Auto, |f| f.justify_self);
        let as_raw = ifg.map_or(AlignValue::Auto, |f| f.align_self);
        let js_eff = if js_raw.base() == AlignValue::Auto {
            justify_items
        } else {
            js_raw
        };
        let as_eff = if as_raw.base() == AlignValue::Auto {
            align_items
        } else {
            as_raw
        };

        let item_col_rev = axes.item_axis_reversed(
            true,
            item_style.text.writing_mode,
            item_style.text.direction,
        );
        let item_row_rev = axes.item_axis_reversed(
            false,
            item_style.text.writing_mode,
            item_style.text.direction,
        );
        let (j_align, j_safe) = resolve_self_align(
            js_eff,
            axes.col_is_x,
            axes.col_reversed,
            item_col_rev,
            is_replaced,
        );
        let (a_align, a_safe) = resolve_self_align(
            as_eff,
            !axes.col_is_x,
            axes.row_reversed,
            item_row_rev,
            is_replaced,
        );

        let mut area_space = space.with_available(area_w, Some(area_h));
        if !axes.col_is_x {
            area_space.cb_inline_size = Some(area_h);
        }
        let im = BoxMetrics::resolve_with_eval(&item_style, &area_space, item_eval);
        let i_pb_w = im.padding.horizontal() + im.border.horizontal();
        let i_pb_h = im.padding.vertical() + im.border.vertical();
        let i_bb = item_style.box_sizing == BoxSizingMode::BorderBox;

        let ml_auto = im.margin_left_auto;
        let mr_auto = im.margin_right_auto;
        let mt_auto = matches!(item_style.box_model.margin.top, LengthPercentageAuto::Auto);
        let mb_auto = matches!(
            item_style.box_model.margin.bottom,
            LengthPercentageAuto::Auto
        );
        let fixed_mx = (if ml_auto { 0.0 } else { im.margin.left })
            + (if mr_auto { 0.0 } else { im.margin.right });
        let fixed_my = (if mt_auto { 0.0 } else { im.margin.top })
            + (if mb_auto { 0.0 } else { im.margin.bottom });

        let (x_align, y_align) = if axes.col_is_x {
            (j_align, a_align)
        } else {
            (a_align, j_align)
        };
        let w_auto = matches!(item_style.box_model.width, SizeValue::Auto);
        let h_auto = matches!(item_style.box_model.height, SizeValue::Auto);
        let stretch_x =
            w_auto && !is_replaced && !ml_auto && !mr_auto && x_align == ResolvedSelfAlign::Stretch;
        let stretch_y =
            h_auto && !is_replaced && !mt_auto && !mb_auto && y_align == ResolvedSelfAlign::Stretch;

        // Compute target content width & height for item layout.
        let mut child_space = area_space;
        if stretch_x {
            let cw = (area_w - fixed_mx - i_pb_w)
                .max(0.0)
                .min(im.max_width)
                .max(im.min_width);
            child_space.available_width = (cw + i_pb_w + fixed_mx).max(0.0);
            child_space.definite_width_override = Some(cw);
        } else if w_auto && !is_replaced {
            // Fit-content width when self-aligned (start/end/center/baseline) or auto-margin.
            let (mn_b, mx_b) = measure_intrinsic_widths(ctx, it.box_id, area_space);
            let avail_c = (area_w - fixed_mx - i_pb_w).max(0.0);
            let mut cw = (mx_b - i_pb_w)
                .max(0.0)
                .min(avail_c)
                .max((mn_b - i_pb_w).max(0.0));
            if let Some(ratio) = ar {
                if let Some(sh) = im.specified_height {
                    cw = sh * ratio;
                } else if stretch_y {
                    let ch = (area_h - fixed_my - i_pb_h)
                        .max(0.0)
                        .min(im.max_height)
                        .max(im.min_height);
                    cw = ch * ratio;
                }
            }
            let clamped = cw.min(im.max_width).max(im.min_width).max(0.0);
            child_space.available_width = (clamped + i_pb_w + fixed_mx).max(0.0);
        } else if let Some(sw) = im.specified_width {
            let clamped = sw.min(im.max_width).max(im.min_width).max(0.0);
            child_space.available_width = (clamped + i_pb_w + fixed_mx).max(0.0);
        }

        if stretch_y {
            let ch = (area_h - fixed_my - i_pb_h)
                .max(0.0)
                .min(im.max_height)
                .max(im.min_height);
            child_space.definite_height_override = Some(ch);
        } else if h_auto && !is_replaced {
            if let Some(ratio) = ar {
                if let Some(sw) = im.specified_width {
                    let ch = (sw / ratio).min(im.max_height).max(im.min_height).max(0.0);
                    child_space.definite_height_override = Some(ch);
                } else if stretch_x {
                    let cw = (area_w - fixed_mx - i_pb_w)
                        .max(0.0)
                        .min(im.max_width)
                        .max(im.min_width);
                    let ch = (cw / ratio).min(im.max_height).max(im.min_height).max(0.0);
                    child_space.definite_height_override = Some(ch);
                }
            }
        }
        let _ = i_bb;

        let item_oof_start = ctx.pending_oof.len();
        let mut frag = layout_box(ctx, it.box_id, child_space);

        // Enforce stretched border-box dimensions for non-replaced items (in case child returned smaller).
        if stretch_x {
            let cw = (area_w - fixed_mx - i_pb_w)
                .max(0.0)
                .min(im.max_width)
                .max(im.min_width);
            frag.border_rect.size.width = cw + i_pb_w;
            frag.content_rect.size.width = cw;
        }
        if stretch_y {
            let ch = (area_h - fixed_my - i_pb_h)
                .max(0.0)
                .min(im.max_height)
                .max(im.min_height);
            frag.border_rect.size.height = ch + i_pb_h;
            frag.content_rect.size.height = ch;
        }

        // Collect column & row baseline group maximums for baseline-aligned items (CSS Align L3 §9).
        let (mc_s_auto, mc_e_auto) = axes.col_margins_auto(&item_style);
        let (mr_s_auto, mr_e_auto) = axes.row_margins_auto(&item_style);
        let (mc_s_raw, mc_e_raw) = axes.col_margins(im.margin);
        let (mr_s_raw, mr_e_raw) = axes.row_margins(im.margin);
        if !mc_s_auto && !mc_e_auto {
            if j_align == ResolvedSelfAlign::Baseline {
                let (side, asc, _) = compute_axis_baseline_metrics(
                    &item_style,
                    frag.border_rect.size.width,
                    frag.border_rect.size.height,
                    frag.baseline,
                    frag.last_baseline,
                    &im,
                    mc_s_raw,
                    mc_e_raw,
                    axes.col_is_x,
                    axes.col_reversed,
                    false,
                );
                col_first_baselines
                    .entry((it.col_start, side))
                    .and_modify(|v| *v = v.max(asc))
                    .or_insert(asc);
            } else if j_align == ResolvedSelfAlign::LastBaseline {
                let (side, _, desc) = compute_axis_baseline_metrics(
                    &item_style,
                    frag.border_rect.size.width,
                    frag.border_rect.size.height,
                    frag.baseline,
                    frag.last_baseline,
                    &im,
                    mc_s_raw,
                    mc_e_raw,
                    axes.col_is_x,
                    axes.col_reversed,
                    true,
                );
                col_last_baselines
                    .entry((it.col_end.saturating_sub(1), side))
                    .and_modify(|v| *v = v.max(desc))
                    .or_insert(desc);
            }
        }
        if !mr_s_auto && !mr_e_auto {
            if a_align == ResolvedSelfAlign::Baseline {
                let (side, asc, _) = compute_axis_baseline_metrics(
                    &item_style,
                    frag.border_rect.size.width,
                    frag.border_rect.size.height,
                    frag.baseline,
                    frag.last_baseline,
                    &im,
                    mr_s_raw,
                    mr_e_raw,
                    !axes.col_is_x,
                    axes.row_reversed,
                    false,
                );
                row_first_baselines
                    .entry((it.row_start, side))
                    .and_modify(|v| *v = v.max(asc))
                    .or_insert(asc);
            } else if a_align == ResolvedSelfAlign::LastBaseline {
                let (side, _, desc) = compute_axis_baseline_metrics(
                    &item_style,
                    frag.border_rect.size.width,
                    frag.border_rect.size.height,
                    frag.baseline,
                    frag.last_baseline,
                    &im,
                    mr_s_raw,
                    mr_e_raw,
                    !axes.col_is_x,
                    axes.row_reversed,
                    true,
                );
                row_last_baselines
                    .entry((it.row_end.saturating_sub(1), side))
                    .and_modify(|v| *v = v.max(desc))
                    .or_insert(desc);
            }
        }

        it.fragment = Some(frag);
        item_align_info.push((j_align, j_safe, a_align, a_safe, item_oof_start));
    }

    // 6. Position every laid-out item inside its grid area with auto-margins, self-alignment,
    //    baseline offsets, writing-mode flips, and relative positioning.
    let mut placed_fragments: Vec<(usize, Fragment)> = Vec::with_capacity(placed.items.len());
    let mut container_first_baseline: Option<f32> = None;
    let mut container_last_baseline: Option<f32> = None;

    for (idx, it) in placed.items.into_iter().enumerate() {
        let mut frag = it.fragment.unwrap();
        let (j_align, j_safe, a_align, a_safe, item_oof_start) = item_align_info[idx];
        let item_style = Arc::clone(&frag.style);
        let (area_w, area_h) = axes.to_physical(it.area_col_size, it.area_row_size);
        let mut area_space = space.with_available(area_w, Some(area_h));
        if !axes.col_is_x {
            area_space.cb_inline_size = Some(area_h);
        }
        let item_eval = area_space.eval_context_for(&item_style);
        let im = BoxMetrics::resolve_with_eval(&item_style, &area_space, item_eval);

        let (m_col_s_auto, m_col_e_auto) = axes.col_margins_auto(&item_style);
        let (m_row_s_auto, m_row_e_auto) = axes.row_margins_auto(&item_style);
        let (m_col_s_raw, m_col_e_raw) = axes.col_margins(im.margin);
        let (m_row_s_raw, m_row_e_raw) = axes.row_margins(im.margin);
        let (frag_col, frag_row) =
            axes.to_logical(frag.border_rect.size.width, frag.border_rect.size.height);

        // Solve column axis offset & auto margins (with baseline offset if participating)
        let col_baseline_off = if !m_col_s_auto && !m_col_e_auto {
            if j_align == ResolvedSelfAlign::Baseline {
                let (side, asc, _) = compute_axis_baseline_metrics(
                    &item_style,
                    frag.border_rect.size.width,
                    frag.border_rect.size.height,
                    frag.baseline,
                    frag.last_baseline,
                    &im,
                    m_col_s_raw,
                    m_col_e_raw,
                    axes.col_is_x,
                    axes.col_reversed,
                    false,
                );
                let group = col_first_baselines
                    .get(&(it.col_start, side))
                    .copied()
                    .unwrap_or(asc);
                Some((group - asc).max(0.0))
            } else if j_align == ResolvedSelfAlign::LastBaseline {
                let (side, _, desc) = compute_axis_baseline_metrics(
                    &item_style,
                    frag.border_rect.size.width,
                    frag.border_rect.size.height,
                    frag.baseline,
                    frag.last_baseline,
                    &im,
                    m_col_s_raw,
                    m_col_e_raw,
                    axes.col_is_x,
                    axes.col_reversed,
                    true,
                );
                let group = col_last_baselines
                    .get(&(it.col_end.saturating_sub(1), side))
                    .copied()
                    .unwrap_or(desc);
                Some(
                    (it.area_col_size - m_col_s_raw - m_col_e_raw - frag_col - (group - desc))
                        .max(0.0),
                )
            } else {
                None
            }
        } else {
            None
        };

        let (col_rel, mc_s, mc_e) = solve_grid_item_axis(
            it.area_col_size,
            frag_col,
            m_col_s_auto,
            m_col_e_auto,
            m_col_s_raw,
            m_col_e_raw,
            j_align,
            j_safe,
            col_baseline_off,
        );

        // Solve row axis offset & auto margins (with baseline offset if participating)
        let row_baseline_off = if !m_row_s_auto && !m_row_e_auto {
            if a_align == ResolvedSelfAlign::Baseline {
                let (side, asc, _) = compute_axis_baseline_metrics(
                    &item_style,
                    frag.border_rect.size.width,
                    frag.border_rect.size.height,
                    frag.baseline,
                    frag.last_baseline,
                    &im,
                    m_row_s_raw,
                    m_row_e_raw,
                    !axes.col_is_x,
                    axes.row_reversed,
                    false,
                );
                let group = row_first_baselines
                    .get(&(it.row_start, side))
                    .copied()
                    .unwrap_or(asc);
                Some((group - asc).max(0.0))
            } else if a_align == ResolvedSelfAlign::LastBaseline {
                let (side, _, desc) = compute_axis_baseline_metrics(
                    &item_style,
                    frag.border_rect.size.width,
                    frag.border_rect.size.height,
                    frag.baseline,
                    frag.last_baseline,
                    &im,
                    m_row_s_raw,
                    m_row_e_raw,
                    !axes.col_is_x,
                    axes.row_reversed,
                    true,
                );
                let group = row_last_baselines
                    .get(&(it.row_end.saturating_sub(1), side))
                    .copied()
                    .unwrap_or(desc);
                Some(
                    (it.area_row_size - m_row_s_raw - m_row_e_raw - frag_row - (group - desc))
                        .max(0.0),
                )
            } else {
                None
            }
        } else {
            None
        };

        let (row_rel, mr_s, mr_e) = solve_grid_item_axis(
            it.area_row_size,
            frag_row,
            m_row_s_auto,
            m_row_e_auto,
            m_row_s_raw,
            m_row_e_raw,
            a_align,
            a_safe,
            row_baseline_off,
        );

        let log_col_pos = it.area_col_start + col_rel;
        let log_row_pos = it.area_row_start + row_rel;

        let phys_col = if axes.col_reversed {
            final_col_size - log_col_pos - frag_col
        } else {
            log_col_pos
        };
        let phys_row = if axes.row_reversed {
            final_row_size - log_row_pos - frag_row
        } else {
            log_row_pos
        };
        let (dx, dy) = axes.to_physical(phys_col, phys_row);
        let base_origin = Point::new(content_origin_x + dx, content_origin_y + dy);

        // Write resolved physical margins back onto fragment
        frag.margin = if axes.col_is_x {
            let (ml, mr) = if axes.col_reversed {
                (mc_e, mc_s)
            } else {
                (mc_s, mc_e)
            };
            let (mt, mb) = if axes.row_reversed {
                (mr_e, mr_s)
            } else {
                (mr_s, mr_e)
            };
            EdgeInsets {
                top: mt,
                right: mr,
                bottom: mb,
                left: ml,
            }
        } else {
            let (mt, mb) = if axes.col_reversed {
                (mc_e, mc_s)
            } else {
                (mc_s, mc_e)
            };
            let (ml, mr) = if axes.row_reversed {
                (mr_e, mr_s)
            } else {
                (mr_s, mr_e)
            };
            EdgeInsets {
                top: mt,
                right: mr,
                bottom: mb,
                left: ml,
            }
        };

        frag.border_rect.origin = base_origin;
        if item_style.position == PositionMode::Relative {
            apply_relative_offset(&mut frag, area_space);
        }
        let final_origin = frag.border_rect.origin;
        offset_pending_oof(
            &mut ctx.pending_oof,
            item_oof_start,
            final_origin.x,
            final_origin.y,
        );

        // Track container first & last baseline (§10.6)
        if axes.col_is_x {
            if it.row_start == 0 {
                let fb = final_origin.y + frag.baseline.unwrap_or(frag.border_rect.size.height);
                container_first_baseline =
                    Some(container_first_baseline.map_or(fb, |cur| cur.min(fb)));
            }
            if it.row_end == row_tracks.len() {
                let lb = final_origin.y
                    + frag
                        .last_baseline
                        .or(frag.baseline)
                        .unwrap_or(frag.border_rect.size.height);
                container_last_baseline =
                    Some(container_last_baseline.map_or(lb, |cur| cur.max(lb)));
            }
        }

        placed_fragments.push((it.dom_index, frag));
    }

    // Paint order follows CSS `order` (already sorted); keep fragments in order-modified document order.
    let mut children_frags: Vec<Fragment> = placed_fragments.into_iter().map(|(_, f)| f).collect();

    // 7. CSS Grid L1 §9: Push direct out-of-flow (`position: absolute | fixed`) children
    //    carrying exact resolved grid-area containing block & static-position hints.
    let pad_rect = Rect::new(
        metrics.border.left,
        metrics.border.top,
        content_w + metrics.padding.horizontal(),
        content_h + metrics.padding.vertical(),
    );
    let (pad_col_s, pad_col_e, cont_col_s, cont_col_e) = if axes.col_is_x {
        (
            metrics.border.left,
            metrics.border.left + pad_rect.size.width,
            content_origin_x,
            content_origin_x + content_w,
        )
    } else {
        (
            metrics.border.top,
            metrics.border.top + pad_rect.size.height,
            content_origin_y,
            content_origin_y + content_h,
        )
    };
    let (pad_row_s, pad_row_e, cont_row_s, cont_row_e) = if axes.col_is_x {
        (
            metrics.border.top,
            metrics.border.top + pad_rect.size.height,
            content_origin_y,
            content_origin_y + content_h,
        )
    } else {
        (
            metrics.border.left,
            metrics.border.left + pad_rect.size.width,
            content_origin_x,
            content_origin_x + content_w,
        )
    };

    for oof_id in oof_ids {
        let oof_style = Arc::clone(&ctx.tree.get(oof_id).style);
        let is_fixed = oof_style.position == PositionMode::Fixed;
        let ofg = oof_style.flex_grid.as_deref();
        let cs = ofg.map_or(&GridPlacementValue::Auto, |f| &f.grid_column_start);
        let ce = ofg.map_or(&GridPlacementValue::Auto, |f| &f.grid_column_end);
        let rs = ofg.map_or(&GridPlacementValue::Auto, |f| &f.grid_row_start);
        let re = ofg.map_or(&GridPlacementValue::Auto, |f| &f.grid_row_end);

        // Compute logical edges relative to border-box origin, then flip to physical if reversed.
        let (cb_cs, cb_ce, st_cs, st_ce) = resolve_abspos_axis_edges(
            cs,
            ce,
            &exp_cols,
            placed.col_min_idx,
            &col_tracks,
            &col_starts,
            pad_col_s,
            pad_col_e,
            cont_col_s,
            cont_col_e,
            &eval,
        );
        let (cb_rs, cb_re, st_rs, st_re) = resolve_abspos_axis_edges(
            rs,
            re,
            &exp_rows,
            placed.row_min_idx,
            &row_tracks,
            &row_starts,
            pad_row_s,
            pad_row_e,
            cont_row_s,
            cont_row_e,
            &eval,
        );

        let flip_interval = |s: f32, e: f32, rev: bool, total_border: f32| -> (f32, f32) {
            if rev {
                (total_border - e, (e - s).max(0.0))
            } else {
                (s, (e - s).max(0.0))
            }
        };
        let (tot_col_b, tot_row_b) = axes.to_logical(border_w, border_h);
        let (p_cb_c, p_cb_cw) = flip_interval(cb_cs, cb_ce, axes.col_reversed, tot_col_b);
        let (p_cb_r, p_cb_rh) = flip_interval(cb_rs, cb_re, axes.row_reversed, tot_row_b);
        let (p_st_c, p_st_cw) = flip_interval(st_cs, st_ce, axes.col_reversed, tot_col_b);
        let (p_st_r, p_st_rh) = flip_interval(st_rs, st_re, axes.row_reversed, tot_row_b);

        let (cb_x, cb_y) = axes.to_physical(p_cb_c, p_cb_r);
        let (cb_w, cb_h) = axes.to_physical(p_cb_cw, p_cb_rh);
        let (st_x, st_y) = axes.to_physical(p_st_c, p_st_r);
        let (st_w, st_h) = axes.to_physical(p_st_cw, p_st_rh);

        let grid_area_cb = Rect::new(cb_x, cb_y, cb_w, cb_h);
        let content_box_rect = Rect::new(st_x, st_y, st_w, st_h);

        ctx.pending_oof.push(PendingOof {
            box_id: oof_id,
            static_pos: grid_area_cb.origin,
            is_fixed,
            flex_hint: None,
            grid_hint: Some(GridAbsposHint {
                is_direct_child: true,
                grid_area_cb,
                content_box_rect,
                axes,
                justify_self: ofg.map_or(AlignValue::Auto, |f| f.justify_self),
                align_self: ofg.map_or(AlignValue::Auto, |f| f.align_self),
                justify_items,
                align_items,
            }),
        });
    }

    // Resolve OOF descendants if this grid container establishes an abs/fixed containing block.
    resolve_oof_for_container(
        ctx,
        oof_start,
        &style,
        pad_rect,
        false,
        space,
        &mut children_frags,
    );

    // 8. Format §7.2.6 resolved track listings for `getComputedStyle()`.
    let resolved_tracks = Arc::new(ResolvedGridTracks {
        columns: format_resolved_tracks(&col_tracks, &exp_cols, placed.col_min_idx),
        rows: format_resolved_tracks(&row_tracks, &exp_rows, placed.row_min_idx),
    });

    let _ = is_bb;
    let mut frag = Fragment {
        box_id: container_id,
        dom_node,
        style,
        border_rect: Rect::new(0.0, 0.0, border_w, border_h),
        padding: metrics.padding,
        border: metrics.border,
        margin: metrics.margin,
        content_rect: Rect::new(content_origin_x, content_origin_y, content_w, content_h),
        baseline: container_first_baseline,
        last_baseline: container_last_baseline.or(container_first_baseline),
        overflow_rect: Rect::new(0.0, 0.0, border_w, border_h),
        kind: FragmentKind::Box,
        children: Arc::new(children_frags),
        is_float,
        is_out_of_flow,
        collapsed_margin_top: metrics.margin.top,
        collapsed_margin_bottom: metrics.margin.bottom,
        resolved_grid_tracks: Some(resolved_tracks),
    };
    frag.compute_overflow_rect();
    frag
}
