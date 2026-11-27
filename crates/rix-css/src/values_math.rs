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

    pub(crate) fn single_base_dim(self) -> Option<BaseDim> {
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

/// Parsed CSS Values L5 `random([<options>,]? <min>, <max> [, <step>]?)` AST node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RandomNode {
    pub dashed_ident: Option<String>,
    pub element_scoped: bool,
    pub prop_scoped: bool,
    pub prop_index_scoped: bool,
    pub ua_ident: Option<String>,
    pub fixed: Option<(Box<MathNode>, bool)>,
    pub min: Box<MathNode>,
    pub max: Box<MathNode>,
    pub step: Option<Box<MathNode>>,
}

/// Weighted item inside a CSS Values L5 `calc-mix(...)` expression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalcMixItem {
    pub value: MathNode,
    pub weight: Option<(MathNode, bool)>,
}

/// W3C CSS Values L4/L5 canonical calculation tree node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MathNode {
    Number(f64),
    Percent(f64),
    Dimension(f64, MathUnit),
    SizeKeyword,
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
    CalcMix(Vec<CalcMixItem>),
    Random(Box<RandomNode>),
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
            Self::SizeKeyword => Some(CalcType::base(BaseDim::Length)),
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
            Self::CalcMix(items) => {
                let mut acc = items.first()?.value.calc_type()?;
                for it in items {
                    if let Some((w, _)) = &it.weight {
                        if !w.calc_type()?.is_pure_percent() { return None; }
                    }
                    acc = add_ok(acc, it.value.calc_type()?)?;
                }
                Some(acc)
            }
            Self::Random(r) => {
                if let Some((f, _)) = &r.fixed {
                    f.calc_type()?.as_hinted_number()?;
                }
                let mut acc = add_ok(r.min.calc_type()?, r.max.calc_type()?)?;
                if let Some(st) = &r.step { acc = add_ok(acc, st.calc_type()?)?; }
                Some(acc)
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
            Self::SizeKeyword => Self::SizeKeyword,
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
            Self::Pow(a, b) => simplify_binary_num(s(a), s(b), css_pow_f64, |x, y| Self::Pow(Box::new(x), Box::new(y))),
            Self::Sqrt(a) => simplify_unary_num(s(a), f64::sqrt, |x| Self::Sqrt(Box::new(x))),
            Self::Exp(a) => simplify_unary_num(s(a), f64::exp, |x| Self::Exp(Box::new(x))),
            Self::Log(a, b) => simplify_log(s(a), b.as_deref().map(s)),
            Self::Hypot(items) => simplify_hypot(sv(items)),
            Self::Abs(a) => simplify_abs(s(a)), Self::Sign(a) => simplify_sign(s(a)),
            Self::Progress(nc, v, st, en) => simplify_progress(*nc, s(v), s(st), s(en)),
            Self::CalcMix(items) => simplify_calc_mix(items, ctx, pct_basis),
            Self::Random(r) => simplify_random(r, ctx, pct_basis),
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
        let mut simp = self.simplify(Some(ctx), pct_basis);
        if pct_basis.is_none() && matches!(target, ValueKind::Percentage | ValueKind::NumberOrPercentage) {
            simp = resolve_pure_percent_random(simp);
        }
        match simp {
            Self::Number(v) => format_computed_scalar(v, None, target, min_bound, max_bound),
            Self::Percent(p) => format_computed_scalar(p, Some("%"), target, min_bound, max_bound),
            Self::Dimension(v, u) => format_computed_scalar(v, Some(u.as_str()), target, min_bound, max_bound),
            other => serialize_top_level_calc(&other, false),
        }
    }
}

#[rustfmt::skip]
fn resolve_pure_percent_random(node: MathNode) -> MathNode {
    if let MathNode::Random(r) = &node {
        if let (Some(MathNode::Number(rv)), MathNode::Percent(lo), MathNode::Percent(hi)) =
            (r.fixed.as_ref().map(|(b, _)| b.as_ref()), r.min.as_ref(), r.max.as_ref())
        {
            let st_v = match r.step.as_deref() {
                None => Some(None),
                Some(MathNode::Percent(s)) => Some(Some(*s)),
                _ => None,
            };
            if let Some(st) = st_v {
                let out = if rv.is_nan() || lo.is_nan() || hi.is_nan() || st.is_some_and(f64::is_nan) {
                    f64::NAN
                } else if hi <= lo {
                    *lo
                } else if !lo.is_finite() || !hi.is_finite() {
                    f64::NAN
                } else if let Some(sv) = st.filter(|&s| s > 0.0) {
                    if sv.is_infinite() || sv > (hi - lo) { *lo } else {
                        let n = ((hi - lo) / sv).floor() + 1.0;
                        let k = (rv * n).floor().min(n - 1.0);
                        lo + k * sv
                    }
                } else {
                    lo + rv * (hi - lo)
                };
                return MathNode::Percent(out);
            }
        }
    }
    node
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
        LengthUnit::Ex => v * fs * 0.5,
        LengthUnit::Ch => v * ctx.ch_px.map_or(fs * 0.5, f64::from),
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
    Size,
}

#[rustfmt::skip]
fn leaf_parts(n: &MathNode) -> Option<(f64, LeafUnit)> {
    match n { MathNode::Number(v) => Some((*v, LeafUnit::Num)), MathNode::Percent(v) => Some((*v, LeafUnit::Pct)), MathNode::Dimension(v, u) => Some((*v, LeafUnit::Dim(*u))), _ => None }
}

#[rustfmt::skip]
fn make_leaf(v: f64, u: LeafUnit) -> MathNode {
    match u {
        LeafUnit::Num => MathNode::Number(v),
        LeafUnit::Pct => MathNode::Percent(v),
        LeafUnit::Dim(mu) => MathNode::Dimension(v, mu),
        LeafUnit::Size => if (v - 1.0).abs() == 0.0 { MathNode::SizeKeyword } else { MathNode::Product(vec![MathNode::Number(v), MathNode::SizeKeyword]) },
    }
}

