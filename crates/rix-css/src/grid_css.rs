//! W3C CSS Grid Layout Level 1, Level 2 (`subgrid`), and Level 3 (`grid-lanes`)
//! typed property values, parsers, canonical serializers, shorthand expanders,
//! and track-list interpolation/composition.

use cssparser::{Parser, ParserInput, Token};
use serde::{Deserialize, Serialize};

use crate::substitution::escape_css_ident;
use crate::values::{
    AnimationCompositionMode, CalcExpr, EvalContext, GridAutoFlowMode, GridPlacementValue,
    GridTemplateAreasValue, GridTrackSize, LengthPercentage, LengthUnit,
};
use crate::values_math::{format_math_f64, parse_math_value, MathNode, ValueKind};
use rix_core::{intern_str, InternedStr};

/// Repeat count for CSS Grid `repeat(...)` notations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GridRepeatCount {
    Number(u32),
    NumberMath(Box<MathNode>),
    AutoFill,
    AutoFit,
}

impl GridRepeatCount {
    #[must_use]
    pub fn resolve_number(&self, eval: &EvalContext) -> Option<u32> {
        match self {
            Self::Number(n) => Some((*n).max(1)),
            Self::NumberMath(node) => {
                let v = node.eval_scalar(eval, None).round();
                if v.is_finite() {
                    Some((v.clamp(1.0, 10_000.0)) as u32)
                } else {
                    Some(1)
                }
            }
            Self::AutoFill | Self::AutoFit => None,
        }
    }
}

/// One item in a CSS Grid `<track-list>` or `<auto-track-list>`.
#[derive(Debug, Clone, PartialEq)]
pub enum GridTrackListEntry {
    Track(GridTrackSize),
    Repeat {
        count: GridRepeatCount,
        tracks: Vec<GridTrackSize>,
        /// Line name lists around the repeated tracks (`line_names.len() == tracks.len() + 1`).
        line_names: Vec<Vec<InternedStr>>,
    },
}

/// One item inside `subgrid <line-name-list>?`.
#[derive(Debug, Clone, PartialEq)]
pub enum GridSubgridLineItem {
    Names(Vec<InternedStr>),
    Repeat {
        count: GridRepeatCount,
        names: Vec<Vec<InternedStr>>,
    },
}

/// Complete typed representation of `grid-template-columns` and `grid-template-rows`.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum GridTemplateTrackList {
    #[default]
    None,
    Tracks {
        items: Vec<GridTrackListEntry>,
        /// Line name lists between/around entries (`line_names.len() == items.len() + 1`).
        line_names: Vec<Vec<InternedStr>>,
    },
    Subgrid(Vec<GridSubgridLineItem>),
}

impl GridTemplateTrackList {
    #[must_use]
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    #[must_use]
    pub fn is_subgrid(&self) -> bool {
        matches!(self, Self::Subgrid(_))
    }

    #[must_use]
    pub fn has_repeat(&self) -> bool {
        matches!(
            self,
            Self::Tracks { items, .. }
                if items.iter().any(|e| matches!(e, GridTrackListEntry::Repeat { .. }))
        )
    }

    #[must_use]
    pub fn has_intrinsic_auto_repeat(&self) -> bool {
        matches!(
            self,
            Self::Tracks { items, .. }
                if items.iter().any(|e| matches!(
                    e,
                    GridTrackListEntry::Repeat {
                        count: GridRepeatCount::AutoFill | GridRepeatCount::AutoFit,
                        tracks,
                        ..
                    } if tracks.iter().any(|t| !is_fixed_size(t))
                ))
        )
    }

    #[must_use]
    pub fn from_simple_tracks(tracks: Vec<GridTrackSize>) -> Self {
        if tracks.is_empty() {
            return Self::None;
        }
        let n = tracks.len();
        let items = tracks.into_iter().map(GridTrackListEntry::Track).collect();
        Self::Tracks {
            items,
            line_names: vec![Vec::new(); n + 1],
        }
    }

    /// Flattens explicit tracks into a flat `Vec<GridTrackSize>` for legacy consumers.
    #[must_use]
    pub fn flatten_simple_tracks(&self) -> Vec<GridTrackSize> {
        let Self::Tracks { items, .. } = self else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in items {
            match entry {
                GridTrackListEntry::Track(sz) => out.push(sz.clone()),
                GridTrackListEntry::Repeat { count, tracks, .. } => {
                    let reps = match count {
                        GridRepeatCount::Number(n) => (*n as usize).min(128),
                        GridRepeatCount::NumberMath(_)
                        | GridRepeatCount::AutoFill
                        | GridRepeatCount::AutoFit => 1,
                    };
                    for _ in 0..reps {
                        out.extend(tracks.iter().cloned());
                    }
                }
            }
        }
        out
    }
}

/// CSS Grid Level 3 `fit-tolerance` value (`normal | <length-percentage [0,∞]> | infinite`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum FitToleranceValue {
    #[default]
    Normal,
    Infinite,
    Length(LengthPercentage),
}

/// CSS Grid Level 3 `grid-lanes-direction` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum GridLanesDirectionMode {
    #[default]
    Normal,
    Explicit {
        is_row: bool,
        track_reverse: bool,
        fill_reverse: bool,
        track_before_fill: bool,
    },
}

/// CSS Grid Level 3 `grid-lanes-pack` value (`normal | dense`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum GridLanesPackMode {
    #[default]
    Normal,
    Dense,
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

fn is_css_wide_keyword(ident: &str) -> bool {
    ident.eq_ignore_ascii_case("initial")
        || ident.eq_ignore_ascii_case("inherit")
        || ident.eq_ignore_ascii_case("unset")
        || ident.eq_ignore_ascii_case("revert")
        || ident.eq_ignore_ascii_case("revert-layer")
        || ident.eq_ignore_ascii_case("revert-rule")
        || ident.eq_ignore_ascii_case("default")
}

fn is_invalid_line_name(ident: &str) -> bool {
    ident.eq_ignore_ascii_case("span")
        || ident.eq_ignore_ascii_case("auto")
        || is_css_wide_keyword(ident)
}

fn is_invalid_grid_ident(ident: &str) -> bool {
    ident.eq_ignore_ascii_case("span")
        || ident.eq_ignore_ascii_case("auto")
        || is_css_wide_keyword(ident)
}

fn consume_raw_function(p: &mut Parser<'_, '_>, fn_name: &str) -> Option<String> {
    let start = p.position();
    p.parse_nested_block(|nested| {
        while nested.next_including_whitespace_and_comments().is_ok() {}
        Ok::<(), cssparser::ParseError<'_, ()>>(())
    })
    .ok()?;
    let inner = p.slice_from(start);
    let inner_trimmed = inner.strip_suffix(')').unwrap_or(inner);
    Some(format!("{fn_name}({inner_trimmed})"))
}

fn parse_nonneg_lp_from_parser(p: &mut Parser<'_, '_>) -> Option<LengthPercentage> {
    let state = p.state();
    let tok = p.next().ok()?.clone();
    match tok {
        Token::Dimension { value, unit, .. } => {
            if !value.is_finite() || value < 0.0 {
                p.reset(&state);
                return None;
            }
            let u = LengthUnit::from_unit_str(&unit)?;
            Some(LengthPercentage::Length(value, u))
        }
        Token::Percentage { unit_value, .. } => {
            if !unit_value.is_finite() || unit_value < 0.0 {
                p.reset(&state);
                return None;
            }
            Some(LengthPercentage::Percent(unit_value))
        }
        Token::Number { value, .. } => {
            if value == 0.0 {
                Some(LengthPercentage::Zero)
            } else {
                p.reset(&state);
                None
            }
        }
        Token::Function(name) => {
            let low = name.to_ascii_lowercase();
            if !crate::values_math::is_css_math_fn(&low) {
                p.reset(&state);
                return None;
            }
            let Some(raw) = consume_raw_function(p, &low) else {
                p.reset(&state);
                return None;
            };
            let Some(pm) = parse_math_value(&raw, ValueKind::LengthPercentage) else {
                p.reset(&state);
                return None;
            };
            if !pm.was_math_fn {
                let v = pm.node.eval_scalar(&EvalContext::default(), None);
                if !v.is_finite() || v < 0.0 {
                    p.reset(&state);
                    return None;
                }
            }
            Some(LengthPercentage::Calc(Box::new(CalcExpr::Math(Box::new(
                pm.node,
            )))))
        }
        _ => {
            p.reset(&state);
            None
        }
    }
}

fn parse_nonneg_flex_from_parser(p: &mut Parser<'_, '_>) -> Option<GridTrackSize> {
    let state = p.state();
    let tok = p.next().ok()?.clone();
    match tok {
        Token::Dimension { value, unit, .. } if unit.eq_ignore_ascii_case("fr") => {
            if !value.is_finite() || value < 0.0 {
                p.reset(&state);
                return None;
            }
            Some(GridTrackSize::Fr(value))
        }
        Token::Function(name) => {
            let low = name.to_ascii_lowercase();
            if !crate::values_math::is_css_math_fn(&low) {
                p.reset(&state);
                return None;
            }
            let Some(raw) = consume_raw_function(p, &low) else {
                p.reset(&state);
                return None;
            };
            let Some(pm) = parse_math_value(&raw, ValueKind::Flex) else {
                p.reset(&state);
                return None;
            };
            Some(GridTrackSize::FrMath(Box::new(pm.node)))
        }
        _ => {
            p.reset(&state);
            None
        }
    }
}

