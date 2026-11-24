//! Retained per-container `taffy::TaffyTree` sub-solver invoked STRICTLY for
//! CSS Flexbox (`flex` / `inline-flex`) and CSS Grid (`grid` / `inline-grid`)
//! formatting contexts.
//!
//! Each container keeps its own [`RetainedTaffyContainer`] inside `LayoutCache`
//! so unchanged flex/grid items preserve Taffy's internal measurement cache
//! across layout passes without re-allocating a fresh engine every frame.

use std::sync::Arc;

use rix_core::{Point, Rect};
use rix_css::{
    AlignValue, BoxSizingMode, ComputedStyle, DisplayMode, EvalContext, FlexDirectionMode,
    FlexGridStyle, FlexWrapMode, GridAutoFlowMode, GridPlacementValue, GridTemplateAreasValue,
    GridTrackSize, LengthPercentage, LengthPercentageAuto, OverflowMode, PositionMode, SizeValue,
};
use taffy::prelude as tp;
use taffy::{AvailableSpace, LayoutInput, LayoutOutput, NodeId as TaffyNodeId, TaffyTree};

use crate::bfc::apply_relative_offset;
use crate::box_tree::{BoxId, FormattingContextKind};
use crate::fragment::{
    layout_box, measure_intrinsic_widths, BoxMetrics, ConstraintSpace, Fragment, FragmentKind,
    LayoutContext,
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct TaffyLeafCtx {
    pub box_id: BoxId,
}

#[derive(Debug, Clone, Copy)]
struct RetainedChildEntry {
    dom_key: u64,
    subtree_hash: u64,
    taffy_node: TaffyNodeId,
}

/// Retained `taffy::TaffyTree` scoped to a single Flex or Grid container.
pub(crate) struct RetainedTaffyContainer {
    taffy: TaffyTree<TaffyLeafCtx>,
    root: TaffyNodeId,
    root_hash: u64,
    children: Vec<RetainedChildEntry>,
}

impl RetainedTaffyContainer {
    fn new(root_style: tp::Style) -> Self {
        let mut taffy = TaffyTree::new();
        let root = taffy
            .new_leaf(root_style)
            .expect("taffy root allocation succeeds");
        Self {
            taffy,
            root,
            root_hash: 0,
            children: Vec::new(),
        }
    }
}

/// Lays out a CSS Flex (`flex`/`inline-flex`) or Grid (`grid`/`inline-grid`) container
/// using a retained per-container `taffy::TaffyTree` and leaf measurement callbacks
/// into our BFC/IFC/Table/Replaced formatting contexts.
pub fn layout_flex_grid_container(
    ctx: &mut LayoutContext<'_>,
    container_id: BoxId,
    space: ConstraintSpace,
) -> Fragment {
    let container_box = ctx.tree.get(container_id);
    let container_style = Arc::clone(&container_box.style);
    let fc = container_box.formatting_context();
    debug_assert!(matches!(
        fc,
        FormattingContextKind::Flex | FormattingContextKind::Grid
    ));

    let container_key = container_box
        .dom_node
        .map_or(u64::from(container_id.0) | (1u64 << 63), |n| {
            usize::from(n) as u64
        });

    // Collect in-flow flex/grid items (sorted stably by CSS `order`) and separate out-of-flow children.
    let raw_children = container_box.children.clone();
    let mut in_flow_items: Vec<(i32, usize, BoxId)> = Vec::with_capacity(raw_children.len());
    let mut oof_children: Vec<BoxId> = Vec::new();
    for (idx, cid) in raw_children.into_iter().enumerate() {
        let cb = ctx.tree.get(cid);
        if cb.is_out_of_flow {
            oof_children.push(cid);
        } else {
            let order = cb.style.flex_grid.as_ref().map_or(0, |fg| fg.order);
            in_flow_items.push((order, idx, cid));
        }
    }
    in_flow_items.sort_by_key(|&(order, idx, _)| (order, idx));
    let ordered_items: Vec<BoxId> = in_flow_items.into_iter().map(|(_, _, cid)| cid).collect();

    let eval = ctx.eval_context_for(&container_style, &space);
    let root_taffy_style = to_taffy_container_style(&container_style, fc, &space, &eval);
    let container_metrics = BoxMetrics::resolve_with_eval(&container_style, &space, eval);
    let c_pb_w = container_metrics.padding.horizontal() + container_metrics.border.horizontal();
    let c_pb_h = container_metrics.padding.vertical() + container_metrics.border.vertical();
    let c_avail_w = container_metrics.specified_width.unwrap_or(
        (space.available_width - container_metrics.margin.horizontal() - c_pb_w).max(0.0),
    );
    let c_avail_h = container_metrics.specified_height.or_else(|| {
        space
            .available_height
            .map(|ah| (ah - container_metrics.margin.vertical() - c_pb_h).max(0.0))
    });
    let is_flex = fc == FormattingContextKind::Flex;
    let is_row = container_style.flex_grid.as_ref().is_none_or(|fg| {
        matches!(
            fg.flex_direction,
            FlexDirectionMode::Row | FlexDirectionMode::RowReverse
        )
    });
    let mut item_evals = Vec::with_capacity(ordered_items.len());
    let mut item_special_basis: Vec<Option<(f32, f32)>> = Vec::with_capacity(ordered_items.len());
    for &cid in &ordered_items {
        let cs = Arc::clone(&ctx.tree.get(cid).style);
        let ie = ctx.eval_context_for(&cs, &space);
        let sb = if is_flex {
            resolve_flex_item_special_basis(
                ctx, cid, &cs, is_row, c_avail_w, c_avail_h, &space, &ie,
            )
        } else {
            None
        };
        item_evals.push(ie);
        item_special_basis.push(sb);
    }
    let root_style_hash = Arc::as_ptr(&container_style) as usize as u64
        ^ (u64::from(space.available_width.to_bits()) << 32)
        ^ item_special_basis.iter().fold(0u64, |acc, b| {
            acc.rotate_left(7)
                ^ u64::from(b.map_or(0u32, |(v, m)| v.to_bits() ^ m.to_bits().rotate_left(3)))
        });

    // Take the retained container out of `LayoutCache` temporarily so the measure closure can
    // borrow `ctx` mutably (including `LayoutCache` fragment cache for leaf subtrees!).
    let mut retained = ctx
        .cache
        .taffy_containers
        .remove(&container_key)
        .unwrap_or_else(|| RetainedTaffyContainer::new(root_taffy_style.clone()));

    let parent_areas = container_style
        .flex_grid
        .as_deref()
        .map(|fg| &fg.grid_template_areas)
        .filter(|a| a.columns > 0 && !a.areas.is_empty());

    reconcile_retained_tree(
        &mut retained,
        ctx.tree,
        &ordered_items,
        &item_evals,
        &item_special_basis,
        is_row,
        root_taffy_style,
        root_style_hash,
        &space,
        parent_areas,
    );

    let taffy_avail = tp::Size {
        width: AvailableSpace::Definite(space.available_width.max(0.0)),
        height: space
            .available_height
            .map_or(AvailableSpace::MaxContent, AvailableSpace::Definite),
    };

    let _ = retained.taffy.compute_layout_with_measure(
        retained.root,
        taffy_avail,
        |input: LayoutInput,
         _node: TaffyNodeId,
         node_ctx: Option<&mut TaffyLeafCtx>,
         _style: &tp::Style| {
            let Some(leaf) = node_ctx else {
                return LayoutOutput::HIDDEN;
            };
            measure_taffy_leaf(ctx, leaf.box_id, input, space)
        },
    );

    let root_layout = *retained
        .taffy
        .layout(retained.root)
        .expect("taffy root layout computed");
    let item_layouts: Vec<(BoxId, taffy::Layout)> = ordered_items
        .iter()
        .zip(retained.children.iter())
        .map(|(&cid, entry)| {
            let layout = *retained
                .taffy
                .layout(entry.taffy_node)
                .expect("taffy child layout computed");
            (cid, layout)
        })
        .collect();

    // Return retained container back into `LayoutCache` for future incremental passes.
    ctx.cache.taffy_containers.insert(container_key, retained);

    let metrics = BoxMetrics::resolve(&container_style, &space);
    let pb_w = metrics.padding.horizontal() + metrics.border.horizontal();
    let pb_h = metrics.padding.vertical() + metrics.border.vertical();
    let border_w = root_layout.size.width;
    let border_h = root_layout.size.height;

    let my_oof_start = ctx.pending_oof.len();
    let mut placed_children = Vec::with_capacity(item_layouts.len() + oof_children.len());
    for (cid, item_layout) in item_layouts {
        let item_w = item_layout.size.width.max(0.0);
        let item_h = item_layout.size.height.max(0.0);
        let probe_key = (cid, LayoutContext::quantize_px(item_w), u32::MAX);
        let reused_frag = if !has_percent_height_direct_child(ctx.tree, cid) {
            ctx.pass_fragment_memo.get(&probe_key).and_then(|f| {
                if (f.border_rect.size.height - item_h).abs() < 0.75 {
                    Some(f.clone())
                } else {
                    None
                }
            })
        } else {
            None
        };
        let child_oof_start = ctx.pending_oof.len();
        let mut frag = match reused_frag {
            Some(f) => f,
            None => {
                let item_space = space.with_available(item_w, Some(item_h));
                layout_box(ctx, cid, item_space)
            }
        };
        // Pin border-box to Taffy's exact solved Flex/Grid item geometry.
        let i_pb_w = frag.padding.horizontal() + frag.border.horizontal();
        let i_pb_h = frag.padding.vertical() + frag.border.vertical();
        frag.border_rect = Rect::new(
            item_layout.location.x,
            item_layout.location.y,
            item_w,
            item_h,
        );
        frag.content_rect = Rect::new(
            frag.border.left + frag.padding.left,
            frag.border.top + frag.padding.top,
            (item_w - i_pb_w).max(0.0),
            (item_h - i_pb_h).max(0.0),
        );
        apply_relative_offset(&mut frag, space);
        crate::position::offset_pending_oof(
            &mut ctx.pending_oof,
            child_oof_start,
            frag.border_rect.origin.x,
            frag.border_rect.origin.y,
        );
        frag.compute_overflow_rect();
        placed_children.push(frag);
    }

    // Push direct out-of-flow (`position: absolute | fixed`) children and resolve any
    // whose true CSS containing block is this Flex/Grid container's padding box.
    for oof_id in oof_children {
        let is_fixed = ctx.tree.get(oof_id).style.position == PositionMode::Fixed;
        ctx.pending_oof.push(crate::position::PendingOof {
            box_id: oof_id,
            static_pos: Point::new(
                metrics.border.left + metrics.padding.left,
                metrics.border.top + metrics.padding.top,
            ),
            is_fixed,
            flex_hint: None,
            grid_hint: None,
        });
    }
    crate::position::resolve_oof_for_container(
        ctx,
        my_oof_start,
        &container_style,
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

    let mut fragment = Fragment {
        box_id: container_id,
        dom_node: ctx.tree.get(container_id).dom_node,
        style: container_style,
        border_rect: Rect::new(0.0, 0.0, border_w, border_h),
        padding: metrics.padding,
        border: metrics.border,
        margin: metrics.margin,
        content_rect: Rect::new(
            metrics.border.left + metrics.padding.left,
            metrics.border.top + metrics.padding.top,
            (border_w - pb_w).max(0.0),
            (border_h - pb_h).max(0.0),
        ),
        baseline: Some(border_h),
        last_baseline: Some(border_h),
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

#[allow(clippy::too_many_arguments)]
fn reconcile_retained_tree(
    retained: &mut RetainedTaffyContainer,
    tree: &crate::box_tree::BoxTree,
    ordered_items: &[BoxId],
    item_evals: &[EvalContext],
    item_special_basis: &[Option<(f32, f32)>],
    is_row: bool,
    root_style: tp::Style,
    root_hash: u64,
    space: &ConstraintSpace,
    parent_areas: Option<&GridTemplateAreasValue>,
) {
    let root_changed = retained.root_hash != root_hash;
    if root_changed {
        let _ = retained.taffy.set_style(retained.root, root_style);
        retained.root_hash = root_hash;
    }

    let same_structure = retained.children.len() == ordered_items.len()
        && retained
            .children
            .iter()
            .zip(ordered_items.iter())
            .all(|(entry, &cid)| {
                let dom_key = child_dom_key(tree, cid);
                entry.dom_key == dom_key
            });

    if same_structure {
        // Fast incremental path: preserve Taffy node IDs and only mark mutated items dirty!
        for (i, (entry, &cid)) in retained
            .children
            .iter_mut()
            .zip(ordered_items.iter())
            .enumerate()
        {
            let child_box = tree.get(cid);
            if let Some(leaf_ctx) = retained.taffy.get_node_context_mut(entry.taffy_node) {
                leaf_ctx.box_id = cid;
            }
            if root_changed
                || child_box.subtree_dirty
                || entry.subtree_hash != child_box.subtree_hash
            {
                let item_style = to_taffy_item_style(
                    &child_box.style,
                    space,
                    &item_evals[i],
                    parent_areas,
                    item_special_basis[i],
                    is_row,
                );
                let _ = retained.taffy.set_style(entry.taffy_node, item_style);
                let _ = retained.taffy.mark_dirty(entry.taffy_node);
                entry.subtree_hash = child_box.subtree_hash;
            }
        }
        return;
    }

    // Structural change (items inserted/removed/reordered): rebuild children list cleanly.
    for old in retained.children.drain(..) {
        let _ = retained.taffy.remove(old.taffy_node);
    }
    let mut taffy_kids = Vec::with_capacity(ordered_items.len());
    for (i, &cid) in ordered_items.iter().enumerate() {
        let child_box = tree.get(cid);
        let item_style = to_taffy_item_style(
            &child_box.style,
            space,
            &item_evals[i],
            parent_areas,
            item_special_basis[i],
            is_row,
        );
        let node = retained
            .taffy
            .new_leaf_with_context(item_style, TaffyLeafCtx { box_id: cid })
            .expect("taffy leaf allocation succeeds");
        taffy_kids.push(node);
        retained.children.push(RetainedChildEntry {
            dom_key: child_dom_key(tree, cid),
            subtree_hash: child_box.subtree_hash,
            taffy_node: node,
        });
    }
    let _ = retained.taffy.set_children(retained.root, &taffy_kids);
}

#[allow(clippy::too_many_arguments)]
#[rustfmt::skip]
fn resolve_flex_item_special_basis(
    ctx: &mut LayoutContext<'_>, cid: BoxId, style: &ComputedStyle,
    is_row: bool, c_avail_w: f32, c_avail_h: Option<f32>,
    space: &ConstraintSpace, eval: &EvalContext,
) -> Option<(f32, f32)> {
    use crate::fragment::{measure_content_intrinsic_widths, resolve_size_with_intrinsic, size_value_needs_intrinsic};
    use rix_css::values_math::CalcSizeBasis;
    let fg = style.flex_grid.as_deref();
    let fb_size = fg.and_then(|f| f.flex_basis_size.as_ref());
    let main_sz = if is_row { &style.box_model.width } else { &style.box_model.height };
    let fb_is_auto = fg.is_none_or(|f| matches!(f.flex_basis, LengthPercentageAuto::Auto));
    if fb_size.is_none() && !(fb_is_auto && (size_value_needs_intrinsic(main_sz) || matches!(main_sz, SizeValue::Stretch))) {
        return None;
    }
    let metrics = BoxMetrics::resolve_with_eval(style, space, *eval);
    let is_bb = style.box_sizing == BoxSizingMode::BorderBox;
    let (pb_m, margin_m, main_avail) = if is_row {
        (metrics.padding.horizontal() + metrics.border.horizontal(), metrics.margin.horizontal(), Some(c_avail_w))
    } else {
        (metrics.padding.vertical() + metrics.border.vertical(), metrics.margin.vertical(), c_avail_h)
    };
    let (min_c, max_c) = if is_row {
        measure_content_intrinsic_widths(ctx, cid, space.with_available(c_avail_w, c_avail_h))
    } else {
        let h = measure_content_intrinsic_height(ctx, cid, space.with_available(c_avail_w, None));
        (h, h)
    };
    let avail_c = main_avail.map_or(max_c, |a| (a - margin_m - pb_m).max(0.0));
    let stf_c = max_c.min(avail_c).max(min_c);
    let resolve_main_auto = || -> f32 {
        let m_intr = |b: &CalcSizeBasis| match b {
            CalcSizeBasis::MinContent => min_c,
            CalcSizeBasis::MaxContent | CalcSizeBasis::Content => max_c,
            CalcSizeBasis::FitContent => stf_c,
            CalcSizeBasis::Stretch => avail_c,
            _ => max_c,
        };
        resolve_size_with_intrinsic(main_sz, main_avail, pb_m, margin_m, is_bb, eval, m_intr).unwrap_or(max_c)
    };
    let content_px = if let Some(sz) = fb_size {
        match sz {
            SizeValue::Content => Some(max_c),
            other => resolve_size_with_intrinsic(other, main_avail, pb_m, margin_m, is_bb, eval, |b| match b {
                CalcSizeBasis::MinContent => min_c,
                CalcSizeBasis::MaxContent | CalcSizeBasis::Content => max_c,
                CalcSizeBasis::FitContent => stf_c,
                CalcSizeBasis::Stretch => avail_c,
                CalcSizeBasis::Auto => resolve_main_auto(),
                _ => max_c,
            }),
        }
    } else {
        Some(resolve_main_auto())
    }?;
    let basis = if is_bb { content_px + pb_m } else { content_px }.max(0.0);
    let main_ov = if is_row { style.overflow.x } else { style.overflow.y };
    let auto_min = if main_ov == OverflowMode::Visible {
        let spec_c = if is_row { metrics.specified_width } else { metrics.specified_height };
        let c_min = spec_c.map_or(min_c, |s| min_c.min(s));
        if is_bb { c_min + pb_m } else { c_min }.max(0.0)
    } else {
        0.0
    };
    Some((basis, auto_min))
}

fn measure_content_intrinsic_height(
    ctx: &mut LayoutContext<'_>,
    box_id: BoxId,
    space: ConstraintSpace,
) -> f32 {
    let b = ctx.tree.get(box_id);
    if b.children.is_empty() {
        return 0.0;
    }
    let has_block = b
        .children
        .iter()
        .any(|&c| ctx.tree.get(c).is_block_level() || ctx.tree.get(c).is_float);
    let saved_oof = ctx.pending_oof.len();
    let res = if has_block {
        let mut h = 0.0f32;
        for cid in b.children.clone() {
            if ctx.tree.get(cid).is_out_of_flow {
                continue;
            }
            let frag = layout_box(ctx, cid, space);
            h += frag.border_rect.size.height + frag.margin.vertical();
        }
        h
    } else {
        let floats = crate::bfc::FloatContext::new();
        crate::ifc::layout_ifc(ctx, box_id, space, &floats, Point::ZERO).height
    };
    ctx.pending_oof.truncate(saved_oof);
    res
}

#[inline]
fn child_dom_key(tree: &crate::box_tree::BoxTree, cid: BoxId) -> u64 {
    tree.get(cid)
        .dom_node
        .map_or(u64::from(cid.0) | (1u64 << 63), |n| usize::from(n) as u64)
}

fn measure_taffy_leaf(
    ctx: &mut LayoutContext<'_>,
    box_id: BoxId,
    input: LayoutInput,
    container_space: ConstraintSpace,
) -> LayoutOutput {
    if let (Some(w), Some(h)) = (input.known_dimensions.width, input.known_dimensions.height) {
        return LayoutOutput::from_outer_size(tp::Size {
            width: w.max(0.0),
            height: h.max(0.0),
        });
    }

    let (min_w, max_w) = measure_intrinsic_widths(ctx, box_id, container_space);
    let resolved_w = match input.known_dimensions.width {
        Some(w) => w.max(0.0),
        None => match input.available_space.width {
            AvailableSpace::MinContent => min_w,
            AvailableSpace::MaxContent => max_w,
            AvailableSpace::Definite(avail) => {
                max_w.min(avail.max(0.0)).max(min_w.min(avail.max(0.0)))
            }
        },
    };

    let resolved_h = match input.known_dimensions.height {
        Some(h) => h.max(0.0),
        None if matches!(input.axis, taffy::RequestedAxis::Horizontal) => 0.0,
        None => {
            let saved_oof = ctx.pending_oof.len();
            let sub_space = container_space.with_available(resolved_w, None);
            let frag = layout_box(ctx, box_id, sub_space);
            ctx.pending_oof.truncate(saved_oof);
            frag.border_rect.size.height
        }
    };

    LayoutOutput::from_outer_size(tp::Size {
        width: resolved_w.max(0.0),
        height: resolved_h.max(0.0),
    })
}

fn has_percent_height_direct_child(tree: &crate::box_tree::BoxTree, container_id: BoxId) -> bool {
    let b = tree.get(container_id);
    b.children.iter().any(|&cid| {
        let h = &tree.get(cid).style.box_model.height;
        matches!(
            h,
            SizeValue::Length(LengthPercentage::Percent(_) | LengthPercentage::Calc(_))
        )
    })
}

fn to_taffy_container_style(
    style: &ComputedStyle,
    fc: FormattingContextKind,
    space: &ConstraintSpace,
    eval: &EvalContext,
) -> tp::Style {
    let mut ts = to_taffy_item_style(style, space, eval, None, None, true);
    ts.display = match fc {
        FormattingContextKind::Grid => tp::Display::Grid,
        _ => tp::Display::Flex,
    };

    // Block-level flex/grid containers fill available width when width is `auto`
    // (border-box width = available_width - horizontal margins).
    if matches!(style.display, DisplayMode::Flex | DisplayMode::Grid)
        && matches!(style.box_model.width, SizeValue::Auto)
    {
        let metrics = BoxMetrics::resolve(style, space);
        let pb_w = metrics.padding.horizontal() + metrics.border.horizontal();
        let border_w = (space.available_width - metrics.margin.horizontal()).max(0.0);
        let fill_w = match style.box_sizing {
            BoxSizingMode::BorderBox => border_w,
            BoxSizingMode::ContentBox => (border_w - pb_w).max(0.0),
        };
        ts.size.width = tp::length(fill_w);
    }

    let fg = style
        .flex_grid
        .as_deref()
        .cloned()
        .unwrap_or_else(FlexGridStyle::default);
    ts.flex_direction = match fg.flex_direction {
        FlexDirectionMode::Row => tp::FlexDirection::Row,
        FlexDirectionMode::RowReverse => tp::FlexDirection::RowReverse,
        FlexDirectionMode::Column => tp::FlexDirection::Column,
        FlexDirectionMode::ColumnReverse => tp::FlexDirection::ColumnReverse,
    };
    ts.flex_wrap = match fg.flex_wrap {
        FlexWrapMode::NoWrap => tp::FlexWrap::NoWrap,
        FlexWrapMode::Wrap => tp::FlexWrap::Wrap,
        FlexWrapMode::WrapReverse => tp::FlexWrap::WrapReverse,
    };
    ts.align_items = map_align_items(fg.align_items);
    ts.align_content = map_align_content(fg.align_content);
    ts.justify_content = map_align_content(fg.justify_content);
    ts.justify_items = map_align_items(fg.justify_items);
    ts.gap = tp::Size {
        width: lp_to_taffy(&fg.column_gap, eval),
        height: lp_to_taffy(&fg.row_gap, eval),
    };

    ts.grid_template_columns = fg
        .grid_template_columns
        .iter()
        .map(|t| tp::GridTemplateComponent::Single(grid_track_to_taffy(t, eval)))
        .collect();
    ts.grid_template_rows = fg
        .grid_template_rows
        .iter()
        .map(|t| tp::GridTemplateComponent::Single(grid_track_to_taffy(t, eval)))
        .collect();
    ts.grid_auto_columns = fg
        .grid_auto_columns
        .iter()
        .map(|t| grid_track_to_taffy(t, eval))
        .collect();
    ts.grid_auto_rows = fg
        .grid_auto_rows
        .iter()
        .map(|t| grid_track_to_taffy(t, eval))
        .collect();
    ts.grid_auto_flow = match fg.grid_auto_flow {
        GridAutoFlowMode::Row => tp::GridAutoFlow::Row,
        GridAutoFlowMode::Column => tp::GridAutoFlow::Column,
        GridAutoFlowMode::RowDense => tp::GridAutoFlow::RowDense,
        GridAutoFlowMode::ColumnDense => tp::GridAutoFlow::ColumnDense,
    };
    ts
}

fn to_taffy_item_style(
    style: &ComputedStyle,
    space: &ConstraintSpace,
    eval: &EvalContext,
    parent_areas: Option<&GridTemplateAreasValue>,
    special_flex_basis: Option<(f32, f32)>,
    is_row: bool,
) -> tp::Style {
    let bm = &style.box_model;
    let metrics = BoxMetrics::resolve_with_eval(style, space, *eval);
    let mut ts = tp::Style {
        box_sizing: match style.box_sizing {
            BoxSizingMode::BorderBox => tp::BoxSizing::BorderBox,
            BoxSizingMode::ContentBox => tp::BoxSizing::ContentBox,
        },
        overflow: taffy::Point {
            x: map_overflow(style.overflow.x),
            y: map_overflow(style.overflow.y),
        },
        size: tp::Size {
            width: size_to_dimension(&bm.width, eval),
            height: size_to_dimension(&bm.height, eval),
        },
        min_size: tp::Size {
            width: size_to_lpa(&bm.min_width, eval),
            height: size_to_lpa(&bm.min_height, eval),
        },
        max_size: tp::Size {
            width: size_to_lpa(&bm.max_width, eval),
            height: size_to_lpa(&bm.max_height, eval),
        },
        margin: tp::Rect {
            left: lpa_to_taffy(&bm.margin.left, eval),
            right: lpa_to_taffy(&bm.margin.right, eval),
            top: lpa_to_taffy(&bm.margin.top, eval),
            bottom: lpa_to_taffy(&bm.margin.bottom, eval),
        },
        padding: tp::Rect {
            left: tp::length(metrics.padding.left),
            right: tp::length(metrics.padding.right),
            top: tp::length(metrics.padding.top),
            bottom: tp::length(metrics.padding.bottom),
        },
        border: tp::Rect {
            left: tp::length(metrics.border.left),
            right: tp::length(metrics.border.right),
            top: tp::length(metrics.border.top),
            bottom: tp::length(metrics.border.bottom),
        },
        ..tp::Style::default()
    };

    if let Some(fg) = &style.flex_grid {
        ts.flex_grow = fg.flex_grow.max(0.0);
        ts.flex_shrink = fg.flex_shrink.max(0.0);
        ts.flex_basis = lpa_to_dimension(&fg.flex_basis, eval);
        ts.align_self = map_align_items(fg.align_self);
        ts.justify_self = map_align_items(fg.justify_self);

        let (row_start, row_end) =
            resolve_grid_axis(&fg.grid_row_start, &fg.grid_row_end, parent_areas, true);
        let (col_start, col_end) = resolve_grid_axis(
            &fg.grid_column_start,
            &fg.grid_column_end,
            parent_areas,
            false,
        );
        ts.grid_row = tp::Line {
            start: row_start,
            end: row_end,
        };
        ts.grid_column = tp::Line {
            start: col_start,
            end: col_end,
        };
    }
    if let Some((sb, auto_min)) = special_flex_basis {
        ts.flex_basis = tp::length(sb);
        if is_row {
            ts.size.width = tp::auto();
            if matches!(bm.min_width, SizeValue::Auto) {
                ts.min_size.width = tp::length(auto_min);
            }
        } else {
            ts.size.height = tp::auto();
            if matches!(bm.min_height, SizeValue::Auto) {
                ts.min_size.height = tp::length(auto_min);
            }
        }
    }
    ts
}

fn resolve_grid_axis(
    start: &GridPlacementValue,
    end: &GridPlacementValue,
    parent_areas: Option<&GridTemplateAreasValue>,
    is_row: bool,
) -> (tp::GridPlacement, tp::GridPlacement) {
    // Handle `grid-area: <ident>` (`row_start = Named("foo"), ...`).
    if let GridPlacementValue::Named(name) = start {
        if let Some((rs, re, cs, ce)) = parent_areas.and_then(|a| a.find_area_lines(name.as_str()))
        {
            return if is_row {
                (
                    tp::GridPlacement::Line(rs.into()),
                    tp::GridPlacement::Line(re.into()),
                )
            } else {
                (
                    tp::GridPlacement::Line(cs.into()),
                    tp::GridPlacement::Line(ce.into()),
                )
            };
        }
    }
    (
        placement_to_taffy(start, parent_areas, is_row, true),
        placement_to_taffy(end, parent_areas, is_row, false),
    )
}

fn placement_to_taffy(
    p: &GridPlacementValue,
    parent_areas: Option<&GridTemplateAreasValue>,
    is_row: bool,
    is_start: bool,
) -> tp::GridPlacement {
    match p {
        GridPlacementValue::Auto => tp::GridPlacement::Auto,
        GridPlacementValue::Line { index, .. } => tp::GridPlacement::Line(
            ((*index).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16).into(),
        ),
        GridPlacementValue::Span { count, .. } => {
            let n = (*count).clamp(1, u32::from(u16::MAX)) as u16;
            tp::GridPlacement::Span(n)
        }
        GridPlacementValue::Named(name) => {
            if let Some((rs, re, cs, ce)) =
                parent_areas.and_then(|a| a.find_area_lines(name.as_str()))
            {
                let line = match (is_row, is_start) {
                    (true, true) => rs,
                    (true, false) => re,
                    (false, true) => cs,
                    (false, false) => ce,
                };
                tp::GridPlacement::Line(line.into())
            } else {
                tp::GridPlacement::Auto
            }
        }
    }
}

fn lp_to_taffy(lp: &LengthPercentage, eval: &EvalContext) -> tp::LengthPercentage {
    match lp {
        LengthPercentage::Zero => tp::length(0.0),
        LengthPercentage::Length(v, unit) => tp::length(unit.resolve_px(*v, eval)),
        LengthPercentage::Percent(p) => tp::percent(*p),
        LengthPercentage::Calc(expr) => tp::length(expr.resolve_px(eval.container_width_px, eval)),
    }
}

fn lpa_to_taffy(lpa: &LengthPercentageAuto, eval: &EvalContext) -> tp::LengthPercentageAuto {
    match lpa {
        LengthPercentageAuto::Auto => tp::auto(),
        LengthPercentageAuto::Value(lp) => lp_to_taffy(lp, eval).into(),
    }
}

fn lpa_to_dimension(lpa: &LengthPercentageAuto, eval: &EvalContext) -> tp::Dimension {
    match lpa {
        LengthPercentageAuto::Auto => tp::auto(),
        LengthPercentageAuto::Value(lp) => lp_to_taffy(lp, eval).into(),
    }
}

fn size_to_dimension(size: &SizeValue, eval: &EvalContext) -> tp::Dimension {
    match size {
        SizeValue::Length(lp) => lp_to_taffy(lp, eval).into(),
        _ => tp::auto(),
    }
}

fn size_to_lpa(size: &SizeValue, eval: &EvalContext) -> tp::LengthPercentageAuto {
    match size {
        SizeValue::Length(lp) => lp_to_taffy(lp, eval).into(),
        _ => tp::auto(),
    }
}

fn map_overflow(mode: OverflowMode) -> taffy::Overflow {
    match mode {
        OverflowMode::Visible => taffy::Overflow::Visible,
        OverflowMode::Hidden => taffy::Overflow::Hidden,
        OverflowMode::Clip => taffy::Overflow::Clip,
        OverflowMode::Scroll | OverflowMode::Auto => taffy::Overflow::Scroll,
    }
}

fn map_align_items(val: AlignValue) -> Option<tp::AlignItems> {
    match val.base() {
        AlignValue::Normal | AlignValue::Auto => None,
        AlignValue::Stretch => Some(tp::AlignItems::STRETCH),
        AlignValue::Start | AlignValue::SelfStart | AlignValue::Left => Some(tp::AlignItems::START),
        AlignValue::End | AlignValue::SelfEnd | AlignValue::Right => Some(tp::AlignItems::END),
        AlignValue::FlexStart => Some(tp::AlignItems::FLEX_START),
        AlignValue::FlexEnd => Some(tp::AlignItems::FLEX_END),
        AlignValue::Center => Some(tp::AlignItems::CENTER),
        AlignValue::Baseline | AlignValue::LastBaseline => Some(tp::AlignItems::BASELINE),
        _ => None,
    }
}

fn map_align_content(val: AlignValue) -> Option<tp::AlignContent> {
    match val.base() {
        AlignValue::Normal | AlignValue::Auto => None,
        AlignValue::Stretch => Some(tp::AlignContent::STRETCH),
        AlignValue::Start | AlignValue::SelfStart | AlignValue::Left => {
            Some(tp::AlignContent::START)
        }
        AlignValue::End | AlignValue::SelfEnd | AlignValue::Right => Some(tp::AlignContent::END),
        AlignValue::FlexStart => Some(tp::AlignContent::FLEX_START),
        AlignValue::FlexEnd => Some(tp::AlignContent::FLEX_END),
        AlignValue::Center => Some(tp::AlignContent::CENTER),
        AlignValue::SpaceBetween => Some(tp::AlignContent::SPACE_BETWEEN),
        AlignValue::SpaceAround => Some(tp::AlignContent::SPACE_AROUND),
        AlignValue::SpaceEvenly => Some(tp::AlignContent::SPACE_EVENLY),
        AlignValue::Baseline | AlignValue::LastBaseline => None,
        _ => None,
    }
}

fn grid_track_to_taffy(track: &GridTrackSize, eval: &EvalContext) -> tp::TrackSizingFunction {
    match track {
        GridTrackSize::Auto => tp::auto(),
        GridTrackSize::MinContent => tp::min_content(),
        GridTrackSize::MaxContent => tp::max_content(),
        GridTrackSize::Length(lp) => match lp {
            LengthPercentage::Percent(p) => tp::percent(*p),
            other => tp::length(other.resolve_px(eval.container_width_px, eval)),
        },
        GridTrackSize::Fr(f) => tp::fr(*f),
        GridTrackSize::FrMath(n) => tp::fr((n.eval_scalar(eval, None) as f32).max(0.0)),
        GridTrackSize::FitContent(lp) => match lp {
            LengthPercentage::Percent(p) => tp::fit_content(tp::percent(*p)),
            other => tp::fit_content(tp::length(other.resolve_px(eval.container_width_px, eval))),
        },
        GridTrackSize::MinMax(min_t, max_t) => {
            let min_fn = match min_t.as_ref() {
                GridTrackSize::MinContent => tp::min_content(),
                GridTrackSize::MaxContent => tp::max_content(),
                GridTrackSize::Length(lp) => match lp {
                    LengthPercentage::Percent(p) => tp::percent(*p),
                    other => tp::length(other.resolve_px(eval.container_width_px, eval)),
                },
                _ => tp::auto(),
            };
            let max_fn = match max_t.as_ref() {
                GridTrackSize::MinContent => tp::min_content(),
                GridTrackSize::MaxContent => tp::max_content(),
                GridTrackSize::Fr(f) => tp::fr(*f),
                GridTrackSize::FrMath(n) => tp::fr((n.eval_scalar(eval, None) as f32).max(0.0)),
                GridTrackSize::Length(lp) => match lp {
                    LengthPercentage::Percent(p) => tp::percent(*p),
                    other => tp::length(other.resolve_px(eval.container_width_px, eval)),
                },
                _ => tp::auto(),
            };
            tp::minmax(min_fn, max_fn)
        }
    }
}
