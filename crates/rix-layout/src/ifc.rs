//! Inline Formatting Context (IFC): HarfBuzz shaping via shared `rix-text`
//! cache, line box construction around active floats, `inline-block` and
//! replaced atomic inlines, inline box padding/border splitting across wrapped
//! lines, `text-align`, `vertical-align`, `text-overflow: ellipsis`, and
//! `white-space` modes.

use std::sync::Arc;

use rix_core::{Point, Rect};
use rix_css::{
    ComputedStyle, DirectionMode, EvalContext, FontStyleMode, LineBreakMode, LineHeightValue,
    OverflowMode, OverflowWrapMode, TextAlignMode, TextOverflowMode, TextWrapMode,
    VerticalAlignMode, WhiteSpaceCollapseMode, WhiteSpaceTrim, WordBreakMode,
};
use rix_text::{FontDatabase, FontDescriptor, FontFaceSet, FontId, FontStyle, FontWeight};

use crate::bfc::{apply_relative_offset, FloatContext};
use crate::box_tree::{BoxId, BoxKind, ReplacedKind};
use crate::fragment::{
    layout_box, measure_intrinsic_widths, BoxMetrics, ConstraintSpace, EdgeInsets, Fragment,
    FragmentKind, LayoutContext, PositionedGlyph, ShapedTextFragment,
};

/// Result of laying out an Inline Formatting Context inside a block container.
pub struct IfcLayoutOutput {
    pub fragments: Vec<Fragment>,
    pub height: f32,
}

/// Resolves the best [`FontId`] for `style` using `@font-face` web fonts first,
/// then system [`FontDatabase`], then default fallback.
#[must_use]
pub fn resolve_font_for_style(
    font_db: &FontDatabase,
    web_fonts: Option<&FontFaceSet>,
    style: &ComputedStyle,
) -> FontId {
    let weight = FontWeight(style.text.font_weight.round().clamp(100.0, 900.0) as u16);
    let font_style = match style.text.font_style {
        FontStyleMode::Normal => FontStyle::Normal,
        FontStyleMode::Italic => FontStyle::Italic,
        FontStyleMode::Oblique(_) => FontStyle::Oblique,
    };

    for family in &style.text.font_family {
        if let Some(wf) = web_fonts {
            if let Some(id) =
                wf.match_face_for_char(font_db, family.as_str(), weight, font_style, 'a')
            {
                return id;
            }
        }
        let desc = FontDescriptor::new(family.as_str())
            .with_weight(weight)
            .with_style(font_style);
        if let Some(id) = font_db.match_family(family.as_str(), &desc) {
            return id;
        }
    }

    let fallback = FontDescriptor::new("sans-serif")
        .with_weight(weight)
        .with_style(font_style);
    font_db
        .match_family("sans-serif", &fallback)
        .or_else(|| font_db.match_font(&fallback))
        .unwrap_or(FontId(0))
}

/// Resolves computed `line-height` to pixels.
#[must_use]
pub fn resolve_line_height_px(style: &ComputedStyle, eval: EvalContext) -> f32 {
    let font_size = style.text.font_size_px.max(1.0);
    match &style.text.line_height {
        LineHeightValue::Normal => font_size * 1.2,
        LineHeightValue::Number(n) => (font_size * *n).max(0.0),
        LineHeightValue::Length(lp) => lp.resolve_px(font_size, &eval).max(0.0),
    }
}

fn line_box_metrics(
    font_db: &FontDatabase,
    font_id: FontId,
    style: &ComputedStyle,
    eval: EvalContext,
) -> (f32, f32, f32) {
    let font_size = style.text.font_size_px.max(1.0);
    let (raw_asc, raw_desc) = font_ascent_descent(font_db, font_id, font_size);
    let lh = match &style.text.line_height {
        LineHeightValue::Normal => font_db
            .get_font(font_id)
            .map(|f| f.metrics.line_height_px(font_size))
            .filter(|h| *h > 0.0)
            .unwrap_or(font_size * 1.2),
        _ => resolve_line_height_px(style, eval),
    };
    if matches!(style.text.line_height, LineHeightValue::Normal) {
        (raw_asc, raw_desc, lh)
    } else {
        // CSS 2.1 §10.8.1: leading = line-height - (ascent + descent), split equally top/bottom.
        let half = (lh - (raw_asc + raw_desc)) * 0.5;
        ((raw_asc + half).max(0.0), (raw_desc + half).max(0.0), lh)
    }
}

#[derive(Clone)]
struct ActiveInline {
    box_id: BoxId,
    dom_node: Option<rix_dom::NodeId>,
    style: Arc<ComputedStyle>,
    padding: EdgeInsets,
    border: EdgeInsets,
    margin: EdgeInsets,
}

enum InlineItem {
    PushInline(ActiveInline),
    PopInline(BoxId),
    OutOfFlow(BoxId),
    Word {
        text: String,
        style: Arc<ComputedStyle>,
        box_id: BoxId,
        is_space: bool,
        is_preserved_space: bool,
        is_tab: bool,
        can_break_before: bool,
        can_emergency_break_before: bool,
        inline_stack: Vec<ActiveInline>,
    },
    HardBreak,
    Atomic {
        box_id: BoxId,
        style: Arc<ComputedStyle>,
        can_break_before: bool,
        inline_stack: Vec<ActiveInline>,
    },
}

struct LinePiece {
    box_id: BoxId,
    x: f32,
    width: f32,
    height: f32,
    ascent: f32,
    descent: f32,
    vertical_align: VerticalAlignMode,
    is_space: bool,
    is_preserved_space: bool,
    hangs_at_line_end: bool,
    can_break_before: bool,
    can_emergency_break_before: bool,
    inline_stack: Vec<ActiveInline>,
    payload: LinePiecePayload,
}

enum LinePiecePayload {
    Text {
        font_id: FontId,
        font_size: f32,
        style: Arc<ComputedStyle>,
        text: String,
        glyphs: Vec<PositionedGlyph>,
    },
    Atomic(Box<Fragment>),
}

/// Computes `(block_thickness_width, max_inline_advance_height)` for a vertical writing-mode
/// inline formatting context constrained to `inline_avail` along its vertical inline axis.
pub fn measure_vertical_ifc_dims(
    ctx: &mut LayoutContext<'_>,
    container_id: BoxId,
    space: ConstraintSpace,
    inline_avail: f32,
) -> (f32, f32) {
    let container_style = Arc::clone(&ctx.tree.get(container_id).style);
    let eval = ctx.eval_context_for(&container_style, &space);
    let strut_font_id = ctx.resolve_font(&container_style);
    let (_, _, strut_line_h) = line_box_metrics(ctx.font_db, strut_font_id, &container_style, eval);
    let allow_wrap = container_style.text.text_wrap_mode == TextWrapMode::Wrap;

    let items = collect_inline_items(ctx, container_id, space);
    if items.is_empty() {
        return (0.0, 0.0);
    }

    let mut total_block = 0.0f32;
    let mut max_inline = 0.0f32;
    let mut cur_inline = 0.0f32;
    let mut cur_line_block = 0.0f32;
    let mut pending_space = 0.0f32;
    let mut has_content = false;

    for item in &items {
        match item {
            InlineItem::PushInline(info) => {
                let lead = info.border.top + info.padding.top + info.margin.top;
                if lead > 0.0 {
                    cur_inline += pending_space + lead;
                    pending_space = 0.0;
                    has_content = true;
                    cur_line_block = cur_line_block.max(strut_line_h);
                }
            }
            InlineItem::PopInline(_) | InlineItem::OutOfFlow(_) => {}
            InlineItem::Word {
                text,
                style,
                is_space,
                ..
            } => {
                let font_id = ctx.resolve_font(style);
                let font_size = style.text.font_size_px.max(1.0);
                let adv = measure_shaped_width(ctx, text, font_id, font_size, style);
                let w_eval = ctx.eval_context_for(style, &space);
                let (_, _, lh) = line_box_metrics(ctx.font_db, font_id, style, w_eval);
                if *is_space {
                    if has_content {
                        pending_space += adv;
                    }
                } else {
                    if allow_wrap
                        && has_content
                        && inline_avail.is_finite()
                        && cur_inline + pending_space + adv > inline_avail + 0.05
                    {
                        total_block += cur_line_block.max(strut_line_h);
                        max_inline = max_inline.max(cur_inline);
                        cur_inline = 0.0;
                        cur_line_block = 0.0;
                        pending_space = 0.0;
                    }
                    cur_inline += pending_space + adv;
                    pending_space = 0.0;
                    has_content = true;
                    cur_line_block = cur_line_block.max(lh.max(strut_line_h));
                }
            }
            InlineItem::HardBreak => {
                total_block += cur_line_block.max(strut_line_h);
                max_inline = max_inline.max(cur_inline);
                cur_inline = 0.0;
                cur_line_block = 0.0;
                pending_space = 0.0;
                has_content = false;
            }
            InlineItem::Atomic { box_id, .. } => {
                let saved = ctx.pending_oof.len();
                let frag = layout_box(ctx, *box_id, space);
                ctx.pending_oof.truncate(saved);
                let adv = frag.border_rect.size.height + frag.margin.vertical();
                let blk = frag.border_rect.size.width + frag.margin.horizontal();
                if allow_wrap
                    && has_content
                    && inline_avail.is_finite()
                    && cur_inline + pending_space + adv > inline_avail + 0.05
                {
                    total_block += cur_line_block.max(strut_line_h);
                    max_inline = max_inline.max(cur_inline);
                    cur_inline = 0.0;
                    cur_line_block = 0.0;
                    pending_space = 0.0;
                }
                cur_inline += pending_space + adv;
                pending_space = 0.0;
                has_content = true;
                cur_line_block = cur_line_block.max(blk.max(strut_line_h));
            }
        }
    }
    if has_content {
        total_block += cur_line_block.max(strut_line_h);
        max_inline = max_inline.max(cur_inline);
    }
    (total_block, max_inline)
}

