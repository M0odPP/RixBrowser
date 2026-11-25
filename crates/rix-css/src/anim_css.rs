//! W3C CSS Animations Level 1/2, Scroll-Driven Animations Level 1 (`animation-range*`,
//! `animation-timeline`, `animation-duration: auto`), CSS Transitions Level 2 shorthands,
//! `text-shadow`, `text-decoration-inset`, and `vertical-align` Values L4 parsing & serialization.

use cssparser::{Parser, ParserInput, Token};
use rix_core::{intern_str, InternedStr};
use serde::{Deserialize, Serialize};

use crate::color::{
    parse_and_serialize_color_specified, parse_css_color, CssColorExpr, ResolvedColor,
};
use crate::properties::{parse_single_timing_function, GlobalKeyword};
use crate::substitution::{escape_css_ident, quote_css_string};
use crate::values::{
    AnimationCompositionMode, AnimationDirectionMode, AnimationFillModeKind,
    AnimationPlayStateMode, AnimationSpec, BoxShadowValue, CalcExpr, ColorRgba, CssColor,
    EvalContext, LengthPercentage, LengthUnit, TimingFunction, TransitionBehaviorMode,
    TransitionSpec, VerticalAlignMode,
};
use crate::values_math::{
    format_math_f64, parse_math_value, parse_math_value_from_parser, MathNode, MathUnit, ValueKind,
};

/// Scroll-Driven Animations Level 1 named timeline range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum TimelineRangeName {
    Cover,
    Contain,
    Entry,
    Exit,
    EntryCrossing,
    ExitCrossing,
}

impl TimelineRangeName {
    #[must_use]
    pub fn parse_ident(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "cover" => Some(Self::Cover),
            "contain" => Some(Self::Contain),
            "entry" => Some(Self::Entry),
            "exit" => Some(Self::Exit),
            "entry-crossing" => Some(Self::EntryCrossing),
            "exit-crossing" => Some(Self::ExitCrossing),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cover => "cover",
            Self::Contain => "contain",
            Self::Entry => "entry",
            Self::Exit => "exit",
            Self::EntryCrossing => "entry-crossing",
            Self::ExitCrossing => "exit-crossing",
        }
    }
}

/// Single endpoint for `animation-range-start` / `animation-range-end`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum AnimationRangePoint {
    #[default]
    Normal,
    Offset {
        value: LengthPercentage,
        was_math_fn: bool,
    },
    Named {
        name: TimelineRangeName,
        offset: Option<(LengthPercentage, bool)>,
    },
}

impl AnimationRangePoint {
    fn is_default_pct(
        &self,
        target_pct: f32,
        eval: Option<&EvalContext>,
    ) -> Option<TimelineRangeName> {
        let Self::Named { name, offset } = self else {
            return None;
        };
        match offset {
            None => Some(*name),
            Some((lp, was_fn)) => {
                if eval.is_none() && *was_fn {
                    return None;
                }
                if lp_is_exact_percent(lp, target_pct, eval) {
                    Some(*name)
                } else {
                    None
                }
            }
        }
    }

    #[must_use]
    pub fn serialize(&self, is_end: bool, eval: Option<&EvalContext>) -> String {
        let default_pct = if is_end { 100.0 } else { 0.0 };
        match self {
            Self::Normal => "normal".to_string(),
            Self::Offset { value, was_math_fn } => serialize_range_lp(value, *was_math_fn, eval),
            Self::Named { name, offset } => match offset {
                None => name.as_str().to_string(),
                Some((lp, was_fn)) => {
                    let omit = if eval.is_some() {
                        lp_is_exact_percent(lp, default_pct, eval)
                    } else {
                        !*was_fn && lp_is_exact_percent(lp, default_pct, None)
                    };
                    if omit {
                        name.as_str().to_string()
                    } else {
                        format!(
                            "{} {}",
                            name.as_str(),
                            serialize_range_lp(lp, *was_fn, eval)
                        )
                    }
                }
            },
        }
    }
}

fn lp_is_exact_percent(lp: &LengthPercentage, pct: f32, eval: Option<&EvalContext>) -> bool {
    match lp {
        LengthPercentage::Percent(p) => (*p * 100.0 - pct).abs() < 1e-4,
        LengthPercentage::Calc(c) => {
            if let CalcExpr::Math(node) = c.as_ref() {
                if let Some(ctx) = eval {
                    let simp = node.simplify(Some(ctx), None);
                    return matches!(simp, MathNode::Percent(v) if (v as f32 - pct).abs() < 1e-4);
                }
            }
            false
        }
        _ => false,
    }
}

fn serialize_range_lp(
    lp: &LengthPercentage,
    was_math_fn: bool,
    eval: Option<&EvalContext>,
) -> String {
    match lp {
        LengthPercentage::Zero => "0px".to_string(),
        LengthPercentage::Length(v, unit) => match eval {
            Some(ctx) => format!("{}px", format_math_f64(f64::from(lp.resolve_px(0.0, ctx)))),
            None => format!("{}{}", format_math_f64(f64::from(*v)), unit_suffix(*unit)),
        },
        LengthPercentage::Percent(p) => format!("{}%", format_math_f64(f64::from(*p * 100.0))),
        LengthPercentage::Calc(c) => match c.as_ref() {
            CalcExpr::Math(node) => {
                if let Some(ctx) = eval {
                    node.serialize_computed(ctx, None, ValueKind::LengthPercentage, None, None)
                } else {
                    node.serialize_specified(was_math_fn)
                }
            }
            _ => "0px".to_string(),
        },
    }
}

fn unit_suffix(u: LengthUnit) -> &'static str {
    match u {
        LengthUnit::Px => "px",
        LengthUnit::Em => "em",
        LengthUnit::Rem => "rem",
        LengthUnit::Vw => "vw",
        LengthUnit::Vh => "vh",
        LengthUnit::Vmin => "vmin",
        LengthUnit::Vmax => "vmax",
        LengthUnit::Ch => "ch",
        LengthUnit::Ex => "ex",
        _ => "px",
    }
}

