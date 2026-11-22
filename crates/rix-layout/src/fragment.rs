//! Immutable Fragment Tree, box geometry resolution, formatting-context
//! dispatch, and per-document incremental [`LayoutCache`].

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Arc;

use rix_core::{Point, Rect, Size};
use rix_css::{
    BoxSizingMode, ColorRgba, ComputedStyle, EvalContext, FlexDirectionMode, FlexWrapMode,
    LengthPercentageAuto, LineHeightValue, SizeValue, TextDecorationLine,
};
use rix_dom::NodeId;
use rix_ipc::{ImageId, SurfaceId};
use rix_text::{FontDatabase, FontFaceSet, FontId, TextShaper, TextShapingCache};

use crate::bfc::layout_bfc_root;
use crate::box_tree::{
    BoxId, BoxKind, BoxTree, FormControlKind, FormattingContextKind, ReplacedKind, SvgPathEntry,
};
use crate::flex_grid::{layout_flex_grid_container, RetainedTaffyContainer};
use crate::ifc::{measure_ifc_min_max_content, resolve_line_height_px};
use crate::table::layout_table;

/// Four-sided resolved geometry insets (padding, border, or margin).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EdgeInsets {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl EdgeInsets {
    pub const ZERO: Self = Self {
        top: 0.0,
        right: 0.0,
        bottom: 0.0,
        left: 0.0,
    };

    #[inline]
    #[must_use]
    pub fn horizontal(&self) -> f32 {
        self.left + self.right
    }

    #[inline]
    #[must_use]
    pub fn vertical(&self) -> f32 {
        self.top + self.bottom
    }
}

/// Available layout space passed down to a formatting context.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConstraintSpace {
    pub available_width: f32,
    pub available_height: Option<f32>,
    pub containing_block_width: f32,
    pub containing_block_height: f32,
    pub viewport_size: Size,
    pub root_font_size_px: f32,
}

impl ConstraintSpace {
    #[must_use]
    pub fn for_viewport(viewport: Size) -> Self {
        Self {
            available_width: viewport.width,
            available_height: Some(viewport.height),
            containing_block_width: viewport.width,
            containing_block_height: viewport.height,
            viewport_size: viewport,
            root_font_size_px: 16.0,
        }
    }

    #[must_use]
    pub fn eval_context(&self, font_size_px: f32) -> EvalContext {
        EvalContext {
            font_size_px,
            root_font_size_px: self.root_font_size_px,
            viewport_width_px: self.viewport_size.width,
            viewport_height_px: self.viewport_size.height,
            container_width_px: self.containing_block_width,
            container_height_px: self.containing_block_height,
        }
    }

    #[must_use]
    pub fn with_available(&self, width: f32, height: Option<f32>) -> Self {
        Self {
            available_width: width.max(0.0),
            available_height: height.map(|h| h.max(0.0)),
            containing_block_width: width.max(0.0),
            containing_block_height: height.unwrap_or(self.containing_block_height).max(0.0),
            viewport_size: self.viewport_size,
            root_font_size_px: self.root_font_size_px,
        }
    }
}

/// Resolved single-box metrics for margins, borders, padding, and specified sizes.
#[derive(Debug, Clone, Copy)]
pub struct BoxMetrics {
    pub padding: EdgeInsets,
    pub border: EdgeInsets,
    pub margin: EdgeInsets,
    pub margin_left_auto: bool,
    pub margin_right_auto: bool,
    pub specified_width: Option<f32>,
    pub specified_height: Option<f32>,
    pub min_width: f32,
    pub max_width: f32,
    pub min_height: f32,
    pub max_height: f32,
}