fn parse_track_breadth(p: &mut Parser<'_, '_>, allow_flex: bool) -> Option<GridTrackSize> {
    let state = p.state();
    if let Ok(ident) = p.try_parse(|p| p.expect_ident_cloned()) {
        let low = ident.to_ascii_lowercase();
        return match low.as_str() {
            "auto" => Some(GridTrackSize::Auto),
            "min-content" => Some(GridTrackSize::MinContent),
            "max-content" => Some(GridTrackSize::MaxContent),
            _ => {
                p.reset(&state);
                None
            }
        };
    }
    if allow_flex {
        if let Some(fr) = parse_nonneg_flex_from_parser(p) {
            return Some(fr);
        }
    }
    if let Some(lp) = parse_nonneg_lp_from_parser(p) {
        return Some(GridTrackSize::Length(lp));
    }
    p.reset(&state);
    None
}

fn parse_track_size(p: &mut Parser<'_, '_>) -> Option<GridTrackSize> {
    let state = p.state();
    if let Ok(fn_name) = p.try_parse(|p| p.expect_function().map(|s| s.to_string())) {
        let low = fn_name.to_ascii_lowercase();
        if low == "minmax" {
            let res: Result<GridTrackSize, cssparser::ParseError<'_, ()>> =
                p.parse_nested_block(|nested| {
                    let min_b = parse_track_breadth(nested, false)
                        .ok_or_else(|| nested.new_custom_error(()))?;
                    nested.expect_comma()?;
                    let max_b = parse_track_breadth(nested, true)
                        .ok_or_else(|| nested.new_custom_error(()))?;
                    nested.skip_whitespace();
                    if !nested.is_exhausted() {
                        return Err(nested.new_custom_error(()));
                    }
                    Ok(GridTrackSize::MinMax(Box::new(min_b), Box::new(max_b)))
                });
            if let Ok(mm) = res {
                return Some(mm);
            }
            p.reset(&state);
            return None;
        }
        if low == "fit-content" {
            let res: Result<GridTrackSize, cssparser::ParseError<'_, ()>> =
                p.parse_nested_block(|nested| {
                    let lp = parse_nonneg_lp_from_parser(nested)
                        .ok_or_else(|| nested.new_custom_error(()))?;
                    nested.skip_whitespace();
                    if !nested.is_exhausted() {
                        return Err(nested.new_custom_error(()));
                    }
                    Ok(GridTrackSize::FitContent(lp))
                });
            if let Ok(fc) = res {
                return Some(fc);
            }
            p.reset(&state);
            return None;
        }
        p.reset(&state);
    }
    parse_track_breadth(p, true)
}

fn is_fixed_breadth(sz: &GridTrackSize) -> bool {
    matches!(sz, GridTrackSize::Length(_))
}

fn is_fixed_size(sz: &GridTrackSize) -> bool {
    match sz {
        GridTrackSize::Length(_) => true,
        GridTrackSize::MinMax(a, b) => is_fixed_breadth(a) || is_fixed_breadth(b),
        _ => false,
    }
}

fn is_intrinsic_or_fixed_size(sz: &GridTrackSize) -> bool {
    match sz {
        GridTrackSize::Length(_)
        | GridTrackSize::Auto
        | GridTrackSize::MinContent
        | GridTrackSize::MaxContent
        | GridTrackSize::FitContent(_) => true,
        GridTrackSize::MinMax(a, b) => {
            !matches!(
                a.as_ref(),
                GridTrackSize::Fr(_) | GridTrackSize::FrMath(_) | GridTrackSize::FitContent(_)
            ) && !matches!(b.as_ref(), GridTrackSize::FitContent(_))
                && (is_fixed_breadth(a)
                    || is_fixed_breadth(b)
                    || !matches!(b.as_ref(), GridTrackSize::Fr(_) | GridTrackSize::FrMath(_)))
        }
        GridTrackSize::Fr(_) | GridTrackSize::FrMath(_) => false,
    }
}

fn parse_line_names(p: &mut Parser<'_, '_>) -> Option<Vec<InternedStr>> {
    let state = p.state();
    if p.expect_square_bracket_block().is_err() {
        p.reset(&state);
        return None;
    }
    let res: Result<Vec<InternedStr>, cssparser::ParseError<'_, ()>> = p.parse_nested_block(|n| {
        let mut names = Vec::new();
        while !n.is_exhausted() {
            n.skip_whitespace();
            if n.is_exhausted() {
                break;
            }
            let ident = n.expect_ident_cloned()?;
            if is_invalid_line_name(&ident) {
                return Err(n.new_custom_error(()));
            }
            names.push(intern_str(&ident));
        }
        Ok(names)
    });
    match res {
        Ok(v) => Some(v),
        Err(_) => {
            p.reset(&state);
            None
        }
    }
}

fn parse_positive_int_or_math(p: &mut Parser<'_, '_>) -> Option<GridRepeatCount> {
    let state = p.state();
    let tok = p.next().ok()?.clone();
    match tok {
        Token::Number {
            int_value: Some(v), ..
        } if v >= 1 => Some(GridRepeatCount::Number(v as u32)),
        Token::Function(name) => {
            let low = name.to_ascii_lowercase();
            let raw = consume_raw_function(p, &low)?;
            let pm = parse_math_value(&raw, ValueKind::Integer)?;
            Some(GridRepeatCount::NumberMath(Box::new(pm.node)))
        }
        _ => {
            p.reset(&state);
            None
        }
    }
}

fn parse_track_repeat(p: &mut Parser<'_, '_>) -> Option<GridTrackListEntry> {
    let state = p.state();
    let fn_name = p
        .try_parse(|p| p.expect_function().map(|s| s.to_string()))
        .ok()?;
    if !fn_name.eq_ignore_ascii_case("repeat") {
        p.reset(&state);
        return None;
    }
    let res: Result<GridTrackListEntry, cssparser::ParseError<'_, ()>> =
        p.parse_nested_block(|n| {
            n.skip_whitespace();
            let count = if let Ok(ident) = n.try_parse(|n| n.expect_ident_cloned()) {
                if ident.eq_ignore_ascii_case("auto-fill") {
                    GridRepeatCount::AutoFill
                } else if ident.eq_ignore_ascii_case("auto-fit") {
                    GridRepeatCount::AutoFit
                } else {
                    return Err(n.new_custom_error(()));
                }
            } else {
                parse_positive_int_or_math(n).ok_or_else(|| n.new_custom_error(()))?
            };
            n.expect_comma()?;
            n.skip_whitespace();

            let mut tracks = Vec::new();
            let mut line_names = Vec::new();
            let mut pending_names = parse_line_names(n).unwrap_or_default();
            while !n.is_exhausted() {
                n.skip_whitespace();
                if n.is_exhausted() {
                    break;
                }
                let sz = parse_track_size(n).ok_or_else(|| n.new_custom_error(()))?;
                if matches!(count, GridRepeatCount::AutoFill | GridRepeatCount::AutoFit)
                    && !is_intrinsic_or_fixed_size(&sz)
                {
                    return Err(n.new_custom_error(()));
                }
                line_names.push(std::mem::take(&mut pending_names));
                tracks.push(sz);
                n.skip_whitespace();
                if let Some(ln) = parse_line_names(n) {
                    pending_names = ln;
                }
            }
            if tracks.is_empty() {
                return Err(n.new_custom_error(()));
            }
            line_names.push(pending_names);
            Ok(GridTrackListEntry::Repeat {
                count,
                tracks,
                line_names,
            })
        });
    match res {
        Ok(entry) => Some(entry),
        Err(_) => {
            p.reset(&state);
            None
        }
    }
}

fn parse_subgrid_tail(p: &mut Parser<'_, '_>) -> Option<Vec<GridSubgridLineItem>> {
    let mut items = Vec::new();
    let mut saw_auto_fill = false;
    while !p.is_exhausted() {
        p.skip_whitespace();
        if p.is_exhausted() {
            break;
        }
        if let Some(ln) = parse_line_names(p) {
            items.push(GridSubgridLineItem::Names(ln));
            continue;
        }
        let state = p.state();
        let Ok(fn_name) = p.expect_function().map(|s| s.to_string()) else {
            p.reset(&state);
            return None;
        };
        if !fn_name.eq_ignore_ascii_case("repeat") {
            p.reset(&state);
            return None;
        }
        let rep = p
            .parse_nested_block(|n| {
                n.skip_whitespace();
                let count = if let Ok(ident) = n.try_parse(|n| n.expect_ident_cloned()) {
                    if ident.eq_ignore_ascii_case("auto-fill") && !saw_auto_fill {
                        saw_auto_fill = true;
                        GridRepeatCount::AutoFill
                    } else {
                        return Err(n.new_custom_error(()));
                    }
                } else {
                    parse_positive_int_or_math(n).ok_or_else(|| n.new_custom_error(()))?
                };
                n.expect_comma()?;
                let mut names_list = Vec::new();
                while !n.is_exhausted() {
                    n.skip_whitespace();
                    if n.is_exhausted() {
                        break;
                    }
                    let ln = parse_line_names(n).ok_or_else(|| n.new_custom_error(()))?;
                    names_list.push(ln);
                }
                if names_list.is_empty() {
                    return Err(n.new_custom_error(()));
                }
                Ok::<GridSubgridLineItem, cssparser::ParseError<'_, ()>>(
                    GridSubgridLineItem::Repeat {
                        count,
                        names: names_list,
                    },
                )
            })
            .ok()?;
        items.push(rep);
    }
    Some(items)
}