fn parsed_math_to_lp(pm: &crate::values_math::ParsedMath) -> LengthPercentage {
    if !pm.was_math_fn {
        match pm.node {
            MathNode::Dimension(v, MathUnit::Length(u)) => {
                return LengthPercentage::Length(v as f32, u)
            }
            MathNode::Percent(p) => return LengthPercentage::Percent((p / 100.0) as f32),
            _ => {}
        }
    }
    LengthPercentage::Calc(Box::new(CalcExpr::Math(Box::new(pm.node.clone()))))
}

fn parse_single_range_point(p: &mut Parser<'_, '_>) -> Option<AnimationRangePoint> {
    let st = p.state();
    if let Ok(ident) = p.try_parse(|p| p.expect_ident().map(|s| s.to_string())) {
        let low = ident.to_ascii_lowercase();
        if low == "normal" {
            return Some(AnimationRangePoint::Normal);
        }
        if let Some(name) = TimelineRangeName::parse_ident(&low) {
            let st2 = p.state();
            if let Some(pm) = parse_math_value_from_parser(p, ValueKind::LengthPercentage) {
                return Some(AnimationRangePoint::Named {
                    name,
                    offset: Some((parsed_math_to_lp(&pm), pm.was_math_fn)),
                });
            }
            p.reset(&st2);
            return Some(AnimationRangePoint::Named { name, offset: None });
        }
        p.reset(&st);
        return None;
    }
    if let Some(pm) = parse_math_value_from_parser(p, ValueKind::LengthPercentage) {
        return Some(AnimationRangePoint::Offset {
            value: parsed_math_to_lp(&pm),
            was_math_fn: pm.was_math_fn,
        });
    }
    p.reset(&st);
    None
}

/// Parses `animation-range-start` or `animation-range-end` comma-separated list.
#[must_use]
pub fn parse_animation_range_point_list(
    raw: &str,
    is_end: bool,
) -> Option<(Vec<AnimationRangePoint>, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut pi = ParserInput::new(trimmed);
    let mut p = Parser::new(&mut pi);
    let mut list = Vec::new();
    loop {
        let pt = parse_single_range_point(&mut p)?;
        list.push(pt);
        if p.is_exhausted() {
            break;
        }
        p.expect_comma().ok()?;
    }
    if list.is_empty() || !p.is_exhausted() {
        return None;
    }
    let ser = list
        .iter()
        .map(|pt| pt.serialize(is_end, None))
        .collect::<Vec<_>>()
        .join(", ");
    Some((list, ser))
}

/// Parses `animation-range` shorthand into `(starts, ends, serialized_shorthand)`.
#[must_use]
pub fn parse_animation_range_shorthand(
    raw: &str,
) -> Option<(Vec<AnimationRangePoint>, Vec<AnimationRangePoint>, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut pi = ParserInput::new(trimmed);
    let mut p = Parser::new(&mut pi);
    let mut starts = Vec::new();
    let mut ends = Vec::new();
    loop {
        let start = parse_single_range_point(&mut p)?;
        let st = p.state();
        let end = if p.is_exhausted() || matches!(p.next(), Ok(Token::Comma)) {
            p.reset(&st);
            match &start {
                AnimationRangePoint::Named { name, .. } => AnimationRangePoint::Named {
                    name: *name,
                    offset: None,
                },
                _ => AnimationRangePoint::Normal,
            }
        } else {
            p.reset(&st);
            parse_single_range_point(&mut p)?
        };
        starts.push(start);
        ends.push(end);
        if p.is_exhausted() {
            break;
        }
        p.expect_comma().ok()?;
    }
    if starts.is_empty() || !p.is_exhausted() {
        return None;
    }
    let ser = serialize_animation_range_shorthand(&starts, &ends, None);
    Some((starts, ends, ser))
}

#[must_use]
pub fn serialize_animation_range_shorthand(
    starts: &[AnimationRangePoint],
    ends: &[AnimationRangePoint],
    eval: Option<&EvalContext>,
) -> String {
    let len = starts.len().max(ends.len()).max(1);
    let mut parts = Vec::with_capacity(len);
    for i in 0..len {
        let s = starts
            .get(i)
            .cloned()
            .unwrap_or(AnimationRangePoint::Normal);
        let e = ends.get(i).cloned().unwrap_or(AnimationRangePoint::Normal);
        let s_str = s.serialize(false, eval);
        let e_str = e.serialize(true, eval);
        let omit_end = match (&s, &e) {
            (AnimationRangePoint::Normal, AnimationRangePoint::Normal) => true,
            (AnimationRangePoint::Offset { .. }, AnimationRangePoint::Normal) => true,
            (
                AnimationRangePoint::Offset { .. },
                AnimationRangePoint::Offset { value, was_math_fn },
            ) => (eval.is_some() || !was_math_fn) && lp_is_exact_percent(value, 100.0, eval),
            (AnimationRangePoint::Named { name: sn, .. }, _) => {
                e.is_default_pct(100.0, eval) == Some(*sn)
            }
            _ => false,
        };
        if omit_end {
            parts.push(s_str);
        } else {
            parts.push(format!("{s_str} {e_str}"));
        }
    }
    parts.join(", ")
}

/// Specified `text-shadow` item prior to `EvalContext` length resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct TextShadowSpec {
    pub offset_x: LengthPercentage,
    pub offset_y: LengthPercentage,
    pub blur: LengthPercentage,
    pub color: Option<Box<CssColorExpr>>,
}