/// Computes `(min_content_width, max_content_width)` of the inline content inside `container_id`.
/// For vertical writing modes, physical width is the block axis (summed wrapped line thickness).
pub fn measure_ifc_min_max_content(
    ctx: &mut LayoutContext<'_>,
    container_id: BoxId,
    space: ConstraintSpace,
) -> (f32, f32) {
    if ctx
        .tree
        .get(container_id)
        .style
        .text
        .writing_mode
        .is_vertical()
    {
        let inline_avail = space.available_height.unwrap_or(f32::INFINITY);
        let (blk_w, _) = measure_vertical_ifc_dims(ctx, container_id, space, inline_avail);
        return (blk_w, blk_w);
    }
    measure_ifc_inline_min_max(ctx, container_id, space)
}

/// Computes `(min_content_inline, max_content_inline)` along the text's own inline progression axis.
pub fn measure_ifc_inline_min_max(
    ctx: &mut LayoutContext<'_>,
    container_id: BoxId,
    space: ConstraintSpace,
) -> (f32, f32) {
    let container_overflow_anywhere =
        ctx.tree.get(container_id).style.text.overflow_wrap == OverflowWrapMode::Anywhere;
    let items = collect_inline_items(ctx, container_id, space);
    let mut min_w = 0.0f32;
    let mut max_w = 0.0f32;
    let mut current_line_w = 0.0f32;
    let mut current_unbreakable_w = 0.0f32;
    let mut pending_space_w = 0.0f32;
    let mut seen_non_space = false;

    for item in &items {
        match item {
            InlineItem::PushInline(info) => {
                let left = info.border.left + info.padding.left + info.margin.left;
                if left > 0.0 {
                    current_line_w += pending_space_w + left;
                    current_unbreakable_w += left;
                    pending_space_w = 0.0;
                    seen_non_space = true;
                    max_w = max_w.max(current_line_w);
                    min_w = min_w.max(current_unbreakable_w);
                }
            }
            InlineItem::PopInline(_) => {}
            InlineItem::Word {
                text,
                style,
                is_space,
                is_preserved_space,
                is_tab,
                can_break_before,
                can_emergency_break_before,
                ..
            } => {
                let font_id = ctx.resolve_font(style);
                let font_size = style.text.font_size_px.max(1.0);
                let w = if *is_tab {
                    let w_eval = ctx.eval_context_for(style, &space);
                    compute_tab_advance(ctx, font_id, font_size, style, w_eval, current_line_w)
                } else {
                    measure_shaped_width(ctx, text, font_id, font_size, style)
                };
                let breaks_min_content = *can_break_before
                    || (*can_emergency_break_before
                        && (container_overflow_anywhere
                            || style.text.overflow_wrap == OverflowWrapMode::Anywhere));
                if *is_space {
                    if *is_preserved_space {
                        let hangs = style.text.white_space_collapse
                            == WhiteSpaceCollapseMode::Preserve
                            && style.text.text_wrap_mode == TextWrapMode::Wrap;
                        if hangs {
                            // `pre-wrap`: preserved spaces hang at line end (not counted in min-content
                            // or trailing max-content), but count between non-space tokens.
                            if seen_non_space {
                                pending_space_w += w;
                            }
                            current_unbreakable_w = 0.0;
                        } else {
                            // `pre` (`preserve nowrap`) or `break-spaces`: preserved spaces do NOT hang.
                            current_line_w += pending_space_w + w;
                            pending_space_w = 0.0;
                            seen_non_space = true;
                            max_w = max_w.max(current_line_w);
                            if style.text.text_wrap_mode == TextWrapMode::Nowrap {
                                current_unbreakable_w += w;
                                min_w = min_w.max(current_unbreakable_w);
                            } else {
                                // `break-spaces`: each space may wrap after itself, contributing at least
                                // one space advance to `min-content` (CSS Text L4 §3).
                                if breaks_min_content {
                                    current_unbreakable_w = w;
                                } else {
                                    current_unbreakable_w += w;
                                }
                                min_w = min_w.max(current_unbreakable_w);
                                current_unbreakable_w = 0.0;
                            }
                        }
                    } else if seen_non_space {
                        pending_space_w += w;
                        if style.text.text_wrap_mode == TextWrapMode::Nowrap {
                            current_unbreakable_w += w;
                        } else {
                            current_unbreakable_w = 0.0;
                        }
                    }
                } else {
                    if breaks_min_content {
                        current_unbreakable_w = w;
                    } else {
                        current_unbreakable_w += w;
                    }
                    min_w = min_w.max(current_unbreakable_w);
                    current_line_w += pending_space_w + w;
                    pending_space_w = 0.0;
                    seen_non_space = true;
                    max_w = max_w.max(current_line_w);
                }
            }
            InlineItem::HardBreak => {
                max_w = max_w.max(current_line_w);
                current_line_w = 0.0;
                current_unbreakable_w = 0.0;
                pending_space_w = 0.0;
                seen_non_space = false;
            }
            InlineItem::Atomic {
                box_id,
                can_break_before,
                ..
            } => {
                let b = ctx.tree.get(*box_id);
                let m = BoxMetrics::resolve(&b.style, &space);
                let (cmin, cmax) = measure_intrinsic_widths(ctx, *box_id, space);
                if *can_break_before {
                    current_unbreakable_w = cmin + m.margin.horizontal();
                } else {
                    current_unbreakable_w += cmin + m.margin.horizontal();
                }
                min_w = min_w.max(current_unbreakable_w);
                current_line_w += pending_space_w + cmax + m.margin.horizontal();
                pending_space_w = 0.0;
                seen_non_space = true;
                max_w = max_w.max(current_line_w);
            }
            InlineItem::OutOfFlow(_) => {}
        }
    }
    (min_w, max_w.max(min_w))
}

