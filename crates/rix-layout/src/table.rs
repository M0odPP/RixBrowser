//! W3C CSS Tables Level 3 formatting context (`display: table`, `inline-table`,
//! `table-caption`, `table-column-group`, `table-column`, `table-header-group`,
//! `table-row-group`, `table-footer-group`, `table-row`, `table-cell`).
//!
//! Orchestrates intrinsic sizing (`measure_table_intrinsic_widths`) and full
//! multi-pass table layout (`layout_table`) using the grid, border-collapse,
//! and track distribution solvers in [`crate::table_grid`].

use std::sync::Arc;

use rix_core::{Point, Rect};
use rix_css::{
    BorderCollapseMode, BoxSizingMode, ColorRgba, ComputedStyle, EmptyCellsMode, LineStyle,
    SizeValue, VerticalAlignMode, VisibilityMode,
};

use crate::box_tree::{BoxId, TableSectionKind};
use crate::fragment::{
    layout_box, measure_intrinsic_widths, BoxMetrics, ConstraintSpace, EdgeInsets, Fragment,
    FragmentKind, LayoutContext,
};
use crate::table_grid::{
    build_table_grid, cell_collapsed_half_borders, compute_auto_col_constraints,
    compute_fixed_col_constraints, compute_percent_adjusted_max_cols, decompose_size,
    distribute_height_to_rows, is_cell_baseline_align, resolve_col_span_rect,
    solve_auto_column_widths, solve_fixed_column_widths, to_border_box_dim, CollapsedBorderGrid,
    TableGrid, INFINITE_TABLE_MAX_W,
};

/// Computes the minimum border-box width imposed by all captions (`tentative/caption.html`).
fn measure_captions_min_width(
    ctx: &mut LayoutContext<'_>,
    grid: &TableGrid,
    space: ConstraintSpace,
) -> f32 {
    let mut cap_min = 0.0f32;
    for &cid in grid.top_captions.iter().chain(grid.bottom_captions.iter()) {
        let cap_style = Arc::clone(&ctx.tree.get(cid).style);
        let cap_eval = ctx.eval_context_for(&cap_style, &space);
        let cap_m = BoxMetrics::resolve_with_eval(&cap_style, &space, cap_eval);
        let is_pct_w = matches!(
            &cap_style.box_model.width,
            SizeValue::Length(lp) if lp.has_percentage()
        );
        let (cmin, _) = match (!is_pct_w, cap_m.specified_width) {
            (true, Some(spec)) => {
                let pb = cap_m.padding.horizontal() + cap_m.border.horizontal();
                let sw = cap_m.clamp_content_width(spec) + pb;
                (sw, sw)
            }
            _ => measure_intrinsic_widths(ctx, cid, space),
        };
        let ml = if cap_m.margin_left_auto
            || matches!(&cap_style.box_model.margin.left, rix_css::LengthPercentageAuto::Value(lp) if lp.has_percentage())
        {
            0.0
        } else {
            cap_m.margin.left
        };
        let mr = if cap_m.margin_right_auto
            || matches!(&cap_style.box_model.margin.right, rix_css::LengthPercentageAuto::Value(lp) if lp.has_percentage())
        {
            0.0
        } else {
            cap_m.margin.right
        };
        cap_min = cap_min.max(cmin + ml + mr);
    }
    cap_min
}

fn table_outer_pb(
    style: &ComputedStyle,
    metrics: &BoxMetrics,
    collapsed_borders: Option<&CollapsedBorderGrid>,
) -> (EdgeInsets, EdgeInsets) {
    if let Some(cb) = collapsed_borders {
        (cb.outer_half, EdgeInsets::ZERO)
    } else if style.text.border_collapse == BorderCollapseMode::Collapse {
        (
            EdgeInsets {
                top: metrics.border.top * 0.5,
                right: metrics.border.right * 0.5,
                bottom: metrics.border.bottom * 0.5,
                left: metrics.border.left * 0.5,
            },
            EdgeInsets::ZERO,
        )
    } else {
        (metrics.border, metrics.padding)
    }
}

/// Computes `(min_content_width, max_content_width)` border-box sizes for a table.
pub fn measure_table_intrinsic_widths(
    ctx: &mut LayoutContext<'_>,
    table_id: BoxId,
    space: ConstraintSpace,
) -> (f32, f32) {
    let style = Arc::clone(&ctx.tree.get(table_id).style);
    let eval = ctx.eval_context_for(&style, &space);
    let metrics = BoxMetrics::resolve_with_eval(&style, &space, eval);
    let collapsed = style.text.border_collapse == BorderCollapseMode::Collapse;
    let h_spacing = if collapsed {
        0.0
    } else {
        style
            .text
            .border_spacing_x
            .resolve_px(space.available_width, &eval)
            .max(0.0)
    };

    let grid = build_table_grid(ctx, table_id, space);
    let (eff_border, eff_padding) =
        table_outer_pb(&style, &metrics, grid.collapsed_borders.as_ref());
    let pb_w = eff_border.horizontal() + eff_padding.horizontal();
    let is_bb = style.box_sizing == BoxSizingMode::BorderBox;
    let cap_min_w = measure_captions_min_width(ctx, &grid, space);

    let (raw_min_grid, raw_max_grid) = if grid.num_cols == 0 {
        (pb_w, pb_w)
    } else if grid.is_fixed_layout {
        let cols = compute_fixed_col_constraints(ctx, &grid, h_spacing, space);
        let total_spacing = h_spacing * (grid.num_cols + 1) as f32;
        let sum_min: f32 = cols.iter().map(|c| c.fixed_px.unwrap_or(0.0)).sum();
        let min_grid = sum_min + total_spacing + pb_w;
        // CSS Tables L3 §4.5 / `table-fixed-minmax.html`: a fixed table with `%` width
        // has infinite max-content width (represented by `INFINITE_TABLE_MAX_W`).
        let is_pct_table = matches!(
            &style.box_model.width,
            SizeValue::Length(lp) if lp.has_percentage()
        );
        let max_grid = if is_pct_table {
            INFINITE_TABLE_MAX_W
        } else {
            let auto_cols = compute_auto_col_constraints(ctx, &grid, h_spacing, space);
            let sum_max: f32 = auto_cols.iter().map(|c| c.max_w).sum();
            (sum_max + total_spacing + pb_w).max(min_grid)
        };
        (min_grid, max_grid)
    } else {
        let cols = compute_auto_col_constraints(ctx, &grid, h_spacing, space);
        let total_spacing = h_spacing * (grid.num_cols + 1) as f32;
        let sum_min: f32 = cols.iter().map(|c| c.min_w).sum();
        let sum_max: f32 = cols.iter().map(|c| c.max_w).sum();
        let pct_max = compute_percent_adjusted_max_cols(&cols);
        let eff_max = if pct_max.is_finite() {
            pct_max
        } else {
            sum_max
        };
        (
            sum_min + total_spacing + pb_w,
            eff_max + total_spacing + pb_w,
        )
    };

    let (spec_fixed, _, _) = decompose_size(&style.box_model.width, &eval);
    let (min_fixed, _, _) = decompose_size(&style.box_model.min_width, &eval);
    let (max_fixed, _, _) = decompose_size(&style.box_model.max_width, &eval);

    let mut min_w = raw_min_grid.max(cap_min_w);
    let mut max_w = raw_max_grid.max(min_w);

    if let Some(mx) = max_fixed {
        let mx_bb = to_border_box_dim(mx, pb_w, is_bb);
        max_w = max_w.min(mx_bb).max(min_w);
    }
    if let Some(fw) = spec_fixed {
        let fw_bb = to_border_box_dim(fw, pb_w, is_bb);
        let eff_fw = if let Some(mx) = max_fixed {
            fw_bb.min(to_border_box_dim(mx, pb_w, is_bb))
        } else {
            fw_bb
        };
        min_w = min_w.max(eff_fw);
        max_w = max_w.max(eff_fw);
    }
    if let Some(mn) = min_fixed {
        let mn_bb = to_border_box_dim(mn, pb_w, is_bb);
        min_w = min_w.max(mn_bb);
        max_w = max_w.max(mn_bb);
    }

    (min_w, max_w.max(min_w))
}