impl TextShadowSpec {
    #[must_use]
    pub fn resolve(
        &self,
        eval: &EvalContext,
        current_color: ColorRgba,
        dark_mode: bool,
    ) -> BoxShadowValue {
        let color = match self.color.as_deref() {
            None | Some(CssColorExpr::CurrentColor) => CssColor::CurrentColor,
            Some(expr) => CssColor::Rgba(
                expr.resolve(ResolvedColor::from_rgba(current_color), dark_mode, eval)
                    .to_srgb_u8(),
            ),
        };
        BoxShadowValue {
            offset_x: LengthPercentage::px(self.offset_x.resolve_px(0.0, eval)),
            offset_y: LengthPercentage::px(self.offset_y.resolve_px(0.0, eval)),
            blur: LengthPercentage::px(self.blur.resolve_px(0.0, eval).max(0.0)),
            spread: LengthPercentage::Zero,
            color,
            inset: false,
        }
    }
}

/// Parses W3C CSS Text Decor L3 `text-shadow` (`none | [ <color>? && <length>{2,3} ]#`).
#[must_use]
pub fn parse_text_shadow_list(raw: &str) -> Option<(Vec<TextShadowSpec>, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.eq_ignore_ascii_case("none") {
        return Some((Vec::new(), "none".to_string()));
    }
    let mut pi = ParserInput::new(trimmed);
    let mut p = Parser::new(&mut pi);
    let mut out = Vec::new();
    let mut ser_parts = Vec::new();
    loop {
        let mut color_expr: Option<Box<CssColorExpr>> = None;
        let mut color_ser: Option<String> = None;
        let mut lengths: Vec<(LengthPercentage, String, bool, f64)> = Vec::new();
        while !p.is_exhausted() {
            let st = p.state();
            if matches!(p.next(), Ok(Token::Comma)) {
                p.reset(&st);
                break;
            }
            p.reset(&st);
            if lengths.len() < 3 {
                if let Some(pm) = parse_math_value_from_parser(&mut p, ValueKind::Length) {
                    let raw_v = if !pm.was_math_fn {
                        pm.node.eval_scalar(&EvalContext::default(), None)
                    } else {
                        0.0
                    };
                    let spec = pm.node.serialize_specified(pm.was_math_fn);
                    lengths.push((parsed_math_to_lp(&pm), spec, pm.was_math_fn, raw_v));
                    continue;
                }
                p.reset(&st);
            }
            if color_expr.is_some() || (!lengths.is_empty() && lengths.len() < 2) {
                return None;
            }
            let c_start = p.position();
            let c_tok = p.next().ok()?.clone();
            if matches!(c_tok, Token::Function(_)) {
                let _ =
                    p.parse_nested_block(|inner| -> Result<(), cssparser::ParseError<'_, ()>> {
                        while inner.next().is_ok() {}
                        Ok(())
                    });
            }
            let slice = p.slice_from(c_start).trim();
            let ce = parse_css_color(slice)?;
            let cs = parse_and_serialize_color_specified(slice)?;
            color_expr = Some(Box::new(ce));
            color_ser = Some(cs);
        }
        if lengths.len() < 2 || lengths.len() > 3 {
            return None;
        }
        if let Some((_, _, was_fn, raw_v)) = lengths.get(2) {
            if !*was_fn && (!raw_v.is_finite() || *raw_v < 0.0) {
                return None;
            }
        }
        let mut item_parts = Vec::with_capacity(4);
        if let Some(cs) = color_ser {
            item_parts.push(cs);
        }
        item_parts.push(lengths[0].1.clone());
        item_parts.push(lengths[1].1.clone());
        let blur_lp = if let Some((lp, sp, _, _)) = lengths.get(2) {
            item_parts.push(sp.clone());
            lp.clone()
        } else {
            LengthPercentage::px(0.0)
        };
        ser_parts.push(item_parts.join(" "));
        out.push(TextShadowSpec {
            offset_x: lengths[0].0.clone(),
            offset_y: lengths[1].0.clone(),
            blur: blur_lp,
            color: color_expr,
        });
        if p.is_exhausted() {
            break;
        }
        p.expect_comma().ok()?;
    }
    if out.is_empty() || !p.is_exhausted() {
        return None;
    }
    Some((out, ser_parts.join(", ")))
}

/// CSS Text Decoration Level 4 `text-decoration-inset` (`auto | <length-percentage>{1,2}`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum TextDecorationInsetValue {
    #[default]
    Auto,
    Lengths(LengthPercentage, LengthPercentage),
}

#[must_use]
pub fn parse_text_decoration_inset(raw: &str) -> Option<(TextDecorationInsetValue, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.eq_ignore_ascii_case("auto") {
        return Some((TextDecorationInsetValue::Auto, "auto".to_string()));
    }
    let mut pi = ParserInput::new(trimmed);
    let mut p = Parser::new(&mut pi);
    let first = parse_math_value_from_parser(&mut p, ValueKind::LengthPercentage)?;
    let first_lp = parsed_math_to_lp(&first);
    let first_s = first.node.serialize_specified(first.was_math_fn);
    if p.is_exhausted() {
        return Some((
            TextDecorationInsetValue::Lengths(first_lp.clone(), first_lp),
            first_s,
        ));
    }
    let second = parse_math_value_from_parser(&mut p, ValueKind::LengthPercentage)?;
    if !p.is_exhausted() {
        return None;
    }
    let second_lp = parsed_math_to_lp(&second);
    let second_s = second.node.serialize_specified(second.was_math_fn);
    let ser = if first_s == second_s {
        first_s
    } else {
        format!("{first_s} {second_s}")
    };
    Some((TextDecorationInsetValue::Lengths(first_lp, second_lp), ser))
}

#[must_use]
pub fn serialize_text_decoration_inset_computed(
    val: &TextDecorationInsetValue,
    eval: &EvalContext,
) -> String {
    match val {
        TextDecorationInsetValue::Auto => "auto".to_string(),
        TextDecorationInsetValue::Lengths(a, b) => {
            let sa = serialize_range_lp(a, false, Some(eval));
            let sb = serialize_range_lp(b, false, Some(eval));
            if sa == sb {
                sa
            } else {
                format!("{sa} {sb}")
            }
        }
    }
}