#[rustfmt::skip]
fn css_pow_f64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() || (a.abs() == 1.0 && b.is_infinite()) {
        f64::NAN
    } else {
        a.powf(b)
    }
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
    if matches!(n, MathNode::SizeKeyword) {
        return Some((1.0, vec![LeafUnit::Size], Vec::new()));
    }
    let MathNode::Product(items) = n else { return None; };
    let (mut scalar, mut nums, mut dens) = (1.0_f64, Vec::new(), Vec::new());
    for it in items {
        match it {
            MathNode::Number(v) => scalar *= *v,
            MathNode::Percent(v) => { scalar *= *v; nums.push(LeafUnit::Pct); }
            MathNode::Dimension(v, u) => { scalar *= *v; nums.push(LeafUnit::Dim(*u)); }
            MathNode::SizeKeyword => { nums.push(LeafUnit::Size); }
            MathNode::Invert(inner) => match inner.as_ref() {
                MathNode::Number(v) => scalar /= *v,
                MathNode::Percent(v) => { scalar /= *v; dens.push(LeafUnit::Pct); }
                MathNode::Dimension(v, u) => { scalar /= *v; dens.push(LeafUnit::Dim(*u)); }
                MathNode::SizeKeyword => { dens.push(LeafUnit::Size); }
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
        (None, Some(h)) => simplify_minmax(true, vec![val, h], is_computed, has_pct_basis),
        (Some(l), None) => simplify_minmax(false, vec![l, val], is_computed, has_pct_basis),
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
            if args.iter().any(|a| leaf_parts(a).unwrap().0.is_nan()) {
                return make_leaf(f64::NAN, u0);
            }
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
    let can_eval = matches!(
        &a,
        MathNode::Number(_)
            | MathNode::Dimension(
                _,
                MathUnit::Length(LengthUnit::Px)
                    | MathUnit::Deg
                    | MathUnit::Sec
                    | MathUnit::Hz
                    | MathUnit::Dppx
                    | MathUnit::Fr
            )
    );
    if can_eval {
        if let Some((v, _)) = leaf_parts(&a) {
            return MathNode::Number(if v.is_nan() { f64::NAN } else if v == 0.0 { v } else if v > 0.0 { 1.0 } else { -1.0 });
        }
    }
    MathNode::Sign(Box::new(a))
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

#[rustfmt::skip]
fn zero_for_node(n: &MathNode) -> MathNode {
    if let Some((_, u)) = leaf_parts(n) { return make_leaf(0.0, u); }
    if n.has_percentage() && !n.calc_type().is_some_and(|t| t.single_base_dim().is_some()) {
        return MathNode::Percent(0.0);
    }
    match n.calc_type() {
        Some(ct) if ct.is_pure_percent() => MathNode::Percent(0.0),
        Some(ct) => match ct.single_base_dim() {
            Some(BaseDim::Length) => MathNode::Dimension(0.0, MathUnit::Length(LengthUnit::Px)),
            Some(BaseDim::Angle) => MathNode::Dimension(0.0, MathUnit::Deg),
            Some(BaseDim::Time) => MathNode::Dimension(0.0, MathUnit::Sec),
            Some(BaseDim::Frequency) => MathNode::Dimension(0.0, MathUnit::Hz),
            Some(BaseDim::Resolution) => MathNode::Dimension(0.0, MathUnit::Dppx),
            Some(BaseDim::Flex) => MathNode::Dimension(0.0, MathUnit::Fr),
            None => MathNode::Number(0.0),
        },
        None => MathNode::Number(0.0),
    }
}

#[rustfmt::skip]
fn simplify_calc_mix(items: &[CalcMixItem], ctx: Option<&EvalContext>, pct_basis: Option<(f64, bool)>) -> MathNode {
    let mut simp: Vec<CalcMixItem> = items.iter().map(|it| CalcMixItem {
        value: it.value.simplify(ctx, pct_basis),
        weight: it.weight.as_ref().map(|(w, wm)| (w.simplify(ctx, None), *wm)),
    }).collect();
    // Check if every weight is statically known (and in [0,100] at specified time; clamped at computed time).
    let mut concrete: Vec<Option<f64>> = Vec::with_capacity(simp.len());
    let mut all_known = true;
    for it in &simp {
        match &it.weight {
            None => concrete.push(None),
            Some((MathNode::Percent(p), wm)) if p.is_finite() => {
                if ctx.is_none() && *wm && !((0.0..=100.0).contains(p)) { all_known = false; break; }
                concrete.push(Some(p.clamp(0.0, 100.0)));
            }
            Some((MathNode::Percent(p), _)) if p.is_nan() => {
                if ctx.is_some() { concrete.push(Some(0.0)); } else { all_known = false; break; }
            }
            Some(_) => { all_known = false; break; }
        }
    }
    if !all_known { return MathNode::CalcMix(simp); }
    let explicit_sum: f64 = concrete.iter().flatten().sum();
    let missing_cnt = concrete.iter().filter(|w| w.is_none()).count();
    let fill = if missing_cnt > 0 { if explicit_sum >= 100.0 { 0.0 } else { (100.0 - explicit_sum) / (missing_cnt as f64) } } else { 0.0 };
    let mut weights: Vec<f64> = concrete.into_iter().map(|w| w.unwrap_or(fill)).collect();
    let total: f64 = weights.iter().sum();
    if total > 100.0 { for w in &mut weights { *w = *w * 100.0 / total; } }
    let first_val = simp.first().map(|it| it.value.clone());
    let mut active: Vec<(MathNode, f64)> = simp.drain(..).zip(weights).filter(|(_, w)| *w > 0.0).map(|(it, w)| (it.value, w)).collect();
    if active.is_empty() {
        return first_val.map_or(MathNode::Number(0.0), |v| zero_for_node(&v));
    }
    let same_unit = active.first().and_then(|(v, _)| leaf_parts(v)).is_some_and(|(_, u0)| {
        active.iter().all(|(v, _)| leaf_parts(v).is_some_and(|(_, u)| u == u0))
    });
    if ctx.is_some() || same_unit {
        let terms: Vec<MathNode> = active.drain(..).map(|(v, w)| MathNode::Product(vec![MathNode::Number(w / 100.0), v])).collect();
        return MathNode::Sum(terms).simplify(ctx, pct_basis);
    }
    MathNode::CalcMix(active.into_iter().map(|(v, w)| CalcMixItem { value: v, weight: Some((MathNode::Percent(w), false)) }).collect())
}

#[rustfmt::skip]
pub(crate) fn hash_random_f64(seed: u64, node_id: u64, dashed: Option<&str>, ua: Option<&str>) -> f64 {
    let mut h = seed ^ 0x9e37_79b9_7f4a_7c15;
    let mix = |mut x: u64| -> u64 {
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^ (x >> 31)
    };
    h = mix(h ^ node_id);
    if let Some(d) = dashed { for &b in d.as_bytes() { h = mix(h ^ u64::from(b)); } h = mix(h ^ 0xff); }
    if let Some(u) = ua { for &b in u.as_bytes() { h = mix(h ^ u64::from(b)); } }
    ((h >> 11) as f64) * (1.0 / ((1u64 << 53) as f64))
}

#[rustfmt::skip]
fn simplify_random(r: &RandomNode, ctx: Option<&EvalContext>, pct_basis: Option<(f64, bool)>) -> MathNode {
    let min_n = r.min.simplify(ctx, pct_basis);
    let max_n = r.max.simplify(ctx, pct_basis);
    let step_n = r.step.as_deref().map(|s| s.simplify(ctx, pct_basis));
    let fixed_n = r.fixed.as_ref().map(|(f, wm)| (f.simplify(ctx, None), *wm));

    let base_r: Option<f64> = if let Some((MathNode::Number(v), _)) = &fixed_n {
        if v.is_nan() { Some(f64::NAN) }
        else if *v < 0.0 { Some(0.0) }
        else if *v >= 1.0 { Some(1.0 - 1e-6) }
        else { Some(*v) }
    } else if fixed_n.is_none() {
        ctx.map(|c| {
            let nid = if r.element_scoped { c.random_node_id } else { 0 };
            hash_random_f64(c.random_doc_seed, nid, r.dashed_ident.as_deref(), r.ua_ident.as_deref())
        })
    } else {
        None
    };

    let has_unresolved_pct = pct_basis.is_none()
        && (min_n.has_percentage() || max_n.has_percentage() || step_n.as_ref().is_some_and(MathNode::has_percentage));

    if let Some(rv) = base_r {
        if !has_unresolved_pct {
            if let (Some((lo, lu)), Some((hi, hu))) = (leaf_parts(&min_n), leaf_parts(&max_n)) {
                let step_parts = step_n.as_ref().map(leaf_parts);
                let units_ok = lu == hu && step_parts.is_none_or(|sp| sp.is_some_and(|(_, su)| su == lu));
                if units_ok {
                    let st_v = step_parts.and_then(|sp| sp.map(|(v, _)| v));
                    let out = if rv.is_nan() || lo.is_nan() || hi.is_nan() || st_v.is_some_and(f64::is_nan) {
                        f64::NAN
                    } else if hi <= lo {
                        lo
                    } else if !lo.is_finite() || !hi.is_finite() {
                        f64::NAN
                    } else if let Some(sv) = st_v.filter(|&s| s > 0.0) {
                        if sv.is_infinite() || sv > (hi - lo) { lo }
                        else {
                            let n = ((hi - lo) / sv).floor() + 1.0;
                            let k = (rv * n).floor().min(n - 1.0);
                            lo + k * sv
                        }
                    } else {
                        lo + rv * (hi - lo)
                    };
                    return make_leaf(out, lu);
                }
            }
        }
        if ctx.is_some() {
            return MathNode::Random(Box::new(RandomNode {
                dashed_ident: None, element_scoped: false, prop_scoped: false, prop_index_scoped: false, ua_ident: None,
                fixed: Some((Box::new(MathNode::Number(rv)), false)),
                min: Box::new(min_n), max: Box::new(max_n), step: step_n.map(Box::new),
            }));
        }
    }
    MathNode::Random(Box::new(RandomNode {
        dashed_ident: r.dashed_ident.clone(),
        element_scoped: r.element_scoped,
        prop_scoped: r.prop_scoped,
        prop_index_scoped: r.prop_index_scoped,
        ua_ident: r.ua_ident.clone(),
        fixed: fixed_n.map(|(n, wm)| (Box::new(n), wm)),
        min: Box::new(min_n),
        max: Box::new(max_n),
        step: step_n.map(Box::new),
    }))
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
        | MathNode::SiblingIndex | MathNode::SiblingCount
        | MathNode::CalcMix(_) | MathNode::Random(_) => serialize_inner(n, false),
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
        MathNode::SizeKeyword => "size".to_string(),
        MathNode::SiblingIndex => "sibling-index()".to_string(),
        MathNode::SiblingCount => "sibling-count()".to_string(),
        MathNode::Negate(a) => wrap(format!("-1 * {}", serialize_inner(a, true))),
        MathNode::Invert(a) => wrap(format!("1 / {}", serialize_inner(a, true))),
        MathNode::Sum(items) => {
            let mut out = String::new();
            for (i, item) in items.iter().enumerate() {
                let need_p = matches!(item, MathNode::Sum(_) | MathNode::Product(_));
                if i == 0 { out.push_str(&serialize_inner(item, need_p)); }
                else {
                    match item {
                        MathNode::Number(v) if v.is_finite() && *v < 0.0 => { out.push_str(" - "); out.push_str(&format_math_f64(-*v)); }
                        MathNode::Percent(v) if v.is_finite() && *v < 0.0 => { out.push_str(" - "); out.push_str(&format!("{}%", format_math_f64(-*v))); }
                        MathNode::Dimension(v, u) if v.is_finite() && *v < 0.0 => { out.push_str(" - "); out.push_str(&format!("{}{}", format_math_f64(-*v), u.as_str())); }
                        MathNode::Negate(inner) => { out.push_str(" - "); out.push_str(&serialize_inner(inner, true)); }
                        other => { out.push_str(" + "); out.push_str(&serialize_inner(other, need_p)); }
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
        MathNode::CalcMix(items) => {
            let parts: Vec<String> = items.iter().map(|it| match &it.weight {
                Some((w, true)) => format!("{} {}", si(&it.value), serialize_top_level_calc(w, true)),
                Some((w, false)) => format!("{} {}", si(&it.value), si(w)),
                None => si(&it.value),
            }).collect();
            format!("calc-mix({})", parts.join(", "))
        }
        MathNode::Random(r) => {
            let mut args = Vec::with_capacity(4);
            if let Some((f, wm)) = &r.fixed {
                let fs = if *wm { serialize_top_level_calc(f, true) } else { si(f) };
                args.push(format!("fixed {fs}"));
            } else {
                let mut opts = Vec::new();
                if let Some(d) = &r.dashed_ident { opts.push(escape_css_ident(d)); }
                if r.element_scoped { opts.push("element-scoped".to_string()); }
                if let Some(u) = &r.ua_ident { opts.push(escape_css_ident(u)); }
                if !opts.is_empty() { args.push(opts.join(" ")); }
            }
            args.push(si(&r.min));
            args.push(si(&r.max));
            if let Some(st) = &r.step { args.push(si(st)); }
            format!("random({})", args.join(", "))
        }
    }
}

fn escape_css_ident(ident: &str) -> String {
    let mut out = String::with_capacity(ident.len());
    for (i, ch) in ident.chars().enumerate() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || !ch.is_ascii() {
            if i == 0 && ch.is_ascii_digit() {
                let _ = std::fmt::Write::write_fmt(&mut out, format_args!("\\{:x} ", ch as u32));
            } else {
                out.push(ch);
            }
        } else {
            out.push('\\');
            out.push(ch);
        }
    }
    out
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
            | "calc-mix" | "random"
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
    p.parse_nested_block(|n| {
        parse_math_fn_opt(name, n, false).ok_or_else(|| n.new_custom_error::<_, ()>(()))
    })
    .ok()
}

fn parse_math_fn<'i, 't>(name: &str, p: &mut Parser<'i, 't>) -> Option<MathNode> {
    parse_math_fn_opt(name, p, false)
}

#[rustfmt::skip]
fn parse_math_fn_opt<'i, 't>(name: &str, p: &mut Parser<'i, 't>, allow_size: bool) -> Option<MathNode> {
    match name {
        "sibling-index" => p.is_exhausted().then_some(MathNode::SiblingIndex),
        "sibling-count" => p.is_exhausted().then_some(MathNode::SiblingCount),
        "calc" => { let e = parse_sum_opt(p, allow_size)?; p.is_exhausted().then_some(e) }
        "clamp" => {
            let lo = if p.try_parse(|p| p.expect_ident_matching("none")).is_ok() { None } else { Some(Box::new(parse_sum_opt(p, allow_size)?)) };
            p.expect_comma().ok()?;
            let mid = Box::new(parse_sum_opt(p, allow_size)?);
            p.expect_comma().ok()?;
            let hi = if p.try_parse(|p| p.expect_ident_matching("none")).is_ok() { None } else { Some(Box::new(parse_sum_opt(p, allow_size)?)) };
            p.is_exhausted().then_some(MathNode::Clamp(lo, mid, hi))
        }
        "progress" => {
            let nc = p.try_parse(|p| p.expect_ident_matching("no-clamp")).is_ok();
            let v = Box::new(parse_sum_opt(p, allow_size)?); p.expect_comma().ok()?;
            let s = Box::new(parse_sum_opt(p, allow_size)?); p.expect_comma().ok()?;
            let e = Box::new(parse_sum_opt(p, allow_size)?);
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
            let a = parse_sum_opt(p, allow_size)?;
            let b = if p.try_parse(Parser::expect_comma).is_ok() { Some(Box::new(parse_sum_opt(p, allow_size)?)) } else { None };
            p.is_exhausted().then_some(MathNode::Round(strat, Box::new(a), b))
        }
        "calc-mix" => parse_calc_mix_fn(p, allow_size),
        "random" => parse_random_fn(p, allow_size),
        _ => {
            let mut args = Vec::new();
            loop { args.push(parse_sum_opt(p, allow_size)?); if p.try_parse(Parser::expect_comma).is_err() { break; } }
            if !p.is_exhausted() { return None; }
            build_math_fn(name, args)
        }
    }
}

#[rustfmt::skip]
fn parse_calc_mix_fn<'i, 't>(p: &mut Parser<'i, 't>, allow_size: bool) -> Option<MathNode> {
    let mut items = Vec::new();
    loop {
        let value = parse_sum_opt(p, allow_size)?;
        let weight = if p.is_exhausted() {
            None
        } else if p.try_parse(Parser::expect_comma).is_ok() {
            items.push(CalcMixItem { value, weight: None });
            continue;
        } else {
            let pm = parse_math_value_from_parser(p, ValueKind::Percentage)?;
            if !pm.was_math_fn {
                if let MathNode::Percent(v) = pm.node {
                    if !((0.0..=100.0).contains(&v)) { return None; }
                }
            }
            Some((pm.node, pm.was_math_fn))
        };
        items.push(CalcMixItem { value, weight });
        if p.try_parse(Parser::expect_comma).is_err() { break; }
    }
    (!items.is_empty() && p.is_exhausted()).then_some(MathNode::CalcMix(items))
}

#[rustfmt::skip]
fn parse_random_fn<'i, 't>(p: &mut Parser<'i, 't>, allow_size: bool) -> Option<MathNode> {
    let st0 = p.state();
    let mut dashed_ident = None::<String>;
    let mut element_scoped = false;
    let mut prop_scoped = false;
    let mut prop_index_scoped = false;
    let mut ua_ident = None::<String>;
    let mut fixed = None::<(Box<MathNode>, bool)>;
    let mut saw_options = false;

    if p.try_parse(|p| -> Result<(), cssparser::ParseError<'i, ()>> {
        p.expect_ident_matching("fixed")?;
        let pm = parse_math_value_from_parser(p, ValueKind::Number).ok_or_else(|| p.new_custom_error(()))?;
        if !pm.was_math_fn {
            if let MathNode::Number(v) = pm.node {
                if !v.is_finite() || v < 0.0 || v >= 1.0 { return Err(p.new_custom_error(())); }
            }
        }
        p.expect_comma()?;
        fixed = Some((Box::new(pm.node), pm.was_math_fn));
        Ok(())
    }).is_ok() {
        saw_options = true;
    } else if p.try_parse(|p| -> Result<(), cssparser::ParseError<'i, ()>> {
        p.expect_ident_matching("auto")?;
        p.expect_comma()?;
        Ok(())
    }).is_ok() {
        element_scoped = true;
        prop_index_scoped = true;
        saw_options = true;
    } else {
        type RandOpts = (Option<String>, bool, bool, bool, Option<String>);
        let opt_res = p.try_parse(|p| -> Result<RandOpts, cssparser::ParseError<'i, ()>> {
            let (mut d, mut es, mut ps, mut pis, mut ua) = (None, false, false, false, None);
            let mut count = 0usize;
            while count < 3 {
                let Ok(id) = p.try_parse(|p| p.expect_ident().map(|s| s.as_ref().to_string())) else { break; };
                let lower = id.to_ascii_lowercase();
                if id.starts_with("--") {
                    if d.is_some() { return Err(p.new_custom_error(())); }
                    d = Some(id);
                } else if lower == "element-scoped" {
                    if es { return Err(p.new_custom_error(())); }
                    es = true;
                } else if lower == "property-scoped" {
                    if ps || pis || ua.is_some() { return Err(p.new_custom_error(())); }
                    ps = true;
                } else if lower == "property-index-scoped" {
                    if ps || pis || ua.is_some() { return Err(p.new_custom_error(())); }
                    pis = true;
                } else if id.starts_with("ua-") && id.len() > 3 {
                    if ps || pis || ua.is_some() { return Err(p.new_custom_error(())); }
                    ua = Some(id);
                } else {
                    return Err(p.new_custom_error(()));
                }
                count += 1;
            }
            if count == 0 { return Err(p.new_custom_error(())); }
            p.expect_comma()?;
            Ok((d, es, ps, pis, ua))
        });
        if let Ok((d, es, ps, pis, ua)) = opt_res {
            dashed_ident = d; element_scoped = es; prop_scoped = ps; prop_index_scoped = pis; ua_ident = ua;
            saw_options = true;
        }
    }
    if !saw_options {
        p.reset(&st0);
        element_scoped = true;
        prop_index_scoped = true;
    }
    let min = Box::new(parse_sum_opt(p, allow_size)?);
    p.expect_comma().ok()?;
    let max = Box::new(parse_sum_opt(p, allow_size)?);
    let step = if p.try_parse(Parser::expect_comma).is_ok() {
        Some(Box::new(parse_sum_opt(p, allow_size)?))
    } else {
        None
    };
    p.is_exhausted().then_some(MathNode::Random(Box::new(RandomNode {
        dashed_ident, element_scoped, prop_scoped, prop_index_scoped, ua_ident, fixed, min, max, step,
    })))
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
fn parse_sum_opt<'i, 't>(p: &mut Parser<'i, 't>, allow_size: bool) -> Option<MathNode> {
    let mut terms = vec![parse_product_opt(p, allow_size)?];
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
        let rhs = parse_product_opt(p, allow_size)?;
        terms.push(if is_add { rhs } else { MathNode::Negate(Box::new(rhs)) });
    }
    if terms.len() == 1 { terms.pop() } else { Some(MathNode::Sum(terms)) }
}

#[rustfmt::skip]
fn parse_product_opt<'i, 't>(p: &mut Parser<'i, 't>, allow_size: bool) -> Option<MathNode> {
    let mut factors = vec![parse_value_opt(p, allow_size)?];
    loop {
        let st = p.state();
        match p.next() {
            Ok(Token::Delim('*')) => factors.push(parse_value_opt(p, allow_size)?),
            Ok(Token::Delim('/')) => factors.push(MathNode::Invert(Box::new(parse_value_opt(p, allow_size)?))),
            _ => { p.reset(&st); break; }
        }
    }
    if factors.len() == 1 { factors.pop() } else { Some(MathNode::Product(factors)) }
}

#[rustfmt::skip]
fn parse_value_opt<'i, 't>(p: &mut Parser<'i, 't>, allow_size: bool) -> Option<MathNode> {
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
            "nan" => Some(MathNode::Number(f64::NAN)),
            "size" if allow_size => Some(MathNode::SizeKeyword),
            _ => None,
        },
        Token::ParenthesisBlock => p.parse_nested_block(|n| {
            let inner = parse_sum_opt(n, allow_size).ok_or_else(|| n.new_custom_error::<_, ()>(()))?;
            if !n.is_exhausted() { return Err(n.new_custom_error(())); }
            Ok(inner)
        }).ok(),
        Token::Function(ref fname) if is_css_math_fn(fname.as_ref()) => {
            let lower = fname.to_ascii_lowercase();
            p.parse_nested_block(|n| parse_math_fn_opt(&lower, n, allow_size).ok_or_else(|| n.new_custom_error::<_, ()>(()))).ok()
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
        let mut rc = 0u32;
        while !p.is_exhausted() && vals.len() < 3 {
            let mut pm = parse_math_value_from_parser(&mut p, ValueKind::NumberOrPercentage)?;
            pm.node.bind_random_property("scale", &mut rc);
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
        let mut rc = 0u32;
        for _ in 0..2 {
            if p.is_exhausted() { break; }
            if angle.is_none() {
                if let Some(mut pm) = parse_math_value_from_parser(&mut p, ValueKind::Angle) {
                    pm.node.bind_random_property("rotate", &mut rc);
                    angle = Some((pm.node.clone(), pm.node.serialize_specified(pm.was_math_fn)));
                    continue;
                }
            }
            if axis_kw.is_none() && axis_nums.is_none() {
                if let Ok(kw) = p.try_parse(|p| -> Result<&'static str, cssparser::ParseError<'_, ()>> {
                    match p.expect_ident()?.to_ascii_lowercase().as_str() { "x" => Ok("x"), "y" => Ok("y"), "z" => Ok("z"), _ => Err(p.new_custom_error(())) }
                }) { axis_kw = Some(kw); continue; }
                let st = p.state();
                if let (Some(mut a), Some(mut b), Some(mut c)) = (
                    parse_math_value_from_parser(&mut p, ValueKind::Number),
                    parse_math_value_from_parser(&mut p, ValueKind::Number),
                    parse_math_value_from_parser(&mut p, ValueKind::Number),
                ) {
                    a.node.bind_random_property("rotate", &mut rc);
                    b.node.bind_random_property("rotate", &mut rc);
                    c.node.bind_random_property("rotate", &mut rc);
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
                if xv == 0.0 && yv == 0.0 && zv > 0.0 { deg_s }
                else if xv > 0.0 && yv == 0.0 && zv == 0.0 { format!("x {deg_s}") }
                else if xv == 0.0 && yv > 0.0 && zv == 0.0 { format!("y {deg_s}") }
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

/// Edge keyword for W3C CSS Values L5 `<position>` (physical + logical `x-start`/`x-end`/`y-start`/`y-end`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PositionEdge {
    Left,
    Right,
    Top,
    Bottom,
    Center,
    XStart,
    XEnd,
    YStart,
    YEnd,
}

#[rustfmt::skip]
impl PositionEdge {
    fn from_ident(s: &str) -> Option<Self> {
        match s {
            "left" => Some(Self::Left), "right" => Some(Self::Right), "top" => Some(Self::Top),
            "bottom" => Some(Self::Bottom), "center" => Some(Self::Center),
            "x-start" => Some(Self::XStart), "x-end" => Some(Self::XEnd),
            "y-start" => Some(Self::YStart), "y-end" => Some(Self::YEnd),
            _ => None,
        }
    }
    const fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left", Self::Right => "right", Self::Top => "top",
            Self::Bottom => "bottom", Self::Center => "center",
            Self::XStart => "x-start", Self::XEnd => "x-end",
            Self::YStart => "y-start", Self::YEnd => "y-end",
        }
    }
    const fn is_horiz(self) -> bool { matches!(self, Self::Left | Self::Right | Self::XStart | Self::XEnd) }
    const fn is_vert(self) -> bool { matches!(self, Self::Top | Self::Bottom | Self::YStart | Self::YEnd) }
    fn to_physical(self, wm: crate::values::WritingMode, dir: crate::values::DirectionMode) -> Self {
        use crate::values::{DirectionMode as D, WritingMode as W};
        match self {
            Self::XStart => match (wm, dir) {
                (W::HorizontalTb, D::Rtl) | (W::VerticalRl | W::SidewaysRl, _) => Self::Right,
                _ => Self::Left,
            },
            Self::XEnd => match (wm, dir) {
                (W::HorizontalTb, D::Rtl) | (W::VerticalRl | W::SidewaysRl, _) => Self::Left,
                _ => Self::Right,
            },
            Self::YStart => match (wm, dir) {
                (W::VerticalRl | W::SidewaysRl | W::VerticalLr, D::Rtl) | (W::SidewaysLr, D::Ltr) => Self::Bottom,
                _ => Self::Top,
            },
            Self::YEnd => match (wm, dir) {
                (W::VerticalRl | W::SidewaysRl | W::VerticalLr, D::Rtl) | (W::SidewaysLr, D::Ltr) => Self::Top,
                _ => Self::Bottom,
            },
            other => other,
        }
    }
}

/// One axis component of a CSS Values L5 `<position>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PositionComponent {
    Edge(PositionEdge),
    EdgeOffset(PositionEdge, Box<MathNode>, bool),
    Length(Box<MathNode>, bool),
}

/// Parsed CSS Values L5 `<position>` value (e.g. `object-position`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionSpec {
    pub x: PositionComponent,
    pub y: PositionComponent,
}

impl Default for PositionSpec {
    fn default() -> Self {
        Self {
            x: PositionComponent::Edge(PositionEdge::Center),
            y: PositionComponent::Edge(PositionEdge::Center),
        }
    }
}

#[rustfmt::skip]
impl PositionSpec {
    /// Parses a CSS Values L5 `<position>` (1, 2, or 4 values) and returns `(spec, specified_css)`.
    #[must_use]
    pub fn parse(input: &str) -> Option<(Self, String)> {
        enum Tok { Edge(PositionEdge), Len(MathNode, bool) }
        let mut pi = ParserInput::new(input);
        let mut p = Parser::new(&mut pi);
        let mut toks = Vec::with_capacity(4);
        while !p.is_exhausted() {
            if toks.len() >= 4 { return None; }
            if let Ok(edge) = p.try_parse(|p| -> Result<PositionEdge, cssparser::ParseError<'_, ()>> {
                PositionEdge::from_ident(&p.expect_ident()?.to_ascii_lowercase()).ok_or_else(|| p.new_custom_error(()))
            }) {
                toks.push(Tok::Edge(edge));
            } else {
                let pm = parse_math_value_from_parser(&mut p, ValueKind::LengthPercentage)?;
                toks.push(Tok::Len(pm.node, pm.was_math_fn));
            }
        }
        let spec = match toks.len() {
            1 => match toks.pop()? {
                Tok::Edge(e) if e.is_vert() => Self { x: PositionComponent::Edge(PositionEdge::Center), y: PositionComponent::Edge(e) },
                Tok::Edge(e) => Self { x: PositionComponent::Edge(e), y: PositionComponent::Edge(PositionEdge::Center) },
                Tok::Len(n, m) => Self { x: PositionComponent::Length(Box::new(n), m), y: PositionComponent::Edge(PositionEdge::Center) },
            },
            2 => {
                let b = toks.pop()?; let a = toks.pop()?;
                match (a, b) {
                    (Tok::Edge(e1), Tok::Edge(e2)) => {
                        if (e1.is_horiz() && e2.is_horiz()) || (e1.is_vert() && e2.is_vert()) { return None; }
                        if e1.is_vert() || e2.is_horiz() { Self { x: PositionComponent::Edge(e2), y: PositionComponent::Edge(e1) } }
                        else { Self { x: PositionComponent::Edge(e1), y: PositionComponent::Edge(e2) } }
                    }
                    (Tok::Edge(e), Tok::Len(n, m)) if e.is_horiz() || e == PositionEdge::Center => {
                        Self { x: PositionComponent::Edge(e), y: PositionComponent::Length(Box::new(n), m) }
                    }
                    (Tok::Len(n, m), Tok::Edge(e)) if e.is_vert() || e == PositionEdge::Center => {
                        Self { x: PositionComponent::Length(Box::new(n), m), y: PositionComponent::Edge(e) }
                    }
                    (Tok::Len(nx, mx), Tok::Len(ny, my)) => {
                        Self { x: PositionComponent::Length(Box::new(nx), mx), y: PositionComponent::Length(Box::new(ny), my) }
                    }
                    _ => return None,
                }
            }
            4 => {
                let d = toks.pop()?; let c = toks.pop()?; let b = toks.pop()?; let a = toks.pop()?;
                match (a, b, c, d) {
                    (Tok::Edge(e1), Tok::Len(n1, m1), Tok::Edge(e2), Tok::Len(n2, m2)) => {
                        if e1.is_horiz() && e2.is_vert() {
                            Self { x: PositionComponent::EdgeOffset(e1, Box::new(n1), m1), y: PositionComponent::EdgeOffset(e2, Box::new(n2), m2) }
                        } else if e1.is_vert() && e2.is_horiz() {
                            Self { x: PositionComponent::EdgeOffset(e2, Box::new(n2), m2), y: PositionComponent::EdgeOffset(e1, Box::new(n1), m1) }
                        } else { return None; }
                    }
                    _ => return None,
                }
            }
            _ => return None,
        };
        let specified = spec.serialize_specified();
        Some((spec, specified))
    }

    #[must_use]
    pub fn serialize_specified(&self) -> String {
        let fmt = |c: &PositionComponent| match c {
            PositionComponent::Edge(e) => e.as_str().to_string(),
            PositionComponent::EdgeOffset(e, n, m) => format!("{} {}", e.as_str(), n.serialize_specified(*m)),
            PositionComponent::Length(n, m) => n.serialize_specified(*m),
        };
        format!("{} {}", fmt(&self.x), fmt(&self.y))
    }

    #[must_use]
    pub fn serialize_computed(&self, ctx: &EvalContext, wm: crate::values::WritingMode, dir: crate::values::DirectionMode) -> String {
        let fmt_axis = |c: &PositionComponent| -> String {
            match c {
                PositionComponent::Edge(e) => match e.to_physical(wm, dir) {
                    PositionEdge::Left | PositionEdge::Top => "0%".to_string(),
                    PositionEdge::Center => "50%".to_string(),
                    PositionEdge::Right | PositionEdge::Bottom => "100%".to_string(),
                    _ => "50%".to_string(),
                },
                PositionComponent::Length(n, _) => n.serialize_computed(ctx, None, ValueKind::LengthPercentage, None, None),
                PositionComponent::EdgeOffset(e, n, _) => match e.to_physical(wm, dir) {
                    PositionEdge::Right | PositionEdge::Bottom => {
                        let inv = MathNode::Sum(vec![MathNode::Percent(100.0), MathNode::Negate(n.clone())]);
                        inv.serialize_computed(ctx, None, ValueKind::LengthPercentage, None, None)
                    }
                    _ => n.serialize_computed(ctx, None, ValueKind::LengthPercentage, None, None),
                },
            }
        };
        format!("{} {}", fmt_axis(&self.x), fmt_axis(&self.y))
    }
}

/// Parses a CSS Backgrounds L3 §3.6 comma-separated `<bg-position>#` list (supporting 1, 2, 3,
/// and 4-value syntax) into `(positions, specified_css)`.
#[must_use]
pub fn parse_background_position_list(
    input: &str,
) -> Option<(Vec<(LengthPercentage, LengthPercentage)>, String)> {
    enum BgTok {
        Edge(PositionEdge),
        Len(MathNode, bool),
    }
    let parse_physical_edge = |ident: &str| -> Option<PositionEdge> {
        match ident.to_ascii_lowercase().as_str() {
            "left" => Some(PositionEdge::Left),
            "right" => Some(PositionEdge::Right),
            "top" => Some(PositionEdge::Top),
            "bottom" => Some(PositionEdge::Bottom),
            "center" => Some(PositionEdge::Center),
            _ => None,
        }
    };
    let is_h = |e: PositionEdge| matches!(e, PositionEdge::Left | PositionEdge::Right);
    let is_v = |e: PositionEdge| matches!(e, PositionEdge::Top | PositionEdge::Bottom);
    let comp_to_lp = |c: &PositionComponent| -> LengthPercentage {
        match c {
            PositionComponent::Edge(PositionEdge::Left | PositionEdge::Top) => {
                LengthPercentage::Percent(0.0)
            }
            PositionComponent::Edge(PositionEdge::Center) => LengthPercentage::Percent(0.5),
            PositionComponent::Edge(PositionEdge::Right | PositionEdge::Bottom) => {
                LengthPercentage::Percent(1.0)
            }
            PositionComponent::Edge(_) => LengthPercentage::Percent(0.5),
            PositionComponent::Length(n, _) => {
                LengthPercentage::Calc(Box::new(crate::values::CalcExpr::Math(n.clone())))
            }
            PositionComponent::EdgeOffset(PositionEdge::Right | PositionEdge::Bottom, n, _) => {
                let inv =
                    MathNode::Sum(vec![MathNode::Percent(100.0), MathNode::Negate(n.clone())]);
                LengthPercentage::Calc(Box::new(crate::values::CalcExpr::Math(Box::new(inv))))
            }
            PositionComponent::EdgeOffset(_, n, _) => {
                LengthPercentage::Calc(Box::new(crate::values::CalcExpr::Math(n.clone())))
            }
        }
    };
    let fmt_comp = |c: &PositionComponent| -> String {
        match c {
            PositionComponent::Edge(PositionEdge::Left | PositionEdge::Top) => "0%".to_string(),
            PositionComponent::Edge(PositionEdge::Center) => "50%".to_string(),
            PositionComponent::Edge(PositionEdge::Right | PositionEdge::Bottom) => {
                "100%".to_string()
            }
            PositionComponent::Edge(e) => e.as_str().to_string(),
            PositionComponent::Length(n, m) => n.serialize_specified(*m),
            PositionComponent::EdgeOffset(e, n, m) => {
                format!("{} {}", e.as_str(), n.serialize_specified(*m))
            }
        }
    };

    let mut pi = ParserInput::new(input);
    let mut p = Parser::new(&mut pi);
    let parsed: Result<Vec<PositionSpec>, cssparser::ParseError<'_, ()>> =
        p.parse_comma_separated(|p| {
            let mut toks = Vec::with_capacity(4);
            while !p.is_exhausted() {
                if toks.len() >= 4 {
                    return Err(p.new_custom_error(()));
                }
                if let Ok(edge) =
                    p.try_parse(|p| -> Result<PositionEdge, cssparser::ParseError<'_, ()>> {
                        let id = p.expect_ident()?;
                        parse_physical_edge(id.as_ref()).ok_or_else(|| p.new_custom_error(()))
                    })
                {
                    toks.push(BgTok::Edge(edge));
                } else if let Some(pm) =
                    parse_math_value_from_parser(p, ValueKind::LengthPercentage)
                {
                    toks.push(BgTok::Len(pm.node, pm.was_math_fn));
                } else {
                    return Err(p.new_custom_error(()));
                }
            }
            let spec = match toks.len() {
                1 => match toks.pop().ok_or_else(|| p.new_custom_error(()))? {
                    BgTok::Edge(e) if is_v(e) => PositionSpec {
                        x: PositionComponent::Edge(PositionEdge::Center),
                        y: PositionComponent::Edge(e),
                    },
                    BgTok::Edge(e) => PositionSpec {
                        x: PositionComponent::Edge(e),
                        y: PositionComponent::Edge(PositionEdge::Center),
                    },
                    BgTok::Len(n, m) => PositionSpec {
                        x: PositionComponent::Length(Box::new(n), m),
                        y: PositionComponent::Edge(PositionEdge::Center),
                    },
                },
                2 => {
                    let b = toks.pop().ok_or_else(|| p.new_custom_error(()))?;
                    let a = toks.pop().ok_or_else(|| p.new_custom_error(()))?;
                    match (a, b) {
                        (BgTok::Edge(e1), BgTok::Edge(e2)) => {
                            if (is_h(e1) && is_h(e2)) || (is_v(e1) && is_v(e2)) {
                                return Err(p.new_custom_error(()));
                            }
                            if is_v(e1) || is_h(e2) {
                                PositionSpec {
                                    x: PositionComponent::Edge(e2),
                                    y: PositionComponent::Edge(e1),
                                }
                            } else {
                                PositionSpec {
                                    x: PositionComponent::Edge(e1),
                                    y: PositionComponent::Edge(e2),
                                }
                            }
                        }
                        (BgTok::Edge(e), BgTok::Len(n, m))
                            if is_h(e) || e == PositionEdge::Center =>
                        {
                            PositionSpec {
                                x: PositionComponent::Edge(e),
                                y: PositionComponent::Length(Box::new(n), m),
                            }
                        }
                        (BgTok::Len(n, m), BgTok::Edge(e))
                            if is_v(e) || e == PositionEdge::Center =>
                        {
                            PositionSpec {
                                x: PositionComponent::Length(Box::new(n), m),
                                y: PositionComponent::Edge(e),
                            }
                        }
                        (BgTok::Len(nx, mx), BgTok::Len(ny, my)) => PositionSpec {
                            x: PositionComponent::Length(Box::new(nx), mx),
                            y: PositionComponent::Length(Box::new(ny), my),
                        },
                        _ => return Err(p.new_custom_error(())),
                    }
                }
                3 => {
                    let c = toks.pop().ok_or_else(|| p.new_custom_error(()))?;
                    let b = toks.pop().ok_or_else(|| p.new_custom_error(()))?;
                    let a = toks.pop().ok_or_else(|| p.new_custom_error(()))?;
                    match (a, b, c) {
                        (BgTok::Edge(e1), BgTok::Len(n1, m1), BgTok::Edge(e2)) => {
                            if is_h(e1) && (is_v(e2) || e2 == PositionEdge::Center) {
                                PositionSpec {
                                    x: PositionComponent::EdgeOffset(e1, Box::new(n1), m1),
                                    y: PositionComponent::Edge(e2),
                                }
                            } else if is_v(e1) && (is_h(e2) || e2 == PositionEdge::Center) {
                                PositionSpec {
                                    x: PositionComponent::Edge(e2),
                                    y: PositionComponent::EdgeOffset(e1, Box::new(n1), m1),
                                }
                            } else {
                                return Err(p.new_custom_error(()));
                            }
                        }
                        (BgTok::Edge(e1), BgTok::Edge(e2), BgTok::Len(n2, m2)) => {
                            if (is_h(e1) || e1 == PositionEdge::Center) && is_v(e2) {
                                PositionSpec {
                                    x: PositionComponent::Edge(e1),
                                    y: PositionComponent::EdgeOffset(e2, Box::new(n2), m2),
                                }
                            } else if (is_v(e1) || e1 == PositionEdge::Center) && is_h(e2) {
                                PositionSpec {
                                    x: PositionComponent::EdgeOffset(e2, Box::new(n2), m2),
                                    y: PositionComponent::Edge(e1),
                                }
                            } else {
                                return Err(p.new_custom_error(()));
                            }
                        }
                        _ => return Err(p.new_custom_error(())),
                    }
                }
                4 => {
                    let d = toks.pop().ok_or_else(|| p.new_custom_error(()))?;
                    let c = toks.pop().ok_or_else(|| p.new_custom_error(()))?;
                    let b = toks.pop().ok_or_else(|| p.new_custom_error(()))?;
                    let a = toks.pop().ok_or_else(|| p.new_custom_error(()))?;
                    match (a, b, c, d) {
                        (
                            BgTok::Edge(e1),
                            BgTok::Len(n1, m1),
                            BgTok::Edge(e2),
                            BgTok::Len(n2, m2),
                        ) => {
                            if is_h(e1) && is_v(e2) {
                                PositionSpec {
                                    x: PositionComponent::EdgeOffset(e1, Box::new(n1), m1),
                                    y: PositionComponent::EdgeOffset(e2, Box::new(n2), m2),
                                }
                            } else if is_v(e1) && is_h(e2) {
                                PositionSpec {
                                    x: PositionComponent::EdgeOffset(e2, Box::new(n2), m2),
                                    y: PositionComponent::EdgeOffset(e1, Box::new(n1), m1),
                                }
                            } else {
                                return Err(p.new_custom_error(()));
                            }
                        }
                        _ => return Err(p.new_custom_error(())),
                    }
                }
                _ => return Err(p.new_custom_error(())),
            };
            Ok(spec)
        });
    let specs = parsed.ok()?;
    if specs.is_empty() || !p.is_exhausted() {
        return None;
    }
    let lps: Vec<(LengthPercentage, LengthPercentage)> = specs
        .iter()
        .map(|s| (comp_to_lp(&s.x), comp_to_lp(&s.y)))
        .collect();
    let serialized = specs
        .iter()
        .map(|s| format!("{} {}", fmt_comp(&s.x), fmt_comp(&s.y)))
        .collect::<Vec<_>>()
        .join(", ");
    Some((lps, serialized))
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
                ("left", false) | ("top", true) => (0.0, 0.0),
                ("center", _) => (0.5, 50.0),
                ("right", false) | ("bottom", true) => (1.0, 100.0),
                _ => {
                    p.reset(&st);
                    return None;
                }
            };
            return Some((LengthPercentage::Percent(pct), format!("{}%", format_math_f64(kw))));
        }
        let pm = parse_math_value_from_parser(p, ValueKind::LengthPercentage)?;
        let spec = pm.node.serialize_specified(pm.was_math_fn);
        Some((LengthPercentage::Calc(Box::new(crate::values::CalcExpr::Math(Box::new(pm.node)))), spec))
    };
    // CSS Backgrounds L3 §3.6: `<bg-position>` accepts vertical keyword first
    // (`top`, `bottom`, `top center`, `bottom right`, etc.) in addition to `x y`.
    let st_start = p.state();
    if let Ok(first_kw) = p.try_parse(|p| p.expect_ident().map(|s| s.to_ascii_lowercase())) {
        if matches!(first_kw.as_str(), "top" | "bottom") {
            let (yp, ys) = if first_kw == "top" {
                (LengthPercentage::Percent(0.0), "0%".to_string())
            } else {
                (LengthPercentage::Percent(1.0), "100%".to_string())
            };
            if p.is_exhausted() {
                return Some((LengthPercentage::Percent(0.5), yp, format!("50% {ys}")));
            }
            if let Ok(second_kw) = p.try_parse(|p| p.expect_ident().map(|s| s.to_ascii_lowercase())) {
                let x_opt = match second_kw.as_str() {
                    "left" => Some((LengthPercentage::Percent(0.0), "0%".to_string())),
                    "center" => Some((LengthPercentage::Percent(0.5), "50%".to_string())),
                    "right" => Some((LengthPercentage::Percent(1.0), "100%".to_string())),
                    _ => None,
                };
                if let Some((xp, xs)) = x_opt {
                    if p.is_exhausted() {
                        return Some((xp, yp, format!("{xs} {ys}")));
                    }
                }
            }
            p.reset(&st_start);
            return None;
        }
        p.reset(&st_start);
    }
    let (x, xs) = parse_part(&mut p, false)?;
    if p.is_exhausted() {
        return Some((x, LengthPercentage::Percent(0.5), format!("{xs} 50%")));
    }
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
            Self::Number(_) | Self::Percent(_) | Self::Dimension(_, _) | Self::SizeKeyword => false,
            Self::Negate(a) | Self::Invert(a) | Self::Sin(a) | Self::Cos(a) | Self::Tan(a) | Self::Asin(a) | Self::Acos(a) | Self::Atan(a) | Self::Sqrt(a) | Self::Exp(a) | Self::Abs(a) | Self::Sign(a) => a.contains_sibling_fn(),
            Self::Mod(a, b) | Self::Rem(a, b) | Self::Atan2(a, b) | Self::Pow(a, b) => a.contains_sibling_fn() || b.contains_sibling_fn(),
            Self::Round(_, a, b) | Self::Log(a, b) => a.contains_sibling_fn() || b.as_deref().is_some_and(Self::contains_sibling_fn),
            Self::Clamp(a, b, c) => a.as_deref().is_some_and(Self::contains_sibling_fn) || b.contains_sibling_fn() || c.as_deref().is_some_and(Self::contains_sibling_fn),
            Self::Progress(_, a, b, c) => a.contains_sibling_fn() || b.contains_sibling_fn() || c.contains_sibling_fn(),
            Self::Sum(v) | Self::Product(v) | Self::Min(v) | Self::Max(v) | Self::Hypot(v) => v.iter().any(Self::contains_sibling_fn),
            Self::CalcMix(items) => items.iter().any(|it| it.value.contains_sibling_fn() || it.weight.as_ref().is_some_and(|(w, _)| w.contains_sibling_fn())),
            Self::Random(r) => r.fixed.as_ref().is_some_and(|(f, _)| f.contains_sibling_fn()) || r.min.contains_sibling_fn() || r.max.contains_sibling_fn() || r.step.as_deref().is_some_and(Self::contains_sibling_fn),
        }
    }

    #[must_use]
    pub fn contains_random(&self) -> bool {
        match self {
            Self::Random(_) => true,
            Self::Number(_) | Self::Percent(_) | Self::Dimension(_, _) | Self::SizeKeyword | Self::SiblingIndex | Self::SiblingCount => false,
            Self::Negate(a) | Self::Invert(a) | Self::Sin(a) | Self::Cos(a) | Self::Tan(a) | Self::Asin(a) | Self::Acos(a) | Self::Atan(a) | Self::Sqrt(a) | Self::Exp(a) | Self::Abs(a) | Self::Sign(a) => a.contains_random(),
            Self::Mod(a, b) | Self::Rem(a, b) | Self::Atan2(a, b) | Self::Pow(a, b) => a.contains_random() || b.contains_random(),
            Self::Round(_, a, b) | Self::Log(a, b) => a.contains_random() || b.as_deref().is_some_and(Self::contains_random),
            Self::Clamp(a, b, c) => a.as_deref().is_some_and(Self::contains_random) || b.contains_random() || c.as_deref().is_some_and(Self::contains_random),
            Self::Progress(_, a, b, c) => a.contains_random() || b.contains_random() || c.contains_random(),
            Self::Sum(v) | Self::Product(v) | Self::Min(v) | Self::Max(v) | Self::Hypot(v) => v.iter().any(Self::contains_random),
            Self::CalcMix(items) => items.iter().any(|it| it.value.contains_random() || it.weight.as_ref().is_some_and(|(w, _)| w.contains_random())),
        }
    }

    /// Assigns canonical `ua-<prop>` / `ua-<prop>-<index>` caching identifiers to `random()` nodes.
    pub fn bind_random_property(&mut self, prop_name: &str, counter: &mut u32) {
        match self {
            Self::Number(_) | Self::Percent(_) | Self::Dimension(_, _) | Self::SizeKeyword | Self::SiblingIndex | Self::SiblingCount => {}
            Self::Negate(a) | Self::Invert(a) | Self::Sin(a) | Self::Cos(a) | Self::Tan(a) | Self::Asin(a) | Self::Acos(a) | Self::Atan(a) | Self::Sqrt(a) | Self::Exp(a) | Self::Abs(a) | Self::Sign(a) => a.bind_random_property(prop_name, counter),
            Self::Mod(a, b) | Self::Rem(a, b) | Self::Atan2(a, b) | Self::Pow(a, b) => { a.bind_random_property(prop_name, counter); b.bind_random_property(prop_name, counter); }
            Self::Round(_, a, b) | Self::Log(a, b) => { a.bind_random_property(prop_name, counter); if let Some(x) = b { x.bind_random_property(prop_name, counter); } }
            Self::Clamp(a, b, c) => { if let Some(x) = a { x.bind_random_property(prop_name, counter); } b.bind_random_property(prop_name, counter); if let Some(x) = c { x.bind_random_property(prop_name, counter); } }
            Self::Progress(_, a, b, c) => { a.bind_random_property(prop_name, counter); b.bind_random_property(prop_name, counter); c.bind_random_property(prop_name, counter); }
            Self::Sum(v) | Self::Product(v) | Self::Min(v) | Self::Max(v) | Self::Hypot(v) => { for x in v { x.bind_random_property(prop_name, counter); } }
            Self::CalcMix(items) => { for it in items { it.value.bind_random_property(prop_name, counter); if let Some((w, _)) = &mut it.weight { w.bind_random_property(prop_name, counter); } } }
            Self::Random(r) => {
                if let Some((f, _)) = &mut r.fixed { f.bind_random_property(prop_name, counter); }
                r.min.bind_random_property(prop_name, counter);
                r.max.bind_random_property(prop_name, counter);
                if let Some(st) = &mut r.step { st.bind_random_property(prop_name, counter); }
                if r.fixed.is_none() && r.ua_ident.is_none() {
                    if r.prop_index_scoped {
                        *counter += 1;
                        r.ua_ident = Some(format!("ua-{prop_name}-{counter}"));
                    } else if r.prop_scoped {
                        r.ua_ident = Some(format!("ua-{prop_name}"));
                    }
                }
            }
        }
    }
}

