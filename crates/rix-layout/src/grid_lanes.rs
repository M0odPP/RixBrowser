//! Native pure-Rust W3C CSS Grid Layout Module Level 3 (`display: grid-lanes`) layout engine.
//!
//! Implements:
//! - `display: grid-lanes | inline-grid-lanes | inline grid-lanes`
//! - `grid-lanes-direction`: `normal` (rows when `grid-template-rows` is set and
//!   `grid-template-columns` is `none`, else columns), `column` / `row`, `track-reverse`, `fill-reverse`
//! - `fit-tolerance`: `normal` (`1em`), `<length-percentage>`, `infinite`
//! - `grid-lanes-pack`: `normal` and `dense` gap backfilling
//! - §4.1 Virtual items for grid-axis track sizing (every auto-placed item of span `k` contributes
//!   to every valid `k`-track span on the grid axis, plus intrinsic `repeat(auto-fill|auto-fit, ...)`
//!   track probing)
//! - §3.3 Running-position lane packing along the stacking axis with gap accounting, item alignment,
//!   out-of-flow §9 abspos hints, and §7.2.6 resolved track listing (`"none"` on the stacking axis).

use std::sync::Arc;

use rix_core::{Point, Rect};
use rix_css::grid_css::{
    FitToleranceValue, GridLanesDirectionMode, GridLanesPackMode, GridTemplateTrackList,
};
use rix_css::{
    AlignValue, BoxSizingMode, DisplayMode, EvalContext, GridPlacementValue,
    GridTemplateAreasValue, LengthPercentage, LengthPercentageAuto, PositionMode, SizeValue,
};

use crate::bfc::apply_relative_offset;
use crate::box_tree::{BoxId, BoxKind};
use crate::fragment::{
    layout_box, measure_intrinsic_widths, resolve_size_with_intrinsic, BoxMetrics, ConstraintSpace,
    Fragment, FragmentKind, LayoutContext, ResolvedGridTracks,
};
use crate::grid::{compute_item_contributions, effective_aspect_ratio, measure_item_block_height};
use crate::grid_tracks::{
    active_gaps_in_range, build_axis_tracks, compute_track_positions, expand_explicit_axis,
    format_resolved_tracks, mark_empty_auto_fit_tracks, resolve_abspos_axis_edges,
    resolve_axis_placement, resolve_self_align, resolve_tracks, solve_grid_item_axis,
    track_has_percentage, AxisPlacement, GridAbsposHint, GridAxes, GridItem, ResolvedSelfAlign,
    SizingConstraint, Track,
};
use crate::position::{offset_pending_oof, resolve_oof_for_container, PendingOof};

/// Returns `true` if `display` establishes a CSS Grid Level 3 grid-lanes formatting context.
#[inline]
#[must_use]
pub const fn is_grid_lanes_display(display: DisplayMode) -> bool {
    matches!(
        display,
        DisplayMode::GridLanes | DisplayMode::InlineGridLanes | DisplayMode::InlineGridLanesMulti
    )
}

/// Resolved grid-lanes orientation and reversal flags per CSS Grid Level 3 §3.1.
#[derive(Debug, Clone, Copy)]
struct LanesDirection {
    /// `true` when the grid axis is rows (`grid-template-rows`) and items stack horizontally;
    /// `false` when the grid axis is columns (`grid-template-columns`) and items stack vertically.
    grid_axis_is_row: bool,
    /// Reverses the visual order of tracks along the grid axis (`track-reverse`).
    track_reverse: bool,
    /// Reverses the stacking fill direction within each lane (`fill-reverse`).
    fill_reverse: bool,
}

impl LanesDirection {
    fn resolve(
        mode: GridLanesDirectionMode,
        cols_list: &GridTemplateTrackList,
        rows_list: &GridTemplateTrackList,
    ) -> Self {
        match mode {
            GridLanesDirectionMode::Normal => {
                let grid_axis_is_row = !rows_list.is_none() && cols_list.is_none();
                Self {
                    grid_axis_is_row,
                    track_reverse: false,
                    fill_reverse: false,
                }
            }
            GridLanesDirectionMode::Explicit {
                is_row,
                track_reverse,
                fill_reverse,
                ..
            } => Self {
                grid_axis_is_row: is_row,
                track_reverse,
                fill_reverse,
            },
        }
    }
}

fn resolve_fit_tolerance_px(
    tol: &FitToleranceValue,
    stack_avail: Option<f32>,
    eval: &EvalContext,
) -> f32 {
    match tol {
        FitToleranceValue::Normal => eval.font_size_px.max(0.0),
        FitToleranceValue::Infinite => f32::INFINITY,
        FitToleranceValue::Length(lp) => {
            if lp.has_percentage() {
                stack_avail.map_or(0.0, |a| lp.resolve_px(a, eval).max(0.0))
            } else {
                lp.resolve_px(0.0, eval).max(0.0)
            }
        }
    }
}

