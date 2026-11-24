//! W3C CSS Values and Units Level 4 & Level 5 (`sibling-index`/`sibling-count`/`progress`)
//! typed math expression parser, §10.7 type checker, §10.8 simplifier, §10.9 canonical
//! serializer, and compound transform/position/scale/rotate/translate/resolution helpers.

use cssparser::{Parser, ParserInput, Token};
use serde::{Deserialize, Serialize};

use crate::values::{EvalContext, LengthPercentage, LengthUnit};

/// Maximum finite clamped pixel value at computed/used-value time (matches Blink ~3.35e7px).
pub const MAX_CSS_PX: f64 = 3.355_443_2e7;
/// Maximum finite clamped numeric/time value at computed/used-value time.
pub const MAX_CSS_NUM: f64 = 3.402_823_4e38;

/// Base CSS dimension categories per CSS Values L4 §10.7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum BaseDim {
    Length = 0,
    Angle = 1,
    Time = 2,
    Frequency = 3,
    Resolution = 4,
    Flex = 5,
}

/// Target grammar for validating a CSS math/dimension value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Number,
    Integer,
    Length,
    LengthPercentage,
    Angle,
    Time,
    Frequency,
    Resolution,
    Flex,
    Percentage,
    NumberOrPercentage,
}

/// CSS Values L4 §10.7 algebraic dimension type vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CalcType {
    pub dims: [i8; 6],
    pub percent: i8,
    pub percent_hint: Option<BaseDim>,
    pub has_percent: bool,
}

#[rustfmt::skip]
impl CalcType {
    pub const NUMBER: Self = Self { dims: [0; 6], percent: 0, percent_hint: None, has_percent: false };
    pub const PERCENT: Self = Self { dims: [0; 6], percent: 1, percent_hint: None, has_percent: true };

    #[must_use]
    pub const fn base(dim: BaseDim) -> Self {
        let mut dims = [0i8; 6];
        dims[dim as usize] = 1;
        Self { dims, percent: 0, percent_hint: None, has_percent: false }
    }

    pub(crate) fn apply_hint(mut self, hint: BaseDim) -> Option<Self> {
        if self.percent_hint.is_some_and(|e| e != hint) { return None; }
        if self.percent != 0 {
            self.dims[hint as usize] = self.dims[hint as usize].checked_add(self.percent)?;
            self.percent = 0;
        }
        self.percent_hint = Some(hint);
        Some(self)
    }

    fn single_base_dim(self) -> Option<BaseDim> {
        let mut found = None;
        for (i, &exp) in self.dims.iter().enumerate() {
            if exp != 0 {
                if found.is_some() { return None; }
                found = Some(match i { 0 => BaseDim::Length, 1 => BaseDim::Angle, 2 => BaseDim::Time, 3 => BaseDim::Frequency, 4 => BaseDim::Resolution, _ => BaseDim::Flex });
            }
        }
        found
    }

    /// CSS Values L4 §10.7: consistent type for `+`, `-`, `min`, `max`, `clamp`, `round`, `mod`, `rem`, `hypot`.
    #[must_use]
    pub fn add_consistent(mut a: Self, mut b: Self) -> Option<Self> {
        if a.percent_hint.is_some() && b.percent_hint.is_some() && a.percent_hint != b.percent_hint { return None; }
        let has_percent = a.has_percent || b.has_percent;
        let hint = a.percent_hint.or(b.percent_hint)
            .or_else(|| (a.percent != 0 && b.percent == 0).then(|| b.single_base_dim()).flatten())
            .or_else(|| (b.percent != 0 && a.percent == 0).then(|| a.single_base_dim()).flatten());
        if let Some(h) = hint { a = a.apply_hint(h)?; b = b.apply_hint(h)?; }
        (a.dims == b.dims && a.percent == b.percent).then_some(Self { dims: a.dims, percent: a.percent, percent_hint: hint, has_percent })
    }

    /// CSS Values L4 §10.7: product type for `*`.
    #[must_use]
    pub fn mul_types(mut a: Self, mut b: Self) -> Option<Self> {
        if a.percent_hint.is_some() && b.percent_hint.is_some() && a.percent_hint != b.percent_hint { return None; }
        let has_percent = a.has_percent || b.has_percent;
        let hint = a.percent_hint.or(b.percent_hint);
        if let Some(h) = hint { a = a.apply_hint(h)?; b = b.apply_hint(h)?; }
        let mut dims = [0i8; 6];
        for i in 0..6 { dims[i] = a.dims[i].checked_add(b.dims[i])?; }
        Some(Self { dims, percent: a.percent.checked_add(b.percent)?, percent_hint: hint, has_percent })
    }

    /// CSS Values L4 §10.7: quotient type for `/`.
    #[must_use]
    pub fn div_types(mut a: Self, mut b: Self) -> Option<Self> {
        if a.percent_hint.is_some() && b.percent_hint.is_some() && a.percent_hint != b.percent_hint { return None; }
        let has_percent = a.has_percent || b.has_percent;
        let hint = a.percent_hint.or(b.percent_hint);
        if let Some(h) = hint { a = a.apply_hint(h)?; b = b.apply_hint(h)?; }
        let mut dims = [0i8; 6];
        for i in 0..6 { dims[i] = a.dims[i].checked_sub(b.dims[i])?; }
        Some(Self { dims, percent: a.percent.checked_sub(b.percent)?, percent_hint: hint, has_percent })
    }

    fn as_hinted_number(self) -> Option<Self> {
        if self.dims == [0; 6] && self.percent == 0 { return Some(self); }
        if self.percent != 0 {
            let h = self.percent_hint.or_else(|| self.single_base_dim())?;
            let a = self.apply_hint(h)?;
            if a.dims == [0; 6] && a.percent == 0 { return Some(a); }
        }
        None
    }

    #[must_use] pub fn is_number(self) -> bool { self.dims == [0; 6] && self.percent == 0 && self.percent_hint.is_none() && !self.has_percent }
    #[must_use] pub fn is_pure_percent(self) -> bool { self.dims == [0; 6] && self.percent == 1 && self.percent_hint.is_none() }

    #[must_use]
    pub fn matches_target(self, target: ValueKind) -> bool {
        let exact_dim = |d: BaseDim| self.dims == Self::base(d).dims && self.percent == 0 && self.percent_hint.is_none() && !self.has_percent;
        match target {
            ValueKind::Number | ValueKind::Integer => self.is_number(),
            ValueKind::Percentage => self.is_pure_percent(),
            ValueKind::NumberOrPercentage => self.dims == [0; 6] && (self.percent == 0 || self.percent == 1) && self.percent_hint.is_none(),
            ValueKind::Length => exact_dim(BaseDim::Length),
            ValueKind::LengthPercentage => self.is_pure_percent() || self.apply_hint(BaseDim::Length).is_some_and(|t| t.dims == Self::base(BaseDim::Length).dims && t.percent == 0),
            ValueKind::Angle => exact_dim(BaseDim::Angle),
            ValueKind::Time => exact_dim(BaseDim::Time),
            ValueKind::Frequency => exact_dim(BaseDim::Frequency),
            ValueKind::Resolution => exact_dim(BaseDim::Resolution),
            ValueKind::Flex => exact_dim(BaseDim::Flex),
        }
    }
}

/// Canonical unit attached to a [`MathNode::Dimension`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MathUnit {
    Length(LengthUnit),
    Deg,
    Sec,
    Hz,
    Dppx,
    Fr,
}

#[rustfmt::skip]
impl MathUnit {
    #[must_use] pub const fn as_str(self) -> &'static str {
        match self { Self::Length(lu) => lu.as_str(), Self::Deg => "deg", Self::Sec => "s", Self::Hz => "hz", Self::Dppx => "dppx", Self::Fr => "fr" }
    }
    #[must_use] pub const fn base_dim(self) -> BaseDim {
        match self { Self::Length(_) => BaseDim::Length, Self::Deg => BaseDim::Angle, Self::Sec => BaseDim::Time, Self::Hz => BaseDim::Frequency, Self::Dppx => BaseDim::Resolution, Self::Fr => BaseDim::Flex }
    }
    #[must_use] pub const fn is_absolute(self) -> bool { !matches!(self, Self::Length(lu) if !matches!(lu, LengthUnit::Px)) }

    /// Parses any CSS Values L4 unit, returning `(scale_to_canonical, canonical_unit)`.
    #[must_use]
    pub fn parse_unit(unit: &str) -> Option<(f64, Self)> {
        let res = match unit.to_ascii_lowercase().as_str() {
            "px" => (1.0, Self::Length(LengthUnit::Px)), "cm" => (96.0 / 2.54, Self::Length(LengthUnit::Px)),
            "mm" => (96.0 / 25.4, Self::Length(LengthUnit::Px)), "q" => (96.0 / 101.6, Self::Length(LengthUnit::Px)),
            "in" => (96.0, Self::Length(LengthUnit::Px)), "pc" => (16.0, Self::Length(LengthUnit::Px)),
            "pt" => (96.0 / 72.0, Self::Length(LengthUnit::Px)), "deg" => (1.0, Self::Deg),
            "rad" => (180.0 / std::f64::consts::PI, Self::Deg), "grad" => (0.9, Self::Deg), "turn" => (360.0, Self::Deg),
            "s" => (1.0, Self::Sec), "ms" => (0.001, Self::Sec), "hz" => (1.0, Self::Hz), "khz" => (1000.0, Self::Hz),
            "dppx" | "x" => (1.0, Self::Dppx), "dpi" => (1.0 / 96.0, Self::Dppx), "dpcm" => (2.54 / 96.0, Self::Dppx),
            "fr" => (1.0, Self::Fr), other => (1.0, Self::Length(LengthUnit::from_unit_str(other)?)),
        };
        Some(res)
    }
}

/// Rounding strategy for CSS Values L4 `round()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RoundStrategy {
    Nearest,
    Up,
    Down,
    ToZero,
}

#[rustfmt::skip]
impl RoundStrategy {
    #[must_use] pub const fn css_name(self) -> &'static str {
        match self { Self::Nearest => "nearest", Self::Up => "up", Self::Down => "down", Self::ToZero => "to-zero" }
    }
}

/// W3C CSS Values L4 canonical calculation tree node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MathNode {
    Number(f64),
    Percent(f64),
    Dimension(f64, MathUnit),
    SiblingIndex,
    SiblingCount,
    Sum(Vec<MathNode>),
    Product(Vec<MathNode>),
    Negate(Box<MathNode>),
    Invert(Box<MathNode>),
    Min(Vec<MathNode>),
    Max(Vec<MathNode>),
    Clamp(Option<Box<MathNode>>, Box<MathNode>, Option<Box<MathNode>>),
    Round(RoundStrategy, Box<MathNode>, Option<Box<MathNode>>),
    Mod(Box<MathNode>, Box<MathNode>),
    Rem(Box<MathNode>, Box<MathNode>),
    Sin(Box<MathNode>),
    Cos(Box<MathNode>),
    Tan(Box<MathNode>),
    Asin(Box<MathNode>),
    Acos(Box<MathNode>),
    Atan(Box<MathNode>),
    Atan2(Box<MathNode>, Box<MathNode>),
    Pow(Box<MathNode>, Box<MathNode>),
    Sqrt(Box<MathNode>),
    Hypot(Vec<MathNode>),
    Log(Box<MathNode>, Option<Box<MathNode>>),
    Exp(Box<MathNode>),
    Abs(Box<MathNode>),
    Sign(Box<MathNode>),
    Progress(bool, Box<MathNode>, Box<MathNode>, Box<MathNode>),
}

/// Parsed CSS numeric/math value along with whether the top-level was a math function.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedMath {
    pub node: MathNode,
    pub calc_type: CalcType,
    pub was_math_fn: bool,
}