fn parse_track_list_from_parser(
    p: &mut Parser<'_, '_>,
    allow_subgrid: bool,
    allow_repeat: bool,
) -> Option<GridTemplateTrackList> {
    p.skip_whitespace();
    if p.is_exhausted() {
        return None;
    }
    let start_state = p.state();
    if let Ok(ident) = p.try_parse(|p| p.expect_ident_cloned()) {
        if ident.eq_ignore_ascii_case("none") {
            p.skip_whitespace();
            if p.is_exhausted() {
                return Some(GridTemplateTrackList::None);
            }
            p.reset(&start_state);
            return None;
        }
        if allow_subgrid && ident.eq_ignore_ascii_case("subgrid") {
            let sub = parse_subgrid_tail(p)?;
            return Some(GridTemplateTrackList::Subgrid(sub));
        }
        p.reset(&start_state);
    }

    let mut items = Vec::new();
    let mut line_names = Vec::new();
    let mut auto_repeat_count = 0usize;
    let mut pending_names = parse_line_names(p).unwrap_or_default();

    while !p.is_exhausted() {
        p.skip_whitespace();
        if p.is_exhausted() {
            break;
        }
        let entry = if allow_repeat {
            if let Some(rep) = parse_track_repeat(p) {
                if matches!(
                    rep,
                    GridTrackListEntry::Repeat {
                        count: GridRepeatCount::AutoFill | GridRepeatCount::AutoFit,
                        ..
                    }
                ) {
                    auto_repeat_count += 1;
                    if auto_repeat_count > 1 {
                        return None;
                    }
                }
                rep
            } else {
                GridTrackListEntry::Track(parse_track_size(p)?)
            }
        } else {
            GridTrackListEntry::Track(parse_track_size(p)?)
        };
        line_names.push(std::mem::take(&mut pending_names));
        items.push(entry);
        p.skip_whitespace();
        if let Some(ln) = parse_line_names(p) {
            pending_names = ln;
        }
    }
    if items.is_empty() {
        return None;
    }
    line_names.push(pending_names);

    // CSS Grid L1 §7.2.2 + L3 intrinsic auto-repeat validation:
    // If an auto-repeat is present, every track outside the auto-repeat must be `<fixed-size>`.
    if auto_repeat_count == 1 {
        for entry in &items {
            match entry {
                GridTrackListEntry::Track(sz) => {
                    if !is_fixed_size(sz) {
                        return None;
                    }
                }
                GridTrackListEntry::Repeat { count, tracks, .. } => {
                    if !matches!(count, GridRepeatCount::AutoFill | GridRepeatCount::AutoFit)
                        && tracks.iter().any(|t| !is_fixed_size(t))
                    {
                        return None;
                    }
                }
            }
        }
    }

    Some(GridTemplateTrackList::Tracks { items, line_names })
}

/// Parses CSS `grid-template-columns` or `grid-template-rows`.
#[must_use]
pub fn parse_grid_template_track_list(input: &str) -> Option<GridTemplateTrackList> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut pi = ParserInput::new(trimmed);
    let mut p = Parser::new(&mut pi);
    let res = parse_track_list_from_parser(&mut p, true, true)?;
    p.skip_whitespace();
    p.is_exhausted().then_some(res)
}

/// Parses CSS `grid-auto-columns` or `grid-auto-rows` (`<track-size>+`).
#[must_use]
pub fn parse_grid_auto_tracks(input: &str) -> Option<Vec<GridTrackSize>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut pi = ParserInput::new(trimmed);
    let mut p = Parser::new(&mut pi);
    let mut out = Vec::new();
    while !p.is_exhausted() {
        p.skip_whitespace();
        if p.is_exhausted() {
            break;
        }
        out.push(parse_track_size(&mut p)?);
    }
    (!out.is_empty() && p.is_exhausted()).then_some(out)
}

fn tokenize_area_row(row_str: &str) -> Option<Vec<Option<InternedStr>>> {
    let mut cells = Vec::new();
    let chars: Vec<char> = row_str.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c') {
            i += 1;
            continue;
        }
        if c == '.' {
            while i < chars.len() && chars[i] == '.' {
                i += 1;
            }
            cells.push(None);
            continue;
        }
        if is_area_name_char(c) {
            let start = i;
            while i < chars.len() && is_area_name_char(chars[i]) {
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();
            cells.push(Some(intern_str(&s)));
            continue;
        }
        return None;
    }
    (!cells.is_empty()).then_some(cells)
}

fn is_area_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || (c as u32) >= 0x80
}

fn validate_and_build_areas(rows: Vec<Vec<Option<InternedStr>>>) -> Option<GridTemplateAreasValue> {
    if rows.is_empty() {
        return None;
    }
    let cols = rows[0].len();
    if cols == 0 || rows.iter().any(|r| r.len() != cols) {
        return None;
    }
    let num_rows = rows.len();
    let mut flat = Vec::with_capacity(num_rows * cols);
    for r in &rows {
        flat.extend(r.iter().cloned());
    }
    // CSS Grid L1 §7.3: every named area must form a single filled rectangle.
    let mut bounds: std::collections::HashMap<InternedStr, (usize, usize, usize, usize, usize)> =
        std::collections::HashMap::new();
    for (r_idx, row) in rows.iter().enumerate() {
        for (c_idx, cell) in row.iter().enumerate() {
            if let Some(name) = cell {
                bounds
                    .entry(name.clone())
                    .and_modify(|e| {
                        e.0 = e.0.min(r_idx);
                        e.1 = e.1.max(r_idx);
                        e.2 = e.2.min(c_idx);
                        e.3 = e.3.max(c_idx);
                        e.4 += 1;
                    })
                    .or_insert((r_idx, r_idx, c_idx, c_idx, 1));
            }
        }
    }
    for (min_r, max_r, min_c, max_c, count) in bounds.values() {
        let expected = (max_r - min_r + 1) * (max_c - min_c + 1);
        if *count != expected {
            return None;
        }
    }
    Some(GridTemplateAreasValue {
        columns: cols as u32,
        areas: flat,
    })
}

/// Parses CSS `grid-template-areas` (`none | <string>+`).
#[must_use]
pub fn parse_grid_template_areas(input: &str) -> Option<GridTemplateAreasValue> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut pi = ParserInput::new(trimmed);
    let mut p = Parser::new(&mut pi);
    if let Ok(ident) = p.try_parse(|p| p.expect_ident_cloned()) {
        p.skip_whitespace();
        return (ident.eq_ignore_ascii_case("none") && p.is_exhausted())
            .then(GridTemplateAreasValue::default);
    }
    let mut rows = Vec::new();
    while !p.is_exhausted() {
        p.skip_whitespace();
        if p.is_exhausted() {
            break;
        }
        let s = p.expect_string_cloned().ok()?;
        rows.push(tokenize_area_row(&s)?);
    }
    validate_and_build_areas(rows)
}

/// Parses CSS `<grid-line>` (`auto | <custom-ident> | [ <integer> && <custom-ident>? ] | [ span && [ <integer> || <custom-ident> ] ]`).
#[must_use]
pub fn parse_grid_line(input: &str) -> Option<GridPlacementValue> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut pi = ParserInput::new(trimmed);
    let mut p = Parser::new(&mut pi);
    let v = parse_grid_line_from_parser(&mut p)?;
    p.skip_whitespace();
    p.is_exhausted().then_some(v)
}

fn parse_grid_line_from_parser(p: &mut Parser<'_, '_>) -> Option<GridPlacementValue> {
    p.skip_whitespace();
    if p.is_exhausted() {
        return None;
    }
    let mut is_span = false;
    let mut span_pos: Option<usize> = None;
    let mut int_val: Option<i32> = None;
    let mut int_math: Option<Box<MathNode>> = None;
    let mut ident: Option<InternedStr> = None;
    let mut saw_auto = false;
    let mut token_count = 0usize;

    while !p.is_exhausted() {
        p.skip_whitespace();
        if p.is_exhausted() {
            break;
        }
        let state = p.state();
        let Ok(tok) = p.next().cloned() else {
            break;
        };
        match tok {
            Token::Delim('/') => {
                p.reset(&state);
                break;
            }
            Token::Ident(id) => {
                if id.eq_ignore_ascii_case("auto") {
                    if token_count > 0 {
                        return None;
                    }
                    saw_auto = true;
                    token_count += 1;
                } else if id.eq_ignore_ascii_case("span") {
                    if is_span || saw_auto {
                        return None;
                    }
                    is_span = true;
                    span_pos = Some(token_count);
                    token_count += 1;
                } else {
                    if ident.is_some() || saw_auto || is_invalid_grid_ident(&id) {
                        return None;
                    }
                    ident = Some(intern_str(&id));
                    token_count += 1;
                }
            }
            Token::Number {
                int_value: Some(v), ..
            } => {
                if int_val.is_some() || int_math.is_some() || saw_auto {
                    return None;
                }
                int_val = Some(v);
                token_count += 1;
            }
            Token::Function(name) => {
                if int_val.is_some() || int_math.is_some() || saw_auto {
                    return None;
                }
                let low = name.to_ascii_lowercase();
                let raw = consume_raw_function(p, &low)?;
                let pm = parse_math_value(&raw, ValueKind::Integer)?;
                int_math = Some(Box::new(pm.node));
                token_count += 1;
            }
            _ => return None,
        }
    }

    if token_count == 0 {
        return None;
    }
    if saw_auto {
        return (token_count == 1).then_some(GridPlacementValue::Auto);
    }
    if is_span {
        if token_count == 3 && span_pos == Some(1) {
            return None;
        }
        if int_val.is_none() && int_math.is_none() && ident.is_none() {
            return None;
        }
        if let Some(v) = int_val {
            if v <= 0 {
                return None;
            }
            return Some(GridPlacementValue::Span {
                count: v as u32,
                count_math: None,
                name: ident,
            });
        }
        return Some(GridPlacementValue::Span {
            count: 1,
            count_math: int_math,
            name: ident,
        });
    }
    if let Some(v) = int_val {
        if v == 0 {
            return None;
        }
        return Some(GridPlacementValue::Line {
            index: v,
            index_math: None,
            name: ident,
        });
    }
    if let Some(m) = int_math {
        return Some(GridPlacementValue::Line {
            index: 1,
            index_math: Some(m),
            name: ident,
        });
    }
    ident.map(GridPlacementValue::Named)
}

