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
use crate::flex_grid::RetainedTaffyContainer;
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
    /// Optional containing-block inline size for resolving percentage `margin`/`padding`
    /// when the containing block has a vertical `writing-mode` (CSS Box Model L3 / Writing Modes L3 §7.1).
    /// When `None`, defaults to `containing_block_width`. Cleared by [`Self::with_available`].
    pub cb_inline_size: Option<f32>,
    /// Optional forced definite content width imposed by a parent formatting context
    /// (e.g. stretched inset-modified width on an out-of-flow box per CSS Position L3 §5.1).
    /// Cleared by [`Self::with_available`] so it applies only to the direct child box.
    pub definite_width_override: Option<f32>,
    /// Optional forced definite content height imposed by a parent formatting context
    /// (e.g. stretched or flexed height on a CSS Flex item per Flexbox L1 §9.8).
    /// Cleared by [`Self::with_available`] so it applies only to the direct child box.
    pub definite_height_override: Option<f32>,
    /// When `true`, measures raw intrinsic content height of the direct child box ignoring
    /// its own `height`/`min-height`/`max-height` (used for `flex-basis: min-content|max-content|content`
    /// and automatic minimum height probing in column flex containers). Cleared by [`Self::with_available`].
    pub ignore_specified_height: bool,
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
            cb_inline_size: None,
            definite_width_override: None,
            definite_height_override: None,
            ignore_specified_height: false,
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
            ..EvalContext::default()
        }
    }

    #[must_use]
    pub fn eval_context_for(&self, style: &ComputedStyle) -> EvalContext {
        let mut ctx = style.text.eval_ctx;
        ctx.font_size_px = style.text.font_size_px;
        ctx.root_font_size_px = self.root_font_size_px;
        ctx.viewport_width_px = self.viewport_size.width;
        ctx.viewport_height_px = self.viewport_size.height;
        ctx.container_width_px = self.containing_block_width;
        ctx.container_height_px = self.containing_block_height;
        ctx
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
            cb_inline_size: None,
            definite_width_override: None,
            definite_height_override: None,
            ignore_specified_height: false,
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
        Self::resolve_with_eval(style, space, space.eval_context_for(style))
    }

    #[must_use]
    pub fn resolve_with_eval(
        style: &ComputedStyle,
        space: &ConstraintSpace,
        eval: EvalContext,
    ) -> Self {
        let cb_w = space.containing_block_width;
        let cb_h = space.containing_block_height;
        let cb_inline = space.cb_inline_size.unwrap_or(cb_w);
        let bm = &style.box_model;

        let padding = EdgeInsets {
            top: bm.padding.top.resolve_px(cb_inline, &eval).max(0.0),
            right: bm.padding.right.resolve_px(cb_inline, &eval).max(0.0),
            bottom: bm.padding.bottom.resolve_px(cb_inline, &eval).max(0.0),
            left: bm.padding.left.resolve_px(cb_inline, &eval).max(0.0),
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
            top: resolve_lpa(&bm.margin.top, cb_inline, &eval),
            right: resolve_lpa(&bm.margin.right, cb_inline, &eval),
            bottom: resolve_lpa(&bm.margin.bottom, cb_inline, &eval),
            left: resolve_lpa(&bm.margin.left, cb_inline, &eval),
        };

        let pb_w = padding.horizontal() + border.horizontal();
        let pb_h = padding.vertical() + border.vertical();
        let is_border_box = style.box_sizing == BoxSizingMode::BorderBox;

        let resolve_h = |sz: &SizeValue| -> Option<f32> {
            match sz {
                SizeValue::Length(lp)
                    if !lp.has_percentage() || space.available_height.is_some() =>
                {
                    let basis = space.available_height.unwrap_or(cb_h);
                    Some(to_content_dim(
                        lp.resolve_px(basis, &eval).max(0.0),
                        pb_h,
                        is_border_box,
                    ))
                }
                SizeValue::Stretch => space
                    .available_height
                    .map(|ah| (ah - margin.vertical() - pb_h).max(0.0)),
                SizeValue::CalcSize(cs) if cs.is_definite(space.available_height.is_some()) => {
                    let st = space.available_height.map_or(0.0, |ah| {
                        let c = (ah - margin.vertical() - pb_h).max(0.0);
                        if is_border_box {
                            c + pb_h
                        } else {
                            c
                        }
                    });
                    Some(to_content_dim(
                        cs.eval_px(space.available_height, &eval, &|_| st),
                        pb_h,
                        is_border_box,
                    ))
                }
                _ => None,
            }
        };
        let specified_width = match &bm.width {
            SizeValue::Length(lp) => Some(to_content_dim(
                lp.resolve_px(cb_w, &eval).max(0.0),
                pb_w,
                is_border_box,
            )),
            SizeValue::Stretch => {
                Some((space.available_width - margin.horizontal() - pb_w).max(0.0))
            }
            _ => None,
        };
        let specified_height = resolve_h(&bm.height);

        let min_width = resolve_size_value(&bm.min_width, cb_w, &eval)
            .map_or(0.0, |v| to_content_dim(v, pb_w, is_border_box));
        let max_width = resolve_size_value(&bm.max_width, cb_w, &eval)
            .map_or(f32::INFINITY, |v| to_content_dim(v, pb_w, is_border_box));
        let min_height = resolve_h(&bm.min_height).unwrap_or(0.0);
        let max_height = resolve_h(&bm.max_height).unwrap_or(f32::INFINITY);

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

/// Resolves any CSS `SizeValue` (including `calc-size()`, `stretch`, `min-content`,
/// `max-content`, `fit-content`) to a content-box dimension given an intrinsic content-box closure.
#[must_use]
pub fn resolve_size_with_intrinsic<F>(
    v: &SizeValue,
    avail: Option<f32>,
    pb: f32,
    margin_sum: f32,
    is_border_box: bool,
    eval: &EvalContext,
    intrinsic_content: F,
) -> Option<f32>
where
    F: Fn(&rix_css::values_math::CalcSizeBasis) -> f32,
{
    use rix_css::values_math::CalcSizeBasis;
    match v {
        SizeValue::Length(lp) => {
            if avail.is_none() && lp.has_percentage() {
                None
            } else {
                Some(to_content_dim(
                    lp.resolve_px(avail.unwrap_or(0.0), eval).max(0.0),
                    pb,
                    is_border_box,
                ))
            }
        }
        SizeValue::Stretch => avail.map(|a| (a - margin_sum - pb).max(0.0)),
        SizeValue::MinContent => Some(intrinsic_content(&CalcSizeBasis::MinContent).max(0.0)),
        SizeValue::MaxContent => Some(intrinsic_content(&CalcSizeBasis::MaxContent).max(0.0)),
        SizeValue::FitContent(None) => Some(intrinsic_content(&CalcSizeBasis::FitContent).max(0.0)),
        SizeValue::FitContent(Some(lp)) => {
            let lim = to_content_dim(
                lp.resolve_px(avail.unwrap_or(0.0), eval).max(0.0),
                pb,
                is_border_box,
            );
            let min_c = intrinsic_content(&CalcSizeBasis::MinContent);
            let max_c = intrinsic_content(&CalcSizeBasis::MaxContent);
            Some(max_c.min(lim).max(min_c).max(0.0))
        }
        SizeValue::CalcSize(cs) => {
            let box_intrinsic = |b: &CalcSizeBasis| -> f32 {
                let c = intrinsic_content(b).max(0.0);
                if is_border_box {
                    c + pb
                } else {
                    c
                }
            };
            let raw = cs.eval_px(avail, eval, &box_intrinsic);
            Some(to_content_dim(raw, pb, is_border_box))
        }
        SizeValue::Auto | SizeValue::None | SizeValue::Content => None,
    }
}

#[inline]
pub(crate) fn size_value_needs_intrinsic(v: &SizeValue) -> bool {
    matches!(
        v,
        SizeValue::MinContent
            | SizeValue::MaxContent
            | SizeValue::FitContent(_)
            | SizeValue::CalcSize(_)
    )
}

/// Positioned shaped glyph within a [`ShapedTextFragment`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionedGlyph {
    pub font_id: FontId,
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

/// Resolved CSS Grid track listings for CSS Grid L1 §7.2.6 `getComputedStyle()`
/// (`grid-template-columns`, `grid-template-rows`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedGridTracks {
    pub columns: String,
    pub rows: String,
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
    pub last_baseline: Option<f32>,
    pub overflow_rect: Rect,
    pub kind: FragmentKind,
    pub children: Arc<Vec<Fragment>>,
    pub is_float: bool,
    pub is_out_of_flow: bool,
    pub collapsed_margin_top: f32,
    pub collapsed_margin_bottom: f32,
    pub resolved_grid_tracks: Option<Arc<ResolvedGridTracks>>,
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
    ch_advances: FastMap<(FontId, u32), f32>,
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
        self.ch_advances.clear();
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

    /// Resolves and caches the CSS Values L3 §5.1.1 `1ch` advance measure (`"0"` glyph width).
    pub fn ch_advance_px(
        &mut self,
        font_db: &FontDatabase,
        web_fonts: Option<&FontFaceSet>,
        shaping_cache: &Arc<TextShapingCache>,
        style: &ComputedStyle,
    ) -> f32 {
        if style.text.font_size_px <= 0.0 {
            return 0.0;
        }
        let font_id = self.resolve_font(font_db, web_fonts, style);
        let font_size = style.text.font_size_px;
        let key = (font_id, font_size.to_bits());
        if let Some(&w) = self.ch_advances.get(&key) {
            return w;
        }
        let shaper = TextShaper::with_cache(font_db, Arc::clone(shaping_cache));
        let w: f32 = shaper
            .shape_text("0", font_id, font_size)
            .iter()
            .map(|r| r.width)
            .sum();
        let adv = if w > 0.0 { w } else { font_size * 0.5 };
        if self.ch_advances.len() > 4096 {
            self.ch_advances.clear();
        }
        self.ch_advances.insert(key, adv);
        adv
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

/// Per-pass layout memo key capturing definite override dimensions used by Flex/Grid passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PassFragmentKey {
    pub(crate) box_id: BoxId,
    pub(crate) avail_w: u32,
    pub(crate) avail_h: u32,
    pub(crate) def_w: u32,
    pub(crate) def_h: u32,
    pub(crate) ignore_h: bool,
}

/// Shared layout execution context threaded through all formatting contexts.
pub struct LayoutContext<'a> {
    pub tree: &'a BoxTree,
    pub font_db: &'a FontDatabase,
    pub web_fonts: Option<&'a FontFaceSet>,
    pub shaper: TextShaper<'a>,
    pub cache: &'a mut LayoutCache,
    /// Per-pass memo for `measure_intrinsic_widths` eliminating exponential recursion in nested Flex/Grid/Tables.
    pub(crate) pass_intrinsic_memo: HashMap<(BoxId, u32, u32), (f32, f32)>,
    /// Per-pass memo for `measure_content_intrinsic_widths` called directly by Flex/BFC/Table passes.
    pub(crate) pass_content_intrinsic_memo: HashMap<(BoxId, u32), (f32, f32)>,
    /// Per-pass memo for `layout_box` keyed by full constraint overrides (including Flex/Grid definite sizes).
    /// Also records any out-of-flow (`position: absolute | fixed`) descendants propagated to an ancestor
    /// containing block so memo hits replay them in O(1) instead of forcing exponential re-layout.
    pub(crate) pass_fragment_memo:
        HashMap<PassFragmentKey, (Fragment, Vec<crate::position::PendingOof>)>,
    /// Out-of-flow (`position: absolute | fixed`) boxes propagating upward to their true CSS containing block.
    pub(crate) pending_oof: Vec<crate::position::PendingOof>,
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
            pass_intrinsic_memo: HashMap::with_capacity(256),
            pass_content_intrinsic_memo: HashMap::with_capacity(256),
            pass_fragment_memo: HashMap::with_capacity(256),
            pending_oof: Vec::new(),
        }
    }
    pub fn resolve_font(&mut self, style: &ComputedStyle) -> FontId {
        self.cache.resolve_font(self.font_db, self.web_fonts, style)
    }

    /// Resolves the CSS Values L3 §5.1.1 `1ch` advance measure (`"0"` glyph width) for `style`.
    pub fn ch_advance_px(&mut self, style: &ComputedStyle) -> f32 {
        if style.text.font_size_px <= 0.0 {
            return 0.0;
        }
        let font_id = self.resolve_font(style);
        let font_size = style.text.font_size_px;
        let key = (font_id, font_size.to_bits());
        if let Some(&w) = self.cache.ch_advances.get(&key) {
            return w;
        }
        let w: f32 = self
            .shaper
            .shape_text("0", font_id, font_size)
            .iter()
            .map(|r| r.width)
            .sum();
        let adv = if w > 0.0 { w } else { font_size * 0.5 };
        if self.cache.ch_advances.len() > 4096 {
            self.cache.ch_advances.clear();
        }
        self.cache.ch_advances.insert(key, adv);
        adv
    }

    /// Builds an [`EvalContext`] populated with the element's real font-metric `ch` advance.
    pub fn eval_context_for(
        &mut self,
        style: &ComputedStyle,
        space: &ConstraintSpace,
    ) -> EvalContext {
        let mut eval = space.eval_context_for(style);
        eval.ch_px = Some(self.ch_advance_px(style));
        eval
    }

    /// Resolves [`BoxMetrics`] using the element's real font-metric [`EvalContext`].
    pub fn box_metrics(&mut self, style: &ComputedStyle, space: &ConstraintSpace) -> BoxMetrics {
        let eval = self.eval_context_for(style, space);
        BoxMetrics::resolve_with_eval(style, space, eval)
    }

    #[inline]
    pub(crate) fn quantize_px(v: f32) -> u32 {
        if !v.is_finite() {
            u32::MAX - 1
        } else {
            (v.max(0.0) * 4.0).round() as u32
        }
    }

    #[inline]
    pub(crate) fn quantize_opt_px(v: Option<f32>) -> u32 {
        match v {
            None => u32::MAX,
            Some(x) => Self::quantize_px(x),
        }
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
    static EMPTY_SCROLL: std::sync::OnceLock<HashMap<NodeId, Point>> = std::sync::OnceLock::new();
    let empty = EMPTY_SCROLL.get_or_init(HashMap::new);
    layout_tree_with_scroll(
        tree,
        viewport,
        font_db,
        web_fonts,
        shaping_cache,
        cache,
        empty,
        Point::ZERO,
    )
}