impl BoxMetrics {
    #[must_use]
    pub fn resolve(style: &ComputedStyle, space: &ConstraintSpace) -> Self {
        let eval = space.eval_context(style.text.font_size_px);
        let cb_w = space.containing_block_width;
        let cb_h = space.containing_block_height;
        let bm = &style.box_model;

        let padding = EdgeInsets {
            top: bm.padding.top.resolve_px(cb_w, &eval).max(0.0),
            right: bm.padding.right.resolve_px(cb_w, &eval).max(0.0),
            bottom: bm.padding.bottom.resolve_px(cb_w, &eval).max(0.0),
            left: bm.padding.left.resolve_px(cb_w, &eval).max(0.0),
        };
        let border = EdgeInsets {
            top: resolve_border_side(bm.border_width.top, style.visual.border_style.top),
            right: resolve_border_side(bm.border_width.right, style.visual.border_style.right),
            bottom: resolve_border_side(bm.border_width.bottom, style.visual.border_style.bottom),
            left: resolve_border_side(bm.border_width.left, style.visual.border_style.left),
        };
        let margin_left_auto = matches!(bm.margin.left, LengthPercentageAuto::Auto);
        let margin_right_auto = matches!(bm.margin.right, LengthPercentageAuto::Auto);
        let margin = EdgeInsets {
            top: resolve_lpa(&bm.margin.top, cb_w, &eval),
            right: resolve_lpa(&bm.margin.right, cb_w, &eval),
            bottom: resolve_lpa(&bm.margin.bottom, cb_w, &eval),
            left: resolve_lpa(&bm.margin.left, cb_w, &eval),
        };

        let pb_w = padding.horizontal() + border.horizontal();
        let pb_h = padding.vertical() + border.vertical();
        let is_border_box = style.box_sizing == BoxSizingMode::BorderBox;

        let specified_width = resolve_size_value(&bm.width, cb_w, &eval)
            .map(|v| to_content_dim(v, pb_w, is_border_box));
        let specified_height = space
            .available_height
            .and_then(|ah| {
                resolve_size_value(&bm.height, ah, &eval)
                    .map(|v| to_content_dim(v, pb_h, is_border_box))
            })
            .or_else(|| match &bm.height {
                SizeValue::Length(lp) if !is_percent_dep(lp) => Some(to_content_dim(
                    lp.resolve_px(cb_h, &eval),
                    pb_h,
                    is_border_box,
                )),
                _ => None,
            });

        let min_width = resolve_size_value(&bm.min_width, cb_w, &eval)
            .map_or(0.0, |v| to_content_dim(v, pb_w, is_border_box));
        let max_width = resolve_size_value(&bm.max_width, cb_w, &eval)
            .map_or(f32::INFINITY, |v| to_content_dim(v, pb_w, is_border_box));
        let min_height = resolve_size_value(&bm.min_height, cb_h, &eval)
            .map_or(0.0, |v| to_content_dim(v, pb_h, is_border_box));
        let max_height = resolve_size_value(&bm.max_height, cb_h, &eval)
            .map_or(f32::INFINITY, |v| to_content_dim(v, pb_h, is_border_box));

        Self {
            padding,
            border,
            margin,
            margin_left_auto,
            margin_right_auto,
            specified_width,
            specified_height,
            min_width,
            max_width,
            min_height,
            max_height,
        }
    }

    #[inline]
    #[must_use]
    pub fn clamp_content_width(&self, w: f32) -> f32 {
        w.min(self.max_width).max(self.min_width).max(0.0)
    }

    #[inline]
    #[must_use]
    pub fn clamp_content_height(&self, h: f32) -> f32 {
        h.min(self.max_height).max(self.min_height).max(0.0)
    }
}

#[inline]
fn resolve_border_side(width: f32, style: rix_css::LineStyle) -> f32 {
    if matches!(style, rix_css::LineStyle::None | rix_css::LineStyle::Hidden) {
        0.0
    } else {
        width.max(0.0)
    }
}

#[inline]
fn resolve_lpa(v: &LengthPercentageAuto, basis: f32, eval: &EvalContext) -> f32 {
    match v {
        LengthPercentageAuto::Auto => 0.0,
        LengthPercentageAuto::Value(lp) => lp.resolve_px(basis, eval),
    }
}

#[inline]
fn resolve_size_value(v: &SizeValue, basis: f32, eval: &EvalContext) -> Option<f32> {
    match v {
        SizeValue::Length(lp) => Some(lp.resolve_px(basis, eval).max(0.0)),
        _ => None,
    }
}

#[inline]
fn to_content_dim(val: f32, pb: f32, is_border_box: bool) -> f32 {
    if is_border_box {
        (val - pb).max(0.0)
    } else {
        val.max(0.0)
    }
}

fn is_percent_dep(lp: &rix_css::LengthPercentage) -> bool {
    matches!(
        lp,
        rix_css::LengthPercentage::Percent(_) | rix_css::LengthPercentage::Calc(_)
    )
}