/// Parses CSS `grid-auto-flow` (`[ row | column ] || dense`).
#[must_use]
pub fn parse_grid_auto_flow(input: &str) -> Option<GridAutoFlowMode> {
    let mut dir: Option<bool> = None; // false = row, true = column
    let mut dense = false;
    let mut count = 0usize;
    for tok in input.split_ascii_whitespace() {
        count += 1;
        if tok.eq_ignore_ascii_case("row") {
            if dir.is_some() {
                return None;
            }
            dir = Some(false);
        } else if tok.eq_ignore_ascii_case("column") {
            if dir.is_some() {
                return None;
            }
            dir = Some(true);
        } else if tok.eq_ignore_ascii_case("dense") {
            if dense {
                return None;
            }
            dense = true;
        } else {
            return None;
        }
    }
    if count == 0 {
        return None;
    }
    Some(match (dir.unwrap_or(false), dense) {
        (false, false) => GridAutoFlowMode::Row,
        (true, false) => GridAutoFlowMode::Column,
        (false, true) => GridAutoFlowMode::RowDense,
        (true, true) => GridAutoFlowMode::ColumnDense,
    })
}

/// Parses `grid-row` or `grid-column` shorthand (`<grid-line> [ / <grid-line> ]?`).
#[must_use]
pub fn parse_grid_row_or_col_shorthand(
    input: &str,
) -> Option<(GridPlacementValue, GridPlacementValue)> {
    let mut pi = ParserInput::new(input.trim());
    let mut p = Parser::new(&mut pi);
    let first = parse_grid_line_from_parser(&mut p)?;
    p.skip_whitespace();
    if p.is_exhausted() {
        let second = match &first {
            GridPlacementValue::Named(_) => first.clone(),
            _ => GridPlacementValue::Auto,
        };
        return Some((first, second));
    }
    p.expect_delim('/').ok()?;
    let second = parse_grid_line_from_parser(&mut p)?;
    p.skip_whitespace();
    p.is_exhausted().then_some((first, second))
}

/// Parses `grid-area` shorthand (`<grid-line> [ / <grid-line> ]{0,3}`).
#[must_use]
pub fn parse_grid_area_shorthand(
    input: &str,
) -> Option<(
    GridPlacementValue,
    GridPlacementValue,
    GridPlacementValue,
    GridPlacementValue,
)> {
    let mut pi = ParserInput::new(input.trim());
    let mut p = Parser::new(&mut pi);
    let mut parts = Vec::with_capacity(4);
    parts.push(parse_grid_line_from_parser(&mut p)?);
    while !p.is_exhausted() {
        p.skip_whitespace();
        if p.is_exhausted() {
            break;
        }
        p.expect_delim('/').ok()?;
        if parts.len() >= 4 {
            return None;
        }
        parts.push(parse_grid_line_from_parser(&mut p)?);
    }
    let copy_if_ident = |v: &GridPlacementValue| match v {
        GridPlacementValue::Named(_) => v.clone(),
        _ => GridPlacementValue::Auto,
    };
    let rs = parts[0].clone();
    let cs = parts.get(1).cloned().unwrap_or_else(|| copy_if_ident(&rs));
    let re = parts.get(2).cloned().unwrap_or_else(|| copy_if_ident(&rs));
    let ce = parts.get(3).cloned().unwrap_or_else(|| copy_if_ident(&cs));
    Some((rs, cs, re, ce))
}

fn split_top_level_slash(input: &str) -> Option<(&str, Option<&str>)> {
    let bytes = input.as_bytes();
    let mut depth = 0usize;
    let mut in_str: Option<u8> = None;
    let mut slash_pos: Option<usize> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' => in_str = Some(b),
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b'/' if depth == 0 => {
                if slash_pos.is_some() {
                    return None; // More than one top-level `/`
                }
                slash_pos = Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    match slash_pos {
        Some(pos) => Some((input[..pos].trim(), Some(input[pos + 1..].trim()))),
        None => Some((input.trim(), None)),
    }
}

/// Parses `grid-template` shorthand into `(rows, columns, areas)`.
#[must_use]
pub fn parse_grid_template_shorthand(
    input: &str,
) -> Option<(
    GridTemplateTrackList,
    GridTemplateTrackList,
    GridTemplateAreasValue,
)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.eq_ignore_ascii_case("none") {
        return Some((
            GridTemplateTrackList::None,
            GridTemplateTrackList::None,
            GridTemplateAreasValue::default(),
        ));
    }
    let (lhs, rhs) = split_top_level_slash(trimmed)?;
    // Branch 1: Areas form (`[ <line-names>? <string> <track-size>? <line-names>? ]+ [ / <explicit-track-list> ]?`)
    if lhs.contains('"') || lhs.contains('\'') {
        let mut pi = ParserInput::new(lhs);
        let mut p = Parser::new(&mut pi);
        let mut area_rows = Vec::new();
        let mut row_items = Vec::new();
        let mut row_line_names: Vec<Vec<InternedStr>> = Vec::new();
        let mut pending: Vec<InternedStr> = Vec::new();

        while !p.is_exhausted() {
            p.skip_whitespace();
            if p.is_exhausted() {
                break;
            }
            if let Some(ln) = parse_line_names(&mut p) {
                pending.extend(ln);
                p.skip_whitespace();
            }
            let area_str = p.expect_string_cloned().ok()?;
            area_rows.push(tokenize_area_row(&area_str)?);
            p.skip_whitespace();
            let row_sz = parse_track_size(&mut p).unwrap_or(GridTrackSize::Auto);
            row_line_names.push(std::mem::take(&mut pending));
            row_items.push(GridTrackListEntry::Track(row_sz));
            p.skip_whitespace();
            if let Some(ln) = parse_line_names(&mut p) {
                pending.extend(ln);
            }
        }
        row_line_names.push(pending);
        let areas = validate_and_build_areas(area_rows)?;
        let rows = GridTemplateTrackList::Tracks {
            items: row_items,
            line_names: row_line_names,
        };
        let cols = if let Some(col_str) = rhs {
            let mut cpi = ParserInput::new(col_str);
            let mut cp = Parser::new(&mut cpi);
            let c = parse_track_list_from_parser(&mut cp, false, false)?;
            if c.is_none() {
                return None;
            }
            cp.skip_whitespace();
            if !cp.is_exhausted() {
                return None;
            }
            c
        } else {
            GridTemplateTrackList::None
        };
        return Some((rows, cols, areas));
    }

    // Branch 2: `<rows> / <cols>`
    let col_str = rhs?;
    let rows = parse_grid_template_track_list(lhs)?;
    let cols = parse_grid_template_track_list(col_str)?;
    Some((rows, cols, GridTemplateAreasValue::default()))
}

fn parse_auto_flow_side(input: &str) -> Option<(bool, Vec<GridTrackSize>)> {
    let mut pi = ParserInput::new(input.trim());
    let mut p = Parser::new(&mut pi);
    let mut saw_af = false;
    let mut dense = false;
    for _ in 0..2 {
        let st = p.state();
        if let Ok(ident) = p.try_parse(|p| p.expect_ident_cloned()) {
            if ident.eq_ignore_ascii_case("auto-flow") && !saw_af {
                saw_af = true;
                continue;
            }
            if ident.eq_ignore_ascii_case("dense") && !dense {
                dense = true;
                continue;
            }
        }
        p.reset(&st);
        break;
    }
    if !saw_af {
        return None;
    }
    p.skip_whitespace();
    let tracks = if p.is_exhausted() {
        vec![GridTrackSize::Auto]
    } else {
        let mut t = Vec::new();
        while !p.is_exhausted() {
            p.skip_whitespace();
            if p.is_exhausted() {
                break;
            }
            t.push(parse_track_size(&mut p)?);
        }
        t
    };
    Some((dense, tracks))
}

/// Parses CSS `grid` shorthand into `(rows, cols, areas, auto_rows, auto_cols, auto_flow)`.
#[must_use]
#[allow(clippy::type_complexity)]
pub fn parse_grid_shorthand(
    input: &str,
) -> Option<(
    GridTemplateTrackList,
    GridTemplateTrackList,
    GridTemplateAreasValue,
    Vec<GridTrackSize>,
    Vec<GridTrackSize>,
    GridAutoFlowMode,
)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((r, c, a)) = parse_grid_template_shorthand(trimmed) {
        return Some((
            r,
            c,
            a,
            vec![GridTrackSize::Auto],
            vec![GridTrackSize::Auto],
            GridAutoFlowMode::Row,
        ));
    }
    let (lhs, rhs_opt) = split_top_level_slash(trimmed)?;
    let rhs = rhs_opt?;
    // Form A: `[ auto-flow && dense? ] <auto-rows>? / <template-columns>`
    if let Some((dense, auto_rows)) = parse_auto_flow_side(lhs) {
        let cols = parse_grid_template_track_list(rhs)?;
        return Some((
            GridTemplateTrackList::None,
            cols,
            GridTemplateAreasValue::default(),
            auto_rows,
            vec![GridTrackSize::Auto],
            if dense {
                GridAutoFlowMode::RowDense
            } else {
                GridAutoFlowMode::Row
            },
        ));
    }
    // Form B: `<template-rows> / [ auto-flow && dense? ] <auto-columns>?`
    if let Some((dense, auto_cols)) = parse_auto_flow_side(rhs) {
        let rows = parse_grid_template_track_list(lhs)?;
        return Some((
            rows,
            GridTemplateTrackList::None,
            GridTemplateAreasValue::default(),
            vec![GridTrackSize::Auto],
            auto_cols,
            if dense {
                GridAutoFlowMode::ColumnDense
            } else {
                GridAutoFlowMode::Column
            },
        ));
    }
    None
}