/// Lays out `tree` against `viewport`, resolving remaining out-of-flow boxes against the
/// Initial Containing Block (ICB) and applying `position: sticky` offsets for active scrolls.
#[allow(clippy::too_many_arguments)]
pub fn layout_tree_with_scroll(
    tree: &BoxTree,
    viewport: Size,
    font_db: &FontDatabase,
    web_fonts: Option<&FontFaceSet>,
    shaping_cache: Arc<TextShapingCache>,
    cache: &mut LayoutCache,
    scroll_offsets: &HashMap<NodeId, Point>,
    root_scroll: Point,
) -> FragmentTree {
    cache.reset_counters();
    let Some(root_id) = tree.root else {
        return FragmentTree::default();
    };
    let root_font_size = tree.get(root_id).style.text.font_size_px.max(1.0);
    let mut space = ConstraintSpace::for_viewport(viewport);
    space.root_font_size_px = root_font_size;

    let mut ctx = LayoutContext::new(tree, font_db, web_fonts, shaping_cache, cache);
    let mut root = layout_box(&mut ctx, root_id, space);
    // Drain any out-of-flow (`position: absolute | fixed`) boxes whose containing block
    // is the Initial Containing Block (the viewport `0..viewport.width x 0..viewport.height`).
    if !ctx.pending_oof.is_empty() {
        let icb_rect = Rect::new(
            -root.border_rect.origin.x,
            -root.border_rect.origin.y,
            viewport.width,
            viewport.height,
        );
        let root_style = Arc::clone(&root.style);
        let mut icb_children = Vec::new();
        crate::position::resolve_oof_for_container(
            &mut ctx,
            0,
            &root_style,
            icb_rect,
            true,
            space,
            &mut icb_children,
        );
        if !icb_children.is_empty() {
            Arc::make_mut(&mut root.children).extend(icb_children);
            root.compute_overflow_rect();
        }
    }
    crate::position::apply_sticky_offsets(&mut root, scroll_offsets, root_scroll, viewport);
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
/// per-pass memoization and [`LayoutCache`] first to keep nested Flex/Grid/Table passes strictly linear.
pub fn layout_box(ctx: &mut LayoutContext<'_>, box_id: BoxId, space: ConstraintSpace) -> Fragment {
    let b = ctx.tree.get(box_id);
    // Per-pass memoization covers ALL BFC/Block/Flex/Grid/Table/InlineBlock boxes including
    // Flex and Grid items sized with definite overrides (preventing O(2^depth) exponential
    // re-layout in deeply nested Flex/Grid trees).
    let pass_cacheable = b.establishes_bfc
        || matches!(
            b.kind,
            BoxKind::Block { .. } | BoxKind::InlineBlock { .. } | BoxKind::Table
        );
    let cross_frame_cacheable = pass_cacheable
        && space.definite_width_override.is_none()
        && space.definite_height_override.is_none()
        && !space.ignore_specified_height;
    let pass_key = PassFragmentKey {
        box_id,
        avail_w: LayoutContext::quantize_px(space.available_width),
        avail_h: LayoutContext::quantize_opt_px(space.available_height),
        def_w: LayoutContext::quantize_opt_px(space.definite_width_override),
        def_h: LayoutContext::quantize_opt_px(space.definite_height_override),
        ignore_h: space.ignore_specified_height,
    };
    if pass_cacheable {
        if let Some((hit_frag, hit_oofs)) = ctx.pass_fragment_memo.get(&pass_key) {
            if !hit_oofs.is_empty() {
                ctx.pending_oof.extend(hit_oofs.iter().cloned());
            }
            return hit_frag.clone();
        }
        if cross_frame_cacheable {
            if let Some(cached) = ctx.cache.get_fragment(b, &space) {
                return cached;
            }
        }
    }

    let oof_before = ctx.pending_oof.len();
    let frag = match &b.kind {
        BoxKind::Replaced(replaced) => layout_replaced(b, replaced, space),
        BoxKind::Table | BoxKind::AnonymousTable => layout_table(ctx, box_id, space),
        BoxKind::Block {
            formatting_context: FormattingContextKind::Flex,
        }
        | BoxKind::InlineBlock {
            formatting_context: FormattingContextKind::Flex,
        } => crate::flex::layout_flex_container(ctx, box_id, space),
        BoxKind::Block {
            formatting_context: FormattingContextKind::Grid,
        }
        | BoxKind::InlineBlock {
            formatting_context: FormattingContextKind::Grid,
        } => crate::grid::layout_grid_container(ctx, box_id, space),
        BoxKind::InlineBlock {
            formatting_context: FormattingContextKind::Table,
        } => layout_table(ctx, box_id, space),
        _ => layout_bfc_root(ctx, box_id, space),
    };
    let propagated_oof = ctx.pending_oof.len() > oof_before;

    if pass_cacheable {
        if ctx.pass_fragment_memo.len() < 32_768 {
            let oof_delta = if propagated_oof {
                ctx.pending_oof[oof_before..].to_vec()
            } else {
                Vec::new()
            };
            ctx.pass_fragment_memo
                .insert(pass_key, (frag.clone(), oof_delta));
        }
        if cross_frame_cacheable && !propagated_oof {
            ctx.cache
                .store_fragment(ctx.tree.get(box_id), &space, &frag);
        }
    }
    frag
}

/// Computes `(min_content_width, max_content_width)` border-box sizes for any box.
///
/// Used by Table column sizing, shrink-to-fit Floats/`inline-block`, and Taffy
/// Flex/Grid leaf measurement callbacks. Memoized per layout pass in `O(1)`.
pub fn measure_intrinsic_widths(
    ctx: &mut LayoutContext<'_>,
    box_id: BoxId,
    space: ConstraintSpace,
) -> (f32, f32) {
    let memo_key = (
        box_id,
        LayoutContext::quantize_px(space.available_width),
        LayoutContext::quantize_opt_px(space.available_height),
    );
    if let Some(&hit) = ctx.pass_intrinsic_memo.get(&memo_key) {
        return hit;
    }
    let result = measure_intrinsic_widths_uncached(ctx, box_id, space);
    if ctx.pass_intrinsic_memo.len() < 32_768 {
        ctx.pass_intrinsic_memo.insert(memo_key, result);
    }
    result
}

/// Measures the raw intrinsic content widths `(min_content_px, max_content_px)` of `box_id`'s
/// children without applying `box_id`'s own `width`/`min-width`/`max-width`/padding/border.
/// Memoized per layout pass in `O(1)`.
pub(crate) fn measure_content_intrinsic_widths(
    ctx: &mut LayoutContext<'_>,
    box_id: BoxId,
    space: ConstraintSpace,
) -> (f32, f32) {
    let key = (box_id, LayoutContext::quantize_px(space.available_width));
    if let Some(&hit) = ctx.pass_content_intrinsic_memo.get(&key) {
        return hit;
    }
    let result = measure_content_intrinsic_widths_uncached(ctx, box_id, space);
    if ctx.pass_content_intrinsic_memo.len() < 32_768 {
        ctx.pass_content_intrinsic_memo.insert(key, result);
    }
    result
}

fn measure_content_intrinsic_widths_uncached(
    ctx: &mut LayoutContext<'_>,
    box_id: BoxId,
    space: ConstraintSpace,
) -> (f32, f32) {
    let b = ctx.tree.get(box_id);
    if b.children.is_empty() {
        return (0.0, 0.0);
    }
    let fc = b.formatting_context();
    if fc == FormattingContextKind::Flex {
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
            let mut float_run_max = 0.0f32;
            for &cid in &b.children {
                let cbox = ctx.tree.get(cid);
                if cbox.is_out_of_flow {
                    continue;
                }
                let is_float = cbox.is_float;
                let has_clear = cbox.style.clear_mode != rix_css::ClearMode::None;
                let cm = BoxMetrics::resolve(&cbox.style, &space);
                let (cmin, cmax) = measure_intrinsic_widths(ctx, cid, space);
                let item_min = cmin + cm.margin.horizontal();
                let item_max = cmax + cm.margin.horizontal();
                min_w = min_w.max(item_min);
                if is_float {
                    if has_clear {
                        float_run_max = item_max;
                    } else {
                        float_run_max += item_max;
                    }
                    max_w = max_w.max(float_run_max);
                } else {
                    float_run_max = 0.0;
                    max_w = max_w.max(item_max);
                }
            }
            (min_w, max_w)
        } else {
            measure_ifc_min_max_content(ctx, box_id, space)
        }
    }
}

