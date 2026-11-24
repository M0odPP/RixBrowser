//! CSS Table Formatting Context (`table`, `inline-table`, `table-row-group`,
//! `table-row`, `table-cell`) with anonymous table fixup, `colspan`/`rowspan`
//! occupancy grid, auto & fixed column sizing, `border-collapse` and
//! `border-spacing`. Tables are NEVER collapsed into `taffy::Display::Block`.

use std::sync::Arc;

use rix_core::{Point, Rect};
use rix_css::{BorderCollapseMode, TableLayoutMode, VerticalAlignMode};

use crate::box_tree::{BoxId, BoxKind};
use crate::fragment::{
    layout_box, measure_intrinsic_widths, BoxMetrics, ConstraintSpace, EdgeInsets, Fragment,
    FragmentKind, LayoutContext,
};

struct PlacedCell {
    box_id: BoxId,
    row_idx: usize,
    col_idx: usize,
    colspan: usize,
    rowspan: usize,
    min_w: f32,
    max_w: f32,
    spec_w: Option<f32>,
}

struct TableRowMeta {
    row_box_id: BoxId,
}

struct TableGrid {
    rows: Vec<TableRowMeta>,
    cells: Vec<PlacedCell>,
    num_cols: usize,
    num_rows: usize,
}

fn build_table_grid(
    ctx: &mut LayoutContext<'_>,
    table_id: BoxId,
    space: ConstraintSpace,
) -> TableGrid {
    let mut row_ids = Vec::new();
    collect_table_rows(ctx, table_id, &mut row_ids);

    let mut rows = Vec::with_capacity(row_ids.len());
    let mut cells = Vec::new();
    let mut occupied: Vec<Vec<bool>> = Vec::new();
    let mut num_cols = 0usize;

    for (row_idx, row_box_id) in row_ids.into_iter().enumerate() {
        rows.push(TableRowMeta { row_box_id });
        if occupied.len() <= row_idx {
            occupied.resize_with(row_idx + 1, Vec::new);
        }
        let mut col_cursor = 0usize;
        let cell_ids = ctx.tree.get(row_box_id).children.clone();

        for cell_id in cell_ids {
            let (colspan, rowspan) = match ctx.tree.get(cell_id).kind {
                BoxKind::TableCell { colspan, rowspan } => (colspan.max(1), rowspan.max(1)),
                BoxKind::AnonymousTableCell => (1, 1),
                _ => continue,
            };

            // Advance past slots occupied by previous rows' `rowspan`.
            while occupied[row_idx].get(col_cursor).copied().unwrap_or(false) {
                col_cursor += 1;
            }

            let cell_box = ctx.tree.get(cell_id);
            let cell_metrics = BoxMetrics::resolve(&cell_box.style, &space);
            let pb_w = cell_metrics.padding.horizontal() + cell_metrics.border.horizontal();
            let spec_w = cell_metrics
                .specified_width
                .map(|w| cell_metrics.clamp_content_width(w) + pb_w);
            let (min_w, max_w) = measure_intrinsic_widths(ctx, cell_id, space);

            // Reserve grid occupancy slots for `[row_idx..row_idx+rowspan] x [col_cursor..col_cursor+colspan]`.
            let end_row = row_idx + rowspan;
            if occupied.len() < end_row {
                occupied.resize_with(end_row, Vec::new);
            }
            let end_col = col_cursor + colspan;
            for occ_row in occupied.iter_mut().take(end_row).skip(row_idx) {
                if occ_row.len() < end_col {
                    occ_row.resize(end_col, false);
                }
                for slot in occ_row.iter_mut().take(end_col).skip(col_cursor) {
                    *slot = true;
                }
            }
            num_cols = num_cols.max(end_col);

            cells.push(PlacedCell {
                box_id: cell_id,
                row_idx,
                col_idx: col_cursor,
                colspan,
                rowspan,
                min_w,
                max_w: max_w.max(min_w),
                spec_w,
            });
            col_cursor = end_col;
        }
    }

    let num_rows = rows.len();
    // Clamp rowspan to existing rows.
    for cell in &mut cells {
        if cell.row_idx + cell.rowspan > num_rows {
            cell.rowspan = (num_rows - cell.row_idx).max(1);
        }
    }

    TableGrid {
        rows,
        cells,
        num_cols,
        num_rows,
    }
}