/// Lays out all inline children of `container_id` into positioned `LineBox` fragments.
pub fn layout_ifc(
    ctx: &mut LayoutContext<'_>,
    container_id: BoxId,
    space: ConstraintSpace,
    floats: &FloatContext,
    content_origin: Point,
) -> IfcLayoutOutput {
    let container_style = Arc::clone(&ctx.tree.get(container_id).style);
    let container_width = space.available_width.max(0.0);
    let eval = ctx.eval_context_for(&container_style, &space);
    let strut_font_id = ctx.resolve_font(&container_style);
    let (strut_ascent, strut_descent, strut_line_h) =
        line_box_metrics(ctx.font_db, strut_font_id, &container_style, eval);

    let items = collect_inline_items(ctx, container_id, space);
    if items.is_empty() {
        return IfcLayoutOutput {
            fragments: Vec::new(),
            height: 0.0,
        };
    }

    let allow_wrap = container_style.text.text_wrap_mode == TextWrapMode::Wrap;
    let want_ellipsis = container_style.box_model.text_overflow == TextOverflowMode::Ellipsis
        && container_style.overflow.x != OverflowMode::Visible
        && !allow_wrap;

    let mut lines: Vec<FinishedLine> = Vec::new();
    let mut cur_pieces: Vec<LinePiece> = Vec::new();
    let mut cursor_y = 0.0f32;
    let mut pending_left_edge = 0.0f32;
    let mut seen_inline_starts: Vec<BoxId> = Vec::new();

    let text_indent_px = container_style
        .text
        .text_indent
        .resolve_px(container_width, &eval);
    let indent_hanging = container_style.text.text_indent_hanging;
    let indent_each_line = container_style.text.text_indent_each_line;
    let first_line_indent = if indent_hanging { 0.0 } else { text_indent_px };
    let wrapped_line_indent = if indent_hanging { text_indent_px } else { 0.0 };
    let after_break_indent = if indent_hanging {
        if indent_each_line {
            0.0
        } else {
            text_indent_px
        }
    } else if indent_each_line {
        text_indent_px
    } else {
        0.0
    };

    let (mut band_left, mut band_right) =
        floats.available_band(cursor_y, strut_line_h, container_width);
    let mut cur_x = first_line_indent;

    let item_count = items.len();
    for (idx, item) in items.into_iter().enumerate() {
        let is_last_item = idx + 1 == item_count;
        match item {
            InlineItem::PushInline(info) => {
                pending_left_edge += info.border.left + info.padding.left + info.margin.left;
            }
            InlineItem::PopInline(bid) => {
                if let Some(last) = cur_pieces.last_mut() {
                    if let Some(info) = last.inline_stack.iter().find(|i| i.box_id == bid) {
                        let right = info.border.right + info.padding.right + info.margin.right;
                        cur_x += right;
                    }
                }
            }
            InlineItem::OutOfFlow(oof_id) => {
                let is_fixed = ctx.tree.get(oof_id).style.position == rix_css::PositionMode::Fixed;
                let is_rtl = container_style.text.direction == rix_css::DirectionMode::Rtl;
                let is_vert = container_style.text.writing_mode.is_vertical();
                let x_end = (!is_vert && is_rtl)
                    || matches!(
                        container_style.text.writing_mode,
                        rix_css::WritingMode::VerticalRl | rix_css::WritingMode::SidewaysRl
                    );
                let sx = if x_end {
                    content_origin.x + band_right - cur_x - pending_left_edge
                } else {
                    content_origin.x + band_left + cur_x + pending_left_edge
                };
                ctx.pending_oof.push(crate::position::PendingOof {
                    box_id: oof_id,
                    static_pos: Point::new(sx, content_origin.y + cursor_y),
                    is_fixed,
                    static_x_is_end: x_end,
                    static_y_is_end: false,
                    flex_hint: None,
                    grid_hint: None,
                });
            }
            InlineItem::HardBreak => {
                let line = finish_line(
                    ctx,
                    &mut cur_pieces,
                    band_left,
                    band_right,
                    cursor_y,
                    strut_ascent,
                    strut_descent,
                    strut_line_h,
                    &container_style,
                    true,
                    want_ellipsis,
                    &mut seen_inline_starts,
                );
                cursor_y += line.height;
                lines.push(line);
                let (bl, br) = floats.available_band(cursor_y, strut_line_h, container_width);
                band_left = bl;
                band_right = br;
                cur_x = after_break_indent;
                pending_left_edge = 0.0;
            }
            InlineItem::Word {
                text,
                style,
                box_id,
                is_space,
                is_preserved_space,
                is_tab,
                can_break_before,
                can_emergency_break_before,
                inline_stack,
            } => {
                // Skip leading collapsible spaces at line start.
                if is_space && !is_preserved_space && cur_pieces.is_empty() {
                    continue;
                }

                let font_id = ctx.resolve_font(&style);
                let font_size = style.text.font_size_px.max(1.0);
                let word_eval = ctx.eval_context_for(&style, &space);
                let (glyphs, width) = if is_tab {
                    let adv = compute_tab_advance(
                        ctx,
                        font_id,
                        font_size,
                        &style,
                        word_eval,
                        cur_x + pending_left_edge,
                    );
                    (Vec::new(), adv)
                } else {
                    shape_word_glyphs(ctx, &text, font_id, font_size, &style)
                };
                let (asc, desc, lh) = line_box_metrics(ctx.font_db, font_id, &style, word_eval);

                let hangs_at_line_end = is_space
                    && is_preserved_space
                    && style.text.white_space_collapse == WhiteSpaceCollapseMode::Preserve
                    && style.text.text_wrap_mode == TextWrapMode::Wrap;
                let band_w = (band_right - band_left).max(0.0);
                let item_allows_wrap =
                    allow_wrap || style.text.text_wrap_mode == TextWrapMode::Wrap;
                // Preserved `break-spaces` can wrap when overflowing; collapsible and hanging
                // `pre-wrap` spaces do not trigger a wrap before themselves.
                let triggers_overflow_wrap = (!is_space
                    || (is_preserved_space
                        && style.text.white_space_collapse == WhiteSpaceCollapseMode::BreakSpaces))
                    && !cur_pieces.is_empty()
                    && cur_x + pending_left_edge + width > band_w + 0.05;

                if item_allows_wrap && triggers_overflow_wrap {
                    if can_break_before {
                        let line = finish_line(
                            ctx,
                            &mut cur_pieces,
                            band_left,
                            band_right,
                            cursor_y,
                            strut_ascent,
                            strut_descent,
                            strut_line_h,
                            &container_style,
                            false,
                            want_ellipsis,
                            &mut seen_inline_starts,
                        );
                        cursor_y += line.height;
                        lines.push(line);
                        let (bl, br) = floats.available_band(cursor_y, lh, container_width);
                        band_left = bl;
                        band_right = br;
                        cur_x = wrapped_line_indent;
                    } else if let Some(split_idx) = cur_pieces
                        .iter()
                        .rposition(|p| p.can_break_before)
                        .filter(|&i| i > 0)
                    {
                        // Backtrack to the last valid soft-wrap opportunity so an unbreakable
                        // cluster spanning inline boxes (e.g. `文<span>々</span>`) wraps together.
                        let mut carried = cur_pieces.split_off(split_idx);
                        let line = finish_line(
                            ctx,
                            &mut cur_pieces,
                            band_left,
                            band_right,
                            cursor_y,
                            strut_ascent,
                            strut_descent,
                            strut_line_h,
                            &container_style,
                            false,
                            want_ellipsis,
                            &mut seen_inline_starts,
                        );
                        cursor_y += line.height;
                        lines.push(line);
                        let (bl, br) = floats.available_band(cursor_y, lh, container_width);
                        band_left = bl;
                        band_right = br;
                        let shift = carried.first().map_or(0.0, |p| p.x) - wrapped_line_indent;
                        for cp in &mut carried {
                            cp.x -= shift;
                        }
                        cur_x = carried
                            .last()
                            .map_or(wrapped_line_indent, |p| p.x + p.width);
                        cur_pieces = carried;
                    } else if can_emergency_break_before {
                        // Emergency break between `nowrap` spans under `overflow-wrap: break-word`
                        // only after confirming no earlier normal soft-wrap opportunity exists.
                        let line = finish_line(
                            ctx,
                            &mut cur_pieces,
                            band_left,
                            band_right,
                            cursor_y,
                            strut_ascent,
                            strut_descent,
                            strut_line_h,
                            &container_style,
                            false,
                            want_ellipsis,
                            &mut seen_inline_starts,
                        );
                        cursor_y += line.height;
                        lines.push(line);
                        let (bl, br) = floats.available_band(cursor_y, lh, container_width);
                        band_left = bl;
                        band_right = br;
                        cur_x = wrapped_line_indent;
                    } else if let Some(split_idx) = cur_pieces
                        .iter()
                        .rposition(|p| p.can_emergency_break_before)
                        .filter(|&i| i > 0)
                    {
                        let mut carried = cur_pieces.split_off(split_idx);
                        let line = finish_line(
                            ctx,
                            &mut cur_pieces,
                            band_left,
                            band_right,
                            cursor_y,
                            strut_ascent,
                            strut_descent,
                            strut_line_h,
                            &container_style,
                            false,
                            want_ellipsis,
                            &mut seen_inline_starts,
                        );
                        cursor_y += line.height;
                        lines.push(line);
                        let (bl, br) = floats.available_band(cursor_y, lh, container_width);
                        band_left = bl;
                        band_right = br;
                        let shift = carried.first().map_or(0.0, |p| p.x) - wrapped_line_indent;
                        for cp in &mut carried {
                            cp.x -= shift;
                        }
                        cur_x = carried
                            .last()
                            .map_or(wrapped_line_indent, |p| p.x + p.width);
                        cur_pieces = carried;
                    } else if matches!(
                        style.text.overflow_wrap,
                        OverflowWrapMode::BreakWord | OverflowWrapMode::Anywhere
                    ) || style.text.line_break == LineBreakMode::Anywhere
                    {
                        let line = finish_line(
                            ctx,
                            &mut cur_pieces,
                            band_left,
                            band_right,
                            cursor_y,
                            strut_ascent,
                            strut_descent,
                            strut_line_h,
                            &container_style,
                            false,
                            want_ellipsis,
                            &mut seen_inline_starts,
                        );
                        cursor_y += line.height;
                        lines.push(line);
                        let (bl, br) = floats.available_band(cursor_y, lh, container_width);
                        band_left = bl;
                        band_right = br;
                        cur_x = wrapped_line_indent;
                    }
                }

                // If word is still wider than line and break-word/break-all/anywhere is active,
                // split on character boundaries if overflowing an empty line.
                let new_band_w = (band_right - band_left).max(0.0);
                if style.text.text_wrap_mode == TextWrapMode::Wrap
                    && !is_space
                    && cur_pieces.is_empty()
                    && width > new_band_w + 0.05
                    && new_band_w > font_size
                    && (matches!(
                        style.text.word_break,
                        WordBreakMode::BreakAll | WordBreakMode::BreakWord
                    ) || matches!(
                        style.text.overflow_wrap,
                        OverflowWrapMode::BreakWord | OverflowWrapMode::Anywhere
                    ) || style.text.line_break == LineBreakMode::Anywhere)
                {
                    let chunks =
                        split_word_to_fit(ctx, &text, font_id, font_size, &style, new_band_w);
                    let chunk_len = chunks.len();
                    for (ci, chunk) in chunks.into_iter().enumerate() {
                        let (cg, cw) = shape_word_glyphs(ctx, &chunk, font_id, font_size, &style);
                        let text_va = inline_stack
                            .last()
                            .map_or(VerticalAlignMode::Baseline, |a| {
                                a.style.box_model.vertical_align
                            });
                        cur_pieces.push(LinePiece {
                            box_id,
                            x: cur_x + pending_left_edge,
                            width: cw,
                            height: lh,
                            ascent: asc,
                            descent: desc,
                            vertical_align: text_va,
                            is_space: false,
                            is_preserved_space: false,
                            hangs_at_line_end: false,
                            can_break_before: ci > 0 || can_break_before,
                            can_emergency_break_before: false,
                            inline_stack: inline_stack.clone(),
                            payload: LinePiecePayload::Text {
                                font_id,
                                font_size,
                                style: Arc::clone(&style),
                                text: chunk,
                                glyphs: cg,
                            },
                        });
                        cur_x += pending_left_edge + cw;
                        pending_left_edge = 0.0;
                        if ci + 1 < chunk_len {
                            let line = finish_line(
                                ctx,
                                &mut cur_pieces,
                                band_left,
                                band_right,
                                cursor_y,
                                strut_ascent,
                                strut_descent,
                                strut_line_h,
                                &container_style,
                                false,
                                want_ellipsis,
                                &mut seen_inline_starts,
                            );
                            cursor_y += line.height;
                            lines.push(line);
                            let (bl, br) = floats.available_band(cursor_y, lh, container_width);
                            band_left = bl;
                            band_right = br;
                            cur_x = wrapped_line_indent;
                        }
                    }
                    continue;
                }

                cur_x += pending_left_edge;
                pending_left_edge = 0.0;
                // CSS 2.1 §10.8.1: `vertical-align` is non-inherited; bare text directly inside
                // a block or `table-cell` aligns to `baseline` rather than inheriting the container's
                // `vertical-align` (such as `td { vertical-align: middle }`).
                let text_va = inline_stack
                    .last()
                    .map_or(VerticalAlignMode::Baseline, |a| {
                        a.style.box_model.vertical_align
                    });
                cur_pieces.push(LinePiece {
                    box_id,
                    x: cur_x,
                    width,
                    height: lh,
                    ascent: asc,
                    descent: desc,
                    vertical_align: text_va,
                    is_space,
                    is_preserved_space,
                    hangs_at_line_end,
                    can_break_before,
                    can_emergency_break_before,
                    inline_stack,
                    payload: LinePiecePayload::Text {
                        font_id,
                        font_size,
                        style,
                        text,
                        glyphs,
                    },
                });
                cur_x += width;
            }
            InlineItem::Atomic {
                box_id,
                style,
                can_break_before,
                inline_stack,
            } => {
                let atomic_oof_start = ctx.pending_oof.len();
                let mut frag = layout_box(ctx, box_id, space);
                let total_w = frag.border_rect.size.width + frag.margin.horizontal();
                let total_h = frag.border_rect.size.height + frag.margin.vertical();

                let band_w = (band_right - band_left).max(0.0);
                if allow_wrap
                    && can_break_before
                    && !cur_pieces.is_empty()
                    && cur_x + pending_left_edge + total_w > band_w + 0.05
                {
                    let line = finish_line(
                        ctx,
                        &mut cur_pieces,
                        band_left,
                        band_right,
                        cursor_y,
                        strut_ascent,
                        strut_descent,
                        strut_line_h,
                        &container_style,
                        false,
                        want_ellipsis,
                        &mut seen_inline_starts,
                    );
                    cursor_y += line.height;
                    lines.push(line);
                    let (bl, br) =
                        floats.available_band(cursor_y, total_h.max(strut_line_h), container_width);
                    band_left = bl;
                    band_right = br;
                    cur_x = 0.0;
                }

                // If floats narrow the band too much for this atomic inline, shift below floats.
                if !floats.is_empty()
                    && cur_pieces.is_empty()
                    && total_w > (band_right - band_left) + 0.05
                    && total_w <= container_width + 0.05
                {
                    let cleared = floats.clear_y(rix_css::ClearMode::Both, cursor_y);
                    if cleared > cursor_y {
                        cursor_y = cleared;
                        let (bl, br) = floats.available_band(
                            cursor_y,
                            total_h.max(strut_line_h),
                            container_width,
                        );
                        band_left = bl;
                        band_right = br;
                    }
                }

                cur_x += pending_left_edge;
                pending_left_edge = 0.0;
                let base_y = frag.baseline.map_or(total_h, |b| frag.margin.top + b);
                let asc = base_y.min(total_h);
                let desc = (total_h - asc).max(0.0);
                apply_relative_offset(&mut frag, space);
                crate::position::offset_pending_oof(
                    &mut ctx.pending_oof,
                    atomic_oof_start,
                    content_origin.x + band_left + cur_x + frag.margin.left,
                    content_origin.y + cursor_y + frag.margin.top,
                );

                cur_pieces.push(LinePiece {
                    box_id,
                    x: cur_x,
                    width: total_w,
                    height: total_h,
                    ascent: asc,
                    descent: desc,
                    vertical_align: style.box_model.vertical_align,
                    is_space: false,
                    is_preserved_space: false,
                    hangs_at_line_end: false,
                    can_break_before,
                    can_emergency_break_before: false,
                    inline_stack,
                    payload: LinePiecePayload::Atomic(Box::new(frag)),
                });
                cur_x += total_w;
            }
        }
        let _ = is_last_item;
    }

    if !cur_pieces.is_empty() {
        let line = finish_line(
            ctx,
            &mut cur_pieces,
            band_left,
            band_right,
            cursor_y,
            strut_ascent,
            strut_descent,
            strut_line_h,
            &container_style,
            true,
            want_ellipsis,
            &mut seen_inline_starts,
        );
        cursor_y += line.height;
        lines.push(line);
    }

    // Mark `is_last` accurately on split inline box fragments across all emitted lines.
    mark_last_inline_fragments(&mut lines);

    let mut fragments = Vec::with_capacity(lines.len());
    for line in lines {
        let mut line_frag = Fragment {
            box_id: container_id,
            dom_node: None,
            style: Arc::clone(&container_style),
            border_rect: Rect::new(
                content_origin.x + line.x,
                content_origin.y + line.y,
                line.width,
                line.height,
            ),
            padding: EdgeInsets::ZERO,
            border: EdgeInsets::ZERO,
            margin: EdgeInsets::ZERO,
            content_rect: Rect::new(0.0, 0.0, line.width, line.height),
            baseline: Some(line.baseline),
            last_baseline: Some(line.baseline),
            overflow_rect: Rect::new(0.0, 0.0, line.width, line.height),
            kind: FragmentKind::LineBox {
                baseline: line.baseline,
            },
            children: Arc::new(line.children),
            is_float: false,
            is_out_of_flow: false,
            collapsed_margin_top: 0.0,
            collapsed_margin_bottom: 0.0,
            resolved_grid_tracks: None,
        };
        line_frag.compute_overflow_rect();
        fragments.push(line_frag);
    }

    IfcLayoutOutput {
        fragments,
        height: cursor_y,
    }
}