#[rustfmt::skip]
impl MathNode {
    /// Computes the CSS Values L4 §10.7 algebraic type of this subtree.
    #[must_use]
    pub fn calc_type(&self) -> Option<CalcType> {
        let add_ok = |a: CalcType, b: CalcType| {
            CalcType::add_consistent(a, b)
                .or_else(|| CalcType::add_consistent(a.as_hinted_number()?, b.as_hinted_number()?))
        };
        match self {
            Self::Number(_) | Self::SiblingIndex | Self::SiblingCount => Some(CalcType::NUMBER),
            Self::Percent(_) => Some(CalcType::PERCENT),
            Self::Dimension(_, u) => Some(CalcType::base(u.base_dim())),
            Self::Negate(a) | Self::Abs(a) => a.calc_type(),
            Self::Invert(a) => CalcType::div_types(CalcType::NUMBER, a.calc_type()?),
            Self::Sum(items) | Self::Min(items) | Self::Max(items) | Self::Hypot(items) => {
                let mut acc = items.first()?.calc_type()?;
                for it in &items[1..] { acc = add_ok(acc, it.calc_type()?)?; }
                Some(acc)
            }
            Self::Product(items) => {
                let mut acc = items.first()?.calc_type()?;
                for it in &items[1..] { acc = CalcType::mul_types(acc, it.calc_type()?)?; }
                Some(acc)
            }
            Self::Clamp(a, b, c) => {
                let mut t = b.calc_type()?;
                if let Some(lo) = a { t = add_ok(lo.calc_type()?, t)?; }
                if let Some(hi) = c { t = add_ok(t, hi.calc_type()?)?; }
                Some(t)
            }
            Self::Round(_, a, None) | Self::Sqrt(a) | Self::Exp(a) => a.calc_type()?.as_hinted_number(),
            Self::Round(_, a, Some(b)) | Self::Mod(a, b) | Self::Rem(a, b) => add_ok(a.calc_type()?, b.calc_type()?),
            Self::Sin(a) | Self::Cos(a) | Self::Tan(a) => {
                let t = a.calc_type()?;
                if let Some(hn) = t.as_hinted_number() { Some(hn) }
                else if t.dims == CalcType::base(BaseDim::Angle).dims && t.percent == 0 {
                    Some(CalcType { dims: [0; 6], percent: 0, percent_hint: t.percent_hint, has_percent: t.has_percent })
                } else { None }
            }
            Self::Asin(a) | Self::Acos(a) | Self::Atan(a) => {
                let hn = a.calc_type()?.as_hinted_number()?;
                Some(CalcType { percent_hint: hn.percent_hint, has_percent: hn.has_percent, ..CalcType::base(BaseDim::Angle) })
            }
            Self::Atan2(y, x) => {
                let t = add_ok(y.calc_type()?, x.calc_type()?)?;
                (t.dims == [0; 6] || t.is_pure_percent() || t.single_base_dim().is_some())
                    .then_some(CalcType { percent_hint: t.percent_hint, has_percent: t.has_percent, ..CalcType::base(BaseDim::Angle) })
            }
            Self::Pow(a, b) => CalcType::add_consistent(a.calc_type()?.as_hinted_number()?, b.calc_type()?.as_hinted_number()?),
            Self::Log(a, b) => {
                let at = a.calc_type()?.as_hinted_number()?;
                match b { None => Some(at), Some(e) => CalcType::add_consistent(at, e.calc_type()?.as_hinted_number()?) }
            }
            Self::Sign(a) => {
                let t = a.calc_type()?;
                (t.as_hinted_number().is_some() || t.is_pure_percent() || t.single_base_dim().is_some())
                    .then_some(CalcType { dims: [0; 6], percent: 0, percent_hint: t.percent_hint, has_percent: t.has_percent })
            }
            Self::Progress(_, v, s, e) => {
                let t = CalcType::add_consistent(CalcType::add_consistent(v.calc_type()?, s.calc_type()?)?, e.calc_type()?)?;
                Some(CalcType { dims: [0; 6], percent: 0, percent_hint: t.percent_hint, has_percent: t.percent_hint.is_some() })
            }
        }
    }

    /// Simplifies this tree per W3C CSS Values L4 §10.8.
    #[must_use]
    pub fn simplify(&self, ctx: Option<&EvalContext>, pct_basis: Option<(f64, bool)>) -> Self {
        let s = |n: &Self| n.simplify(ctx, pct_basis);
        let sv = |v: &[Self]| v.iter().map(s).collect::<Vec<_>>();
        match self {
            Self::Number(n) => Self::Number(*n),
            Self::Percent(p) => match pct_basis {
                Some((basis, false)) => Self::Dimension(*p * basis / 100.0, MathUnit::Length(LengthUnit::Px)),
                Some((basis, true)) => Self::Number(*p * basis / 100.0),
                None => Self::Percent(*p),
            },
            Self::Dimension(v, MathUnit::Length(lu)) if !matches!(lu, LengthUnit::Px) => {
                ctx.map_or(Self::Dimension(*v, MathUnit::Length(*lu)), |c| Self::Dimension(resolve_length_unit_f64(*lu, *v, c), MathUnit::Length(LengthUnit::Px)))
            }
            Self::Dimension(v, u) => Self::Dimension(*v, *u),
            Self::SiblingIndex => ctx.map_or(Self::SiblingIndex, |c| Self::Number(f64::from(c.sibling_index.max(1)))),
            Self::SiblingCount => ctx.map_or(Self::SiblingCount, |c| Self::Number(f64::from(c.sibling_count.max(1)))),
            Self::Negate(a) => simplify_negate(s(a)), Self::Invert(a) => simplify_invert(s(a)),
            Self::Sum(items) => simplify_sum(sv(items)), Self::Product(items) => simplify_product(sv(items)),
            Self::Min(items) => simplify_minmax(true, sv(items), ctx.is_some(), pct_basis.is_some()),
            Self::Max(items) => simplify_minmax(false, sv(items), ctx.is_some(), pct_basis.is_some()),
            Self::Clamp(lo, val, hi) => simplify_clamp(lo.as_deref().map(s), s(val), hi.as_deref().map(s), ctx.is_some(), pct_basis.is_some()),
            Self::Round(st, a, b) => simplify_round(*st, s(a), b.as_deref().map(s)),
            Self::Mod(a, b) => simplify_mod_rem(true, s(a), s(b)), Self::Rem(a, b) => simplify_mod_rem(false, s(a), s(b)),
            Self::Sin(a) => simplify_trig(0, s(a)), Self::Cos(a) => simplify_trig(1, s(a)), Self::Tan(a) => simplify_trig(2, s(a)),
            Self::Asin(a) => simplify_inv_trig(0, s(a)), Self::Acos(a) => simplify_inv_trig(1, s(a)), Self::Atan(a) => simplify_inv_trig(2, s(a)),
            Self::Atan2(y, x) => simplify_atan2(s(y), s(x)),
            Self::Pow(a, b) => simplify_binary_num(s(a), s(b), f64::powf, |x, y| Self::Pow(Box::new(x), Box::new(y))),
            Self::Sqrt(a) => simplify_unary_num(s(a), f64::sqrt, |x| Self::Sqrt(Box::new(x))),
            Self::Exp(a) => simplify_unary_num(s(a), f64::exp, |x| Self::Exp(Box::new(x))),
            Self::Log(a, b) => simplify_log(s(a), b.as_deref().map(s)),
            Self::Hypot(items) => simplify_hypot(sv(items)),
            Self::Abs(a) => simplify_abs(s(a)), Self::Sign(a) => simplify_sign(s(a)),
            Self::Progress(nc, v, st, en) => simplify_progress(*nc, s(v), s(st), s(en)),
        }
    }

    #[must_use]
    pub fn eval_scalar(&self, ctx: &EvalContext, pct_basis: Option<(f64, bool)>) -> f64 {
        match self.simplify(Some(ctx), pct_basis) {
            Self::Number(v) | Self::Dimension(v, _) | Self::Percent(v) => v,
            other if pct_basis.is_none() => match other.simplify(Some(ctx), Some((0.0, false))) {
                Self::Number(v) | Self::Dimension(v, _) | Self::Percent(v) => v,
                _ => f64::NAN,
            },
            _ => f64::NAN,
        }
    }

    #[must_use]
    pub fn resolve_px(&self, percent_basis: f32, ctx: &EvalContext) -> f32 {
        let v = self.eval_scalar(ctx, Some((f64::from(percent_basis), false)));
        if v.is_nan() { 0.0 } else { v.clamp(-MAX_CSS_PX, MAX_CSS_PX) as f32 }
    }

    #[must_use]
    pub fn serialize_specified(&self, was_math_fn: bool) -> String {
        let simp = self.simplify(None, None);
        if !was_math_fn { if let Some(leaf) = serialize_simple_literal(&simp) { return leaf; } }
        serialize_top_level_calc(&simp, true)
    }

    #[must_use]
    pub fn serialize_computed(&self, ctx: &EvalContext, pct_basis: Option<(f64, bool)>, target: ValueKind, min_bound: Option<f64>, max_bound: Option<f64>) -> String {
        match self.simplify(Some(ctx), pct_basis) {
            Self::Number(v) => format_computed_scalar(v, None, target, min_bound, max_bound),
            Self::Percent(p) => format_computed_scalar(p, Some("%"), target, min_bound, max_bound),
            Self::Dimension(v, u) => format_computed_scalar(v, Some(u.as_str()), target, min_bound, max_bound),
            other => serialize_top_level_calc(&other, false),
        }
    }
}

#[rustfmt::skip]
fn resolve_length_unit_f64(unit: LengthUnit, v: f64, ctx: &EvalContext) -> f64 {
    let (fs, rfs) = (f64::from(ctx.font_size_px), f64::from(ctx.root_font_size_px));
    let (lh, rlh) = (f64::from(ctx.line_height_px), f64::from(ctx.root_line_height_px));
    let (vw, vh) = (f64::from(ctx.viewport_width_px) * 0.01, f64::from(ctx.viewport_height_px) * 0.01);
    let (cw, ch) = (f64::from(ctx.container_width_px) * 0.01, f64::from(ctx.container_height_px) * 0.01);
    match unit {
        LengthUnit::Px => v,
        LengthUnit::Em => v * fs,
        LengthUnit::Rem => v * rfs,
        LengthUnit::Ex | LengthUnit::Ch => v * fs * 0.5,
        LengthUnit::Rex | LengthUnit::Rch => v * rfs * 0.5,
        LengthUnit::Cap => v * fs * 0.7,
        LengthUnit::Rcap => v * rfs * 0.7,
        LengthUnit::Ic => v * fs,
        LengthUnit::Ric => v * rfs,
        LengthUnit::Lh => v * lh,
        LengthUnit::Rlh => v * rlh,
        LengthUnit::Vw | LengthUnit::Vi | LengthUnit::Svw | LengthUnit::Svi | LengthUnit::Lvw | LengthUnit::Lvi | LengthUnit::Dvw | LengthUnit::Dvi => v * vw,
        LengthUnit::Vh | LengthUnit::Vb | LengthUnit::Svh | LengthUnit::Svb | LengthUnit::Lvh | LengthUnit::Lvb | LengthUnit::Dvh | LengthUnit::Dvb => v * vh,
        LengthUnit::Vmin | LengthUnit::Svmin | LengthUnit::Lvmin | LengthUnit::Dvmin => v * vw.min(vh),
        LengthUnit::Vmax | LengthUnit::Svmax | LengthUnit::Lvmax | LengthUnit::Dvmax => v * vw.max(vh),
        LengthUnit::Cqw | LengthUnit::Cqi => v * cw,
        LengthUnit::Cqh | LengthUnit::Cqb => v * ch,
        LengthUnit::Cqmin => v * cw.min(ch),
        LengthUnit::Cqmax => v * cw.max(ch),
    }
}

// ---------------------------------------------------------------------------
// W3C CSS Values L4 §10.8 Simplification Helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum LeafUnit {
    Num,
    Pct,
    Dim(MathUnit),
}

#[rustfmt::skip]
fn leaf_parts(n: &MathNode) -> Option<(f64, LeafUnit)> {
    match n { MathNode::Number(v) => Some((*v, LeafUnit::Num)), MathNode::Percent(v) => Some((*v, LeafUnit::Pct)), MathNode::Dimension(v, u) => Some((*v, LeafUnit::Dim(*u))), _ => None }
}

