//! Pure-Rust W3C CSS Grid Level 1 track expansion, line resolution, auto-placement,
//! §11 Track Sizing Algorithm, §9 abspos geometry, and CSS Box Alignment L3 helpers.
//!
//! Separated from [`crate::grid`] (container/item layout orchestration) to keep both modules
//! cohesive, independently testable, and `#![deny(unsafe_code)]`.

use std::collections::{HashMap, HashSet};

use rix_core::{InternedStr, Point, Rect};
use rix_css::grid_css::{
    GridRepeatCount, GridSubgridLineItem, GridTemplateTrackList, GridTrackListEntry,
};
use rix_css::substitution::escape_css_ident;
use rix_css::values_math::format_math_f64;
use rix_css::{
    AlignValue, ComputedStyle, DirectionMode, EvalContext, GridAutoFlowMode, GridPlacementValue,
    GridTemplateAreasValue, GridTrackSize, LengthPercentage, LengthPercentageAuto,
    TextOrientationMode, WritingMode,
};

use crate::box_tree::BoxId;
use crate::fragment::{BoxMetrics, EdgeInsets, Fragment, LayoutContext};

///
/// In CSS Grid L1, columns lie along the **inline axis** and rows lie along the **block axis**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridAxes {
    /// `true` if the column (inline) axis is physical X (`horizontal-tb`), `false` if physical Y.
    pub col_is_x: bool,
    /// `true` if column line 1 starts at the physical max edge (RTL inline axis).
    pub col_reversed: bool,
    /// `true` if row line 1 starts at the physical max edge (`vertical-rl` / `sideways-rl`).
    pub row_reversed: bool,
}

impl GridAxes {
    #[must_use]
    pub const fn compute(wm: WritingMode, dir: DirectionMode) -> Self {
        let is_rtl = matches!(dir, DirectionMode::Rtl);
        let (col_is_x, col_reversed, row_reversed) = match wm {
            WritingMode::HorizontalTb => (true, is_rtl, false),
            WritingMode::VerticalRl | WritingMode::SidewaysRl => (false, is_rtl, true),
            WritingMode::VerticalLr => (false, is_rtl, false),
            WritingMode::SidewaysLr => (false, !is_rtl, false),
        };
        Self {
            col_is_x,
            col_reversed,
            row_reversed,
        }
    }

    #[inline]
    #[must_use]
    pub const fn item_axis_reversed(
        self,
        is_col: bool,
        item_wm: WritingMode,
        item_dir: DirectionMode,
    ) -> bool {
        let is_rtl = matches!(item_dir, DirectionMode::Rtl);
        let (inline_is_x, inline_rev, block_rev) = match item_wm {
            WritingMode::HorizontalTb => (true, is_rtl, false),
            WritingMode::VerticalRl | WritingMode::SidewaysRl => (false, is_rtl, true),
            WritingMode::VerticalLr => (false, is_rtl, false),
            WritingMode::SidewaysLr => (false, !is_rtl, false),
        };
        let axis_is_x = if is_col {
            self.col_is_x
        } else {
            !self.col_is_x
        };
        if axis_is_x == inline_is_x {
            inline_rev
        } else {
            block_rev
        }
    }

    #[inline]
    #[must_use]
    pub(crate) fn to_logical(self, x: f32, y: f32) -> (f32, f32) {
        if self.col_is_x {
            (x, y)
        } else {
            (y, x)
        }
    }

    #[inline]
    #[must_use]
    pub(crate) fn to_physical(self, col: f32, row: f32) -> (f32, f32) {
        if self.col_is_x {
            (col, row)
        } else {
            (row, col)
        }
    }

    #[inline]
    #[must_use]
    pub(crate) fn col_margins(self, m: EdgeInsets) -> (f32, f32) {
        if self.col_is_x {
            if self.col_reversed {
                (m.right, m.left)
            } else {
                (m.left, m.right)
            }
        } else if self.col_reversed {
            (m.bottom, m.top)
        } else {
            (m.top, m.bottom)
        }
    }

    #[inline]
    #[must_use]
    pub(crate) fn row_margins(self, m: EdgeInsets) -> (f32, f32) {
        if self.col_is_x {
            if self.row_reversed {
                (m.bottom, m.top)
            } else {
                (m.top, m.bottom)
            }
        } else if self.row_reversed {
            (m.right, m.left)
        } else {
            (m.left, m.right)
        }
    }

    #[inline]
    #[must_use]
    pub(crate) fn col_margins_auto(self, style: &ComputedStyle) -> (bool, bool) {
        let m = &style.box_model.margin;
        let (lo, hi) = if self.col_is_x {
            (
                matches!(m.left, LengthPercentageAuto::Auto),
                matches!(m.right, LengthPercentageAuto::Auto),
            )
        } else {
            (
                matches!(m.top, LengthPercentageAuto::Auto),
                matches!(m.bottom, LengthPercentageAuto::Auto),
            )
        };
        if self.col_reversed {
            (hi, lo)
        } else {
            (lo, hi)
        }
    }

    #[inline]
    #[must_use]
    pub(crate) fn row_margins_auto(self, style: &ComputedStyle) -> (bool, bool) {
        let m = &style.box_model.margin;
        let (lo, hi) = if self.col_is_x {
            (
                matches!(m.top, LengthPercentageAuto::Auto),
                matches!(m.bottom, LengthPercentageAuto::Auto),
            )
        } else {
            (
                matches!(m.left, LengthPercentageAuto::Auto),
                matches!(m.right, LengthPercentageAuto::Auto),
            )
        };
        if self.row_reversed {
            (hi, lo)
        } else {
            (lo, hi)
        }
    }
}

/// CSS Grid Level 1 §9 out-of-flow containing-block & static-position metadata attached to a
/// [`PendingOof`] whose layout parent is a CSS Grid container.
#[derive(Debug, Clone, Copy)]
pub struct GridAbsposHint {
    /// `true` while being resolved directly by the grid container itself (cleared when propagating
    /// above a non-CB `position: static` grid container).
    pub is_direct_child: bool,
    /// Resolved CSS Grid L1 §9.1 grid-area containing block in the grid container's border-box coords.
    pub grid_area_cb: Rect,
    /// Grid container's content box (used as the §9.2 static-position rect when the grid container
    /// is NOT the item's containing block, where grid-placement properties do not apply).
    pub content_box_rect: Rect,
    pub axes: GridAxes,
    pub justify_self: AlignValue,
    pub align_self: AlignValue,
    pub justify_items: AlignValue,
    pub align_items: AlignValue,
}

impl GridAbsposHint {
    /// Computes the static margin-box top-left position `(x, y)` of an abspos grid child inside
    /// its static-position alignment rectangle per CSS Grid L1 §9.2 & CSS Box Alignment L3:
    /// - When the grid container IS the containing block (`is_direct_child`), aligns within `grid_area_cb`
    ///   (which equals the container's padding box on `auto/auto` axes).
    /// - When the grid container is NOT the containing block, grid placement does not apply and
    ///   alignment uses `content_box_rect`.
    #[must_use]
    pub fn compute_static_pos(
        &self,
        outer_w: f32,
        outer_h: f32,
        item_wm: WritingMode,
        item_dir: DirectionMode,
    ) -> Point {
        let align_rect = if self.is_direct_child {
            self.grid_area_cb
        } else {
            self.content_box_rect
        };
        let j_eff = if self.justify_self.base() == AlignValue::Auto {
            self.justify_items
        } else {
            self.justify_self
        };
        let a_eff = if self.align_self.base() == AlignValue::Auto {
            self.align_items
        } else {
            self.align_self
        };

        let (cb_col, cb_row) = self
            .axes
            .to_logical(align_rect.size.width, align_rect.size.height);
        let (it_col, it_row) = self.axes.to_logical(outer_w, outer_h);

        let item_col_rev = self.axes.item_axis_reversed(true, item_wm, item_dir);
        let item_row_rev = self.axes.item_axis_reversed(false, item_wm, item_dir);

        let col_off = align_single_in_span(
            cb_col,
            it_col,
            j_eff,
            self.axes.col_is_x,
            self.axes.col_reversed,
            item_col_rev,
        );
        let row_off = align_single_in_span(
            cb_row,
            it_row,
            a_eff,
            !self.axes.col_is_x,
            self.axes.row_reversed,
            item_row_rev,
        );

        let phys_col = if self.axes.col_reversed {
            cb_col - col_off - it_col
        } else {
            col_off
        };
        let phys_row = if self.axes.row_reversed {
            cb_row - row_off - it_row
        } else {
            row_off
        };
        let (dx, dy) = self.axes.to_physical(phys_col, phys_row);
        Point::new(align_rect.origin.x + dx, align_rect.origin.y + dy)
    }
}

// ---------------------------------------------------------------------------
// Alignment math helpers (CSS Box Alignment Level 3)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolvedSelfAlign {
    Start,
    End,
    Center,
    Stretch,
    Baseline,
    LastBaseline,
}

pub(crate) fn resolve_self_align(
    val: AlignValue,
    axis_is_x: bool,
    container_rev: bool,
    item_rev: bool,
    is_replaced: bool,
) -> (ResolvedSelfAlign, bool) {
    let safe = val.is_safe();
    let base = val.base();
    let res = match base {
        AlignValue::Normal | AlignValue::Auto => {
            if is_replaced {
                ResolvedSelfAlign::Start
            } else {
                ResolvedSelfAlign::Stretch
            }
        }
        AlignValue::Stretch => ResolvedSelfAlign::Stretch,
        AlignValue::Start | AlignValue::FlexStart => ResolvedSelfAlign::Start,
        AlignValue::End | AlignValue::FlexEnd => ResolvedSelfAlign::End,
        AlignValue::SelfStart => {
            if item_rev == container_rev {
                ResolvedSelfAlign::Start
            } else {
                ResolvedSelfAlign::End
            }
        }
        AlignValue::SelfEnd => {
            if item_rev == container_rev {
                ResolvedSelfAlign::End
            } else {
                ResolvedSelfAlign::Start
            }
        }
        AlignValue::Left => {
            if axis_is_x {
                if container_rev {
                    ResolvedSelfAlign::End
                } else {
                    ResolvedSelfAlign::Start
                }
            } else {
                ResolvedSelfAlign::Start
            }
        }
        AlignValue::Right => {
            if axis_is_x {
                if container_rev {
                    ResolvedSelfAlign::Start
                } else {
                    ResolvedSelfAlign::End
                }
            } else {
                ResolvedSelfAlign::Start
            }
        }
        AlignValue::Center => ResolvedSelfAlign::Center,
        AlignValue::Baseline => ResolvedSelfAlign::Baseline,
        AlignValue::LastBaseline => ResolvedSelfAlign::LastBaseline,
        _ => ResolvedSelfAlign::Start,
    };
    (res, safe)
}

/// Computes logical offset from logical start edge for a single item of size `item_sz` inside `span`.
fn align_single_in_span(
    span: f32,
    item_sz: f32,
    val: AlignValue,
    axis_is_x: bool,
    container_rev: bool,
    item_rev: bool,
) -> f32 {
    let (resolved, safe) = resolve_self_align(val, axis_is_x, container_rev, item_rev, true);
    let free = span - item_sz;
    if safe && free < 0.0 {
        return 0.0;
    }
    match resolved {
        ResolvedSelfAlign::Start | ResolvedSelfAlign::Stretch | ResolvedSelfAlign::Baseline => 0.0,
        ResolvedSelfAlign::End | ResolvedSelfAlign::LastBaseline => free,
        ResolvedSelfAlign::Center => free * 0.5,
    }
}