struct FinishedLine {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    baseline: f32,
    children: Vec<Fragment>,
}

#[allow(clippy::too_many_arguments)]
fn finish_line(
    ctx: &mut LayoutContext<'_>,
    pieces: &mut Vec<LinePiece>,
    band_left: f32,
    band_right: f32,
    line_y: f32,
    strut_ascent: f32,
    strut_descent: f32,
    strut_line_h: f32,
    container_style: &Arc<ComputedStyle>,
    is_last_line: bool,
    want_ellipsis: bool,
    seen_inline_starts: &mut Vec<BoxId>,
) -> FinishedLine {
    // Trim trailing collapsible whitespace (never preserved spaces in `pre`/`break-spaces`).
    while pieces
        .last()
        .is_some_and(|p| p.is_space && !p.is_preserved_space)
    {
        pieces.pop();
    }
    // CSS Text L3 §4.1.3: preserved trailing spaces under `pre-wrap` hang at end of line
    // without contributing to line content width or scrollable overflow, while still
    // preserving a non-empty line box height (`strut_line_h`) if they were the only content.
    while pieces.last().is_some_and(|p| p.hangs_at_line_end) {
        pieces.pop();
    }

    let band_width = (band_right - band_left).max(0.0);

    // Apply `text-overflow: ellipsis` if requested and line overflows.
    if want_ellipsis && !pieces.is_empty() {
        let raw_end = pieces.last().map_or(0.0, |p| p.x + p.width);
        if raw_end > band_width + 0.05 {
            apply_ellipsis_truncation(ctx, pieces, band_width, container_style);
        }
    }

    let content_width = pieces.last().map_or(0.0, |p| (p.x + p.width).max(0.0));

    // UAX #9 / CSS Writing Modes L3: reorder visual horizontal placement of pieces on RTL lines.
    if container_style.text.direction == DirectionMode::Rtl && pieces.len() > 1 {
        let mut cursor_rtl = content_width;
        for p in pieces.iter_mut() {
            cursor_rtl -= p.width;
            p.x = cursor_rtl.max(0.0);
        }
    }

    // Horizontal alignment (`text-align`).
    let extra_space = (band_width - content_width).max(0.0);
    let align_offset_x = match container_style.text.text_align {
        TextAlignMode::Left => 0.0,
        TextAlignMode::Start | TextAlignMode::MatchParent => {
            if container_style.text.direction == DirectionMode::Rtl {
                extra_space
            } else {
                0.0
            }
        }
        TextAlignMode::Right => extra_space,
        TextAlignMode::End => {
            if container_style.text.direction == DirectionMode::Rtl {
                0.0
            } else {
                extra_space
            }
        }
        TextAlignMode::Center => extra_space * 0.5,
        TextAlignMode::Justify => {
            if !is_last_line && extra_space > 0.5 {
                let space_count = pieces.iter().filter(|p| p.is_space).count();
                if space_count > 0 {
                    let per_space = extra_space / space_count as f32;
                    let mut shift = 0.0f32;
                    for p in pieces.iter_mut() {
                        p.x += shift;
                        if p.is_space {
                            p.width += per_space;
                            shift += per_space;
                        }
                    }
                }
            }
            0.0
        }
    };

    // Compute line baseline and height across all pieces + strut.
    let is_ahem = container_style
        .text
        .font_family
        .iter()
        .any(|f| f.as_str().eq_ignore_ascii_case("ahem"));
    let strut_half_x = if is_ahem {
        container_style.text.font_size_px * 0.4
    } else {
        container_style.text.font_size_px * 0.25
    };
    let mut max_ascent = strut_ascent;
    let mut max_descent = strut_descent;
    let mut max_top_bottom_h = strut_line_h;
    for p in pieces.iter() {
        match p.vertical_align {
            VerticalAlignMode::Top | VerticalAlignMode::Bottom => {
                max_top_bottom_h = max_top_bottom_h.max(p.height);
            }
            VerticalAlignMode::Middle => {
                let half = p.height * 0.5;
                max_ascent = max_ascent.max(half + strut_half_x);
                max_descent = max_descent.max((half - strut_half_x).max(0.0));
            }
            VerticalAlignMode::LengthPx(shift) => {
                max_ascent = max_ascent.max(p.ascent + shift);
                max_descent = max_descent.max((p.descent - shift).max(0.0));
            }
            VerticalAlignMode::Sub => {
                max_descent = max_descent.max(p.descent + strut_ascent * 0.2);
            }
            VerticalAlignMode::Super => {
                max_ascent = max_ascent.max(p.ascent + strut_ascent * 0.35);
            }
            _ => {
                max_ascent = max_ascent.max(p.ascent);
                max_descent = max_descent.max(p.descent);
            }
        }
    }

    let natural_h = (max_ascent + max_descent).max(max_top_bottom_h);
    let leading = (natural_h - (max_ascent + max_descent)).max(0.0);
    let baseline = max_ascent + leading * 0.5;
    let line_height = natural_h;

    // Collect inline box spans appearing on this line so padding/border/background wrap cleanly!
    struct SpanBounds {
        info: ActiveInline,
        min_x: f32,
        max_x: f32,
        min_y: f32,
        max_y: f32,
    }
    let mut inline_spans: Vec<SpanBounds> = Vec::new();
    let mut emitted_pieces: Vec<Fragment> = Vec::with_capacity(pieces.len());

    for piece in pieces.drain(..) {
        let px = align_offset_x + piece.x;
        let py = match piece.vertical_align {
            VerticalAlignMode::Top | VerticalAlignMode::TextTop => 0.0,
            VerticalAlignMode::Bottom | VerticalAlignMode::TextBottom => {
                (line_height - piece.height).max(0.0)
            }
            VerticalAlignMode::Middle => (baseline - strut_half_x - piece.height * 0.5).max(0.0),
            VerticalAlignMode::Sub => (baseline - piece.ascent + strut_ascent * 0.2).max(0.0),
            VerticalAlignMode::Super => (baseline - piece.ascent - strut_ascent * 0.35).max(0.0),
            VerticalAlignMode::LengthPx(shift) => (baseline - piece.ascent - shift).max(0.0),
            VerticalAlignMode::Baseline => (baseline - piece.ascent).max(0.0),
        };

        // Accumulate bounding rect for every enclosing inline box (`<span>`, `<a>`, etc.)
        for active in &piece.inline_stack {
            let p_top = py;
            let p_bot = py + piece.height;
            if let Some(existing) = inline_spans
                .iter_mut()
                .find(|s| s.info.box_id == active.box_id)
            {
                existing.min_x = existing.min_x.min(px);
                existing.max_x = existing.max_x.max(px + piece.width);
                existing.min_y = existing.min_y.min(p_top);
                existing.max_y = existing.max_y.max(p_bot);
            } else {
                inline_spans.push(SpanBounds {
                    info: active.clone(),
                    min_x: px,
                    max_x: px + piece.width,
                    min_y: p_top,
                    max_y: p_bot,
                });
            }
        }

        match piece.payload {
            LinePiecePayload::Text {
                font_id,
                font_size,
                style,
                text,
                glyphs,
            } => {
                let local_baseline = (baseline - py).clamp(0.0, piece.height);
                let run = ShapedTextFragment {
                    font_id,
                    font_size,
                    color: style.text.color,
                    glyphs,
                    baseline_y: local_baseline,
                    width: piece.width,
                    text,
                    decoration: style.visual.text_decoration_line,
                };
                emitted_pieces.push(Fragment {
                    box_id: piece.box_id,
                    dom_node: None,
                    style,
                    border_rect: Rect::new(px, py, piece.width, piece.height),
                    padding: EdgeInsets::ZERO,
                    border: EdgeInsets::ZERO,
                    margin: EdgeInsets::ZERO,
                    content_rect: Rect::new(0.0, 0.0, piece.width, piece.height),
                    baseline: Some(local_baseline),
                    last_baseline: Some(local_baseline),
                    overflow_rect: Rect::new(0.0, 0.0, piece.width, piece.height),
                    kind: FragmentKind::TextRun(run),
                    children: Arc::new(Vec::new()),
                    is_float: false,
                    is_out_of_flow: false,
                    collapsed_margin_top: 0.0,
                    collapsed_margin_bottom: 0.0,
                    resolved_grid_tracks: None,
                });
            }
            LinePiecePayload::Atomic(mut frag) => {
                frag.border_rect.origin = Point::new(px + frag.margin.left, py + frag.margin.top);
                emitted_pieces.push(*frag);
            }
        }
    }

    // Emit inline box background/border fragments BEFORE text/atomic pieces so text paints on top.
    let mut children = Vec::with_capacity(inline_spans.len() + emitted_pieces.len());
    for span in inline_spans {
        let is_first = if seen_inline_starts.contains(&span.info.box_id) {
            false
        } else {
            seen_inline_starts.push(span.info.box_id);
            true
        };
        let left_pad = if is_first {
            span.info.padding.left
        } else {
            0.0
        };
        let left_brd = if is_first { span.info.border.left } else { 0.0 };
        let top_extra = span.info.padding.top + span.info.border.top;
        let bot_extra = span.info.padding.bottom + span.info.border.bottom;
        let bx = span.min_x - left_pad - left_brd;
        let by = span.min_y - top_extra;
        let bw = (span.max_x - span.min_x) + left_pad + left_brd;
        let bh = (span.max_y - span.min_y) + top_extra + bot_extra;
        children.push(Fragment {
            box_id: span.info.box_id,
            dom_node: span.info.dom_node,
            style: span.info.style,
            border_rect: Rect::new(bx, by, bw, bh),
            padding: EdgeInsets {
                top: span.info.padding.top,
                right: span.info.padding.right,
                bottom: span.info.padding.bottom,
                left: left_pad,
            },
            border: EdgeInsets {
                top: span.info.border.top,
                right: span.info.border.right,
                bottom: span.info.border.bottom,
                left: left_brd,
            },
            margin: span.info.margin,
            content_rect: Rect::new(
                left_brd + left_pad,
                top_extra,
                (span.max_x - span.min_x).max(0.0),
                (span.max_y - span.min_y).max(0.0),
            ),
            baseline: Some(baseline - by),
            last_baseline: Some(baseline - by),
            overflow_rect: Rect::new(0.0, 0.0, bw, bh),
            kind: FragmentKind::InlineBox {
                is_first,
                is_last: false,
            },
            children: Arc::new(Vec::new()),
            is_float: false,
            is_out_of_flow: false,
            collapsed_margin_top: 0.0,
            collapsed_margin_bottom: 0.0,
            resolved_grid_tracks: None,
        });
    }
    children.extend(emitted_pieces);

    FinishedLine {
        x: band_left,
        y: line_y,
        width: band_width.max(content_width),
        height: line_height,
        baseline,
        children,
    }
}