#[rustfmt::skip]
fn make_leaf(v: f64, u: LeafUnit) -> MathNode {
    match u { LeafUnit::Num => MathNode::Number(v), LeafUnit::Pct => MathNode::Percent(v), LeafUnit::Dim(mu) => MathNode::Dimension(v, mu) }
}

#[rustfmt::skip]
fn canonical_nan_for_type(t: Option<CalcType>) -> MathNode {
    match t {
        Some(ct) if ct.is_pure_percent() => MathNode::Percent(f64::NAN),
        Some(ct) => match ct.single_base_dim() {
            Some(BaseDim::Length) => MathNode::Dimension(f64::NAN, MathUnit::Length(LengthUnit::Px)),
            Some(BaseDim::Angle) => MathNode::Dimension(f64::NAN, MathUnit::Deg),
            Some(BaseDim::Time) => MathNode::Dimension(f64::NAN, MathUnit::Sec),
            Some(BaseDim::Frequency) => MathNode::Dimension(f64::NAN, MathUnit::Hz),
            Some(BaseDim::Resolution) => MathNode::Dimension(f64::NAN, MathUnit::Dppx),
            Some(BaseDim::Flex) => MathNode::Dimension(f64::NAN, MathUnit::Fr),
            None => MathNode::Number(f64::NAN),
        },
        None => MathNode::Number(f64::NAN),
    }
}

#[rustfmt::skip]
fn simplify_negate(a: MathNode) -> MathNode {
    match a {
        MathNode::Number(v) => MathNode::Number(-v), MathNode::Percent(v) => MathNode::Percent(-v),
        MathNode::Dimension(v, u) => MathNode::Dimension(-v, u), MathNode::Negate(inner) => *inner,
        MathNode::Product(mut items) => { items.insert(0, MathNode::Number(-1.0)); simplify_product(items) }
        MathNode::Sum(items) => MathNode::Sum(items.into_iter().map(simplify_negate).collect()),
        other => MathNode::Negate(Box::new(other)),
    }
}

#[rustfmt::skip]
fn simplify_invert(a: MathNode) -> MathNode {
    match a { MathNode::Number(v) => MathNode::Number(1.0 / v), MathNode::Invert(inner) => *inner, other => MathNode::Invert(Box::new(other)) }
}

#[rustfmt::skip]
fn simplify_product(raw: Vec<MathNode>) -> MathNode {
    let mut flat = Vec::with_capacity(raw.len());
    for item in raw { match item { MathNode::Product(sub) => flat.extend(sub), other => flat.push(other) } }
    let (mut scalar, mut num_dims, mut den_dims, mut rest) = (1.0_f64, Vec::<(f64, LeafUnit)>::new(), Vec::<(f64, LeafUnit)>::new(), Vec::<MathNode>::new());
    for item in flat {
        match item {
            MathNode::Number(v) => scalar *= v,
            MathNode::Percent(v) => num_dims.push((v, LeafUnit::Pct)),
            MathNode::Dimension(v, u) => num_dims.push((v, LeafUnit::Dim(u))),
            MathNode::Invert(inner) => match *inner {
                MathNode::Number(v) => scalar /= v,
                MathNode::Percent(v) => den_dims.push((v, LeafUnit::Pct)),
                MathNode::Dimension(v, u) => den_dims.push((v, LeafUnit::Dim(u))),
                other => rest.push(MathNode::Invert(Box::new(other))),
            },
            MathNode::Negate(inner) => { scalar = -scalar; rest.push(*inner); }
            other => rest.push(other),
        }
    }
    let mut i = 0;
    while i < num_dims.len() {
        let (nv, nu) = num_dims[i];
        if let Some(j) = den_dims.iter().position(|&(_, du)| du == nu) {
            let (dv, _) = den_dims.remove(j);
            num_dims.remove(i);
            scalar *= nv / dv;
        } else { i += 1; }
    }
    if den_dims.is_empty() && rest.is_empty() {
        if num_dims.is_empty() { return MathNode::Number(scalar); }
        if num_dims.len() == 1 { return make_leaf(num_dims[0].0 * scalar, num_dims[0].1); }
    }
    if num_dims.is_empty() && den_dims.is_empty() && rest.len() == 1 {
        if let MathNode::Sum(terms) = &rest[0] {
            if scalar.is_finite() {
                return simplify_sum(terms.iter().map(|t| simplify_product(vec![MathNode::Number(scalar), t.clone()])).collect());
            }
        }
    }
    let mut out = Vec::new();
    if num_dims.len() > 1 || (!num_dims.is_empty() && !den_dims.is_empty()) {
        for (v, _) in &mut num_dims { scalar *= *v; *v = 1.0; }
        for (v, _) in &mut den_dims { scalar /= *v; *v = 1.0; }
        out.push(MathNode::Number(scalar));
    } else if (scalar - 1.0).abs() > 0.0 || scalar.is_nan() || (num_dims.is_empty() && den_dims.is_empty() && rest.is_empty()) {
        if !num_dims.is_empty() && scalar.is_finite() { num_dims[0].0 *= scalar; } else { out.push(MathNode::Number(scalar)); }
    }
    for (v, u) in num_dims { out.push(make_leaf(v, u)); }
    out.extend(rest);
    for (v, u) in den_dims { out.push(MathNode::Invert(Box::new(make_leaf(v, u)))); }
    if out.len() == 1 { out.pop().unwrap() } else { MathNode::Product(out) }
}

#[rustfmt::skip]
fn product_monomial(n: &MathNode) -> Option<(f64, Vec<LeafUnit>, Vec<LeafUnit>)> {
    let MathNode::Product(items) = n else { return None; };
    let (mut scalar, mut nums, mut dens) = (1.0_f64, Vec::new(), Vec::new());
    for it in items {
        match it {
            MathNode::Number(v) => scalar *= *v,
            MathNode::Percent(v) => { scalar *= *v; nums.push(LeafUnit::Pct); }
            MathNode::Dimension(v, u) => { scalar *= *v; nums.push(LeafUnit::Dim(*u)); }
            MathNode::Invert(inner) => match inner.as_ref() {
                MathNode::Number(v) => scalar /= *v,
                MathNode::Percent(v) => { scalar /= *v; dens.push(LeafUnit::Pct); }
                MathNode::Dimension(v, u) => { scalar /= *v; dens.push(LeafUnit::Dim(*u)); }
                _ => return None,
            },
            _ => return None,
        }
    }
    Some((scalar, nums, dens))
}

#[rustfmt::skip]
fn sum_sort_key(n: &MathNode) -> (u8, &'static str) {
    match n { MathNode::Number(_) => (0, ""), MathNode::Percent(_) => (1, "%"), MathNode::Dimension(_, u) => (2, u.as_str()), _ => (3, "") }
}

#[rustfmt::skip]
fn simplify_sum(raw: Vec<MathNode>) -> MathNode {
    let mut flat = Vec::with_capacity(raw.len());
    for item in raw { match item { MathNode::Sum(sub) => flat.extend(sub), other => flat.push(other) } }
    if flat.iter().any(|x| leaf_parts(x).is_some_and(|(v, _)| v.is_nan())) { return canonical_nan_for_type(MathNode::Sum(flat).calc_type()); }
    let (mut num_acc, mut pct_acc) = (None::<f64>, None::<f64>);
    let (mut dim_acc, mut mono_acc, mut rest) = (Vec::<(MathUnit, f64)>::new(), Vec::<(Vec<LeafUnit>, Vec<LeafUnit>, f64)>::new(), Vec::<MathNode>::new());
    for item in flat {
        match item {
            MathNode::Number(v) => num_acc = Some(num_acc.map_or(v, |a| a + v)),
            MathNode::Percent(v) => pct_acc = Some(pct_acc.map_or(v, |a| a + v)),
            MathNode::Dimension(v, u) => if let Some((_, a)) = dim_acc.iter_mut().find(|(du, _)| *du == u) { *a += v; } else { dim_acc.push((u, v)); },
            other => if let Some((sc, nums, dens)) = product_monomial(&other) {
                if let Some((_, _, a)) = mono_acc.iter_mut().find(|(n, d, _)| *n == nums && *d == dens) { *a += sc; } else { mono_acc.push((nums, dens, sc)); }
            } else { rest.push(other); },
        }
    }
    let mut out = Vec::new();
    if let Some(v) = num_acc { out.push(MathNode::Number(v)); }
    if let Some(v) = pct_acc { out.push(MathNode::Percent(v)); }
    for (u, v) in dim_acc { out.push(MathNode::Dimension(v, u)); }
    for (nums, dens, sc) in mono_acc {
        let mut f = vec![MathNode::Number(sc)];
        for u in nums { f.push(make_leaf(1.0, u)); }
        for u in dens { f.push(MathNode::Invert(Box::new(make_leaf(1.0, u)))); }
        out.push(simplify_product(f));
    }
    out.extend(rest);
    if out.iter().any(|x| leaf_parts(x).is_some_and(|(v, _)| v.is_nan())) { return canonical_nan_for_type(MathNode::Sum(out).calc_type()); }
    out.sort_by_key(sum_sort_key);
    if out.len() == 1 { out.pop().unwrap() } else { MathNode::Sum(out) }
}

#[rustfmt::skip]
fn simplify_minmax(is_min: bool, args: Vec<MathNode>, is_computed: bool, has_pct_basis: bool) -> MathNode {
    if args.is_empty() { return MathNode::Number(f64::NAN); }
    if args.len() == 1 { return args.into_iter().next().unwrap(); }
    let all_same = leaf_parts(&args[0]).and_then(|(_, u0)| args.iter().all(|a| leaf_parts(a).is_some_and(|(_, u)| u == u0)).then_some(u0));
    let has_nan = args.iter().any(|a| leaf_parts(a).is_some_and(|(v, _)| v.is_nan()));
    if has_nan && (is_computed || all_same.is_some()) {
        return canonical_nan_for_type(if is_min { MathNode::Min(args) } else { MathNode::Max(args) }.calc_type());
    }
    if let Some(u0) = all_same {
        if u0 == LeafUnit::Pct && !has_pct_basis { return if is_min { MathNode::Min(args) } else { MathNode::Max(args) }; }
        let mut best = leaf_parts(&args[0]).unwrap().0;
        for a in &args[1..] {
            let v = leaf_parts(a).unwrap().0;
            best = if is_min { css_min_f64(best, v) } else { css_max_f64(best, v) };
        }
        return make_leaf(best, u0);
    }
    if is_min { MathNode::Min(args) } else { MathNode::Max(args) }
}

#[rustfmt::skip]
fn css_min_f64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() { f64::NAN }
    else if a == 0.0 && b == 0.0 { if a.is_sign_negative() || b.is_sign_negative() { -0.0 } else { 0.0 } }
    else if a < b { a } else { b }
}

#[rustfmt::skip]
fn css_max_f64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() { f64::NAN }
    else if a == 0.0 && b == 0.0 { if a.is_sign_positive() || b.is_sign_positive() { 0.0 } else { -0.0 } }
    else if a > b { a } else { b }
}