/// Computes `(initial_offset, extra_gap_between_tracks)` for CSS Box Alignment L3 content distribution
/// (`justify-content` / `align-content`).
fn compute_content_alignment(
    avail: f32,
    used: f32,
    active_count: usize,
    val: AlignValue,
    axis_is_x: bool,
    container_rev: bool,
) -> (f32, f32) {
    let free = avail - used;
    let safe = val.is_safe();
    let base = val.base();

    if active_count == 0 {
        return (0.0, 0.0);
    }
    if safe && free < 0.0 {
        return (0.0, 0.0);
    }

    match base {
        AlignValue::SpaceBetween => {
            if active_count > 1 && free > 0.0 {
                (0.0, free / (active_count - 1) as f32)
            } else {
                (0.0, 0.0)
            }
        }
        AlignValue::SpaceAround => {
            if free > 0.0 {
                let per = free / active_count as f32;
                (per * 0.5, per)
            } else {
                // Fallback alignment for space-around is safe center (or center when unsafe).
                if safe {
                    (0.0, 0.0)
                } else {
                    (free * 0.5, 0.0)
                }
            }
        }
        AlignValue::SpaceEvenly => {
            if free > 0.0 {
                let per = free / (active_count + 1) as f32;
                (per, per)
            } else if safe {
                (0.0, 0.0)
            } else {
                (free * 0.5, 0.0)
            }
        }
        AlignValue::Center => (free * 0.5, 0.0),
        AlignValue::End | AlignValue::FlexEnd | AlignValue::LastBaseline => (free, 0.0),
        AlignValue::Left if axis_is_x && container_rev => (free, 0.0),
        AlignValue::Right if axis_is_x && !container_rev => (free, 0.0),
        _ => (0.0, 0.0),
    }
}

// ---------------------------------------------------------------------------
// Track Sizing Model (W3C CSS Grid Level 1 §7.2 & §11)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum MinSizing {
    Fixed(f32),
    MinContent,
    MaxContent,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum MaxSizing {
    Fixed(f32),
    MinContent,
    MaxContent,
    Auto,
    Fr(f32),
    FitContent(f32),
}

impl MinSizing {
    fn is_intrinsic(self) -> bool {
        !matches!(self, Self::Fixed(_))
    }

    fn is_content_based(self) -> bool {
        matches!(self, Self::MinContent | Self::MaxContent)
    }
}

impl MaxSizing {
    fn is_intrinsic(self) -> bool {
        matches!(
            self,
            Self::MinContent | Self::MaxContent | Self::Auto | Self::FitContent(_)
        )
    }

    pub(crate) fn is_flexible(self) -> bool {
        matches!(self, Self::Fr(_))
    }