#[must_use]
#[rustfmt::skip]
pub fn has_values4_syntax(raw: &str) -> bool {
    if raw.contains('(') {
        let lower = raw.to_ascii_lowercase();
        for fn_pfx in ["calc(", "calc-size(", "calc-mix(", "random(", "min(", "max(", "clamp(", "round(", "mod(", "rem(", "sin(", "cos(", "tan(", "asin(", "acos(", "atan(", "atan2(", "pow(", "sqrt(", "hypot(", "log(", "exp(", "abs(", "sign(", "progress(", "sibling-index(", "sibling-count("] {
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
        if let Ok((rep_tracks, rep_text)) = p.try_parse(|p| -> Result<(Vec<GridTrackSize>, String), cssparser::ParseError<'_, ()>> {
            let Token::Function(ref fname) = p.next()?.clone() else { return Err(p.new_custom_error(())); };
            if !fname.eq_ignore_ascii_case("repeat") { return Err(p.new_custom_error(())); }
            p.parse_nested_block(|n| {
                let Some(cnt_pm) = parse_math_value_from_parser(n, ValueKind::Integer) else { return Err(n.new_custom_error(())); };
                let cnt_eval = cnt_pm.node.eval_scalar(&EvalContext::default(), None);
                if !cnt_eval.is_finite() { return Err(n.new_custom_error(())); }
                let count = (cnt_eval + 0.5).floor() as i64;
                if !(1..=1024).contains(&count) { return Err(n.new_custom_error(())); }
                n.expect_comma()?;
                let rest = n.current_line();
                let _ = rest;
                let mut inner_tracks = Vec::new();
                let mut inner_texts = Vec::new();
                while !n.is_exhausted() {
                    if let Ok(kw) = n.try_parse(|p| -> Result<&'static str, cssparser::ParseError<'_, ()>> {
                        match p.expect_ident()?.to_ascii_lowercase().as_str() { "auto" => Ok("auto"), "min-content" => Ok("min-content"), "max-content" => Ok("max-content"), _ => Err(p.new_custom_error(())) }
                    }) {
                        inner_tracks.push(match kw { "min-content" => GridTrackSize::MinContent, "max-content" => GridTrackSize::MaxContent, _ => GridTrackSize::Auto });
                        inner_texts.push(kw.to_string());
                    } else if let Some(pm) = parse_math_value_from_parser(n, ValueKind::Flex) {
                        inner_texts.push(pm.node.serialize_specified(pm.was_math_fn));
                        inner_tracks.push(GridTrackSize::FrMath(Box::new(pm.node)));
                    } else if let Some(pm) = parse_math_value_from_parser(n, ValueKind::LengthPercentage) {
                        inner_texts.push(pm.node.serialize_specified(pm.was_math_fn));
                        inner_tracks.push(GridTrackSize::Length(LengthPercentage::Calc(Box::new(CalcExpr::Math(Box::new(pm.node))))));
                    } else { return Err(n.new_custom_error(())); }
                }
                if inner_tracks.is_empty() { return Err(n.new_custom_error(())); }
                let mut expanded = Vec::with_capacity(inner_tracks.len() * count as usize);
                for _ in 0..count { expanded.extend(inner_tracks.iter().cloned()); }
                Ok((expanded, format!("repeat({}, {})", cnt_pm.node.serialize_specified(cnt_pm.was_math_fn), inner_texts.join(" "))))
            })
        }) {
            tracks.extend(rep_tracks);
            texts.push(rep_text);
            continue;
        }
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
        GridTrackSize::FitContent(lp) => format!(
            "fit-content({})",
            format_grid_tracks_computed(&[GridTrackSize::Length(lp.clone())], ctx)
        ),
        GridTrackSize::MinMax(a, b) => format!("minmax({}, {})", format_grid_tracks_computed(&[(**a).clone()], ctx), format_grid_tracks_computed(&[(**b).clone()], ctx)),
    }).collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// W3C CSS Values L5 §10.10 `calc-size()` & Tree Utilities