/// Parses `vertical-align` (`baseline | top | middle | bottom | text-top | text-bottom | sub | super | <length-percentage>`).
#[must_use]
pub fn parse_vertical_align_values4(
    raw: &str,
) -> Option<(VerticalAlignMode, Option<LengthPercentage>, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let low = trimmed.to_ascii_lowercase();
    let kw = match low.as_str() {
        "baseline" => Some(VerticalAlignMode::Baseline),
        "top" => Some(VerticalAlignMode::Top),
        "middle" => Some(VerticalAlignMode::Middle),
        "bottom" => Some(VerticalAlignMode::Bottom),
        "text-top" => Some(VerticalAlignMode::TextTop),
        "text-bottom" => Some(VerticalAlignMode::TextBottom),
        "sub" => Some(VerticalAlignMode::Sub),
        "super" => Some(VerticalAlignMode::Super),
        _ => None,
    };
    if let Some(m) = kw {
        return Some((m, None, low));
    }
    let pm = parse_math_value(trimmed, ValueKind::LengthPercentage)?;
    let lp = parsed_math_to_lp(&pm);
    let px = lp.resolve_px(0.0, &EvalContext::default());
    let spec = pm.node.serialize_specified(pm.was_math_fn);
    Some((VerticalAlignMode::LengthPx(px), Some(lp), spec))
}

fn is_restricted_animation_keyword(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "none" | "initial" | "inherit" | "unset" | "revert" | "revert-layer" | "default"
    )
}

/// Serializes an `animation-name` token (`none`, identifier, escaped identifier, or quoted restricted keyword).
#[must_use]
pub fn serialize_animation_name_token(raw_stored: &str) -> String {
    if let Some(inner) = raw_stored
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
    {
        if is_restricted_animation_keyword(inner) {
            return quote_css_string(inner);
        }
        return escape_css_ident(inner);
    }
    if raw_stored.eq_ignore_ascii_case("none") {
        return "none".to_string();
    }
    escape_css_ident(raw_stored)
}

/// Parses `animation-name` (`[ none | <keyframes-name> ]#`) preserving quoted restricted keywords.
#[must_use]
pub fn parse_animation_name_list(raw: &str) -> Option<(Vec<InternedStr>, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut pi = ParserInput::new(trimmed);
    let mut p = Parser::new(&mut pi);
    let mut names = Vec::new();
    let mut ser = Vec::new();
    loop {
        let tok = p.next().ok()?.clone();
        match tok {
            Token::Ident(id) => {
                let s = id.as_ref();
                if s.eq_ignore_ascii_case("none") {
                    names.push(intern_str("none"));
                    ser.push("none".to_string());
                } else if GlobalKeyword::from_ident(&s.to_ascii_lowercase()).is_some()
                    || s.eq_ignore_ascii_case("default")
                {
                    return None;
                } else {
                    names.push(intern_str(s));
                    ser.push(escape_css_ident(s));
                }
            }
            Token::QuotedString(st) => {
                let s = st.as_ref();
                if is_restricted_animation_keyword(s) {
                    let q = quote_css_string(s);
                    names.push(intern_str(&q));
                    ser.push(q);
                } else {
                    names.push(intern_str(s));
                    ser.push(escape_css_ident(s));
                }
            }
            _ => return None,
        }
        if p.is_exhausted() {
            break;
        }
        p.expect_comma().ok()?;
    }
    if names.is_empty() || !p.is_exhausted() {
        return None;
    }
    Some((names, ser.join(", ")))
}

/// Parses a comma-separated CSS `<time>` list (`animation-duration`, `animation-delay`,
/// `transition-duration`, `transition-delay`), supporting `auto` (when `allow_auto`),
/// rejecting negative literals (when `non_negative`), preserving `ms` vs `s` in specified
/// serialization for literal dimensions, and supporting Values L4 math (`calc(...)`).
#[must_use]
pub fn parse_time_list_values4(
    raw: &str,
    allow_auto: bool,
    non_negative: bool,
) -> Option<(Vec<(f32, bool)>, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut pi = ParserInput::new(trimmed);
    let mut p = Parser::new(&mut pi);
    let mut out = Vec::new();
    let mut ser = Vec::new();
    loop {
        let st = p.state();
        if allow_auto {
            if let Ok(id) = p.try_parse(|p| p.expect_ident().map(|s| s.to_ascii_lowercase())) {
                if id == "auto" {
                    out.push((0.0, true));
                    ser.push("auto".to_string());
                    if p.is_exhausted() {
                        break;
                    }
                    p.expect_comma().ok()?;
                    continue;
                }
                return None;
            }
        }
        p.reset(&st);
        let tok_opt = p.next().ok().cloned();
        if let Some(Token::Dimension { value, unit, .. }) = tok_opt {
            let ul = unit.to_ascii_lowercase();
            if ul == "s" || ul == "ms" {
                let is_sep = p.is_exhausted() || {
                    let st_sep = p.state();
                    let comma = matches!(p.next(), Ok(Token::Comma));
                    p.reset(&st_sep);
                    comma
                };
                if is_sep {
                    if !value.is_finite() || (non_negative && value < 0.0) {
                        return None;
                    }
                    let ms = if ul == "s" { value * 1000.0 } else { value };
                    out.push((ms, false));
                    ser.push(format!("{}{}", format_math_f64(f64::from(value)), ul));
                    if p.is_exhausted() {
                        break;
                    }
                    p.expect_comma().ok()?;
                    continue;
                }
            }
        }
        p.reset(&st);
        let pm = parse_math_value_from_parser(&mut p, ValueKind::Time)?;
        let sec = pm.node.eval_scalar(&EvalContext::default(), None);
        if !pm.was_math_fn && (!sec.is_finite() || (non_negative && sec < 0.0)) {
            return None;
        }
        let ms = if sec.is_finite() {
            let v = sec * 1000.0;
            if non_negative {
                v.max(0.0) as f32
            } else {
                v as f32
            }
        } else {
            0.0
        };
        out.push((ms, false));
        ser.push(pm.node.serialize_specified(pm.was_math_fn));
        if p.is_exhausted() {
            break;
        }
        p.expect_comma().ok()?;
    }
    if out.is_empty() || !p.is_exhausted() {
        return None;
    }
    Some((out, ser.join(", ")))
}