/// Parses CSS Grid L3 `fit-tolerance` (`normal | infinite | <length-percentage [0,∞]>`).
#[must_use]
pub fn parse_fit_tolerance(input: &str) -> Option<FitToleranceValue> {
    let trimmed = input.trim();
    if trimmed.eq_ignore_ascii_case("normal") {
        return Some(FitToleranceValue::Normal);
    }
    if trimmed.eq_ignore_ascii_case("infinite") {
        return Some(FitToleranceValue::Infinite);
    }
    let mut pi = ParserInput::new(trimmed);
    let mut p = Parser::new(&mut pi);
    let lp = parse_nonneg_lp_from_parser(&mut p)?;
    p.skip_whitespace();
    p.is_exhausted().then_some(FitToleranceValue::Length(lp))
}

/// Parses CSS Grid L3 `grid-lanes-direction` (`normal | [ [row|column] || track-reverse || fill-reverse ]`).
#[must_use]
pub fn parse_grid_lanes_direction(input: &str) -> Option<GridLanesDirectionMode> {
    let toks: Vec<&str> = input.split_ascii_whitespace().collect();
    if toks.is_empty() {
        return None;
    }
    if toks.len() == 1 && toks[0].eq_ignore_ascii_case("normal") {
        return Some(GridLanesDirectionMode::Normal);
    }
    let mut axis: Option<bool> = None; // true = row, false = column
    let mut track_rev = false;
    let mut fill_rev = false;
    let mut track_before_fill = false;
    for t in toks {
        if t.eq_ignore_ascii_case("row") {
            if axis.is_some() {
                return None;
            }
            axis = Some(true);
        } else if t.eq_ignore_ascii_case("column") {
            if axis.is_some() {
                return None;
            }
            axis = Some(false);
        } else if t.eq_ignore_ascii_case("track-reverse") {
            if track_rev {
                return None;
            }
            track_rev = true;
            if !fill_rev {
                track_before_fill = true;
            }
        } else if t.eq_ignore_ascii_case("fill-reverse") {
            if fill_rev {
                return None;
            }
            fill_rev = true;
        } else {
            return None;
        }
    }
    let is_row = axis?;
    Some(GridLanesDirectionMode::Explicit {
        is_row,
        track_reverse: track_rev,
        fill_reverse: fill_rev,
        track_before_fill: track_rev && fill_rev && track_before_fill,
    })
}

/// Parses CSS Grid L3 `grid-lanes-pack` (`normal | dense`).
#[must_use]
pub fn parse_grid_lanes_pack(input: &str) -> Option<GridLanesPackMode> {
    let t = input.trim();
    if t.eq_ignore_ascii_case("normal") {
        Some(GridLanesPackMode::Normal)
    } else if t.eq_ignore_ascii_case("dense") {
        Some(GridLanesPackMode::Dense)
    } else {
        None
    }
}

/// Parses CSS Grid L3 `grid-lanes` shorthand into `(cols, rows, areas, direction)`.
#[must_use]
pub fn parse_grid_lanes_shorthand(
    input: &str,
) -> Option<(
    GridTemplateTrackList,
    GridTemplateTrackList,
    GridTemplateAreasValue,
    GridLanesDirectionMode,
)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut pi = ParserInput::new(trimmed);
    let mut p = Parser::new(&mut pi);

    let mut area_str: Option<String> = None;
    let mut dir_normal = false;
    let mut dir_axis: Option<bool> = None;
    let mut track_rev = false;
    let mut fill_rev = false;
    let mut track_before_fill = false;
    let mut track_tokens: Vec<String> = Vec::new();
    let mut saw_none = false;

    while !p.is_exhausted() {
        p.skip_whitespace();
        if p.is_exhausted() {
            break;
        }
        let st = p.state();
        if let Ok(s) = p.try_parse(|p| p.expect_string_cloned()) {
            if area_str.is_some() {
                return None;
            }
            area_str = Some(s.to_string());
            continue;
        }
        if let Ok(id) = p.try_parse(|p| p.expect_ident_cloned()) {
            let low = id.to_ascii_lowercase();
            match low.as_str() {
                "normal" => {
                    if dir_normal || dir_axis.is_some() || track_rev || fill_rev {
                        return None;
                    }
                    dir_normal = true;
                    continue;
                }
                "row" => {
                    if dir_normal || dir_axis.is_some() {
                        return None;
                    }
                    dir_axis = Some(true);
                    continue;
                }
                "column" => {
                    if dir_normal || dir_axis.is_some() {
                        return None;
                    }
                    dir_axis = Some(false);
                    continue;
                }
                "track-reverse" => {
                    if dir_normal || track_rev {
                        return None;
                    }
                    track_rev = true;
                    if !fill_rev {
                        track_before_fill = true;
                    }
                    continue;
                }
                "fill-reverse" => {
                    if dir_normal || fill_rev {
                        return None;
                    }
                    fill_rev = true;
                    continue;
                }
                "none" => {
                    if saw_none || !track_tokens.is_empty() {
                        return None;
                    }
                    saw_none = true;
                    continue;
                }
                _ => {
                    p.reset(&st);
                }
            }
        }
        // Must be a track-list component (`[names]`, `repeat(...)`, or `<track-size>`).
        if saw_none {
            return None;
        }
        let comp_start = p.position();
        if parse_line_names(&mut p).is_some()
            || parse_track_repeat(&mut p).is_some()
            || parse_track_size(&mut p).is_some()
        {
            track_tokens.push(p.slice_from(comp_start).trim().to_string());
            continue;
        }
        return None;
    }

    if (track_rev || fill_rev) && dir_axis.is_none() {
        return None;
    }
    let dir = if dir_normal {
        GridLanesDirectionMode::Normal
    } else if let Some(is_row) = dir_axis {
        GridLanesDirectionMode::Explicit {
            is_row,
            track_reverse: track_rev,
            fill_reverse: fill_rev,
            track_before_fill: track_rev && fill_rev && track_before_fill,
        }
    } else {
        GridLanesDirectionMode::Explicit {
            is_row: false,
            track_reverse: false,
            fill_reverse: false,
            track_before_fill: false,
        }
    };
    let is_row = matches!(dir, GridLanesDirectionMode::Explicit { is_row: true, .. });
    let tracks = if saw_none || track_tokens.is_empty() {
        GridTemplateTrackList::None
    } else {
        let joined = track_tokens.join(" ");
        parse_grid_template_track_list(&joined)?
    };
    let areas = if let Some(astr) = area_str {
        let row_cells = tokenize_area_row(&astr)?;
        if is_row {
            let area_rows: Vec<Vec<Option<InternedStr>>> =
                row_cells.into_iter().map(|c| vec![c]).collect();
            validate_and_build_areas(area_rows)?
        } else {
            validate_and_build_areas(vec![row_cells])?
        }
    } else {
        GridTemplateAreasValue::default()
    };
    let (cols, rows) = if is_row {
        (GridTemplateTrackList::None, tracks)
    } else {
        (tracks, GridTemplateTrackList::None)
    };
    Some((cols, rows, areas, dir))
}

// ---------------------------------------------------------------------------
// Canonical serialization (specified & computed)
// ---------------------------------------------------------------------------

fn format_lp_specified(lp: &LengthPercentage) -> String {
    match lp {
        LengthPercentage::Zero => "0px".to_string(),
        LengthPercentage::Length(v, u) => {
            format!("{}{}", format_math_f64(f64::from(*v)), u.as_str())
        }
        LengthPercentage::Percent(p) => format!("{}%", format_math_f64(f64::from(*p) * 100.0)),
        LengthPercentage::Calc(c) => match c.as_ref() {
            CalcExpr::Math(m) => m.serialize_specified(true),
            other => {
                let eval = EvalContext::default();
                if other.has_percentage() {
                    let px = other.resolve_px(0.0, &eval);
                    let pct = other.resolve_px(100.0, &eval) - px;
                    format!(
                        "calc({}% + {}px)",
                        format_math_f64(f64::from(pct)),
                        format_math_f64(f64::from(px))
                    )
                } else {
                    format!(
                        "{}px",
                        format_math_f64(f64::from(other.resolve_px(0.0, &eval)))
                    )
                }
            }
        },
    }
}

fn format_lp_computed(lp: &LengthPercentage, eval: &EvalContext) -> String {
    match lp {
        LengthPercentage::Zero => "0px".to_string(),
        LengthPercentage::Length(v, u) => {
            let px = LengthPercentage::Length(*v, *u)
                .resolve_px(0.0, eval)
                .max(0.0);
            format!("{}px", format_math_f64(f64::from(px)))
        }
        LengthPercentage::Percent(p) => {
            format!("{}%", format_math_f64((f64::from(*p) * 100.0).max(0.0)))
        }
        LengthPercentage::Calc(c) => match c.as_ref() {
            CalcExpr::Math(m) => {
                let simp = m.simplify(Some(eval), None);
                if simp.has_percentage() {
                    if let Some((px, pct)) = extract_linear_px_pct(&simp, eval) {
                        format!(
                            "calc({}% + {}px)",
                            format_math_f64(pct),
                            format_math_f64(px)
                        )
                    } else {
                        simp.serialize_computed(
                            eval,
                            None,
                            ValueKind::LengthPercentage,
                            Some(0.0),
                            None,
                        )
                    }
                } else {
                    let px = simp.eval_scalar(eval, None).max(0.0);
                    format!("{}px", format_math_f64(px))
                }
            }
            other => {
                if other.has_percentage() {
                    let px = other.resolve_px(0.0, eval);
                    let pct = other.resolve_px(100.0, eval) - px;
                    format!(
                        "calc({}% + {}px)",
                        format_math_f64(f64::from(pct)),
                        format_math_f64(f64::from(px))
                    )
                } else {
                    let px = other.resolve_px(0.0, eval).max(0.0);
                    format!("{}px", format_math_f64(f64::from(px)))
                }
            }
        },
    }
}