fn collect_table_rows(ctx: &LayoutContext<'_>, container: BoxId, out: &mut Vec<BoxId>) {
    let children = ctx.tree.get(container).children.clone();
    for cid in children {
        match ctx.tree.get(cid).kind {
            BoxKind::TableRow | BoxKind::AnonymousTableRow => out.push(cid),
            BoxKind::TableRowGroup => collect_table_rows(ctx, cid, out),
            _ => {}
        }
    }
}

fn compute_column_bounds(grid: &TableGrid, h_spacing: f32) -> (Vec<f32>, Vec<f32>) {
    let mut col_min = vec![0.0f32; grid.num_cols];
    let mut col_max = vec![0.0f32; grid.num_cols];

    // Pass 1: single-column cells establish baseline column min/max bounds.
    for cell in &grid.cells {
        if cell.colspan == 1 {
            let c = cell.col_idx;
            let eff_min = cell.spec_w.map_or(cell.min_w, |sw| sw.max(cell.min_w));
            let eff_max = cell.spec_w.unwrap_or(cell.max_w).max(eff_min);
            col_min[c] = col_min[c].max(eff_min);
            col_max[c] = col_max[c].max(eff_max);
        }
    }

    // Pass 2: multi-column `colspan > 1` cells distribute any deficit across spanned columns.
    for cell in &grid.cells {
        if cell.colspan > 1 {
            let start = cell.col_idx;
            let end = (start + cell.colspan).min(grid.num_cols);
            let span_count = end.saturating_sub(start);
            if span_count == 0 {
                continue;
            }
            let inter_spacing = h_spacing * (span_count.saturating_sub(1) as f32);
            let cur_min: f32 = col_min[start..end].iter().sum::<f32>() + inter_spacing;
            let cur_max: f32 = col_max[start..end].iter().sum::<f32>() + inter_spacing;

            let target_min = cell.spec_w.map_or(cell.min_w, |sw| sw.max(cell.min_w));
            let target_max = cell.spec_w.unwrap_or(cell.max_w).max(target_min);

            if target_min > cur_min {
                let extra = (target_min - cur_min) / span_count as f32;
                for c in start..end {
                    col_min[c] += extra;
                    col_max[c] = col_max[c].max(col_min[c]);
                }
            }
            if target_max > cur_max {
                let extra = (target_max - cur_max) / span_count as f32;
                for mx in col_max.iter_mut().take(end).skip(start) {
                    *mx += extra;
                }
            }
        }
    }

    (col_min, col_max)
}

/// Computes `(min_content_width, max_content_width)` border-box sizes for a table.
pub fn measure_table_intrinsic_widths(
    ctx: &mut LayoutContext<'_>,
    table_id: BoxId,
    space: ConstraintSpace,
) -> (f32, f32) {
    let style = Arc::clone(&ctx.tree.get(table_id).style);
    let metrics = BoxMetrics::resolve(&style, &space);
    let eval = space.eval_context(style.text.font_size_px);
    let collapsed = style.box_model.border_collapse == BorderCollapseMode::Collapse;
    let h_spacing = if collapsed {
        0.0
    } else {
        style
            .box_model
            .border_spacing_x
            .resolve_px(space.available_width, &eval)
            .max(0.0)
    };
    let grid = build_table_grid(ctx, table_id, space);
    let (col_min, col_max) = compute_column_bounds(&grid, h_spacing);
    let total_spacing = h_spacing * (grid.num_cols + 1) as f32;
    let pb_w = metrics.padding.horizontal() + metrics.border.horizontal();
    let min_w = col_min.iter().sum::<f32>() + total_spacing + pb_w;
    let max_w = col_max.iter().sum::<f32>() + total_spacing + pb_w;
    (min_w, max_w.max(min_w))
}