/// Computes `(min_content_width, max_content_width)` border-box sizes for a `grid-lanes` container.
pub fn measure_grid_lanes_intrinsic_widths(
    ctx: &mut LayoutContext<'_>,
    container_id: BoxId,
    space: ConstraintSpace,
) -> (f32, f32) {
    let b = ctx.tree.get(container_id);
    let style = Arc::clone(&b.style);
    let eval = space.eval_context_for(&style);
    let metrics = BoxMetrics::resolve_with_eval(&style, &space, eval);
    let pb_w = metrics.padding.horizontal() + metrics.border.horizontal();
    let is_bb = style.box_sizing == BoxSizingMode::BorderBox;

    let (content_min, content_max) =
        measure_grid_lanes_content_intrinsic_widths(ctx, container_id, space);
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

fn measure_grid_lanes_content_intrinsic_widths(
    ctx: &mut LayoutContext<'_>,
    container_id: BoxId,
    space: ConstraintSpace,
) -> (f32, f32) {
    let b = ctx.tree.get(container_id);
    let style = Arc::clone(&b.style);
    let eval = space.eval_context_for(&style);
    let axes = GridAxes::compute(style.text.writing_mode, style.text.direction);
    let fg = style.flex_grid.as_deref();

    let default_tracks = GridTemplateTrackList::None;
    let default_areas = GridTemplateAreasValue::default();
    let cols_list = fg.map_or(&default_tracks, |f| &f.grid_template_columns_list);
    let rows_list = fg.map_or(&default_tracks, |f| &f.grid_template_rows_list);
    let areas = fg.map_or(&default_areas, |f| &f.grid_template_areas);
    let auto_cols = fg.map_or(&[][..], |f| f.grid_auto_columns.as_slice());
    let auto_rows = fg.map_or(&[][..], |f| f.grid_auto_rows.as_slice());
    let dir_mode = fg.map_or(GridLanesDirectionMode::Normal, |f| f.grid_lanes_direction);
    let ldir = LanesDirection::resolve(dir_mode, cols_list, rows_list);

    let in_flow_ids: Vec<BoxId> = b
        .children
        .iter()
        .copied()
        .filter(|&cid| !ctx.tree.get(cid).is_out_of_flow)
        .collect();

    let grid_axis_is_col = !ldir.grid_axis_is_row;
    let grid_axis_is_x = if grid_axis_is_col {
        axes.col_is_x
    } else {
        !axes.col_is_x
    };

    if !grid_axis_is_x {
        // Physical X is the stacking axis: intrinsic width is the maximum item width.
        let mut min_w = 0.0f32;
        let mut max_w = 0.0f32;
        for &cid in &in_flow_ids {
            let cs = &ctx.tree.get(cid).style;
            let cm = BoxMetrics::resolve(cs, &space);
            let (mn, mx) = measure_intrinsic_widths(ctx, cid, space);
            min_w = min_w.max(mn + cm.margin.horizontal());
            max_w = max_w.max(mx + cm.margin.horizontal());
        }
        return (min_w, max_w.max(min_w));
    }

    let (grid_list, auto_grid, gap_lp) = if grid_axis_is_col {
        (cols_list, auto_cols, fg.map(|f| &f.column_gap))
    } else {
        (rows_list, auto_rows, fg.map(|f| &f.row_gap))
    };
    let grid_gap = gap_lp.map_or(0.0, |lp| {
        if lp.has_percentage() {
            0.0
        } else {
            lp.resolve_px(0.0, &eval).max(0.0)
        }
    });
    let exp_grid = expand_explicit_axis(
        grid_list,
        areas,
        auto_grid,
        ldir.grid_axis_is_row,
        None,
        None,
        None,
        grid_gap,
        &eval,
        None,
    );
    let (grid_min_idx, grid_total, placements) =
        resolve_lanes_grid_placements(ctx, &in_flow_ids, &exp_grid, grid_axis_is_col, &eval);
    let mut tracks_min =
        build_axis_tracks(&exp_grid, auto_grid, grid_min_idx, grid_total, None, &eval);
    let virtual_items = build_virtual_grid_items(
        ctx,
        &placements,
        grid_min_idx,
        grid_total,
        grid_axis_is_col,
        axes,
        &tracks_min,
        space,
    );
    let mut tracks_max = tracks_min.clone();
    resolve_tracks(
        &mut tracks_min,
        &virtual_items,
        grid_axis_is_col,
        None,
        None,
        None,
        grid_gap,
        AlignValue::Start,
        SizingConstraint::MinContent,
    );
    resolve_tracks(
        &mut tracks_max,
        &virtual_items,
        grid_axis_is_col,
        None,
        None,
        None,
        grid_gap,
        AlignValue::Start,
        SizingConstraint::MaxContent,
    );
    let min_w: f32 = tracks_min.iter().map(|t| t.base_size).sum::<f32>()
        + active_gaps_in_range(&tracks_min, 0, tracks_min.len(), grid_gap);
    let max_w: f32 = tracks_max.iter().map(|t| t.base_size).sum::<f32>()
        + active_gaps_in_range(&tracks_max, 0, tracks_max.len(), grid_gap);
    (min_w.max(0.0), max_w.max(min_w).max(0.0))
}

struct LaneItemPlacement {
    box_id: BoxId,
    dom_index: usize,
    order: i32,
    placement: AxisPlacement,
}

fn resolve_lanes_grid_placements(
    ctx: &LayoutContext<'_>,
    in_flow_ids: &[BoxId],
    exp_grid: &crate::grid_tracks::ExplicitAxis,
    grid_axis_is_col: bool,
    eval: &EvalContext,
) -> (i32, usize, Vec<LaneItemPlacement>) {
    let mut entries = Vec::with_capacity(in_flow_ids.len());
    let mut min_idx = 0i32;
    let mut max_idx = exp_grid.track_count() as i32;
    let mut max_auto_span = 1i32;

    for (dom_index, &bid) in in_flow_ids.iter().enumerate() {
        let style = &ctx.tree.get(bid).style;
        let fg = style.flex_grid.as_deref();
        let order = fg.map_or(0, |f| f.order);
        let (s_val, e_val) = if grid_axis_is_col {
            (
                fg.map_or(&GridPlacementValue::Auto, |f| &f.grid_column_start),
                fg.map_or(&GridPlacementValue::Auto, |f| &f.grid_column_end),
            )
        } else {
            (
                fg.map_or(&GridPlacementValue::Auto, |f| &f.grid_row_start),
                fg.map_or(&GridPlacementValue::Auto, |f| &f.grid_row_end),
            )
        };
        let p = resolve_axis_placement(s_val, e_val, exp_grid, eval);
        match p {
            AxisPlacement::Definite { start, end } => {
                min_idx = min_idx.min(start);
                max_idx = max_idx.max(end);
            }
            AxisPlacement::Auto { span } => {
                max_auto_span = max_auto_span.max(span);
            }
        }
        entries.push(LaneItemPlacement {
            box_id: bid,
            dom_index,
            order,
            placement: p,
        });
    }
    entries.sort_by_key(|e| (e.order, e.dom_index));

    let total = if max_idx - min_idx == 0 {
        if entries.is_empty() {
            0
        } else {
            max_auto_span.max(1) as usize
        }
    } else {
        (max_idx - min_idx).max(max_auto_span) as usize
    };
    (min_idx, total, entries)
}

/// Builds CSS Grid Level 3 §4.1 virtual grid items for grid-axis track sizing:
/// definite items contribute to their placed span; auto-placed items of span `k` contribute to
/// every valid `k`-track span `[i .. i + k]` along the grid axis.
#[allow(clippy::too_many_arguments)]
fn build_virtual_grid_items(
    ctx: &mut LayoutContext<'_>,
    entries: &[LaneItemPlacement],
    grid_min_idx: i32,
    grid_total: usize,
    grid_axis_is_col: bool,
    axes: GridAxes,
    tracks: &[Track],
    space: ConstraintSpace,
) -> Vec<GridItem> {
    if grid_total == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for entry in entries {
        match entry.placement {
            AxisPlacement::Definite { start, end } => {
                let s = (start - grid_min_idx).clamp(0, grid_total as i32) as usize;
                let e = (end - grid_min_idx).clamp((s + 1) as i32, grid_total as i32) as usize;
                let mut item =
                    make_dummy_grid_item(entry.box_id, entry.dom_index, grid_axis_is_col, s, e);
                let (mc, min_c, max_c) = compute_item_contributions(
                    ctx,
                    &item,
                    grid_axis_is_col,
                    axes,
                    tracks,
                    None,
                    0.0,
                    space,
                );
                item.min_contrib = mc;
                item.min_content_contrib = min_c;
                item.max_content_contrib = max_c;
                out.push(item);
            }
            AxisPlacement::Auto { span } => {
                let k = (span as usize).clamp(1, grid_total);
                // Measure once on the first candidate span and project to every active span [i..i+k].
                let probe =
                    make_dummy_grid_item(entry.box_id, entry.dom_index, grid_axis_is_col, 0, k);
                let (mc, min_c, max_c) = compute_item_contributions(
                    ctx,
                    &probe,
                    grid_axis_is_col,
                    axes,
                    tracks,
                    None,
                    0.0,
                    space,
                );
                for s in 0..=(grid_total - k) {
                    if tracks[s..s + k].iter().all(|t| t.is_collapsed) {
                        continue;
                    }
                    let mut v = make_dummy_grid_item(
                        entry.box_id,
                        entry.dom_index,
                        grid_axis_is_col,
                        s,
                        s + k,
                    );
                    v.min_contrib = mc;
                    v.min_content_contrib = min_c;
                    v.max_content_contrib = max_c;
                    out.push(v);
                }
            }
        }
    }
    out
}

fn make_dummy_grid_item(
    box_id: BoxId,
    dom_index: usize,
    grid_axis_is_col: bool,
    s: usize,
    e: usize,
) -> GridItem {
    let (cs, ce, rs, re) = if grid_axis_is_col {
        (s, e, 0, 1)
    } else {
        (0, 1, s, e)
    };
    GridItem {
        box_id,
        dom_index,
        col_start: cs,
        col_end: ce,
        row_start: rs,
        row_end: re,
        min_contrib: 0.0,
        min_content_contrib: 0.0,
        max_content_contrib: 0.0,
        fragment: None,
        area_col_start: 0.0,
        area_col_size: 0.0,
        area_row_start: 0.0,
        area_row_size: 0.0,
    }
}

/// Lays out a CSS Grid Level 3 `grid-lanes` container.
pub fn layout_grid_lanes_container(
    ctx: &mut LayoutContext<'_>,
    container_id: BoxId,
    space: ConstraintSpace,
) -> Fragment {
    let b = ctx.tree.get(container_id);
    let style = Arc::clone(&b.style);
    let dom_node = b.dom_node;
    let is_float = b.is_float;
    let is_out_of_flow = b.is_out_of_flow;
    let is_inline = matches!(b.kind, BoxKind::InlineBlock { .. })
        || matches!(
            style.display,
            DisplayMode::InlineGridLanes | DisplayMode::InlineGridLanesMulti
        );
    let child_ids: Vec<BoxId> = b.children.clone();

    let eval = space.eval_context_for(&style);
    let metrics = BoxMetrics::resolve_with_eval(&style, &space, eval);
    let pb_w = metrics.padding.horizontal() + metrics.border.horizontal();
    let pb_h = metrics.padding.vertical() + metrics.border.vertical();
    let axes = GridAxes::compute(style.text.writing_mode, style.text.direction);

    let fg = style.flex_grid.as_deref();
    let default_tracks = GridTemplateTrackList::None;
    let default_areas = GridTemplateAreasValue::default();
    let cols_list = fg.map_or(&default_tracks, |f| &f.grid_template_columns_list);
    let rows_list = fg.map_or(&default_tracks, |f| &f.grid_template_rows_list);
    let areas = fg.map_or(&default_areas, |f| &f.grid_template_areas);
    let auto_cols = fg.map_or(&[][..], |f| f.grid_auto_columns.as_slice());
    let auto_rows = fg.map_or(&[][..], |f| f.grid_auto_rows.as_slice());
    let dir_mode = fg.map_or(GridLanesDirectionMode::Normal, |f| f.grid_lanes_direction);
    let pack_mode = fg.map_or(GridLanesPackMode::Normal, |f| f.grid_lanes_pack);
    let fit_tol_val = fg.map_or(FitToleranceValue::Normal, |f| f.fit_tolerance.clone());
    let justify_content = fg.map_or(AlignValue::Normal, |f| f.justify_content);
    let align_content = fg.map_or(AlignValue::Normal, |f| f.align_content);
    let justify_items = fg.map_or(AlignValue::Normal, |f| f.justify_items);
    let align_items = fg.map_or(AlignValue::Normal, |f| f.align_items);

    let ldir = LanesDirection::resolve(dir_mode, cols_list, rows_list);
    let grid_axis_is_col = !ldir.grid_axis_is_row;

    let mut in_flow_ids = Vec::with_capacity(child_ids.len());
    let mut oof_ids = Vec::new();
    for cid in child_ids {
        if ctx.tree.get(cid).is_out_of_flow {
            oof_ids.push(cid);
        } else {
            in_flow_ids.push(cid);
        }
    }

    let shrink_w = is_inline
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

    let (def_grid, min_grid, max_grid, def_stack, min_stack, max_stack) = if grid_axis_is_col {
        (def_col, min_col, max_col, def_row, min_row, max_row)
    } else {
        (def_row, min_row, max_row, def_col, min_col, max_col)
    };

    let resolve_gap = |lp: &LengthPercentage, basis: Option<f32>| -> f32 {
        if lp.has_percentage() {
            basis.map_or(0.0, |b| lp.resolve_px(b, &eval).max(0.0))
        } else {
            lp.resolve_px(0.0, &eval).max(0.0)
        }
    };
    let (grid_list, auto_grid, grid_gap_lp, stack_gap_lp, grid_content_align, grid_items_align) =
        if grid_axis_is_col {
            (
                cols_list,
                auto_cols,
                fg.map(|f| &f.column_gap),
                fg.map(|f| &f.row_gap),
                justify_content,
                justify_items,
            )
        } else {
            (
                rows_list,
                auto_rows,
                fg.map(|f| &f.row_gap),
                fg.map(|f| &f.column_gap),
                align_content,
                align_items,
            )
        };
    let mut grid_gap = grid_gap_lp.map_or(0.0, |lp| resolve_gap(lp, def_grid));
    let stack_gap = stack_gap_lp.map_or(0.0, |lp| resolve_gap(lp, def_stack));

    let grid_axis_is_x = if grid_axis_is_col {
        axes.col_is_x
    } else {
        !axes.col_is_x
    };

    // Probe single-item intrinsic size for intrinsic auto-repeat (`repeat(auto-fill, auto)`).
    let grid_intr_probe = if grid_list.has_intrinsic_auto_repeat() && !in_flow_ids.is_empty() {
        let mut best = 0.0f32;
        for &cid in &in_flow_ids {
            let v = if grid_axis_is_x {
                measure_intrinsic_widths(ctx, cid, space).1
            } else {
                measure_item_block_height(ctx, cid, space.available_width, None, space, false)
            };
            best = best.max(v);
        }
        (best > 0.0).then_some(best)
    } else {
        None
    };

    // 1. Expand explicit tracks on the grid axis & resolve 1D placements.
    let exp_grid = expand_explicit_axis(
        grid_list,
        areas,
        auto_grid,
        ldir.grid_axis_is_row,
        def_grid,
        min_grid,
        max_grid,
        grid_gap,
        &eval,
        grid_intr_probe,
    );
    let (grid_min_idx, grid_total, entries) =
        resolve_lanes_grid_placements(ctx, &in_flow_ids, &exp_grid, grid_axis_is_col, &eval);
    let mut grid_tracks = build_axis_tracks(
        &exp_grid,
        auto_grid,
        grid_min_idx,
        grid_total,
        def_grid,
        &eval,
    );

    // 2. Collapse empty auto-fit tracks when fewer items exist than auto-fit tracks (§7.2.3.2).
    // Note: definite items occupy their tracks; auto-placed items occupy up to `sum(auto spans)` tracks.
    let mut occupied_preview: Vec<GridItem> = Vec::new();
    let mut auto_cursor = 0usize;
    for entry in &entries {
        match entry.placement {
            AxisPlacement::Definite { start, end } => {
                let s = (start - grid_min_idx).clamp(0, grid_total as i32) as usize;
                let e = (end - grid_min_idx).clamp((s + 1) as i32, grid_total as i32) as usize;
                occupied_preview.push(make_dummy_grid_item(
                    entry.box_id,
                    entry.dom_index,
                    grid_axis_is_col,
                    s,
                    e,
                ));
            }
            AxisPlacement::Auto { span } => {
                if grid_total > 0 {
                    let k = (span as usize).clamp(1, grid_total);
                    let s = auto_cursor % (grid_total - k + 1).max(1);
                    occupied_preview.push(make_dummy_grid_item(
                        entry.box_id,
                        entry.dom_index,
                        grid_axis_is_col,
                        s,
                        s + k,
                    ));
                    auto_cursor = auto_cursor.saturating_add(k);
                }
            }
        }
    }
    mark_empty_auto_fit_tracks(&mut grid_tracks, &occupied_preview, grid_axis_is_col);

    // 3. Build §4.1 virtual items and run W3C §11 Track Sizing Algorithm on the grid axis.
    let virtual_items = build_virtual_grid_items(
        ctx,
        &entries,
        grid_min_idx,
        grid_total,
        grid_axis_is_col,
        axes,
        &grid_tracks,
        space,
    );
    let grid_sizing_constraint = if def_grid.is_some() {
        SizingConstraint::Definite(0)
    } else if grid_axis_is_x && matches!(style.box_model.width, SizeValue::MinContent) {
        SizingConstraint::MinContent
    } else if grid_axis_is_x && matches!(style.box_model.width, SizeValue::MaxContent) {
        SizingConstraint::MaxContent
    } else {
        SizingConstraint::Indefinite
    };
    resolve_tracks(
        &mut grid_tracks,
        &virtual_items,
        grid_axis_is_col,
        def_grid,
        min_grid,
        max_grid,
        grid_gap,
        grid_content_align,
        grid_sizing_constraint,
    );

    let used_grid_sum: f32 = grid_tracks.iter().map(|t| t.base_size).sum::<f32>()
        + active_gaps_in_range(&grid_tracks, 0, grid_tracks.len(), grid_gap);
    let final_grid_size = if let Some(dg) = def_grid {
        dg
    } else {
        let mut sz = used_grid_sum;
        if let Some(mx) = max_grid {
            sz = sz.min(mx);
        }
        if let Some(mn) = min_grid {
            sz = sz.max(mn);
        }
        sz.max(0.0)
    };

    // Re-resolve percentage grid-axis tracks/gaps if container grid size was indefinite.
    let has_pct_grid = grid_tracks.iter().any(|t| track_has_percentage(&t.raw))
        || grid_gap_lp.is_some_and(|lp| lp.has_percentage());
    if def_grid.is_none() && has_pct_grid && final_grid_size > 0.0 {
        grid_gap = grid_gap_lp.map_or(0.0, |lp| resolve_gap(lp, Some(final_grid_size)));
        for t in &mut grid_tracks {
            t.refresh_fns(Some(final_grid_size), &eval);
        }
        resolve_tracks(
            &mut grid_tracks,
            &virtual_items,
            grid_axis_is_col,
            Some(final_grid_size),
            min_grid,
            max_grid,
            grid_gap,
            grid_content_align,
            SizingConstraint::Definite(0),
        );
    }

    let grid_axis_rev = if grid_axis_is_col {
        axes.col_reversed
    } else {
        axes.row_reversed
    };
    let (grid_starts, _grid_lines, used_tracks_extent) = compute_track_positions(
        &grid_tracks,
        grid_gap,
        final_grid_size,
        grid_content_align,
        grid_axis_is_x,
        grid_axis_rev,
    );

    // 4. CSS Grid Level 3 §3.3 Running-Position Lane Placement along the stacking axis.
    let lane_count = grid_tracks.len();
    let mut running_pos = vec![0.0f32; lane_count];
    let mut has_item_in_lane = vec![false; lane_count];
    // For `grid-lanes-pack: dense`, record occupied [start, end] intervals per lane.
    let mut lane_intervals: Vec<Vec<(f32, f32)>> = vec![Vec::new(); lane_count];
    let tolerance = resolve_fit_tolerance_px(&fit_tol_val, def_stack, &eval);
    let is_dense = matches!(pack_mode, GridLanesPackMode::Dense);

    let oof_start = ctx.pending_oof.len();

    struct PlacedLaneItem {
        frag: Fragment,
        grid_pos: f32,
        grid_border_sz: f32,
        stack_pos: f32,
        stack_outer_sz: f32,
        stack_margin_start: f32,
        stack_margin_end: f32,
        item_oof_start: usize,
        item_oof_end: usize,
        area_space: ConstraintSpace,
    }
    let mut placed_items: Vec<PlacedLaneItem> = Vec::with_capacity(entries.len());

    let mut auto_tie_cursor = 0usize;
    for entry in &entries {
        if lane_count == 0 {
            continue;
        }
        let item_box = ctx.tree.get(entry.box_id);
        let item_style = Arc::clone(&item_box.style);
        let is_replaced = matches!(item_box.kind, BoxKind::Replaced(_));
        let item_eval = space.eval_context_for(&item_style);
        let ar = effective_aspect_ratio(item_box, &item_eval);

        // Choose lane span `(s, e)` on the grid axis.
        let (s, e, dense_y_override) = match entry.placement {
            AxisPlacement::Definite { start, end } => {
                let s = (start - grid_min_idx).clamp(0, lane_count as i32 - 1) as usize;
                let e = (end - grid_min_idx).clamp((s + 1) as i32, lane_count as i32) as usize;
                (s, e, None)
            }
            AxisPlacement::Auto { span } => {
                let k = (span as usize).clamp(1, lane_count);
                // Candidate span start positions (skip spans where all tracks are collapsed).
                let mut best_pos = f32::INFINITY;
                let mut candidates: Vec<(usize, f32)> = Vec::new();
                for cand in 0..=(lane_count - k) {
                    if grid_tracks[cand..cand + k].iter().all(|t| t.is_collapsed) {
                        continue;
                    }
                    let pos = running_pos[cand..cand + k]
                        .iter()
                        .copied()
                        .fold(0.0f32, f32::max);
                    best_pos = best_pos.min(pos);
                    candidates.push((cand, pos));
                }
                if candidates.is_empty() {
                    (0, k, None)
                } else {
                    let threshold = if tolerance.is_infinite() {
                        f32::INFINITY
                    } else {
                        best_pos + tolerance
                    };
                    // Pick earliest eligible candidate at or after tie-break cursor (wrap around).
                    let chosen = candidates
                        .iter()
                        .find(|&&(c, p)| c >= auto_tie_cursor && p <= threshold + 1e-4)
                        .or_else(|| candidates.iter().find(|&&(_, p)| p <= threshold + 1e-4))
                        .map_or(candidates[0].0, |&(c, _)| c);
                    auto_tie_cursor = (chosen + k) % lane_count.max(1);

                    // For `dense`, probe if the item fits into an earlier empty gap in any span.
                    let mut dense_pick: Option<(usize, f32)> = None;
                    if is_dense && k == 1 && best_pos > 0.0 {
                        let lane_w_0 = grid_tracks[chosen].base_size;
                        let est_stack = if grid_axis_is_x {
                            measure_item_block_height(
                                ctx,
                                entry.box_id,
                                lane_w_0,
                                None,
                                space,
                                false,
                            )
                        } else {
                            measure_intrinsic_widths(ctx, entry.box_id, space).1
                        };
                        for (lane_i, ivs) in lane_intervals.iter().enumerate() {
                            if grid_tracks[lane_i].is_collapsed || ivs.is_empty() {
                                continue;
                            }
                            let mut prev_end = 0.0f32;
                            for &(iv_s, iv_e) in ivs {
                                let gap_start = if prev_end > 0.0 {
                                    prev_end + stack_gap
                                } else {
                                    0.0
                                };
                                let gap_end = (iv_s - stack_gap).max(gap_start);
                                if gap_end - gap_start >= est_stack - 0.5 && gap_start < best_pos {
                                    dense_pick = Some((lane_i, gap_start));
                                    break;
                                }
                                prev_end = prev_end.max(iv_e);
                            }
                            if dense_pick.is_some() {
                                break;
                            }
                        }
                    }
                    if let Some((dl, dy)) = dense_pick {
                        (dl, dl + 1, Some(dy))
                    } else {
                        (chosen, chosen + k, None)
                    }
                }
            }
        };

        let g_start = grid_starts.get(s).copied().unwrap_or(0.0);
        let g_end = grid_starts.get(e - 1).copied().unwrap_or(0.0)
            + grid_tracks.get(e - 1).map_or(0.0, |t| t.base_size);
        let area_grid_sz = (g_end - g_start).max(0.0);

        // Compute alignment along the grid axis.
        let ifg = item_style.flex_grid.as_deref();
        let raw_self = if grid_axis_is_col {
            ifg.map_or(AlignValue::Auto, |f| f.justify_self)
        } else {
            ifg.map_or(AlignValue::Auto, |f| f.align_self)
        };
        let eff_self = if raw_self.base() == AlignValue::Auto {
            grid_items_align
        } else {
            raw_self
        };
        let item_grid_rev = axes.item_axis_reversed(
            grid_axis_is_col,
            item_style.text.writing_mode,
            item_style.text.direction,
        );
        let (g_align, g_safe) = resolve_self_align(
            eff_self,
            grid_axis_is_x,
            grid_axis_rev,
            item_grid_rev,
            is_replaced,
        );

        let (area_w, area_h_opt) = if grid_axis_is_x {
            (area_grid_sz, def_stack)
        } else {
            (
                def_stack.unwrap_or(space.available_width),
                Some(area_grid_sz),
            )
        };
        let mut area_space = space.with_available(area_w, area_h_opt);
        if !axes.col_is_x {
            area_space.cb_inline_size = area_h_opt;
        }
        let im = BoxMetrics::resolve_with_eval(&item_style, &area_space, item_eval);
        let i_pb_w = im.padding.horizontal() + im.border.horizontal();
        let i_pb_h = im.padding.vertical() + im.border.vertical();

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

        let w_auto = matches!(item_style.box_model.width, SizeValue::Auto);
        let h_auto = matches!(item_style.box_model.height, SizeValue::Auto);
        let stretch_x = grid_axis_is_x
            && w_auto
            && !is_replaced
            && !ml_auto
            && !mr_auto
            && g_align == ResolvedSelfAlign::Stretch;
        let stretch_y = !grid_axis_is_x
            && h_auto
            && !is_replaced
            && !mt_auto
            && !mb_auto
            && g_align == ResolvedSelfAlign::Stretch;

        let mut child_space = area_space;
        if stretch_x {
            let cw = (area_w - fixed_mx - i_pb_w)
                .max(0.0)
                .min(im.max_width)
                .max(im.min_width);
            child_space.available_width = (cw + i_pb_w + fixed_mx).max(0.0);
            child_space.definite_width_override = Some(cw);
        } else if w_auto && !is_replaced {
            let (mn_b, mx_b) = measure_intrinsic_widths(ctx, entry.box_id, area_space);
            let avail_c = if grid_axis_is_x {
                (area_w - fixed_mx - i_pb_w).max(0.0)
            } else {
                (mx_b - i_pb_w).max(0.0)
            };
            let mut cw = (mx_b - i_pb_w)
                .max(0.0)
                .min(avail_c)
                .max((mn_b - i_pb_w).max(0.0));
            if let Some(ratio) = ar {
                if let Some(sh) = im.specified_height {
                    cw = sh * ratio;
                } else if stretch_y {
                    let ch = (area_grid_sz - fixed_my - i_pb_h)
                        .max(0.0)
                        .min(im.max_height)
                        .max(im.min_height);
                    cw = ch * ratio;
                }
            }
            let clamped = cw.min(im.max_width).max(im.min_width).max(0.0);
            child_space.available_width = (clamped + i_pb_w + fixed_mx).max(0.0);
            if !grid_axis_is_x {
                child_space.definite_width_override = Some(clamped);
            }
        } else if let Some(sw) = im.specified_width {
            let clamped = sw.min(im.max_width).max(im.min_width).max(0.0);
            child_space.available_width = (clamped + i_pb_w + fixed_mx).max(0.0);
        }

        if stretch_y {
            let ch = (area_grid_sz - fixed_my - i_pb_h)
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

        let item_oof_start = ctx.pending_oof.len();
        let mut frag = layout_box(ctx, entry.box_id, child_space);
        let item_oof_end = ctx.pending_oof.len();

        if stretch_x {
            let cw = (area_w - fixed_mx - i_pb_w)
                .max(0.0)
                .min(im.max_width)
                .max(im.min_width);
            frag.border_rect.size.width = cw + i_pb_w;
            frag.content_rect.size.width = cw;
        }
        if stretch_y {
            let ch = (area_grid_sz - fixed_my - i_pb_h)
                .max(0.0)
                .min(im.max_height)
                .max(im.min_height);
            frag.border_rect.size.height = ch + i_pb_h;
            frag.content_rect.size.height = ch;
        }

        // Compute logical margins & offsets along grid axis and stacking axis.
        let (mg_s_auto, mg_e_auto, ms_s_auto, ms_e_auto) = if grid_axis_is_col {
            let (csa, cea) = axes.col_margins_auto(&item_style);
            let (rsa, rea) = axes.row_margins_auto(&item_style);
            (csa, cea, rsa, rea)
        } else {
            let (rsa, rea) = axes.row_margins_auto(&item_style);
            let (csa, cea) = axes.col_margins_auto(&item_style);
            (rsa, rea, csa, cea)
        };
        let (mg_s_raw, mg_e_raw, ms_s_raw, ms_e_raw) = if grid_axis_is_col {
            let (cs, ce) = axes.col_margins(im.margin);
            let (rs, re) = axes.row_margins(im.margin);
            (cs, ce, rs, re)
        } else {
            let (rs, re) = axes.row_margins(im.margin);
            let (cs, ce) = axes.col_margins(im.margin);
            (rs, re, cs, ce)
        };

        let (frag_grid, frag_stack) = if grid_axis_is_x {
            (frag.border_rect.size.width, frag.border_rect.size.height)
        } else {
            (frag.border_rect.size.height, frag.border_rect.size.width)
        };

        let (grid_rel, _mg_s, _mg_e) = solve_grid_item_axis(
            area_grid_sz,
            frag_grid,
            mg_s_auto,
            mg_e_auto,
            mg_s_raw,
            mg_e_raw,
            g_align,
            g_safe,
            None,
        );
        let sm_s = if ms_s_auto { 0.0 } else { ms_s_raw };
        let sm_e = if ms_e_auto { 0.0 } else { ms_e_raw };
        let outer_stack = (frag_stack + sm_s + sm_e).max(0.0);

        let stack_pos = if let Some(dy) = dense_y_override {
            dy
        } else {
            let max_rp = running_pos[s..e].iter().copied().fold(0.0f32, f32::max);
            let any_prev = has_item_in_lane[s..e].iter().any(|&h| h);
            if any_prev {
                max_rp + stack_gap
            } else {
                max_rp
            }
        };
        let item_stack_end = stack_pos + outer_stack;
        for lane in s..e {
            if dense_y_override.is_none() {
                running_pos[lane] = running_pos[lane].max(item_stack_end);
                has_item_in_lane[lane] = true;
            }
            let ivs = &mut lane_intervals[lane];
            ivs.push((stack_pos, item_stack_end));
            ivs.sort_by(|a, b| a.0.total_cmp(&b.0));
        }

        placed_items.push(PlacedLaneItem {
            frag,
            grid_pos: g_start + grid_rel,
            grid_border_sz: frag_grid,
            stack_pos,
            stack_outer_sz: outer_stack,
            stack_margin_start: sm_s,
            stack_margin_end: sm_e,
            item_oof_start,
            item_oof_end,
            area_space,
        });
    }

    // 5. Final container dimensions (applying `track-reverse` & `fill-reverse`).
    let used_stack = running_pos.iter().copied().fold(0.0f32, f32::max);
    let final_stack_size = if let Some(ds) = def_stack {
        ds
    } else {
        let mut sz = used_stack;
        if let Some(mx) = max_stack {
            sz = sz.min(mx);
        }
        if let Some(mn) = min_stack {
            sz = sz.max(mn);
        }
        sz.max(0.0)
    };

    let (final_col_size, final_row_size) = if grid_axis_is_col {
        (final_grid_size, final_stack_size)
    } else {
        (final_stack_size, final_grid_size)
    };
    let (content_w, content_h) = axes.to_physical(final_col_size, final_row_size);
    let border_w = content_w + pb_w;
    let border_h = content_h + pb_h;
    let content_origin_x = metrics.border.left + metrics.padding.left;
    let content_origin_y = metrics.border.top + metrics.padding.top;

    // Effective track-reverse extent (tracks span `used_tracks_extent` or `final_grid_size`).
    let track_rev_span = if used_tracks_extent > 0.0
        && (def_grid.is_none() || (ldir.track_reverse && used_tracks_extent < final_grid_size))
    {
        used_tracks_extent
    } else {
        final_grid_size
    };

    let mut children_frags = Vec::with_capacity(placed_items.len());
    let stack_axis_rev = if grid_axis_is_col {
        axes.row_reversed
    } else {
        axes.col_reversed
    };

    for mut pi in placed_items {
        let log_grid = if ldir.track_reverse {
            track_rev_span - pi.grid_pos - pi.grid_border_sz
        } else {
            pi.grid_pos
        };
        let log_stack = if ldir.fill_reverse {
            final_stack_size - pi.stack_pos - pi.stack_outer_sz + pi.stack_margin_end
        } else {
            pi.stack_pos + pi.stack_margin_start
        };

        let phys_grid = if grid_axis_rev {
            final_grid_size - log_grid - pi.grid_border_sz
        } else {
            log_grid
        };
        let frag_stack_b = if grid_axis_is_x {
            pi.frag.border_rect.size.height
        } else {
            pi.frag.border_rect.size.width
        };
        let phys_stack = if stack_axis_rev {
            final_stack_size - log_stack - frag_stack_b
        } else {
            log_stack
        };

        let (dx, dy) = if grid_axis_is_x {
            (phys_grid, phys_stack)
        } else {
            (phys_stack, phys_grid)
        };
        pi.frag.border_rect.origin = Point::new(content_origin_x + dx, content_origin_y + dy);
        if pi.frag.style.position == PositionMode::Relative {
            apply_relative_offset(&mut pi.frag, pi.area_space);
        }
        let final_origin = pi.frag.border_rect.origin;
        if pi.item_oof_end > pi.item_oof_start {
            offset_pending_oof(
                &mut ctx.pending_oof[..pi.item_oof_end],
                pi.item_oof_start,
                final_origin.x,
                final_origin.y,
            );
        }
        children_frags.push(pi.frag);
    }

    // 6. Out-of-flow abspos children (CSS Grid L1 §9 adapted for grid-lanes).
    let pad_rect = Rect::new(
        metrics.border.left,
        metrics.border.top,
        content_w + metrics.padding.horizontal(),
        content_h + metrics.padding.vertical(),
    );
    let (pad_g_s, pad_g_e, cont_g_s, cont_g_e) = if grid_axis_is_x {
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
    let (pad_s_s, pad_s_e, cont_s_s, cont_s_e) = if grid_axis_is_x {
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
        let (gs, ge) = if grid_axis_is_col {
            (
                ofg.map_or(&GridPlacementValue::Auto, |f| &f.grid_column_start),
                ofg.map_or(&GridPlacementValue::Auto, |f| &f.grid_column_end),
            )
        } else {
            (
                ofg.map_or(&GridPlacementValue::Auto, |f| &f.grid_row_start),
                ofg.map_or(&GridPlacementValue::Auto, |f| &f.grid_row_end),
            )
        };
        let (cb_gs, cb_ge, _, _) = resolve_abspos_axis_edges(
            gs,
            ge,
            &exp_grid,
            grid_min_idx,
            &grid_tracks,
            &grid_starts,
            pad_g_s,
            pad_g_e,
            cont_g_s,
            cont_g_e,
            &eval,
        );
        let (cb_x, cb_y, cb_w, cb_h) = if grid_axis_is_x {
            (
                cb_gs,
                pad_s_s,
                (cb_ge - cb_gs).max(0.0),
                (pad_s_e - pad_s_s).max(0.0),
            )
        } else {
            (
                pad_s_s,
                cb_gs,
                (pad_s_e - pad_s_s).max(0.0),
                (cb_ge - cb_gs).max(0.0),
            )
        };
        let (cx, cy, cw, ch) = if grid_axis_is_x {
            (
                cont_g_s,
                cont_s_s,
                (cont_g_e - cont_g_s).max(0.0),
                (cont_s_e - cont_s_s).max(0.0),
            )
        } else {
            (
                cont_s_s,
                cont_g_s,
                (cont_s_e - cont_s_s).max(0.0),
                (cont_g_e - cont_g_s).max(0.0),
            )
        };
        let grid_area_cb = Rect::new(cb_x, cb_y, cb_w, cb_h);
        let content_box_rect = Rect::new(cx, cy, cw, ch);
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

    resolve_oof_for_container(
        ctx,
        oof_start,
        &style,
        pad_rect,
        false,
        space,
        &mut children_frags,
    );

    // 7. Format §7.2.6 resolved tracks (`"none"` on the stacking axis).
    let resolved_grid_str = format_resolved_tracks(&grid_tracks, &exp_grid, grid_min_idx);
    let resolved_tracks = Arc::new(if grid_axis_is_col {
        ResolvedGridTracks {
            columns: resolved_grid_str,
            rows: "none".to_string(),
        }
    } else {
        ResolvedGridTracks {
            columns: "none".to_string(),
            rows: resolved_grid_str,
        }
    });

    let mut frag = Fragment {
        box_id: container_id,
        dom_node,
        style,
        border_rect: Rect::new(0.0, 0.0, border_w, border_h),
        padding: metrics.padding,
        border: metrics.border,
        margin: metrics.margin,
        content_rect: Rect::new(content_origin_x, content_origin_y, content_w, content_h),
        baseline: None,
        last_baseline: None,
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