// ---------------------------------------------------------------------------

/// Basis of a CSS Values L5 `calc-size(<calc-size-basis>, <calc-size-calculation>)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CalcSizeBasis {
    Auto,
    MinContent,
    MaxContent,
    FitContent,
    Stretch,
    Content,
    Any,
    Length(Box<MathNode>),
    Nested(Box<CalcSizeExpr>),
}

/// Parsed CSS Values L5 `calc-size(basis, calculation)` expression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalcSizeExpr {
    pub basis: CalcSizeBasis,
    pub calculation: Box<MathNode>,
}

#[rustfmt::skip]
impl MathNode {
    #[must_use]
    pub fn has_percentage(&self) -> bool {
        match self {
            Self::Percent(_) => true,
            Self::Number(_) | Self::Dimension(_, _) | Self::SizeKeyword | Self::SiblingIndex | Self::SiblingCount => false,
            Self::Negate(a) | Self::Invert(a) | Self::Sin(a) | Self::Cos(a) | Self::Tan(a) | Self::Asin(a) | Self::Acos(a) | Self::Atan(a) | Self::Sqrt(a) | Self::Exp(a) | Self::Abs(a) | Self::Sign(a) => a.has_percentage(),
            Self::Mod(a, b) | Self::Rem(a, b) | Self::Atan2(a, b) | Self::Pow(a, b) => a.has_percentage() || b.has_percentage(),
            Self::Log(a, b) | Self::Round(_, a, b) => a.has_percentage() || b.as_deref().is_some_and(Self::has_percentage),
            Self::Clamp(a, b, c) => a.as_deref().is_some_and(Self::has_percentage) || b.has_percentage() || c.as_deref().is_some_and(Self::has_percentage),
            Self::Progress(_, v, s, e) => v.has_percentage() || s.has_percentage() || e.has_percentage(),
            Self::Sum(v) | Self::Product(v) | Self::Min(v) | Self::Max(v) | Self::Hypot(v) => v.iter().any(Self::has_percentage),
            Self::CalcMix(items) => items.iter().any(|it| it.value.has_percentage()),
            Self::Random(r) => r.min.has_percentage() || r.max.has_percentage() || r.step.as_deref().is_some_and(Self::has_percentage),
        }
    }