/// Positioned shaped glyph within a [`ShapedTextFragment`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionedGlyph {
    pub glyph_id: u32,
    pub x: f32,
    pub y: f32,
    pub advance: f32,
}

/// Shaped text run stored on a [`FragmentKind::TextRun`] fragment.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapedTextFragment {
    pub font_id: FontId,
    pub font_size: f32,
    pub color: ColorRgba,
    pub glyphs: Vec<PositionedGlyph>,
    pub baseline_y: f32,
    pub width: f32,
    pub text: String,
    pub decoration: TextDecorationLine,
}

/// Semantic kind of an immutable layout [`Fragment`].
#[derive(Debug, Clone, PartialEq)]
pub enum FragmentKind {
    Box,
    InlineBox {
        is_first: bool,
        is_last: bool,
    },
    LineBox {
        baseline: f32,
    },
    TextRun(ShapedTextFragment),
    Image {
        image_id: Option<ImageId>,
        intrinsic_size: Size,
    },
    Svg {
        view_box: Rect,
        paths: Vec<SvgPathEntry>,
    },
    FormControl {
        kind: FormControlKind,
        text: String,
        placeholder: String,
    },
    ExternalSurface {
        surface_id: SurfaceId,
        intrinsic_size: Size,
    },
}

/// Immutable positioned layout output fragment.
///
/// `border_rect.origin` is always relative to the parent fragment's border-box origin.
#[derive(Debug, Clone)]
pub struct Fragment {
    pub box_id: BoxId,
    pub dom_node: Option<NodeId>,
    pub style: Arc<ComputedStyle>,
    pub border_rect: Rect,
    pub padding: EdgeInsets,
    pub border: EdgeInsets,
    pub margin: EdgeInsets,
    pub content_rect: Rect,
    pub baseline: Option<f32>,
    pub overflow_rect: Rect,
    pub kind: FragmentKind,
    pub children: Arc<Vec<Fragment>>,
    pub is_float: bool,
    pub is_out_of_flow: bool,
    pub collapsed_margin_top: f32,
    pub collapsed_margin_bottom: f32,
}

impl Fragment {
    /// Returns the border rect translated into absolute document coordinates given `parent_origin`.
    #[inline]
    #[must_use]
    pub fn absolute_border_rect(&self, parent_origin: Point) -> Rect {
        Rect::new(
            parent_origin.x + self.border_rect.origin.x,
            parent_origin.y + self.border_rect.origin.y,
            self.border_rect.size.width,
            self.border_rect.size.height,
        )
    }

    /// Recomputes `overflow_rect` from own `border_rect` and all child overflow rects.
    pub fn compute_overflow_rect(&mut self) {
        let mut min_x = 0.0f32;
        let mut min_y = 0.0f32;
        let mut max_x = self.border_rect.size.width;
        let mut max_y = self.border_rect.size.height;
        for child in self.children.iter() {
            let cx = child.border_rect.origin.x;
            let cy = child.border_rect.origin.y;
            min_x = min_x.min(cx + child.overflow_rect.origin.x);
            min_y = min_y.min(cy + child.overflow_rect.origin.y);
            max_x = max_x.max(cx + child.overflow_rect.max_x());
            max_y = max_y.max(cy + child.overflow_rect.max_y());
        }
        self.overflow_rect = Rect::new(
            min_x,
            min_y,
            (max_x - min_x).max(0.0),
            (max_y - min_y).max(0.0),
        );
    }
}

/// Complete laid-out document fragment tree.
#[derive(Debug, Default)]
pub struct FragmentTree {
    pub root: Option<Fragment>,
    pub content_bounds: Rect,
}

/// Fast FNV-1a hasher for per-document fragment cache keys.
#[derive(Default)]
struct FastFnvHasher(u64);

impl Hasher for FastFnvHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = if self.0 == 0 {
            0xcbf2_9ce4_8422_2325
        } else {
            self.0
        };
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        self.0 = h;
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.0 = self.0.wrapping_mul(0x0100_0000_01b3) ^ i;
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.0 = self.0.wrapping_mul(0x0100_0000_01b3) ^ u64::from(i);
    }
}

type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FastFnvHasher>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FragmentCacheKey {
    dom_node_id: u64,
    subtree_hash: u64,
    avail_w_bits: u32,
    avail_h_bits: u32,
    cb_w_bits: u32,
    cb_h_bits: u32,
}