/// Parses `animation-duration` list (`[ auto | <time [0s,∞]> ]#`), returning `(ms_or_auto, serialized)`.
#[must_use]
pub fn parse_animation_duration_list(raw: &str) -> Option<(Vec<(f32, bool)>, String)> {
    parse_time_list_values4(raw, true, true)
}

fn split_top_level_commas(raw: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, ch) in raw.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(raw[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = raw[start..].trim();
    if !tail.is_empty() || !parts.is_empty() {
        parts.push(tail);
    }
    parts
}

/// Re-serializes the `transition` shorthand from its 5 CSSOM longhand strings,
/// returning `None` if longhand list lengths disagree or any value is invalid.
#[must_use]
pub fn serialize_transition_shorthand_from_longhands(
    prop_raw: &str,
    dur_raw: &str,
    timing_raw: &str,
    delay_raw: &str,
    beh_raw: &str,
) -> Option<String> {
    let props: Vec<InternedStr> = split_top_level_commas(prop_raw)
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(intern_str)
        .collect();
    let (durs, _) = parse_time_list_values4(dur_raw, false, true)?;
    let (dels, _) = parse_time_list_values4(delay_raw, false, false)?;
    let timings: Vec<TimingFunction> = split_top_level_commas(timing_raw)
        .into_iter()
        .map(|s| parse_single_timing_function(s).map(|(t, _)| t))
        .collect::<Option<Vec<_>>>()?;
    let behs: Vec<TransitionBehaviorMode> = split_top_level_commas(beh_raw)
        .into_iter()
        .map(|s| match s.to_ascii_lowercase().as_str() {
            "normal" => Some(TransitionBehaviorMode::Normal),
            "allow-discrete" => Some(TransitionBehaviorMode::AllowDiscrete),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let len = props.len();
    if len == 0
        || durs.len() != len
        || dels.len() != len
        || timings.len() != len
        || behs.len() != len
    {
        return None;
    }
    let specs: Vec<TransitionSpec> = (0..len)
        .map(|i| TransitionSpec {
            property: props[i].clone(),
            duration_ms: durs[i].0,
            delay_ms: dels[i].0,
            timing: timings[i].clone(),
            behavior: behs[i],
        })
        .collect();
    Some(serialize_transition_list(&specs))
}

/// Re-serializes the `animation` shorthand from its 11 CSSOM longhand strings per CSS Animations L2.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn serialize_animation_shorthand_from_longhands(
    dur_raw: &str,
    timing_raw: &str,
    delay_raw: &str,
    iter_raw: &str,
    dir_raw: &str,
    fill_raw: &str,
    state_raw: &str,
    name_raw: &str,
    timeline_raw: &str,
    range_start_raw: &str,
    range_end_raw: &str,
) -> Option<String> {
    if !timeline_raw.trim().eq_ignore_ascii_case("auto")
        || !range_start_raw.trim().eq_ignore_ascii_case("normal")
        || !range_end_raw.trim().eq_ignore_ascii_case("normal")
    {
        return None;
    }
    let (durs, _) = parse_animation_duration_list(dur_raw)?;
    let (dels, _) = parse_time_list_values4(delay_raw, false, false)?;
    let (iters, _) = parse_animation_iteration_count_list(iter_raw)?;
    let (names, _) = parse_animation_name_list(name_raw)?;
    let timings: Vec<TimingFunction> = split_top_level_commas(timing_raw)
        .into_iter()
        .map(|s| parse_single_timing_function(s).map(|(t, _)| t))
        .collect::<Option<Vec<_>>>()?;
    let dirs: Vec<AnimationDirectionMode> = split_top_level_commas(dir_raw)
        .into_iter()
        .map(|s| match s.to_ascii_lowercase().as_str() {
            "normal" => Some(AnimationDirectionMode::Normal),
            "reverse" => Some(AnimationDirectionMode::Reverse),
            "alternate" => Some(AnimationDirectionMode::Alternate),
            "alternate-reverse" => Some(AnimationDirectionMode::AlternateReverse),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let fills: Vec<AnimationFillModeKind> = split_top_level_commas(fill_raw)
        .into_iter()
        .map(|s| match s.to_ascii_lowercase().as_str() {
            "none" => Some(AnimationFillModeKind::None),
            "forwards" => Some(AnimationFillModeKind::Forwards),
            "backwards" => Some(AnimationFillModeKind::Backwards),
            "both" => Some(AnimationFillModeKind::Both),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let states: Vec<AnimationPlayStateMode> = split_top_level_commas(state_raw)
        .into_iter()
        .map(|s| match s.to_ascii_lowercase().as_str() {
            "running" => Some(AnimationPlayStateMode::Running),
            "paused" => Some(AnimationPlayStateMode::Paused),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let len = names.len();
    if len == 0
        || durs.len() != len
        || dels.len() != len
        || iters.len() != len
        || timings.len() != len
        || dirs.len() != len
        || fills.len() != len
        || states.len() != len
    {
        return None;
    }
    let specs: Vec<AnimationSpec> = (0..len)
        .map(|i| AnimationSpec {
            name: names[i].clone(),
            duration_ms: durs[i].0,
            duration_auto: durs[i].1,
            delay_ms: dels[i].0,
            timing: timings[i].clone(),
            iteration_count: iters[i],
            direction: dirs[i],
            fill_mode: fills[i],
            play_state: states[i],
            composition: AnimationCompositionMode::Replace,
            timeline: intern_str("auto"),
            range_start: AnimationRangePoint::Normal,
            range_end: AnimationRangePoint::Normal,
        })
        .collect();
    Some(serialize_animation_list(&specs))
}

/// Parses `animation-iteration-count` (`[ infinite | <number [0,∞]> ]#`), rejecting negative literals.
#[must_use]
pub fn parse_animation_iteration_count_list(raw: &str) -> Option<(Vec<f32>, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut pi = ParserInput::new(trimmed);
    let mut p = Parser::new(&mut pi);
    let mut out = Vec::new();
    let mut ser = Vec::new();
    loop {
        let st = p.state();
        if let Ok(id) = p.try_parse(|p| p.expect_ident().map(|s| s.to_ascii_lowercase())) {
            if id == "infinite" {
                out.push(f32::INFINITY);
                ser.push("infinite".to_string());
            } else {
                return None;
            }
        } else {
            p.reset(&st);
            let pm = parse_math_value_from_parser(&mut p, ValueKind::Number)?;
            let v = pm.node.eval_scalar(&EvalContext::default(), None);
            if !pm.was_math_fn && (!v.is_finite() || v < 0.0) {
                return None;
            }
            out.push(if v.is_finite() {
                v.max(0.0) as f32
            } else {
                1.0
            });
            ser.push(pm.node.serialize_specified(pm.was_math_fn));
        }
        if p.is_exhausted() {
            break;
        }
        p.expect_comma().ok()?;
    }
    if out.is_empty() || !p.is_exhausted() {
        return None;
    }
    Some((out, ser.join(", ")))
}

/// Parses `animation-timeline` (`[ auto | none | <dashed-ident> | scroll(...) | view(...) ]#`).
#[must_use]
pub fn parse_animation_timeline_list(raw: &str) -> Option<(Vec<InternedStr>, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut pi = ParserInput::new(trimmed);
    let mut p = Parser::new(&mut pi);
    let mut out = Vec::new();
    let mut ser = Vec::new();
    loop {
        let start = p.position();
        let tok = p.next().ok()?.clone();
        match tok {
            Token::Ident(id) => {
                let s = id.as_ref();
                let low = s.to_ascii_lowercase();
                if low == "auto" || low == "none" {
                    out.push(intern_str(&low));
                    ser.push(low);
                } else if s.starts_with("--") && s.len() > 2 {
                    out.push(intern_str(s));
                    ser.push(s.to_string());
                } else {
                    return None;
                }
            }
            Token::Function(fname)
                if fname.eq_ignore_ascii_case("scroll") || fname.eq_ignore_ascii_case("view") =>
            {
                let _ =
                    p.parse_nested_block(|inner| -> Result<(), cssparser::ParseError<'_, ()>> {
                        while inner.next().is_ok() {}
                        Ok(())
                    });
                let full = p.slice_from(start).trim().to_string();
                out.push(intern_str(&full));
                ser.push(full);
            }
            _ => return None,
        }
        if p.is_exhausted() {
            break;
        }
        p.expect_comma().ok()?;
    }
    if out.is_empty() || !p.is_exhausted() {
        return None;
    }
    Some((out, ser.join(", ")))
}

fn format_time_s(ms: f32) -> String {
    format!("{}s", format_math_f64(f64::from(ms / 1000.0)))
}

/// Canonical serialization of a `TransitionSpec` list for `transition` shorthand.
#[must_use]
pub fn serialize_transition_list(specs: &[TransitionSpec]) -> String {
    if specs.is_empty() {
        return "all".to_string();
    }
    specs
        .iter()
        .map(|t| {
            let mut parts: Vec<String> = Vec::new();
            let dur_s = t.duration_ms / 1000.0;
            let del_s = t.delay_ms / 1000.0;
            let is_def_timing = t.timing == TimingFunction::Ease;
            let is_def_beh = t.behavior == TransitionBehaviorMode::Normal;
            if t.property.as_str() != "all"
                || (dur_s == 0.0 && del_s == 0.0 && is_def_timing && is_def_beh)
            {
                parts.push(t.property.as_str().to_string());
            }
            if dur_s != 0.0 || del_s != 0.0 {
                parts.push(format_time_s(t.duration_ms));
            }
            if !is_def_timing {
                parts.push(t.timing.to_css_string());
            }
            if del_s != 0.0 {
                parts.push(format_time_s(t.delay_ms));
            }
            if !is_def_beh {
                parts.push("allow-discrete".to_string());
            }
            parts.join(" ")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Expands a parsed `transition` shorthand into its 5 CSSOM longhands.
#[must_use]
pub fn expand_transition_longhands(specs: &[TransitionSpec]) -> Vec<(String, String)> {
    let prop = specs
        .iter()
        .map(|t| t.property.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let dur = specs
        .iter()
        .map(|t| format_time_s(t.duration_ms))
        .collect::<Vec<_>>()
        .join(", ");
    let timing = specs
        .iter()
        .map(|t| t.timing.to_css_string())
        .collect::<Vec<_>>()
        .join(", ");
    let del = specs
        .iter()
        .map(|t| format_time_s(t.delay_ms))
        .collect::<Vec<_>>()
        .join(", ");
    let beh = specs
        .iter()
        .map(|t| match t.behavior {
            TransitionBehaviorMode::Normal => "normal",
            TransitionBehaviorMode::AllowDiscrete => "allow-discrete",
        })
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        ("transition-property".to_string(), prop),
        ("transition-duration".to_string(), dur),
        ("transition-timing-function".to_string(), timing),
        ("transition-delay".to_string(), del),
        ("transition-behavior".to_string(), beh),
    ]
}

/// Canonical serialization of an `AnimationSpec` list for `animation` shorthand.
#[must_use]
pub fn serialize_animation_list(specs: &[AnimationSpec]) -> String {
    if specs.is_empty() {
        return "none".to_string();
    }
    specs
        .iter()
        .map(|a| {
            let mut parts: Vec<String> = Vec::new();
            let dur_s = a.duration_ms / 1000.0;
            let del_s = a.delay_ms / 1000.0;
            let is_def_timing = a.timing == TimingFunction::Ease;
            let is_def_iter = (a.iteration_count - 1.0).abs() < 1e-6;
            let is_def_dir = a.direction == AnimationDirectionMode::Normal;
            let is_def_fill = a.fill_mode == AnimationFillModeKind::None;
            let is_def_state = a.play_state == AnimationPlayStateMode::Running;
            let is_def_name = a.name.as_str().eq_ignore_ascii_case("none");

            if (!a.duration_auto && dur_s != 0.0) || del_s != 0.0 {
                parts.push(format_time_s(a.duration_ms));
            }
            if !is_def_timing {
                parts.push(a.timing.to_css_string());
            }
            if del_s != 0.0 {
                parts.push(format_time_s(a.delay_ms));
            }
            if !is_def_iter {
                if a.iteration_count.is_infinite() {
                    parts.push("infinite".to_string());
                } else {
                    parts.push(format_math_f64(f64::from(a.iteration_count)));
                }
            }
            if !is_def_dir {
                parts.push(
                    match a.direction {
                        AnimationDirectionMode::Normal => "normal",
                        AnimationDirectionMode::Reverse => "reverse",
                        AnimationDirectionMode::Alternate => "alternate",
                        AnimationDirectionMode::AlternateReverse => "alternate-reverse",
                    }
                    .to_string(),
                );
            }
            if !is_def_fill {
                parts.push(
                    match a.fill_mode {
                        AnimationFillModeKind::None => "none",
                        AnimationFillModeKind::Forwards => "forwards",
                        AnimationFillModeKind::Backwards => "backwards",
                        AnimationFillModeKind::Both => "both",
                    }
                    .to_string(),
                );
            }
            if !is_def_state {
                parts.push("paused".to_string());
            }
            if !is_def_name || parts.is_empty() {
                parts.push(serialize_animation_name_token(a.name.as_str()));
            }
            parts.join(" ")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Expands a parsed `animation` shorthand into its 11 CSSOM longhands per CSS Animations L2.
#[must_use]
pub fn expand_animation_longhands(specs: &[AnimationSpec]) -> Vec<(String, String)> {
    let dur = specs
        .iter()
        .map(|a| {
            if a.duration_auto {
                "auto".to_string()
            } else {
                format_time_s(a.duration_ms)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let timing = specs
        .iter()
        .map(|a| a.timing.to_css_string())
        .collect::<Vec<_>>()
        .join(", ");
    let del = specs
        .iter()
        .map(|a| format_time_s(a.delay_ms))
        .collect::<Vec<_>>()
        .join(", ");
    let iter = specs
        .iter()
        .map(|a| {
            if a.iteration_count.is_infinite() {
                "infinite".to_string()
            } else {
                format_math_f64(f64::from(a.iteration_count))
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let dir = specs
        .iter()
        .map(|a| match a.direction {
            AnimationDirectionMode::Normal => "normal",
            AnimationDirectionMode::Reverse => "reverse",
            AnimationDirectionMode::Alternate => "alternate",
            AnimationDirectionMode::AlternateReverse => "alternate-reverse",
        })
        .collect::<Vec<_>>()
        .join(", ");
    let fill = specs
        .iter()
        .map(|a| match a.fill_mode {
            AnimationFillModeKind::None => "none",
            AnimationFillModeKind::Forwards => "forwards",
            AnimationFillModeKind::Backwards => "backwards",
            AnimationFillModeKind::Both => "both",
        })
        .collect::<Vec<_>>()
        .join(", ");
    let state = specs
        .iter()
        .map(|a| match a.play_state {
            AnimationPlayStateMode::Running => "running",
            AnimationPlayStateMode::Paused => "paused",
        })
        .collect::<Vec<_>>()
        .join(", ");
    let name = specs
        .iter()
        .map(|a| serialize_animation_name_token(a.name.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        ("animation-duration".to_string(), dur),
        ("animation-timing-function".to_string(), timing),
        ("animation-delay".to_string(), del),
        ("animation-iteration-count".to_string(), iter),
        ("animation-direction".to_string(), dir),
        ("animation-fill-mode".to_string(), fill),
        ("animation-play-state".to_string(), state),
        ("animation-name".to_string(), name),
        ("animation-timeline".to_string(), "auto".to_string()),
        ("animation-range-start".to_string(), "normal".to_string()),
        ("animation-range-end".to_string(), "normal".to_string()),
    ]
}

fn parse_time_ms_anim(p: &mut Parser<'_, '_>) -> Option<f32> {
    let st = p.state();
    match p.next().ok()?.clone() {
        Token::Dimension { value, unit, .. } => {
            if unit.eq_ignore_ascii_case("s") {
                Some(value * 1000.0)
            } else if unit.eq_ignore_ascii_case("ms") {
                Some(value)
            } else {
                p.reset(&st);
                None
            }
        }
        _ => {
            p.reset(&st);
            None
        }
    }
}

/// Parses CSS Animations L1/L2 `animation` shorthand with full `duration_auto` and
/// quoted-string keyframes-name support.
#[must_use]
pub fn parse_animation_shorthand_full(input: &str) -> Option<(Vec<AnimationSpec>, String)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut pi = ParserInput::new(trimmed);
    let mut p = Parser::new(&mut pi);
    let mut out = Vec::new();
    loop {
        let mut name: Option<InternedStr> = None;
        let mut dur: Option<(f32, bool)> = None;
        let mut del: Option<f32> = None;
        let mut timing: Option<TimingFunction> = None;
        let mut iter: Option<f32> = None;
        let mut dir: Option<AnimationDirectionMode> = None;
        let mut fill: Option<AnimationFillModeKind> = None;
        let mut state: Option<AnimationPlayStateMode> = None;
        let mut comp: Option<AnimationCompositionMode> = None;
        let mut any = false;
        while !p.is_exhausted() {
            let st = p.state();
            if matches!(p.next(), Ok(Token::Comma)) {
                p.reset(&st);
                break;
            }
            p.reset(&st);
            if let Some(ms) = parse_time_ms_anim(&mut p) {
                if dur.is_none() {
                    if ms < 0.0 {
                        return None;
                    }
                    dur = Some((ms, false));
                } else if del.is_none() {
                    del = Some(ms);
                } else {
                    return None;
                }
                any = true;
                continue;
            }
            if timing.is_none() {
                let st_tf = p.state();
                let tok_start = p.position();
                if let Ok(tok) = p.next().cloned() {
                    let is_tf_cand = match &tok {
                        Token::Ident(id) => matches!(
                            id.to_ascii_lowercase().as_str(),
                            "linear"
                                | "ease"
                                | "ease-in"
                                | "ease-out"
                                | "ease-in-out"
                                | "step-start"
                                | "step-end"
                        ),
                        Token::Function(f) => matches!(
                            f.to_ascii_lowercase().as_str(),
                            "cubic-bezier" | "steps" | "linear"
                        ),
                        _ => false,
                    };
                    if is_tf_cand {
                        if matches!(tok, Token::Function(_)) {
                            let _ = p.parse_nested_block(
                                |inner| -> Result<(), cssparser::ParseError<'_, ()>> {
                                    while inner.next().is_ok() {}
                                    Ok(())
                                },
                            );
                        }
                        let slice = p.slice_from(tok_start);
                        if let Some((tf, _)) = parse_single_timing_function(slice) {
                            timing = Some(tf);
                            any = true;
                            continue;
                        }
                    }
                }
                p.reset(&st_tf);
            }
            if iter.is_none() {
                if let Ok(n) = p.try_parse(Parser::expect_number) {
                    if !n.is_finite() || n < 0.0 {
                        return None;
                    }
                    iter = Some(n);
                    any = true;
                    continue;
                }
            }
            if let Ok(s) = p.try_parse(|p| p.expect_string().map(|v| v.to_string())) {
                if name.is_some() {
                    return None;
                }
                let stored = if is_restricted_animation_keyword(&s) {
                    quote_css_string(&s)
                } else {
                    s
                };
                name = Some(intern_str(&stored));
                any = true;
                continue;
            }
            if let Ok(ident) = p.try_parse(|p| p.expect_ident().map(|s| s.to_string())) {
                let lower = ident.to_ascii_lowercase();
                if dur.is_none() && lower == "auto" {
                    dur = Some((0.0, true));
                    any = true;
                    continue;
                }
                if iter.is_none() && lower == "infinite" {
                    iter = Some(f32::INFINITY);
                    any = true;
                    continue;
                }
                if dir.is_none() {
                    let d = match lower.as_str() {
                        "normal" => Some(AnimationDirectionMode::Normal),
                        "reverse" => Some(AnimationDirectionMode::Reverse),
                        "alternate" => Some(AnimationDirectionMode::Alternate),
                        "alternate-reverse" => Some(AnimationDirectionMode::AlternateReverse),
                        _ => None,
                    };
                    if let Some(d) = d {
                        dir = Some(d);
                        any = true;
                        continue;
                    }
                }
                if fill.is_none() {
                    let f = match lower.as_str() {
                        "forwards" => Some(AnimationFillModeKind::Forwards),
                        "backwards" => Some(AnimationFillModeKind::Backwards),
                        "both" => Some(AnimationFillModeKind::Both),
                        _ => None,
                    };
                    if let Some(f) = f {
                        fill = Some(f);
                        any = true;
                        continue;
                    }
                }
                if state.is_none() && (lower == "running" || lower == "paused") {
                    state = Some(if lower == "paused" {
                        AnimationPlayStateMode::Paused
                    } else {
                        AnimationPlayStateMode::Running
                    });
                    any = true;
                    continue;
                }
                if comp.is_none() && matches!(lower.as_str(), "replace" | "add" | "accumulate") {
                    comp = Some(match lower.as_str() {
                        "add" => AnimationCompositionMode::Add,
                        "accumulate" => AnimationCompositionMode::Accumulate,
                        _ => AnimationCompositionMode::Replace,
                    });
                    any = true;
                    continue;
                }
                if name.is_none() {
                    if lower == "none" {
                        name = Some(intern_str("none"));
                        any = true;
                        continue;
                    }
                    if GlobalKeyword::from_ident(&lower).is_some() || lower == "default" {
                        return None;
                    }
                    name = Some(intern_str(&ident));
                    any = true;
                    continue;
                }
                return None;
            }
            return None;
        }
        if !any {
            return None;
        }
        let (duration_ms, duration_auto) = dur.unwrap_or((0.0, true));
        out.push(AnimationSpec {
            name: name.unwrap_or_else(|| intern_str("none")),
            duration_ms,
            duration_auto,
            delay_ms: del.unwrap_or(0.0),
            timing: timing.unwrap_or(TimingFunction::Ease),
            iteration_count: iter.unwrap_or(1.0),
            direction: dir.unwrap_or(AnimationDirectionMode::Normal),
            fill_mode: fill.unwrap_or(AnimationFillModeKind::None),
            play_state: state.unwrap_or(AnimationPlayStateMode::Running),
            composition: comp.unwrap_or(AnimationCompositionMode::Replace),
            timeline: intern_str("auto"),
            range_start: AnimationRangePoint::Normal,
            range_end: AnimationRangePoint::Normal,
        });
        if p.try_parse(Parser::expect_comma).is_err() {
            break;
        }
    }
    if out.is_empty() || !p.is_exhausted() {
        return None;
    }
    let ser = serialize_animation_list(&out);
    Some((out, ser))
}