    #[must_use]
    pub fn contains_size_keyword(&self) -> bool {
        match self {
            Self::SizeKeyword => true,
            Self::Number(_) | Self::Percent(_) | Self::Dimension(_, _) | Self::SiblingIndex | Self::SiblingCount => false,
            Self::Negate(a) | Self::Invert(a) | Self::Sin(a) | Self::Cos(a) | Self::Tan(a) | Self::Asin(a) | Self::Acos(a) | Self::Atan(a) | Self::Sqrt(a) | Self::Exp(a) | Self::Abs(a) | Self::Sign(a) => a.contains_size_keyword(),
            Self::Mod(a, b) | Self::Rem(a, b) | Self::Atan2(a, b) | Self::Pow(a, b) => a.contains_size_keyword() || b.contains_size_keyword(),
            Self::Log(a, b) | Self::Round(_, a, b) => a.contains_size_keyword() || b.as_deref().is_some_and(Self::contains_size_keyword),
            Self::Clamp(a, b, c) => a.as_deref().is_some_and(Self::contains_size_keyword) || b.contains_size_keyword() || c.as_deref().is_some_and(Self::contains_size_keyword),
            Self::Progress(_, v, s, e) => v.contains_size_keyword() || s.contains_size_keyword() || e.contains_size_keyword(),
            Self::Sum(v) | Self::Product(v) | Self::Min(v) | Self::Max(v) | Self::Hypot(v) => v.iter().any(Self::contains_size_keyword),
            Self::CalcMix(items) => items.iter().any(|it| it.value.contains_size_keyword()),
            Self::Random(r) => r.min.contains_size_keyword() || r.max.contains_size_keyword() || r.step.as_deref().is_some_and(Self::contains_size_keyword),
        }
    }