/// Per-document retained layout cache storing unchanged subtree fragments,
/// font-match memo entries, and retained per-container `taffy::TaffyTree`
/// instances for Flex/Grid contexts.
#[derive(Default)]
pub struct LayoutCache {
    fragments: FastMap<FragmentCacheKey, Arc<Fragment>>,
    font_matches: FastMap<(usize, usize), FontId>,
    pub(crate) taffy_containers: HashMap<u64, RetainedTaffyContainer>,
    pub cache_hits: usize,
    pub cache_misses: usize,
}

impl LayoutCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset_counters(&mut self) {
        self.cache_hits = 0;
        self.cache_misses = 0;
    }

    pub fn clear(&mut self) {
        self.fragments.clear();
        self.font_matches.clear();
        self.taffy_containers.clear();
        self.cache_hits = 0;
        self.cache_misses = 0;
    }

    /// Resolves and caches the matched [`FontId`] for `style.text` in O(1) after first lookup.
    pub fn resolve_font(
        &mut self,
        font_db: &FontDatabase,
        web_fonts: Option<&FontFaceSet>,
        style: &ComputedStyle,
    ) -> FontId {
        let text_ptr = Arc::as_ptr(&style.text) as usize;
        let wf_count = web_fonts.map_or(0, |wf| wf.faces().len());
        let key = (text_ptr, wf_count);
        if let Some(&id) = self.font_matches.get(&key) {
            return id;
        }
        let id = crate::ifc::resolve_font_for_style(font_db, web_fonts, style);
        if self.font_matches.len() > 8192 {
            self.font_matches.clear();
        }
        self.font_matches.insert(key, id);
        id
    }

    fn make_key(
        b: &crate::box_tree::LayoutBox,
        space: &ConstraintSpace,
    ) -> Option<FragmentCacheKey> {
        let dom = b.dom_node?;
        Some(FragmentCacheKey {
            dom_node_id: usize::from(dom) as u64,
            subtree_hash: b.subtree_hash,
            avail_w_bits: space.available_width.to_bits(),
            avail_h_bits: space.available_height.map_or(u32::MAX, f32::to_bits),
            cb_w_bits: space.containing_block_width.to_bits(),
            cb_h_bits: space.containing_block_height.to_bits(),
        })
    }

    pub fn get_fragment(
        &mut self,
        b: &crate::box_tree::LayoutBox,
        space: &ConstraintSpace,
    ) -> Option<Fragment> {
        if b.subtree_dirty {
            return None;
        }
        let key = Self::make_key(b, space)?;
        if let Some(cached) = self.fragments.get(&key) {
            self.cache_hits += 1;
            let mut frag = (**cached).clone();
            frag.box_id = b.id;
            return Some(frag);
        }
        None
    }

    pub fn store_fragment(
        &mut self,
        b: &crate::box_tree::LayoutBox,
        space: &ConstraintSpace,
        fragment: &Fragment,
    ) {
        if let Some(key) = Self::make_key(b, space) {
            if self.fragments.len() > 32_768 {
                self.fragments.clear();
            }
            self.fragments.insert(key, Arc::new(fragment.clone()));
        }
        self.cache_misses += 1;
    }
}

/// Shared layout execution context threaded through all formatting contexts.
pub struct LayoutContext<'a> {
    pub tree: &'a BoxTree,
    pub font_db: &'a FontDatabase,
    pub web_fonts: Option<&'a FontFaceSet>,
    pub shaper: TextShaper<'a>,
    pub cache: &'a mut LayoutCache,
}