fn measure_intrinsic_widths_uncached(
    ctx: &mut LayoutContext<'_>,
    box_id: BoxId,
    space: ConstraintSpace,
) -> (f32, f32) {
    let b = ctx.tree.get(box_id);
    // CSS Tables L3: Tables always compute intrinsic widths via the table algorithm because a
    // table's specified `width` is a minimum (never shrinks below grid min-content width) and
    // `min-width`/`max-width` apply in the table's own `box-sizing`.
    if matches!(
        b.kind,
        BoxKind::Table
            | BoxKind::AnonymousTable
            | BoxKind::InlineBlock {
                formatting_context: FormattingContextKind::Table
            }
    ) {
        return crate::table::measure_table_intrinsic_widths(ctx, box_id, space);
    }

    let metrics = BoxMetrics::resolve(&b.style, &space);
    let pb_w = metrics.padding.horizontal() + metrics.border.horizontal();

    let is_percent_width = match &b.style.box_model.width {
        SizeValue::Length(lp) => lp.has_percentage(),
        _ => false,
    };
    let is_percent_max_width = match &b.style.box_model.max_width {
        SizeValue::Length(lp) => lp.has_percentage(),
        _ => false,
    };
    if !is_percent_width
        && !is_percent_max_width
        && !size_value_needs_intrinsic(&b.style.box_model.min_width)
        && !size_value_needs_intrinsic(&b.style.box_model.max_width)
    {
        if let Some(spec_w) = metrics.specified_width {
            let total = metrics.clamp_content_width(spec_w) + pb_w;
            return (total, total);
        }
    }

    match &b.kind {
        BoxKind::Replaced(replaced) => {
            let frag = layout_replaced(b, replaced, space);
            if is_percent_width || is_percent_max_width {
                // CSS Sizing L3 §5.2.1 / CSS Tables L3: a replaced element with a percentage
                // `width` or `max-width` (e.g. `img { max-width: 100% }`) is compressible and
                // contributes `0` (clamped by `min-width`, plus border/padding) to min-content.
                let min_w = metrics.min_width.max(0.0) + pb_w;
                (
                    min_w.min(frag.border_rect.size.width),
                    frag.border_rect.size.width,
                )
            } else {
                (frag.border_rect.size.width, frag.border_rect.size.width)
            }
        }
        BoxKind::Text(text) => {
            let (min_w, max_w) = measure_text_intrinsic(ctx, text, &b.style);
            (min_w, max_w)
        }
        BoxKind::Block {
            formatting_context: FormattingContextKind::Flex,
        }
        | BoxKind::InlineBlock {
            formatting_context: FormattingContextKind::Flex,
        } => crate::flex::measure_flex_intrinsic_widths(ctx, box_id, space),
        BoxKind::Block {
            formatting_context: FormattingContextKind::Grid,
        }
        | BoxKind::InlineBlock {
            formatting_context: FormattingContextKind::Grid,
        } => crate::grid::measure_grid_intrinsic_widths(ctx, box_id, space),
        _ => {
            let (content_min, content_max) = measure_content_intrinsic_widths(ctx, box_id, space);
            let b = ctx.tree.get(box_id);
            let eval = space.eval_context_for(&b.style);
            let is_bb = b.style.box_sizing == BoxSizingMode::BorderBox;
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
                &b.style.box_model.min_width,
                Some(space.containing_block_width),
                pb_w,
                metrics.margin.horizontal(),
                is_bb,
                &eval,
                intr_fn,
            )
            .unwrap_or(metrics.min_width);
            let max_w = resolve_size_with_intrinsic(
                &b.style.box_model.max_width,
                Some(space.containing_block_width),
                pb_w,
                metrics.margin.horizontal(),
                is_bb,
                &eval,
                intr_fn,
            )
            .unwrap_or(metrics.max_width);
            if matches!(b.style.box_model.width, SizeValue::CalcSize(_)) {
                if let Some(cw) = resolve_size_with_intrinsic(
                    &b.style.box_model.width,
                    Some(space.containing_block_width),
                    pb_w,
                    metrics.margin.horizontal(),
                    is_bb,
                    &eval,
                    intr_fn,
                ) {
                    let total = cw.min(max_w).max(min_w).max(0.0) + pb_w;
                    return (total, total);
                }
            }
            if let Some(spec_w) = metrics.specified_width {
                if !is_percent_width {
                    let eff_max = if is_percent_max_width {
                        f32::INFINITY
                    } else {
                        max_w
                    };
                    let max_total = spec_w.min(eff_max).max(min_w).max(0.0) + pb_w;
                    let min_total = if is_percent_max_width {
                        content_min.min(spec_w).max(min_w).max(0.0) + pb_w
                    } else {
                        max_total
                    };
                    return (min_total, max_total);
                }
            }
            let eff_max = if is_percent_max_width {
                f32::INFINITY
            } else {
                max_w
            };
            (
                content_min.min(eff_max).max(min_w).max(0.0) + pb_w,
                content_max.min(eff_max).max(min_w).max(0.0) + pb_w,
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
    let shape_w = |ctx: &mut LayoutContext<'_>, s: &str| -> f32 {
        ctx.shaper
            .shape_text(s, font_id, font_size)
            .iter()
            .map(|r| r.width)
            .sum()
    };

    if style.text.white_space == rix_css::WhiteSpaceMode::Pre {
        let mut max_line = 0.0f32;
        for line in text.split('\n') {
            max_line = max_line.max(shape_w(ctx, line));
        }
        let w = max_line.ceil();
        return (w, w);
    }

    let is_css_ws = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c');
    let space_w = shape_w(ctx, " ");
    let mut min_w = 0.0f32;
    let mut words_sum = 0.0f32;
    let mut word_count = 0usize;
    let mut parts: Vec<&str> = Vec::new();
    for word in text.split(is_css_ws).filter(|w| !w.is_empty()) {
        let w = shape_w(ctx, word);
        min_w = min_w.max(w);
        words_sum += w;
        word_count += 1;
        parts.push(word);
    }
    if word_count > 1 {
        words_sum += space_w * (word_count - 1) as f32;
    }
    let normalized = parts.join(" ");
    let joined_w = shape_w(ctx, &normalized);
    let max_ceil = joined_w.max(words_sum).max(min_w).ceil();
    if style.text.white_space == rix_css::WhiteSpaceMode::Nowrap {
        return (max_ceil, max_ceil);
    }
    let min_ceil = min_w.ceil();
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
            let (w, h) = resolve_replaced_size_styled(*intrinsic_size, &b.style, &space, &metrics);
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
            let (w, h) = resolve_replaced_size_styled(*intrinsic_size, &b.style, &space, &metrics);
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
            let (w, h) = resolve_replaced_size_styled(*intrinsic_size, &b.style, &space, &metrics);
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
    // CSS 2.1 §10.3.4: block-level replaced elements in normal flow honor `margin: auto` centering.
    let resolved_margin = if b.style.display.is_block_level()
        && !b.is_float
        && !b.is_out_of_flow
        && (metrics.margin_left_auto || metrics.margin_right_auto)
    {
        let remaining = (space.available_width - border_w).max(0.0);
        let (ml, mr) = match (metrics.margin_left_auto, metrics.margin_right_auto) {
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
        };
        EdgeInsets {
            top: metrics.margin.top,
            right: mr,
            bottom: metrics.margin.bottom,
            left: ml,
        }
    } else {
        metrics.margin
    };
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
        margin: resolved_margin,
        content_rect: Rect::new(
            metrics.border.left + metrics.padding.left,
            metrics.border.top + metrics.padding.top,
            content_w,
            content_h,
        ),
        baseline,
        last_baseline: baseline,
        overflow_rect: Rect::new(0.0, 0.0, border_w, border_h),
        kind,
        children: Arc::new(Vec::new()),
        is_float: b.is_float,
        is_out_of_flow: b.is_out_of_flow,
        collapsed_margin_top: resolved_margin.top,
        collapsed_margin_bottom: resolved_margin.bottom,
        resolved_grid_tracks: None,
    }
}