#[rustfmt::skip]
fn simplify_clamp(lo: Option<MathNode>, val: MathNode, hi: Option<MathNode>, is_computed: bool, has_pct_basis: bool) -> MathNode {
    match (lo, hi) {
        (None, None) => val,
        (None, Some(h)) => {
            if let (Some((vv, vu)), Some((hv, hu))) = (leaf_parts(&val), leaf_parts(&h)) {
                if vu == hu && (vu != LeafUnit::Pct || has_pct_basis) { return make_leaf(css_min_f64(vv, hv), vu); }
            }
            if is_computed && [&val, &h].iter().any(|x| leaf_parts(x).is_some_and(|(v, _)| v.is_nan())) {
                return canonical_nan_for_type(MathNode::Clamp(None, Box::new(val), Some(Box::new(h))).calc_type());
            }
            MathNode::Clamp(None, Box::new(val), Some(Box::new(h)))
        }
        (Some(l), None) => {
            if let (Some((lv, lu)), Some((vv, vu))) = (leaf_parts(&l), leaf_parts(&val)) {
                if lu == vu && (lu != LeafUnit::Pct || has_pct_basis) { return make_leaf(css_max_f64(vv, lv), lu); }
            }
            if is_computed && [&l, &val].iter().any(|x| leaf_parts(x).is_some_and(|(v, _)| v.is_nan())) {
                return canonical_nan_for_type(MathNode::Clamp(Some(Box::new(l)), Box::new(val), None).calc_type());
            }
            MathNode::Clamp(Some(Box::new(l)), Box::new(val), None)
        }
        (Some(l), Some(h)) => {
            let same = match (leaf_parts(&l), leaf_parts(&val), leaf_parts(&h)) {
                (Some((lv, lu)), Some((vv, vu)), Some((hv, hu))) if lu == vu && vu == hu => Some((lv, vv, hv, lu)),
                _ => None,
            };
            let has_nan = [&l, &val, &h].iter().any(|x| leaf_parts(x).is_some_and(|(v, _)| v.is_nan()));
            if has_nan && (is_computed || same.is_some()) {
                return canonical_nan_for_type(MathNode::Clamp(Some(Box::new(l)), Box::new(val), Some(Box::new(h))).calc_type());
            }
            if let Some((lv, vv, hv, u)) = same { if u != LeafUnit::Pct || has_pct_basis { return make_leaf(css_max_f64(lv, css_min_f64(vv, hv)), u); } }
            MathNode::Clamp(Some(Box::new(l)), Box::new(val), Some(Box::new(h)))
        }
    }
}

#[rustfmt::skip]
fn eval_round_f64(st: RoundStrategy, a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() || b == 0.0 || (a.is_infinite() && b.is_infinite()) { return f64::NAN; }
    if a.is_infinite() { return a; }
    if b.is_infinite() {
        return match st {
            RoundStrategy::Nearest | RoundStrategy::ToZero => if a.is_sign_negative() { -0.0 } else { 0.0 },
            RoundStrategy::Up => if a > 0.0 { f64::INFINITY } else if a == 0.0 { a } else { -0.0 },
            RoundStrategy::Down => if a < 0.0 { f64::NEG_INFINITY } else if a == 0.0 { a } else { 0.0 },
        };
    }
    let step = b.abs();
    let rem = a % step;
    if rem == 0.0 { return a; }
    let lower = (a / step).floor() * step;
    let upper = (a / step).ceil() * step;
    let fix_neg_zero = |r: f64| if r == 0.0 && a.is_sign_negative() { -0.0 } else { r };
    match st {
        RoundStrategy::Up => fix_neg_zero(upper),
        RoundStrategy::Down => fix_neg_zero(lower),
        RoundStrategy::ToZero => if a < 0.0 { fix_neg_zero(upper) } else { lower },
        RoundStrategy::Nearest => {
            let dl = (a - lower).abs();
            let du = (upper - a).abs();
            if du <= dl { fix_neg_zero(upper) } else { fix_neg_zero(lower) }
        }
    }
}

fn simplify_round(st: RoundStrategy, a: MathNode, b: Option<MathNode>) -> MathNode {
    match (&a, &b) {
        (MathNode::Number(av), None) => MathNode::Number(eval_round_f64(st, *av, 1.0)),
        (_, Some(bn)) => {
            if let (Some((av, au)), Some((bv, bu))) = (leaf_parts(&a), leaf_parts(bn)) {
                if au == bu {
                    return make_leaf(eval_round_f64(st, av, bv), au);
                }
            }
            MathNode::Round(st, Box::new(a), b.map(Box::new))
        }
        _ => MathNode::Round(st, Box::new(a), None),
    }
}

#[rustfmt::skip]
fn simplify_mod_rem(is_mod: bool, a: MathNode, b: MathNode) -> MathNode {
    if let (Some((av, au)), Some((bv, bu))) = (leaf_parts(&a), leaf_parts(&b)) {
        if au == bu {
            let r = if av.is_nan() || bv.is_nan() || bv == 0.0 || av.is_infinite() {
                f64::NAN
            } else if bv.is_infinite() {
                if !is_mod || av.is_sign_positive() == bv.is_sign_positive() { av } else { f64::NAN }
            } else if is_mod {
                let m = av - bv * (av / bv).floor();
                if m == 0.0 { if bv.is_sign_negative() { -0.0 } else { 0.0 } } else { m }
            } else {
                av % bv
            };
            return make_leaf(r, au);
        }
    }
    if is_mod { MathNode::Mod(Box::new(a), Box::new(b)) } else { MathNode::Rem(Box::new(a), Box::new(b)) }
}

#[rustfmt::skip]
fn simplify_trig(kind: u8, a: MathNode) -> MathNode {
    let eval = |deg: Option<f64>, rad: f64| -> f64 {
        if !rad.is_finite() { return f64::NAN; }
        if let Some(d) = deg {
            let r = d.rem_euclid(360.0);
            match kind {
                0 => if d == 0.0 { d } else if r == 0.0 || r == 180.0 { 0.0 } else if r == 90.0 { 1.0 } else if r == 270.0 { -1.0 } else { d.to_radians().sin() },
                1 => if r == 0.0 { 1.0 } else if r == 90.0 || r == 270.0 { 0.0 } else if r == 180.0 { -1.0 } else { d.to_radians().cos() },
                _ => if d == 0.0 { d } else if r == 0.0 || r == 180.0 { 0.0 } else if r == 90.0 { f64::INFINITY } else if r == 270.0 { f64::NEG_INFINITY } else if r == 45.0 || r == 225.0 { 1.0 } else if r == 135.0 || r == 315.0 { -1.0 } else { d.to_radians().tan() },
            }
        } else {
            match kind { 0 => rad.sin(), 1 => rad.cos(), _ => rad.tan() }
        }
    };
    match a {
        MathNode::Number(r) => MathNode::Number(eval(None, r)),
        MathNode::Dimension(d, MathUnit::Deg) => MathNode::Number(eval(Some(d), d.to_radians())),
        other => match kind { 0 => MathNode::Sin(Box::new(other)), 1 => MathNode::Cos(Box::new(other)), _ => MathNode::Tan(Box::new(other)) },
    }
}

#[rustfmt::skip]
fn simplify_inv_trig(kind: u8, a: MathNode) -> MathNode {
    match a {
        MathNode::Number(v) => MathNode::Dimension(match kind { 0 => v.asin(), 1 => v.acos(), _ => v.atan() }.to_degrees(), MathUnit::Deg),
        other => match kind { 0 => MathNode::Asin(Box::new(other)), 1 => MathNode::Acos(Box::new(other)), _ => MathNode::Atan(Box::new(other)) },
    }
}

#[rustfmt::skip]
fn simplify_atan2(y: MathNode, x: MathNode) -> MathNode {
    if let (Some((yv, yu)), Some((xv, xu))) = (leaf_parts(&y), leaf_parts(&x)) {
        if yu == xu { return MathNode::Dimension(yv.atan2(xv).to_degrees(), MathUnit::Deg); }
    }
    MathNode::Atan2(Box::new(y), Box::new(x))
}

fn simplify_unary_num(a: MathNode, f: fn(f64) -> f64, wrap: fn(MathNode) -> MathNode) -> MathNode {
    if let MathNode::Number(v) = a {
        MathNode::Number(f(v))
    } else {
        wrap(a)
    }
}

fn simplify_binary_num(
    a: MathNode,
    b: MathNode,
    f: fn(f64, f64) -> f64,
    wrap: fn(MathNode, MathNode) -> MathNode,
) -> MathNode {
    if let (MathNode::Number(av), MathNode::Number(bv)) = (&a, &b) {
        MathNode::Number(f(*av, *bv))
    } else {
        wrap(a, b)
    }
}

#[rustfmt::skip]
fn simplify_log(a: MathNode, b: Option<MathNode>) -> MathNode {
    match (&a, &b) {
        (MathNode::Number(v), None) => MathNode::Number(if *v < 0.0 { f64::NAN } else { v.ln() }),
        (MathNode::Number(v), Some(MathNode::Number(base))) => MathNode::Number(
            if v.is_nan() || base.is_nan() || *v < 0.0 || *base <= 0.0 || *base == 1.0 { f64::NAN }
            else if *base == 10.0 { v.log10() } else if *base == 2.0 { v.log2() } else { v.ln() / base.ln() }
        ),
        _ => MathNode::Log(Box::new(a), b.map(Box::new)),
    }
}

#[rustfmt::skip]
fn simplify_hypot(args: Vec<MathNode>) -> MathNode {
    if let Some((_, u0)) = args.first().and_then(leaf_parts) {
        if args.iter().all(|a| leaf_parts(a).is_some_and(|(_, u)| u == u0)) {
            let mut acc = 0.0_f64;
            for a in &args { acc = acc.hypot(leaf_parts(a).unwrap().0); }
            return make_leaf(acc, u0);
        }
    }
    MathNode::Hypot(args)
}

fn simplify_abs(a: MathNode) -> MathNode {
    if let Some((v, u)) = leaf_parts(&a) {
        make_leaf(v.abs(), u)
    } else {
        MathNode::Abs(Box::new(a))
    }
}

#[rustfmt::skip]
fn simplify_sign(a: MathNode) -> MathNode {
    if let Some((v, _)) = leaf_parts(&a) {
        MathNode::Number(if v.is_nan() { f64::NAN } else if v == 0.0 { v } else if v > 0.0 { 1.0 } else { -1.0 })
    } else { MathNode::Sign(Box::new(a)) }
}

#[rustfmt::skip]
fn simplify_progress(no_clamp: bool, v: MathNode, s: MathNode, e: MathNode) -> MathNode {
    if let (Some((vv, vu)), Some((sv, su)), Some((ev, eu))) = (leaf_parts(&v), leaf_parts(&s), leaf_parts(&e)) {
        if vu == su && su == eu {
            let raw = if vv.is_nan() || sv.is_nan() || ev.is_nan() { f64::NAN }
                else if ev == sv { if !no_clamp || vv == sv { 0.0 } else { (vv - sv) / 0.0 } }
                else { (vv - sv) / (ev - sv) };
            return MathNode::Number(if no_clamp || raw.is_nan() { raw } else { raw.clamp(0.0, 1.0) });
        }
    }
    MathNode::Progress(no_clamp, Box::new(v), Box::new(s), Box::new(e))
}

// ---------------------------------------------------------------------------
// W3C CSS Values L4 §10.9 Canonical Serialization
// ---------------------------------------------------------------------------

#[must_use]
#[rustfmt::skip]
pub fn format_math_f64(v: f64) -> String {
    if v.is_nan() { return "NaN".to_string(); }
    if v.is_infinite() { return if v.is_sign_positive() { "infinity" } else { "-infinity" }.to_string(); }
    let v = if v == 0.0 { 0.0 } else { v };
    if v.abs() >= 1e15 || (v != 0.0 && v.abs() < 1e-6) {
        let s = format!("{v:e}");
        return if s.contains("e-") { s } else { s.replace('e', "e+") };
    }
    let dec: usize = if v.abs() >= 1.0 { (5 - v.abs().log10().floor() as i32).clamp(0, 6) as usize } else { 6 };
    let factor = 10f64.powi(dec as i32);
    let r = (v * factor).round() / factor;
    let r = if r == 0.0 { 0.0 } else { r };
    if (r - r.round()).abs() < 1e-6 && r.abs() < 1e15 { format!("{}", r.round() as i64) }
    else { format!("{r:.dec$}").trim_end_matches('0').trim_end_matches('.').to_string() }
}