fn mark_last_inline_fragments(lines: &mut [FinishedLine]) {
    let mut seen_last: Vec<BoxId> = Vec::new();
    for line in lines.iter_mut().rev() {
        for child in line.children.iter_mut().rev() {
            if let FragmentKind::InlineBox { is_last, .. } = &mut child.kind {
                if !seen_last.contains(&child.box_id) {
                    seen_last.push(child.box_id);
                    *is_last = true;
                    let right_extra = child.padding.right + child.border.right;
                    child.border_rect.size.width += right_extra;
                    child.overflow_rect.size.width = child
                        .overflow_rect
                        .size
                        .width
                        .max(child.border_rect.size.width);
                } else {
                    child.padding.right = 0.0;
                    child.border.right = 0.0;
                }
            }
        }
    }
}

fn apply_ellipsis_truncation(
    ctx: &mut LayoutContext<'_>,
    pieces: &mut Vec<LinePiece>,
    max_width: f32,
    container_style: &Arc<ComputedStyle>,
) {
    let ell_font = ctx.resolve_font(container_style);
    let ell_size = container_style.text.font_size_px.max(1.0);
    let (ell_glyphs, ell_w) =
        shape_word_glyphs(ctx, "\u{2026}", ell_font, ell_size, container_style);
    let limit = (max_width - ell_w).max(0.0);

    let mut kept = Vec::new();
    let mut end_x = 0.0f32;
    for piece in pieces.drain(..) {
        if piece.x + piece.width <= limit + 0.05 {
            end_x = piece.x + piece.width;
            kept.push(piece);
        } else {
            // Try partial glyph fit inside a text piece.
            if let LinePiecePayload::Text {
                font_id,
                font_size,
                style,
                glyphs,
                ..
            } = piece.payload
            {
                let avail = (limit - piece.x).max(0.0);
                let mut partial = Vec::new();
                let mut acc = 0.0f32;
                for g in glyphs {
                    if acc + g.advance <= avail {
                        partial.push(g);
                        acc += g.advance;
                    } else {
                        break;
                    }
                }
                if !partial.is_empty() {
                    end_x = piece.x + acc;
                    kept.push(LinePiece {
                        box_id: piece.box_id,
                        x: piece.x,
                        width: acc,
                        height: piece.height,
                        ascent: piece.ascent,
                        descent: piece.descent,
                        vertical_align: piece.vertical_align,
                        is_space: false,
                        is_preserved_space: false,
                        hangs_at_line_end: false,
                        can_break_before: piece.can_break_before,
                        can_emergency_break_before: piece.can_emergency_break_before,
                        inline_stack: piece.inline_stack,
                        payload: LinePiecePayload::Text {
                            font_id,
                            font_size,
                            style,
                            text: String::new(),
                            glyphs: partial,
                        },
                    });
                }
            }
            break;
        }
    }

    let (asc, desc) = font_ascent_descent(ctx.font_db, ell_font, ell_size);
    let lh = resolve_line_height_px(container_style, EvalContext::default());
    let ell_box_id = kept.last().map_or(BoxId(0), |p| p.box_id);
    kept.push(LinePiece {
        box_id: ell_box_id,
        x: end_x,
        width: ell_w,
        height: lh,
        ascent: asc,
        descent: desc,
        vertical_align: VerticalAlignMode::Baseline,
        is_space: false,
        is_preserved_space: false,
        hangs_at_line_end: false,
        can_break_before: false,
        can_emergency_break_before: false,
        inline_stack: Vec::new(),
        payload: LinePiecePayload::Text {
            font_id: ell_font,
            font_size: ell_size,
            style: Arc::clone(container_style),
            text: "\u{2026}".to_string(),
            glyphs: ell_glyphs,
        },
    });
    *pieces = kept;
}