impl<'a> LayoutContext<'a> {
    #[must_use]
    pub fn new(
        tree: &'a BoxTree,
        font_db: &'a FontDatabase,
        web_fonts: Option<&'a FontFaceSet>,
        shaping_cache: Arc<TextShapingCache>,
        cache: &'a mut LayoutCache,
    ) -> Self {
        Self {
            tree,
            font_db,
            web_fonts,
            shaper: TextShaper::with_cache(font_db, shaping_cache),
            cache,
        }
    }
    pub fn resolve_font(&mut self, style: &ComputedStyle) -> FontId {
        self.cache.resolve_font(self.font_db, self.web_fonts, style)
    }
}
pub fn layout_tree(
    tree: &BoxTree,
    viewport: Size,
    font_db: &FontDatabase,
    web_fonts: Option<&FontFaceSet>,
    shaping_cache: Arc<TextShapingCache>,
    cache: &mut LayoutCache,
) -> FragmentTree {
    cache.reset_counters();
    let Some(root_id) = tree.root else {
        return FragmentTree::default();
    };
    let root_font_size = tree.get(root_id).style.text.font_size_px.max(1.0);
    let mut space = ConstraintSpace::for_viewport(viewport);
    space.root_font_size_px = root_font_size;

    let mut ctx = LayoutContext::new(tree, font_db, web_fonts, shaping_cache, cache);
    let root = layout_box(&mut ctx, root_id, space);
    let content_bounds = Rect::new(
        0.0,
        0.0,
        root.overflow_rect.max_x().max(viewport.width),
        root.overflow_rect.max_y().max(viewport.height),
    );
    FragmentTree {
        root: Some(root),
        content_bounds,
    }
}

/// Lays out any [`LayoutBox`] by dispatching to its formatting context, checking
/// [`LayoutCache`] first when the subtree is clean.
pub fn layout_box(ctx: &mut LayoutContext<'_>, box_id: BoxId, space: ConstraintSpace) -> Fragment {
    let b = ctx.tree.get(box_id);
    // Only reuse cached fragments for boxes that establish an independent formatting context
    // or have no incoming external floats (BFC roots, Flex, Grid, Table, Replaced).
    let cacheable = b.establishes_bfc || matches!(b.kind, BoxKind::Block { .. } | BoxKind::Table);
    if cacheable {
        if let Some(cached) = ctx.cache.get_fragment(b, &space) {
            return cached;
        }
    }

    let frag = match &b.kind {
        BoxKind::Replaced(replaced) => layout_replaced(b, replaced, space),
        BoxKind::Table | BoxKind::AnonymousTable => layout_table(ctx, box_id, space),
        BoxKind::Block {
            formatting_context: FormattingContextKind::Flex | FormattingContextKind::Grid,
        }
        | BoxKind::InlineBlock {
            formatting_context: FormattingContextKind::Flex | FormattingContextKind::Grid,
        } => layout_flex_grid_container(ctx, box_id, space),
        BoxKind::InlineBlock {
            formatting_context: FormattingContextKind::Table,
        } => layout_table(ctx, box_id, space),
        _ => layout_bfc_root(ctx, box_id, space),
    };

    if cacheable {
        ctx.cache
            .store_fragment(ctx.tree.get(box_id), &space, &frag);
    }
    frag
}