#[rustfmt::skip]
fn format_computed_scalar(v: f64, unit: Option<&str>, target: ValueKind, min_b: Option<f64>, max_b: Option<f64>) -> String {
    let mut val = if v.is_nan() { min_b.unwrap_or(0.0) } else { v };
    if target == ValueKind::Integer {
        let c = if val.is_infinite() { if val.is_sign_positive() { i32::MAX } else { i32::MIN } }
        else { (val + 0.5).floor().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32 };
        return c.to_string();
    }
    let limit = match target { ValueKind::Length | ValueKind::LengthPercentage => MAX_CSS_PX, _ => MAX_CSS_NUM };
    if val.is_infinite() {
        val = if val.is_sign_positive() { max_b.unwrap_or(limit) } else { min_b.unwrap_or(-limit) };
    } else {
        if matches!(target, ValueKind::Length | ValueKind::LengthPercentage) && unit != Some("%") { val = val.clamp(-limit, limit); }
        if let Some(lo) = min_b { val = val.max(lo); }
        if let Some(hi) = max_b { val = val.min(hi); }
    }
    let num = if val.abs() >= 1e15 { format!("{val:.5e}") } else { format_math_f64(val) };
    match unit { Some(u) => format!("{num}{u}"), None => num }
}

#[rustfmt::skip]
fn serialize_simple_literal(n: &MathNode) -> Option<String> {
    match n {
        MathNode::Number(v) if v.is_finite() => Some(format_math_f64(*v)),
        MathNode::Percent(v) if v.is_finite() => Some(format!("{}%", format_math_f64(*v))),
        MathNode::Dimension(v, u) if v.is_finite() => Some(format!("{}{}", format_math_f64(*v), u.as_str())),
        _ => None,
    }
}

#[rustfmt::skip]
fn serialize_top_level_calc(n: &MathNode, specified: bool) -> String {
    match n {
        MathNode::Min(_) | MathNode::Max(_) | MathNode::Clamp(_, _, _) | MathNode::Round(_, _, _)
        | MathNode::Mod(_, _) | MathNode::Rem(_, _) | MathNode::Sin(_) | MathNode::Cos(_) | MathNode::Tan(_)
        | MathNode::Asin(_) | MathNode::Acos(_) | MathNode::Atan(_) | MathNode::Atan2(_, _)
        | MathNode::Pow(_, _) | MathNode::Sqrt(_) | MathNode::Hypot(_) | MathNode::Log(_, _)
        | MathNode::Exp(_) | MathNode::Abs(_) | MathNode::Sign(_) | MathNode::Progress(_, _, _, _)
        | MathNode::SiblingIndex | MathNode::SiblingCount => serialize_inner(n, false),
        _ if !specified => serialize_simple_literal(n).unwrap_or_else(|| format!("calc({})", serialize_inner(n, false))),
        _ => format!("calc({})", serialize_inner(n, false)),
    }
}

#[rustfmt::skip]
fn serialize_leaf_calc(v: f64, unit: Option<&str>) -> String {
    if !v.is_finite() {
        let sym = format_math_f64(v);
        return match unit { Some(u) => format!("{sym} * 1{u}"), None => sym };
    }
    match unit { Some(u) => format!("{}{u}", format_math_f64(v)), None => format_math_f64(v) }
}

#[rustfmt::skip]
fn serialize_inner(n: &MathNode, parens: bool) -> String {
    let wrap = |s: String| if parens { format!("({s})") } else { s };
    let si = |x: &MathNode| serialize_inner(x, false);
    match n {
        MathNode::Number(v) => serialize_leaf_calc(*v, None),
        MathNode::Percent(v) => serialize_leaf_calc(*v, Some("%")),
        MathNode::Dimension(v, u) => serialize_leaf_calc(*v, Some(u.as_str())),
        MathNode::SiblingIndex => "sibling-index()".to_string(),
        MathNode::SiblingCount => "sibling-count()".to_string(),
        MathNode::Negate(a) => wrap(format!("-1 * {}", serialize_inner(a, true))),
        MathNode::Invert(a) => wrap(format!("1 / {}", serialize_inner(a, true))),
        MathNode::Sum(items) => {
            let mut out = String::new();
            for (i, item) in items.iter().enumerate() {
                if i == 0 { out.push_str(&serialize_inner(item, matches!(item, MathNode::Sum(_)))); }
                else {
                    match item {
                        MathNode::Number(v) if v.is_finite() && *v < 0.0 => { out.push_str(" - "); out.push_str(&format_math_f64(-*v)); }
                        MathNode::Percent(v) if v.is_finite() && *v < 0.0 => { out.push_str(" - "); out.push_str(&format!("{}%", format_math_f64(-*v))); }
                        MathNode::Dimension(v, u) if v.is_finite() && *v < 0.0 => { out.push_str(" - "); out.push_str(&format!("{}{}", format_math_f64(-*v), u.as_str())); }
                        MathNode::Negate(inner) => { out.push_str(" - "); out.push_str(&serialize_inner(inner, true)); }
                        other => { out.push_str(" + "); out.push_str(&serialize_inner(other, matches!(other, MathNode::Sum(_)))); }
                    }
                }
            }
            wrap(out)
        }
        MathNode::Product(items) => {
            let mut out = String::new();
            for (i, item) in items.iter().enumerate() {
                if i == 0 { out.push_str(&serialize_inner(item, matches!(item, MathNode::Sum(_) | MathNode::Product(_)))); }
                else if let MathNode::Invert(inner) = item { out.push_str(" / "); out.push_str(&serialize_inner(inner, matches!(**inner, MathNode::Sum(_) | MathNode::Product(_) | MathNode::Invert(_)))); }
                else { out.push_str(" * "); out.push_str(&serialize_inner(item, matches!(item, MathNode::Sum(_) | MathNode::Product(_)))); }
            }
            wrap(out)
        }
        MathNode::Min(a) => fmt_fn("min", a),
        MathNode::Max(a) => fmt_fn("max", a),
        MathNode::Clamp(a, b, c) => format!("clamp({}, {}, {})", a.as_deref().map_or_else(|| "none".to_string(), si), si(b), c.as_deref().map_or_else(|| "none".to_string(), si)),
        MathNode::Round(st, a, b) => {
            let pfx = if *st == RoundStrategy::Nearest { String::new() } else { format!("{}, ", st.css_name()) };
            match b { Some(bv) => format!("round({pfx}{}, {})", si(a), si(bv)), None => format!("round({pfx}{})", si(a)) }
        }
        MathNode::Mod(a, b) => format!("mod({}, {})", si(a), si(b)),
        MathNode::Rem(a, b) => format!("rem({}, {})", si(a), si(b)),
        MathNode::Sin(a) => format!("sin({})", si(a)), MathNode::Cos(a) => format!("cos({})", si(a)), MathNode::Tan(a) => format!("tan({})", si(a)),
        MathNode::Asin(a) => format!("asin({})", si(a)), MathNode::Acos(a) => format!("acos({})", si(a)), MathNode::Atan(a) => format!("atan({})", si(a)),
        MathNode::Atan2(y, x) => format!("atan2({}, {})", si(y), si(x)), MathNode::Pow(a, b) => format!("pow({}, {})", si(a), si(b)),
        MathNode::Sqrt(a) => format!("sqrt({})", si(a)), MathNode::Hypot(a) => fmt_fn("hypot", a),
        MathNode::Log(a, Some(b)) => format!("log({}, {})", si(a), si(b)), MathNode::Log(a, None) => format!("log({})", si(a)),
        MathNode::Exp(a) => format!("exp({})", si(a)), MathNode::Abs(a) => format!("abs({})", si(a)), MathNode::Sign(a) => format!("sign({})", si(a)),
        MathNode::Progress(nc, v, s, e) => format!("progress({}{}, {}, {})", if *nc { "no-clamp " } else { "" }, si(v), si(s), si(e)),
    }
}

fn fmt_fn(name: &str, args: &[MathNode]) -> String {
    let joined = args
        .iter()
        .map(|a| serialize_inner(a, false))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{name}({joined})")
}

// ---------------------------------------------------------------------------
// Strict W3C CSS Values L4 Parser (`cssparser`-backed)
// ---------------------------------------------------------------------------

#[rustfmt::skip]
pub fn is_css_math_fn(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "calc" | "min" | "max" | "clamp" | "round" | "mod" | "rem"
            | "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "atan2"
            | "pow" | "sqrt" | "hypot" | "log" | "exp" | "abs" | "sign"
            | "progress" | "sibling-index" | "sibling-count"
    )
}

/// Parses a single CSS value matching `target` (literal or any CSS Values L4 math function).
#[must_use]
pub fn parse_math_value(input: &str, target: ValueKind) -> Option<ParsedMath> {
    let mut pi = ParserInput::new(input);
    let mut p = Parser::new(&mut pi);
    let parsed = parse_math_value_from_parser(&mut p, target)?;
    p.is_exhausted().then_some(parsed)
}

/// Parses a single CSS value matching `target` from a live `cssparser::Parser`.
#[rustfmt::skip]
pub fn parse_math_value_from_parser<'i, 't>(p: &mut Parser<'i, 't>, target: ValueKind) -> Option<ParsedMath> {
    let (state, start) = (p.state(), p.position());
    let tok = p.next().ok()?.clone();
    let (node, was_math_fn) = match tok {
        Token::Number { value, int_value, .. } => {
            if target == ValueKind::Integer && int_value.is_none() { p.reset(&state); return None; }
            let v = exact_f64(p.slice_from(start), 0, f64::from(value));
            if v == 0.0 && matches!(target, ValueKind::Length | ValueKind::LengthPercentage) { (MathNode::Dimension(0.0, MathUnit::Length(LengthUnit::Px)), false) }
            else { (MathNode::Number(v), false) }
        }
        Token::Percentage { unit_value, .. } => (MathNode::Percent(exact_f64(p.slice_from(start), 1, f64::from(unit_value) * 100.0)), false),
        Token::Dimension { value, unit, .. } => {
            let v = exact_f64(p.slice_from(start), unit.len(), f64::from(value));
            let Some((scale, mu)) = MathUnit::parse_unit(unit.as_ref()) else { p.reset(&state); return None; };
            (MathNode::Dimension(v * scale, mu), false)
        }
        Token::Function(ref fname) if is_css_math_fn(fname.as_ref()) => {
            let lower = fname.to_ascii_lowercase();
            let node = p.parse_nested_block(|n| parse_math_fn(&lower, n).ok_or_else(|| n.new_custom_error::<_, ()>(()))).ok()?;
            (node, true)
        }
        _ => { p.reset(&state); return None; }
    };
    let Some(ct) = node.calc_type().filter(|t| t.matches_target(target)) else { p.reset(&state); return None; };
    Some(ParsedMath { node, calc_type: ct, was_math_fn })
}

fn exact_f64(slice: &str, suffix_len: usize, fallback: f64) -> f64 {
    let t = slice.trim();
    t.get(..t.len().saturating_sub(suffix_len))
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(fallback)
}

pub(crate) fn parse_math_fn_hinted<'i, 't>(name: &str, p: &mut Parser<'i, 't>) -> Option<MathNode> {
    p.parse_nested_block(|n| parse_math_fn(name, n).ok_or_else(|| n.new_custom_error::<_, ()>(())))
        .ok()
}

#[rustfmt::skip]
fn parse_math_fn<'i, 't>(name: &str, p: &mut Parser<'i, 't>) -> Option<MathNode> {
    match name {
        "sibling-index" => p.is_exhausted().then_some(MathNode::SiblingIndex),
        "sibling-count" => p.is_exhausted().then_some(MathNode::SiblingCount),
        "calc" => { let e = parse_sum(p)?; p.is_exhausted().then_some(e) }
        "clamp" => {
            let lo = if p.try_parse(|p| p.expect_ident_matching("none")).is_ok() { None } else { Some(Box::new(parse_sum(p)?)) };
            p.expect_comma().ok()?;
            let mid = Box::new(parse_sum(p)?);
            p.expect_comma().ok()?;
            let hi = if p.try_parse(|p| p.expect_ident_matching("none")).is_ok() { None } else { Some(Box::new(parse_sum(p)?)) };
            p.is_exhausted().then_some(MathNode::Clamp(lo, mid, hi))
        }
        "progress" => {
            let nc = p.try_parse(|p| p.expect_ident_matching("no-clamp")).is_ok();
            let v = Box::new(parse_sum(p)?); p.expect_comma().ok()?;
            let s = Box::new(parse_sum(p)?); p.expect_comma().ok()?;
            let e = Box::new(parse_sum(p)?);
            p.is_exhausted().then_some(MathNode::Progress(nc, v, s, e))
        }
        "round" => {
            let strat = p.try_parse(|p| -> Result<RoundStrategy, cssparser::ParseError<'i, ()>> {
                let st = match p.expect_ident()?.to_ascii_lowercase().as_str() {
                    "nearest" => RoundStrategy::Nearest, "up" => RoundStrategy::Up,
                    "down" => RoundStrategy::Down, "to-zero" => RoundStrategy::ToZero,
                    _ => return Err(p.new_custom_error(())),
                };
                p.expect_comma()?; Ok(st)
            }).unwrap_or(RoundStrategy::Nearest);
            let a = parse_sum(p)?;
            let b = if p.try_parse(Parser::expect_comma).is_ok() { Some(Box::new(parse_sum(p)?)) } else { None };
            p.is_exhausted().then_some(MathNode::Round(strat, Box::new(a), b))
        }
        _ => {
            let mut args = Vec::new();
            loop { args.push(parse_sum(p)?); if p.try_parse(Parser::expect_comma).is_err() { break; } }
            if !p.is_exhausted() { return None; }
            build_math_fn(name, args)
        }
    }
}