/// Emits zero-DOM overlay fragments for resolved collapsed borders so paint renders
/// exact CSS 2.1 §17.6.2.1 conflict-resolved borders without double-painting.
#[allow(clippy::too_many_arguments)]
fn emit_collapsed_border_fragments(
    cb: &CollapsedBorderGrid,
    grid_origin_x: f32,
    grid_origin_y: f32,
    col_x: &[f32],
    col_w: &[f32],
    col_collapsed: &[bool],
    row_y: &[f32],
    row_h: &[f32],
    row_collapsed: &[bool],
    base_style: &Arc<ComputedStyle>,
    out: &mut Vec<Fragment>,
) {
    let num_rows = row_h.len();
    let num_cols = col_w.len();
    if num_rows == 0 || num_cols == 0 {
        return;
    }

    let make_bar = |rect: Rect, color: ColorRgba| -> Fragment {
        let mut st = (**base_style).clone();
        Arc::make_mut(&mut st.visual).background_color = color;
        Arc::make_mut(&mut st.visual).background_layers.clear();
        Arc::make_mut(&mut st.visual).border_style = rix_css::EdgeValues::all(LineStyle::None);
        Arc::make_mut(&mut st.box_model).border_width = rix_css::EdgeValues::all(0.0);
        st.visibility = VisibilityMode::Visible;
        Fragment {
            box_id: BoxId(0),
            dom_node: None,
            style: Arc::new(st),
            border_rect: rect,
            padding: EdgeInsets::ZERO,
            border: EdgeInsets::ZERO,
            margin: EdgeInsets::ZERO,
            content_rect: Rect::new(0.0, 0.0, rect.size.width, rect.size.height),
            baseline: None,
            last_baseline: None,
            overflow_rect: Rect::new(0.0, 0.0, rect.size.width, rect.size.height),
            kind: FragmentKind::Box,
            children: Arc::new(Vec::new()),
            is_float: false,
            is_out_of_flow: false,
            collapsed_margin_top: 0.0,
            collapsed_margin_bottom: 0.0,
            resolved_grid_tracks: None,
        }
    };

    // Grid line X coordinates (`num_cols + 1`) and Y coordinates (`num_rows + 1`).
    let mut gx = Vec::with_capacity(num_cols + 1);
    gx.extend_from_slice(&col_x[..num_cols]);
    gx.push(col_x[num_cols - 1] + col_w[num_cols - 1]);

    let mut gy = Vec::with_capacity(num_rows + 1);
    gy.extend_from_slice(&row_y[..num_rows]);
    gy.push(row_y[num_rows - 1] + row_h[num_rows - 1]);

    // Horizontal segments.
    for r in 0..=num_rows {
        for c in 0..num_cols {
            if col_collapsed[c] {
                continue;
            }
            let e = cb.h_edges[r][c];
            if e.width <= 0.0
                || matches!(e.style, LineStyle::None | LineStyle::Hidden)
                || e.color.a == 0
            {
                continue;
            }
            let left_ext = cb
                .v_edges
                .get(r.min(num_rows - 1))
                .map_or(0.0, |vr| vr[c].width * 0.5);
            let right_ext = cb
                .v_edges
                .get(r.min(num_rows - 1))
                .map_or(0.0, |vr| vr[c + 1].width * 0.5);
            let x0 = grid_origin_x + gx[c] - left_ext;
            let x1 = grid_origin_x + gx[c + 1] + right_ext;
            let y0 = grid_origin_y + gy[r] - e.width * 0.5;
            out.push(make_bar(
                Rect::new(x0, y0, (x1 - x0).max(0.0), e.width),
                e.color,
            ));
        }
    }

    // Vertical segments.
    for r in 0..num_rows {
        if row_collapsed[r] {
            continue;
        }
        for c in 0..=num_cols {
            let e = cb.v_edges[r][c];
            if e.width <= 0.0
                || matches!(e.style, LineStyle::None | LineStyle::Hidden)
                || e.color.a == 0
            {
                continue;
            }
            let x0 = grid_origin_x + gx[c] - e.width * 0.5;
            let y0 = grid_origin_y + gy[r];
            let h = (gy[r + 1] - gy[r]).max(0.0);
            if h > 0.0 {
                out.push(make_bar(Rect::new(x0, y0, e.width, h), e.color));
            }
        }
    }
}