/// Computes `(min_content_width, max_content_width)` border-box sizes for any box.
///
/// Used by Table column sizing, shrink-to-fit Floats/`inline-block`, and Taffy
/// Flex/Grid leaf measurement callbacks.
pub fn measure_intrinsic_widths(
    ctx: &mut LayoutContext<'_>,
    box_id: BoxId,
    space: ConstraintSpace,
) -> (f32, f32) {
    let b = ctx.tree.get(box_id);
    let metrics = BoxMetrics::resolve(&b.style, &space);
    let pb_w = metrics.padding.horizontal() + metrics.border.horizontal();

    let is_percent_width = matches!(
        b.style.box_model.width,
        SizeValue::Length(
            rix_css::LengthPercentage::Percent(_) | rix_css::LengthPercentage::Calc(_)
        )
    );
    if !is_percent_width {
        if let Some(spec_w) = metrics.specified_width {
            let total = metrics.clamp_content_width(spec_w) + pb_w;
            return (total, total);
        }
    }

    match &b.kind {
        BoxKind::Replaced(replaced) => {
            let frag = layout_replaced(b, replaced, space);
            (frag.border_rect.size.width, frag.border_rect.size.width)
        }
        BoxKind::Text(text) => {
            let (min_w, max_w) = measure_text_intrinsic(ctx, text, &b.style);
            (min_w, max_w)
        }
        BoxKind::Table | BoxKind::AnonymousTable => {
            crate::table::measure_table_intrinsic_widths(ctx, box_id, space)
        }
        _ => {
            if b.children.is_empty() {
                let w = metrics.clamp_content_width(0.0) + pb_w;
                return (w, w);
            }
            let fc = b.formatting_context();
            let (content_min, content_max) = if fc == FormattingContextKind::Flex {
                let fg = b.style.flex_grid.as_deref();
                let is_row = fg.is_none_or(|f| {
                    matches!(
                        f.flex_direction,
                        FlexDirectionMode::Row | FlexDirectionMode::RowReverse
                    )
                });
                let is_nowrap = fg.is_none_or(|f| f.flex_wrap == FlexWrapMode::NoWrap);
                let col_gap = fg.map_or(0.0, |f| {
                    f.column_gap.resolve_px(
                        space.available_width,
                        &space.eval_context(b.style.text.font_size_px),
                    )
                });
                let mut min_w = 0.0f32;
                let mut max_w = 0.0f32;
                let mut count = 0usize;
                for &cid in &b.children {
                    if ctx.tree.get(cid).is_out_of_flow {
                        continue;
                    }
                    let cm = BoxMetrics::resolve(&ctx.tree.get(cid).style, &space);
                    let (cmin, cmax) = measure_intrinsic_widths(ctx, cid, space);
                    let item_min = cmin + cm.margin.horizontal();
                    let item_max = cmax + cm.margin.horizontal();
                    if is_row {
                        max_w += item_max;
                        if is_nowrap {
                            min_w += item_min;
                        } else {
                            min_w = min_w.max(item_min);
                        }
                    } else {
                        min_w = min_w.max(item_min);
                        max_w = max_w.max(item_max);
                    }
                    count += 1;
                }
                if is_row && count > 1 {
                    let gaps = col_gap * (count - 1) as f32;
                    max_w += gaps;
                    if is_nowrap {
                        min_w += gaps;
                    }
                }
                (min_w, max_w.max(min_w))
            } else {
                let has_block = b
                    .children
                    .iter()
                    .any(|&c| ctx.tree.get(c).is_block_level() || ctx.tree.get(c).is_float);
                if has_block {
                    let mut min_w = 0.0f32;
                    let mut max_w = 0.0f32;
                    for &cid in &b.children {
                        if ctx.tree.get(cid).is_out_of_flow {
                            continue;
                        }
                        let cm = BoxMetrics::resolve(&ctx.tree.get(cid).style, &space);
                        let (cmin, cmax) = measure_intrinsic_widths(ctx, cid, space);
                        min_w = min_w.max(cmin + cm.margin.horizontal());
                        max_w = max_w.max(cmax + cm.margin.horizontal());
                    }
                    (min_w, max_w)
                } else {
                    measure_ifc_min_max_content(ctx, box_id, space)
                }
            };
            (
                metrics.clamp_content_width(content_min) + pb_w,
                metrics.clamp_content_width(content_max) + pb_w,
            )
        }
    }
}

fn measure_text_intrinsic(
    ctx: &mut LayoutContext<'_>,
    text: &str,
    style: &ComputedStyle,
) -> (f32, f32) {
    let font_id = ctx.resolve_font(style);
    let font_size = style.text.font_size_px.max(1.0);
    let space_w: f32 = ctx
        .shaper
        .shape_text(" ", font_id, font_size)
        .iter()
        .map(|r| r.width)
        .sum();
    let mut min_w = 0.0f32;
    let mut words_sum = 0.0f32;
    let mut word_count = 0usize;
    for word in text.split_whitespace() {
        let w: f32 = ctx
            .shaper
            .shape_text(word, font_id, font_size)
            .iter()
            .map(|r| r.width)
            .sum();
        min_w = min_w.max(w);
        words_sum += w;
        word_count += 1;
    }
    if word_count > 1 {
        words_sum += space_w * (word_count - 1) as f32;
    }
    let normalized: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let joined_w: f32 = ctx
        .shaper
        .shape_text(&normalized, font_id, font_size)
        .iter()
        .map(|r| r.width)
        .sum();
    let min_ceil = min_w.ceil();
    let max_ceil = joined_w.max(words_sum).max(min_w).ceil();
    (min_ceil, max_ceil)
}