    #[must_use]
    pub fn node_count(&self) -> usize {
        match self {
            Self::Number(_) | Self::Percent(_) | Self::Dimension(_, _) | Self::SizeKeyword | Self::SiblingIndex | Self::SiblingCount => 1,
            Self::Negate(a) | Self::Invert(a) | Self::Sin(a) | Self::Cos(a) | Self::Tan(a) | Self::Asin(a) | Self::Acos(a) | Self::Atan(a) | Self::Sqrt(a) | Self::Exp(a) | Self::Abs(a) | Self::Sign(a) => 1 + a.node_count(),
            Self::Mod(a, b) | Self::Rem(a, b) | Self::Atan2(a, b) | Self::Pow(a, b) => 1 + a.node_count() + b.node_count(),
            Self::Log(a, b) | Self::Round(_, a, b) => 1 + a.node_count() + b.as_deref().map_or(0, Self::node_count),
            Self::Clamp(a, b, c) => 1 + a.as_deref().map_or(0, Self::node_count) + b.node_count() + c.as_deref().map_or(0, Self::node_count),
            Self::Progress(_, v, s, e) => 1 + v.node_count() + s.node_count() + e.node_count(),
            Self::Sum(v) | Self::Product(v) | Self::Min(v) | Self::Max(v) | Self::Hypot(v) => 1 + v.iter().map(Self::node_count).sum::<usize>(),
            Self::CalcMix(items) => 1 + items.iter().map(|it| it.value.node_count() + it.weight.as_ref().map_or(0, |(w, _)| w.node_count())).sum::<usize>(),
            Self::Random(r) => 1 + r.min.node_count() + r.max.node_count() + r.step.as_deref().map_or(0, Self::node_count),
        }
    }

    /// Substitutes every `SizeKeyword` with `replacement`, returning `None` if expanded AST exceeds `budget`.
    #[must_use]
    pub fn substitute_size(&self, replacement: &Self, budget: &mut usize) -> Option<Self> {
        if *budget == 0 { return None; }
        *budget -= 1;
        let sub = |n: &Self, b: &mut usize| n.substitute_size(replacement, b);
        let sub_vec = |v: &[Self], b: &mut usize| v.iter().map(|x| sub(x, b)).collect::<Option<Vec<_>>>();
        Some(match self {
            Self::SizeKeyword => {
                let cost = replacement.node_count();
                if cost > *budget + 1 { return None; }
                *budget = (*budget + 1).saturating_sub(cost);
                replacement.clone()
            }
            Self::Number(_) | Self::Percent(_) | Self::Dimension(_, _) | Self::SiblingIndex | Self::SiblingCount => self.clone(),
            Self::Negate(a) => Self::Negate(Box::new(sub(a, budget)?)),
            Self::Invert(a) => Self::Invert(Box::new(sub(a, budget)?)),
            Self::Sum(v) => Self::Sum(sub_vec(v, budget)?),
            Self::Product(v) => Self::Product(sub_vec(v, budget)?),
            Self::Min(v) => Self::Min(sub_vec(v, budget)?),
            Self::Max(v) => Self::Max(sub_vec(v, budget)?),
            Self::Hypot(v) => Self::Hypot(sub_vec(v, budget)?),
            Self::Clamp(a, m, c) => Self::Clamp(
                match a { Some(x) => Some(Box::new(sub(x, budget)?)), None => None },
                Box::new(sub(m, budget)?),
                match c { Some(x) => Some(Box::new(sub(x, budget)?)), None => None },
            ),
            Self::Round(st, a, b) => Self::Round(
                *st,
                Box::new(sub(a, budget)?),
                match b { Some(x) => Some(Box::new(sub(x, budget)?)), None => None },
            ),
            Self::Mod(a, b) => Self::Mod(Box::new(sub(a, budget)?), Box::new(sub(b, budget)?)),
            Self::Rem(a, b) => Self::Rem(Box::new(sub(a, budget)?), Box::new(sub(b, budget)?)),
            Self::Sin(a) => Self::Sin(Box::new(sub(a, budget)?)),
            Self::Cos(a) => Self::Cos(Box::new(sub(a, budget)?)),
            Self::Tan(a) => Self::Tan(Box::new(sub(a, budget)?)),
            Self::Asin(a) => Self::Asin(Box::new(sub(a, budget)?)),
            Self::Acos(a) => Self::Acos(Box::new(sub(a, budget)?)),
            Self::Atan(a) => Self::Atan(Box::new(sub(a, budget)?)),
            Self::Atan2(a, b) => Self::Atan2(Box::new(sub(a, budget)?), Box::new(sub(b, budget)?)),
            Self::Pow(a, b) => Self::Pow(Box::new(sub(a, budget)?), Box::new(sub(b, budget)?)),
            Self::Sqrt(a) => Self::Sqrt(Box::new(sub(a, budget)?)),
            Self::Log(a, b) => Self::Log(
                Box::new(sub(a, budget)?),
                match b { Some(x) => Some(Box::new(sub(x, budget)?)), None => None },
            ),
            Self::Exp(a) => Self::Exp(Box::new(sub(a, budget)?)),
            Self::Abs(a) => Self::Abs(Box::new(sub(a, budget)?)),
            Self::Sign(a) => Self::Sign(Box::new(sub(a, budget)?)),
            Self::Progress(nc, v, s, e) => Self::Progress(*nc, Box::new(sub(v, budget)?), Box::new(sub(s, budget)?), Box::new(sub(e, budget)?)),
            Self::CalcMix(items) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items { out.push(CalcMixItem { value: sub(&it.value, budget)?, weight: it.weight.clone() }); }
                Self::CalcMix(out)
            }
            Self::Random(r) => Self::Random(Box::new(RandomNode {
                min: Box::new(sub(&r.min, budget)?),
                max: Box::new(sub(&r.max, budget)?),
                step: match &r.step { Some(x) => Some(Box::new(sub(x, budget)?)), None => None },
                ..(**r).clone()
            })),
        })
    }

    /// Replaces `Percent(p)` with `(p / 100) * SizeKeyword` for normalizing `%` `calc-size` bases to `100%`.
    #[must_use]
    pub fn replace_percent_with_size(&self) -> Self {
        let r = |n: &Self| n.replace_percent_with_size();
        let rv = |v: &[Self]| v.iter().map(r).collect::<Vec<_>>();
        match self {
            Self::Percent(p) => Self::Product(vec![Self::Number(*p / 100.0), Self::SizeKeyword]),
            Self::Number(_) | Self::Dimension(_, _) | Self::SizeKeyword | Self::SiblingIndex | Self::SiblingCount => self.clone(),
            Self::Negate(a) => Self::Negate(Box::new(r(a))),
            Self::Invert(a) => Self::Invert(Box::new(r(a))),
            Self::Sum(v) => Self::Sum(rv(v)),
            Self::Product(v) => Self::Product(rv(v)),
            Self::Min(v) => Self::Min(rv(v)),
            Self::Max(v) => Self::Max(rv(v)),
            Self::Hypot(v) => Self::Hypot(rv(v)),
            Self::Clamp(a, m, c) => Self::Clamp(a.as_deref().map(|x| Box::new(r(x))), Box::new(r(m)), c.as_deref().map(|x| Box::new(r(x)))),
            Self::Round(st, a, b) => Self::Round(*st, Box::new(r(a)), b.as_deref().map(|x| Box::new(r(x)))),
            Self::Mod(a, b) => Self::Mod(Box::new(r(a)), Box::new(r(b))),
            Self::Rem(a, b) => Self::Rem(Box::new(r(a)), Box::new(r(b))),
            Self::Sin(a) => Self::Sin(Box::new(r(a))),
            Self::Cos(a) => Self::Cos(Box::new(r(a))),
            Self::Tan(a) => Self::Tan(Box::new(r(a))),
            Self::Asin(a) => Self::Asin(Box::new(r(a))),
            Self::Acos(a) => Self::Acos(Box::new(r(a))),
            Self::Atan(a) => Self::Atan(Box::new(r(a))),
            Self::Atan2(a, b) => Self::Atan2(Box::new(r(a)), Box::new(r(b))),
            Self::Pow(a, b) => Self::Pow(Box::new(r(a)), Box::new(r(b))),
            Self::Sqrt(a) => Self::Sqrt(Box::new(r(a))),
            Self::Log(a, b) => Self::Log(Box::new(r(a)), b.as_deref().map(|x| Box::new(r(x)))),
            Self::Exp(a) => Self::Exp(Box::new(r(a))),
            Self::Abs(a) => Self::Abs(Box::new(r(a))),
            Self::Sign(a) => Self::Sign(Box::new(r(a))),
            Self::Progress(nc, v, s, e) => Self::Progress(*nc, Box::new(r(v)), Box::new(r(s)), Box::new(r(e))),
            Self::CalcMix(items) => Self::CalcMix(items.iter().map(|it| CalcMixItem { value: r(&it.value), weight: it.weight.clone() }).collect()),
            Self::Random(rn) => Self::Random(Box::new(RandomNode {
                min: Box::new(r(&rn.min)),
                max: Box::new(r(&rn.max)),
                step: rn.step.as_deref().map(|x| Box::new(r(x))),
                ..(**rn).clone()
            })),
        }
    }

    /// Evaluates this math expression with `size` bound to `size_px` (in CSS px).
    #[must_use]
    pub fn eval_with_size(&self, size_px: f64, ctx: &EvalContext, pct_basis: Option<(f64, bool)>) -> f64 {
        let mut budget = 4096usize;
        let repl = Self::Dimension(size_px, MathUnit::Length(LengthUnit::Px));
        let substituted = self.substitute_size(&repl, &mut budget).unwrap_or_else(|| self.clone());
        substituted.eval_scalar(ctx, pct_basis)
    }

    /// Serializes a simplified calculation tree without outer `calc(...)` wrapper.
    #[must_use]
    pub fn serialize_bare(&self) -> String {
        serialize_inner(self, false)
    }
}