#[rustfmt::skip]
fn build_math_fn(name: &str, mut args: Vec<MathNode>) -> Option<MathNode> {
    match name {
        "min" if !args.is_empty() => Some(MathNode::Min(args)),
        "max" if !args.is_empty() => Some(MathNode::Max(args)),
        "hypot" if !args.is_empty() => Some(MathNode::Hypot(args)),
        "mod" if args.len() == 2 => { let b = args.pop()?; let a = args.pop()?; Some(MathNode::Mod(Box::new(a), Box::new(b))) }
        "rem" if args.len() == 2 => { let b = args.pop()?; let a = args.pop()?; Some(MathNode::Rem(Box::new(a), Box::new(b))) }
        "atan2" if args.len() == 2 => { let x = args.pop()?; let y = args.pop()?; Some(MathNode::Atan2(Box::new(y), Box::new(x))) }
        "pow" if args.len() == 2 => { let b = args.pop()?; let a = args.pop()?; Some(MathNode::Pow(Box::new(a), Box::new(b))) }
        "log" if args.len() == 1 => Some(MathNode::Log(Box::new(args.pop()?), None)),
        "log" if args.len() == 2 => { let b = args.pop()?; let a = args.pop()?; Some(MathNode::Log(Box::new(a), Some(Box::new(b)))) }
        "sin" if args.len() == 1 => Some(MathNode::Sin(Box::new(args.pop()?))),
        "cos" if args.len() == 1 => Some(MathNode::Cos(Box::new(args.pop()?))),
        "tan" if args.len() == 1 => Some(MathNode::Tan(Box::new(args.pop()?))),
        "asin" if args.len() == 1 => Some(MathNode::Asin(Box::new(args.pop()?))),
        "acos" if args.len() == 1 => Some(MathNode::Acos(Box::new(args.pop()?))),
        "atan" if args.len() == 1 => Some(MathNode::Atan(Box::new(args.pop()?))),
        "sqrt" if args.len() == 1 => Some(MathNode::Sqrt(Box::new(args.pop()?))),
        "exp" if args.len() == 1 => Some(MathNode::Exp(Box::new(args.pop()?))),
        "abs" if args.len() == 1 => Some(MathNode::Abs(Box::new(args.pop()?))),
        "sign" if args.len() == 1 => Some(MathNode::Sign(Box::new(args.pop()?))),
        _ => None,
    }
}

#[rustfmt::skip]
fn parse_sum<'i, 't>(p: &mut Parser<'i, 't>) -> Option<MathNode> {
    let mut terms = vec![parse_product(p)?];
    loop {
        let st = p.state();
        let mut saw_ws = false;
        let op = loop {
            match p.next_including_whitespace() {
                Ok(Token::WhiteSpace(_)) => saw_ws = true,
                Ok(Token::Delim('+')) if saw_ws => break Some(true),
                Ok(Token::Delim('-')) if saw_ws => break Some(false),
                _ => { p.reset(&st); break None; }
            }
        };
        let Some(is_add) = op else { break };
        if !matches!(p.next_including_whitespace(), Ok(Token::WhiteSpace(_))) { p.reset(&st); return None; }
        let rhs = parse_product(p)?;
        terms.push(if is_add { rhs } else { MathNode::Negate(Box::new(rhs)) });
    }
    if terms.len() == 1 { terms.pop() } else { Some(MathNode::Sum(terms)) }
}

#[rustfmt::skip]
fn parse_product<'i, 't>(p: &mut Parser<'i, 't>) -> Option<MathNode> {
    let mut factors = vec![parse_value(p)?];
    loop {
        let st = p.state();
        match p.next() {
            Ok(Token::Delim('*')) => factors.push(parse_value(p)?),
            Ok(Token::Delim('/')) => factors.push(MathNode::Invert(Box::new(parse_value(p)?))),
            _ => { p.reset(&st); break; }
        }
    }
    if factors.len() == 1 { factors.pop() } else { Some(MathNode::Product(factors)) }
}