/// Lays out a CSS Table (`display: table` / `inline-table`) per W3C CSS Tables Level 3.
pub fn layout_table(
    ctx: &mut LayoutContext<'_>,
    table_id: BoxId,
    space: ConstraintSpace,
) -> Fragment {
    let style = Arc::clone(&ctx.tree.get(table_id).style);
    let eval = ctx.eval_context_for(&style, &space);
    let metrics = BoxMetrics::resolve_with_eval(&style, &space, eval);
    let is_bb = style.box_sizing == BoxSizingMode::BorderBox;
    let collapsed = style.text.border_collapse == BorderCollapseMode::Collapse;
    let h_spacing = if collapsed {
        0.0
    } else {
        style
            .text
            .border_spacing_x
            .resolve_px(space.available_width, &eval)
            .max(0.0)
    };
    let v_spacing = if collapsed {
        0.0
    } else {
        style
            .text
            .border_spacing_y
            .resolve_px(space.available_width, &eval)
            .max(0.0)
    };

    let grid = build_table_grid(ctx, table_id, space);
    let (eff_border, eff_padding) =
        table_outer_pb(&style, &metrics, grid.collapsed_borders.as_ref());
    let pb_w = eff_border.horizontal() + eff_padding.horizontal();
    let pb_h = eff_border.vertical() + eff_padding.vertical();

    let cap_min_w = measure_captions_min_width(ctx, &grid, space);

    // Compute table min/max/used width and solve column widths.
    let col_constraints = if grid.num_cols == 0 {
        Vec::new()
    } else if grid.is_fixed_layout {
        compute_fixed_col_constraints(ctx, &grid, h_spacing, space)
    } else {
        compute_auto_col_constraints(ctx, &grid, h_spacing, space)
    };

    let total_h_spacing = if grid.num_cols > 0 {
        h_spacing * (grid.num_cols + 1) as f32
    } else {
        0.0
    };

    let (grid_min_cols, raw_max_cols, pct_max_cols) = if grid.num_cols == 0 {
        (0.0, 0.0, 0.0)
    } else if grid.is_fixed_layout {
        let sum_min: f32 = col_constraints
            .iter()
            .map(|c| c.fixed_px.unwrap_or(0.0))
            .sum();
        (sum_min, sum_min, sum_min)
    } else {
        let smin: f32 = col_constraints.iter().map(|c| c.min_w).sum();
        let smax: f32 = col_constraints.iter().map(|c| c.max_w).sum();
        let pmax = compute_percent_adjusted_max_cols(&col_constraints);
        (smin, smax, pmax)
    };

    let grid_min_border_w = grid_min_cols + total_h_spacing + pb_w;
    let avail_border_w = (space.available_width - metrics.margin.horizontal()).max(0.0);

    // Resolve min-width / max-width in table's `box-sizing` (border-box).
    let eff_min_border_w = match &style.box_model.min_width {
        SizeValue::Length(lp) => to_border_box_dim(
            lp.resolve_px(space.containing_block_width, &eval).max(0.0),
            pb_w,
            is_bb,
        ),
        _ => pb_w,
    };
    let eff_max_border_w = match &style.box_model.max_width {
        SizeValue::Length(lp) => to_border_box_dim(
            lp.resolve_px(space.containing_block_width, &eval).max(0.0),
            pb_w,
            is_bb,
        ),
        _ => f32::INFINITY,
    };

    let desired_border_w = if let Some(ov_cw) = space.definite_width_override {
        (ov_cw + pb_w).max(grid_min_border_w)
    } else {
        match &style.box_model.width {
            SizeValue::Length(lp) => {
                let raw = lp.resolve_px(space.containing_block_width, &eval).max(0.0);
                to_border_box_dim(raw, pb_w, is_bb)
            }
            SizeValue::MinContent => grid_min_border_w,
            SizeValue::MaxContent => {
                if grid.is_fixed_layout {
                    INFINITE_TABLE_MAX_W
                } else {
                    raw_max_cols + total_h_spacing + pb_w
                }
            }
            SizeValue::FitContent(opt_lim) => {
                let lim = opt_lim.as_ref().map_or(avail_border_w, |lp| {
                    to_border_box_dim(
                        lp.resolve_px(space.containing_block_width, &eval).max(0.0),
                        pb_w,
                        is_bb,
                    )
                });
                let pref = if pct_max_cols.is_finite() {
                    pct_max_cols + total_h_spacing + pb_w
                } else {
                    lim
                };
                pref.min(lim).max(grid_min_border_w)
            }
            SizeValue::Stretch => avail_border_w.max(grid_min_border_w),
            SizeValue::CalcSize(cs) => {
                let stf = if pct_max_cols.is_finite() {
                    (pct_max_cols + total_h_spacing + pb_w)
                        .min(avail_border_w)
                        .max(grid_min_border_w)
                } else {
                    avail_border_w.max(grid_min_border_w)
                };
                let val = cs.eval_px(Some(space.containing_block_width), &eval, &|basis| {
                    let b = match basis {
                        rix_css::values_math::CalcSizeBasis::MinContent => grid_min_border_w,
                        rix_css::values_math::CalcSizeBasis::MaxContent => {
                            raw_max_cols + total_h_spacing + pb_w
                        }
                        rix_css::values_math::CalcSizeBasis::Stretch => avail_border_w,
                        _ => stf,
                    };
                    if is_bb {
                        b
                    } else {
                        (b - pb_w).max(0.0)
                    }
                });
                to_border_box_dim(val.max(0.0), pb_w, is_bb)
            }
            SizeValue::Auto | SizeValue::None | SizeValue::Content => {
                let pref = if pct_max_cols.is_finite() {
                    pct_max_cols + total_h_spacing + pb_w
                } else {
                    avail_border_w
                };
                pref.min(avail_border_w).max(grid_min_border_w)
            }
        }
    };

    let target_grid_border_w = desired_border_w
        .min(eff_max_border_w)
        .max(eff_min_border_w)
        .max(grid_min_border_w)
        .max(cap_min_w)
        .max(pb_w);

    let assignable_w = (target_grid_border_w - pb_w - total_h_spacing).max(0.0);
    let mut col_widths = if grid.num_cols == 0 {
        Vec::new()
    } else if grid.is_fixed_layout {
        solve_fixed_column_widths(&col_constraints, assignable_w)
    } else {
        solve_auto_column_widths(&col_constraints, assignable_w)
    };
    let col_collapsed: Vec<bool> = col_constraints.iter().map(|c| c.is_collapsed).collect();
    let row_collapsed: Vec<bool> = grid.rows.iter().map(|r| r.is_collapsed).collect();

    // Uncollapsed column sum & grid inner width (used for cell `%` padding basis & Pass-1 layout).
    let pre_collapse_inner_w = if grid.num_cols == 0 {
        (target_grid_border_w - pb_w).max(0.0)
    } else {
        col_widths.iter().sum::<f32>() + h_spacing * (grid.num_cols.saturating_sub(1) as f32)
    };

    // Pass 1: Layout every cell at its full uncollapsed column-span width to compute row heights
    // and baselines (CSS Tables L3 §4.10: collapsing columns does NOT change table height!).
    let my_oof_start = ctx.pending_oof.len();
    struct CellLayoutPass {
        frag: Fragment,
        oof_start: usize,
        oof_end: usize,
        intrinsic_h: f32,
        cell_pb_w: f32,
        cell_pb_h: f32,
        first_baseline: Option<f32>,
    }
    let mut cell_passes: Vec<CellLayoutPass> = Vec::with_capacity(grid.cells.len());

    let mut row_heights = vec![0.0f32; grid.num_rows];
    let mut row_constrained = vec![false; grid.num_rows];
    let mut row_pct: Vec<Option<f32>> = vec![None; grid.num_rows];
    let mut row_max_ascent = vec![0.0f32; grid.num_rows];
    let mut row_max_descent = vec![0.0f32; grid.num_rows];
    let mut row_has_baseline_cell = vec![false; grid.num_rows];
    let mut row_has_non_empty_cell = vec![false; grid.num_rows];
    let mut row_cell_count = vec![0usize; grid.num_rows];
    for cell in &grid.cells {
        if cell.row_idx < grid.num_rows {
            row_cell_count[cell.row_idx] += 1;
        }
    }

    // Seed row constraints from `<tr>` CSS `height`.
    for (r_idx, rm) in grid.rows.iter().enumerate() {
        let r_style = &ctx.tree.get(rm.row_box_id).style;
        let r_eval = ctx.eval_context_for(r_style, &space);
        let (rf, rp, _) = decompose_size(&r_style.box_model.height, &r_eval);
        if let Some(h) = rf {
            row_heights[r_idx] = row_heights[r_idx].max(h);
            row_constrained[r_idx] = true;
        }
        if let Some(p) = rp {
            row_pct[r_idx] = Some(p.min(100.0));
        }
    }

    for cell in &grid.cells {
        let span_end = (cell.col_idx + cell.colspan).min(grid.num_cols);
        let span_w: f32 = col_widths[cell.col_idx..span_end].iter().sum::<f32>()
            + h_spacing * (cell.colspan.saturating_sub(1) as f32);

        let cell_style = Arc::clone(&ctx.tree.get(cell.box_id).style);
        let cell_eval = ctx.eval_context_for(&cell_style, &space);
        let mut cell_space = space.with_available(span_w, None);
        // `element-sizing.html`: `%` padding on `td` resolves against `tr` width (table inner width).
        cell_space.cb_inline_size = Some(pre_collapse_inner_w);

        let cell_m = BoxMetrics::resolve_with_eval(&cell_style, &cell_space, cell_eval);
        let cell_half_b = grid.collapsed_borders.as_ref().map(|cb| {
            cell_collapsed_half_borders(
                cb,
                cell.row_idx,
                cell.row_idx + cell.rowspan,
                cell.col_idx,
                cell.col_idx + cell.colspan,
            )
        });
        let eff_cb = cell_half_b.unwrap_or(cell_m.border);
        let cell_pb_w = cell_m.padding.horizontal() + eff_cb.horizontal();
        let cell_pb_h = cell_m.padding.vertical() + eff_cb.vertical();

        cell_space.definite_width_override = Some((span_w - cell_pb_w).max(0.0));
        // If collapsed, temporarily adjust content width to match half-border box.
        let cell_oof_start = ctx.pending_oof.len();
        let mut frag = layout_box(ctx, cell.box_id, cell_space);
        let cell_oof_end = ctx.pending_oof.len();

        // Enforce exact border-box width and collapsed half-border height adjustments.
        if let Some(hb) = cell_half_b {
            let dh = hb.vertical() - cell_m.border.vertical();
            frag.border = hb;
            frag.border_rect.size.height = (frag.border_rect.size.height + dh).max(cell_pb_h);
            frag.content_rect.origin.x = hb.left + cell_m.padding.left;
            frag.content_rect.origin.y = hb.top + cell_m.padding.top;
            if dh.abs() > 0.01 {
                let dy = hb.top - cell_m.border.top;
                let dx = hb.left - cell_m.border.left;
                for ch in Arc::make_mut(&mut frag.children) {
                    ch.border_rect.origin.x += dx;
                    ch.border_rect.origin.y += dy;
                }
            }
        }
        frag.border_rect.size.width = span_w;
        frag.content_rect.size.width = (span_w - cell_pb_w).max(0.0);

        let intrinsic_h = frag.border_rect.size.height.round();
        let first_baseline = if cell.is_empty { None } else { frag.baseline };

        if cell.rowspan == 1 {
            let r = cell.row_idx;
            row_heights[r] = row_heights[r].max(intrinsic_h);
            if !cell.is_empty {
                row_has_non_empty_cell[r] = true;
            }
            let (cf, cp, _) = decompose_size(&cell_style.box_model.height, &cell_eval);
            if cf.is_some() {
                row_constrained[r] = true;
            }
            if let Some(p) = cp {
                row_pct[r] = Some(row_pct[r].map_or(p.min(100.0), |cur| cur.max(p.min(100.0))));
            }
            if is_cell_baseline_align(cell_style.box_model.vertical_align) {
                if let Some(bl) = first_baseline {
                    row_has_baseline_cell[r] = true;
                    row_max_ascent[r] = row_max_ascent[r].max(bl);
                    row_max_descent[r] = row_max_descent[r].max((intrinsic_h - bl).max(0.0));
                }
            }
        }

        cell_passes.push(CellLayoutPass {
            frag,
            oof_start: cell_oof_start,
            oof_end: cell_oof_end,
            intrinsic_h,
            cell_pb_w,
            cell_pb_h,
            first_baseline,
        });
    }

    // Account for baseline alignment height increase on rows with `vertical-align: baseline` cells.
    for r in 0..grid.num_rows {
        if row_has_baseline_cell[r] {
            row_heights[r] = row_heights[r].max(row_max_ascent[r] + row_max_descent[r]);
        }
    }

    // Pass 2: Distribute `rowspan > 1` cell heights per CSS Tables L3 / `rowspan-height-redistribution.html`.
    let mut rowspan_cells: Vec<usize> = grid
        .cells
        .iter()
        .enumerate()
        .filter(|(_, c)| c.rowspan > 1)
        .map(|(i, _)| i)
        .collect();
    // Sort: inner spans first (smaller end row, then larger start row), taller cell first on same span.
    rowspan_cells.sort_by(|&a, &b| {
        let ca = &grid.cells[a];
        let cb = &grid.cells[b];
        let end_a = ca.row_idx + ca.rowspan;
        let end_b = cb.row_idx + cb.rowspan;
        if ca.row_idx == cb.row_idx && end_a == end_b {
            cell_passes[b]
                .intrinsic_h
                .partial_cmp(&cell_passes[a].intrinsic_h)
                .unwrap_or(std::cmp::Ordering::Equal)
        } else if ca.row_idx >= cb.row_idx && end_a <= end_b {
            std::cmp::Ordering::Less
        } else if cb.row_idx >= ca.row_idx && end_b <= end_a {
            std::cmp::Ordering::Greater
        } else {
            ca.row_idx.cmp(&cb.row_idx).then_with(|| end_a.cmp(&end_b))
        }
    });

    for cell_i in rowspan_cells {
        let cell = &grid.cells[cell_i];
        let r_start = cell.row_idx;
        let r_end = (r_start + cell.rowspan).min(grid.num_rows);
        let span_r = r_end.saturating_sub(r_start);
        if span_r == 0 {
            continue;
        }
        let inter_v = v_spacing * (span_r.saturating_sub(1) as f32);
        let cur_h: f32 = row_heights[r_start..r_end].iter().sum::<f32>() + inter_v;
        let dh = cell_passes[cell_i].intrinsic_h - cur_h;
        if dh > 0.01 {
            let idxs: Vec<usize> = (r_start..r_end).collect();
            let pref_empty: Vec<bool> = (0..grid.num_rows)
                .map(|r| row_cell_count[r] > usize::from(r == cell.row_idx))
                .collect();
            distribute_height_to_rows(
                &idxs,
                dh,
                &mut row_heights,
                &row_constrained,
                &row_pct,
                None,
                true,
                Some(&pref_empty),
            );
        }
    }

    // Pass 3: Distribute row-group (`thead`/`tbody`/`tfoot`) explicit `height` to its rows
    // (`tbody-height-redistribution.html`) and record section percentage heights for Pass 4.
    let mut empty_sec_heights = vec![0.0f32; grid.sections.len()];
    let mut sec_pct_h: Vec<Option<f32>> = vec![None; grid.sections.len()];
    let mut sec_constrained = row_constrained.clone();
    for (s_idx, sec) in grid.sections.iter().enumerate() {
        let Some(sec_id) = sec.box_id else { continue };
        let sec_style = &ctx.tree.get(sec_id).style;
        let sec_eval = ctx.eval_context_for(sec_style, &space);
        let (sec_fixed_h, sec_p, _) = decompose_size(&sec_style.box_model.height, &sec_eval);
        if let Some(p) = sec_p {
            sec_pct_h[s_idx] = Some(p.min(100.0));
        }
        if let Some(target_h) = sec_fixed_h {
            if sec.row_count == 0 {
                empty_sec_heights[s_idx] = target_h.max(0.0);
            } else {
                let r0 = sec.row_start;
                let r1 = (sec.row_start + sec.row_count).min(grid.num_rows);
                for c_flag in &mut sec_constrained[r0..r1] {
                    *c_flag = true;
                }
                let inter_v = v_spacing * (sec.row_count.saturating_sub(1) as f32);
                let cur_h: f32 = row_heights[r0..r1].iter().sum::<f32>() + inter_v;
                if target_h > cur_h + 0.01 {
                    let idxs: Vec<usize> = (r0..r1).collect();
                    let assignable_sec = (target_h - inter_v).max(0.0);
                    distribute_height_to_rows(
                        &idxs,
                        target_h - cur_h,
                        &mut row_heights,
                        &row_constrained,
                        &row_pct,
                        Some(assignable_sec),
                        false,
                        None,
                    );
                }
            }
        }
    }

    // Pass 4: Distribute Table explicit `height` / `min-height` to row-groups & rows
    // (`table-height-redistribution.html`, `table-rows-with-zero-columns.html`, `extra-height-given-to-all-row-groups-*`).
    let total_v_spacing_uncollapsed = if grid.num_rows > 0 {
        v_spacing * (grid.num_rows + 1) as f32
    } else {
        0.0
    };
    let resolve_table_h = |sz: &SizeValue| -> Option<f32> {
        match sz {
            SizeValue::Length(lp) if !lp.has_percentage() || space.available_height.is_some() => {
                let basis = space
                    .available_height
                    .unwrap_or(space.containing_block_height);
                let raw = lp.resolve_px(basis, &eval).max(0.0);
                Some(to_border_box_dim(raw, pb_h, is_bb))
            }
            SizeValue::Stretch => space
                .available_height
                .map(|h| (h - metrics.margin.vertical()).max(pb_h)),
            _ => None,
        }
    };
    let table_spec_border_h = space
        .definite_height_override
        .map(|ch| ch + pb_h)
        .or_else(|| resolve_table_h(&style.box_model.height));
    let table_min_border_h = resolve_table_h(&style.box_model.min_height).unwrap_or(0.0);
    let desired_grid_border_h = table_spec_border_h
        .map(|h| h.max(table_min_border_h))
        .or_else(|| (table_min_border_h > 0.0).then_some(table_min_border_h));

    let cur_rows_total_h = row_heights.iter().sum::<f32>()
        + empty_sec_heights.iter().sum::<f32>()
        + total_v_spacing_uncollapsed;
    if let Some(target_bh) = desired_grid_border_h {
        let target_content_h = (target_bh - pb_h).max(0.0);
        let mut extra = target_content_h - cur_rows_total_h;
        if extra > 0.01 {
            let assignable_h = (target_content_h - total_v_spacing_uncollapsed).max(0.0);
            // 4a. Satisfy percentage-height row-groups (`<tbody style="height:40%">`) first.
            let mut sec_pct_deficits: Vec<(usize, f32)> = Vec::new();
            let mut total_sec_pct_def = 0.0f32;
            for (s_idx, sec) in grid.sections.iter().enumerate() {
                let Some(p) = sec_pct_h[s_idx] else { continue };
                let target_sh = (p / 100.0) * assignable_h;
                let cur_sh = if sec.row_count == 0 {
                    empty_sec_heights[s_idx]
                } else {
                    let r0 = sec.row_start;
                    let r1 = (sec.row_start + sec.row_count).min(grid.num_rows);
                    row_heights[r0..r1].iter().sum::<f32>()
                        + v_spacing * (sec.row_count.saturating_sub(1) as f32)
                };
                let def = (target_sh - cur_sh).max(0.0);
                if def > 0.01 {
                    sec_pct_deficits.push((s_idx, def));
                    total_sec_pct_def += def;
                }
                if sec.row_count > 0 {
                    let r0 = sec.row_start;
                    let r1 = (sec.row_start + sec.row_count).min(grid.num_rows);
                    for c_flag in &mut sec_constrained[r0..r1] {
                        *c_flag = true;
                    }
                }
            }
            if total_sec_pct_def > 0.01 {
                let take = extra.min(total_sec_pct_def);
                for (s_idx, def) in sec_pct_deficits {
                    let add = take * (def / total_sec_pct_def);
                    let sec = &grid.sections[s_idx];
                    if sec.row_count == 0 {
                        empty_sec_heights[s_idx] += add;
                    } else {
                        let r0 = sec.row_start;
                        let r1 = (sec.row_start + sec.row_count).min(grid.num_rows);
                        let idxs: Vec<usize> = (r0..r1).collect();
                        distribute_height_to_rows(
                            &idxs,
                            add,
                            &mut row_heights,
                            &row_constrained,
                            &row_pct,
                            Some(assignable_h),
                            false,
                            None,
                        );
                    }
                }
                extra -= take;
            }

            if extra > 0.01 {
                if grid.num_rows > 0 {
                    // If any unconstrained non-empty rows exist across sections (e.g. `thead` + `tbody`
                    // both unconstrained & non-empty in `extra-height-given-to-all-row-groups-004.html`),
                    // distribute across all sections; otherwise prioritize `tbody` rows per TablesNG
                    // (`table-height-redistribution.html`).
                    let any_all_uncon_non_empty = (0..grid.num_rows).any(|r| {
                        !sec_constrained[r] && row_pct[r].is_none() && row_heights[r] > 0.01
                    });
                    let tbody_rows: Vec<usize> = grid
                        .sections
                        .iter()
                        .filter(|s| s.section == TableSectionKind::Body && s.row_count > 0)
                        .flat_map(|s| s.row_start..(s.row_start + s.row_count))
                        .collect();
                    let target_rows: Vec<usize> = if any_all_uncon_non_empty {
                        (0..grid.num_rows).collect()
                    } else if !tbody_rows.is_empty() {
                        tbody_rows
                    } else {
                        (0..grid.num_rows).collect()
                    };
                    // Exclude percentage-satisfied sections when other non-pct sections exist.
                    let non_pct_sec_rows: Vec<usize> = grid
                        .sections
                        .iter()
                        .enumerate()
                        .filter(|(si, s)| sec_pct_h[*si].is_none() && s.row_count > 0)
                        .flat_map(|(_, s)| s.row_start..(s.row_start + s.row_count))
                        .filter(|r| target_rows.contains(r))
                        .collect();
                    let final_targets = if !non_pct_sec_rows.is_empty()
                        && non_pct_sec_rows.len() < target_rows.len()
                    {
                        non_pct_sec_rows
                    } else {
                        target_rows
                    };
                    distribute_height_to_rows(
                        &final_targets,
                        extra,
                        &mut row_heights,
                        &sec_constrained,
                        &row_pct,
                        Some(assignable_h),
                        false,
                        None,
                    );
                } else if !grid.sections.is_empty() {
                    // Empty `<tbody>` sections grow to fill specified table height (`table-height-redistribution.html:86-88`).
                    let bodies: Vec<usize> = grid
                        .sections
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| s.section == TableSectionKind::Body)
                        .map(|(i, _)| i)
                        .collect();
                    let targets: Vec<usize> = if !bodies.is_empty() {
                        bodies
                    } else {
                        (0..grid.sections.len()).collect()
                    };
                    let each = extra / targets.len() as f32;
                    for &si in &targets {
                        empty_sec_heights[si] += each;
                    }
                }
            }
        }
    }

    // Pass 5: Apply `visibility: collapse` to columns & rows AFTER heights and widths were established
    // (CSS Tables L3 §4.10: collapsed columns preserve table height; collapsed rows preserve table width!).
    let any_col_collapsed = col_collapsed.iter().any(|&c| c);
    let any_row_collapsed = row_collapsed.iter().any(|&r| r);

    if any_col_collapsed {
        for (c, &is_col) in col_collapsed.iter().enumerate() {
            if is_col {
                col_widths[c] = 0.0;
            }
        }
    }
    if any_row_collapsed {
        for (r, &is_col) in row_collapsed.iter().enumerate() {
            if is_col {
                row_heights[r] = 0.0;
            }
        }
    }

    // Compute final column X positions relative to table grid content box.
    let mut col_x = Vec::with_capacity(grid.num_cols);
    let mut visible_cols = 0usize;
    let mut running_x = 0.0f32;
    if grid.num_cols > 0 {
        for c in 0..grid.num_cols {
            if col_collapsed[c] {
                col_x.push(running_x);
            } else {
                running_x += h_spacing;
                col_x.push(running_x);
                running_x += col_widths[c];
                visible_cols += 1;
            }
        }
        if visible_cols > 0 {
            running_x += h_spacing;
        }
    }

    // Compute final row Y positions relative to table grid content box.
    let mut row_y = Vec::with_capacity(grid.num_rows);
    let mut visible_rows = 0usize;
    let mut running_y = 0.0f32;
    if grid.num_rows > 0 {
        for r in 0..grid.num_rows {
            if row_collapsed[r] {
                row_y.push(running_y);
            } else {
                running_y += v_spacing;
                row_y.push(running_y);
                running_y += row_heights[r];
                visible_rows += 1;
            }
        }
        if visible_rows > 0 {
            running_y += v_spacing;
        }
    } else {
        running_y = empty_sec_heights.iter().sum::<f32>();
    }

    let final_grid_content_w = if grid.num_cols == 0 {
        (target_grid_border_w - pb_w).max(0.0)
    } else if any_col_collapsed {
        running_x.max(0.0)
    } else {
        running_x.max((target_grid_border_w - pb_w).max(0.0))
    };
    let final_grid_border_w = final_grid_content_w + pb_w;

    let final_grid_content_h = if any_row_collapsed {
        running_y.max(0.0)
    } else {
        let min_h = desired_grid_border_h
            .map(|h| (h - pb_h).max(0.0))
            .unwrap_or(0.0);
        running_y.max(min_h)
    };
    let final_grid_border_h = final_grid_content_h + pb_h;

    let grid_inner_w = if visible_cols > 0 {
        (final_grid_content_w - 2.0 * h_spacing).max(0.0)
    } else if grid.num_cols == 0 {
        final_grid_content_w
    } else {
        0.0
    };
    let grid_inner_h = if visible_rows > 0 {
        (final_grid_content_h - 2.0 * v_spacing).max(0.0)
    } else {
        final_grid_content_h
    };

    // Pass 6: Layout top & bottom captions at `final_grid_border_w` (`tentative/caption.html`).
    let layout_caption_list =
        |ctx: &mut LayoutContext<'_>, cap_ids: &[BoxId], start_y: f32| -> (Vec<Fragment>, f32) {
            let mut frags = Vec::with_capacity(cap_ids.len());
            let mut cur_y = start_y;
            for &cid in cap_ids {
                let cap_style = Arc::clone(&ctx.tree.get(cid).style);
                // `%` width on caption resolves against table border-box width; `%` height does NOT resolve.
                let mut cap_space = space.with_available(final_grid_border_w, None);
                if style.text.writing_mode.is_vertical() {
                    cap_space.cb_inline_size = Some(final_grid_border_h);
                }
                let cap_oof_s = ctx.pending_oof.len();
                let mut cfrag = layout_box(ctx, cid, cap_space);
                let cap_oof_e = ctx.pending_oof.len();

                let cap_eval = ctx.eval_context_for(&cap_style, &cap_space);
                let cap_m = BoxMetrics::resolve_with_eval(&cap_style, &cap_space, cap_eval);
                let cw = cfrag.border_rect.size.width;
                let remaining = (final_grid_border_w - cw).max(0.0);
                let ml = match (cap_m.margin_left_auto, cap_m.margin_right_auto) {
                    (true, true) => remaining * 0.5,
                    (true, false) => (remaining - cap_m.margin.right).max(0.0),
                    _ => cap_m.margin.left,
                };
                let mt = cap_m.margin.top;
                let mb = cap_m.margin.bottom;
                // Margins between captions do NOT collapse (`tentative/caption.html:99-109`).
                let cx = ml;
                let cy = cur_y + mt;
                cfrag.border_rect.origin = Point::new(cx, cy);
                for p in &mut ctx.pending_oof[cap_oof_s..cap_oof_e] {
                    p.static_pos.x += cx;
                    p.static_pos.y += cy;
                }
                cur_y = cy + cfrag.border_rect.size.height + mb;
                frags.push(cfrag);
            }
            (frags, cur_y - start_y)
        };

    let (top_cap_frags, top_caps_h) = layout_caption_list(ctx, &grid.top_captions, 0.0);
    let grid_box_origin_y = top_caps_h;
    let content_origin_x = eff_border.left + eff_padding.left;
    let content_origin_y = grid_box_origin_y + eff_border.top + eff_padding.top;

    // Pass 7: Finalize cell fragments (relayout if row height grew for `%`-height children,
    // apply `vertical-align`, suppress `empty-cells: hide`, collapse cells).
    let mut row_cell_frags: Vec<Vec<Fragment>> = (0..grid.num_rows).map(|_| Vec::new()).collect();
    let row_x_offset = if visible_cols > 0 { h_spacing } else { 0.0 };

    for (cell_i, pass) in cell_passes.into_iter().enumerate() {
        let cell = &grid.cells[cell_i];
        let r_start = cell.row_idx;
        let r_end = (r_start + cell.rowspan).min(grid.num_rows);
        let c_start = cell.col_idx;
        let c_end = (c_start + cell.colspan).min(grid.num_cols);

        let vis_c_count = (c_start..c_end).filter(|&c| !col_collapsed[c]).count();
        let vis_r_count = (r_start..r_end).filter(|&r| !row_collapsed[r]).count();

        let cell_w: f32 = if vis_c_count == 0 {
            0.0
        } else {
            (c_start..c_end)
                .filter(|&c| !col_collapsed[c])
                .map(|c| col_widths[c])
                .sum::<f32>()
                + h_spacing * (vis_c_count.saturating_sub(1) as f32)
        };
        let cell_h: f32 = if vis_r_count == 0 {
            0.0
        } else {
            (r_start..r_end)
                .filter(|&r| !row_collapsed[r])
                .map(|r| row_heights[r])
                .sum::<f32>()
                + v_spacing * (vis_r_count.saturating_sub(1) as f32)
        };

        let mut frag = pass.frag;
        let mut oof_s = pass.oof_start;
        let mut oof_e = pass.oof_end;

        if vis_c_count == 0 || vis_r_count == 0 {
            // Fully collapsed cell: zero geometry and drop children so overflow is unaffected
            // (`visibility-collapse-row-005.html`).
            frag.border_rect = Rect::new(0.0, 0.0, 0.0, 0.0);
            frag.content_rect = Rect::ZERO;
            frag.overflow_rect = Rect::ZERO;
            frag.border = EdgeInsets::ZERO;
            frag.padding = EdgeInsets::ZERO;
            frag.children = Arc::new(Vec::new());
            row_cell_frags[r_start].push(frag);
            continue;
        }

        // Relayout cell when its final height or width changed significantly so `%`-height children
        // resolve against the final row height (`percentage-sizing-of-table-cell-children.html`).
        let width_changed = (cell_w - frag.border_rect.size.width).abs() > 0.5;
        let height_changed = (cell_h - pass.intrinsic_h).abs() > 0.5;
        if (width_changed || height_changed) && !ctx.tree.get(cell.box_id).children.is_empty() {
            let final_ch = (cell_h - pass.cell_pb_h).max(0.0);
            let final_cw = (cell_w - pass.cell_pb_w).max(0.0);
            let mut relayout_space = space.with_available(cell_w, Some(final_ch));
            relayout_space.cb_inline_size = Some(grid_inner_w);
            relayout_space.definite_width_override = Some(final_cw);
            relayout_space.definite_height_override = Some(final_ch);
            // Measure natural content height with `height: 100%` resolved against `final_ch`.
            oof_s = ctx.pending_oof.len();
            let mut rfrag = layout_box(ctx, cell.box_id, relayout_space);
            oof_e = ctx.pending_oof.len();
            if collapsed {
                let hb = grid
                    .collapsed_borders
                    .as_ref()
                    .map(|cb| cell_collapsed_half_borders(cb, r_start, r_end, c_start, c_end))
                    .unwrap_or(EdgeInsets::ZERO);
                rfrag.border = hb;
            }
            rfrag.border_rect.size.width = cell_w;
            rfrag.content_rect.size.width = final_cw;
            rfrag.border_rect.size.height = cell_h;
            rfrag.content_rect.size.height = final_ch;
            frag = rfrag;
        }

        // Compute content bottom to apply `vertical-align` accurately (skipping if children already fill cell).
        let children_max_y = frag
            .children
            .iter()
            .filter(|c| !c.is_out_of_flow)
            .map(|c| c.border_rect.max_y())
            .fold(0.0f32, f32::max);
        let content_top = frag.content_rect.origin.y;
        let content_used_h = (children_max_y - content_top).max(0.0);
        let avail_inner_h = (cell_h - pass.cell_pb_h).max(0.0);
        let extra_v = (avail_inner_h - content_used_h).max(0.0);

        let shift_y = match frag.style.box_model.vertical_align {
            VerticalAlignMode::Top => 0.0,
            VerticalAlignMode::Middle => extra_v * 0.5,
            VerticalAlignMode::Bottom => extra_v,
            _ => {
                // CSS 2.1 §17.5.3: every other `vertical-align` value aligns to the row baseline.
                if cell.rowspan == 1 && row_has_baseline_cell[r_start] {
                    pass.first_baseline.map_or(0.0, |bl| {
                        (row_max_ascent[r_start] - bl).max(0.0).min(extra_v)
                    })
                } else {
                    0.0
                }
            }
        };
        if shift_y > 0.01 {
            for ch in Arc::make_mut(&mut frag.children) {
                if !ch.is_out_of_flow {
                    ch.border_rect.origin.y += shift_y;
                }
            }
        }

        frag.border_rect.size.width = cell_w;
        frag.border_rect.size.height = cell_h;
        frag.content_rect.size.width = (cell_w - pass.cell_pb_w).max(0.0);
        frag.content_rect.size.height = avail_inner_h;

        // First visible column in cell's span determines cell X relative to its `<tr>`.
        let first_vis_c = (c_start..c_end)
            .find(|&c| !col_collapsed[c])
            .unwrap_or(c_start);
        let cell_rel_x = (col_x[first_vis_c] - row_x_offset).max(0.0);
        let first_vis_r = (r_start..r_end)
            .find(|&r| !row_collapsed[r])
            .unwrap_or(r_start);
        let cell_rel_y = (row_y[first_vis_r] - row_y[r_start]).max(0.0);
        frag.border_rect.origin = Point::new(cell_rel_x, cell_rel_y);

        // In collapsed border mode, table-level overlay fragments paint the winning edges.
        if collapsed {
            frag.border = EdgeInsets::ZERO;
        } else if cell.is_empty && frag.style.text.empty_cells == EmptyCellsMode::Hide {
            // CSS Tables L3 §4.12: `empty-cells: hide` suppresses border & background on empty cells.
            frag.border = EdgeInsets::ZERO;
            let vis = Arc::make_mut(&mut Arc::make_mut(&mut frag.style).visual);
            vis.background_color = ColorRgba::TRANSPARENT;
            vis.background_layers.clear();
        }

        for p in &mut ctx.pending_oof[oof_s..oof_e] {
            p.static_pos.x += content_origin_x + row_x_offset + cell_rel_x;
            p.static_pos.y += content_origin_y + row_y[r_start] + cell_rel_y;
        }
        frag.compute_overflow_rect();
        row_cell_frags[r_start].push(frag);
    }

    // Pass 8: Assemble Bounding-Box Fragment hierarchy in CSS Tables L3 §4.12 paint order:
    // top captions -> colgroups/cols -> sections (rows -> cells) -> collapsed borders -> bottom captions.
    let mut table_children = Vec::new();
    table_children.extend(top_cap_frags);

    let first_vis_row_y = if visible_rows > 0 { v_spacing } else { 0.0 };

    // Emit `<colgroup>` and `<col>` fragments (with exact CSS Tables L3 §4.11 bounding rects!).
    for cg in &grid.colgroups {
        let (cg_x, cg_w) = resolve_col_span_rect(
            &grid,
            cg.orig_start,
            cg.orig_span,
            &col_x,
            &col_widths,
            &col_collapsed,
        );
        let cg_box = ctx.tree.get(cg.box_id);
        let mut col_children = Vec::with_capacity(cg.cols.len());
        for col in &cg.cols {
            let (cx, cw) = resolve_col_span_rect(
                &grid,
                col.orig_start,
                col.orig_span,
                &col_x,
                &col_widths,
                &col_collapsed,
            );
            let col_box = ctx.tree.get(col.box_id);
            let mut col_style = Arc::clone(&col_box.style);
            if collapsed {
                Arc::make_mut(&mut Arc::make_mut(&mut col_style).box_model).border_width =
                    rix_css::EdgeValues::all(0.0);
            }
            col_children.push(Fragment {
                box_id: col.box_id,
                dom_node: col_box.dom_node,
                style: col_style,
                border_rect: Rect::new(cx - cg_x, 0.0, cw, grid_inner_h),
                padding: EdgeInsets::ZERO,
                border: EdgeInsets::ZERO,
                margin: EdgeInsets::ZERO,
                content_rect: Rect::new(0.0, 0.0, cw, grid_inner_h),
                baseline: None,
                last_baseline: None,
                overflow_rect: Rect::new(0.0, 0.0, cw, grid_inner_h),
                kind: FragmentKind::Box,
                children: Arc::new(Vec::new()),
                is_float: false,
                is_out_of_flow: false,
                collapsed_margin_top: 0.0,
                collapsed_margin_bottom: 0.0,
                resolved_grid_tracks: None,
            });
        }
        table_children.push(Fragment {
            box_id: cg.box_id,
            dom_node: cg_box.dom_node,
            style: Arc::clone(&cg_box.style),
            border_rect: Rect::new(
                content_origin_x + cg_x,
                content_origin_y + first_vis_row_y,
                cg_w,
                grid_inner_h,
            ),
            padding: EdgeInsets::ZERO,
            border: EdgeInsets::ZERO,
            margin: EdgeInsets::ZERO,
            content_rect: Rect::new(0.0, 0.0, cg_w, grid_inner_h),
            baseline: None,
            last_baseline: None,
            overflow_rect: Rect::new(0.0, 0.0, cg_w, grid_inner_h),
            kind: FragmentKind::Box,
            children: Arc::new(col_children),
            is_float: false,
            is_out_of_flow: false,
            collapsed_margin_top: 0.0,
            collapsed_margin_bottom: 0.0,
            resolved_grid_tracks: None,
        });
    }
    for col in &grid.direct_cols {
        let (cx, cw) = resolve_col_span_rect(
            &grid,
            col.orig_start,
            col.orig_span,
            &col_x,
            &col_widths,
            &col_collapsed,
        );
        let col_box = ctx.tree.get(col.box_id);
        table_children.push(Fragment {
            box_id: col.box_id,
            dom_node: col_box.dom_node,
            style: Arc::clone(&col_box.style),
            border_rect: Rect::new(
                content_origin_x + cx,
                content_origin_y + first_vis_row_y,
                cw,
                grid_inner_h,
            ),
            padding: EdgeInsets::ZERO,
            border: EdgeInsets::ZERO,
            margin: EdgeInsets::ZERO,
            content_rect: Rect::new(0.0, 0.0, cw, grid_inner_h),
            baseline: None,
            last_baseline: None,
            overflow_rect: Rect::new(0.0, 0.0, cw, grid_inner_h),
            kind: FragmentKind::Box,
            children: Arc::new(Vec::new()),
            is_float: false,
            is_out_of_flow: false,
            collapsed_margin_top: 0.0,
            collapsed_margin_bottom: 0.0,
            resolved_grid_tracks: None,
        });
    }

    // Emit row-group (`thead` / `tbody` / `tfoot`) & row (`tr`) fragments.
    let mut empty_sec_cursor_y = if visible_rows > 0 { v_spacing } else { 0.0 };
    for (s_idx, sec) in grid.sections.iter().enumerate() {
        let r0 = sec.row_start;
        let r1 = (sec.row_start + sec.row_count).min(grid.num_rows);
        let (sec_y, sec_w, sec_h) = if sec.row_count == 0 {
            let eh = empty_sec_heights[s_idx];
            // Empty `tbody` with 0 height has width 0 (`bounding-box-computation-3.html:45-63`),
            // while an empty `tbody` that absorbed height gets `grid_inner_w` (`table-height-redistribution.html:86`).
            let ew = if eh > 0.0 { grid_inner_w } else { 0.0 };
            let sy = if r0 < row_y.len() {
                row_y[r0]
            } else {
                empty_sec_cursor_y
            };
            empty_sec_cursor_y += eh;
            (sy, ew, eh)
        } else {
            let first_vis = (r0..r1).find(|&r| !row_collapsed[r]);
            let last_vis = (r0..r1).rev().find(|&r| !row_collapsed[r]);
            match (first_vis, last_vis) {
                (Some(fv), Some(lv)) => {
                    let sy = row_y[fv];
                    let sh = (row_y[lv] + row_heights[lv] - sy).max(0.0);
                    empty_sec_cursor_y = sy + sh + v_spacing;
                    (sy, grid_inner_w, sh)
                }
                _ => (row_y[r0], 0.0, 0.0),
            }
        };

        let mut sec_row_frags = Vec::with_capacity(sec.row_count);
        for r_idx in r0..r1 {
            let rm = &grid.rows[r_idx];
            let r_box = ctx.tree.get(rm.row_box_id);
            let rh = row_heights[r_idx];
            let rw = if row_collapsed[r_idx] {
                0.0
            } else {
                grid_inner_w
            };
            let ry_rel = if sec.box_id.is_some() {
                (row_y[r_idx] - sec_y).max(0.0)
            } else {
                content_origin_y + row_y[r_idx]
            };
            let rx_rel = if sec.box_id.is_some() {
                0.0
            } else {
                content_origin_x + row_x_offset
            };
            let row_bl = if row_has_baseline_cell[r_idx] {
                Some(row_max_ascent[r_idx])
            } else if row_has_non_empty_cell[r_idx] {
                Some(rh)
            } else {
                Some(0.0)
            };
            let mut row_frag = Fragment {
                box_id: rm.row_box_id,
                dom_node: r_box.dom_node,
                style: Arc::clone(&r_box.style),
                border_rect: Rect::new(rx_rel, ry_rel, rw, rh),
                padding: EdgeInsets::ZERO,
                border: EdgeInsets::ZERO,
                margin: EdgeInsets::ZERO,
                content_rect: Rect::new(0.0, 0.0, rw, rh),
                baseline: row_bl,
                last_baseline: row_bl,
                overflow_rect: Rect::new(0.0, 0.0, rw, rh),
                kind: FragmentKind::Box,
                children: Arc::new(std::mem::take(&mut row_cell_frags[r_idx])),
                is_float: false,
                is_out_of_flow: false,
                collapsed_margin_top: 0.0,
                collapsed_margin_bottom: 0.0,
                resolved_grid_tracks: None,
            };
            if !row_collapsed[r_idx] {
                row_frag.compute_overflow_rect();
            }
            sec_row_frags.push(row_frag);
        }

        if let Some(sec_id) = sec.box_id {
            let sec_box = ctx.tree.get(sec_id);
            let mut sec_frag = Fragment {
                box_id: sec_id,
                dom_node: sec_box.dom_node,
                style: Arc::clone(&sec_box.style),
                border_rect: Rect::new(
                    content_origin_x + row_x_offset,
                    content_origin_y + sec_y,
                    sec_w,
                    sec_h,
                ),
                padding: EdgeInsets::ZERO,
                border: EdgeInsets::ZERO,
                margin: EdgeInsets::ZERO,
                content_rect: Rect::new(0.0, 0.0, sec_w, sec_h),
                baseline: sec_row_frags.first().and_then(|rf| rf.baseline),
                last_baseline: sec_row_frags.last().and_then(|rf| rf.last_baseline),
                overflow_rect: Rect::new(0.0, 0.0, sec_w, sec_h),
                kind: FragmentKind::Box,
                children: Arc::new(sec_row_frags),
                is_float: false,
                is_out_of_flow: false,
                collapsed_margin_top: 0.0,
                collapsed_margin_bottom: 0.0,
                resolved_grid_tracks: None,
            };
            if !sec.is_collapsed {
                sec_frag.compute_overflow_rect();
            }
            table_children.push(sec_frag);
        } else {
            table_children.extend(sec_row_frags);
        }
    }

    // Emit conflict-resolved collapsed borders on top of cells.
    if let Some(ref cb) = grid.collapsed_borders {
        emit_collapsed_border_fragments(
            cb,
            content_origin_x,
            content_origin_y,
            &col_x,
            &col_widths,
            &col_collapsed,
            &row_y,
            &row_heights,
            &row_collapsed,
            &style,
            &mut table_children,
        );
    }

    // Bottom captions stack directly below the table grid box.
    let (bot_cap_frags, bot_caps_h) = layout_caption_list(
        ctx,
        &grid.bottom_captions,
        grid_box_origin_y + final_grid_border_h,
    );
    table_children.extend(bot_cap_frags);

    let total_border_w = final_grid_border_w;
    let total_border_h = top_caps_h + final_grid_border_h + bot_caps_h;

    crate::position::resolve_oof_for_container(
        ctx,
        my_oof_start,
        &style,
        Rect::new(
            eff_border.left,
            grid_box_origin_y + eff_border.top,
            (final_grid_border_w - eff_border.horizontal()).max(0.0),
            (final_grid_border_h - eff_border.vertical()).max(0.0),
        ),
        false,
        space,
        &mut table_children,
    );

    let (resolved_ml, resolved_mr) = if metrics.margin_left_auto || metrics.margin_right_auto {
        let remaining = (space.available_width - total_border_w).max(0.0);
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
    let resolved_margin = EdgeInsets {
        top: metrics.margin.top,
        right: resolved_mr,
        bottom: metrics.margin.bottom,
        left: resolved_ml,
    };

    // CSS Tables L3 / `baseline-table.html`:
    // - Empty table (0 rows): baseline is bottom of border box.
    // - Non-empty table: baseline is baseline of the first visible row:
    //   - if row has `vertical-align: baseline` cells -> max cell baseline ascent;
    //   - else if row has non-empty cells -> bottom of the row;
    //   - else (empty row) -> top of the row (`0.0`).
    let table_baseline = if grid.num_rows == 0 {
        total_border_h
    } else {
        let first_r = (0..grid.num_rows).find(|&r| !row_collapsed[r]).unwrap_or(0);
        let r_bl = if row_has_baseline_cell[first_r] {
            row_max_ascent[first_r]
        } else if row_has_non_empty_cell[first_r] {
            row_heights[first_r]
        } else {
            0.0
        };
        content_origin_y + row_y[first_r] + r_bl
    };

    let frag_border = if collapsed && grid.num_rows > 0 && grid.num_cols > 0 {
        // Collapsed border segments already paint the exact perimeter.
        EdgeInsets::ZERO
    } else {
        eff_border
    };

    let mut table_frag = Fragment {
        box_id: table_id,
        dom_node: ctx.tree.get(table_id).dom_node,
        style,
        border_rect: Rect::new(0.0, 0.0, total_border_w, total_border_h),
        padding: eff_padding,
        border: frag_border,
        margin: resolved_margin,
        content_rect: Rect::new(
            content_origin_x,
            content_origin_y,
            final_grid_content_w,
            final_grid_content_h,
        ),
        baseline: Some(table_baseline),
        last_baseline: Some(table_baseline),
        overflow_rect: Rect::new(0.0, 0.0, total_border_w, total_border_h),
        kind: FragmentKind::Box,
        children: Arc::new(table_children),
        is_float: ctx.tree.get(table_id).is_float,
        is_out_of_flow: ctx.tree.get(table_id).is_out_of_flow,
        collapsed_margin_top: metrics.margin.top,
        collapsed_margin_bottom: metrics.margin.bottom,
        resolved_grid_tracks: None,
    };
    table_frag.compute_overflow_rect();
    table_frag
}