/// Lays out a CSS Table (`display: table` / `inline-table`).
pub fn layout_table(
    ctx: &mut LayoutContext<'_>,
    table_id: BoxId,
    space: ConstraintSpace,
) -> Fragment {
    let style = Arc::clone(&ctx.tree.get(table_id).style);
    let metrics = BoxMetrics::resolve(&style, &space);
    let eval = space.eval_context(style.text.font_size_px);
    let pb_w = metrics.padding.horizontal() + metrics.border.horizontal();
    let pb_h = metrics.padding.vertical() + metrics.border.vertical();

    let collapsed = style.box_model.border_collapse == BorderCollapseMode::Collapse;
    let h_spacing = if collapsed {
        0.0
    } else {
        style
            .box_model
            .border_spacing_x
            .resolve_px(space.available_width, &eval)
            .max(0.0)
    };
    let v_spacing = if collapsed {
        0.0
    } else {
        style
            .box_model
            .border_spacing_y
            .resolve_px(space.available_width, &eval)
            .max(0.0)
    };

    let grid = build_table_grid(ctx, table_id, space);
    if grid.num_cols == 0 || grid.num_rows == 0 {
        let content_w = metrics.specified_width.unwrap_or(0.0);
        let content_h = metrics.specified_height.unwrap_or(0.0);
        let bw = metrics.clamp_content_width(content_w) + pb_w;
        let bh = metrics.clamp_content_height(content_h) + pb_h;
        return Fragment {
            box_id: table_id,
            dom_node: ctx.tree.get(table_id).dom_node,
            style,
            border_rect: Rect::new(0.0, 0.0, bw, bh),
            padding: metrics.padding,
            border: metrics.border,
            margin: metrics.margin,
            content_rect: Rect::new(
                metrics.border.left + metrics.padding.left,
                metrics.border.top + metrics.padding.top,
                bw - pb_w,
                bh - pb_h,
            ),
            baseline: Some(bh),
            overflow_rect: Rect::new(0.0, 0.0, bw, bh),
            kind: FragmentKind::Box,
            children: Arc::new(Vec::new()),
            is_float: ctx.tree.get(table_id).is_float,
            is_out_of_flow: ctx.tree.get(table_id).is_out_of_flow,
            collapsed_margin_top: metrics.margin.top,
            collapsed_margin_bottom: metrics.margin.bottom,
        };
    }

    let (col_min, col_max) = compute_column_bounds(&grid, h_spacing);
    let total_h_spacing = h_spacing * (grid.num_cols + 1) as f32;
    let sum_min: f32 = col_min.iter().sum();
    let sum_max: f32 = col_max.iter().sum();

    let avail_content_w = (space.available_width - metrics.margin.horizontal() - pb_w).max(0.0);
    let target_content_w = if let Some(spec_w) = metrics.specified_width {
        metrics
            .clamp_content_width(spec_w)
            .max(sum_min + total_h_spacing)
    } else if style.box_model.table_layout == TableLayoutMode::Fixed {
        avail_content_w.max(sum_min + total_h_spacing)
    } else {
        // CSS 2.1 auto table width: shrink-to-fit between sum_min and sum_max, bounded by available width.
        let natural = (sum_max + total_h_spacing)
            .min(avail_content_w)
            .max(sum_min + total_h_spacing);
        metrics.clamp_content_width(natural)
    };

    let avail_cols_w = (target_content_w - total_h_spacing).max(0.0);
    let col_widths = solve_column_widths(
        &grid,
        &col_min,
        &col_max,
        avail_cols_w,
        style.box_model.table_layout,
    );

    // Precompute column X offsets relative to table content box.
    let mut col_x = Vec::with_capacity(grid.num_cols);
    let mut running_x = h_spacing;
    for &w in &col_widths {
        col_x.push(running_x);
        running_x += w + h_spacing;
    }

    // Pass 3: Layout every cell at its exact column-span width to determine intrinsic cell heights.
    let my_oof_start = ctx.pending_oof.len();
    let mut cell_fragments: Vec<(usize, Fragment, usize, usize)> =
        Vec::with_capacity(grid.cells.len());
    let mut row_heights = vec![0.0f32; grid.num_rows];

    for (cell_i, cell) in grid.cells.iter().enumerate() {
        let span_end = (cell.col_idx + cell.colspan).min(grid.num_cols);
        let span_w: f32 = col_widths[cell.col_idx..span_end].iter().sum::<f32>()
            + h_spacing * (cell.colspan.saturating_sub(1) as f32);

        let cell_oof_start = ctx.pending_oof.len();
        let cell_space = space.with_available(span_w, None);
        let mut frag = layout_box(ctx, cell.box_id, cell_space);
        let cell_oof_end = ctx.pending_oof.len();
        // Enforce exact column-span border-box width on the cell fragment.
        let c_pb_w = frag.padding.horizontal() + frag.border.horizontal();
        frag.border_rect.size.width = span_w;
        frag.content_rect.size.width = (span_w - c_pb_w).max(0.0);

        if collapsed {
            // Half-border collapse prevents double-thick internal cell borders.
            frag.border = EdgeInsets {
                top: frag.border.top * 0.5,
                right: frag.border.right * 0.5,
                bottom: frag.border.bottom * 0.5,
                left: frag.border.left * 0.5,
            };
        }

        if cell.rowspan == 1 {
            row_heights[cell.row_idx] = row_heights[cell.row_idx].max(frag.border_rect.size.height);
        }
        cell_fragments.push((cell_i, frag, cell_oof_start, cell_oof_end));
    }

    // Pass 4: Distribute `rowspan > 1` cell heights across their spanned rows.
    for (cell_i, frag, _, _) in &cell_fragments {
        let cell = &grid.cells[*cell_i];
        if cell.rowspan > 1 {
            let end_r = (cell.row_idx + cell.rowspan).min(grid.num_rows);
            let span_r = end_r.saturating_sub(cell.row_idx);
            if span_r == 0 {
                continue;
            }
            let current_h: f32 = row_heights[cell.row_idx..end_r].iter().sum::<f32>()
                + v_spacing * (span_r.saturating_sub(1) as f32);
            if frag.border_rect.size.height > current_h {
                let extra = (frag.border_rect.size.height - current_h) / span_r as f32;
                for rh in row_heights.iter_mut().take(end_r).skip(cell.row_idx) {
                    *rh += extra;
                }
            }
        }
    }

    // Precompute row Y offsets relative to table content box.
    let mut row_y = Vec::with_capacity(grid.num_rows);
    let mut running_y = v_spacing;
    for &rh in &row_heights {
        row_y.push(running_y);
        running_y += rh + v_spacing;
    }

    let content_origin_x = metrics.border.left + metrics.padding.left;
    let content_origin_y = metrics.border.top + metrics.padding.top;
    let final_content_w = running_x.max(target_content_w);
    let final_content_h =
        metrics.clamp_content_height(metrics.specified_height.unwrap_or(running_y));

    // Group positioned cell fragments into row fragments for clean paint & DOM correlation.
    let mut row_children: Vec<Vec<Fragment>> = (0..grid.num_rows).map(|_| Vec::new()).collect();
    for (cell_i, mut frag, oof_s, oof_e) in cell_fragments {
        let cell = &grid.cells[cell_i];
        let end_r = (cell.row_idx + cell.rowspan).min(grid.num_rows);
        let span_r = end_r.saturating_sub(cell.row_idx);
        let cell_h: f32 = row_heights[cell.row_idx..end_r].iter().sum::<f32>()
            + v_spacing * (span_r.saturating_sub(1) as f32);

        let intrinsic_h = frag.border_rect.size.height;
        let extra_v = (cell_h - intrinsic_h).max(0.0);
        if extra_v > 0.01 {
            let shift_y = match frag.style.box_model.vertical_align {
                VerticalAlignMode::Top | VerticalAlignMode::TextTop => 0.0,
                VerticalAlignMode::Bottom | VerticalAlignMode::TextBottom => extra_v,
                _ => extra_v * 0.5, // CSS default vertical-align for table cells is middle
            };
            if shift_y > 0.0 {
                for ch in Arc::make_mut(&mut frag.children) {
                    ch.border_rect.origin.y += shift_y;
                }
            }
        }
        frag.border_rect.size.height = cell_h;
        frag.border_rect.origin = Point::new(col_x[cell.col_idx], 0.0);
        for p in &mut ctx.pending_oof[oof_s..oof_e] {
            p.static_pos.x += content_origin_x + col_x[cell.col_idx];
            p.static_pos.y += content_origin_y + row_y[cell.row_idx];
        }
        frag.compute_overflow_rect();
        row_children[cell.row_idx].push(frag);
    }

    let mut placed_rows = Vec::with_capacity(grid.num_rows);
    for (r_idx, meta) in grid.rows.iter().enumerate() {
        let r_box = ctx.tree.get(meta.row_box_id);
        let mut row_frag = Fragment {
            box_id: meta.row_box_id,
            dom_node: r_box.dom_node,
            style: Arc::clone(&r_box.style),
            border_rect: Rect::new(
                content_origin_x,
                content_origin_y + row_y[r_idx],
                final_content_w,
                row_heights[r_idx],
            ),
            padding: EdgeInsets::ZERO,
            border: EdgeInsets::ZERO,
            margin: EdgeInsets::ZERO,
            content_rect: Rect::new(0.0, 0.0, final_content_w, row_heights[r_idx]),
            baseline: Some(row_heights[r_idx]),
            overflow_rect: Rect::new(0.0, 0.0, final_content_w, row_heights[r_idx]),
            kind: FragmentKind::Box,
            children: Arc::new(std::mem::take(&mut row_children[r_idx])),
            is_float: false,
            is_out_of_flow: false,
            collapsed_margin_top: 0.0,
            collapsed_margin_bottom: 0.0,
        };
        row_frag.compute_overflow_rect();
        placed_rows.push(row_frag);
    }

    let border_w = final_content_w + pb_w;
    let border_h = final_content_h + pb_h;

    crate::position::resolve_oof_for_container(
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
        &mut placed_rows,
    );

    let (resolved_ml, resolved_mr) = if metrics.margin_left_auto || metrics.margin_right_auto {
        let remaining = (space.available_width - border_w).max(0.0);
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

    let mut table_frag = Fragment {
        box_id: table_id,
        dom_node: ctx.tree.get(table_id).dom_node,
        style,
        border_rect: Rect::new(0.0, 0.0, border_w, border_h),
        padding: metrics.padding,
        border: metrics.border,
        margin: resolved_margin,
        content_rect: Rect::new(
            content_origin_x,
            content_origin_y,
            final_content_w,
            final_content_h,
        ),
        baseline: Some(border_h),
        overflow_rect: Rect::new(0.0, 0.0, border_w, border_h),
        kind: FragmentKind::Box,
        children: Arc::new(placed_rows),
        is_float: ctx.tree.get(table_id).is_float,
        is_out_of_flow: ctx.tree.get(table_id).is_out_of_flow,
        collapsed_margin_top: metrics.margin.top,
        collapsed_margin_bottom: metrics.margin.bottom,
    };
    table_frag.compute_overflow_rect();
    table_frag
}

fn solve_column_widths(
    grid: &TableGrid,
    col_min: &[f32],
    col_max: &[f32],
    avail_cols_w: f32,
    layout_mode: TableLayoutMode,
) -> Vec<f32> {
    let n = grid.num_cols;
    if n == 0 {
        return Vec::new();
    }

    if layout_mode == TableLayoutMode::Fixed {
        let mut widths = vec![None; n];
        // First row cells with explicit width fix their columns; remaining columns share evenly.
        for cell in &grid.cells {
            if cell.row_idx == 0 && cell.colspan == 1 {
                if let Some(sw) = cell.spec_w {
                    widths[cell.col_idx] = Some(sw);
                }
            }
        }
        let assigned_sum: f32 = widths.iter().flatten().sum();
        let unassigned = widths.iter().filter(|w| w.is_none()).count();
        let per_unassigned = if unassigned > 0 {
            ((avail_cols_w - assigned_sum).max(0.0)) / unassigned as f32
        } else {
            0.0
        };
        return widths
            .into_iter()
            .enumerate()
            .map(|(i, opt)| opt.unwrap_or_else(|| per_unassigned.max(col_min[i])))
            .collect();
    }

    let sum_min: f32 = col_min.iter().sum();
    let sum_max: f32 = col_max.iter().sum();

    if avail_cols_w <= sum_min + 0.01 {
        return col_min.to_vec();
    }
    if avail_cols_w >= sum_max - 0.01 {
        let extra = (avail_cols_w - sum_max).max(0.0);
        if extra <= 0.01 {
            return col_max.to_vec();
        }
        let weight_sum = sum_max.max(1.0);
        return col_max
            .iter()
            .map(|&mx| mx + extra * (mx / weight_sum))
            .collect();
    }

    // Interpolate smoothly between `col_min` and `col_max` per CSS 2.1 §17.5.2.2.
    let diff = (sum_max - sum_min).max(1.0);
    let ratio = ((avail_cols_w - sum_min) / diff).clamp(0.0, 1.0);
    col_min
        .iter()
        .zip(col_max.iter())
        .map(|(&mn, &mx)| mn + (mx - mn) * ratio)
        .collect()
}