#[rustfmt::skip]
fn parse_value<'i, 't>(p: &mut Parser<'i, 't>) -> Option<MathNode> {
    let start = p.position();
    match p.next().ok()?.clone() {
        Token::Number { value, .. } => Some(MathNode::Number(exact_f64(p.slice_from(start), 0, f64::from(value)))),
        Token::Percentage { unit_value, .. } => Some(MathNode::Percent(exact_f64(p.slice_from(start), 1, f64::from(unit_value) * 100.0))),
        Token::Dimension { value, unit, .. } => {
            let v = exact_f64(p.slice_from(start), unit.len(), f64::from(value));
            let (scale, mu) = MathUnit::parse_unit(unit.as_ref())?;
            Some(MathNode::Dimension(v * scale, mu))
        }
        Token::Ident(id) => match id.to_ascii_lowercase().as_str() {
            "pi" => Some(MathNode::Number(std::f64::consts::PI)), "e" => Some(MathNode::Number(std::f64::consts::E)),
            "infinity" => Some(MathNode::Number(f64::INFINITY)), "-infinity" => Some(MathNode::Number(f64::NEG_INFINITY)),
            "nan" => Some(MathNode::Number(f64::NAN)), _ => None,
        },
        Token::ParenthesisBlock => p.parse_nested_block(|n| {
            let inner = parse_sum(n).ok_or_else(|| n.new_custom_error::<_, ()>(()))?;
            if !n.is_exhausted() { return Err(n.new_custom_error(())); }
            Ok(inner)
        }).ok(),
        Token::Function(ref fname) if is_css_math_fn(fname.as_ref()) => {
            let lower = fname.to_ascii_lowercase();
            p.parse_nested_block(|n| parse_math_fn(&lower, n).ok_or_else(|| n.new_custom_error::<_, ()>(()))).ok()
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum ScaleSpec {
    #[default]
    None,
    Values(Vec<MathNode>),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum RotateSpec {
    #[default]
    None,
    Angle {
        axis: Option<[MathNode; 3]>,
        angle: MathNode,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum TranslateSpec {
    #[default]
    None,
    Values {
        x: MathNode,
        y: Option<MathNode>,
        z: Option<MathNode>,
    },
}

#[rustfmt::skip]
impl ScaleSpec {
    #[must_use]
    pub fn parse(input: &str) -> Option<(Self, String)> {
        let mut pi = ParserInput::new(input);
        let mut p = Parser::new(&mut pi);
        if p.try_parse(|p| p.expect_ident_matching("none")).is_ok() { return p.is_exhausted().then_some((Self::None, "none".to_string())); }
        let (mut vals, mut texts) = (Vec::new(), Vec::new());
        while !p.is_exhausted() && vals.len() < 3 {
            let pm = parse_math_value_from_parser(&mut p, ValueKind::NumberOrPercentage)?;
            texts.push(pm.node.serialize_specified(pm.was_math_fn));
            vals.push(pm.node);
        }
        (!vals.is_empty() && p.is_exhausted()).then(|| (Self::Values(vals), texts.join(" ")))
    }

    #[must_use]
    pub fn serialize_computed(&self, ctx: &EvalContext) -> String {
        let Self::Values(vals) = self else { return "none".to_string(); };
        let s: Vec<String> = vals.iter().map(|n| n.serialize_computed(ctx, Some((1.0, true)), ValueKind::Number, None, None)).collect();
        match s.as_slice() {
            [x] => x.clone(), [x, y] if x == y => x.clone(), [x, y] => format!("{x} {y}"),
            [x, y, z] if z == "1" => if x == y { x.clone() } else { format!("{x} {y}") },
            [x, y, z] => format!("{x} {y} {z}"), _ => "none".to_string(),
        }
    }
}

#[rustfmt::skip]
impl RotateSpec {
    #[must_use]
    pub fn parse(input: &str) -> Option<(Self, String)> {
        let mut pi = ParserInput::new(input);
        let mut p = Parser::new(&mut pi);
        if p.try_parse(|p| p.expect_ident_matching("none")).is_ok() {
            return p.is_exhausted().then_some((Self::None, "none".to_string()));
        }
        let (mut axis_kw, mut axis_nums, mut angle) = (None::<&'static str>, None::<([MathNode; 3], [String; 3])>, None::<(MathNode, String)>);
        for _ in 0..2 {
            if p.is_exhausted() { break; }
            if angle.is_none() {
                if let Some(pm) = parse_math_value_from_parser(&mut p, ValueKind::Angle) {
                    angle = Some((pm.node.clone(), pm.node.serialize_specified(pm.was_math_fn)));
                    continue;
                }
            }
            if axis_kw.is_none() && axis_nums.is_none() {
                if let Ok(kw) = p.try_parse(|p| -> Result<&'static str, cssparser::ParseError<'_, ()>> {
                    match p.expect_ident()?.to_ascii_lowercase().as_str() { "x" => Ok("x"), "y" => Ok("y"), "z" => Ok("z"), _ => Err(p.new_custom_error(())) }
                }) { axis_kw = Some(kw); continue; }
                let st = p.state();
                if let (Some(a), Some(b), Some(c)) = (
                    parse_math_value_from_parser(&mut p, ValueKind::Number),
                    parse_math_value_from_parser(&mut p, ValueKind::Number),
                    parse_math_value_from_parser(&mut p, ValueKind::Number),
                ) {
                    let ts = [a.node.serialize_specified(a.was_math_fn), b.node.serialize_specified(b.was_math_fn), c.node.serialize_specified(c.was_math_fn)];
                    axis_nums = Some(([a.node, b.node, c.node], ts));
                    continue;
                }
                p.reset(&st);
            }
            return None;
        }
        let (ang, ang_s) = angle?;
        if !p.is_exhausted() { return None; }
        let (axis, spec_str) = if let Some(kw) = axis_kw {
            let ax = match kw {
                "x" => [MathNode::Number(1.0), MathNode::Number(0.0), MathNode::Number(0.0)],
                "y" => [MathNode::Number(0.0), MathNode::Number(1.0), MathNode::Number(0.0)],
                _ => [MathNode::Number(0.0), MathNode::Number(0.0), MathNode::Number(1.0)],
            };
            (Some(ax), format!("{kw} {ang_s}"))
        } else if let Some((ax, ts)) = axis_nums {
            (Some(ax), format!("{} {} {} {ang_s}", ts[0], ts[1], ts[2]))
        } else { (None, ang_s) };
        Some((Self::Angle { axis, angle: ang }, spec_str))
    }

    #[must_use]
    pub fn serialize_computed(&self, ctx: &EvalContext) -> String {
        let Self::Angle { axis, angle } = self else { return "none".to_string(); };
        let deg_val = angle.eval_scalar(ctx, None);
        let deg_s = if !deg_val.is_finite() { "0deg".to_string() } else { format!("{}deg", format_math_f64(deg_val)) };
        match axis {
            None => deg_s,
            Some([x, y, z]) => {
                let (xv, yv, zv) = (x.eval_scalar(ctx, None), y.eval_scalar(ctx, None), z.eval_scalar(ctx, None));
                if xv == 0.0 && yv == 0.0 && zv != 0.0 { deg_s }
                else { format!("{} {} {} {deg_s}", format_math_f64(xv), format_math_f64(yv), format_math_f64(zv)) }
            }
        }
    }
}

#[rustfmt::skip]
impl TranslateSpec {
    #[must_use]
    pub fn parse(input: &str) -> Option<(Self, String)> {
        let mut pi = ParserInput::new(input);
        let mut p = Parser::new(&mut pi);
        if p.try_parse(|p| p.expect_ident_matching("none")).is_ok() {
            return p.is_exhausted().then_some((Self::None, "none".to_string()));
        }
        let x = parse_math_value_from_parser(&mut p, ValueKind::LengthPercentage)?;
        let mut texts = vec![x.node.serialize_specified(x.was_math_fn)];
        let y = if !p.is_exhausted() { let py = parse_math_value_from_parser(&mut p, ValueKind::LengthPercentage)?; texts.push(py.node.serialize_specified(py.was_math_fn)); Some(py.node) } else { None };
        let z = if !p.is_exhausted() { let pz = parse_math_value_from_parser(&mut p, ValueKind::Length)?; texts.push(pz.node.serialize_specified(pz.was_math_fn)); Some(pz.node) } else { None };
        p.is_exhausted().then_some((Self::Values { x: x.node, y, z }, texts.join(" ")))
    }

    #[must_use]
    pub fn serialize_computed(&self, ctx: &EvalContext) -> String {
        let Self::Values { x, y, z } = self else { return "none".to_string(); };
        let sx = x.serialize_computed(ctx, None, ValueKind::LengthPercentage, None, None);
        let sy = y.as_ref().map(|n| n.serialize_computed(ctx, None, ValueKind::LengthPercentage, None, None));
        let sz = z.as_ref().map(|n| n.serialize_computed(ctx, None, ValueKind::Length, None, None));
        match (sy, sz) {
            (None, _) => sx,
            (Some(y_s), None) => if y_s == "0px" { sx } else { format!("{sx} {y_s}") },
            (Some(y_s), Some(z_s)) if z_s == "0px" => if y_s == "0px" { sx } else { format!("{sx} {y_s}") },
            (Some(y_s), Some(z_s)) => format!("{sx} {y_s} {z_s}"),
        }
    }
}

/// Parses and canonically serializes CSS `transform` with full CSS Values L4 math inside arguments.
#[must_use]
#[rustfmt::skip]
pub fn parse_and_serialize_transform(input: &str, eval: Option<&EvalContext>) -> Option<String> {
    let mut pi = ParserInput::new(input);
    let mut p = Parser::new(&mut pi);
    if p.try_parse(|p| p.expect_ident_matching("none")).is_ok() { return p.is_exhausted().then_some("none".to_string()); }
    let mut out = Vec::new();
    while !p.is_exhausted() {
        let Token::Function(name) = p.next().ok()?.clone() else { return None; };
        let lower = name.to_ascii_lowercase();
        out.push(p.parse_nested_block(|n| parse_transform_fn_args(&lower, n, eval).ok_or_else(|| n.new_custom_error::<_, ()>(() ))).ok()?);
    }
    (!out.is_empty()).then(|| out.join(" "))
}

#[rustfmt::skip]
fn parse_transform_fn_args<'i, 't>(name: &str, p: &mut Parser<'i, 't>, eval: Option<&EvalContext>) -> Option<String> {
    let fmt_arg = |pm: ParsedMath, kind: ValueKind| match eval {
        None => pm.node.serialize_specified(pm.was_math_fn),
        Some(ctx) if kind == ValueKind::Angle => {
            let v = pm.node.eval_scalar(ctx, None);
            if !v.is_finite() { "0deg".to_string() } else { format!("{}deg", format_math_f64(v)) }
        }
        Some(ctx) => pm.node.serialize_computed(ctx, None, kind, None, None),
    };
    let parse_list = |p: &mut Parser<'i, 't>, kinds: &[ValueKind], min_n: usize| -> Option<Vec<String>> {
        let mut res = Vec::new();
        for (i, &k) in kinds.iter().enumerate() {
            if i > 0 { if p.is_exhausted() && i >= min_n { break; } p.expect_comma().ok()?; }
            res.push(fmt_arg(parse_math_value_from_parser(p, k)?, k));
        }
        p.is_exhausted().then_some(res)
    };
    use ValueKind::{Angle, Length, LengthPercentage, Number, NumberOrPercentage};
    let (canon, args) = match name {
        "rotate" | "rotatex" | "rotatey" | "rotatez" | "skewx" | "skewy" => (
            match name { "rotatex" => "rotateX", "rotatey" => "rotateY", "rotatez" => "rotateZ", "skewx" => "skewX", "skewy" => "skewY", _ => "rotate" },
            parse_list(p, &[Angle], 1)?,
        ),
        "skew" => ("skew", parse_list(p, &[Angle, Angle], 1)?),
        "rotate3d" => ("rotate3d", parse_list(p, &[Number, Number, Number, Angle], 4)?),
        "translate" => ("translate", parse_list(p, &[LengthPercentage, LengthPercentage], 1)?),
        "translatex" => ("translateX", parse_list(p, &[LengthPercentage], 1)?),
        "translatey" => ("translateY", parse_list(p, &[LengthPercentage], 1)?),
        "translatez" => ("translateZ", parse_list(p, &[Length], 1)?),
        "translate3d" => ("translate3d", parse_list(p, &[LengthPercentage, LengthPercentage, Length], 3)?),
        "scale" => ("scale", parse_list(p, &[NumberOrPercentage, NumberOrPercentage], 1)?),
        "scalex" => ("scaleX", parse_list(p, &[NumberOrPercentage], 1)?),
        "scaley" => ("scaleY", parse_list(p, &[NumberOrPercentage], 1)?),
        "scalez" => ("scaleZ", parse_list(p, &[NumberOrPercentage], 1)?),
        "scale3d" => ("scale3d", parse_list(p, &[NumberOrPercentage; 3], 3)?),
        "perspective" => if p.try_parse(|p| p.expect_ident_matching("none")).is_ok() && p.is_exhausted() { ("perspective", vec!["none".to_string()]) } else { ("perspective", parse_list(p, &[Length], 1)?) },
        "matrix" => ("matrix", parse_list(p, &[Number; 6], 6)?),
        "matrix3d" => ("matrix3d", parse_list(p, &[Number; 16], 16)?),
        _ => return None,
    };
    Some(format!("{canon}({})", args.join(", ")))
}

/// Evaluates CSS `transform` into a 2D `matrix(a, b, c, d, tx, ty)` or 3D `matrix3d(...)`.
#[must_use]
#[rustfmt::skip]
pub fn compute_transform_matrix_css(input: &str, ctx: &EvalContext) -> Option<String> {
    let mut pi = ParserInput::new(input);
    let mut p = Parser::new(&mut pi);
    if p.try_parse(|p| p.expect_ident_matching("none")).is_ok() { return p.is_exhausted().then_some("none".to_string()); }
    if let Some(m3d) = try_compute_rotate3d_matrix3d(input, ctx) { return Some(m3d); }
    let mut m = [1.0_f64, 0.0, 0.0, 1.0, 0.0, 0.0];
    let mul2d = |a: [f64; 6], b: [f64; 6]| [
        a[0]*b[0] + a[2]*b[1], a[1]*b[0] + a[3]*b[1], a[0]*b[2] + a[2]*b[3],
        a[1]*b[2] + a[3]*b[3], a[0]*b[4] + a[2]*b[5] + a[4], a[1]*b[4] + a[3]*b[5] + a[5],
    ];
    while !p.is_exhausted() {
        let Token::Function(name) = p.next().ok()?.clone() else { return None; };
        let lower = name.to_ascii_lowercase();
        let step: [f64; 6] = p.parse_nested_block(|n| -> Result<[f64; 6], cssparser::ParseError<'_, ()>> {
            match lower.as_str() {
                "rotate" | "rotatez" => {
                    let Some(a) = parse_math_value_from_parser(n, ValueKind::Angle) else { return Err(n.new_custom_error(())); };
                    if !n.is_exhausted() { return Err(n.new_custom_error(())); }
                    let d = a.node.eval_scalar(ctx, None);
                    let r = if d.is_finite() { d.to_radians() } else { 0.0 };
                    Ok([r.cos(), r.sin(), -r.sin(), r.cos(), 0.0, 0.0])
                }
                "scale" | "scalex" | "scaley" => {
                    let Some(a) = parse_math_value_from_parser(n, ValueKind::NumberOrPercentage) else { return Err(n.new_custom_error(())); };
                    let x = a.node.eval_scalar(ctx, Some((1.0, true)));
                    let y = if lower == "scale" && n.try_parse(Parser::expect_comma).is_ok() {
                        let Some(b) = parse_math_value_from_parser(n, ValueKind::NumberOrPercentage) else { return Err(n.new_custom_error(())); };
                        b.node.eval_scalar(ctx, Some((1.0, true)))
                    } else if lower == "scalex" { 1.0 } else { x };
                    if !n.is_exhausted() { return Err(n.new_custom_error(())); }
                    let (sx, sy) = if lower == "scaley" { (1.0, x) } else { (x, y) };
                    Ok([sx, 0.0, 0.0, sy, 0.0, 0.0])
                }
                "translate" | "translatex" | "translatey" => {
                    let Some(a) = parse_math_value_from_parser(n, ValueKind::LengthPercentage) else { return Err(n.new_custom_error(())); };
                    let tx = a.node.eval_scalar(ctx, Some((0.0, false)));
                    let ty = if lower == "translate" && n.try_parse(Parser::expect_comma).is_ok() {
                        let Some(b) = parse_math_value_from_parser(n, ValueKind::LengthPercentage) else { return Err(n.new_custom_error(())); };
                        b.node.eval_scalar(ctx, Some((0.0, false)))
                    } else { 0.0 };
                    if !n.is_exhausted() { return Err(n.new_custom_error(())); }
                    let (fx, fy) = if lower == "translatey" { (0.0, tx) } else { (tx, ty) };
                    Ok([1.0, 0.0, 0.0, 1.0, fx, fy])
                }
                "matrix" => {
                    let mut v = [0.0; 6];
                    for (i, slot) in v.iter_mut().enumerate() {
                        if i > 0 { n.expect_comma()?; }
                        let Some(pm) = parse_math_value_from_parser(n, ValueKind::Number) else { return Err(n.new_custom_error(())); };
                        *slot = pm.node.eval_scalar(ctx, None);
                    }
                    if !n.is_exhausted() { return Err(n.new_custom_error(())); }
                    Ok(v)
                }
                _ => Err(n.new_custom_error(())),
            }
        }).ok()?;
        m = mul2d(m, step);
    }
    let clean = |v: f64| if !v.is_finite() || v.abs() < 1e-6 { 0.0 } else { v };
    Some(format!("matrix({}, {}, {}, {}, {}, {})", format_math_f64(clean(m[0])), format_math_f64(clean(m[1])), format_math_f64(clean(m[2])), format_math_f64(clean(m[3])), format_math_f64(clean(m[4])), format_math_f64(clean(m[5]))))
}

#[rustfmt::skip]
fn try_compute_rotate3d_matrix3d(input: &str, ctx: &EvalContext) -> Option<String> {
    let mut pi = ParserInput::new(input);
    let mut p = Parser::new(&mut pi);
    let Token::Function(name) = p.next().ok()?.clone() else { return None; };
    if !name.eq_ignore_ascii_case("rotate3d") { return None; }
    let (x, y, z, deg) = p.parse_nested_block(|n| -> Result<(f64, f64, f64, f64), cssparser::ParseError<'_, ()>> {
        let Some(ax) = parse_math_value_from_parser(n, ValueKind::Number) else { return Err(n.new_custom_error(())); };
        n.expect_comma()?;
        let Some(ay) = parse_math_value_from_parser(n, ValueKind::Number) else { return Err(n.new_custom_error(())); };
        n.expect_comma()?;
        let Some(az) = parse_math_value_from_parser(n, ValueKind::Number) else { return Err(n.new_custom_error(())); };
        n.expect_comma()?;
        let Some(ang) = parse_math_value_from_parser(n, ValueKind::Angle) else { return Err(n.new_custom_error(())); };
        if !n.is_exhausted() { return Err(n.new_custom_error(())); }
        Ok((ax.node.eval_scalar(ctx, None), ay.node.eval_scalar(ctx, None), az.node.eval_scalar(ctx, None), ang.node.eval_scalar(ctx, None)))
    }).ok()?;
    if !p.is_exhausted() { return None; }
    let len = (x * x + y * y + z * z).sqrt();
    if len == 0.0 || !deg.is_finite() {
        return Some("matrix(1, 0, 0, 1, 0, 0)".to_string());
    }
    let (ux, uy, uz) = (x / len, y / len, z / len);
    let rad = deg.to_radians();
    let (s, c) = (rad.sin(), rad.cos());
    let t = 1.0 - c;
    let m = [
        t * ux * ux + c, t * ux * uy + uz * s, t * ux * uz - uy * s, 0.0,
        t * ux * uy - uz * s, t * uy * uy + c, t * uy * uz + ux * s, 0.0,
        t * ux * uz + uy * s, t * uy * uz - ux * s, t * uz * uz + c, 0.0,
        0.0, 0.0, 0.0, 1.0,
    ];
    let clean = |v: f64| if !v.is_finite() || v.abs() < 1e-6 { 0.0 } else { v };
    let parts: Vec<String> = m.iter().map(|&v| format_math_f64(clean(v))).collect();
    Some(format!("matrix3d({})", parts.join(", ")))
}

#[must_use]
#[rustfmt::skip]
pub fn parse_background_position(input: &str) -> Option<(LengthPercentage, LengthPercentage, String)> {
    let mut pi = ParserInput::new(input);
    let mut p = Parser::new(&mut pi);
    let parse_part = |p: &mut Parser<'_, '_>, is_y: bool| -> Option<(LengthPercentage, String)> {
        let st = p.state();
        if let Ok(id) = p.try_parse(|p| p.expect_ident().map(|s| s.to_ascii_lowercase())) {
            let (pct, kw) = match (id.as_str(), is_y) {
                ("left", false) | ("top", true) => (0.0, 0.0), ("center", _) => (0.5, 50.0), ("right", false) | ("bottom", true) => (1.0, 100.0),
                _ => { p.reset(&st); return None; }
            };
            return Some((LengthPercentage::Percent(pct), format!("{}%", format_math_f64(kw))));
        }
        let pm = parse_math_value_from_parser(p, ValueKind::LengthPercentage)?;
        let spec = pm.node.serialize_specified(pm.was_math_fn);
        Some((LengthPercentage::Calc(Box::new(crate::values::CalcExpr::Math(Box::new(pm.node)))), spec))
    };
    let (x, xs) = parse_part(&mut p, false)?;
    if p.is_exhausted() { return Some((x, LengthPercentage::Percent(0.5), format!("{xs} 50%"))); }
    let (y, ys) = parse_part(&mut p, true)?;
    p.is_exhausted().then_some((x, y, format!("{xs} {ys}")))
}

#[must_use]
#[rustfmt::skip]
pub fn parse_image_resolution(input: &str) -> Option<(MathNode, String)> {
    let mut pi = ParserInput::new(input);
    let mut p = Parser::new(&mut pi);
    let pm = parse_math_value_from_parser(&mut p, ValueKind::Resolution)?;
    if !p.is_exhausted() { return None; }
    if !pm.was_math_fn { if let MathNode::Dimension(v, _) = pm.node { if v <= 0.0 { return None; } } }
    let spec = pm.node.serialize_specified(pm.was_math_fn);
    Some((pm.node, spec))
}

#[must_use]
#[rustfmt::skip]
pub fn parse_image_set_resolution_specified(input: &str) -> Option<String> {
    let mut pi = ParserInput::new(input);
    let mut p = Parser::new(&mut pi);
    let Token::Function(name) = p.next().ok()?.clone() else { return None; };
    if !name.eq_ignore_ascii_case("image-set") { return None; }
    let inner = p.parse_nested_block(|nested| -> Result<String, cssparser::ParseError<'_, ()>> {
        let mut items = Vec::new();
        loop {
            let url_s = match nested.next()?.clone() {
                Token::UnquotedUrl(u) => format!("url(\"{}\")", u.as_ref()),
                Token::Function(fn_n) if fn_n.eq_ignore_ascii_case("url") => format!("url(\"{}\")", nested.parse_nested_block(|u| Ok(u.expect_string()?.as_ref().to_string()))?),
                Token::QuotedString(s) => format!("url(\"{}\")", s.as_ref()),
                _ => return Err(nested.new_custom_error(())),
            };
            let Some(res) = parse_math_value_from_parser(nested, ValueKind::Resolution) else { return Err(nested.new_custom_error(())); };
            items.push(format!("{url_s} {}", res.node.serialize_specified(res.was_math_fn)));
            if nested.try_parse(Parser::expect_comma).is_err() { break; }
        }
        if !nested.is_exhausted() { return Err(nested.new_custom_error(())); }
        Ok(format!("image-set({})", items.join(", ")))
    }).ok()?;
    p.is_exhausted().then_some(inner)
}

#[rustfmt::skip]
impl MathNode {
    #[must_use]
    pub fn contains_sibling_fn(&self) -> bool {
        match self {
            Self::SiblingIndex | Self::SiblingCount => true,
            Self::Number(_) | Self::Percent(_) | Self::Dimension(_, _) => false,
            Self::Negate(a) | Self::Invert(a) | Self::Sin(a) | Self::Cos(a) | Self::Tan(a) | Self::Asin(a) | Self::Acos(a) | Self::Atan(a) | Self::Sqrt(a) | Self::Exp(a) | Self::Abs(a) | Self::Sign(a) => a.contains_sibling_fn(),
            Self::Mod(a, b) | Self::Rem(a, b) | Self::Atan2(a, b) | Self::Pow(a, b) => a.contains_sibling_fn() || b.contains_sibling_fn(),
            Self::Round(_, a, b) | Self::Log(a, b) => a.contains_sibling_fn() || b.as_deref().is_some_and(Self::contains_sibling_fn),
            Self::Clamp(a, b, c) => a.as_deref().is_some_and(Self::contains_sibling_fn) || b.contains_sibling_fn() || c.as_deref().is_some_and(Self::contains_sibling_fn),
            Self::Progress(_, a, b, c) => a.contains_sibling_fn() || b.contains_sibling_fn() || c.contains_sibling_fn(),
            Self::Sum(v) | Self::Product(v) | Self::Min(v) | Self::Max(v) | Self::Hypot(v) => v.iter().any(Self::contains_sibling_fn),
        }
    }
}

#[must_use]
#[rustfmt::skip]
pub fn has_values4_syntax(raw: &str) -> bool {
    if raw.contains('(') {
        let lower = raw.to_ascii_lowercase();
        for fn_pfx in ["calc(", "min(", "max(", "clamp(", "round(", "mod(", "rem(", "sin(", "cos(", "tan(", "asin(", "acos(", "atan(", "atan2(", "pow(", "sqrt(", "hypot(", "log(", "exp(", "abs(", "sign(", "progress(", "sibling-index(", "sibling-count("] {
            if lower.contains(fn_pfx) { return true; }
        }
    }
    let bytes = raw.as_bytes();
    for (idx, &b) in bytes.iter().enumerate() {
        if b.is_ascii_digit() {
            let mut j = idx + 1;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b'.') { j += 1; }
            let u_start = j;
            while j < bytes.len() && (bytes[j].is_ascii_alphabetic() || bytes[j] == b'-') { j += 1; }
            if j > u_start && matches!(raw[u_start..j].to_ascii_lowercase().as_str(), "cap" | "rcap" | "rch" | "rex" | "ic" | "ric" | "lh" | "rlh" | "q" | "vi" | "vb" | "svw" | "svh" | "svi" | "svb" | "svmin" | "svmax" | "lvw" | "lvh" | "lvi" | "lvb" | "lvmin" | "lvmax" | "dvw" | "dvh" | "dvi" | "dvb" | "dvmin" | "dvmax" | "cqmin" | "cqmax" | "dppx" | "dpi" | "dpcm" | "x") {
                return true;
            }
        }
    }
    false
}

#[must_use]
#[rustfmt::skip]
pub fn parse_grid_tracks_math(input: &str) -> Option<(Vec<crate::values::GridTrackSize>, String)> {
    use crate::values::{CalcExpr, GridTrackSize};
    let mut pi = ParserInput::new(input);
    let mut p = Parser::new(&mut pi);
    if p.try_parse(|p| p.expect_ident_matching("none")).is_ok() { return p.is_exhausted().then_some((Vec::new(), "none".to_string())); }
    let (mut tracks, mut texts) = (Vec::new(), Vec::new());
    while !p.is_exhausted() {
        if let Ok(kw) = p.try_parse(|p| -> Result<&'static str, cssparser::ParseError<'_, ()>> {
            match p.expect_ident()?.to_ascii_lowercase().as_str() { "auto" => Ok("auto"), "min-content" => Ok("min-content"), "max-content" => Ok("max-content"), _ => Err(p.new_custom_error(())) }
        }) {
            tracks.push(match kw { "min-content" => GridTrackSize::MinContent, "max-content" => GridTrackSize::MaxContent, _ => GridTrackSize::Auto });
            texts.push(kw.to_string());
            continue;
        }
        if let Some(pm) = parse_math_value_from_parser(&mut p, ValueKind::Flex) {
            if !pm.was_math_fn { if let MathNode::Dimension(v, _) = pm.node { if v < 0.0 { return None; } } }
            texts.push(pm.node.serialize_specified(pm.was_math_fn));
            tracks.push(GridTrackSize::FrMath(Box::new(pm.node)));
            continue;
        }
        if let Some(pm) = parse_math_value_from_parser(&mut p, ValueKind::LengthPercentage) {
            if !pm.was_math_fn { if let MathNode::Dimension(v, _) | MathNode::Percent(v) = pm.node { if v < 0.0 { return None; } } }
            texts.push(pm.node.serialize_specified(pm.was_math_fn));
            tracks.push(GridTrackSize::Length(LengthPercentage::Calc(Box::new(CalcExpr::Math(Box::new(pm.node))))));
            continue;
        }
        return None;
    }
    (!tracks.is_empty()).then(|| (tracks, texts.join(" ")))
}

#[must_use]
#[rustfmt::skip]
pub fn format_grid_tracks_computed(tracks: &[crate::values::GridTrackSize], ctx: &EvalContext) -> String {
    use crate::values::{CalcExpr, GridTrackSize};
    if tracks.is_empty() { return "none".to_string(); }
    tracks.iter().map(|t| match t {
        GridTrackSize::Auto => "auto".to_string(),
        GridTrackSize::MinContent => "min-content".to_string(),
        GridTrackSize::MaxContent => "max-content".to_string(),
        GridTrackSize::Fr(f) => format!("{}fr", format_math_f64(f64::from(*f))),
        GridTrackSize::FrMath(n) => n.serialize_computed(ctx, None, ValueKind::Flex, Some(0.0), None),
        GridTrackSize::Length(lp) => match lp {
            LengthPercentage::Calc(c) => match c.as_ref() {
                CalcExpr::Math(n) => n.serialize_computed(ctx, None, ValueKind::LengthPercentage, Some(0.0), None),
                _ => format!("{}px", format_math_f64(f64::from(lp.resolve_px(0.0, ctx).max(0.0)))),
            },
            LengthPercentage::Percent(p) => format!("{}%", format_math_f64(f64::from(*p * 100.0))),
            _ => format!("{}px", format_math_f64(f64::from(lp.resolve_px(0.0, ctx).max(0.0)))),
        },
        GridTrackSize::MinMax(a, b) => format!("minmax({}, {})", format_grid_tracks_computed(&[(**a).clone()], ctx), format_grid_tracks_computed(&[(**b).clone()], ctx)),
    }).collect::<Vec<_>>().join(" ")
}
