//! W3C CSS Tables Level 3 grid construction, collapsed border resolution,
//! column width constraint solvers, and row height distribution algorithms.

use std::sync::Arc;

use rix_css::{
    BorderCollapseMode, BoxSizingMode, CaptionSideMode, ColorRgba, ComputedStyle, EvalContext,
    LengthPercentage, LineStyle, SizeValue, TableLayoutMode, VerticalAlignMode, VisibilityMode,
};

use crate::box_tree::{BoxId, BoxKind, TableSectionKind};
use crate::fragment::{
    measure_content_intrinsic_widths, ConstraintSpace, EdgeInsets, LayoutContext,
};

pub(crate) const INFINITE_TABLE_MAX_W: f32 = 10_000.0;

#[derive(Debug, Clone)]
pub(crate) struct ColGroupMeta {
    pub(crate) box_id: BoxId,
    pub(crate) orig_start: usize,
    pub(crate) orig_span: usize,
    pub(crate) cols: Vec<ColMeta>,
}

#[derive(Debug, Clone)]
pub(crate) struct ColMeta {
    pub(crate) box_id: BoxId,
    pub(crate) orig_start: usize,
    pub(crate) orig_span: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ColSlotDef {
    pub(crate) width: SizeValue,
    pub(crate) is_collapsed: bool,
    pub(crate) is_single_explicit_col: bool,
    pub(crate) style: Arc<ComputedStyle>,
    pub(crate) group_style: Option<Arc<ComputedStyle>>,
}

#[derive(Debug, Clone)]
pub(crate) struct SectionMeta {
    pub(crate) box_id: Option<BoxId>,
    pub(crate) section: TableSectionKind,
    pub(crate) row_start: usize,
    pub(crate) row_count: usize,
    pub(crate) is_collapsed: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct TableRowMeta {
    pub(crate) row_box_id: BoxId,
    pub(crate) is_collapsed: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct PlacedCell {
    pub(crate) box_id: BoxId,
    pub(crate) row_idx: usize,
    /// Effective column index after track merging.
    pub(crate) col_idx: usize,
    /// Effective column span after track merging.
    pub(crate) colspan: usize,
    /// Original HTML/CSS `colspan` before track merging.
    pub(crate) orig_colspan: usize,
    /// Clamped row span within its section.
    pub(crate) rowspan: usize,
    /// Intrinsic min/max border-box widths (with fixed padding+border, % padding = 0).
    pub(crate) min_w: f32,
    pub(crate) max_w: f32,
    /// Fixed border-box width if CSS `width` is a fixed length (after `min-width`/`max-width`).
    pub(crate) fixed_w: Option<f32>,
    /// Percentage width (`0.0..=100.0`) if CSS `width` has a percentage component.
    pub(crate) pct_w: Option<f32>,
    /// Additional px term when CSS `width` is `calc(P% + Lpx)`.
    pub(crate) calc_px_w: f32,
    /// Fixed padding + border horizontal size (used for fixed-layout content-box `%` cells).
    pub(crate) pb_w: f32,
    pub(crate) is_border_box: bool,
    pub(crate) is_empty: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ColConstraint {
    pub(crate) min_w: f32,
    pub(crate) max_w: f32,
    pub(crate) pct: Option<f32>,
    pub(crate) calc_px: f32,
    pub(crate) is_constrained: bool,
    pub(crate) is_collapsed: bool,
    pub(crate) is_empty: bool,
    pub(crate) fixed_px: Option<f32>,
    pub(crate) fixed_pct: Option<f32>,
    pub(crate) fixed_pct_pb: f32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CollapsedEdge {
    pub(crate) width: f32,
    pub(crate) style: LineStyle,
    pub(crate) color: ColorRgba,
    pub(crate) prio: u8,
}

impl Default for CollapsedEdge {
    fn default() -> Self {
        Self {
            width: 0.0,
            style: LineStyle::None,
            color: ColorRgba::TRANSPARENT,
            prio: 0,
        }
    }
}

pub(crate) struct CollapsedBorderGrid {
    /// Horizontal edges: `[num_rows + 1][num_cols]`.
    pub(crate) h_edges: Vec<Vec<CollapsedEdge>>,
    /// Vertical edges: `[num_rows][num_cols + 1]`.
    pub(crate) v_edges: Vec<Vec<CollapsedEdge>>,
    /// Half-widths of the outer perimeter edges of the table grid.
    pub(crate) outer_half: EdgeInsets,
}

pub(crate) struct TableGrid {
    pub(crate) top_captions: Vec<BoxId>,
    pub(crate) bottom_captions: Vec<BoxId>,
    pub(crate) colgroups: Vec<ColGroupMeta>,
    pub(crate) direct_cols: Vec<ColMeta>,
    pub(crate) sections: Vec<SectionMeta>,
    pub(crate) rows: Vec<TableRowMeta>,
    pub(crate) cells: Vec<PlacedCell>,
    pub(crate) eff_col_defs: Vec<Option<ColSlotDef>>,
    /// Maps each original grid column `0..raw_cols` to its kept column index (or `None` if dropped trailing).
    pub(crate) orig_to_eff: Vec<Option<usize>>,
    /// For each kept column, its first original column index.
    pub(crate) eff_first_orig: Vec<usize>,
    /// For each kept column, its exclusive end original column index.
    pub(crate) eff_end_orig: Vec<usize>,
    pub(crate) num_cols: usize,
    pub(crate) num_rows: usize,
    pub(crate) is_fixed_layout: bool,
    pub(crate) collapsed_borders: Option<CollapsedBorderGrid>,
}

#[inline]
fn line_style_priority(st: LineStyle) -> u8 {
    match st {
        LineStyle::Hidden => 255,
        LineStyle::Double => 8,
        LineStyle::Solid => 7,
        LineStyle::Dashed => 6,
        LineStyle::Dotted => 5,
        LineStyle::Ridge => 4,
        LineStyle::Outset => 3,
        LineStyle::Groove => 2,
        LineStyle::Inset => 1,
        LineStyle::None | LineStyle::Auto => 0,
    }
}

fn merge_collapsed_edge(dst: &mut CollapsedEdge, cand: CollapsedEdge) {
    if dst.style == LineStyle::Hidden {
        return;
    }
    if cand.style == LineStyle::Hidden {
        *dst = CollapsedEdge {
            width: 0.0,
            style: LineStyle::Hidden,
            color: ColorRgba::TRANSPARENT,
            prio: 255,
        };
        return;
    }
    if cand.style == LineStyle::None || cand.width <= 0.0 {
        return;
    }
    if dst.style == LineStyle::None || dst.width <= 0.0 {
        *dst = cand;
        return;
    }
    if cand.width > dst.width + 0.01 {
        *dst = cand;
        return;
    }
    if (cand.width - dst.width).abs() <= 0.01 {
        let cp = line_style_priority(cand.style);
        let dp = line_style_priority(dst.style);
        if cp > dp || (cp == dp && cand.prio > dst.prio) {
            *dst = cand;
        }
    }
}

fn edge_from_style(style: &ComputedStyle, side: usize, prio: u8) -> CollapsedEdge {
    let (w, ls, col) = match side {
        0 => (
            style.box_model.border_width.top,
            style.visual.border_style.top,
            style.visual.border_color.top,
        ),
        1 => (
            style.box_model.border_width.right,
            style.visual.border_style.right,
            style.visual.border_color.right,
        ),
        2 => (
            style.box_model.border_width.bottom,
            style.visual.border_style.bottom,
            style.visual.border_color.bottom,
        ),
        _ => (
            style.box_model.border_width.left,
            style.visual.border_style.left,
            style.visual.border_color.left,
        ),
    };
    let eff_w = if matches!(ls, LineStyle::None | LineStyle::Hidden) {
        0.0
    } else {
        w.max(0.0)
    };
    CollapsedEdge {
        width: eff_w,
        style: ls,
        color: col.resolve(style.text.color, style.color_scheme_dark),
        prio,
    }
}

#[derive(Debug, Clone, Copy)]
enum TableDim {
    Auto,
    Fixed(f32),
    Percent(f32),
}

/// Decomposes a CSS `LengthPercentage` for table column/cell/row sizing.
///
/// Note: `LengthPercentage::Percent(p)` is stored in `0.0..=1.0`, while table solvers operate
/// in `0.0..=100.0`. Per CSS Tables L3 / CSSWG Issue #94, mixed percentage+length `calc()`
/// expressions (`calc(20% + 80px)`) are treated as `auto`, whereas pure-percentage `calc()`
/// (`calc(50% + 0px)`) resolves as a percentage and pure-length `calc()` resolves as fixed px.
fn decompose_lp(lp: &LengthPercentage, eval: &EvalContext) -> TableDim {
    match lp {
        LengthPercentage::Zero => TableDim::Fixed(0.0),
        LengthPercentage::Percent(p) => TableDim::Percent((p * 100.0).max(0.0)),
        LengthPercentage::Calc(_) => {
            let at0 = lp.resolve_px(0.0, eval);
            let at100 = lp.resolve_px(100.0, eval);
            let pct = at100 - at0;
            if pct.abs() > 1e-4 && at0.abs() > 1e-4 {
                TableDim::Auto
            } else if pct > 1e-4 {
                TableDim::Percent(pct.max(0.0))
            } else {
                TableDim::Fixed(at0.max(0.0))
            }
        }
        _ => TableDim::Fixed(lp.resolve_px(0.0, eval).max(0.0)),
    }
}

/// Decomposes a CSS `SizeValue` into `(fixed_px, pct_0_to_100, calc_px)`.
pub(crate) fn decompose_size(
    sz: &SizeValue,
    eval: &EvalContext,
) -> (Option<f32>, Option<f32>, f32) {
    match sz {
        SizeValue::Length(lp) => match decompose_lp(lp, eval) {
            TableDim::Auto => (None, None, 0.0),
            TableDim::Fixed(px) => (Some(px.max(0.0)), None, 0.0),
            TableDim::Percent(p) => (None, Some(p), 0.0),
        },
        _ => (None, None, 0.0),
    }
}

/// CSS 2.1 §17.5.3 / CSS Tables L3: every `vertical-align` value on a table cell other than
/// `top`, `middle`, or `bottom` maps to `baseline`.
#[inline]
pub(crate) fn is_cell_baseline_align(va: VerticalAlignMode) -> bool {
    !matches!(
        va,
        VerticalAlignMode::Top | VerticalAlignMode::Middle | VerticalAlignMode::Bottom
    )
}

#[inline]
pub(crate) fn to_border_box_dim(val: f32, pb: f32, is_border_box: bool) -> f32 {
    if is_border_box {
        val.max(pb)
    } else {
        (val + pb).max(pb)
    }
}

/// Returns fixed horizontal `padding + border` (`%` padding treated as 0 during intrinsic pass).
fn cell_fixed_pb_w(
    style: &ComputedStyle,
    eval: &EvalContext,
    collapsed_half: Option<(f32, f32)>,
) -> f32 {
    let pl = if style.box_model.padding.left.has_percentage() {
        0.0
    } else {
        style.box_model.padding.left.resolve_px(0.0, eval).max(0.0)
    };
    let pr = if style.box_model.padding.right.has_percentage() {
        0.0
    } else {
        style.box_model.padding.right.resolve_px(0.0, eval).max(0.0)
    };
    let (bl, br) = if let Some((hl, hr)) = collapsed_half {
        (hl, hr)
    } else {
        let bl = if matches!(
            style.visual.border_style.left,
            LineStyle::None | LineStyle::Hidden
        ) {
            0.0
        } else {
            style.box_model.border_width.left.max(0.0)
        };
        let br = if matches!(
            style.visual.border_style.right,
            LineStyle::None | LineStyle::Hidden
        ) {
            0.0
        } else {
            style.box_model.border_width.right.max(0.0)
        };
        (bl, br)
    };
    pl + pr + bl + br
}

fn cell_is_visually_empty(ctx: &LayoutContext<'_>, cell_id: BoxId) -> bool {
    let b = ctx.tree.get(cell_id);
    for &cid in &b.children {
        let ch = ctx.tree.get(cid);
        if ch.is_out_of_flow {
            continue;
        }
        match &ch.kind {
            BoxKind::Text(t)
                if t.bytes()
                    .all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0C)) => {}
            _ => return false,
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn build_collapsed_borders(
    ctx: &LayoutContext<'_>,
    table_style: &ComputedStyle,
    eff_col_defs: &[Option<ColSlotDef>],
    sections: &[SectionMeta],
    rows: &[TableRowMeta],
    cells: &[PlacedCell],
    num_rows: usize,
    num_cols: usize,
) -> CollapsedBorderGrid {
    if num_rows == 0 || num_cols == 0 {
        return CollapsedBorderGrid {
            h_edges: Vec::new(),
            v_edges: Vec::new(),
            outer_half: EdgeInsets::ZERO,
        };
    }

    let mut h_edges = vec![vec![CollapsedEdge::default(); num_cols]; num_rows + 1];
    let mut v_edges = vec![vec![CollapsedEdge::default(); num_cols + 1]; num_rows];

    // 1. Table outer edges (prio 1).
    let tbl_t = edge_from_style(table_style, 0, 1);
    let tbl_r = edge_from_style(table_style, 1, 1);
    let tbl_b = edge_from_style(table_style, 2, 1);
    let tbl_l = edge_from_style(table_style, 3, 1);
    for c in 0..num_cols {
        merge_collapsed_edge(&mut h_edges[0][c], tbl_t);
        merge_collapsed_edge(&mut h_edges[num_rows][c], tbl_b);
    }
    for v_row in v_edges.iter_mut().take(num_rows) {
        merge_collapsed_edge(&mut v_row[0], tbl_l);
        merge_collapsed_edge(&mut v_row[num_cols], tbl_r);
    }

    // 2. Column-group (prio 2) and Column (prio 3) edges.
    for (c, def_opt) in eff_col_defs.iter().enumerate().take(num_cols) {
        if let Some(def) = def_opt {
            if let Some(ref gs) = def.group_style {
                let gl = edge_from_style(gs, 3, 2);
                let gr = edge_from_style(gs, 1, 2);
                let gt = edge_from_style(gs, 0, 2);
                let gb = edge_from_style(gs, 2, 2);
                merge_collapsed_edge(&mut h_edges[0][c], gt);
                merge_collapsed_edge(&mut h_edges[num_rows][c], gb);
                for v_row in v_edges.iter_mut().take(num_rows) {
                    merge_collapsed_edge(&mut v_row[c], gl);
                    merge_collapsed_edge(&mut v_row[c + 1], gr);
                }
            }
            let cl = edge_from_style(&def.style, 3, 3);
            let cr = edge_from_style(&def.style, 1, 3);
            let ct = edge_from_style(&def.style, 0, 3);
            let cb = edge_from_style(&def.style, 2, 3);
            merge_collapsed_edge(&mut h_edges[0][c], ct);
            merge_collapsed_edge(&mut h_edges[num_rows][c], cb);
            for v_row in v_edges.iter_mut().take(num_rows) {
                merge_collapsed_edge(&mut v_row[c], cl);
                merge_collapsed_edge(&mut v_row[c + 1], cr);
            }
        }
    }

    // 3. Row-group edges (prio 4).
    for sec in sections {
        if sec.row_count == 0 {
            continue;
        }
        let Some(sec_id) = sec.box_id else { continue };
        let sec_style = &ctx.tree.get(sec_id).style;
        let st = edge_from_style(sec_style, 0, 4);
        let sr = edge_from_style(sec_style, 1, 4);
        let sb = edge_from_style(sec_style, 2, 4);
        let sl = edge_from_style(sec_style, 3, 4);
        let r0 = sec.row_start;
        let r1 = (sec.row_start + sec.row_count).min(num_rows);
        for c in 0..num_cols {
            merge_collapsed_edge(&mut h_edges[r0][c], st);
            merge_collapsed_edge(&mut h_edges[r1][c], sb);
        }
        for v_row in v_edges.iter_mut().take(r1).skip(r0) {
            merge_collapsed_edge(&mut v_row[0], sl);
            merge_collapsed_edge(&mut v_row[num_cols], sr);
        }
    }

    // 4. Row edges (prio 5).
    for (r, rm) in rows.iter().enumerate().take(num_rows) {
        let row_style = &ctx.tree.get(rm.row_box_id).style;
        let rt = edge_from_style(row_style, 0, 5);
        let rr = edge_from_style(row_style, 1, 5);
        let rb = edge_from_style(row_style, 2, 5);
        let rl = edge_from_style(row_style, 3, 5);
        for c in 0..num_cols {
            merge_collapsed_edge(&mut h_edges[r][c], rt);
            merge_collapsed_edge(&mut h_edges[r + 1][c], rb);
        }
        merge_collapsed_edge(&mut v_edges[r][0], rl);
        merge_collapsed_edge(&mut v_edges[r][num_cols], rr);
    }

    // 5. Cell outer edges (prio 6).
    for cell in cells {
        let cell_style = &ctx.tree.get(cell.box_id).style;
        let ct = edge_from_style(cell_style, 0, 6);
        let cr = edge_from_style(cell_style, 1, 6);
        let cb = edge_from_style(cell_style, 2, 6);
        let cl = edge_from_style(cell_style, 3, 6);
        let r0 = cell.row_idx;
        let r1 = (cell.row_idx + cell.rowspan).min(num_rows);
        let c0 = cell.col_idx;
        let c1 = (cell.col_idx + cell.colspan).min(num_cols);
        for c in c0..c1 {
            merge_collapsed_edge(&mut h_edges[r0][c], ct);
            merge_collapsed_edge(&mut h_edges[r1][c], cb);
        }
        for v_row in v_edges.iter_mut().take(r1).skip(r0) {
            merge_collapsed_edge(&mut v_row[c0], cl);
            merge_collapsed_edge(&mut v_row[c1], cr);
        }
    }

    let top_max = h_edges[0].iter().map(|e| e.width).fold(0.0f32, f32::max);
    let bot_max = h_edges[num_rows]
        .iter()
        .map(|e| e.width)
        .fold(0.0f32, f32::max);
    let left_max = v_edges
        .iter()
        .map(|row| row[0].width)
        .fold(0.0f32, f32::max);
    let right_max = v_edges
        .iter()
        .map(|row| row[num_cols].width)
        .fold(0.0f32, f32::max);

    CollapsedBorderGrid {
        h_edges,
        v_edges,
        outer_half: EdgeInsets {
            top: top_max * 0.5,
            right: right_max * 0.5,
            bottom: bot_max * 0.5,
            left: left_max * 0.5,
        },
    }
}

pub(crate) fn cell_collapsed_half_borders(
    cb: &CollapsedBorderGrid,
    r0: usize,
    r1: usize,
    c0: usize,
    c1: usize,
) -> EdgeInsets {
    let mut top = 0.0f32;
    let mut bottom = 0.0f32;
    let mut left = 0.0f32;
    let mut right = 0.0f32;
    if let Some(h_row) = cb.h_edges.get(r0) {
        for edge in h_row.iter().take(c1.min(h_row.len())).skip(c0) {
            top = top.max(edge.width * 0.5);
        }
    }
    if let Some(h_row) = cb.h_edges.get(r1) {
        for edge in h_row.iter().take(c1.min(h_row.len())).skip(c0) {
            bottom = bottom.max(edge.width * 0.5);
        }
    }
    for v_row in cb.v_edges.iter().take(r1.min(cb.v_edges.len())).skip(r0) {
        if let Some(e) = v_row.get(c0) {
            left = left.max(e.width * 0.5);
        }
        if let Some(e) = v_row.get(c1) {
            right = right.max(e.width * 0.5);
        }
    }
    EdgeInsets {
        top,
        right,
        bottom,
        left,
    }
}

pub(crate) fn build_table_grid(
    ctx: &mut LayoutContext<'_>,
    table_id: BoxId,
    space: ConstraintSpace,
) -> TableGrid {
    let table_style = Arc::clone(&ctx.tree.get(table_id).style);
    let collapsed = table_style.text.border_collapse == BorderCollapseMode::Collapse;
    let is_fixed_layout = table_style.box_model.table_layout == TableLayoutMode::Fixed
        && !matches!(
            table_style.box_model.width,
            SizeValue::Auto | SizeValue::MaxContent
        );

    let children = ctx.tree.get(table_id).children.clone();
    let mut top_captions = Vec::new();
    let mut bottom_captions = Vec::new();
    let mut colgroups = Vec::new();
    let mut direct_cols = Vec::new();
    let mut explicit_col_defs: Vec<ColSlotDef> = Vec::new();

    // Collect sections in CSS Tables L3 §3.1 visual order:
    // first Header -> all remaining sections & direct-row groups in DOM order -> first Footer.
    struct RawSection {
        box_id: Option<BoxId>,
        section: TableSectionKind,
        row_ids: Vec<BoxId>,
        is_collapsed: bool,
    }
    let mut first_header: Option<RawSection> = None;
    let mut first_footer: Option<RawSection> = None;
    let mut body_sections: Vec<RawSection> = Vec::new();
    let mut pending_direct_rows: Vec<BoxId> = Vec::new();

    let flush_direct_rows = |pending: &mut Vec<BoxId>, bodies: &mut Vec<RawSection>| {
        if !pending.is_empty() {
            bodies.push(RawSection {
                box_id: None,
                section: TableSectionKind::Body,
                row_ids: std::mem::take(pending),
                is_collapsed: false,
            });
        }
    };

    for cid in children {
        let cbox = ctx.tree.get(cid);
        if cbox.is_out_of_flow {
            continue;
        }
        match cbox.kind {
            BoxKind::TableCaption => match cbox.style.text.caption_side {
                CaptionSideMode::Bottom
                | CaptionSideMode::BlockEnd
                | CaptionSideMode::InlineEnd => bottom_captions.push(cid),
                _ => top_captions.push(cid),
            },
            BoxKind::TableColumnGroup { span } => {
                let cg_style = Arc::clone(&cbox.style);
                let cg_collapsed = cg_style.visibility == VisibilityMode::Collapse;
                let cg_children = cbox.children.clone();
                let orig_start = explicit_col_defs.len();
                let mut cols_meta = Vec::new();
                for col_id in cg_children {
                    if let BoxKind::TableColumn { span: cspan } = ctx.tree.get(col_id).kind {
                        let cspan = cspan.max(1);
                        let col_style = Arc::clone(&ctx.tree.get(col_id).style);
                        let col_start = explicit_col_defs.len();
                        let eff_w = if !matches!(col_style.box_model.width, SizeValue::Auto) {
                            col_style.box_model.width.clone()
                        } else {
                            cg_style.box_model.width.clone()
                        };
                        let col_collapsed =
                            cg_collapsed || col_style.visibility == VisibilityMode::Collapse;
                        for _ in 0..cspan {
                            explicit_col_defs.push(ColSlotDef {
                                width: eff_w.clone(),
                                is_collapsed: col_collapsed,
                                is_single_explicit_col: cspan == 1,
                                style: Arc::clone(&col_style),
                                group_style: Some(Arc::clone(&cg_style)),
                            });
                        }
                        cols_meta.push(ColMeta {
                            box_id: col_id,
                            orig_start: col_start,
                            orig_span: cspan,
                        });
                    }
                }
                if cols_meta.is_empty() {
                    let gspan = span.max(1);
                    for _ in 0..gspan {
                        explicit_col_defs.push(ColSlotDef {
                            width: cg_style.box_model.width.clone(),
                            is_collapsed: cg_collapsed,
                            is_single_explicit_col: gspan == 1,
                            style: Arc::clone(&cg_style),
                            group_style: Some(Arc::clone(&cg_style)),
                        });
                    }
                    colgroups.push(ColGroupMeta {
                        box_id: cid,
                        orig_start,
                        orig_span: gspan,
                        cols: cols_meta,
                    });
                } else {
                    let orig_span = explicit_col_defs.len() - orig_start;
                    colgroups.push(ColGroupMeta {
                        box_id: cid,
                        orig_start,
                        orig_span,
                        cols: cols_meta,
                    });
                }
            }
            BoxKind::TableColumn { span } => {
                let cspan = span.max(1);
                let col_style = Arc::clone(&cbox.style);
                let orig_start = explicit_col_defs.len();
                let col_collapsed = col_style.visibility == VisibilityMode::Collapse;
                for _ in 0..cspan {
                    explicit_col_defs.push(ColSlotDef {
                        width: col_style.box_model.width.clone(),
                        is_collapsed: col_collapsed,
                        is_single_explicit_col: cspan == 1,
                        style: Arc::clone(&col_style),
                        group_style: None,
                    });
                }
                direct_cols.push(ColMeta {
                    box_id: cid,
                    orig_start,
                    orig_span: cspan,
                });
            }
            BoxKind::TableRowGroup { section } => {
                flush_direct_rows(&mut pending_direct_rows, &mut body_sections);
                let sec_collapsed = cbox.style.visibility == VisibilityMode::Collapse;
                let row_ids: Vec<BoxId> = cbox
                    .children
                    .iter()
                    .copied()
                    .filter(|&rid| {
                        let rb = ctx.tree.get(rid);
                        !rb.is_out_of_flow
                            && matches!(rb.kind, BoxKind::TableRow | BoxKind::AnonymousTableRow)
                    })
                    .collect();
                let raw = RawSection {
                    box_id: Some(cid),
                    section,
                    row_ids,
                    is_collapsed: sec_collapsed,
                };
                match section {
                    TableSectionKind::Header if first_header.is_none() => {
                        first_header = Some(raw);
                    }
                    TableSectionKind::Footer if first_footer.is_none() => {
                        first_footer = Some(raw);
                    }
                    _ => body_sections.push(raw),
                }
            }
            BoxKind::TableRow | BoxKind::AnonymousTableRow => {
                pending_direct_rows.push(cid);
            }
            _ => {}
        }
    }
    flush_direct_rows(&mut pending_direct_rows, &mut body_sections);

    let mut ordered_raw = Vec::with_capacity(
        body_sections.len()
            + usize::from(first_header.is_some())
            + usize::from(first_footer.is_some()),
    );
    if let Some(h) = first_header {
        ordered_raw.push(h);
    }
    ordered_raw.extend(body_sections);
    if let Some(f) = first_footer {
        ordered_raw.push(f);
    }

    // Build per-section occupancy grids and place cells.
    let mut sections = Vec::with_capacity(ordered_raw.len());
    let mut rows = Vec::new();
    let mut raw_cells = Vec::new();
    let mut num_cell_cols = 0usize;

    for raw_sec in ordered_raw {
        let row_start = rows.len();
        let sec_row_count = raw_sec.row_ids.len();
        let mut occupied: Vec<Vec<bool>> = vec![Vec::new(); sec_row_count];

        for (local_r, row_box_id) in raw_sec.row_ids.into_iter().enumerate() {
            let global_r = rows.len();
            let row_collapsed = raw_sec.is_collapsed
                || ctx.tree.get(row_box_id).style.visibility == VisibilityMode::Collapse;
            rows.push(TableRowMeta {
                row_box_id,
                is_collapsed: row_collapsed,
            });

            let mut col_cursor = 0usize;
            let cell_ids = ctx.tree.get(row_box_id).children.clone();
            for cell_id in cell_ids {
                let cbox = ctx.tree.get(cell_id);
                if cbox.is_out_of_flow {
                    continue;
                }
                let (colspan, raw_rowspan) = match cbox.kind {
                    BoxKind::TableCell { colspan, rowspan } => (colspan.max(1), rowspan),
                    BoxKind::AnonymousTableCell => (1, 1),
                    _ => continue,
                };
                while occupied[local_r].get(col_cursor).copied().unwrap_or(false) {
                    col_cursor += 1;
                }
                // HTML Standard §4.9.11: rowspan=0 spans all remaining rows in the row group.
                let eff_rowspan = if raw_rowspan == 0 {
                    (sec_row_count - local_r).max(1)
                } else {
                    raw_rowspan.min(sec_row_count - local_r).max(1)
                };
                let end_col = col_cursor + colspan;
                for occ_row in occupied.iter_mut().skip(local_r).take(eff_rowspan) {
                    if occ_row.len() < end_col {
                        occ_row.resize(end_col, false);
                    }
                    for slot in occ_row.iter_mut().take(end_col).skip(col_cursor) {
                        *slot = true;
                    }
                }
                num_cell_cols = num_cell_cols.max(end_col);
                raw_cells.push((cell_id, global_r, col_cursor, colspan, eff_rowspan));
                col_cursor = end_col;
            }
        }

        sections.push(SectionMeta {
            box_id: raw_sec.box_id,
            section: raw_sec.section,
            row_start,
            row_count: sec_row_count,
            is_collapsed: raw_sec.is_collapsed,
        });
    }

    let num_rows = rows.len();
    // CSS Tables L3 / HTML5 Table Formatting: empty tables (0 rows) do not create columns.
    let raw_cols = if num_rows == 0 {
        0
    } else {
        num_cell_cols.max(explicit_col_defs.len())
    };

    // Column track merging (CSS Tables L3 §3.1).
    let mut has_orig_cell = vec![false; raw_cols];
    for &(_, _, c0, _, _) in &raw_cells {
        if c0 < raw_cols {
            has_orig_cell[c0] = true;
        }
    }

    let mut keep = vec![false; raw_cols];
    for c in 0..raw_cols {
        let def_opt = explicit_col_defs.get(c);
        let has_explicit_w = def_opt.is_some_and(|d| !matches!(d.width, SizeValue::Auto));
        let is_collapsed_col = def_opt.is_some_and(|d| d.is_collapsed);
        let is_single_explicit = def_opt.is_some_and(|d| d.is_single_explicit_col);
        keep[c] = is_fixed_layout
            || has_orig_cell[c]
            || has_explicit_w
            || is_collapsed_col
            || (is_single_explicit && num_rows > 0);
    }

    let mut orig_to_eff: Vec<Option<usize>> = vec![None; raw_cols];
    let mut eff_col_defs: Vec<Option<ColSlotDef>> = Vec::new();
    let mut eff_first_orig: Vec<usize> = Vec::new();
    let mut eff_end_orig: Vec<usize> = Vec::new();

    for c in 0..raw_cols {
        if keep[c] {
            let eff_idx = eff_col_defs.len();
            orig_to_eff[c] = Some(eff_idx);
            eff_col_defs.push(explicit_col_defs.get(c).cloned());
            eff_first_orig.push(c);
            eff_end_orig.push(c + 1);
        } else if c < num_cell_cols && !eff_col_defs.is_empty() {
            // Merged empty column inside a spanning cell attaches to the preceding kept column.
            let prev = eff_col_defs.len() - 1;
            orig_to_eff[c] = Some(prev);
            eff_end_orig[prev] = c + 1;
        }
    }
    let num_cols = eff_col_defs.len();

    // Build initial PlacedCell records with merged column coordinates.
    let mut cells: Vec<PlacedCell> = Vec::with_capacity(raw_cells.len());
    for (cell_id, row_idx, orig_c, orig_cs, rowspan) in raw_cells {
        let eff_c = orig_to_eff
            .get(orig_c)
            .and_then(|&x| x)
            .unwrap_or(0)
            .min(num_cols.saturating_sub(1));
        let orig_end = (orig_c + orig_cs).min(raw_cols);
        let eff_end = if orig_end > orig_c {
            let mut last = eff_c;
            for oc in orig_c..orig_end {
                if let Some(ec) = orig_to_eff.get(oc).and_then(|&x| x) {
                    last = last.max(ec);
                }
            }
            (last + 1).min(num_cols)
        } else {
            (eff_c + 1).min(num_cols)
        };
        let eff_cs = eff_end.saturating_sub(eff_c).max(1);
        cells.push(PlacedCell {
            box_id: cell_id,
            row_idx,
            col_idx: eff_c,
            colspan: eff_cs,
            orig_colspan: orig_cs,
            rowspan,
            min_w: 0.0,
            max_w: 0.0,
            fixed_w: None,
            pct_w: None,
            calc_px_w: 0.0,
            pb_w: 0.0,
            is_border_box: false,
            is_empty: true,
        });
    }

    // Compute collapsed border grid before measuring cells so cell half-borders are exact.
    let collapsed_borders = if collapsed {
        Some(build_collapsed_borders(
            ctx,
            &table_style,
            &eff_col_defs,
            &sections,
            &rows,
            &cells,
            num_rows,
            num_cols,
        ))
    } else {
        None
    };

    // Measure every cell's intrinsic widths and classify its CSS width constraints.
    let intrinsic_space = space.with_available(space.available_width, None);
    for cell in &mut cells {
        let cell_style = Arc::clone(&ctx.tree.get(cell.box_id).style);
        let cell_eval = ctx.eval_context_for(&cell_style, &intrinsic_space);
        let half_lr = collapsed_borders.as_ref().map(|cb| {
            let hb = cell_collapsed_half_borders(
                cb,
                cell.row_idx,
                cell.row_idx + cell.rowspan,
                cell.col_idx,
                cell.col_idx + cell.colspan,
            );
            (hb.left, hb.right)
        });
        let pb_w = cell_fixed_pb_w(&cell_style, &cell_eval, half_lr);
        let is_bb = cell_style.box_sizing == BoxSizingMode::BorderBox;

        let (content_min, content_max) =
            measure_content_intrinsic_widths(ctx, cell.box_id, intrinsic_space);
        let mut c_min = (content_min + pb_w).max(pb_w);
        let mut c_max = (content_max + pb_w).max(c_min);

        let (raw_fixed, raw_pct, raw_calc_px) =
            decompose_size(&cell_style.box_model.width, &cell_eval);
        // CSS 2.1 §10.7 / CSS Writing Modes 3: `min-block-size`/`max-block-size` (which map to
        // `min-width`/`max-width` in vertical writing modes) do not apply to `table-cell`.
        let (min_fixed, max_fixed) = if cell_style.text.writing_mode.is_vertical() {
            (None, None)
        } else {
            (
                decompose_size(&cell_style.box_model.min_width, &cell_eval).0,
                decompose_size(&cell_style.box_model.max_width, &cell_eval).0,
            )
        };

        let min_bb = min_fixed.map(|v| to_border_box_dim(v, pb_w, is_bb));
        let max_bb = max_fixed.map(|v| to_border_box_dim(v, pb_w, is_bb));

        // Clamp intrinsic measures by explicit `max-width` / `min-width` on `td`
        // (`td-min-width-auto-layout.html`, `td-max-width-auto-layout.html`).
        if let Some(mx) = max_bb {
            c_min = c_min.min(mx).max(pb_w);
            c_max = c_max.min(mx).max(c_min);
        }
        if let Some(mn) = min_bb {
            c_min = c_min.max(mn);
            c_max = c_max.max(mn);
        }

        let fixed_w = raw_fixed.map(|v| {
            let mut bb = to_border_box_dim(v, pb_w, is_bb);
            if let Some(mx) = max_bb {
                bb = bb.min(mx);
            }
            if let Some(mn) = min_bb {
                bb = bb.max(mn);
            }
            bb.max(pb_w)
        });

        // In auto layout, a cell's explicit fixed width constrains its max-content measure
        // (`column-widths.html` lines 65-93: `td.max = max(td.min, css_width)`).
        if let Some(fw) = fixed_w {
            c_max = c_min.max(fw);
        } else if let Some(mn) = min_bb {
            c_max = c_max.max(mn);
        }

        let is_empty = cell_is_visually_empty(ctx, cell.box_id) && c_max <= pb_w + 0.01;
        cell.min_w = c_min;
        cell.max_w = c_max;
        cell.fixed_w = fixed_w;
        cell.pct_w = raw_pct.map(|p| p.min(100.0));
        cell.calc_px_w = if raw_pct.is_some() && raw_calc_px > 0.0 {
            to_border_box_dim(raw_calc_px, pb_w, is_bb)
        } else {
            0.0
        };
        cell.pb_w = pb_w;
        cell.is_border_box = is_bb;
        cell.is_empty = is_empty;
    }

    TableGrid {
        top_captions,
        bottom_captions,
        colgroups,
        direct_cols,
        sections,
        rows,
        cells,
        eff_col_defs,
        orig_to_eff,
        eff_first_orig,
        eff_end_orig,
        num_cols,
        num_rows,
        is_fixed_layout,
        collapsed_borders,
    }
}

/// Computes per-column constraints for Auto Table Layout (CSS Tables L3 §4.4–§4.6).
pub(crate) fn compute_auto_col_constraints(
    ctx: &mut LayoutContext<'_>,
    grid: &TableGrid,
    h_spacing: f32,
    space: ConstraintSpace,
) -> Vec<ColConstraint> {
    let n = grid.num_cols;
    let mut cols = vec![ColConstraint::default(); n];
    if n == 0 {
        return cols;
    }

    // Track unconstrained vs constrained vs percentage single-column cell aggregates.
    let mut un_min = vec![0.0f32; n];
    let mut un_max = vec![0.0f32; n];
    let mut has_un = vec![false; n];
    let mut con_min = vec![0.0f32; n];
    let mut con_max = vec![0.0f32; n];
    let mut has_con = vec![false; n];
    let mut pct_max = vec![0.0f32; n];
    let mut has_pct = vec![false; n];
    let mut calc_px = vec![0.0f32; n];
    let mut any_non_empty = vec![false; n];

    for cell in &grid.cells {
        if cell.colspan != 1 || cell.col_idx >= n {
            continue;
        }
        let c = cell.col_idx;
        if !cell.is_empty {
            any_non_empty[c] = true;
        }
        if let Some(p) = cell.pct_w {
            if p > 0.0 {
                has_pct[c] = true;
                pct_max[c] = pct_max[c].max(p);
                calc_px[c] = calc_px[c].max(cell.calc_px_w);
            }
            un_min[c] = un_min[c].max(cell.min_w);
            un_max[c] = un_max[c].max(cell.max_w);
            has_un[c] = true;
        } else if cell.fixed_w.is_some() && cell.orig_colspan == 1 {
            has_con[c] = true;
            con_min[c] = con_min[c].max(cell.min_w);
            con_max[c] = con_max[c].max(cell.max_w);
        } else {
            has_un[c] = true;
            un_min[c] = un_min[c].max(cell.min_w);
            un_max[c] = un_max[c].max(cell.max_w);
        }
    }

    for c in 0..n {
        cols[c].is_empty = !any_non_empty[c];
        if has_pct[c] {
            cols[c].pct = Some(pct_max[c]);
            cols[c].calc_px = calc_px[c];
            cols[c].min_w = un_min[c].max(con_min[c]);
            cols[c].max_w = un_max[c].max(con_max[c]).max(cols[c].min_w);
        } else if has_con[c] {
            cols[c].is_constrained = true;
            cols[c].min_w = un_min[c].max(con_min[c]);
            // CSS Tables L3 / `column-widths.html`: constrained cell caps unconstrained max to UN.min!
            cols[c].max_w = un_min[c].max(con_max[c]).max(cols[c].min_w);
        } else if has_un[c] {
            cols[c].min_w = un_min[c];
            cols[c].max_w = un_max[c].max(un_min[c]);
        }

        // Merge `<col>` / `<colgroup>` constraints into column `c`.
        if let Some(Some(def)) = grid.eff_col_defs.get(c) {
            cols[c].is_collapsed = def.is_collapsed;
            let eval = ctx.eval_context_for(&def.style, &space);
            let (col_fixed, col_pct, col_calc_px) = decompose_size(&def.width, &eval);
            if let Some(p) = col_pct {
                // `colgroup-col.html`: `width: 0%` on `<col>` is ignored!
                if p > 0.0 {
                    if cols[c].pct.is_none() {
                        cols[c].pct = Some(p.min(100.0));
                    }
                    cols[c].calc_px = cols[c].calc_px.max(col_calc_px);
                    if col_calc_px > 0.0 {
                        cols[c].max_w = cols[c].max_w.max(col_calc_px);
                    }
                }
            } else if let Some(fw) = col_fixed {
                if cols[c].pct.is_none() {
                    cols[c].is_constrained = true;
                    if has_con[c] {
                        cols[c].max_w = cols[c].max_w.max(fw).max(cols[c].min_w);
                    } else {
                        cols[c].max_w = cols[c].min_w.max(fw);
                    }
                } else {
                    cols[c].max_w = cols[c].max_w.max(fw);
                }
                if fw > 0.0 {
                    cols[c].is_empty = false;
                }
            }
        }
    }

    // Cap single-column percentages left-to-right at 100% total.
    let mut running_pct = 0.0f32;
    for col in &mut cols {
        if let Some(p) = col.pct {
            let clamped = p.min((100.0 - running_pct).max(0.0));
            running_pct += clamped;
            col.pct = if clamped > 0.0 { Some(clamped) } else { None };
        }
    }

    // Distribute `colspan > 1` cells (sorted by span length ascending, then column index).
    let mut spanning: Vec<usize> = grid
        .cells
        .iter()
        .enumerate()
        .filter(|(_, c)| c.colspan > 1)
        .map(|(i, _)| i)
        .collect();
    spanning.sort_by_key(|&i| (grid.cells[i].colspan, grid.cells[i].col_idx));

    for cell_i in spanning {
        let cell = &grid.cells[cell_i];
        let start = cell.col_idx;
        let end = (start + cell.colspan).min(n);
        let span = end.saturating_sub(start);
        if span == 0 {
            continue;
        }
        let inter_spacing = h_spacing * (span.saturating_sub(1) as f32);
        let target_min = (cell.min_w - inter_spacing).max(0.0);
        let target_max = (cell.max_w - inter_spacing).max(target_min);

        // 1) Percentage redistribution to non-percentage columns (`colspan-redistribution.html`).
        if let Some(cell_p) = cell.pct_w {
            if cell_p > 0.0 {
                let existing_pct: f32 = cols[start..end].iter().filter_map(|c| c.pct).sum();
                let non_pct_idxs: Vec<usize> =
                    (start..end).filter(|&c| cols[c].pct.is_none()).collect();
                let rem_p = (cell_p - existing_pct).max(0.0);
                // Also respect overall table 100% cap.
                let table_pct: f32 = cols.iter().filter_map(|c| c.pct).sum();
                let avail_p = rem_p.min((100.0 - table_pct).max(0.0));
                if avail_p > 0.0 && !non_pct_idxs.is_empty() {
                    let max_sum: f32 = non_pct_idxs.iter().map(|&c| cols[c].max_w).sum();
                    if max_sum > 0.0 {
                        for &c in &non_pct_idxs {
                            cols[c].pct = Some(avail_p * (cols[c].max_w / max_sum));
                        }
                    } else {
                        let each = avail_p / non_pct_idxs.len() as f32;
                        for &c in &non_pct_idxs {
                            cols[c].pct = Some(each);
                        }
                    }
                }
            }
        }

        // 2) Minimum width distribution across spanned columns.
        let cur_min: f32 = cols[start..end].iter().map(|c| c.min_w).sum();
        if target_min > cur_min + 0.01 {
            let mut deficit = target_min - cur_min;
            // First grow columns toward their existing `max_w` (if any have `max_w > min_w`).
            let grow_room: f32 = cols[start..end]
                .iter()
                .map(|c| (c.max_w - c.min_w).max(0.0))
                .sum();
            if grow_room > 0.01 {
                let take = deficit.min(grow_room);
                for col in &mut cols[start..end] {
                    let room = (col.max_w - col.min_w).max(0.0);
                    col.min_w += take * (room / grow_room);
                }
                deficit -= take;
            }
            if deficit > 0.01 {
                // Prefer unconstrained non-pct columns if any; else non-pct; else all.
                let auto_idxs: Vec<usize> = (start..end)
                    .filter(|&c| !cols[c].is_constrained && cols[c].pct.is_none())
                    .collect();
                let non_pct_idxs: Vec<usize> =
                    (start..end).filter(|&c| cols[c].pct.is_none()).collect();
                let targets: Vec<usize> = if !auto_idxs.is_empty() {
                    auto_idxs
                } else if !non_pct_idxs.is_empty() {
                    non_pct_idxs
                } else {
                    (start..end).collect()
                };
                let max_sum: f32 = targets.iter().map(|&c| cols[c].max_w).sum();
                if max_sum > 0.01 {
                    for &c in &targets {
                        cols[c].min_w += deficit * (cols[c].max_w / max_sum);
                        cols[c].max_w = cols[c].max_w.max(cols[c].min_w);
                    }
                } else {
                    let each = deficit / targets.len() as f32;
                    for &c in &targets {
                        cols[c].min_w += each;
                        cols[c].max_w = cols[c].max_w.max(cols[c].min_w);
                    }
                }
            }
        }

        // 3) Maximum width distribution across spanned columns.
        let cur_max: f32 = cols[start..end].iter().map(|c| c.max_w).sum();
        if target_max > cur_max + 0.01 {
            let deficit = target_max - cur_max;
            let auto_idxs: Vec<usize> = (start..end)
                .filter(|&c| !cols[c].is_constrained && cols[c].pct.is_none())
                .collect();
            let non_pct_idxs: Vec<usize> =
                (start..end).filter(|&c| cols[c].pct.is_none()).collect();
            let targets: Vec<usize> = if !auto_idxs.is_empty() {
                auto_idxs
            } else if !non_pct_idxs.is_empty() {
                non_pct_idxs
            } else {
                (start..end).collect()
            };
            let max_sum: f32 = targets.iter().map(|&c| cols[c].max_w).sum();
            if max_sum > 0.01 {
                for &c in &targets {
                    cols[c].max_w += deficit * (cols[c].max_w / max_sum);
                }
            } else {
                let each = deficit / targets.len() as f32;
                for &c in &targets {
                    cols[c].max_w += each;
                }
            }
        }

        if !cell.is_empty {
            for col in &mut cols[start..end] {
                col.is_empty = false;
            }
        }
    }

    cols
}

/// Computes per-column constraints for Fixed Table Layout (CSS Tables L3 §4.2).
/// Only `<col>`/`<colgroup>` elements and Row 0 cells contribute (`fixed-layout-1.html`).
pub(crate) fn compute_fixed_col_constraints(
    ctx: &mut LayoutContext<'_>,
    grid: &TableGrid,
    h_spacing: f32,
    space: ConstraintSpace,
) -> Vec<ColConstraint> {
    let n = grid.num_cols;
    let mut cols = vec![ColConstraint::default(); n];
    if n == 0 {
        return cols;
    }

    // 1. Apply `<col>` / `<colgroup>` definitions first.
    for (c, def_opt) in grid.eff_col_defs.iter().enumerate().take(n) {
        if let Some(def) = def_opt {
            cols[c].is_collapsed = def.is_collapsed;
            let eval = ctx.eval_context_for(&def.style, &space);
            let (col_fixed, col_pct, col_calc_px) = decompose_size(&def.width, &eval);
            if let Some(p) = col_pct {
                if p > 0.0 {
                    cols[c].fixed_pct = Some(p.min(100.0));
                    cols[c].calc_px = col_calc_px;
                }
            } else if let Some(fw) = col_fixed {
                cols[c].fixed_px = Some(fw);
                cols[c].min_w = fw;
                cols[c].max_w = fw;
                cols[c].is_constrained = true;
            }
        }
    }

    // 2. Apply Row 0 cells to columns not already fixed by `<col>`/`<colgroup>`.
    for cell in &grid.cells {
        if cell.row_idx != 0 || cell.col_idx >= n {
            continue;
        }
        let start = cell.col_idx;
        let end = (start + cell.colspan).min(n);
        let span = end.saturating_sub(start);
        if span == 0 {
            continue;
        }
        if span == 1 {
            let c = start;
            if cols[c].fixed_px.is_none() && cols[c].fixed_pct.is_none() {
                if let Some(p) = cell.pct_w {
                    cols[c].fixed_pct = Some(p.min(100.0));
                    cols[c].calc_px = cell.calc_px_w;
                    // `td-box-sizing-001.html`: in fixed layout, content-box % cell adds border+padding!
                    cols[c].fixed_pct_pb = if cell.is_border_box { 0.0 } else { cell.pb_w };
                } else if let Some(fw) = cell.fixed_w {
                    cols[c].fixed_px = Some(fw);
                    cols[c].min_w = fw;
                    cols[c].max_w = fw;
                    cols[c].is_constrained = true;
                }
            }
        } else {
            let inter_spacing = h_spacing * (span.saturating_sub(1) as f32);
            if let Some(p) = cell.pct_w {
                // `colspan-redistribution.html` lines 107-182: in fixed tables, colspan %
                // distributes `p / colspan` ONLY to auto columns (not fixed or % columns).
                let per_p = p / span as f32;
                for col in &mut cols[start..end] {
                    if col.fixed_px.is_none() && col.fixed_pct.is_none() {
                        col.fixed_pct = Some(per_p);
                        col.fixed_pct_pb = 0.0;
                    }
                }
            } else if let Some(fw) = cell.fixed_w {
                let per_w = ((fw - inter_spacing).max(0.0)) / span as f32;
                for col in &mut cols[start..end] {
                    if col.fixed_px.is_none() && col.fixed_pct.is_none() {
                        col.fixed_px = Some(per_w);
                        col.min_w = per_w;
                        col.max_w = per_w;
                        col.is_constrained = true;
                    }
                }
            }
        }
    }

    // Cap total fixed percentages left-to-right at 100%.
    let mut running_pct = 0.0f32;
    for col in &mut cols {
        if let Some(p) = col.fixed_pct {
            let clamped = p.min((100.0 - running_pct).max(0.0));
            running_pct += clamped;
            col.fixed_pct = Some(clamped);
        }
    }

    cols
}

/// Computes inverse-percentage target column width (Rules #1 & #2 from CSS Tables L3 /
/// `colspan-redistribution.html` & `table-width-redistribution.html`).
pub(crate) fn compute_percent_adjusted_max_cols(cols: &[ColConstraint]) -> f32 {
    let raw_max: f32 = cols.iter().map(|c| c.max_w).sum();
    let mut target = raw_max;
    let mut total_pct = 0.0f32;
    let mut non_pct_max = 0.0f32;

    for col in cols {
        if let Some(p) = col.pct {
            if p > 0.0 {
                total_pct += p;
                let implied = col.max_w / (p / 100.0);
                if implied.is_finite() {
                    target = target.max(implied);
                }
            }
        } else {
            non_pct_max += col.max_w;
        }
    }
    if total_pct > 0.0 {
        if total_pct < 99.99 {
            let implied = non_pct_max / (1.0 - total_pct / 100.0);
            if implied.is_finite() {
                target = target.max(implied);
            }
        } else if non_pct_max > 0.01 {
            return f32::INFINITY;
        }
    }
    target
}

/// Exact CSS Tables L3 §4.7 4-guess column width distribution for Auto Table Layout.
pub(crate) fn solve_auto_column_widths(cols: &[ColConstraint], assignable: f32) -> Vec<f32> {
    let n = cols.len();
    if n == 0 {
        return Vec::new();
    }

    let g0: Vec<f32> = cols.iter().map(|c| c.min_w).collect();
    let g1: Vec<f32> = cols
        .iter()
        .map(|c| {
            if let Some(p) = c.pct {
                ((p / 100.0) * assignable + c.calc_px).max(c.min_w)
            } else {
                c.min_w
            }
        })
        .collect();
    let g2: Vec<f32> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if c.pct.is_some() {
                g1[i]
            } else if c.is_constrained {
                c.max_w.max(c.min_w)
            } else {
                c.min_w
            }
        })
        .collect();
    let g3: Vec<f32> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if c.pct.is_some() {
                g1[i]
            } else {
                c.max_w.max(c.min_w)
            }
        })
        .collect();

    let s0: f32 = g0.iter().sum();
    let s1: f32 = g1.iter().sum();
    let s2: f32 = g2.iter().sum();
    let s3: f32 = g3.iter().sum();

    let interp = |a: &[f32], b: &[f32], sa: f32, sb: f32| -> Vec<f32> {
        let denom = (sb - sa).max(1e-5);
        let t = ((assignable - sa) / denom).clamp(0.0, 1.0);
        a.iter()
            .zip(b.iter())
            .map(|(&va, &vb)| va + (vb - va) * t)
            .collect()
    };

    if assignable <= s0 + 0.01 {
        return g0;
    }
    if assignable <= s1 + 0.01 {
        return interp(&g0, &g1, s0, s1);
    }
    if assignable <= s2 + 0.01 {
        return interp(&g1, &g2, s1, s2);
    }
    if assignable <= s3 + 0.01 {
        return interp(&g2, &g3, s2, s3);
    }

    // Excess width beyond Guess 3 (`table-width-redistribution.html` priorities).
    let mut w = g3;
    let extra = (assignable - s3).max(0.0);
    if extra <= 0.01 {
        return w;
    }

    // Priority 1: Non-empty unconstrained (auto) non-percentage columns.
    let auto_non_empty: Vec<usize> = (0..n)
        .filter(|&i| !cols[i].is_constrained && cols[i].pct.is_none() && !cols[i].is_empty)
        .collect();
    if !auto_non_empty.is_empty() {
        let weight_sum: f32 = auto_non_empty.iter().map(|&i| cols[i].max_w).sum();
        if weight_sum > 0.01 {
            for &i in &auto_non_empty {
                w[i] += extra * (cols[i].max_w / weight_sum);
            }
        } else {
            let each = extra / auto_non_empty.len() as f32;
            for &i in &auto_non_empty {
                w[i] += each;
            }
        }
        return w;
    }

    // Priority 2: Constrained (fixed) non-percentage columns.
    let fixed_cols: Vec<usize> = (0..n)
        .filter(|&i| cols[i].is_constrained && cols[i].pct.is_none())
        .collect();
    if !fixed_cols.is_empty() {
        let weight_sum: f32 = fixed_cols.iter().map(|&i| cols[i].max_w).sum();
        if weight_sum > 0.01 {
            for &i in &fixed_cols {
                w[i] += extra * (cols[i].max_w / weight_sum);
            }
        } else {
            let each = extra / fixed_cols.len() as f32;
            for &i in &fixed_cols {
                w[i] += each;
            }
        }
        return w;
    }

    // Priority 3: Percentage columns.
    let pct_cols: Vec<usize> = (0..n).filter(|&i| cols[i].pct.is_some()).collect();
    if !pct_cols.is_empty() {
        let weight_sum: f32 = pct_cols.iter().map(|&i| cols[i].pct.unwrap_or(0.0)).sum();
        if weight_sum > 0.01 {
            for &i in &pct_cols {
                w[i] += extra * (cols[i].pct.unwrap_or(0.0) / weight_sum);
            }
        } else {
            let each = extra / pct_cols.len() as f32;
            for &i in &pct_cols {
                w[i] += each;
            }
        }
        return w;
    }

    // Priority 4: Empty auto columns share evenly.
    let each = extra / n as f32;
    for val in &mut w {
        *val += each;
    }
    w
}

/// Fixed table layout column width distribution (`table-width-redistribution-fixed.html` &
/// `table-width-redistribution-fixed-padding.html`).
pub(crate) fn solve_fixed_column_widths(cols: &[ColConstraint], assignable: f32) -> Vec<f32> {
    let n = cols.len();
    if n == 0 {
        return Vec::new();
    }
    let mut w = vec![0.0f32; n];
    let mut fixed_idxs = Vec::new();
    let mut pct_idxs = Vec::new();
    let mut auto_idxs = Vec::new();

    for (i, col) in cols.iter().enumerate() {
        if let Some(fw) = col.fixed_px {
            w[i] = fw;
            fixed_idxs.push(i);
        } else if let Some(p) = col.fixed_pct {
            w[i] = ((p / 100.0) * assignable + col.calc_px + col.fixed_pct_pb).max(0.0);
            pct_idxs.push(i);
        } else {
            auto_idxs.push(i);
        }
    }

    let used: f32 = w.iter().sum();
    if used < assignable - 0.01 {
        let extra = assignable - used;
        if !auto_idxs.is_empty() {
            let each = extra / auto_idxs.len() as f32;
            for &i in &auto_idxs {
                w[i] += each;
            }
        } else if !fixed_idxs.is_empty() {
            // CSS Tables L3 `#distributing-width-to-columns`: with no auto columns, excess
            // goes ONLY to fixed-width columns in proportion to their widths.
            let weight_sum: f32 = fixed_idxs.iter().map(|&i| w[i]).sum();
            if weight_sum > 0.01 {
                for &i in &fixed_idxs {
                    w[i] += extra * (w[i] / weight_sum);
                }
            } else {
                let each = extra / fixed_idxs.len() as f32;
                for &i in &fixed_idxs {
                    w[i] += each;
                }
            }
        } else if !pct_idxs.is_empty() {
            let weight_sum: f32 = pct_idxs.iter().map(|&i| w[i]).sum();
            if weight_sum > 0.01 {
                for &i in &pct_idxs {
                    w[i] += extra * (w[i] / weight_sum);
                }
            } else {
                let each = extra / pct_idxs.len() as f32;
                for &i in &pct_idxs {
                    w[i] += each;
                }
            }
        }
    } else if used > assignable + 0.01 && !pct_idxs.is_empty() {
        // Fixed columns never shrink below `fixed_px`; shrink percentage columns to fit.
        let fixed_sum: f32 = fixed_idxs.iter().map(|&i| w[i]).sum();
        let pct_budget = (assignable - fixed_sum).max(0.0);
        let pct_sum: f32 = pct_idxs.iter().map(|&i| w[i]).sum();
        if pct_sum > 0.01 {
            let scale = (pct_budget / pct_sum).clamp(0.0, 1.0);
            for &i in &pct_idxs {
                w[i] *= scale;
            }
        }
    }

    w
}

/// Distributes `extra` height across `row_indices` according to CSS Tables L3 / TablesNG
/// priority order (`rowspan-height-redistribution.html`, `tbody-height-redistribution.html`,
/// `table-height-redistribution.html`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn distribute_height_to_rows(
    row_indices: &[usize],
    extra: f32,
    row_heights: &mut [f32],
    row_constrained: &[bool],
    row_pct: &[Option<f32>],
    pct_basis: Option<f32>,
    all_empty_last_only: bool,
    preferred_empty_rows: Option<&[bool]>,
) {
    if row_indices.is_empty() || extra <= 0.01 {
        return;
    }
    let mut rem = extra;

    // 1. If a percentage resolution size is available, grow percentage rows toward their target.
    if let Some(basis) = pct_basis {
        let mut pct_deficits: Vec<(usize, f32)> = Vec::new();
        let mut total_def = 0.0f32;
        for &r in row_indices {
            if let Some(p) = row_pct[r] {
                let target = (p / 100.0) * basis;
                let def = (target - row_heights[r]).max(0.0);
                if def > 0.01 {
                    pct_deficits.push((r, def));
                    total_def += def;
                }
            }
        }
        if total_def > 0.01 {
            let take = rem.min(total_def);
            for (r, def) in pct_deficits {
                row_heights[r] += take * (def / total_def);
            }
            rem -= take;
        }
    }
    if rem <= 0.01 {
        return;
    }

    // Helper: distribute `rem` proportionally to existing heights, or evenly if all 0.
    let apply_group = |targets: &[usize], amount: f32, heights: &mut [f32], last_only: bool| {
        if targets.is_empty() || amount <= 0.01 {
            return;
        }
        let sum: f32 = targets.iter().map(|&r| heights[r]).sum();
        if sum > 0.01 {
            for &r in targets {
                heights[r] += amount * (heights[r] / sum);
            }
        } else if last_only {
            if let Some(&last) = targets.last() {
                heights[last] += amount;
            }
        } else {
            let each = amount / targets.len() as f32;
            for &r in targets {
                heights[r] += each;
            }
        }
    };

    // Note: when `pct_basis` is None, percent-only rows behave as unconstrained (`rowspan-height-redistribution.html:180`).
    let is_con = |r: usize| {
        if pct_basis.is_some() {
            row_constrained[r] || row_pct[r].is_some()
        } else {
            row_constrained[r]
        }
    };

    // Priority A: Unconstrained non-empty rows (`height > 0`).
    let uncon_non_empty: Vec<usize> = row_indices
        .iter()
        .copied()
        .filter(|&r| !is_con(r) && row_heights[r] > 0.01)
        .collect();
    if !uncon_non_empty.is_empty() {
        apply_group(&uncon_non_empty, rem, row_heights, false);
        return;
    }

    // Priority B: For rowspan distribution (`all_empty_last_only`), constrained non-empty rows
    // grow before empty rows (`computing-column-measure-1.html:28`, `rowspan-height-redistribution.html:164`).
    // For tbody/table height distribution, unconstrained empty rows grow before constrained rows
    // (`tbody-height-redistribution.html:103-139`).
    let con_non_empty: Vec<usize> = row_indices
        .iter()
        .copied()
        .filter(|&r| is_con(r) && row_heights[r] > 0.01)
        .collect();
    let uncon_empty: Vec<usize> = row_indices
        .iter()
        .copied()
        .filter(|&r| !is_con(r) && row_heights[r] <= 0.01)
        .collect();

    if all_empty_last_only {
        if !con_non_empty.is_empty() {
            apply_group(&con_non_empty, rem, row_heights, false);
            return;
        }
        if !uncon_empty.is_empty() {
            if let Some(pref) = preferred_empty_rows {
                let pref_rows: Vec<usize> = uncon_empty
                    .iter()
                    .copied()
                    .filter(|&r| pref.get(r).copied().unwrap_or(false))
                    .collect();
                if !pref_rows.is_empty() {
                    apply_group(&pref_rows, rem, row_heights, true);
                    return;
                }
            }
            apply_group(&uncon_empty, rem, row_heights, true);
            return;
        }
    } else {
        if !uncon_empty.is_empty() {
            apply_group(&uncon_empty, rem, row_heights, false);
            return;
        }
        if !con_non_empty.is_empty() {
            apply_group(&con_non_empty, rem, row_heights, false);
            return;
        }
    }

    // Fallback: all rows.
    apply_group(row_indices, rem, row_heights, all_empty_last_only);
}

/// Computes `(left_rel_to_grid_content, width)` for a `<col>` or `<colgroup>` spanning
/// original column range `[orig_start..orig_start + orig_span)` after track merging.
pub(crate) fn resolve_col_span_rect(
    grid: &TableGrid,
    orig_start: usize,
    orig_span: usize,
    col_x: &[f32],
    col_w: &[f32],
    col_collapsed: &[bool],
) -> (f32, f32) {
    if grid.num_cols == 0 || orig_span == 0 {
        return (0.0, 0.0);
    }
    let raw_len = grid.orig_to_eff.len();
    if orig_start >= raw_len {
        return (0.0, 0.0);
    }
    let orig_end = (orig_start + orig_span).min(raw_len);
    let mut first_eff: Option<usize> = None;
    let mut last_eff: Option<usize> = None;
    for &ec_opt in grid.orig_to_eff.iter().take(orig_end).skip(orig_start) {
        if let Some(ec) = ec_opt {
            if first_eff.is_none() {
                first_eff = Some(ec);
            }
            last_eff = Some(ec);
        }
    }
    let (Some(fe), Some(le)) = (first_eff, last_eff) else {
        return (0.0, 0.0);
    };

    // Determine start X: if `orig_start` begins strictly inside a merged column group,
    // interpolate proportionally across `fe` so edges stay within the spanning cell without
    // overlapping earlier `<col>`s (`col-offsetwidth-span-starting-inside-spanning-cell.html`).
    let fe_orig_s = grid.eff_first_orig[fe];
    let fe_orig_e = grid.eff_end_orig[fe];
    let fe_count = (fe_orig_e - fe_orig_s).max(1) as f32;
    let start_frac = ((orig_start.saturating_sub(fe_orig_s)) as f32 / fe_count).clamp(0.0, 1.0);
    let x_start = if col_collapsed[fe] {
        col_x[fe]
    } else {
        col_x[fe] + col_w[fe] * start_frac
    };

    let le_orig_s = grid.eff_first_orig[le];
    let le_orig_e = grid.eff_end_orig[le];
    let le_count = (le_orig_e - le_orig_s).max(1) as f32;
    let end_in_le = orig_end.min(le_orig_e).saturating_sub(le_orig_s);
    let end_frac = (end_in_le as f32 / le_count).clamp(0.0, 1.0);
    let x_end = if col_collapsed[le] {
        col_x[le]
    } else {
        col_x[le] + col_w[le] * end_frac
    };

    (x_start, (x_end - x_start).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_column_widths_four_guess_distribution_and_priority() {
        let cols = vec![
            ColConstraint {
                min_w: 40.0,
                max_w: 100.0,
                pct: None,
                is_constrained: false,
                is_empty: false,
                ..Default::default()
            },
            ColConstraint {
                min_w: 20.0,
                max_w: 50.0,
                pct: Some(25.0),
                is_constrained: false,
                is_empty: false,
                ..Default::default()
            },
            ColConstraint {
                min_w: 50.0,
                max_w: 50.0,
                pct: None,
                is_constrained: true,
                is_empty: false,
                ..Default::default()
            },
        ];
        // Excess width beyond max_w (400px) goes exclusively to the unconstrained auto column.
        let w = solve_auto_column_widths(&cols, 400.0);
        assert!((w[0] - 250.0).abs() < 0.01, "auto col={}", w[0]);
        assert!((w[1] - 100.0).abs() < 0.01, "pct col={}", w[1]);
        assert!((w[2] - 50.0).abs() < 0.01, "fixed col={}", w[2]);
    }

    #[test]
    fn fixed_column_widths_give_excess_only_to_fixed_columns() {
        let cols = vec![
            ColConstraint {
                fixed_px: Some(20.0),
                ..Default::default()
            },
            ColConstraint {
                fixed_px: Some(10.0),
                ..Default::default()
            },
            ColConstraint {
                fixed_pct: Some(10.0),
                ..Default::default()
            },
        ];
        let w = solve_fixed_column_widths(&cols, 300.0);
        assert!((w[0] - 180.0).abs() < 0.01, "w0={}", w[0]);
        assert!((w[1] - 90.0).abs() < 0.01, "w1={}", w[1]);
        assert!((w[2] - 30.0).abs() < 0.01, "w2={}", w[2]);
    }

    #[test]
    fn percent_adjusted_max_cols_resolves_inverse_percentage() {
        let cols = vec![ColConstraint {
            min_w: 150.0,
            max_w: 150.0,
            pct: Some(50.0),
            ..Default::default()
        }];
        let implied = compute_percent_adjusted_max_cols(&cols);
        assert!((implied - 300.0).abs() < 0.01, "implied={implied}");
    }

    #[test]
    fn collapsed_border_conflict_resolution_prefers_hidden_then_wider_then_style() {
        let mut edge = CollapsedEdge {
            width: 2.0,
            style: LineStyle::Dashed,
            color: ColorRgba::BLACK,
            prio: 1,
        };
        // Same width: solid beats dashed.
        merge_collapsed_edge(
            &mut edge,
            CollapsedEdge {
                width: 2.0,
                style: LineStyle::Solid,
                color: ColorRgba::rgba(255, 0, 0, 255),
                prio: 1,
            },
        );
        assert_eq!(edge.style, LineStyle::Solid);
        // Wider beats narrower regardless of style.
        merge_collapsed_edge(
            &mut edge,
            CollapsedEdge {
                width: 4.0,
                style: LineStyle::Dotted,
                color: ColorRgba::rgba(0, 255, 0, 255),
                prio: 1,
            },
        );
        assert_eq!(edge.width, 4.0);
        assert_eq!(edge.style, LineStyle::Dotted);
        // Hidden suppresses all other borders.
        merge_collapsed_edge(
            &mut edge,
            CollapsedEdge {
                width: 0.0,
                style: LineStyle::Hidden,
                color: ColorRgba::TRANSPARENT,
                prio: 1,
            },
        );
        assert_eq!(edge.style, LineStyle::Hidden);
        assert_eq!(edge.width, 0.0);
    }

    #[test]
    fn rowspan_height_redistribution_prefers_last_row_with_other_cells() {
        let mut heights = vec![0.0, 0.0, 0.0, 0.0];
        let constrained = vec![false; 4];
        let pct = vec![None; 4];
        let pref = vec![true, true, false, false];
        distribute_height_to_rows(
            &[0, 1, 2, 3],
            100.0,
            &mut heights,
            &constrained,
            &pct,
            None,
            true,
            Some(&pref),
        );
        assert_eq!(heights, vec![0.0, 100.0, 0.0, 0.0]);
    }
}
