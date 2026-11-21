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
    FlexGridStyle, FlexWrapMode, GridAutoFlowMode, GridTrackSize, LengthPercentage,
    LengthPercentageAuto, PositionMode, SizeValue,
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

    let eval = space.eval_context(container_style.text.font_size_px);
    let root_taffy_style = to_taffy_container_style(&container_style, fc, &space, &eval);
    let root_style_hash = Arc::as_ptr(&container_style) as usize as u64
        ^ (u64::from(space.available_width.to_bits()) << 32);

    // Take the retained container out of `LayoutCache` temporarily so the measure closure can
    // borrow `ctx` mutably (including `LayoutCache` fragment cache for leaf subtrees!).
    let mut retained = ctx
        .cache
        .taffy_containers
        .remove(&container_key)
        .unwrap_or_else(|| RetainedTaffyContainer::new(root_taffy_style.clone()));

    reconcile_retained_tree(
        &mut retained,
        ctx.tree,
        &ordered_items,
        root_taffy_style,
        root_style_hash,
        &space,
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

    let mut placed_children = Vec::with_capacity(item_layouts.len() + oof_children.len());
    for (cid, item_layout) in item_layouts {
        let item_w = item_layout.size.width.max(0.0);
        let item_h = item_layout.size.height.max(0.0);
        let item_space = space.with_available(item_w, Some(item_h));
        let mut frag = layout_box(ctx, cid, item_space);
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
        frag.compute_overflow_rect();
        placed_children.push(frag);
    }

    // Place out-of-flow (`position: absolute | fixed`) children against container padding box.
    for oof_id in oof_children {
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
                (border_w - metrics.border.horizontal()).max(0.0),
                (border_h - metrics.border.vertical()).max(0.0),
                metrics.border.left,
                metrics.border.top,
            )
        };
        let oof_space = space.with_available(cb_w, Some(cb_h));
        let mut frag = layout_box(ctx, oof_id, oof_space);
        let oof_eval = space.eval_context(oof_box.style.text.font_size_px);
        let inset = &oof_box.style.box_model.inset;
        let x = match (&inset.left, &inset.right) {
            (LengthPercentageAuto::Value(l), _) => base_x + l.resolve_px(cb_w, &oof_eval),
            (LengthPercentageAuto::Auto, LengthPercentageAuto::Value(r)) => {
                base_x + cb_w - r.resolve_px(cb_w, &oof_eval) - frag.border_rect.size.width
            }
            _ => base_x,
        };
        let y = match (&inset.top, &inset.bottom) {
            (LengthPercentageAuto::Value(t), _) => base_y + t.resolve_px(cb_h, &oof_eval),
            (LengthPercentageAuto::Auto, LengthPercentageAuto::Value(b)) => {
                base_y + cb_h - b.resolve_px(cb_h, &oof_eval) - frag.border_rect.size.height
            }
            _ => base_y,
        };
        frag.border_rect.origin = Point::new(x, y);
        placed_children.push(frag);
    }

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
        overflow_rect: Rect::new(0.0, 0.0, border_w, border_h),
        kind: FragmentKind::Box,
        children: Arc::new(placed_children),
        is_float: ctx.tree.get(container_id).is_float,
        is_out_of_flow: ctx.tree.get(container_id).is_out_of_flow,
        collapsed_margin_top: metrics.margin.top,
        collapsed_margin_bottom: metrics.margin.bottom,
    };
    fragment.compute_overflow_rect();
    fragment
}

fn reconcile_retained_tree(
    retained: &mut RetainedTaffyContainer,
    tree: &crate::box_tree::BoxTree,
    ordered_items: &[BoxId],
    root_style: tp::Style,
    root_hash: u64,
    space: &ConstraintSpace,
) {
    if retained.root_hash != root_hash {
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
        for (entry, &cid) in retained.children.iter_mut().zip(ordered_items.iter()) {
            let child_box = tree.get(cid);
            if let Some(leaf_ctx) = retained.taffy.get_node_context_mut(entry.taffy_node) {
                leaf_ctx.box_id = cid;
            }
            if child_box.subtree_dirty || entry.subtree_hash != child_box.subtree_hash {
                let eval = space.eval_context(child_box.style.text.font_size_px);
                let item_style = to_taffy_item_style(&child_box.style, space, &eval);
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
    for &cid in ordered_items {
        let child_box = tree.get(cid);
        let eval = space.eval_context(child_box.style.text.font_size_px);
        let item_style = to_taffy_item_style(&child_box.style, space, &eval);
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
        None => {
            let sub_space = container_space.with_available(resolved_w, None);
            let frag = layout_box(ctx, box_id, sub_space);
            frag.border_rect.size.height
        }
    };

    LayoutOutput::from_outer_size(tp::Size {
        width: resolved_w.max(0.0),
        height: resolved_h.max(0.0),
    })
}

fn to_taffy_container_style(
    style: &ComputedStyle,
    fc: FormattingContextKind,
    space: &ConstraintSpace,
    eval: &EvalContext,
) -> tp::Style {
    let mut ts = to_taffy_item_style(style, space, eval);
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
) -> tp::Style {
    let bm = &style.box_model;
    let metrics = BoxMetrics::resolve(style, space);
    let mut ts = tp::Style {
        box_sizing: match style.box_sizing {
            BoxSizingMode::BorderBox => tp::BoxSizing::BorderBox,
            BoxSizingMode::ContentBox => tp::BoxSizing::ContentBox,
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
    }
    ts
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

fn map_align_items(val: AlignValue) -> Option<tp::AlignItems> {
    match val {
        AlignValue::Normal | AlignValue::Auto => None,
        AlignValue::Stretch => Some(tp::AlignItems::STRETCH),
        AlignValue::Start => Some(tp::AlignItems::START),
        AlignValue::End => Some(tp::AlignItems::END),
        AlignValue::FlexStart => Some(tp::AlignItems::FLEX_START),
        AlignValue::FlexEnd => Some(tp::AlignItems::FLEX_END),
        AlignValue::Center => Some(tp::AlignItems::CENTER),
        AlignValue::Baseline => Some(tp::AlignItems::BASELINE),
        _ => None,
    }
}

fn map_align_content(val: AlignValue) -> Option<tp::AlignContent> {
    match val {
        AlignValue::Normal | AlignValue::Auto => None,
        AlignValue::Stretch => Some(tp::AlignContent::STRETCH),
        AlignValue::Start => Some(tp::AlignContent::START),
        AlignValue::End => Some(tp::AlignContent::END),
        AlignValue::FlexStart => Some(tp::AlignContent::FLEX_START),
        AlignValue::FlexEnd => Some(tp::AlignContent::FLEX_END),
        AlignValue::Center => Some(tp::AlignContent::CENTER),
        AlignValue::SpaceBetween => Some(tp::AlignContent::SPACE_BETWEEN),
        AlignValue::SpaceAround => Some(tp::AlignContent::SPACE_AROUND),
        AlignValue::SpaceEvenly => Some(tp::AlignContent::SPACE_EVENLY),
        AlignValue::Baseline => None,
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