#[rustfmt::skip]
impl CalcSizeBasis {
    #[must_use]
    pub fn serialize_specified(&self) -> String {
        match self {
            Self::Auto => "auto".to_string(),
            Self::MinContent => "min-content".to_string(),
            Self::MaxContent => "max-content".to_string(),
            Self::FitContent => "fit-content".to_string(),
            Self::Stretch => "stretch".to_string(),
            Self::Content => "content".to_string(),
            Self::Any => "any".to_string(),
            Self::Length(n) => serialize_inner(&n.simplify(None, None), false),
            Self::Nested(inner) => inner.serialize_specified(),
        }
    }

    #[must_use]
    pub fn serialize_computed(&self, ctx: &EvalContext) -> String {
        match self {
            Self::Auto => "auto".to_string(),
            Self::MinContent => "min-content".to_string(),
            Self::MaxContent => "max-content".to_string(),
            Self::FitContent => "fit-content".to_string(),
            Self::Stretch => "stretch".to_string(),
            Self::Content => "content".to_string(),
            Self::Any => "any".to_string(),
            Self::Length(n) => serialize_inner(&n.simplify(Some(ctx), None), false),
            Self::Nested(inner) => inner.serialize_computed(ctx),
        }
    }

    #[must_use]
    pub fn contains_sibling_fn(&self) -> bool {
        match self {
            Self::Length(n) => n.contains_sibling_fn(),
            Self::Nested(inner) => inner.contains_sibling_fn(),
            _ => false,
        }
    }

    #[must_use]
    pub fn has_percent_basis(&self) -> bool {
        match self {
            Self::Length(n) => n.has_percentage(),
            Self::Nested(inner) => inner.basis.has_percent_basis(),
            _ => false,
        }
    }

    #[must_use]
    pub fn is_definite(&self, parent_definite: bool) -> bool {
        match self {
            Self::Any => true,
            Self::Stretch => parent_definite,
            Self::Length(n) => !n.has_percentage() || parent_definite,
            Self::Nested(inner) => inner.basis.is_definite(parent_definite),
            Self::Auto | Self::MinContent | Self::MaxContent | Self::FitContent | Self::Content => false,
        }
    }
}

#[rustfmt::skip]
impl CalcSizeExpr {
    #[must_use]
    pub fn serialize_specified(&self) -> String {
        format!("calc-size({}, {})", self.basis.serialize_specified(), serialize_inner(&self.calculation.simplify(None, None), false))
    }

    #[must_use]
    pub fn serialize_computed(&self, ctx: &EvalContext) -> String {
        format!("calc-size({}, {})", self.basis.serialize_computed(ctx), serialize_inner(&self.calculation.simplify(Some(ctx), None), false))
    }

    #[must_use]
    pub fn contains_sibling_fn(&self) -> bool {
        self.basis.contains_sibling_fn() || self.calculation.contains_sibling_fn()
    }

    #[must_use]
    pub fn is_definite(&self, parent_definite: bool) -> bool {
        self.basis.is_definite(parent_definite)
    }

    /// Evaluates `calc-size()` to CSS pixels (clamped to `>= 0.0`).
    ///
    /// Per CSS Values L5 §10.10.2:
    /// - If the basis contains a percentage and `avail_opt` is `None` (indefinite), the entire
    ///   `calc-size()` behaves as `auto` (`intrinsic(CalcSizeBasis::Auto)`).
    /// - Percentages inside `calculation` resolve against `avail_opt.unwrap_or(0.0)`.
    #[must_use]
    pub fn eval_px<F>(&self, avail_opt: Option<f32>, eval: &EvalContext, intrinsic: &F) -> f32
    where
        F: Fn(&CalcSizeBasis) -> f32,
    {
        if avail_opt.is_none() && self.basis.has_percent_basis() {
            return intrinsic(&CalcSizeBasis::Auto).max(0.0);
        }
        let basis_px = match &self.basis {
            CalcSizeBasis::Any => 0.0,
            CalcSizeBasis::Length(node) => {
                let v = node.eval_scalar(eval, Some((f64::from(avail_opt.unwrap_or(0.0)), false)));
                if v.is_nan() { 0.0 } else { v.clamp(-MAX_CSS_PX, MAX_CSS_PX) }
            }
            CalcSizeBasis::Nested(inner) => f64::from(inner.eval_px(avail_opt, eval, intrinsic)),
            kw => f64::from(intrinsic(kw)),
        };
        let pct_basis = f64::from(avail_opt.unwrap_or(0.0));
        let v = self.calculation.eval_with_size(basis_px, eval, Some((pct_basis, false)));
        if v.is_nan() { 0.0 } else { v.clamp(0.0, MAX_CSS_PX) as f32 }
    }

    /// Flattens nested `calc-size()` and normalizes `<length-percentage>` bases for interpolation
    /// per CSS Values L5 §10.10.3, enforcing nesting depth & `max_nodes` expansion caps (CSSWG #10369).
    #[must_use]
    pub fn normalize_for_interpolation(&self, ctx: &EvalContext, max_nodes: usize) -> Option<(CalcSizeBasis, MathNode)> {
        self.normalize_for_interpolation_depth(ctx, max_nodes, 0)
    }

    fn normalize_for_interpolation_depth(&self, ctx: &EvalContext, max_nodes: usize, depth: usize) -> Option<(CalcSizeBasis, MathNode)> {
        if depth > 16 { return None; }
        let calc_simp = self.calculation.simplify(Some(ctx), None);
        match &self.basis {
            CalcSizeBasis::Nested(inner) => {
                let (ib, ic) = inner.normalize_for_interpolation_depth(ctx, max_nodes, depth + 1)?;
                let mut budget = max_nodes;
                let sub = calc_simp.substitute_size(&ic, &mut budget)?;
                let simp = sub.simplify(Some(ctx), None);
                if simp.node_count() > max_nodes { return None; }
                Some((ib, simp))
            }
            CalcSizeBasis::Length(bnode) => {
                let bs = bnode.simplify(Some(ctx), None);
                if !bs.has_percentage() {
                    let mut budget = max_nodes;
                    let sub = calc_simp.substitute_size(&bs, &mut budget)?;
                    let simp = sub.simplify(Some(ctx), None);
                    if simp.node_count() > max_nodes { return None; }
                    Some((CalcSizeBasis::Any, simp))
                } else {
                    let bs_as_size = bs.replace_percent_with_size().simplify(Some(ctx), None);
                    let mut budget = max_nodes;
                    let sub = calc_simp.substitute_size(&bs_as_size, &mut budget)?;
                    let simp = sub.simplify(Some(ctx), None);
                    if simp.node_count() > max_nodes { return None; }
                    Some((CalcSizeBasis::Length(Box::new(MathNode::Percent(100.0))), simp))
                }
            }
            other => Some((other.clone(), calc_simp)),
        }
    }
}

/// Strictly parses CSS Values L5 `calc-size(<calc-size-basis>, <calc-size-calculation>)`.
#[must_use]
pub fn parse_calc_size(
    input: &str,
    allow_auto: bool,
    allow_content: bool,
) -> Option<(CalcSizeExpr, String)> {
    let mut pi = ParserInput::new(input);
    let mut p = Parser::new(&mut pi);
    let expr = parse_calc_size_from_parser(&mut p, allow_auto, allow_content)?;
    if !p.is_exhausted() {
        return None;
    }
    let spec = expr.serialize_specified();
    Some((expr, spec))
}

#[rustfmt::skip]
fn parse_calc_size_from_parser<'i, 't>(p: &mut Parser<'i, 't>, allow_auto: bool, allow_content: bool) -> Option<CalcSizeExpr> {
    let state = p.state();
    let Token::Function(fname) = p.next().ok()?.clone() else { p.reset(&state); return None; };
    if !fname.eq_ignore_ascii_case("calc-size") { p.reset(&state); return None; }
    let res = p.parse_nested_block(|n| -> Result<CalcSizeExpr, cssparser::ParseError<'i, ()>> {
        let Some(basis) = parse_calc_size_basis(n, allow_auto, allow_content) else { return Err(n.new_custom_error(())); };
        n.expect_comma()?;
        let allow_size = !matches!(basis, CalcSizeBasis::Any);
        let Some(calc_node) = parse_sum_opt(n, allow_size) else { return Err(n.new_custom_error(())); };
        if !n.is_exhausted() { return Err(n.new_custom_error(())); }
        let Some(ct) = calc_node.calc_type() else { return Err(n.new_custom_error(())); };
        if !ct.matches_target(ValueKind::LengthPercentage) { return Err(n.new_custom_error(())); }
        Ok(CalcSizeExpr { basis, calculation: Box::new(calc_node.simplify(None, None)) })
    }).ok();
    if res.is_none() { p.reset(&state); }
    res
}

#[rustfmt::skip]
fn parse_calc_size_basis<'i, 't>(p: &mut Parser<'i, 't>, allow_auto: bool, allow_content: bool) -> Option<CalcSizeBasis> {
    let st = p.state();
    if let Ok(ident) = p.try_parse(|p| p.expect_ident().map(|s| s.to_ascii_lowercase())) {
        let kw = match ident.as_str() {
            "any" => Some(CalcSizeBasis::Any),
            "auto" if allow_auto => Some(CalcSizeBasis::Auto),
            "min-content" => Some(CalcSizeBasis::MinContent),
            "max-content" => Some(CalcSizeBasis::MaxContent),
            "fit-content" => Some(CalcSizeBasis::FitContent),
            "stretch" => Some(CalcSizeBasis::Stretch),
            "content" if allow_content => Some(CalcSizeBasis::Content),
            _ => None,
        };
        if let Some(b) = kw { return Some(b); }
        p.reset(&st);
    }
    if let Some(nested) = parse_calc_size_from_parser(p, allow_auto, allow_content) {
        return Some(CalcSizeBasis::Nested(Box::new(nested)));
    }
    let node = parse_sum_opt(p, false)?;
    let ct = node.calc_type()?;
    if !ct.matches_target(ValueKind::LengthPercentage) { p.reset(&st); return None; }
    Some(CalcSizeBasis::Length(Box::new(node.simplify(None, None))))
}