fn extract_linear_px_pct(node: &MathNode, eval: &EvalContext) -> Option<(f64, f64)> {
    match node {
        MathNode::Dimension(v, crate::values_math::MathUnit::Length(u)) => {
            let px = u.resolve_px(*v as f32, eval);
            Some((f64::from(px), 0.0))
        }
        MathNode::Percent(p) => Some((0.0, *p)),
        MathNode::Number(n) if *n == 0.0 => Some((0.0, 0.0)),
        MathNode::Sum(terms) => {
            let (mut px, mut pct) = (0.0, 0.0);
            for t in terms {
                let (a, b) = extract_linear_px_pct(t, eval)?;
                px += a;
                pct += b;
            }
            Some((px, pct))
        }
        MathNode::Negate(inner) => {
            let (a, b) = extract_linear_px_pct(inner, eval)?;
            Some((-a, -b))
        }
        _ => None,
    }
}

fn format_line_names(names: &[InternedStr]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let joined: Vec<String> = names.iter().map(|n| escape_css_ident(n.as_str())).collect();
    Some(format!("[{}]", joined.join(" ")))
}

#[must_use]
pub fn serialize_track_size(sz: &GridTrackSize, eval: Option<&EvalContext>) -> String {
    match sz {
        GridTrackSize::Auto => "auto".to_string(),
        GridTrackSize::MinContent => "min-content".to_string(),
        GridTrackSize::MaxContent => "max-content".to_string(),
        GridTrackSize::Length(lp) => match eval {
            Some(e) => format_lp_computed(lp, e),
            None => format_lp_specified(lp),
        },
        GridTrackSize::Fr(v) => format!("{}fr", format_math_f64(f64::from((*v).max(0.0)))),
        GridTrackSize::FrMath(m) => match eval {
            Some(e) => format!("{}fr", format_math_f64(m.eval_scalar(e, None).max(0.0))),
            None => m.serialize_specified(true),
        },
        GridTrackSize::MinMax(a, b) => format!(
            "minmax({}, {})",
            serialize_track_size(a, eval),
            serialize_track_size(b, eval)
        ),
        GridTrackSize::FitContent(lp) => {
            let inner = match eval {
                Some(e) => format_lp_computed(lp, e),
                None => format_lp_specified(lp),
            };
            format!("fit-content({inner})")
        }
    }
}

#[must_use]
pub fn serialize_track_list(list: &GridTemplateTrackList, eval: Option<&EvalContext>) -> String {
    match list {
        GridTemplateTrackList::None => "none".to_string(),
        GridTemplateTrackList::Subgrid(items) => {
            let mut parts = vec!["subgrid".to_string()];
            for it in items {
                match it {
                    GridSubgridLineItem::Names(n) => {
                        let inner: Vec<String> =
                            n.iter().map(|x| escape_css_ident(x.as_str())).collect();
                        parts.push(format!("[{}]", inner.join(" ")));
                    }
                    GridSubgridLineItem::Repeat { count, names } => {
                        let cnt = format_repeat_count(count, eval);
                        let inner: Vec<String> = names
                            .iter()
                            .map(|n| {
                                let s: Vec<String> =
                                    n.iter().map(|x| escape_css_ident(x.as_str())).collect();
                                format!("[{}]", s.join(" "))
                            })
                            .collect();
                        parts.push(format!("repeat({cnt}, {})", inner.join(" ")));
                    }
                }
            }
            parts.join(" ")
        }
        GridTemplateTrackList::Tracks { items, line_names } => {
            let mut parts = Vec::new();
            for (idx, entry) in items.iter().enumerate() {
                if let Some(ln) = line_names.get(idx).and_then(|n| format_line_names(n)) {
                    parts.push(ln);
                }
                match entry {
                    GridTrackListEntry::Track(sz) => parts.push(serialize_track_size(sz, eval)),
                    GridTrackListEntry::Repeat {
                        count,
                        tracks,
                        line_names: r_names,
                    } => {
                        let cnt = format_repeat_count(count, eval);
                        let mut rparts = Vec::new();
                        for (ti, t) in tracks.iter().enumerate() {
                            if let Some(ln) = r_names.get(ti).and_then(|n| format_line_names(n)) {
                                rparts.push(ln);
                            }
                            rparts.push(serialize_track_size(t, eval));
                        }
                        if let Some(ln) =
                            r_names.get(tracks.len()).and_then(|n| format_line_names(n))
                        {
                            rparts.push(ln);
                        }
                        parts.push(format!("repeat({cnt}, {})", rparts.join(" ")));
                    }
                }
            }
            if let Some(ln) = line_names
                .get(items.len())
                .and_then(|n| format_line_names(n))
            {
                parts.push(ln);
            }
            parts.join(" ")
        }
    }
}

fn format_repeat_count(count: &GridRepeatCount, eval: Option<&EvalContext>) -> String {
    match count {
        GridRepeatCount::Number(n) => n.to_string(),
        GridRepeatCount::NumberMath(m) => match eval {
            Some(e) => ((m.eval_scalar(e, None).round()).max(1.0) as u32).to_string(),
            None => m.serialize_specified(true),
        },
        GridRepeatCount::AutoFill => "auto-fill".to_string(),
        GridRepeatCount::AutoFit => "auto-fit".to_string(),
    }
}

#[must_use]
pub fn serialize_auto_tracks(tracks: &[GridTrackSize], eval: Option<&EvalContext>) -> String {
    if tracks.is_empty() {
        return "auto".to_string();
    }
    tracks
        .iter()
        .map(|t| serialize_track_size(t, eval))
        .collect::<Vec<_>>()
        .join(" ")
}

