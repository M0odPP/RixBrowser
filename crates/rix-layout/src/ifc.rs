//! Inline Formatting Context (IFC): HarfBuzz shaping via shared `rix-text`
//! cache, line box construction around active floats, `inline-block` and
//! replaced atomic inlines, inline box padding/border splitting across wrapped
//! lines, `text-align`, `vertical-align`, `text-overflow: ellipsis`, and
//! `white-space` modes.

use std::sync::Arc;

use rix_core::{Point, Rect};
use rix_css::{
    ComputedStyle, DirectionMode, EvalContext, FontStyleMode, LineHeightValue, OverflowMode,
    OverflowWrapMode, TextAlignMode, TextOverflowMode, TextTransformMode, VerticalAlignMode,
    WhiteSpaceMode, WordBreakMode,
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
        LineHeightValue::Number(n) => (font_size * *n).max(font_size * 0.5),
        LineHeightValue::Length(lp) => lp.resolve_px(font_size, &eval).max(1.0),
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
    Word {
        text: String,
        style: Arc<ComputedStyle>,
        box_id: BoxId,
        is_space: bool,
        inline_stack: Vec<ActiveInline>,
    },
    HardBreak,
    Atomic {
        box_id: BoxId,
        style: Arc<ComputedStyle>,
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

/// Computes `(min_content_width, max_content_width)` of the inline content inside `container_id`.
pub fn measure_ifc_min_max_content(
    ctx: &mut LayoutContext<'_>,
    container_id: BoxId,
    space: ConstraintSpace,
) -> (f32, f32) {
    let items = collect_inline_items(ctx, container_id, space);
    let mut min_w = 0.0f32;
    let mut max_w = 0.0f32;
    let mut current_line_w = 0.0f32;
    let mut inline_edge_extra = 0.0f32;

    for item in &items {
        match item {
            InlineItem::PushInline(info) => {
                let left = info.border.left + info.padding.left + info.margin.left;
                current_line_w += left;
                inline_edge_extra += left;
            }
            InlineItem::PopInline(_) => {
                // Right edge accounted for in max-content below via stack or word tracking.
            }
            InlineItem::Word {
                text,
                style,
                is_space,
                ..
            } => {
                let font_id = resolve_font_for_style(ctx.font_db, ctx.web_fonts, style);
                let font_size = style.text.font_size_px.max(1.0);
                let w = measure_shaped_width(ctx, text, font_id, font_size, style);
                if !is_space {
                    min_w = min_w.max(w);
                }
                current_line_w += w;
                max_w = max_w.max(current_line_w);
            }
            InlineItem::HardBreak => {
                max_w = max_w.max(current_line_w);
                current_line_w = 0.0;
            }
            InlineItem::Atomic { box_id, .. } => {
                let b = ctx.tree.get(*box_id);
                let m = BoxMetrics::resolve(&b.style, &space);
                let (cmin, cmax) = measure_intrinsic_widths(ctx, *box_id, space);
                min_w = min_w.max(cmin + m.margin.horizontal());
                current_line_w += cmax + m.margin.horizontal();
                max_w = max_w.max(current_line_w);
            }
        }
    }
    let _ = inline_edge_extra;
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
    let eval = space.eval_context(container_style.text.font_size_px);
    let strut_line_h = resolve_line_height_px(&container_style, eval);
    let strut_font_id = resolve_font_for_style(ctx.font_db, ctx.web_fonts, &container_style);
    let (strut_ascent, strut_descent) = font_ascent_descent(
        ctx.font_db,
        strut_font_id,
        container_style.text.font_size_px,
    );

    let items = collect_inline_items(ctx, container_id, space);
    if items.is_empty() {
        return IfcLayoutOutput {
            fragments: Vec::new(),
            height: 0.0,
        };
    }

    let allow_wrap = !matches!(
        container_style.text.white_space,
        WhiteSpaceMode::Nowrap | WhiteSpaceMode::Pre
    );
    let want_ellipsis = container_style.box_model.text_overflow == TextOverflowMode::Ellipsis
        && container_style.overflow.x != OverflowMode::Visible
        && !allow_wrap;

    let mut lines: Vec<FinishedLine> = Vec::new();
    let mut cur_pieces: Vec<LinePiece> = Vec::new();
    let mut cursor_y = 0.0f32;
    let mut pending_left_edge = 0.0f32;
    let mut seen_inline_starts: Vec<BoxId> = Vec::new();

    let (mut band_left, mut band_right) =
        floats.available_band(cursor_y, strut_line_h, container_width);
    let mut cur_x = 0.0f32;

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
                cur_x = 0.0;
                pending_left_edge = 0.0;
            }
            InlineItem::Word {
                text,
                style,
                box_id,
                is_space,
                inline_stack,
            } => {
                // Skip leading collapsible spaces at line start.
                if is_space
                    && cur_pieces.is_empty()
                    && matches!(
                        style.text.white_space,
                        WhiteSpaceMode::Normal | WhiteSpaceMode::Nowrap | WhiteSpaceMode::PreLine
                    )
                {
                    continue;
                }

                let font_id = resolve_font_for_style(ctx.font_db, ctx.web_fonts, &style);
                let font_size = style.text.font_size_px.max(1.0);
                let (glyphs, width) = shape_word_glyphs(ctx, &text, font_id, font_size, &style);
                let (asc, desc) = font_ascent_descent(ctx.font_db, font_id, font_size);
                let lh = resolve_line_height_px(&style, space.eval_context(font_size));

                let band_w = (band_right - band_left).max(0.0);
                let wrap_here = allow_wrap
                    && !is_space
                    && !cur_pieces.is_empty()
                    && cur_x + pending_left_edge + width > band_w + 0.05;

                if wrap_here {
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
                    cur_x = 0.0;
                }

                // If word is still wider than line and break-word/break-all is active, allow piece as-is
                // or split on character boundaries if overflowing an empty line.
                let new_band_w = (band_right - band_left).max(0.0);
                if allow_wrap
                    && !is_space
                    && cur_pieces.is_empty()
                    && width > new_band_w + 0.05
                    && new_band_w > font_size
                    && matches!(
                        style.text.word_break,
                        WordBreakMode::BreakAll | WordBreakMode::BreakWord
                    ) | matches!(
                        style.text.overflow_wrap,
                        OverflowWrapMode::BreakWord | OverflowWrapMode::Anywhere
                    )
                {
                    let chunks =
                        split_word_to_fit(ctx, &text, font_id, font_size, &style, new_band_w);
                    let chunk_len = chunks.len();
                    for (ci, chunk) in chunks.into_iter().enumerate() {
                        let (cg, cw) = shape_word_glyphs(ctx, &chunk, font_id, font_size, &style);
                        cur_pieces.push(LinePiece {
                            box_id,
                            x: cur_x + pending_left_edge,
                            width: cw,
                            height: lh,
                            ascent: asc,
                            descent: desc,
                            vertical_align: style.box_model.vertical_align,
                            is_space: false,
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
                            cur_x = 0.0;
                        }
                    }
                    continue;
                }

                cur_x += pending_left_edge;
                pending_left_edge = 0.0;
                cur_pieces.push(LinePiece {
                    box_id,
                    x: cur_x,
                    width,
                    height: lh,
                    ascent: asc,
                    descent: desc,
                    vertical_align: style.box_model.vertical_align,
                    is_space,
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
                inline_stack,
            } => {
                let mut frag = layout_box(ctx, box_id, space);
                let total_w = frag.border_rect.size.width + frag.margin.horizontal();
                let total_h = frag.border_rect.size.height + frag.margin.vertical();

                let band_w = (band_right - band_left).max(0.0);
                if allow_wrap
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

                cur_pieces.push(LinePiece {
                    box_id,
                    x: cur_x,
                    width: total_w,
                    height: total_h,
                    ascent: asc,
                    descent: desc,
                    vertical_align: style.box_model.vertical_align,
                    is_space: false,
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
            overflow_rect: Rect::new(0.0, 0.0, line.width, line.height),
            kind: FragmentKind::LineBox {
                baseline: line.baseline,
            },
            children: line.children,
            is_float: false,
            is_out_of_flow: false,
            collapsed_margin_top: 0.0,
            collapsed_margin_bottom: 0.0,
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
    ctx: &LayoutContext<'_>,
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
    // Trim trailing collapsible whitespace.
    while pieces.last().is_some_and(|p| p.is_space) {
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
                max_ascent = max_ascent.max(half + strut_ascent * 0.25);
                max_descent = max_descent.max((half - strut_ascent * 0.25).max(0.0));
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
            VerticalAlignMode::Middle => ((line_height - piece.height) * 0.5).max(0.0),
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
                    overflow_rect: Rect::new(0.0, 0.0, piece.width, piece.height),
                    kind: FragmentKind::TextRun(run),
                    children: Vec::new(),
                    is_float: false,
                    is_out_of_flow: false,
                    collapsed_margin_top: 0.0,
                    collapsed_margin_bottom: 0.0,
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
            overflow_rect: Rect::new(0.0, 0.0, bw, bh),
            kind: FragmentKind::InlineBox {
                is_first,
                is_last: false,
            },
            children: Vec::new(),
            is_float: false,
            is_out_of_flow: false,
            collapsed_margin_top: 0.0,
            collapsed_margin_bottom: 0.0,
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
    ctx: &LayoutContext<'_>,
    pieces: &mut Vec<LinePiece>,
    max_width: f32,
    container_style: &Arc<ComputedStyle>,
) {
    let ell_font = resolve_font_for_style(ctx.font_db, ctx.web_fonts, container_style);
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

fn collect_inline_items(
    ctx: &LayoutContext<'_>,
    container_id: BoxId,
    space: ConstraintSpace,
) -> Vec<InlineItem> {
    let mut items = Vec::new();
    let mut stack: Vec<ActiveInline> = Vec::new();
    let children = ctx.tree.get(container_id).children.clone();
    for cid in children {
        collect_inline_recursive(ctx, cid, space, &mut stack, &mut items);
    }
    items
}

fn collect_inline_recursive(
    ctx: &LayoutContext<'_>,
    box_id: BoxId,
    space: ConstraintSpace,
    stack: &mut Vec<ActiveInline>,
    out: &mut Vec<InlineItem>,
) {
    let b = ctx.tree.get(box_id);
    if b.is_out_of_flow || b.is_float {
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
                collect_inline_recursive(ctx, cid, space, stack, out);
            }
            stack.pop();
            out.push(InlineItem::PopInline(box_id));
        }
        BoxKind::Text(raw) => {
            let transformed = apply_text_transform(raw, b.style.text.text_transform);
            tokenize_text(&transformed, &b.style, box_id, stack, out);
        }
        BoxKind::Replaced(ReplacedKind::LineBreak) => {
            out.push(InlineItem::HardBreak);
        }
        BoxKind::InlineBlock { .. } | BoxKind::Replaced(_) => {
            out.push(InlineItem::Atomic {
                box_id,
                style: Arc::clone(&b.style),
                inline_stack: stack.clone(),
            });
        }
        _ => {}
    }
}

fn tokenize_text(
    text: &str,
    style: &Arc<ComputedStyle>,
    box_id: BoxId,
    stack: &[ActiveInline],
    out: &mut Vec<InlineItem>,
) {
    let ws = style.text.white_space;
    let preserve_newlines = matches!(
        ws,
        WhiteSpaceMode::Pre
            | WhiteSpaceMode::PreWrap
            | WhiteSpaceMode::PreLine
            | WhiteSpaceMode::BreakSpaces
    );
    let preserve_spaces = matches!(
        ws,
        WhiteSpaceMode::Pre | WhiteSpaceMode::PreWrap | WhiteSpaceMode::BreakSpaces
    );

    if preserve_spaces {
        for (line_idx, segment) in text.split('\n').enumerate() {
            if line_idx > 0 {
                out.push(InlineItem::HardBreak);
            }
            if !segment.is_empty() {
                let mut word = String::new();
                let mut in_space = false;
                for ch in segment.chars() {
                    let sp = ch == ' ' || ch == '\t';
                    if !word.is_empty() && sp != in_space {
                        out.push(InlineItem::Word {
                            text: std::mem::take(&mut word),
                            style: Arc::clone(style),
                            box_id,
                            is_space: in_space,
                            inline_stack: stack.to_vec(),
                        });
                    }
                    in_space = sp;
                    if ch == '\t' {
                        word.push_str("    ");
                    } else {
                        word.push(ch);
                    }
                }
                if !word.is_empty() {
                    out.push(InlineItem::Word {
                        text: word,
                        style: Arc::clone(style),
                        box_id,
                        is_space: in_space,
                        inline_stack: stack.to_vec(),
                    });
                }
            }
        }
        return;
    }

    // Collapsible whitespace (`normal`, `nowrap`, `pre-line`).
    let mut word = String::new();
    let mut pending_space = false;
    for ch in text.chars() {
        if preserve_newlines && ch == '\n' {
            if !word.is_empty() {
                out.push(InlineItem::Word {
                    text: std::mem::take(&mut word),
                    style: Arc::clone(style),
                    box_id,
                    is_space: false,
                    inline_stack: stack.to_vec(),
                });
            }
            pending_space = false;
            out.push(InlineItem::HardBreak);
            continue;
        }
        if ch.is_whitespace() {
            if !word.is_empty() {
                out.push(InlineItem::Word {
                    text: std::mem::take(&mut word),
                    style: Arc::clone(style),
                    box_id,
                    is_space: false,
                    inline_stack: stack.to_vec(),
                });
            }
            pending_space = true;
        } else {
            if pending_space {
                out.push(InlineItem::Word {
                    text: " ".to_string(),
                    style: Arc::clone(style),
                    box_id,
                    is_space: true,
                    inline_stack: stack.to_vec(),
                });
                pending_space = false;
            }
            word.push(ch);
        }
    }
    if !word.is_empty() {
        out.push(InlineItem::Word {
            text: word,
            style: Arc::clone(style),
            box_id,
            is_space: false,
            inline_stack: stack.to_vec(),
        });
    }
    if pending_space {
        out.push(InlineItem::Word {
            text: " ".to_string(),
            style: Arc::clone(style),
            box_id,
            is_space: true,
            inline_stack: stack.to_vec(),
        });
    }
}

fn apply_text_transform(text: &str, mode: TextTransformMode) -> String {
    match mode {
        TextTransformMode::None => text.to_string(),
        TextTransformMode::Uppercase => text.to_uppercase(),
        TextTransformMode::Lowercase => text.to_lowercase(),
        TextTransformMode::Capitalize => {
            let mut out = String::with_capacity(text.len());
            let mut prev_ws = true;
            for ch in text.chars() {
                if prev_ws && ch.is_alphabetic() {
                    for uc in ch.to_uppercase() {
                        out.push(uc);
                    }
                } else {
                    out.push(ch);
                }
                prev_ws = ch.is_whitespace();
            }
            out
        }
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

    let runs = ctx.shaper.shape_text(text, font_id, font_size);
    if runs.is_empty() {
        // Deterministic fallback width when running in an environment with no system fonts installed.
        let approx = text.chars().count() as f32 * (font_size * 0.56) + word_sp;
        return (Vec::new(), approx);
    }

    let mut glyphs = Vec::new();
    let mut pen_x = 0.0f32;
    for run in runs {
        for g in run.glyphs {
            let adv = (g.x_advance + letter_sp + word_sp).max(0.0);
            glyphs.push(PositionedGlyph {
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