/// Lays out a replaced element (`<img>`, `<svg>`, form controls, `<br>`, `<hr>`).
#[must_use]
pub fn layout_replaced(
    b: &crate::box_tree::LayoutBox,
    replaced: &ReplacedKind,
    space: ConstraintSpace,
) -> Fragment {
    let metrics = BoxMetrics::resolve(&b.style, &space);
    let pb_w = metrics.padding.horizontal() + metrics.border.horizontal();
    let pb_h = metrics.padding.vertical() + metrics.border.vertical();

    let (content_w, content_h, kind) = match replaced {
        ReplacedKind::LineBreak => {
            let lh =
                resolve_line_height_px(&b.style, space.eval_context(b.style.text.font_size_px));
            (0.0, lh, FragmentKind::Box)
        }
        ReplacedKind::HorizontalRule => {
            let w = metrics
                .specified_width
                .unwrap_or((space.available_width - metrics.margin.horizontal() - pb_w).max(0.0));
            let h = metrics.specified_height.unwrap_or(1.0);
            (
                metrics.clamp_content_width(w),
                metrics.clamp_content_height(h),
                FragmentKind::Box,
            )
        }
        ReplacedKind::Image {
            image_id,
            intrinsic_size,
            ..
        } => {
            let (w, h) = resolve_replaced_size(*intrinsic_size, &metrics);
            (
                w,
                h,
                FragmentKind::Image {
                    image_id: *image_id,
                    intrinsic_size: *intrinsic_size,
                },
            )
        }
        ReplacedKind::ExternalSurface {
            surface_id,
            intrinsic_size,
        } => {
            let (w, h) = resolve_replaced_size(*intrinsic_size, &metrics);
            (
                w,
                h,
                FragmentKind::ExternalSurface {
                    surface_id: *surface_id,
                    intrinsic_size: *intrinsic_size,
                },
            )
        }
        ReplacedKind::Svg {
            view_box,
            intrinsic_size,
            paths,
        } => {
            let (w, h) = resolve_replaced_size(*intrinsic_size, &metrics);
            let vb = view_box.map_or_else(
                || {
                    Rect::new(
                        0.0,
                        0.0,
                        intrinsic_size.width.max(1.0),
                        intrinsic_size.height.max(1.0),
                    )
                },
                |v| Rect::new(v.min_x, v.min_y, v.width, v.height),
            );
            (
                w,
                h,
                FragmentKind::Svg {
                    view_box: vb,
                    paths: paths.clone(),
                },
            )
        }
        ReplacedKind::FormControl {
            kind,
            text,
            placeholder,
            intrinsic_size,
        } => {
            let (w, h) = resolve_replaced_size(*intrinsic_size, &metrics);
            (
                w,
                h,
                FragmentKind::FormControl {
                    kind: *kind,
                    text: text.clone(),
                    placeholder: placeholder.clone(),
                },
            )
        }
    };

    let border_w = content_w + pb_w;
    let border_h = content_h + pb_h;
    let baseline = match &b.style.text.line_height {
        LineHeightValue::Normal => Some(border_h),
        _ => Some(border_h),
    };
    Fragment {
        box_id: b.id,
        dom_node: b.dom_node,
        style: Arc::clone(&b.style),
        border_rect: Rect::new(0.0, 0.0, border_w, border_h),
        padding: metrics.padding,
        border: metrics.border,
        margin: metrics.margin,
        content_rect: Rect::new(
            metrics.border.left + metrics.padding.left,
            metrics.border.top + metrics.padding.top,
            content_w,
            content_h,
        ),
        baseline,
        overflow_rect: Rect::new(0.0, 0.0, border_w, border_h),
        kind,
        children: Arc::new(Vec::new()),
        is_float: b.is_float,
        is_out_of_flow: b.is_out_of_flow,
        collapsed_margin_top: metrics.margin.top,
        collapsed_margin_bottom: metrics.margin.bottom,
    }
}

fn resolve_replaced_size(intrinsic: Size, metrics: &BoxMetrics) -> (f32, f32) {
    let ratio = if intrinsic.height > 0.0 && intrinsic.width > 0.0 {
        Some(intrinsic.width / intrinsic.height)
    } else {
        None
    };
    let (w, h) = match (metrics.specified_width, metrics.specified_height, ratio) {
        (Some(w), Some(h), _) => (w, h),
        (Some(w), None, Some(r)) => (w, (w / r).max(0.0)),
        (None, Some(h), Some(r)) => ((h * r).max(0.0), h),
        (Some(w), None, None) => (w, intrinsic.height),
        (None, Some(h), None) => (intrinsic.width, h),
        (None, None, _) => (intrinsic.width, intrinsic.height),
    };
    (
        metrics.clamp_content_width(w),
        metrics.clamp_content_height(h),
    )
}