#[must_use]
pub fn serialize_grid_areas(areas: &GridTemplateAreasValue) -> String {
    let cols = areas.columns as usize;
    if cols == 0 || areas.areas.is_empty() {
        return "none".to_string();
    }
    areas
        .areas
        .chunks(cols)
        .map(|row| {
            let tokens: Vec<&str> = row
                .iter()
                .map(|c| c.as_ref().map_or(".", InternedStr::as_str))
                .collect();
            format!("\"{}\"", tokens.join(" "))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[must_use]
pub fn serialize_grid_auto_flow(mode: GridAutoFlowMode) -> &'static str {
    match mode {
        GridAutoFlowMode::Row => "row",
        GridAutoFlowMode::Column => "column",
        GridAutoFlowMode::RowDense => "dense",
        GridAutoFlowMode::ColumnDense => "column dense",
    }
}

#[must_use]
pub fn serialize_grid_line(val: &GridPlacementValue, eval: Option<&EvalContext>) -> String {
    match val {
        GridPlacementValue::Auto => "auto".to_string(),
        GridPlacementValue::Named(n) => escape_css_ident(n.as_str()),
        GridPlacementValue::Line {
            index,
            index_math,
            name,
        } => {
            let num_s = if let Some(m) = index_math {
                match eval {
                    Some(e) => {
                        let v = m.eval_scalar(e, None).round() as i32;
                        if v == 0 {
                            "1".to_string()
                        } else {
                            v.to_string()
                        }
                    }
                    None => m.serialize_specified(true),
                }
            } else {
                index.to_string()
            };
            match name {
                Some(n) => format!("{num_s} {}", escape_css_ident(n.as_str())),
                None => num_s,
            }
        }
        GridPlacementValue::Span {
            count,
            count_math,
            name,
        } => {
            let cnt_str = if let Some(m) = count_math {
                match eval {
                    Some(e) => {
                        let v = (m.eval_scalar(e, None).round() as i32).max(1) as u32;
                        if v == 1 && name.is_some() {
                            None
                        } else {
                            Some(v.to_string())
                        }
                    }
                    None => Some(m.serialize_specified(true)),
                }
            } else if *count == 1 && name.is_some() {
                None
            } else {
                Some(count.to_string())
            };
            match (cnt_str, name) {
                (Some(c), Some(n)) => format!("span {c} {}", escape_css_ident(n.as_str())),
                (Some(c), None) => format!("span {c}"),
                (None, Some(n)) => format!("span {}", escape_css_ident(n.as_str())),
                (None, None) => "span 1".to_string(),
            }
        }
    }
}

#[must_use]
pub fn serialize_grid_row_or_col_shortest(
    start: &GridPlacementValue,
    end: &GridPlacementValue,
    eval: Option<&EvalContext>,
) -> String {
    let s = serialize_grid_line(start, eval);
    let e = serialize_grid_line(end, eval);
    let omit_end = match start {
        GridPlacementValue::Named(_) => s == e,
        _ => *end == GridPlacementValue::Auto,
    };
    if omit_end {
        s
    } else {
        format!("{s} / {e}")
    }
}

#[must_use]
pub fn serialize_grid_area_shortest(
    rs: &GridPlacementValue,
    cs: &GridPlacementValue,
    re: &GridPlacementValue,
    ce: &GridPlacementValue,
    eval: Option<&EvalContext>,
) -> String {
    let s_rs = serialize_grid_line(rs, eval);
    let s_cs = serialize_grid_line(cs, eval);
    let s_re = serialize_grid_line(re, eval);
    let s_ce = serialize_grid_line(ce, eval);
    let ce_default = match cs {
        GridPlacementValue::Named(_) => s_ce == s_cs,
        _ => *ce == GridPlacementValue::Auto,
    };
    if !ce_default {
        return format!("{s_rs} / {s_cs} / {s_re} / {s_ce}");
    }
    let re_default = match rs {
        GridPlacementValue::Named(_) => s_re == s_rs,
        _ => *re == GridPlacementValue::Auto,
    };
    if !re_default {
        return format!("{s_rs} / {s_cs} / {s_re}");
    }
    let cs_default = match rs {
        GridPlacementValue::Named(_) => s_cs == s_rs,
        _ => *cs == GridPlacementValue::Auto,
    };
    if !cs_default {
        return format!("{s_rs} / {s_cs}");
    }
    s_rs
}

#[must_use]
pub fn serialize_fit_tolerance(val: &FitToleranceValue, eval: Option<&EvalContext>) -> String {
    match val {
        FitToleranceValue::Normal => "normal".to_string(),
        FitToleranceValue::Infinite => "infinite".to_string(),
        FitToleranceValue::Length(lp) => match eval {
            Some(e) => format_lp_computed(lp, e),
            None => format_lp_specified(lp),
        },
    }
}

#[must_use]
pub fn serialize_grid_lanes_direction(dir: GridLanesDirectionMode) -> String {
    serialize_grid_lanes_direction_mode(dir, false)
}

#[must_use]
pub fn serialize_grid_lanes_direction_computed(dir: GridLanesDirectionMode) -> String {
    serialize_grid_lanes_direction_mode(dir, true)
}

fn serialize_grid_lanes_direction_mode(dir: GridLanesDirectionMode, is_computed: bool) -> String {
    match dir {
        GridLanesDirectionMode::Normal => "normal".to_string(),
        GridLanesDirectionMode::Explicit {
            is_row,
            track_reverse,
            fill_reverse,
            track_before_fill,
        } => {
            let mut out = String::from(if is_row { "row" } else { "column" });
            if !is_computed && track_before_fill {
                if track_reverse {
                    out.push_str(" track-reverse");
                }
                if fill_reverse {
                    out.push_str(" fill-reverse");
                }
            } else {
                if fill_reverse {
                    out.push_str(" fill-reverse");
                }
                if track_reverse {
                    out.push_str(" track-reverse");
                }
            }
            out
        }
    }
}

#[must_use]
pub fn serialize_grid_lanes_pack(pack: GridLanesPackMode) -> &'static str {
    match pack {
        GridLanesPackMode::Normal => "normal",
        GridLanesPackMode::Dense => "dense",
    }
}

/// Serializes `grid-template` shorthand from parsed longhand values (returns `None` if non-roundtrippable).
#[must_use]
pub fn serialize_grid_template_from_values(
    rows: &GridTemplateTrackList,
    cols: &GridTemplateTrackList,
    areas: &GridTemplateAreasValue,
) -> Option<String> {
    if areas.columns == 0 || areas.areas.is_empty() {
        if rows.is_none() && cols.is_none() {
            return Some("none".to_string());
        }
        return Some(format!(
            "{} / {}",
            serialize_track_list(rows, None),
            serialize_track_list(cols, None)
        ));
    }
    if cols.has_repeat() || cols.is_subgrid() || rows.has_repeat() || rows.is_subgrid() {
        return None;
    }
    let num_area_rows = areas.areas.len() / (areas.columns as usize);
    let GridTemplateTrackList::Tracks { items, line_names } = rows else {
        return None;
    };
    if items.len() != num_area_rows {
        return None;
    }
    let mut parts = Vec::new();
    for (idx, chunk) in areas.areas.chunks(areas.columns as usize).enumerate() {
        if let Some(ln) = line_names.get(idx).and_then(|n| format_line_names(n)) {
            parts.push(ln);
        }
        let tokens: Vec<&str> = chunk
            .iter()
            .map(|c| c.as_ref().map_or(".", InternedStr::as_str))
            .collect();
        parts.push(format!("\"{}\"", tokens.join(" ")));
        if let GridTrackListEntry::Track(sz) = &items[idx] {
            if *sz != GridTrackSize::Auto {
                parts.push(serialize_track_size(sz, None));
            }
        }
    }
    if let Some(ln) = line_names
        .get(num_area_rows)
        .and_then(|n| format_line_names(n))
    {
        parts.push(ln);
    }
    let mut out = parts.join(" ");
    if !cols.is_none() {
        out.push_str(" / ");
        out.push_str(&serialize_track_list(cols, None));
    }
    Some(out)
}

/// Serializes `grid` shorthand from parsed longhand values (returns `None` if non-roundtrippable).
#[must_use]
pub fn serialize_grid_from_values(
    rows: &GridTemplateTrackList,
    cols: &GridTemplateTrackList,
    areas: &GridTemplateAreasValue,
    auto_rows: &[GridTrackSize],
    auto_cols: &[GridTrackSize],
    auto_flow: GridAutoFlowMode,
) -> Option<String> {
    let ar_is_auto =
        auto_rows.is_empty() || (auto_rows.len() == 1 && auto_rows[0] == GridTrackSize::Auto);
    let ac_is_auto =
        auto_cols.is_empty() || (auto_cols.len() == 1 && auto_cols[0] == GridTrackSize::Auto);
    if ar_is_auto && ac_is_auto && auto_flow == GridAutoFlowMode::Row {
        return serialize_grid_template_from_values(rows, cols, areas);
    }
    if areas.columns > 0 && !areas.areas.is_empty() {
        return None;
    }
    match auto_flow {
        GridAutoFlowMode::Row | GridAutoFlowMode::RowDense => {
            if !rows.is_none() || !ac_is_auto {
                return None;
            }
            let mut lhs = String::from("auto-flow");
            if auto_flow == GridAutoFlowMode::RowDense {
                lhs.push_str(" dense");
            }
            if !ar_is_auto {
                lhs.push(' ');
                lhs.push_str(&serialize_auto_tracks(auto_rows, None));
            }
            Some(format!("{lhs} / {}", serialize_track_list(cols, None)))
        }
        GridAutoFlowMode::Column | GridAutoFlowMode::ColumnDense => {
            if !cols.is_none() || !ar_is_auto {
                return None;
            }
            let mut rhs = String::from("auto-flow");
            if auto_flow == GridAutoFlowMode::ColumnDense {
                rhs.push_str(" dense");
            }
            if !ac_is_auto {
                rhs.push(' ');
                rhs.push_str(&serialize_auto_tracks(auto_cols, None));
            }
            Some(format!("{} / {rhs}", serialize_track_list(rows, None)))
        }
    }
}

/// Serializes CSS Grid L3 `grid-lanes` shorthand from computed/specified longhands.
#[must_use]
pub fn serialize_grid_lanes_from_values(
    cols: &GridTemplateTrackList,
    rows: &GridTemplateTrackList,
    areas: &GridTemplateAreasValue,
    dir: GridLanesDirectionMode,
    eval: Option<&EvalContext>,
) -> Option<String> {
    let is_row = matches!(dir, GridLanesDirectionMode::Explicit { is_row: true, .. });
    let active = if is_row { rows } else { cols };
    let mut parts = Vec::new();
    if areas.columns > 0 && !areas.areas.is_empty() {
        if is_row {
            if areas.columns != 1 {
                return None;
            }
            let toks: Vec<&str> = areas
                .areas
                .iter()
                .map(|c| c.as_ref().map_or(".", InternedStr::as_str))
                .collect();
            parts.push(format!("\"{}\"", toks.join(" ")));
        } else {
            if areas.areas.len() != areas.columns as usize {
                return None;
            }
            parts.push(serialize_grid_areas(areas));
        }
    }
    if !active.is_none() {
        parts.push(serialize_track_list(active, eval));
    }
    parts.push(serialize_grid_lanes_direction_mode(dir, eval.is_some()));
    Some(parts.join(" "))
}

// ---------------------------------------------------------------------------
// Interpolation & Composition (CSS Grid L1 §7.2.7 + CSS Grid L3 fit-tolerance)
// ---------------------------------------------------------------------------

fn lp_to_px_pct(lp: &LengthPercentage, eval: &EvalContext) -> Option<(f64, f64, bool)> {
    match lp {
        LengthPercentage::Zero => Some((0.0, 0.0, false)),
        LengthPercentage::Length(v, u) => {
            let px = LengthPercentage::Length(*v, *u).resolve_px(0.0, eval);
            Some((f64::from(px), 0.0, false))
        }
        LengthPercentage::Percent(p) => Some((0.0, f64::from(*p) * 100.0, true)),
        LengthPercentage::Calc(c) => match c.as_ref() {
            CalcExpr::Math(m) => {
                let simp = m.simplify(Some(eval), None);
                if simp.has_percentage() {
                    let (px, pct) = extract_linear_px_pct(&simp, eval)?;
                    Some((px, pct, true))
                } else {
                    Some((simp.eval_scalar(eval, None), 0.0, false))
                }
            }
            other => {
                if other.has_percentage() {
                    let px = f64::from(other.resolve_px(0.0, eval));
                    let pct = f64::from(other.resolve_px(100.0, eval)) - px;
                    Some((px, pct, true))
                } else {
                    Some((f64::from(other.resolve_px(0.0, eval)), 0.0, false))
                }
            }
        },
    }
}

fn lp_from_px_pct(px: f64, pct: f64, had_px: bool, had_pct: bool) -> LengthPercentage {
    if had_px && had_pct {
        let sum = MathNode::Sum(vec![
            MathNode::Percent(pct),
            MathNode::Dimension(px, crate::values_math::MathUnit::Length(LengthUnit::Px)),
        ]);
        LengthPercentage::Calc(Box::new(CalcExpr::Math(Box::new(sum))))
    } else if had_pct {
        LengthPercentage::Percent((pct.max(0.0) / 100.0) as f32)
    } else {
        LengthPercentage::px((px.max(0.0)) as f32)
    }
}

fn interp_lp_nonneg(
    a: &LengthPercentage,
    b: &LengthPercentage,
    p: f64,
    eval: &EvalContext,
) -> LengthPercentage {
    if let (Some((apx, apct, a_has_pct)), Some((bpx, bpct, b_has_pct))) =
        (lp_to_px_pct(a, eval), lp_to_px_pct(b, eval))
    {
        let a_has_px = !a_has_pct || apx.abs() > 1e-6;
        let b_has_px = !b_has_pct || bpx.abs() > 1e-6;
        let mix_pct = a_has_pct || b_has_pct;
        let mix_px = a_has_px || b_has_px;
        let inv = 1.0 - p;
        let rpx = apx * inv + bpx * p;
        let rpct = apct * inv + bpct * p;
        if (p - 1.0).abs() < 1e-9 {
            return b.clone();
        }
        lp_from_px_pct(rpx, rpct, mix_px, mix_pct)
    } else if p < 0.5 {
        a.clone()
    } else {
        b.clone()
    }
}

fn compose_lp_nonneg(
    u: &LengthPercentage,
    k: &LengthPercentage,
    eval: &EvalContext,
) -> LengthPercentage {
    if let (Some((upx, upct, u_has_pct)), Some((kpx, kpct, k_has_pct))) =
        (lp_to_px_pct(u, eval), lp_to_px_pct(k, eval))
    {
        let u_has_px = !u_has_pct || upx.abs() > 1e-6;
        let k_has_px = !k_has_pct || kpx.abs() > 1e-6;
        let rpx = upx + kpx;
        let rpct = upct + kpct;
        lp_from_px_pct(
            rpx,
            rpct,
            (u_has_px || k_has_px) && (rpx.abs() > 1e-6 || !(u_has_pct || k_has_pct)),
            u_has_pct || k_has_pct,
        )
    } else {
        k.clone()
    }
}

fn eval_fr(sz: &GridTrackSize, eval: &EvalContext) -> Option<f64> {
    match sz {
        GridTrackSize::Fr(v) => Some(f64::from(*v)),
        GridTrackSize::FrMath(m) => Some(m.eval_scalar(eval, None).max(0.0)),
        _ => None,
    }
}

fn interpolate_track_size(
    a: &GridTrackSize,
    b: &GridTrackSize,
    p: f64,
    eval: &EvalContext,
) -> GridTrackSize {
    if (p - 1.0).abs() < 1e-9 {
        return b.clone();
    }
    match (a, b) {
        (GridTrackSize::Length(la), GridTrackSize::Length(lb)) => {
            GridTrackSize::Length(interp_lp_nonneg(la, lb, p, eval))
        }
        (GridTrackSize::FitContent(la), GridTrackSize::FitContent(lb)) => {
            GridTrackSize::FitContent(interp_lp_nonneg(la, lb, p, eval))
        }
        (GridTrackSize::MinMax(a1, a2), GridTrackSize::MinMax(b1, b2)) => GridTrackSize::MinMax(
            Box::new(interpolate_track_size(a1, b1, p, eval)),
            Box::new(interpolate_track_size(a2, b2, p, eval)),
        ),
        _ => {
            if let (Some(fa), Some(fb)) = (eval_fr(a, eval), eval_fr(b, eval)) {
                let v = (fa * (1.0 - p) + fb * p).max(0.0);
                GridTrackSize::Fr(v as f32)
            } else if p < 0.5 {
                a.clone()
            } else {
                b.clone()
            }
        }
    }
}

fn compose_track_size(u: &GridTrackSize, k: &GridTrackSize, eval: &EvalContext) -> GridTrackSize {
    match (u, k) {
        (GridTrackSize::Length(lu), GridTrackSize::Length(lk)) => {
            GridTrackSize::Length(compose_lp_nonneg(lu, lk, eval))
        }
        (GridTrackSize::FitContent(lu), GridTrackSize::FitContent(lk)) => {
            GridTrackSize::FitContent(compose_lp_nonneg(lu, lk, eval))
        }
        (GridTrackSize::MinMax(u1, u2), GridTrackSize::MinMax(k1, k2)) => GridTrackSize::MinMax(
            Box::new(compose_track_size(u1, k1, eval)),
            Box::new(compose_track_size(u2, k2, eval)),
        ),
        _ => {
            if let (Some(fu), Some(fk)) = (eval_fr(u, eval), eval_fr(k, eval)) {
                GridTrackSize::Fr((fu + fk).max(0.0) as f32)
            } else {
                k.clone()
            }
        }
    }
}

fn track_lists_interpolable(
    a: &GridTemplateTrackList,
    b: &GridTemplateTrackList,
    eval: &EvalContext,
) -> bool {
    let (
        GridTemplateTrackList::Tracks { items: ia, .. },
        GridTemplateTrackList::Tracks { items: ib, .. },
    ) = (a, b)
    else {
        return false;
    };
    if ia.len() != ib.len() {
        return false;
    }
    for (ea, eb) in ia.iter().zip(ib.iter()) {
        match (ea, eb) {
            (GridTrackListEntry::Track(_), GridTrackListEntry::Track(_)) => {}
            (
                GridTrackListEntry::Repeat {
                    count: ca,
                    tracks: ta,
                    ..
                },
                GridTrackListEntry::Repeat {
                    count: cb,
                    tracks: tb,
                    ..
                },
            ) => {
                // Auto-repeats cannot be interpolated per CSS Grid L1 §7.2.7.
                let (Some(na), Some(nb)) = (ca.resolve_number(eval), cb.resolve_number(eval))
                else {
                    return false;
                };
                if na != nb || ta.len() != tb.len() {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Interpolates two `GridTemplateTrackList` values per CSS Grid Level 1 §7.2.7.
/// Returns `None` when the two lists are incompatible (requiring discrete fallback).
#[must_use]
pub fn interpolate_track_list(
    from: &GridTemplateTrackList,
    to: &GridTemplateTrackList,
    p: f64,
    eval: &EvalContext,
) -> Option<GridTemplateTrackList> {
    if !track_lists_interpolable(from, to, eval) {
        return None;
    }
    let (
        GridTemplateTrackList::Tracks {
            items: ia,
            line_names: lna,
        },
        GridTemplateTrackList::Tracks {
            items: ib,
            line_names: lnb,
        },
    ) = (from, to)
    else {
        return None;
    };
    let line_names = if p < 0.5 { lna.clone() } else { lnb.clone() };
    let mut items = Vec::with_capacity(ia.len());
    for (ea, eb) in ia.iter().zip(ib.iter()) {
        match (ea, eb) {
            (GridTrackListEntry::Track(sa), GridTrackListEntry::Track(sb)) => {
                items.push(GridTrackListEntry::Track(interpolate_track_size(
                    sa, sb, p, eval,
                )));
            }
            (
                GridTrackListEntry::Repeat {
                    count: ca,
                    tracks: ta,
                    line_names: rla,
                },
                GridTrackListEntry::Repeat {
                    tracks: tb,
                    line_names: rlb,
                    ..
                },
            ) => {
                let n = ca.resolve_number(eval).unwrap_or(1);
                let tracks = ta
                    .iter()
                    .zip(tb.iter())
                    .map(|(x, y)| interpolate_track_size(x, y, p, eval))
                    .collect();
                items.push(GridTrackListEntry::Repeat {
                    count: GridRepeatCount::Number(n),
                    tracks,
                    line_names: if p < 0.5 { rla.clone() } else { rlb.clone() },
                });
            }
            _ => return None,
        }
    }
    Some(GridTemplateTrackList::Tracks { items, line_names })
}

/// Composes (`add` / `accumulate`) a keyframe `GridTemplateTrackList` onto an underlying value.
#[must_use]
pub fn compose_track_list(
    underlying: &GridTemplateTrackList,
    kf: &GridTemplateTrackList,
    mode: AnimationCompositionMode,
    eval: &EvalContext,
) -> GridTemplateTrackList {
    if mode == AnimationCompositionMode::Replace || !track_lists_interpolable(underlying, kf, eval)
    {
        return kf.clone();
    }
    let (
        GridTemplateTrackList::Tracks { items: iu, .. },
        GridTemplateTrackList::Tracks {
            items: ik,
            line_names: lnk,
        },
    ) = (underlying, kf)
    else {
        return kf.clone();
    };
    let mut items = Vec::with_capacity(ik.len());
    for (eu, ek) in iu.iter().zip(ik.iter()) {
        match (eu, ek) {
            (GridTrackListEntry::Track(su), GridTrackListEntry::Track(sk)) => {
                items.push(GridTrackListEntry::Track(compose_track_size(su, sk, eval)));
            }
            (
                GridTrackListEntry::Repeat { tracks: tu, .. },
                GridTrackListEntry::Repeat {
                    count: ck,
                    tracks: tk,
                    line_names: rlk,
                },
            ) => {
                let n = ck.resolve_number(eval).unwrap_or(1);
                let tracks = tu
                    .iter()
                    .zip(tk.iter())
                    .map(|(x, y)| compose_track_size(x, y, eval))
                    .collect();
                items.push(GridTrackListEntry::Repeat {
                    count: GridRepeatCount::Number(n),
                    tracks,
                    line_names: rlk.clone(),
                });
            }
            _ => return kf.clone(),
        }
    }
    GridTemplateTrackList::Tracks {
        items,
        line_names: lnk.clone(),
    }
}

/// Interpolates CSS Grid L3 `fit-tolerance` (interpolable between `<length-percentage>` values).
#[must_use]
pub fn interpolate_fit_tolerance(
    from: &FitToleranceValue,
    to: &FitToleranceValue,
    p: f64,
    eval: &EvalContext,
) -> Option<FitToleranceValue> {
    match (from, to) {
        (FitToleranceValue::Length(a), FitToleranceValue::Length(b)) => {
            Some(FitToleranceValue::Length(interp_lp_nonneg(a, b, p, eval)))
        }
        _ => None,
    }
}

/// Composes CSS Grid L3 `fit-tolerance`.
#[must_use]
pub fn compose_fit_tolerance(
    underlying: &FitToleranceValue,
    kf: &FitToleranceValue,
    mode: AnimationCompositionMode,
    eval: &EvalContext,
) -> FitToleranceValue {
    if mode == AnimationCompositionMode::Replace {
        return kf.clone();
    }
    match (underlying, kf) {
        (FitToleranceValue::Length(u), FitToleranceValue::Length(k)) => {
            FitToleranceValue::Length(compose_lp_nonneg(u, k, eval))
        }
        _ => kf.clone(),
    }
}