struct InlineCollectState {
    prev_char: Option<char>,
    prev_non_bidi: Option<char>,
    prev_ended_with_space: bool,
    prev_nowrap_box: Option<BoxId>,
    container_style: Arc<ComputedStyle>,
}

fn collect_inline_items(
    ctx: &LayoutContext<'_>,
    container_id: BoxId,
    space: ConstraintSpace,
) -> Vec<InlineItem> {
    let mut items = Vec::new();
    let mut stack: Vec<ActiveInline> = Vec::new();
    let container_style = Arc::clone(&ctx.tree.get(container_id).style);
    let mut state = InlineCollectState {
        prev_char: None,
        prev_non_bidi: None,
        prev_ended_with_space: false,
        prev_nowrap_box: None,
        container_style,
    };
    let children = ctx.tree.get(container_id).children.clone();
    // Pre-collect the first visible non-bidi, non-ASCII-space character following each text box
    // (ignoring inline box boundaries and out-of-flow elements) for CSS Text L4 §4.1.2.
    let mut text_order: Vec<(BoxId, Option<char>)> = Vec::new();
    collect_text_first_chars(ctx, &children, &mut text_order);
    let mut next_char_map: std::collections::HashMap<BoxId, Option<char>> =
        std::collections::HashMap::with_capacity(text_order.len());
    for i in 0..text_order.len() {
        let next_ch = text_order[i + 1..].iter().find_map(|&(_, ch)| ch);
        next_char_map.insert(text_order[i].0, next_ch);
    }

    for cid in children {
        collect_inline_recursive(
            ctx,
            cid,
            space,
            &mut stack,
            &mut state,
            &next_char_map,
            &mut items,
        );
    }
    apply_white_space_trim(&mut items);
    items
}

fn collect_text_first_chars(
    ctx: &LayoutContext<'_>,
    ids: &[BoxId],
    out: &mut Vec<(BoxId, Option<char>)>,
) {
    for &cid in ids {
        let b = ctx.tree.get(cid);
        if b.is_out_of_flow || b.is_float {
            continue;
        }
        match &b.kind {
            BoxKind::Inline => collect_text_first_chars(ctx, &b.children, out),
            BoxKind::Text(raw) => {
                let first = raw.chars().find(|&c| {
                    !is_ascii_collapsible_ws(c) && !crate::line_break::is_bidi_control(c)
                });
                out.push((cid, first));
            }
            BoxKind::Replaced(ReplacedKind::LineBreak) => {
                out.push((cid, Some('\n')));
            }
            BoxKind::InlineBlock { .. }
            | BoxKind::Replaced(_)
            | BoxKind::Block { .. }
            | BoxKind::Table
            | BoxKind::AnonymousTable
            | BoxKind::AnonymousBlock => {
                out.push((cid, Some('\u{FFFC}')));
            }
            _ => {}
        }
    }
}