fn escape_css_url_string(u: &str) -> String {
    let mut out = String::with_capacity(u.len() + 7);
    out.push_str("url(\"");
    for ch in u.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out.push_str("\")");
    out
}

fn math_node_to_length(pm: &ParsedMath) -> LengthPercentage {
    if !pm.was_math_fn {
        if let MathNode::Dimension(v, MathUnit::Length(LengthUnit::Px)) = pm.node {
            return LengthPercentage::px(v as f32);
        }
    }
    LengthPercentage::Calc(Box::new(crate::values::CalcExpr::Math(Box::new(
        pm.node.clone(),
    ))))
}

/// Parses a CSS Filter Effects Level 1/2 `<filter-value-list> | none` declaration
/// (supporting both `filter` and `backdrop-filter`, omitted default arguments, `url()`,
/// `drop-shadow()`, and full CSS Values L4/L5 math expressions such as `calc(sign(...))`).
#[must_use]
pub fn parse_css_filter_list(input: &str) -> Option<(Vec<crate::values::FilterSpecOp>, String)> {
    use crate::values::{FilterScalar, FilterSpecOp};

    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.eq_ignore_ascii_case("none") {
        return Some((Vec::new(), "none".to_string()));
    }

    let mut pi = ParserInput::new(trimmed);
    let mut p = Parser::new(&mut pi);
    let mut ops: Vec<FilterSpecOp> = Vec::new();
    let mut parts: Vec<String> = Vec::new();

    while !p.is_exhausted() {
        let tok = p.next().ok()?.clone();
        match tok {
            Token::UnquotedUrl(u) => {
                let s = u.as_ref();
                parts.push(escape_css_url_string(s));
                ops.push(FilterSpecOp::Url(s.to_string()));
            }
            Token::Function(fname) => {
                let lower = fname.to_ascii_lowercase();
                let parsed: Option<(FilterSpecOp, String)> = p
                    .parse_nested_block(|n| -> Result<(FilterSpecOp, String), cssparser::ParseError<'_, ()>> {
                        match lower.as_str() {
                            "url" => {
                                let s = n.expect_string()?.as_ref().to_string();
                                if !n.is_exhausted() {
                                    return Err(n.new_custom_error(()));
                                }
                                Ok((
                                    FilterSpecOp::Url(s.clone()),
                                    escape_css_url_string(&s),
                                ))
                            }
                            "blur" => {
                                if n.is_exhausted() {
                                    return Ok((
                                        FilterSpecOp::Blur(LengthPercentage::px(0.0)),
                                        "blur(0px)".to_string(),
                                    ));
                                }
                                let Some(pm) = parse_math_value_from_parser(n, ValueKind::Length) else {
                                    return Err(n.new_custom_error(()));
                                };
                                if !n.is_exhausted() {
                                    return Err(n.new_custom_error(()));
                                }
                                if !pm.was_math_fn {
                                    let v = pm.node.eval_scalar(&EvalContext::default(), None);
                                    if !v.is_finite() || v < 0.0 {
                                        return Err(n.new_custom_error(()));
                                    }
                                }
                                let spec = pm.node.serialize_specified(pm.was_math_fn);
                                Ok((FilterSpecOp::Blur(math_node_to_length(&pm)), format!("blur({spec})")))
                            }
                            "brightness" | "contrast" | "saturate" | "grayscale" | "invert" | "sepia" | "opacity" => {
                                let clamped_unit = matches!(lower.as_str(), "grayscale" | "invert" | "sepia" | "opacity");
                                if n.is_exhausted() {
                                    let op = match lower.as_str() {
                                        "brightness" => FilterSpecOp::Brightness(FilterScalar::Value(1.0)),
                                        "contrast" => FilterSpecOp::Contrast(FilterScalar::Value(1.0)),
                                        "saturate" => FilterSpecOp::Saturate(FilterScalar::Value(1.0)),
                                        "grayscale" => FilterSpecOp::Grayscale(FilterScalar::Value(1.0)),
                                        "invert" => FilterSpecOp::Invert(FilterScalar::Value(1.0)),
                                        "sepia" => FilterSpecOp::Sepia(FilterScalar::Value(1.0)),
                                        _ => FilterSpecOp::Opacity(FilterScalar::Value(1.0)),
                                    };
                                    return Ok((op, format!("{lower}(1)")));
                                }
                                let Some(pm) = parse_math_value_from_parser(n, ValueKind::NumberOrPercentage) else {
                                    return Err(n.new_custom_error(()));
                                };
                                if !n.is_exhausted() {
                                    return Err(n.new_custom_error(()));
                                }
                                let (scalar, spec_arg) = if !pm.was_math_fn {
                                    match pm.node {
                                        MathNode::Number(v) => {
                                            if !v.is_finite() || v < 0.0 {
                                                return Err(n.new_custom_error(()));
                                            }
                                            let cv = if clamped_unit { v.min(1.0) } else { v };
                                            (FilterScalar::Value(cv as f32), format_math_f64(cv))
                                        }
                                        MathNode::Percent(pct) => {
                                            if !pct.is_finite() || pct < 0.0 {
                                                return Err(n.new_custom_error(()));
                                            }
                                            let cpct = if clamped_unit { pct.min(100.0) } else { pct };
                                            (
                                                FilterScalar::Value((cpct / 100.0) as f32),
                                                format!("{}%", format_math_f64(cpct)),
                                            )
                                        }
                                        _ => return Err(n.new_custom_error(())),
                                    }
                                } else {
                                    let spec = pm.node.serialize_specified(true);
                                    (FilterScalar::Math(Box::new(pm.node)), spec)
                                };
                                let op = match lower.as_str() {
                                    "brightness" => FilterSpecOp::Brightness(scalar),
                                    "contrast" => FilterSpecOp::Contrast(scalar),
                                    "saturate" => FilterSpecOp::Saturate(scalar),
                                    "grayscale" => FilterSpecOp::Grayscale(scalar),
                                    "invert" => FilterSpecOp::Invert(scalar),
                                    "sepia" => FilterSpecOp::Sepia(scalar),
                                    _ => FilterSpecOp::Opacity(scalar),
                                };
                                Ok((op, format!("{lower}({spec_arg})")))
                            }
                            "hue-rotate" => {
                                if n.is_exhausted() {
                                    return Ok((
                                        FilterSpecOp::HueRotateDeg(FilterScalar::Value(0.0)),
                                        "hue-rotate(0deg)".to_string(),
                                    ));
                                }
                                let st = n.state();
                                let start_pos = n.position();
                                if let Ok(tok) = n.next().cloned() {
                                    match tok {
                                        Token::Number { value, .. } if value == 0.0 && n.is_exhausted() => {
                                            return Ok((
                                                FilterSpecOp::HueRotateDeg(FilterScalar::Value(0.0)),
                                                "hue-rotate(0deg)".to_string(),
                                            ));
                                        }
                                        Token::Dimension { value, unit, .. } if n.is_exhausted() => {
                                            let unit_low = unit.to_ascii_lowercase();
                                            let v = exact_f64(n.slice_from(start_pos), unit.len(), f64::from(value));
                                            let deg = match unit_low.as_str() {
                                                "deg" => Some(v),
                                                "grad" => Some(v * 0.9),
                                                "rad" => Some(v * (180.0 / std::f64::consts::PI)),
                                                "turn" => Some(v * 360.0),
                                                _ => None,
                                            };
                                            if let Some(d) = deg {
                                                return Ok((
                                                    FilterSpecOp::HueRotateDeg(FilterScalar::Value(d as f32)),
                                                    format!("hue-rotate({}{unit_low})", format_math_f64(v)),
                                                ));
                                            }
                                            return Err(n.new_custom_error(()));
                                        }
                                        _ => n.reset(&st),
                                    }
                                } else {
                                    n.reset(&st);
                                }
                                let Some(pm) = parse_math_value_from_parser(n, ValueKind::Angle) else {
                                    return Err(n.new_custom_error(()));
                                };
                                if !n.is_exhausted() {
                                    return Err(n.new_custom_error(()));
                                }
                                let spec = pm.node.serialize_specified(pm.was_math_fn);
                                Ok((
                                    FilterSpecOp::HueRotateDeg(FilterScalar::Math(Box::new(pm.node))),
                                    format!("hue-rotate({spec})"),
                                ))
                            }
                            "drop-shadow" => {
                                let mut color_expr: Option<Box<crate::color::CssColorExpr>> = None;
                                let mut color_ser: Option<String> = None;
                                let mut lengths: Vec<(LengthPercentage, String, bool, f64)> = Vec::new();
                                while !n.is_exhausted() {
                                    let st = n.state();
                                    if lengths.len() < 3 {
                                        if let Some(pm) = parse_math_value_from_parser(n, ValueKind::Length) {
                                            let raw_v = if !pm.was_math_fn {
                                                pm.node.eval_scalar(&EvalContext::default(), None)
                                            } else {
                                                0.0
                                            };
                                            let spec = pm.node.serialize_specified(pm.was_math_fn);
                                            lengths.push((math_node_to_length(&pm), spec, pm.was_math_fn, raw_v));
                                            continue;
                                        }
                                        n.reset(&st);
                                    }
                                    if color_expr.is_some() || (!lengths.is_empty() && lengths.len() < 2) {
                                        return Err(n.new_custom_error(()));
                                    }
                                    let c_start = n.position();
                                    let c_tok = n.next()?.clone();
                                    if matches!(c_tok, Token::Function(_)) {
                                        let _ = n.parse_nested_block(|inner| -> Result<(), cssparser::ParseError<'_, ()>> {
                                            while inner.next().is_ok() {}
                                            Ok(())
                                        });
                                    }
                                    let slice = n.slice_from(c_start).trim();
                                    let Some(ce) = crate::color::parse_css_color(slice) else {
                                        return Err(n.new_custom_error(()));
                                    };
                                    let Some(cs) = crate::color::parse_and_serialize_color_specified(slice) else {
                                        return Err(n.new_custom_error(()));
                                    };
                                    color_expr = Some(Box::new(ce));
                                    color_ser = Some(cs);
                                }
                                if lengths.len() < 2 || lengths.len() > 3 {
                                    return Err(n.new_custom_error(()));
                                }
                                // W3C Filter Effects L1 §9.2: negative literal blur radius is invalid.
                                if let Some((_, _, was_fn, raw_v)) = lengths.get(2) {
                                    if !*was_fn && (!raw_v.is_finite() || *raw_v < 0.0) {
                                        return Err(n.new_custom_error(()));
                                    }
                                }
                                let mut ds_parts: Vec<String> = Vec::with_capacity(4);
                                if let Some(cs) = color_ser {
                                    ds_parts.push(cs);
                                }
                                ds_parts.push(lengths[0].1.clone());
                                ds_parts.push(lengths[1].1.clone());
                                let blur_lp = if let Some((lp, sp, _, _)) = lengths.get(2) {
                                    ds_parts.push(sp.clone());
                                    lp.clone()
                                } else {
                                    LengthPercentage::px(0.0)
                                };
                                Ok((
                                    FilterSpecOp::DropShadow {
                                        offset_x: lengths[0].0.clone(),
                                        offset_y: lengths[1].0.clone(),
                                        blur: blur_lp,
                                        color: color_expr,
                                    },
                                    format!("drop-shadow({})", ds_parts.join(" ")),
                                ))
                            }
                            _ => Err(n.new_custom_error(())),
                        }
                    })
                    .ok();
                let (op, ser) = parsed?;
                ops.push(op);
                parts.push(ser);
            }
            _ => return None,
        }
    }

    if ops.is_empty() {
        return None;
    }
    Some((ops, parts.join(" ")))
}