fn resolve_replaced_size_styled(
    intrinsic: Size,
    style: &ComputedStyle,
    space: &ConstraintSpace,
    metrics: &BoxMetrics,
) -> (f32, f32) {
    use rix_css::values_math::CalcSizeBasis;
    let eval = space.eval_context_for(style);
    let pb_w = metrics.padding.horizontal() + metrics.border.horizontal();
    let pb_h = metrics.padding.vertical() + metrics.border.vertical();
    let is_bb = style.box_sizing == BoxSizingMode::BorderBox;
    let avail_w = (space.available_width - metrics.margin.horizontal() - pb_w).max(0.0);
    let spec_w = space
        .definite_width_override
        .or(metrics.specified_width)
        .or_else(|| {
            resolve_size_with_intrinsic(
                &style.box_model.width,
                Some(space.containing_block_width),
                pb_w,
                metrics.margin.horizontal(),
                is_bb,
                &eval,
                |b| {
                    if matches!(b, CalcSizeBasis::Stretch) {
                        avail_w
                    } else {
                        intrinsic.width
                    }
                },
            )
        });
    let spec_h = space
        .definite_height_override
        .or(metrics.specified_height)
        .or_else(|| {
            resolve_size_with_intrinsic(
                &style.box_model.height,
                space.available_height,
                pb_h,
                metrics.margin.vertical(),
                is_bb,
                &eval,
                |b| match b {
                    CalcSizeBasis::Stretch => {
                        space.available_height.map_or(intrinsic.height, |ah| {
                            (ah - metrics.margin.vertical() - pb_h).max(0.0)
                        })
                    }
                    _ => intrinsic.height,
                },
            )
        });
    resolve_replaced_pair(intrinsic, spec_w, spec_h, metrics)
}