/// Applies CSS Text L4 §3.4 `white-space-trim` (`discard-before | discard-after | discard-inner`)
/// on collected inline items, merging surrounding break opportunities when a space is discarded.
fn apply_white_space_trim(items: &mut Vec<InlineItem>) {
    let mut trim_map: std::collections::HashMap<BoxId, WhiteSpaceTrim> =
        std::collections::HashMap::new();
    for item in items.iter() {
        if let InlineItem::PushInline(info) = item {
            if !info.style.white_space_trim.is_none() {
                trim_map.insert(info.box_id, info.style.white_space_trim);
            }
        }
    }
    if trim_map.is_empty() {
        return;
    }

    let mut remove = vec![false; items.len()];
    let mut clear_break = vec![false; items.len()];

    for i in 0..items.len() {
        match &items[i] {
            InlineItem::PushInline(info) => {
                let trim = info.style.white_space_trim;
                if trim.0 & WhiteSpaceTrim::DISCARD_BEFORE != 0 {
                    // Remove immediately preceding collapsible space(s) before this box.
                    let mut j = i;
                    let mut removed_any = false;
                    while j > 0 {
                        j -= 1;
                        match &items[j] {
                            InlineItem::PushInline(_)
                            | InlineItem::PopInline(_)
                            | InlineItem::OutOfFlow(_) => {}
                            InlineItem::Word {
                                is_space: true,
                                is_preserved_space: false,
                                ..
                            } => {
                                remove[j] = true;
                                removed_any = true;
                            }
                            _ => break,
                        }
                    }
                    if removed_any {
                        // Also remove soft-wrap opportunity at the first non-space word inside/after.
                        for (k, it) in items.iter().enumerate().skip(i + 1) {
                            match it {
                                InlineItem::PushInline(_)
                                | InlineItem::PopInline(_)
                                | InlineItem::OutOfFlow(_) => {}
                                InlineItem::Word {
                                    is_space: false, ..
                                } => {
                                    clear_break[k] = true;
                                    break;
                                }
                                _ => break,
                            }
                        }
                    }
                }
                if trim.0 & WhiteSpaceTrim::DISCARD_INNER != 0 {
                    for (k, it) in items.iter().enumerate().skip(i + 1) {
                        match it {
                            InlineItem::Word {
                                is_space: true,
                                is_preserved_space: false,
                                ..
                            } => remove[k] = true,
                            InlineItem::PushInline(_) | InlineItem::OutOfFlow(_) => {}
                            _ => break,
                        }
                    }
                }
            }
            InlineItem::PopInline(bid) => {
                if let Some(&trim) = trim_map.get(bid) {
                    if trim.0 & WhiteSpaceTrim::DISCARD_INNER != 0 {
                        let mut j = i;
                        while j > 0 {
                            j -= 1;
                            match &items[j] {
                                InlineItem::Word {
                                    is_space: true,
                                    is_preserved_space: false,
                                    ..
                                } => remove[j] = true,
                                InlineItem::PopInline(_) | InlineItem::OutOfFlow(_) => {}
                                _ => break,
                            }
                        }
                    }
                    if trim.0 & WhiteSpaceTrim::DISCARD_AFTER != 0 {
                        let mut removed_any = false;
                        for (k, it) in items.iter().enumerate().skip(i + 1) {
                            match it {
                                InlineItem::PushInline(_)
                                | InlineItem::PopInline(_)
                                | InlineItem::OutOfFlow(_) => {}
                                InlineItem::Word {
                                    is_space: true,
                                    is_preserved_space: false,
                                    ..
                                } => {
                                    remove[k] = true;
                                    removed_any = true;
                                }
                                InlineItem::Word {
                                    is_space: false, ..
                                } => {
                                    if removed_any {
                                        clear_break[k] = true;
                                    }
                                    break;
                                }
                                _ => break,
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    for (i, item) in items.iter_mut().enumerate() {
        if clear_break[i] {
            if let InlineItem::Word {
                can_break_before, ..
            } = item
            {
                *can_break_before = false;
            }
        }
    }
    let mut idx = 0usize;
    items.retain(|_| {
        let keep = !remove[idx];
        idx += 1;
        keep
    });
}

#[allow(clippy::too_many_arguments)]
fn collect_inline_recursive(
    ctx: &LayoutContext<'_>,
    box_id: BoxId,
    space: ConstraintSpace,
    stack: &mut Vec<ActiveInline>,
    state: &mut InlineCollectState,
    next_char_map: &std::collections::HashMap<BoxId, Option<char>>,
    out: &mut Vec<InlineItem>,
) {
    let b = ctx.tree.get(box_id);
    if b.is_out_of_flow {
        out.push(InlineItem::OutOfFlow(box_id));
        return;
    }
    if b.is_float {
        return;
    }
    match &b.kind {
        BoxKind::Inline => {
            let m = BoxMetrics::resolve(&b.style, &space);
            let active = ActiveInline {
                box_id,
                dom_node: b.dom_node,
                style: Arc::clone(&b.style),
                padding: m.padding,
                border: m.border,
                margin: m.margin,
            };
            out.push(InlineItem::PushInline(active.clone()));
            stack.push(active);
            for &cid in &b.children {
                collect_inline_recursive(ctx, cid, space, stack, state, next_char_map, out);
            }
            stack.pop();
            out.push(InlineItem::PopInline(box_id));
        }
        BoxKind::Text(raw) => {
            let lang = b.lang.as_deref();
            let transformed = crate::text_transform::apply_text_transform_flags(
                raw,
                b.style.text.text_transform_flags,
                lang,
                state.prev_char,
            );
            let cur_nowrap_box = if b.style.text.text_wrap_mode == TextWrapMode::Nowrap {
                stack
                    .iter()
                    .rev()
                    .find(|a| a.style.text.text_wrap_mode == TextWrapMode::Nowrap)
                    .map(|a| a.box_id)
            } else {
                None
            };
            let next_non_bidi = next_char_map.get(&box_id).copied().flatten();
            tokenize_text(
                &transformed,
                &b.style,
                lang,
                box_id,
                stack,
                state,
                cur_nowrap_box,
                next_non_bidi,
                out,
            );
        }
        BoxKind::Replaced(ReplacedKind::LineBreak) => {
            state.prev_char = None;
            state.prev_non_bidi = None;
            state.prev_ended_with_space = false;
            state.prev_nowrap_box = None;
            out.push(InlineItem::HardBreak);
        }
        BoxKind::InlineBlock { .. }
        | BoxKind::Replaced(_)
        | BoxKind::Block { .. }
        | BoxKind::Table
        | BoxKind::AnonymousTable
        | BoxKind::AnonymousBlock => {
            let can_break = state.container_style.text.text_wrap_mode == TextWrapMode::Wrap
                && b.style.text.text_wrap_mode == TextWrapMode::Wrap;
            state.prev_char = Some('\u{FFFC}');
            state.prev_non_bidi = Some('\u{FFFC}');
            state.prev_ended_with_space = false;
            state.prev_nowrap_box = None;
            out.push(InlineItem::Atomic {
                box_id,
                style: Arc::clone(&b.style),
                can_break_before: can_break,
                inline_stack: stack.clone(),
            });
        }
        _ => {}
    }
}

#[inline]
const fn is_ascii_collapsible_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0C')
}

#[allow(clippy::too_many_arguments)]
fn tokenize_text(
    text: &str,
    style: &Arc<ComputedStyle>,
    lang: Option<&str>,
    box_id: BoxId,
    stack: &[ActiveInline],
    state: &mut InlineCollectState,
    cur_nowrap_box: Option<BoxId>,
    next_non_bidi: Option<char>,
    out: &mut Vec<InlineItem>,
) {
    let collapse = style.text.white_space_collapse;
    let wrap_mode = style.text.text_wrap_mode;
    let preserve_spaces = matches!(
        collapse,
        WhiteSpaceCollapseMode::Preserve
            | WhiteSpaceCollapseMode::PreserveSpaces
            | WhiteSpaceCollapseMode::BreakSpaces
    );
    let preserve_breaks = matches!(
        collapse,
        WhiteSpaceCollapseMode::Preserve
            | WhiteSpaceCollapseMode::PreserveBreaks
            | WhiteSpaceCollapseMode::BreakSpaces
    );

    // Returns `(can_break_before, can_emergency_break_before)` at the start of this text node.
    let boundary_breaks = |first_ch: char, st: &InlineCollectState| -> (bool, bool) {
        let Some(prev_ch) = st.prev_char else {
            return (false, false);
        };
        if prev_ch == '\u{FFFC}' {
            let can = st.container_style.text.text_wrap_mode == TextWrapMode::Wrap;
            return (can, false);
        }
        if wrap_mode == TextWrapMode::Nowrap {
            // CSS Text L3 §5.1 & §5.5: the boundary between two distinct `nowrap` inline boxes
            // is governed by their enclosing container:
            // - `overflow-wrap: anywhere` creates a soft-wrap opportunity for both layout and min-content
            //   only when overflowing, while normal UAX #14 breaks apply if container allows wrapping.
            // - `overflow-wrap: break-word` creates an EMERGENCY break ONLY (NOT for min-content!).
            if st.container_style.text.text_wrap_mode == TextWrapMode::Wrap
                && cur_nowrap_box != st.prev_nowrap_box
            {
                if is_ascii_collapsible_ws(prev_ch) {
                    return (true, false);
                }
                let uax = crate::line_break::can_break_between(
                    prev_ch,
                    first_ch,
                    &st.container_style,
                    lang,
                );
                if uax {
                    return (true, false);
                }
                let emerg = matches!(
                    st.container_style.text.overflow_wrap,
                    OverflowWrapMode::BreakWord | OverflowWrapMode::Anywhere
                );
                return (false, emerg);
            }
            return (false, false);
        }
        if is_ascii_collapsible_ws(prev_ch) {
            return (true, false);
        }
        (
            crate::line_break::can_break_between(prev_ch, first_ch, style, lang),
            false,
        )
    };

    if preserve_spaces {
        let is_break_spaces = collapse == WhiteSpaceCollapseMode::BreakSpaces;
        for (line_idx, raw_seg) in text.split('\n').enumerate() {
            if line_idx > 0 {
                out.push(InlineItem::HardBreak);
                state.prev_char = None;
                state.prev_non_bidi = None;
                state.prev_ended_with_space = false;
                state.prev_nowrap_box = None;
            }
            if raw_seg.is_empty() {
                continue;
            }
            // Split around `\t` characters so CSS Text L3 §4.2 tab-stop layout computes
            // exact horizontal advance relative to the line's current x position.
            let mut part_start = 0usize;
            let mut is_first_piece = true;
            for (tab_idx, _) in raw_seg.match_indices('\t') {
                if tab_idx > part_start {
                    emit_preserved_segment(
                        &raw_seg[part_start..tab_idx],
                        style,
                        lang,
                        box_id,
                        stack,
                        state,
                        cur_nowrap_box,
                        wrap_mode,
                        is_break_spaces,
                        is_first_piece,
                        &boundary_breaks,
                        out,
                    );
                    is_first_piece = false;
                }
                let (can_b, can_eb) = if is_first_piece {
                    boundary_breaks('\t', state)
                } else {
                    (wrap_mode == TextWrapMode::Wrap && is_break_spaces, false)
                };
                out.push(InlineItem::Word {
                    text: "\t".to_string(),
                    style: Arc::clone(style),
                    box_id,
                    is_space: true,
                    is_preserved_space: true,
                    is_tab: true,
                    can_break_before: can_b,
                    can_emergency_break_before: can_eb,
                    inline_stack: stack.to_vec(),
                });
                state.prev_char = Some('\t');
                state.prev_ended_with_space = true;
                state.prev_nowrap_box = cur_nowrap_box;
                is_first_piece = false;
                part_start = tab_idx + 1;
            }
            if part_start < raw_seg.len() {
                emit_preserved_segment(
                    &raw_seg[part_start..],
                    style,
                    lang,
                    box_id,
                    stack,
                    state,
                    cur_nowrap_box,
                    wrap_mode,
                    is_break_spaces,
                    is_first_piece,
                    &boundary_breaks,
                    out,
                );
            }
        }
        return;
    }

    // Collapsible whitespace (`collapse`, `preserve-breaks`, `discard`) per CSS Text L3/L4 §4.1.
    let discard_ws = collapse == WhiteSpaceCollapseMode::Discard;
    let normalized_lines = crate::line_break::collapse_whitespace_l4(
        text,
        preserve_breaks,
        discard_ws,
        state.prev_non_bidi,
        next_non_bidi,
        state.prev_ended_with_space,
    );

    for (line_idx, norm) in normalized_lines.into_iter().enumerate() {
        if line_idx > 0 {
            out.push(InlineItem::HardBreak);
            state.prev_char = None;
            state.prev_non_bidi = None;
            state.prev_ended_with_space = false;
            state.prev_nowrap_box = None;
        }
        if norm.is_empty() {
            continue;
        }

        let mut cursor = 0usize;
        let mut emitted_any = false;
        if norm.starts_with(' ') {
            out.push(InlineItem::Word {
                text: " ".to_string(),
                style: Arc::clone(style),
                box_id,
                is_space: true,
                is_preserved_space: false,
                is_tab: false,
                can_break_before: false,
                can_emergency_break_before: false,
                inline_stack: stack.to_vec(),
            });
            state.prev_char = Some(' ');
            state.prev_ended_with_space = true;
            state.prev_nowrap_box = cur_nowrap_box;
            cursor = 1;
            emitted_any = true;
        }

        let body = &norm[cursor..];
        if !body.is_empty() {
            let breaks = if wrap_mode == TextWrapMode::Wrap {
                crate::line_break::line_break_offsets(body, style, lang)
            } else {
                Vec::new()
            };
            let mut seg_start = 0usize;
            for &brk in breaks.iter().chain(std::iter::once(&body.len())) {
                if brk <= seg_start {
                    continue;
                }
                let raw_piece = &body[seg_start..brk];
                let has_trailing_sp = raw_piece.ends_with(' ');
                let core = if has_trailing_sp {
                    &raw_piece[..raw_piece.len() - 1]
                } else {
                    raw_piece
                };
                if !core.is_empty() {
                    let (can_brk, can_ebrk) = if !emitted_any {
                        core.chars()
                            .next()
                            .map_or((false, false), |c| boundary_breaks(c, state))
                    } else {
                        (wrap_mode == TextWrapMode::Wrap, false)
                    };
                    out.push(InlineItem::Word {
                        text: core.to_string(),
                        style: Arc::clone(style),
                        box_id,
                        is_space: false,
                        is_preserved_space: false,
                        is_tab: false,
                        can_break_before: can_brk,
                        can_emergency_break_before: can_ebrk,
                        inline_stack: stack.to_vec(),
                    });
                    state.prev_char = core.chars().last();
                    if let Some(nb) = core
                        .chars()
                        .rev()
                        .find(|&c| !crate::line_break::is_bidi_control(c))
                    {
                        state.prev_non_bidi = Some(nb);
                    }
                    state.prev_ended_with_space = false;
                    state.prev_nowrap_box = cur_nowrap_box;
                    emitted_any = true;
                }
                if has_trailing_sp {
                    out.push(InlineItem::Word {
                        text: " ".to_string(),
                        style: Arc::clone(style),
                        box_id,
                        is_space: true,
                        is_preserved_space: false,
                        is_tab: false,
                        can_break_before: false,
                        can_emergency_break_before: false,
                        inline_stack: stack.to_vec(),
                    });
                    state.prev_char = Some(' ');
                    state.prev_ended_with_space = true;
                    state.prev_nowrap_box = cur_nowrap_box;
                    emitted_any = true;
                }
                seg_start = brk;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_preserved_segment<F>(
    seg: &str,
    style: &Arc<ComputedStyle>,
    lang: Option<&str>,
    box_id: BoxId,
    stack: &[ActiveInline],
    state: &mut InlineCollectState,
    cur_nowrap_box: Option<BoxId>,
    wrap_mode: TextWrapMode,
    is_break_spaces: bool,
    is_first_piece: bool,
    boundary_breaks: &F,
    out: &mut Vec<InlineItem>,
) where
    F: Fn(char, &InlineCollectState) -> (bool, bool),
{
    if wrap_mode == TextWrapMode::Nowrap {
        let first_ch = seg.chars().next().unwrap_or(' ');
        let (can_b, can_eb) = if is_first_piece {
            boundary_breaks(first_ch, state)
        } else {
            (false, false)
        };
        let all_sp = seg.chars().all(|c| c == ' ');
        out.push(InlineItem::Word {
            text: seg.to_string(),
            style: Arc::clone(style),
            box_id,
            is_space: all_sp,
            is_preserved_space: all_sp,
            is_tab: false,
            can_break_before: can_b,
            can_emergency_break_before: can_eb,
            inline_stack: stack.to_vec(),
        });
    } else {
        let mut breaks = crate::line_break::line_break_offsets(seg, style, lang);
        if is_break_spaces {
            for (idx, ch) in seg.char_indices() {
                if ch == ' ' {
                    let after = idx + 1;
                    if after < seg.len() {
                        if let Err(pos) = breaks.binary_search(&after) {
                            breaks.insert(pos, after);
                        }
                    }
                }
            }
        }
        let mut start = 0usize;
        let mut seg_i = 0usize;
        for &brk in breaks.iter().chain(std::iter::once(&seg.len())) {
            if brk <= start {
                continue;
            }
            let slice = &seg[start..brk];
            // Separate trailing ASCII spaces so `pre-wrap` can hang them cleanly at line end.
            let non_sp_len = slice.trim_end_matches(' ').len();
            let (core, trail_sp) = slice.split_at(non_sp_len);
            if !core.is_empty() {
                let (can_b, can_eb) = if seg_i == 0 && is_first_piece {
                    core.chars()
                        .next()
                        .map_or((false, false), |c| boundary_breaks(c, state))
                } else {
                    (true, false)
                };
                out.push(InlineItem::Word {
                    text: core.to_string(),
                    style: Arc::clone(style),
                    box_id,
                    is_space: false,
                    is_preserved_space: false,
                    is_tab: false,
                    can_break_before: can_b,
                    can_emergency_break_before: can_eb,
                    inline_stack: stack.to_vec(),
                });
                seg_i += 1;
            }
            if !trail_sp.is_empty() {
                let (can_b, can_eb) = if seg_i == 0 && is_first_piece {
                    boundary_breaks(' ', state)
                } else {
                    (is_break_spaces, false)
                };
                out.push(InlineItem::Word {
                    text: trail_sp.to_string(),
                    style: Arc::clone(style),
                    box_id,
                    is_space: true,
                    is_preserved_space: true,
                    is_tab: false,
                    can_break_before: can_b,
                    can_emergency_break_before: can_eb,
                    inline_stack: stack.to_vec(),
                });
                seg_i += 1;
            }
            start = brk;
        }
    }
    state.prev_char = seg.chars().last();
    if let Some(nb) = seg
        .chars()
        .rev()
        .find(|&c| !crate::line_break::is_bidi_control(c))
    {
        state.prev_non_bidi = Some(nb);
    }
    state.prev_ended_with_space = seg.ends_with(' ');
    state.prev_nowrap_box = cur_nowrap_box;
}

/// Computes the CSS Text L3 §4.2 tab-stop advance for a single U+0009 TAB character
/// positioned at `current_x` along the line.
fn compute_tab_advance(
    ctx: &LayoutContext<'_>,
    font_id: FontId,
    font_size: f32,
    style: &ComputedStyle,
    eval: EvalContext,
    current_x: f32,
) -> f32 {
    let tab_stop = if let Some(ref lp) = style.text.tab_size_len {
        lp.resolve_px(font_size, &eval).max(0.0)
    } else {
        let sp_w = measure_shaped_width(ctx, " ", font_id, font_size, style);
        (style.text.tab_size_num * sp_w).max(0.0)
    };
    if tab_stop <= 1e-4 {
        return 0.0;
    }
    let rem = current_x.rem_euclid(tab_stop);
    if rem < 1e-4 {
        tab_stop
    } else {
        tab_stop - rem
    }
}

fn shape_word_glyphs(
    ctx: &LayoutContext<'_>,
    text: &str,
    font_id: FontId,
    font_size: f32,
    style: &ComputedStyle,
) -> (Vec<PositionedGlyph>, f32) {
    let letter_sp = style
        .text
        .letter_spacing
        .resolve_px(font_size, &EvalContext::default());
    let word_sp = if text == " " {
        style
            .text
            .word_spacing
            .resolve_px(font_size, &EvalContext::default())
    } else {
        0.0
    };

    if style.text.font_size_px <= 0.0 {
        return (Vec::new(), 0.0);
    }

    // Strip zero-width default-ignorable format/bidi controls so fonts without glyphs for
    // U+200B / U+202E / U+202C / U+2060 do not contribute tofu advance widths.
    let shaped_input: std::borrow::Cow<'_, str> =
        if text.chars().any(crate::line_break::is_zero_width_ignorable) {
            std::borrow::Cow::Owned(
                text.chars()
                    .filter(|&c| !crate::line_break::is_zero_width_ignorable(c))
                    .collect(),
            )
        } else {
            std::borrow::Cow::Borrowed(text)
        };
    if shaped_input.is_empty() {
        return (Vec::new(), 0.0);
    }

    let runs = ctx.shaper.shape_text(&shaped_input, font_id, font_size);
    if runs.is_empty() {
        // Deterministic fallback width when running in an environment with no system fonts installed.
        let approx = shaped_input.chars().count() as f32 * (font_size * 0.56) + word_sp;
        return (Vec::new(), approx);
    }

    let mut glyphs = Vec::new();
    let mut pen_x = 0.0f32;
    for run in runs {
        let run_font_id = run.font_id;
        for g in run.glyphs {
            let adv = (g.x_advance + letter_sp + word_sp).max(0.0);
            glyphs.push(PositionedGlyph {
                font_id: run_font_id,
                glyph_id: g.glyph_id,
                x: pen_x + g.x_offset,
                y: -g.y_offset,
                advance: adv,
            });
            pen_x += adv;
        }
    }
    (glyphs, pen_x)
}

fn measure_shaped_width(
    ctx: &LayoutContext<'_>,
    text: &str,
    font_id: FontId,
    font_size: f32,
    style: &ComputedStyle,
) -> f32 {
    shape_word_glyphs(ctx, text, font_id, font_size, style).1
}

fn split_word_to_fit(
    ctx: &LayoutContext<'_>,
    word: &str,
    font_id: FontId,
    font_size: f32,
    style: &ComputedStyle,
    max_width: f32,
) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in word.chars() {
        let mut candidate = current.clone();
        candidate.push(ch);
        let w = measure_shaped_width(ctx, &candidate, font_id, font_size, style);
        if w > max_width && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn font_ascent_descent(font_db: &FontDatabase, font_id: FontId, font_size: f32) -> (f32, f32) {
    if let Some(font) = font_db.get_font(font_id) {
        let asc = font.metrics.ascender_px(font_size).abs();
        let desc = font.metrics.descender_px(font_size).abs();
        if asc > 0.0 {
            return (asc, desc);
        }
    }
    (font_size * 0.8, font_size * 0.2)
}