    pub(crate) fn flex_factor(self) -> f32 {
        match self {
            Self::Fr(f) => f,
            _ => 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Track {
    pub(crate) raw: GridTrackSize,
    pub(crate) min_fn: MinSizing,
    pub(crate) max_fn: MaxSizing,
    pub(crate) base_size: f32,
    pub(crate) growth_limit: f32,
    pub(crate) planned_increase: f32,
    pub(crate) item_incurred_increase: f32,
    pub(crate) infinitely_growable: bool,
    pub(crate) from_auto_fit: bool,
    pub(crate) is_collapsed: bool,
}

impl Track {
    fn from_size(
        raw: GridTrackSize,
        avail: Option<f32>,
        eval: &EvalContext,
        from_auto_fit: bool,
    ) -> Self {
        let (min_fn, max_fn) = resolve_track_fns(&raw, avail, eval);
        Self {
            raw,
            min_fn,
            max_fn,
            base_size: 0.0,
            growth_limit: f32::INFINITY,
            planned_increase: 0.0,
            item_incurred_increase: 0.0,
            infinitely_growable: false,
            from_auto_fit,
            is_collapsed: false,
        }
    }

    pub(crate) fn refresh_fns(&mut self, avail: Option<f32>, eval: &EvalContext) {
        let (min_fn, max_fn) = resolve_track_fns(&self.raw, avail, eval);
        self.min_fn = min_fn;
        self.max_fn = max_fn;
    }
}

fn resolve_lp_opt(lp: &LengthPercentage, avail: Option<f32>, eval: &EvalContext) -> Option<f32> {
    if lp.has_percentage() {
        avail.map(|a| lp.resolve_px(a, eval).max(0.0))
    } else {
        Some(lp.resolve_px(0.0, eval).max(0.0))
    }
}

fn resolve_min_fn(sz: &GridTrackSize, avail: Option<f32>, eval: &EvalContext) -> MinSizing {
    match sz {
        GridTrackSize::Auto | GridTrackSize::Fr(_) | GridTrackSize::FrMath(_) => MinSizing::Auto,
        GridTrackSize::MinContent => MinSizing::MinContent,
        GridTrackSize::MaxContent => MinSizing::MaxContent,
        GridTrackSize::Length(lp) => match resolve_lp_opt(lp, avail, eval) {
            Some(px) => MinSizing::Fixed(px),
            None => MinSizing::Auto,
        },
        GridTrackSize::FitContent(_) => MinSizing::Auto,
        GridTrackSize::MinMax(a, _) => resolve_min_fn(a, avail, eval),
    }
}

fn resolve_max_fn(sz: &GridTrackSize, avail: Option<f32>, eval: &EvalContext) -> MaxSizing {
    match sz {
        GridTrackSize::Auto => MaxSizing::Auto,
        GridTrackSize::MinContent => MaxSizing::MinContent,
        GridTrackSize::MaxContent => MaxSizing::MaxContent,
        GridTrackSize::Fr(f) => MaxSizing::Fr((*f).max(0.0)),
        GridTrackSize::FrMath(m) => MaxSizing::Fr((m.eval_scalar(eval, None) as f32).max(0.0)),
        GridTrackSize::Length(lp) => match resolve_lp_opt(lp, avail, eval) {
            Some(px) => MaxSizing::Fixed(px),
            None => MaxSizing::Auto,
        },
        GridTrackSize::FitContent(lp) => match resolve_lp_opt(lp, avail, eval) {
            Some(px) => MaxSizing::FitContent(px),
            None => MaxSizing::Auto,
        },
        GridTrackSize::MinMax(_, b) => resolve_max_fn(b, avail, eval),
    }
}

fn resolve_track_fns(
    sz: &GridTrackSize,
    avail: Option<f32>,
    eval: &EvalContext,
) -> (MinSizing, MaxSizing) {
    match sz {
        GridTrackSize::MinMax(a, b) => (
            resolve_min_fn(a, avail, eval),
            resolve_max_fn(b, avail, eval),
        ),
        other => (
            resolve_min_fn(other, avail, eval),
            resolve_max_fn(other, avail, eval),
        ),
    }
}

/// Fixed contribution of a track size used for `repeat(auto-fill|auto-fit)` repetition count (§7.2.3.2).
pub(crate) fn auto_repeat_track_size(
    sz: &GridTrackSize,
    avail: f32,
    eval: &EvalContext,
) -> Option<f32> {
    match sz {
        GridTrackSize::Length(lp) => Some(lp.resolve_px(avail, eval).max(0.0)),
        GridTrackSize::FitContent(lp) => Some(lp.resolve_px(avail, eval).max(0.0)),
        GridTrackSize::MinMax(a, b) => match (a.as_ref(), b.as_ref()) {
            (GridTrackSize::Length(la), GridTrackSize::Length(lb)) => Some(
                la.resolve_px(avail, eval)
                    .max(0.0)
                    .max(lb.resolve_px(avail, eval).max(0.0)),
            ),
            (_, GridTrackSize::Length(lp)) | (GridTrackSize::Length(lp), _) => {
                Some(lp.resolve_px(avail, eval).max(0.0))
            }
            _ => None,
        },
        _ => None,
    }
}

fn non_repeat_fixed_size(sz: &GridTrackSize, avail: f32, eval: &EvalContext) -> f32 {
    match sz {
        GridTrackSize::Length(lp) | GridTrackSize::FitContent(lp) => {
            lp.resolve_px(avail, eval).max(0.0)
        }
        GridTrackSize::MinMax(a, b) => match (a.as_ref(), b.as_ref()) {
            (GridTrackSize::Length(la), GridTrackSize::Length(lb)) => la
                .resolve_px(avail, eval)
                .max(0.0)
                .max(lb.resolve_px(avail, eval).max(0.0)),
            (GridTrackSize::Length(lp), _) | (_, GridTrackSize::Length(lp)) => {
                lp.resolve_px(avail, eval).max(0.0)
            }
            _ => 0.0,
        },
        _ => 0.0,
    }
}

/// Expanded explicit track definition for one axis (columns or rows).
#[derive(Debug, Clone)]
pub(crate) struct ExplicitAxis {
    pub(crate) tracks: Vec<GridTrackSize>,
    pub(crate) from_auto_fit: Vec<bool>,
    /// Explicit CSS `[line-names]` of length `tracks.len() + 1` (used for both placement and §7.2.6 CSSOM).
    pub(crate) explicit_line_names: Vec<Vec<InternedStr>>,
    /// Lookup map including BOTH explicit `[line-names]` and `grid-template-areas` implicit `-start`/`-end` names.
    pub(crate) lookup_line_names: Vec<Vec<InternedStr>>,
}

impl ExplicitAxis {
    pub(crate) fn track_count(&self) -> usize {
        self.tracks.len()
    }
}

/// Expands a [`GridTemplateTrackList`] into concrete explicit tracks and merged line-name lists.
#[allow(clippy::too_many_arguments)]
pub(crate) fn expand_explicit_axis(
    list: &GridTemplateTrackList,
    areas: &GridTemplateAreasValue,
    auto_tracks: &[GridTrackSize],
    is_row: bool,
    definite_avail: Option<f32>,
    min_avail: Option<f32>,
    max_avail: Option<f32>,
    gap: f32,
    eval: &EvalContext,
    intrinsic_probe: Option<f32>,
) -> ExplicitAxis {
    let mut tracks: Vec<GridTrackSize> = Vec::new();
    let mut from_auto_fit: Vec<bool> = Vec::new();
    let mut explicit_line_names: Vec<Vec<InternedStr>> = vec![Vec::new()];

    if let GridTemplateTrackList::Tracks { items, line_names } = list {
        // Compute auto-repeat count if an auto-fill/auto-fit entry is present.
        let mut auto_rep_count = 1usize;
        if let Some((rep_tracks, is_intrinsic_unit)) = items.iter().find_map(|it| match it {
            GridTrackListEntry::Repeat {
                count: GridRepeatCount::AutoFill | GridRepeatCount::AutoFit,
                tracks: rt,
                ..
            } => {
                let basis = definite_avail.or(max_avail).or(min_avail).unwrap_or(0.0);
                let any_none = rt
                    .iter()
                    .any(|t| auto_repeat_track_size(t, basis, eval).is_none());
                Some((rt.as_slice(), any_none))
            }
            _ => None,
        }) {
            let ref_size = definite_avail.or(max_avail);
            if let Some(avail) = ref_size {
                let mut fixed_other = 0.0f32;
                let mut other_count = 0usize;
                for it in items {
                    match it {
                        GridTrackListEntry::Track(sz) => {
                            fixed_other += non_repeat_fixed_size(sz, avail, eval);
                            other_count += 1;
                        }
                        GridTrackListEntry::Repeat {
                            count, tracks: rt, ..
                        } => {
                            if let Some(n) = count.resolve_number(eval) {
                                let n_us = n as usize;
                                for sz in rt {
                                    fixed_other +=
                                        non_repeat_fixed_size(sz, avail, eval) * n_us as f32;
                                }
                                other_count += rt.len() * n_us;
                            }
                        }
                    }
                }
                let unit_len = rep_tracks.len();
                let unit_sum: f32 = if is_intrinsic_unit {
                    if let Some(probe) = intrinsic_probe.filter(|&v| v > 0.0) {
                        rep_tracks
                            .iter()
                            .map(|t| auto_repeat_track_size(t, avail, eval).unwrap_or(probe))
                            .sum()
                    } else {
                        0.0
                    }
                } else {
                    rep_tracks
                        .iter()
                        .map(|t| auto_repeat_track_size(t, avail, eval).unwrap_or(0.0))
                        .sum()
                };
                let denom = unit_sum + gap * unit_len as f32;
                if denom > 0.0 {
                    let rem = avail - fixed_other - gap * other_count as f32 + gap;
                    let raw = (rem / denom).floor();
                    auto_rep_count = if raw.is_finite() && raw >= 1.0 {
                        (raw as usize).min(10_000)
                    } else {
                        1
                    };
                }
            } else if let Some(min_sz) = min_avail.filter(|&v| v > 0.0) {
                let unit_len = rep_tracks.len();
                let unit_sum: f32 = rep_tracks
                    .iter()
                    .map(|t| auto_repeat_track_size(t, min_sz, eval).unwrap_or(0.0))
                    .sum();
                let denom = unit_sum + gap * unit_len as f32;
                if denom > 0.0 {
                    let raw = ((min_sz + gap) / denom).ceil();
                    auto_rep_count = if raw.is_finite() && raw >= 1.0 {
                        (raw as usize).min(10_000)
                    } else {
                        1
                    };
                }
            }
        }

        if let Some(first_ln) = line_names.first() {
            explicit_line_names[0].extend(first_ln.iter().cloned());
        }
        for (idx, item) in items.iter().enumerate() {
            match item {
                GridTrackListEntry::Track(sz) => {
                    tracks.push(sz.clone());
                    from_auto_fit.push(false);
                    explicit_line_names.push(Vec::new());
                }
                GridTrackListEntry::Repeat {
                    count,
                    tracks: rt,
                    line_names: rln,
                } => {
                    let is_auto_fit = matches!(count, GridRepeatCount::AutoFit);
                    let reps = match count {
                        GridRepeatCount::AutoFill | GridRepeatCount::AutoFit => auto_rep_count,
                        _ => (count.resolve_number(eval).unwrap_or(1) as usize).min(10_000),
                    };
                    for _ in 0..reps {
                        if let Some(lead) = rln.first() {
                            if let Some(last) = explicit_line_names.last_mut() {
                                last.extend(lead.iter().cloned());
                            }
                        }
                        for (ti, tsz) in rt.iter().enumerate() {
                            tracks.push(tsz.clone());
                            from_auto_fit.push(is_auto_fit);
                            let after = rln.get(ti + 1).cloned().unwrap_or_default();
                            explicit_line_names.push(after);
                        }
                    }
                }
            }
            if let Some(after_ln) = line_names.get(idx + 1) {
                if let Some(last) = explicit_line_names.last_mut() {
                    last.extend(after_ln.iter().cloned());
                }
            }
        }
    }

    // Expand explicit grid if `grid-template-areas` defines more rows/cols than track list (§7.3).
    let area_rows = if areas.columns > 0 {
        areas.areas.len() / areas.columns as usize
    } else {
        0
    };
    let area_count = if is_row {
        area_rows
    } else {
        areas.columns as usize
    };
    let explicit_defined_len = tracks.len();
    while tracks.len() < area_count {
        let extra_sz = if auto_tracks.is_empty() {
            GridTrackSize::Auto
        } else {
            let cycle_idx = (tracks.len() - explicit_defined_len) % auto_tracks.len();
            auto_tracks[cycle_idx].clone()
        };
        tracks.push(extra_sz);
        from_auto_fit.push(false);
        explicit_line_names.push(Vec::new());
    }

    // Build lookup line names (explicit names + `<area>-start`/`<area>-end` implicit names).
    let mut lookup_line_names = explicit_line_names.clone();
    if areas.columns > 0 && !areas.areas.is_empty() {
        let mut seen_areas: HashSet<InternedStr> = HashSet::new();
        for name in areas.areas.iter().flatten() {
            if !seen_areas.insert(name.clone()) {
                continue;
            }
            if let Some((rs, re, cs, ce)) = areas.find_area_lines(name.as_str()) {
                let (s_idx, e_idx) = if is_row {
                    ((rs - 1).max(0) as usize, (re - 1).max(0) as usize)
                } else {
                    ((cs - 1).max(0) as usize, (ce - 1).max(0) as usize)
                };
                let start_name = rix_core::intern_str(&format!("{}-start", name.as_str()));
                let end_name = rix_core::intern_str(&format!("{}-end", name.as_str()));
                if let Some(slot) = lookup_line_names.get_mut(s_idx) {
                    slot.push(start_name);
                }
                if let Some(slot) = lookup_line_names.get_mut(e_idx) {
                    slot.push(end_name);
                }
            }
        }
    }

    ExplicitAxis {
        tracks,
        from_auto_fit,
        explicit_line_names,
        lookup_line_names,
    }
}

/// Parent-supplied resolved track state for a CSS Grid Level 2 subgridded axis (§2.3–§2.7).
#[derive(Debug, Clone)]
pub(crate) struct SubgridAxisContext {
    /// Resolved parent track base sizes for the subgrid's spanned tracks (`len == span`).
    pub(crate) track_sizes: Vec<f32>,
    /// Logical track start positions relative to the subgrid's border-box start edge
    /// (already shifted by `-area_start - margin_start`, preserving parent content-distribution gaps).
    pub(crate) track_starts: Vec<f32>,
    /// Effective gap between parent tracks (parent gap + parent content-distribution extra gap).
    pub(crate) parent_gap: f32,
    /// Inherited parent explicit line names for lines `[0..=span]`.
    pub(crate) parent_line_names: Vec<Vec<InternedStr>>,
    /// Subgrid's own border + padding on start/end edges of this axis (subtracted from first/last tracks).
    pub(crate) start_bp: f32,
    pub(crate) end_bp: f32,
    /// Total logical span of the parent grid area minus subgrid margins (= subgrid border-box size).
    pub(crate) border_span: f32,
}

/// Expands a CSS Grid Level 2 `subgrid <line-name-list>?` axis into an [`ExplicitAxis`]
/// with `span` tracks by merging parent line names `[0..=span]` with the subgrid's own line names
/// (including `repeat(auto-fill, ...)` filling up to `span + 1` lines per §2.2).
pub(crate) fn expand_subgrid_axis(
    parent_line_names: &[Vec<InternedStr>],
    sub_items: &[GridSubgridLineItem],
    span: usize,
    eval: &EvalContext,
) -> ExplicitAxis {
    let line_count = span + 1;
    let mut sub_names: Vec<Vec<InternedStr>> = Vec::with_capacity(line_count);

    // Compute auto-fill repetition count if present in `sub_items`.
    let mut fixed_lines = 0usize;
    let mut auto_unit_len = 0usize;
    for item in sub_items {
        match item {
            GridSubgridLineItem::Names(_) => fixed_lines += 1,
            GridSubgridLineItem::Repeat { count, names } => {
                if matches!(count, GridRepeatCount::AutoFill | GridRepeatCount::AutoFit) {
                    auto_unit_len = names.len();
                } else {
                    let reps = count.resolve_number(eval).unwrap_or(1) as usize;
                    fixed_lines += names.len().saturating_mul(reps);
                }
            }
        }
    }
    let auto_reps = if auto_unit_len > 0 && line_count > fixed_lines {
        (line_count - fixed_lines) / auto_unit_len
    } else {
        0
    };

    for item in sub_items {
        match item {
            GridSubgridLineItem::Names(ns) => sub_names.push(ns.clone()),
            GridSubgridLineItem::Repeat { count, names } => {
                let reps = if matches!(count, GridRepeatCount::AutoFill | GridRepeatCount::AutoFit)
                {
                    auto_reps
                } else {
                    (count.resolve_number(eval).unwrap_or(1) as usize).min(10_000)
                };
                for _ in 0..reps {
                    for ns in names {
                        sub_names.push(ns.clone());
                        if sub_names.len() >= line_count {
                            break;
                        }
                    }
                    if sub_names.len() >= line_count {
                        break;
                    }
                }
            }
        }
        if sub_names.len() >= line_count {
            break;
        }
    }

    let mut explicit_line_names: Vec<Vec<InternedStr>> = Vec::with_capacity(line_count);
    for i in 0..line_count {
        let mut merged = Vec::new();
        if let Some(pns) = parent_line_names.get(i) {
            merged.extend(pns.iter().cloned());
        }
        if let Some(sns) = sub_names.get(i) {
            merged.extend(sns.iter().cloned());
        }
        explicit_line_names.push(merged);
    }

    ExplicitAxis {
        tracks: vec![GridTrackSize::Auto; span],
        from_auto_fit: vec![false; span],
        lookup_line_names: explicit_line_names.clone(),
        explicit_line_names,
    }
}

/// Runs §8.5 placement and clamps any subgridded axis into `[0..explicit_count]` so subgrids
/// never generate implicit tracks on subgridded axes (CSS Grid Level 2 §2.2).
#[allow(clippy::too_many_arguments)]
pub(crate) fn place_grid_items_clamped(
    ctx: &LayoutContext<'_>,
    in_flow_ids: &[BoxId],
    explicit_cols: &ExplicitAxis,
    explicit_rows: &ExplicitAxis,
    clamp_cols: bool,
    clamp_rows: bool,
    auto_flow: GridAutoFlowMode,
    eval: &EvalContext,
) -> PlacementResult {
    let mut res = place_grid_items(
        ctx,
        in_flow_ids,
        explicit_cols,
        explicit_rows,
        auto_flow,
        eval,
    );
    if clamp_cols && explicit_cols.track_count() > 0 {
        let max_c = explicit_cols.track_count();
        let shift = (-res.col_min_idx).max(0) as usize;
        for it in &mut res.items {
            let s = it.col_start.saturating_sub(shift).min(max_c - 1);
            let e = it.col_end.saturating_sub(shift).clamp(s + 1, max_c);
            it.col_start = s;
            it.col_end = e;
        }
        res.col_min_idx = 0;
        res.col_total = max_c;
    }
    if clamp_rows && explicit_rows.track_count() > 0 {
        let max_r = explicit_rows.track_count();
        let shift = (-res.row_min_idx).max(0) as usize;
        for it in &mut res.items {
            let s = it.row_start.saturating_sub(shift).min(max_r - 1);
            let e = it.row_end.saturating_sub(shift).clamp(s + 1, max_r);
            it.row_start = s;
            it.row_end = e;
        }
        res.row_min_idx = 0;
        res.row_total = max_r;
    }
    res
}

// ---------------------------------------------------------------------------
// Line Resolution & Auto-Placement (W3C CSS Grid Level 1 §8.3 & §8.5)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) enum AxisPlacement {
    Definite { start: i32, end: i32 },
    Auto { span: i32 },
}

fn eval_line_int(
    val: i32,
    math: Option<&rix_css::values_math::MathNode>,
    eval: &EvalContext,
) -> i32 {
    if let Some(m) = math {
        let v = m.eval_scalar(eval, None).round();
        if v.is_finite() {
            (v.clamp(-10_000.0, 10_000.0)) as i32
        } else {
            1
        }
    } else {
        val
    }
}

fn eval_span_int(
    val: u32,
    math: Option<&rix_css::values_math::MathNode>,
    eval: &EvalContext,
) -> i32 {
    if let Some(m) = math {
        let v = m.eval_scalar(eval, None).round();
        if v.is_finite() {
            (v.clamp(1.0, 10_000.0)) as i32
        } else {
            1
        }
    } else {
        (val.clamp(1, 10_000)) as i32
    }
}

/// Finds the `n`-th (1-based) line index (explicit-relative, `0..=explicit_tracks`) matching `name`.
/// Per CSS Grid L1 §8.3.1, all implicit lines outside `[0..=explicit_tracks]` are treated as having
/// every name when explicit matching lines are exhausted.
fn find_named_line(axis: &ExplicitAxis, name: &str, mut count: i32) -> i32 {
    let explicit_lines = axis.lookup_line_names.len() as i32; // explicit_tracks + 1
    if count > 0 {
        for (i, names) in axis.lookup_line_names.iter().enumerate() {
            if names.iter().any(|n| n.as_str() == name) {
                count -= 1;
                if count == 0 {
                    return i as i32;
                }
            }
        }
        // Remaining count extends into trailing implicit lines (explicit_lines, explicit_lines + 1, ...).
        explicit_lines - 1 + count
    } else {
        let mut rem = -count;
        for (i, names) in axis.lookup_line_names.iter().enumerate().rev() {
            if names.iter().any(|n| n.as_str() == name) {
                rem -= 1;
                if rem == 0 {
                    return i as i32;
                }
            }
        }
        // Remaining count extends into leading implicit lines (-1, -2, ...).
        -rem
    }
}

/// Searches forward (`dir > 0`) or backward (`dir < 0`) from `origin` for the `span`-th line named `name`.
fn span_named_line(axis: &ExplicitAxis, origin: i32, name: &str, span: i32, forward: bool) -> i32 {
    let explicit_max = (axis.lookup_line_names.len() as i32) - 1;
    let mut rem = span.max(1);
    let mut cur = origin;
    if forward {
        while rem > 0 {
            cur += 1;
            if cur > explicit_max
                || (cur >= 0
                    && axis.lookup_line_names[cur as usize]
                        .iter()
                        .any(|n| n.as_str() == name))
            {
                rem -= 1;
            }
        }
    } else {
        while rem > 0 {
            cur -= 1;
            if cur < 0
                || (cur <= explicit_max
                    && axis.lookup_line_names[cur as usize]
                        .iter()
                        .any(|n| n.as_str() == name))
            {
                rem -= 1;
            }
        }
    }
    cur
}

/// Resolves a definite single `<grid-line>` (non-auto, non-span) to a 0-based explicit-relative line index.
fn resolve_definite_line(
    p: &GridPlacementValue,
    axis: &ExplicitAxis,
    is_start: bool,
    eval: &EvalContext,
) -> Option<i32> {
    let explicit_tracks = axis.track_count() as i32;
    match p {
        GridPlacementValue::Auto | GridPlacementValue::Span { .. } => None,
        GridPlacementValue::Named(ident) => {
            let suffix = if is_start {
                format!("{}-start", ident.as_str())
            } else {
                format!("{}-end", ident.as_str())
            };
            if axis
                .lookup_line_names
                .iter()
                .any(|ns| ns.iter().any(|n| n.as_str() == suffix))
            {
                Some(find_named_line(axis, &suffix, 1))
            } else {
                Some(find_named_line(axis, ident.as_str(), 1))
            }
        }
        GridPlacementValue::Line {
            index,
            index_math,
            name,
        } => {
            let n = eval_line_int(*index, index_math.as_deref(), eval);
            if n == 0 {
                return None;
            }
            if let Some(ident) = name {
                Some(find_named_line(axis, ident.as_str(), n))
            } else if n > 0 {
                Some(n - 1)
            } else {
                Some(explicit_tracks + 1 + n)
            }
        }
    }
}

pub(crate) fn resolve_axis_placement(
    start_val: &GridPlacementValue,
    end_val: &GridPlacementValue,
    axis: &ExplicitAxis,
    eval: &EvalContext,
) -> AxisPlacement {
    // §8.3: If both start and end are `span`, treat end as `auto`.
    let eff_end = if matches!(start_val, GridPlacementValue::Span { .. })
        && matches!(end_val, GridPlacementValue::Span { .. })
    {
        &GridPlacementValue::Auto
    } else {
        end_val
    };

    let s_def = resolve_definite_line(start_val, axis, true, eval);
    let e_def = resolve_definite_line(eff_end, axis, false, eval);

    match (s_def, e_def) {
        (Some(mut s), Some(mut e)) => {
            if s > e {
                std::mem::swap(&mut s, &mut e);
            }
            if s == e {
                e = s + 1;
            }
            AxisPlacement::Definite { start: s, end: e }
        }
        (Some(s), None) => {
            let e = match eff_end {
                GridPlacementValue::Span {
                    count,
                    count_math,
                    name,
                } => {
                    let sp = eval_span_int(*count, count_math.as_deref(), eval);
                    if let Some(ident) = name {
                        span_named_line(axis, s, ident.as_str(), sp, true)
                    } else {
                        s + sp
                    }
                }
                _ => s + 1,
            };
            AxisPlacement::Definite {
                start: s,
                end: e.max(s + 1),
            }
        }
        (None, Some(e)) => {
            let s = match start_val {
                GridPlacementValue::Span {
                    count,
                    count_math,
                    name,
                } => {
                    let sp = eval_span_int(*count, count_math.as_deref(), eval);
                    if let Some(ident) = name {
                        span_named_line(axis, e, ident.as_str(), sp, false)
                    } else {
                        e - sp
                    }
                }
                _ => e - 1,
            };
            AxisPlacement::Definite {
                start: s.min(e - 1),
                end: e,
            }
        }
        (None, None) => {
            let span = match (start_val, eff_end) {
                (
                    GridPlacementValue::Span {
                        count,
                        count_math,
                        name,
                    },
                    _,
                )
                | (
                    _,
                    GridPlacementValue::Span {
                        count,
                        count_math,
                        name,
                    },
                ) => {
                    if name.is_some() {
                        // §8.3: `span <ident>` with auto opposite edge has span 1 for auto-placement.
                        1
                    } else {
                        eval_span_int(*count, count_math.as_deref(), eval)
                    }
                }
                _ => 1,
            };
            AxisPlacement::Auto { span: span.max(1) }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct GridItem {
    pub(crate) box_id: BoxId,
    pub(crate) dom_index: usize,
    pub(crate) col_start: usize,
    pub(crate) col_end: usize,
    pub(crate) row_start: usize,
    pub(crate) row_end: usize,
    /// Cached intrinsic contributions for the current track sizing axis.
    pub(crate) min_contrib: f32,
    pub(crate) min_content_contrib: f32,
    pub(crate) max_content_contrib: f32,
    /// Resolved final fragment & box metrics for baseline/alignment passes.
    pub(crate) fragment: Option<Fragment>,
    pub(crate) area_col_start: f32,
    pub(crate) area_col_size: f32,
    pub(crate) area_row_start: f32,
    pub(crate) area_row_size: f32,
}

/// Sparse 2D cell occupancy map supporting negative implicit track indices.
#[derive(Default)]
struct OccupancyGrid {
    cells: HashSet<(i32, i32)>,
}

impl OccupancyGrid {
    fn is_free(&self, r0: i32, r1: i32, c0: i32, c1: i32) -> bool {
        for r in r0..r1 {
            for c in c0..c1 {
                if self.cells.contains(&(r, c)) {
                    return false;
                }
            }
        }
        true
    }

    fn occupy(&mut self, r0: i32, r1: i32, c0: i32, c1: i32) {
        for r in r0..r1 {
            for c in c0..c1 {
                self.cells.insert((r, c));
            }
        }
    }
}

pub(crate) struct PlacementResult {
    pub(crate) items: Vec<GridItem>,
    pub(crate) col_min_idx: i32,
    pub(crate) col_total: usize,
    pub(crate) row_min_idx: i32,
    pub(crate) row_total: usize,
}

/// Runs W3C CSS Grid L1 §8.5 Grid Item Placement Algorithm.
pub(crate) fn place_grid_items(
    ctx: &LayoutContext<'_>,
    in_flow_ids: &[BoxId],
    explicit_cols: &ExplicitAxis,
    explicit_rows: &ExplicitAxis,
    auto_flow: GridAutoFlowMode,
    eval: &EvalContext,
) -> PlacementResult {
    let col_flow = matches!(
        auto_flow,
        GridAutoFlowMode::Column | GridAutoFlowMode::ColumnDense
    );
    let dense = matches!(
        auto_flow,
        GridAutoFlowMode::RowDense | GridAutoFlowMode::ColumnDense
    );

    let mut entries: Vec<(BoxId, usize, i32, AxisPlacement, AxisPlacement)> =
        Vec::with_capacity(in_flow_ids.len());
    for (dom_index, &bid) in in_flow_ids.iter().enumerate() {
        let style = &ctx.tree.get(bid).style;
        let fg = style.flex_grid.as_deref();
        let order = fg.map_or(0, |f| f.order);
        let cs = fg.map_or(&GridPlacementValue::Auto, |f| &f.grid_column_start);
        let ce = fg.map_or(&GridPlacementValue::Auto, |f| &f.grid_column_end);
        let rs = fg.map_or(&GridPlacementValue::Auto, |f| &f.grid_row_start);
        let re = fg.map_or(&GridPlacementValue::Auto, |f| &f.grid_row_end);
        let cp = resolve_axis_placement(cs, ce, explicit_cols, eval);
        let rp = resolve_axis_placement(rs, re, explicit_rows, eval);
        entries.push((bid, dom_index, order, cp, rp));
    }
    entries.sort_by_key(|e| (e.2, e.1));

    let mut occ = OccupancyGrid::default();
    let mut placed: Vec<Option<(i32, i32, i32, i32)>> = vec![None; entries.len()];

    // Step 1: Place items with definite positions on both axes.
    for (i, (_, _, _, cp, rp)) in entries.iter().enumerate() {
        if let (
            AxisPlacement::Definite { start: c0, end: c1 },
            AxisPlacement::Definite { start: r0, end: r1 },
        ) = (cp, rp)
        {
            placed[i] = Some((*c0, *c1, *r0, *r1));
            occ.occupy(*r0, *r1, *c0, *c1);
        }
    }

    // Step 2: Process items locked to the primary axis (rows if row-flow, cols if col-flow).
    let mut primary_cursors: HashMap<i32, i32> = HashMap::new();
    for (i, (_, _, _, cp, rp)) in entries.iter().enumerate() {
        if placed[i].is_some() {
            continue;
        }
        if !col_flow {
            if let (
                AxisPlacement::Auto { span: c_span },
                AxisPlacement::Definite { start: r0, end: r1 },
            ) = (cp, rp)
            {
                let mut c = if dense {
                    0
                } else {
                    *primary_cursors.get(r0).unwrap_or(&0)
                };
                while !occ.is_free(*r0, *r1, c, c + *c_span) {
                    c += 1;
                }
                occ.occupy(*r0, *r1, c, c + *c_span);
                placed[i] = Some((c, c + *c_span, *r0, *r1));
                if !dense {
                    primary_cursors.insert(*r0, c + *c_span);
                }
            }
        } else if let (
            AxisPlacement::Definite { start: c0, end: c1 },
            AxisPlacement::Auto { span: r_span },
        ) = (cp, rp)
        {
            let mut r = if dense {
                0
            } else {
                *primary_cursors.get(c0).unwrap_or(&0)
            };
            while !occ.is_free(r, r + *r_span, *c0, *c1) {
                r += 1;
            }
            occ.occupy(r, r + *r_span, *c0, *c1);
            placed[i] = Some((*c0, *c1, r, r + *r_span));
            if !dense {
                primary_cursors.insert(*c0, r + *r_span);
            }
        }
    }

    // Step 3: Determine implicit grid breadth along the secondary axis (cols for row-flow, rows for col-flow).
    let mut sec_min = 0i32;
    let mut sec_max = if !col_flow {
        explicit_cols.track_count() as i32
    } else {
        explicit_rows.track_count() as i32
    };
    let mut max_auto_span = 1i32;
    for &(c0, c1, r0, r1) in placed.iter().flatten() {
        if !col_flow {
            sec_min = sec_min.min(c0);
            sec_max = sec_max.max(c1);
        } else {
            sec_min = sec_min.min(r0);
            sec_max = sec_max.max(r1);
        }
    }
    for (_, _, _, cp, rp) in &entries {
        if !col_flow {
            match cp {
                AxisPlacement::Definite { start, end } => {
                    sec_min = sec_min.min(*start);
                    sec_max = sec_max.max(*end);
                }
                AxisPlacement::Auto { span } => max_auto_span = max_auto_span.max(*span),
            }
        } else {
            match rp {
                AxisPlacement::Definite { start, end } => {
                    sec_min = sec_min.min(*start);
                    sec_max = sec_max.max(*end);
                }
                AxisPlacement::Auto { span } => max_auto_span = max_auto_span.max(*span),
            }
        }
    }
    let sec_breadth = (sec_max - sec_min).max(max_auto_span).max(1);
    let sec_limit = sec_min + sec_breadth;

    // Step 4: Place remaining auto-positioned items (both auto/auto and secondary-locked).
    let mut cur_primary = 0i32;
    let mut cur_secondary = sec_min;
    for (i, (_, _, _, cp, rp)) in entries.iter().enumerate() {
        if placed[i].is_some() {
            continue;
        }
        if !col_flow {
            match (cp, rp) {
                (
                    AxisPlacement::Definite { start: c0, end: c1 },
                    AxisPlacement::Auto { span: r_span },
                ) => {
                    let mut r = if dense {
                        0
                    } else if *c0 < cur_secondary {
                        cur_primary + 1
                    } else {
                        cur_primary
                    };
                    while !occ.is_free(r, r + *r_span, *c0, *c1) {
                        r += 1;
                    }
                    occ.occupy(r, r + *r_span, *c0, *c1);
                    placed[i] = Some((*c0, *c1, r, r + *r_span));
                    if !dense {
                        cur_primary = r;
                        cur_secondary = *c0;
                    }
                }
                (AxisPlacement::Auto { span: c_span }, AxisPlacement::Auto { span: r_span }) => {
                    let mut r = if dense { 0 } else { cur_primary };
                    let mut c = if dense { sec_min } else { cur_secondary };
                    loop {
                        if c + *c_span > sec_limit {
                            r += 1;
                            c = sec_min;
                        }
                        if occ.is_free(r, r + *r_span, c, c + *c_span) {
                            break;
                        }
                        c += 1;
                    }
                    occ.occupy(r, r + *r_span, c, c + *c_span);
                    placed[i] = Some((c, c + *c_span, r, r + *r_span));
                    if !dense {
                        cur_primary = r;
                        cur_secondary = c;
                    }
                }
                _ => {}
            }
        } else {
            match (cp, rp) {
                (
                    AxisPlacement::Auto { span: c_span },
                    AxisPlacement::Definite { start: r0, end: r1 },
                ) => {
                    let mut c = if dense {
                        0
                    } else if *r0 < cur_secondary {
                        cur_primary + 1
                    } else {
                        cur_primary
                    };
                    while !occ.is_free(*r0, *r1, c, c + *c_span) {
                        c += 1;
                    }
                    occ.occupy(*r0, *r1, c, c + *c_span);
                    placed[i] = Some((c, c + *c_span, *r0, *r1));
                    if !dense {
                        cur_primary = c;
                        cur_secondary = *r0;
                    }
                }
                (AxisPlacement::Auto { span: c_span }, AxisPlacement::Auto { span: r_span }) => {
                    let mut c = if dense { 0 } else { cur_primary };
                    let mut r = if dense { sec_min } else { cur_secondary };
                    loop {
                        if r + *r_span > sec_limit {
                            c += 1;
                            r = sec_min;
                        }
                        if occ.is_free(r, r + *r_span, c, c + *c_span) {
                            break;
                        }
                        r += 1;
                    }
                    occ.occupy(r, r + *r_span, c, c + *c_span);
                    placed[i] = Some((c, c + *c_span, r, r + *r_span));
                    if !dense {
                        cur_primary = c;
                        cur_secondary = r;
                    }
                }
                _ => {}
            }
        }
    }

    // Compute final grid bounds covering both explicit tracks [0..explicit_count) and all items.
    let mut col_min = 0i32;
    let mut col_max = explicit_cols.track_count() as i32;
    let mut row_min = 0i32;
    let mut row_max = explicit_rows.track_count() as i32;
    for &(c0, c1, r0, r1) in placed.iter().flatten() {
        col_min = col_min.min(c0);
        col_max = col_max.max(c1);
        row_min = row_min.min(r0);
        row_max = row_max.max(r1);
    }
    let col_total = (col_max - col_min).max(0) as usize;
    let row_total = (row_max - row_min).max(0) as usize;

    let mut items = Vec::with_capacity(entries.len());
    for (i, (bid, dom_index, _order, _, _)) in entries.into_iter().enumerate() {
        let (c0, c1, r0, r1) = placed[i].unwrap_or((0, 1, 0, 1));
        items.push(GridItem {
            box_id: bid,
            dom_index,
            col_start: (c0 - col_min) as usize,
            col_end: (c1 - col_min) as usize,
            row_start: (r0 - row_min) as usize,
            row_end: (r1 - row_min) as usize,
            min_contrib: 0.0,
            min_content_contrib: 0.0,
            max_content_contrib: 0.0,
            fragment: None,
            area_col_start: 0.0,
            area_col_size: 0.0,
            area_row_start: 0.0,
            area_row_size: 0.0,
        });
    }

    PlacementResult {
        items,
        col_min_idx: col_min,
        col_total,
        row_min_idx: row_min,
        row_total,
    }
}

/// Builds the full track vector for one axis combining leading implicit tracks, explicit tracks,
/// and trailing implicit tracks, cycling `auto_tracks` per CSS Grid L1 §7.6.
pub(crate) fn build_axis_tracks(
    explicit: &ExplicitAxis,
    auto_tracks: &[GridTrackSize],
    min_idx: i32,
    total: usize,
    avail: Option<f32>,
    eval: &EvalContext,
) -> Vec<Track> {
    let explicit_count = explicit.track_count() as i32;
    let mut tracks = Vec::with_capacity(total);
    for pos in 0..total {
        let signed = min_idx + pos as i32;
        if signed >= 0 && signed < explicit_count {
            let ei = signed as usize;
            tracks.push(Track::from_size(
                explicit.tracks[ei].clone(),
                avail,
                eval,
                explicit.from_auto_fit[ei],
            ));
        } else {
            let raw = if auto_tracks.is_empty() {
                GridTrackSize::Auto
            } else {
                let cycle_idx = if signed < 0 {
                    signed.rem_euclid(auto_tracks.len() as i32) as usize
                } else {
                    ((signed - explicit_count) as usize) % auto_tracks.len()
                };
                auto_tracks[cycle_idx].clone()
            };
            tracks.push(Track::from_size(raw, avail, eval, false));
        }
    }
    tracks
}

// ---------------------------------------------------------------------------
// W3C CSS Grid Level 1 §11 Track Sizing Algorithm
// ---------------------------------------------------------------------------

pub(crate) fn active_gaps_in_range(tracks: &[Track], start: usize, end: usize, gap: f32) -> f32 {
    let s = start.min(tracks.len());
    let e = end.min(tracks.len());
    if e <= s + 1 || gap <= 0.0 {
        return 0.0;
    }
    let active = tracks[s..e].iter().filter(|t| !t.is_collapsed).count();
    if active > 1 {
        gap * (active - 1) as f32
    } else {
        0.0
    }
}

pub(crate) fn span_track_size(tracks: &[Track], start: usize, end: usize, gap: f32) -> f32 {
    let s = start.min(tracks.len());
    let e = end.min(tracks.len());
    if e <= s {
        return 0.0;
    }
    let sum: f32 = tracks[s..e].iter().map(|t| t.base_size).sum();
    sum + active_gaps_in_range(tracks, s, e, gap)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DistTarget {
    BaseSize,
    GrowthLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackFilter {
    IntrinsicMin,
    ContentBasedMin,
    MaxContentMin,
    IntrinsicMax,
    MaxContentMax,
}

impl TrackFilter {
    fn matches(self, t: &Track) -> bool {
        match self {
            Self::IntrinsicMin => t.min_fn.is_intrinsic(),
            Self::ContentBasedMin => t.min_fn.is_content_based(),
            Self::MaxContentMin => matches!(t.min_fn, MinSizing::MaxContent),
            Self::IntrinsicMax => t.max_fn.is_intrinsic(),
            Self::MaxContentMax => matches!(
                t.max_fn,
                MaxSizing::MaxContent | MaxSizing::Auto | MaxSizing::FitContent(_)
            ),
        }
    }

    fn matches_beyond_limit(self, t: &Track) -> bool {
        match self {
            Self::IntrinsicMin | Self::ContentBasedMin => {
                matches!(
                    t.max_fn,
                    MaxSizing::Auto
                        | MaxSizing::MinContent
                        | MaxSizing::MaxContent
                        | MaxSizing::FitContent(_)
                )
            }
            Self::MaxContentMin => matches!(t.max_fn, MaxSizing::MaxContent | MaxSizing::Auto),
            Self::IntrinsicMax | Self::MaxContentMax => self.matches(t),
        }
    }
}

/// CSS Grid L1 §11.5.1 `Distribute Extra Space` water-filling across spanned tracks.
fn distribute_extra_space(
    tracks: &mut [Track],
    indices: &[usize],
    extra_space: f32,
    target: DistTarget,
    filter: TrackFilter,
) {
    if extra_space <= 0.0 || indices.is_empty() {
        return;
    }
    let affected: Vec<usize> = indices
        .iter()
        .copied()
        .filter(|&i| !tracks[i].is_collapsed && filter.matches(&tracks[i]))
        .collect();
    if affected.is_empty() {
        return;
    }

    for &i in &affected {
        tracks[i].item_incurred_increase = 0.0;
    }

    let mut remaining = extra_space;

    // Phase 1: Distribute up to each track's finite limit (`growth_limit` for BaseSize,
    // or `fit-content(limit)` cap for GrowthLimit).
    let mut unfrozen: Vec<usize> = affected.clone();
    while remaining > 1e-4 && !unfrozen.is_empty() {
        let share = remaining / unfrozen.len() as f32;
        let mut next_unfrozen = Vec::with_capacity(unfrozen.len());
        let mut consumed = 0.0f32;
        let mut froze_any = false;
        for &i in &unfrozen {
            let cap = match target {
                DistTarget::BaseSize => {
                    if tracks[i].growth_limit.is_finite() {
                        (tracks[i].growth_limit
                            - tracks[i].base_size
                            - tracks[i].item_incurred_increase)
                            .max(0.0)
                    } else {
                        f32::INFINITY
                    }
                }
                DistTarget::GrowthLimit => {
                    if let MaxSizing::FitContent(lim) = tracks[i].max_fn {
                        let cur = if tracks[i].growth_limit.is_finite() {
                            tracks[i].growth_limit
                        } else {
                            tracks[i].base_size
                        };
                        (lim - cur - tracks[i].item_incurred_increase).max(0.0)
                    } else {
                        f32::INFINITY
                    }
                }
            };
            if share >= cap {
                tracks[i].item_incurred_increase += cap;
                consumed += cap;
                froze_any = true;
            } else {
                next_unfrozen.push(i);
            }
        }
        if !froze_any {
            for &i in &next_unfrozen {
                tracks[i].item_incurred_increase += share;
            }
            remaining = 0.0;
            break;
        }
        remaining = (remaining - consumed).max(0.0);
        unfrozen = next_unfrozen;
    }

    // Phase 2: If space remains after all affected tracks hit their caps, distribute equally
    // among tracks growable beyond their limits (for BaseSize, or non-fit-content for GrowthLimit).
    if remaining > 1e-4 {
        let growable: Vec<usize> = affected
            .iter()
            .copied()
            .filter(|&i| match target {
                DistTarget::BaseSize => filter.matches_beyond_limit(&tracks[i]),
                DistTarget::GrowthLimit => {
                    !matches!(tracks[i].max_fn, MaxSizing::FitContent(_))
                        || tracks[i].infinitely_growable
                }
            })
            .collect();
        let pool = if growable.is_empty() {
            if target == DistTarget::GrowthLimit {
                &[][..]
            } else {
                affected.as_slice()
            }
        } else {
            growable.as_slice()
        };
        if !pool.is_empty() {
            let share = remaining / pool.len() as f32;
            for &i in pool {
                tracks[i].item_incurred_increase += share;
            }
        }
    }

    for &i in &affected {
        tracks[i].planned_increase = tracks[i]
            .planned_increase
            .max(tracks[i].item_incurred_increase);
    }
}

fn commit_planned_increases(tracks: &mut [Track], target: DistTarget) {
    for t in tracks.iter_mut() {
        if t.planned_increase > 0.0 {
            match target {
                DistTarget::BaseSize => {
                    t.base_size += t.planned_increase;
                }
                DistTarget::GrowthLimit => {
                    if !t.growth_limit.is_finite() {
                        t.growth_limit = t.base_size + t.planned_increase;
                        t.infinitely_growable = true;
                    } else {
                        t.growth_limit += t.planned_increase;
                    }
                }
            }
            t.planned_increase = 0.0;
        }
    }
}

/// CSS Grid L1 §11.7.1 `Find the Size of an fr` iterative freeze loop.
fn find_size_of_fr(tracks: &[Track], indices: &[usize], space_to_fill: f32) -> f32 {
    let mut leftover = space_to_fill;
    let mut flex_factor_sum = 0.0f32;
    let mut frozen = vec![false; indices.len()];

    for (pos, &ti) in indices.iter().enumerate() {
        if tracks[ti].max_fn.is_flexible() && !tracks[ti].is_collapsed {
            flex_factor_sum += tracks[ti].max_fn.flex_factor();
        } else {
            leftover -= tracks[ti].base_size;
            frozen[pos] = true;
        }
    }

    loop {
        if flex_factor_sum <= 0.0 || leftover <= 0.0 {
            return 0.0;
        }
        let eff_sum = flex_factor_sum.max(1.0);
        let hyp_fr = leftover / eff_sum;
        let mut froze_any = false;
        for (pos, &ti) in indices.iter().enumerate() {
            if frozen[pos] {
                continue;
            }
            let f = tracks[ti].max_fn.flex_factor();
            if f * hyp_fr < tracks[ti].base_size {
                leftover -= tracks[ti].base_size;
                flex_factor_sum -= f;
                frozen[pos] = true;
                froze_any = true;
            }
        }
        if !froze_any {
            return hyp_fr.max(0.0);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SizingConstraint {
    Definite(u32), // quantized not needed; use Option<f32> alongside
    MinContent,
    MaxContent,
    Indefinite,
}

/// Runs the complete W3C CSS Grid Level 1 §11 Track Sizing Algorithm for one axis.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_tracks(
    tracks: &mut [Track],
    items: &[GridItem],
    is_col: bool,
    avail: Option<f32>,
    min_avail: Option<f32>,
    max_avail: Option<f32>,
    gap: f32,
    content_align: AlignValue,
    constraint: SizingConstraint,
) {
    if tracks.is_empty() {
        return;
    }

    // §11.4 Initialize Track Sizes
    for t in tracks.iter_mut() {
        if t.is_collapsed {
            t.base_size = 0.0;
            t.growth_limit = 0.0;
            t.planned_increase = 0.0;
            t.item_incurred_increase = 0.0;
            t.infinitely_growable = false;
            continue;
        }
        t.base_size = match t.min_fn {
            MinSizing::Fixed(v) => v,
            _ => 0.0,
        };
        t.growth_limit = match t.max_fn {
            MaxSizing::Fixed(v) => v,
            _ => f32::INFINITY,
        };
        if t.growth_limit < t.base_size {
            t.growth_limit = t.base_size;
        }
        t.planned_increase = 0.0;
        t.item_incurred_increase = 0.0;
        t.infinitely_growable = false;
    }

    // §11.5 Resolve Intrinsic Track Sizes
    let item_span = |it: &GridItem| -> (usize, usize) {
        if is_col {
            (it.col_start, it.col_end)
        } else {
            (it.row_start, it.row_end)
        }
    };

    // 1. Single-span items not crossing flexible tracks
    for it in items {
        let (s, e) = item_span(it);
        if e != s + 1 || tracks[s].is_collapsed || tracks[s].max_fn.is_flexible() {
            continue;
        }
        let t = &mut tracks[s];
        match t.min_fn {
            MinSizing::Auto => {
                let c = match constraint {
                    SizingConstraint::MinContent => it.min_content_contrib,
                    SizingConstraint::MaxContent => it.max_content_contrib,
                    _ => it.min_contrib,
                };
                t.base_size = t.base_size.max(c);
            }
            MinSizing::MinContent => {
                t.base_size = t.base_size.max(it.min_content_contrib);
            }
            MinSizing::MaxContent => {
                t.base_size = t.base_size.max(it.max_content_contrib);
            }
            MinSizing::Fixed(_) => {}
        }
        match t.max_fn {
            MaxSizing::MinContent => {
                if !t.growth_limit.is_finite() {
                    t.growth_limit = it.min_content_contrib;
                } else {
                    t.growth_limit = t.growth_limit.max(it.min_content_contrib);
                }
            }
            MaxSizing::MaxContent | MaxSizing::Auto => {
                if !t.growth_limit.is_finite() {
                    t.growth_limit = it.max_content_contrib;
                } else {
                    t.growth_limit = t.growth_limit.max(it.max_content_contrib);
                }
            }
            MaxSizing::FitContent(lim) => {
                let capped = it.max_content_contrib.min(lim);
                if !t.growth_limit.is_finite() {
                    t.growth_limit = capped;
                } else {
                    t.growth_limit = t.growth_limit.max(capped);
                }
            }
            MaxSizing::Fixed(_) | MaxSizing::Fr(_) => {}
        }
        if t.growth_limit < t.base_size {
            t.growth_limit = t.base_size;
        }
    }

    // 2. Multi-span items (span >= 2) not crossing flexible tracks, in increasing span order
    let mut max_span = 1usize;
    for it in items {
        let (s, e) = item_span(it);
        max_span = max_span.max(e.saturating_sub(s));
    }

    for span_len in 2..=max_span {
        let span_items: Vec<&GridItem> = items
            .iter()
            .filter(|it| {
                let (s, e) = item_span(it);
                e.saturating_sub(s) == span_len
                    && !tracks[s..e].iter().any(|t| t.max_fn.is_flexible())
            })
            .collect();
        if span_items.is_empty() {
            continue;
        }

        // (a) Intrinsic minimums -> base_size (minimum contribution)
        for it in &span_items {
            let (s, e) = item_span(it);
            let cur_sum: f32 = tracks[s..e].iter().map(|t| t.base_size).sum();
            let gaps = active_gaps_in_range(tracks, s, e, gap);
            let contrib = match constraint {
                SizingConstraint::MinContent => it.min_content_contrib,
                SizingConstraint::MaxContent => it.max_content_contrib,
                _ => it.min_contrib,
            };
            let idx: Vec<usize> = (s..e).collect();
            distribute_extra_space(
                tracks,
                &idx,
                contrib - cur_sum - gaps,
                DistTarget::BaseSize,
                TrackFilter::IntrinsicMin,
            );
        }
        commit_planned_increases(tracks, DistTarget::BaseSize);

        // (b) Content-based minimums -> base_size (min-content contribution)
        for it in &span_items {
            let (s, e) = item_span(it);
            let cur_sum: f32 = tracks[s..e].iter().map(|t| t.base_size).sum();
            let gaps = active_gaps_in_range(tracks, s, e, gap);
            let idx: Vec<usize> = (s..e).collect();
            distribute_extra_space(
                tracks,
                &idx,
                it.min_content_contrib - cur_sum - gaps,
                DistTarget::BaseSize,
                TrackFilter::ContentBasedMin,
            );
        }
        commit_planned_increases(tracks, DistTarget::BaseSize);

        // (c) Max-content minimums -> base_size (max-content contribution)
        for it in &span_items {
            let (s, e) = item_span(it);
            let cur_sum: f32 = tracks[s..e].iter().map(|t| t.base_size).sum();
            let gaps = active_gaps_in_range(tracks, s, e, gap);
            let idx: Vec<usize> = (s..e).collect();
            distribute_extra_space(
                tracks,
                &idx,
                it.max_content_contrib - cur_sum - gaps,
                DistTarget::BaseSize,
                TrackFilter::MaxContentMin,
            );
        }
        commit_planned_increases(tracks, DistTarget::BaseSize);

        // (d) Ensure growth_limit >= base_size
        for t in tracks.iter_mut() {
            if t.growth_limit < t.base_size {
                t.growth_limit = t.base_size;
            }
        }

        // (e) Intrinsic maximums -> growth_limit (min-content contribution)
        for it in &span_items {
            let (s, e) = item_span(it);
            let cur_sum: f32 = tracks[s..e]
                .iter()
                .map(|t| {
                    if t.growth_limit.is_finite() {
                        t.growth_limit
                    } else {
                        t.base_size
                    }
                })
                .sum();
            let gaps = active_gaps_in_range(tracks, s, e, gap);
            let idx: Vec<usize> = (s..e).collect();
            distribute_extra_space(
                tracks,
                &idx,
                it.min_content_contrib - cur_sum - gaps,
                DistTarget::GrowthLimit,
                TrackFilter::IntrinsicMax,
            );
        }
        commit_planned_increases(tracks, DistTarget::GrowthLimit);

        // (f) Max-content maximums -> growth_limit (max-content contribution)
        for it in &span_items {
            let (s, e) = item_span(it);
            let cur_sum: f32 = tracks[s..e]
                .iter()
                .map(|t| {
                    if t.growth_limit.is_finite() {
                        t.growth_limit
                    } else {
                        t.base_size
                    }
                })
                .sum();
            let gaps = active_gaps_in_range(tracks, s, e, gap);
            let idx: Vec<usize> = (s..e).collect();
            distribute_extra_space(
                tracks,
                &idx,
                it.max_content_contrib - cur_sum - gaps,
                DistTarget::GrowthLimit,
                TrackFilter::MaxContentMax,
            );
        }
        commit_planned_increases(tracks, DistTarget::GrowthLimit);
    }

    // 3. Items crossing flexible tracks (intrinsic minimums only, §11.5 step 3)
    for it in items {
        let (s, e) = item_span(it);
        if !tracks[s..e].iter().any(|t| t.max_fn.is_flexible()) {
            continue;
        }
        let flex_indices: Vec<usize> = (s..e)
            .filter(|&i| {
                tracks[i].max_fn.is_flexible()
                    && !tracks[i].is_collapsed
                    && tracks[i].min_fn.is_intrinsic()
            })
            .collect();
        if flex_indices.is_empty() {
            continue;
        }
        let cur_sum: f32 = tracks[s..e].iter().map(|t| t.base_size).sum();
        let gaps = active_gaps_in_range(tracks, s, e, gap);
        let contrib = match constraint {
            SizingConstraint::MinContent => it.min_content_contrib,
            SizingConstraint::MaxContent => it.max_content_contrib,
            _ => it.min_contrib,
        };
        let extra = contrib - cur_sum - gaps;
        if extra <= 0.0 {
            continue;
        }
        let fr_sum: f32 = flex_indices
            .iter()
            .map(|&i| tracks[i].max_fn.flex_factor())
            .sum();
        if fr_sum > 0.0 {
            for &i in &flex_indices {
                let share = extra * (tracks[i].max_fn.flex_factor() / fr_sum);
                tracks[i].planned_increase = tracks[i].planned_increase.max(share);
            }
        } else {
            let share = extra / flex_indices.len() as f32;
            for &i in &flex_indices {
                tracks[i].planned_increase = tracks[i].planned_increase.max(share);
            }
        }
    }
    commit_planned_increases(tracks, DistTarget::BaseSize);

    // 4. Replace remaining infinite growth limits with base_size
    for t in tracks.iter_mut() {
        if !t.growth_limit.is_finite() {
            t.growth_limit = t.base_size;
        }
        if t.growth_limit < t.base_size {
            t.growth_limit = t.base_size;
        }
    }

    // §11.6 Maximize Tracks
    let total_gaps = active_gaps_in_range(tracks, 0, tracks.len(), gap);
    let base_sum: f32 = tracks.iter().map(|t| t.base_size).sum();
    let free_space_for_max = match constraint {
        SizingConstraint::MinContent => Some(0.0),
        SizingConstraint::MaxContent => Some(f32::INFINITY),
        _ => avail
            .or(max_avail)
            .map(|a| (a - base_sum - total_gaps).max(0.0)),
    };

    match free_space_for_max {
        Some(fs) if fs.is_infinite() => {
            for t in tracks.iter_mut() {
                if !t.is_collapsed {
                    t.base_size = t.growth_limit;
                }
            }
        }
        Some(mut fs) if fs > 0.0 => {
            let mut growable: Vec<usize> = (0..tracks.len())
                .filter(|&i| {
                    !tracks[i].is_collapsed && tracks[i].base_size < tracks[i].growth_limit
                })
                .collect();
            while fs > 1e-4 && !growable.is_empty() {
                let share = fs / growable.len() as f32;
                let mut next = Vec::with_capacity(growable.len());
                let mut consumed = 0.0f32;
                let mut froze = false;
                for &i in &growable {
                    let cap = (tracks[i].growth_limit - tracks[i].base_size).max(0.0);
                    if share >= cap {
                        tracks[i].base_size = tracks[i].growth_limit;
                        consumed += cap;
                        froze = true;
                    } else {
                        next.push(i);
                    }
                }
                if !froze {
                    for &i in &next {
                        tracks[i].base_size += share;
                    }
                    break;
                }
                fs = (fs - consumed).max(0.0);
                growable = next;
            }
        }
        None => {
            // Indefinite container without max-size: grow every track to its growth limit.
            for t in tracks.iter_mut() {
                if !t.is_collapsed {
                    t.base_size = t.growth_limit;
                }
            }
        }
        _ => {}
    }

    // §11.7 Expand Flexible Tracks (`fr`)
    let has_flex = tracks
        .iter()
        .any(|t| !t.is_collapsed && t.max_fn.is_flexible());
    if has_flex {
        let all_idx: Vec<usize> = (0..tracks.len()).collect();
        let flex_fraction = if let Some(a) = avail.filter(|_| {
            !matches!(
                constraint,
                SizingConstraint::MinContent | SizingConstraint::MaxContent
            )
        }) {
            find_size_of_fr(tracks, &all_idx, (a - total_gaps).max(0.0))
        } else if matches!(constraint, SizingConstraint::MinContent) {
            0.0
        } else {
            // Indefinite / max-content flexible track sizing (§11.7):
            let mut fr_size = 0.0f32;
            for t in tracks.iter() {
                if !t.is_collapsed && t.max_fn.is_flexible() {
                    let f = t.max_fn.flex_factor();
                    if f > 0.0 {
                        // §11.7: If flex factor <= 1, use track's base size directly.
                        fr_size = fr_size.max(t.base_size / f.max(1.0));
                    }
                }
            }
            for it in items {
                let (s, e) = item_span(it);
                if tracks[s..e].iter().any(|t| t.max_fn.is_flexible()) {
                    let span_idx: Vec<usize> = (s..e).collect();
                    let span_gaps = active_gaps_in_range(tracks, s, e, gap);
                    let f_it = find_size_of_fr(
                        tracks,
                        &span_idx,
                        (it.max_content_contrib - span_gaps).max(0.0),
                    );
                    fr_size = fr_size.max(f_it);
                }
            }
            if let Some(min_a) = min_avail {
                let min_fr = find_size_of_fr(tracks, &all_idx, (min_a - total_gaps).max(0.0));
                fr_size = fr_size.max(min_fr);
            }
            if let Some(max_a) = max_avail {
                let used_if_fr: f32 = tracks
                    .iter()
                    .map(|t| {
                        if t.max_fn.is_flexible() {
                            t.base_size.max(t.max_fn.flex_factor() * fr_size)
                        } else {
                            t.base_size
                        }
                    })
                    .sum::<f32>()
                    + total_gaps;
                if used_if_fr > max_a {
                    fr_size = find_size_of_fr(tracks, &all_idx, (max_a - total_gaps).max(0.0));
                }
            }
            fr_size
        };

        for t in tracks.iter_mut() {
            if !t.is_collapsed && t.max_fn.is_flexible() {
                let target = t.max_fn.flex_factor() * flex_fraction;
                if target > t.base_size {
                    t.base_size = target;
                }
            }
        }
    }

    // §11.8 Stretch `auto` Tracks
    let stretch_auto = matches!(
        content_align.base(),
        AlignValue::Normal | AlignValue::Stretch
    );
    if stretch_auto
        && !matches!(
            constraint,
            SizingConstraint::MinContent | SizingConstraint::MaxContent
        )
    {
        if let Some(a) = avail {
            let cur_sum: f32 = tracks.iter().map(|t| t.base_size).sum();
            let free = a - cur_sum - total_gaps;
            if free > 1e-4 {
                let auto_tracks: Vec<usize> = (0..tracks.len())
                    .filter(|&i| {
                        !tracks[i].is_collapsed && matches!(tracks[i].max_fn, MaxSizing::Auto)
                    })
                    .collect();
                if !auto_tracks.is_empty() {
                    let add = free / auto_tracks.len() as f32;
                    for i in auto_tracks {
                        tracks[i].base_size += add;
                    }
                }
            }
        }
    }
}

/// Computes exact track start positions `[0..tracks.len()]` and line positions `[0..=tracks.len()]`
/// in logical coordinates relative to the container's content-box start edge.
pub(crate) fn compute_track_positions(
    tracks: &[Track],
    gap: f32,
    avail: f32,
    content_align: AlignValue,
    axis_is_x: bool,
    container_rev: bool,
) -> (Vec<f32>, Vec<f32>, f32) {
    let n = tracks.len();
    if n == 0 {
        return (Vec::new(), vec![0.0], 0.0);
    }
    let active_count = tracks.iter().filter(|t| !t.is_collapsed).count();
    let sum_tracks: f32 = tracks.iter().map(|t| t.base_size).sum();
    let total_gaps = if active_count > 1 {
        gap * (active_count - 1) as f32
    } else {
        0.0
    };
    let used = sum_tracks + total_gaps;
    let (init_off, extra_gap) = compute_content_alignment(
        avail,
        used,
        active_count,
        content_align,
        axis_is_x,
        container_rev,
    );
    let eff_gap = gap + extra_gap;

    let mut track_starts = Vec::with_capacity(n);
    let mut line_starts = Vec::with_capacity(n + 1);
    let mut cursor = init_off;
    let mut seen_active = false;

    for t in tracks {
        if !t.is_collapsed && seen_active {
            cursor += eff_gap;
        }
        line_starts.push(cursor);
        track_starts.push(cursor);
        cursor += t.base_size;
        if !t.is_collapsed {
            seen_active = true;
        }
    }
    line_starts.push(cursor);
    (track_starts, line_starts, used)
}

/// Formats CSS Grid L1 §7.2.6 resolved track listing (`getComputedStyle().gridTemplateColumns/Rows`).
pub(crate) fn format_resolved_tracks(
    tracks: &[Track],
    explicit: &ExplicitAxis,
    min_idx: i32,
) -> String {
    if tracks.is_empty() {
        return "none".to_string();
    }
    let explicit_origin = (-min_idx).max(0) as usize;
    let explicit_lines = explicit.explicit_line_names.len();
    let mut tokens: Vec<String> = Vec::with_capacity(tracks.len() * 2 + 1);

    let push_names = |line_pos: usize, out: &mut Vec<String>| {
        if line_pos >= explicit_origin {
            let ei = line_pos - explicit_origin;
            if ei < explicit_lines {
                let ns = &explicit.explicit_line_names[ei];
                if !ns.is_empty() {
                    let joined: Vec<String> =
                        ns.iter().map(|s| escape_css_ident(s.as_str())).collect();
                    out.push(format!("[{}]", joined.join(" ")));
                }
            }
        }
    };

    for (i, t) in tracks.iter().enumerate() {
        push_names(i, &mut tokens);
        let px = if t.is_collapsed {
            0.0
        } else {
            // Match browser subpixel rounding tolerance cleanly.
            let v = f64::from(t.base_size.max(0.0));
            if (v - v.round()).abs() < 1e-4 {
                v.round()
            } else {
                v
            }
        };
        tokens.push(format!("{}px", format_math_f64(px)));
    }
    push_names(tracks.len(), &mut tokens);
    tokens.join(" ")
}

/// Resolves a CSS Grid L1 §9.1 abspos item edge coordinate along one logical axis (relative to
/// container's border-box start edge in logical coordinates).
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_abspos_axis_edges(
    start_val: &GridPlacementValue,
    end_val: &GridPlacementValue,
    explicit: &ExplicitAxis,
    min_idx: i32,
    tracks: &[Track],
    track_starts: &[f32],
    pad_box_start: f32,
    pad_box_end: f32,
    content_start: f32,
    content_end: f32,
    eval: &EvalContext,
) -> (f32, f32, f32, f32) {
    let explicit_tracks = explicit.track_count() as i32;
    let total = tracks.len() as i32;

    // Resolve single line to an index in [0..=total] if in bounds, else None (snaps to padding edge).
    let resolve_abs_line = |p: &GridPlacementValue, is_start: bool| -> Option<i32> {
        let raw = resolve_definite_line(p, explicit, is_start, eval)?;
        let shifted = raw - min_idx;
        if shifted >= 0 && shifted <= total {
            Some(shifted)
        } else {
            None
        }
    };

    let eff_end = if matches!(start_val, GridPlacementValue::Span { .. })
        && matches!(end_val, GridPlacementValue::Span { .. })
    {
        &GridPlacementValue::Auto
    } else {
        end_val
    };

    let mut s_line = resolve_abs_line(start_val, true);
    let mut e_line = resolve_abs_line(eff_end, false);

    // Handle `span` paired with a definite opposite line for abspos children.
    if let (
        Some(s),
        GridPlacementValue::Span {
            count,
            count_math,
            name,
        },
    ) = (s_line, eff_end)
    {
        let sp = eval_span_int(*count, count_math.as_deref(), eval);
        let raw_s = s + min_idx;
        let raw_e = if let Some(ident) = name {
            span_named_line(explicit, raw_s, ident.as_str(), sp, true)
        } else {
            raw_s + sp
        };
        let shifted = raw_e - min_idx;
        if shifted >= 0 && shifted <= total {
            e_line = Some(shifted);
        }
    } else if let (
        GridPlacementValue::Span {
            count,
            count_math,
            name,
        },
        Some(e),
    ) = (start_val, e_line)
    {
        let sp = eval_span_int(*count, count_math.as_deref(), eval);
        let raw_e = e + min_idx;
        let raw_s = if let Some(ident) = name {
            span_named_line(explicit, raw_e, ident.as_str(), sp, false)
        } else {
            raw_e - sp
        };
        let shifted = raw_s - min_idx;
        if shifted >= 0 && shifted <= total {
            s_line = Some(shifted);
        }
    }
    let _ = explicit_tracks;

    if let (Some(s), Some(e)) = (s_line, e_line) {
        if s > e {
            s_line = Some(e);
            e_line = Some(s);
        }
    }

    let line_coord = |line: i32, is_end: bool| -> f32 {
        let idx = line as usize;
        if tracks.is_empty() {
            return content_start;
        }
        if !is_end {
            if idx < tracks.len() {
                content_start + track_starts[idx]
            } else {
                content_start + track_starts[tracks.len() - 1] + tracks[tracks.len() - 1].base_size
            }
        } else if idx == 0 {
            content_start + track_starts[0]
        } else {
            content_start + track_starts[idx - 1] + tracks[idx - 1].base_size
        }
    };

    let cb_start = s_line.map_or(pad_box_start, |l| line_coord(l, false));
    let cb_end = e_line.map_or(pad_box_end, |l| line_coord(l, true));
    let (cb_s, cb_e) = if cb_end < cb_start {
        (cb_start, cb_start)
    } else {
        (cb_start, cb_end)
    };
    (cb_s, cb_e, content_start, content_end)
}

/// Computes CSS Box Alignment L3 §9.1–§9.3 baseline group side and `(ascent, descent)` in
/// logical axis coordinates (including fixed margins) for a grid item along one grid axis.
///
/// Returns `(group_side, ascent_from_axis_start, descent_to_axis_end)`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_axis_baseline_metrics(
    item_style: &ComputedStyle,
    frag_w: f32,
    frag_h: f32,
    baseline_y: Option<f32>,
    last_baseline_y: Option<f32>,
    im: &BoxMetrics,
    m_start: f32,
    m_end: f32,
    axis_is_x: bool,
    axis_rev: bool,
    is_last: bool,
) -> (bool, f32, f32) {
    let item_wm = item_style.text.writing_mode;
    let item_orient = item_style.text.text_orientation;
    if !axis_is_x {
        // Alignment axis is physical Y.
        let (group_side, b_top) = if !item_wm.is_vertical() {
            let b = if is_last {
                last_baseline_y.or(baseline_y).unwrap_or(frag_h)
            } else {
                baseline_y.unwrap_or(frag_h)
            };
            (true, b.clamp(0.0, frag_h))
        } else if matches!(item_wm, WritingMode::SidewaysLr) {
            (false, 0.0)
        } else {
            // VerticalLr | VerticalRl | SidewaysRl orthogonal to Y: synthesized at under (bottom) border edge.
            (true, frag_h)
        };
        if !axis_rev {
            (
                group_side,
                m_start + b_top,
                (frag_h - b_top).max(0.0) + m_end,
            )
        } else {
            (
                group_side,
                m_start + (frag_h - b_top).max(0.0),
                b_top + m_end,
            )
        }
    } else {
        // Alignment axis is physical X.
        let (group_side, b_left) = if item_wm.is_vertical() {
            let side = matches!(item_wm, WritingMode::VerticalLr | WritingMode::SidewaysLr);
            let pad_l = im.border.left + im.padding.left;
            let pad_r = im.border.right + im.padding.right;
            let cw = (frag_w - pad_l - pad_r).max(0.0);
            let b = if matches!(item_orient, TextOrientationMode::Sideways)
                || matches!(item_wm, WritingMode::SidewaysRl | WritingMode::SidewaysLr)
            {
                let fs = item_style.text.font_size_px.min(cw);
                if matches!(item_wm, WritingMode::SidewaysLr) {
                    pad_l + (0.8 * fs).min(cw)
                } else {
                    pad_l + (0.2 * fs).min(cw)
                }
            } else {
                pad_l + 0.5 * cw
            };
            (side, b.clamp(0.0, frag_w))
        } else {
            // HorizontalTb orthogonal to X: synthesized at start border edge (left in LTR).
            (true, if axis_rev { frag_w } else { 0.0 })
        };
        if !axis_rev {
            (
                group_side,
                m_start + b_left,
                (frag_w - b_left).max(0.0) + m_end,
            )
        } else {
            (
                group_side,
                m_start + (frag_w - b_left).max(0.0),
                b_left + m_end,
            )
        }
    }
}

pub(crate) fn mark_empty_auto_fit_tracks(tracks: &mut [Track], items: &[GridItem], is_col: bool) {
    let mut occupied = vec![false; tracks.len()];
    for it in items {
        let (s, e) = if is_col {
            (it.col_start, it.col_end)
        } else {
            (it.row_start, it.row_end)
        };
        for slot in occupied.iter_mut().take(e.min(tracks.len())).skip(s) {
            *slot = true;
        }
    }
    for (i, t) in tracks.iter_mut().enumerate() {
        if t.from_auto_fit && !occupied[i] {
            t.is_collapsed = true;
            t.base_size = 0.0;
            t.growth_limit = 0.0;
        }
    }
}

pub(crate) fn track_has_percentage(sz: &GridTrackSize) -> bool {
    match sz {
        GridTrackSize::Length(lp) | GridTrackSize::FitContent(lp) => lp.has_percentage(),
        GridTrackSize::MinMax(a, b) => track_has_percentage(a) || track_has_percentage(b),
        _ => false,
    }
}

/// Solves logical start position (border-box start relative to grid-area start) and resolved
/// `(margin_start, margin_end)` for a grid item along one axis.
#[allow(clippy::too_many_arguments)]
pub(crate) fn solve_grid_item_axis(
    area_size: f32,
    border_size: f32,
    m_start_auto: bool,
    m_end_auto: bool,
    m_start_raw: f32,
    m_end_raw: f32,
    align: ResolvedSelfAlign,
    safe: bool,
    baseline_offset: Option<f32>,
) -> (f32, f32, f32) {
    let fixed_s = if m_start_auto { 0.0 } else { m_start_raw };
    let fixed_e = if m_end_auto { 0.0 } else { m_end_raw };
    let free = area_size - border_size - fixed_s - fixed_e;

    if m_start_auto || m_end_auto {
        let (ms, me) = match (m_start_auto, m_end_auto) {
            (true, true) => {
                if free > 0.0 {
                    (free * 0.5, free * 0.5)
                } else {
                    (0.0, 0.0)
                }
            }
            (true, false) => (free.max(0.0), fixed_e),
            (false, true) => (fixed_s, free.max(0.0)),
            (false, false) => (fixed_s, fixed_e),
        };
        return (ms, ms, me);
    }

    if let Some(bo) = baseline_offset {
        return (fixed_s + bo, fixed_s, fixed_e);
    }

    if safe && free < 0.0 {
        return (fixed_s, fixed_s, fixed_e);
    }

    let extra = match align {
        ResolvedSelfAlign::Start | ResolvedSelfAlign::Stretch | ResolvedSelfAlign::Baseline => 0.0,
        ResolvedSelfAlign::End | ResolvedSelfAlign::LastBaseline => free,
        ResolvedSelfAlign::Center => free * 0.5,
    };
    (fixed_s + extra, fixed_s, fixed_e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rix_css::LengthUnit;

    #[test]
    fn grid_axes_writing_modes_and_directions() {
        let h_ltr = GridAxes::compute(WritingMode::HorizontalTb, DirectionMode::Ltr);
        assert!(h_ltr.col_is_x && !h_ltr.col_reversed && !h_ltr.row_reversed);

        let h_rtl = GridAxes::compute(WritingMode::HorizontalTb, DirectionMode::Rtl);
        assert!(h_rtl.col_is_x && h_rtl.col_reversed && !h_rtl.row_reversed);

        let v_rl = GridAxes::compute(WritingMode::VerticalRl, DirectionMode::Ltr);
        assert!(!v_rl.col_is_x && !v_rl.col_reversed && v_rl.row_reversed);

        let v_lr = GridAxes::compute(WritingMode::VerticalLr, DirectionMode::Rtl);
        assert!(!v_lr.col_is_x && v_lr.col_reversed && !v_lr.row_reversed);
    }

    #[test]
    fn auto_repeat_minmax_uses_max_of_fixed_bounds_and_collapses_empty_auto_fit() {
        let eval = EvalContext::default();
        let sz = GridTrackSize::MinMax(
            Box::new(GridTrackSize::Length(LengthPercentage::Length(
                30.0,
                LengthUnit::Px,
            ))),
            Box::new(GridTrackSize::Length(LengthPercentage::Length(
                13.0,
                LengthUnit::Px,
            ))),
        );
        assert_eq!(auto_repeat_track_size(&sz, 200.0, &eval), Some(30.0));

        let list = GridTemplateTrackList::Tracks {
            items: vec![GridTrackListEntry::Repeat {
                count: GridRepeatCount::AutoFit,
                tracks: vec![GridTrackSize::Length(LengthPercentage::Length(
                    30.0,
                    LengthUnit::Px,
                ))],
                line_names: vec![Vec::new(), Vec::new()],
            }],
            line_names: vec![Vec::new(), Vec::new()],
        };
        let exp = expand_explicit_axis(
            &list,
            &GridTemplateAreasValue::default(),
            &[],
            false,
            Some(120.0),
            None,
            None,
            0.0,
            &eval,
            None,
        );
        assert_eq!(exp.track_count(), 4);
        let mut tracks = build_axis_tracks(&exp, &[], 0, 4, Some(120.0), &eval);
        mark_empty_auto_fit_tracks(&mut tracks, &[], true);
        resolve_tracks(
            &mut tracks,
            &[],
            true,
            Some(120.0),
            None,
            None,
            0.0,
            AlignValue::Normal,
            SizingConstraint::Definite(0),
        );
        assert_eq!(format_resolved_tracks(&tracks, &exp, 0), "0px 0px 0px 0px");
    }
}