fn resolve_replaced_size(intrinsic: Size, metrics: &BoxMetrics) -> (f32, f32) {
    resolve_replaced_pair(
        intrinsic,
        metrics.specified_width,
        metrics.specified_height,
        metrics,
    )
}

fn resolve_replaced_pair(
    intrinsic: Size,
    spec_w: Option<f32>,
    spec_h: Option<f32>,
    metrics: &BoxMetrics,
) -> (f32, f32) {
    let ratio = (intrinsic.height > 0.0 && intrinsic.width > 0.0)
        .then(|| intrinsic.width / intrinsic.height);
    let min_w = metrics.min_width.max(0.0);
    let max_w = metrics.max_width.max(min_w);
    let min_h = metrics.min_height.max(0.0);
    let max_h = metrics.max_height.max(min_h);

    match (spec_w, spec_h, ratio) {
        (Some(w), Some(h), _) => (w.min(max_w).max(min_w), h.min(max_h).max(min_h)),
        (Some(w), None, Some(r)) => {
            let cw = w.min(max_w).max(min_w);
            let ch = (cw / r).max(0.0).min(max_h).max(min_h);
            (cw, ch)
        }
        (None, Some(h), Some(r)) => {
            let ch = h.min(max_h).max(min_h);
            let cw = (ch * r).max(0.0).min(max_w).max(min_w);
            (cw, ch)
        }
        (None, None, Some(r)) => {
            // W3C CSS 2.1 §10.4 constraint resolution table for replaced elements with
            // intrinsic aspect ratio `r = w / h` and both `width: auto; height: auto`.
            let w = intrinsic.width;
            let h = intrinsic.height;
            let over_w = w > max_w;
            let under_w = w < min_w;
            let over_h = h > max_h;
            let under_h = h < min_h;
            let (rw, rh) = if over_w && over_h {
                if max_w / w <= max_h / h {
                    (max_w, (max_w / r).max(min_h))
                } else {
                    ((max_h * r).max(min_w), max_h)
                }
            } else if under_w && under_h {
                if min_w / w <= min_h / h {
                    ((min_h * r).min(max_w), min_h)
                } else {
                    (min_w, (min_w / r).min(max_h))
                }
            } else if over_w && under_h {
                (max_w, min_h)
            } else if under_w && over_h {
                (min_w, max_h)
            } else if over_w {
                (max_w, (max_w / r).max(min_h))
            } else if under_w {
                (min_w, (min_w / r).min(max_h))
            } else if over_h {
                ((max_h * r).max(min_w), max_h)
            } else if under_h {
                ((min_h * r).min(max_w), min_h)
            } else {
                (w, h)
            };
            (rw.min(max_w).max(min_w), rh.min(max_h).max(min_h))
        }
        (Some(w), None, None) => (
            w.min(max_w).max(min_w),
            intrinsic.height.min(max_h).max(min_h),
        ),
        (None, Some(h), None) => (
            intrinsic.width.min(max_w).max(min_w),
            h.min(max_h).max(min_h),
        ),
        (None, None, None) => (
            intrinsic.width.min(max_w).max(min_w),
            intrinsic.height.min(max_h).max(min_h),
        ),
    }
}
