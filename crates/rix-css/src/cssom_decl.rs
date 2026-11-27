//! CSSOM §6.7.2 declaration validation, shorthand expansion, and canonical
//! value serialization (`el.style`, `CSSStyleRule.style`, `CSS.supports`).

use lightningcss::properties::{Property, PropertyId};
use lightningcss::stylesheet::{ParserOptions, PrinterOptions};
use smallvec::SmallVec;

use crate::properties::{
    compile_extra_standard_property, property_has_disallowed_value, GlobalKeyword, PropertyIdKind,
};

/// Validates and canonically serializes a single CSS property declaration (`name: value`).
///
/// Returns `None` if `name` is unsupported or `value` fails CSS syntax validation.
/// Follows CSSOM §6.7.2 (`setProperty` / `CSS.supports(property, value)`).
#[must_use]
pub fn parse_and_serialize_declaration(name: &str, value: &str) -> Option<String> {
    if name.starts_with("--") {
        // CSS Custom Properties L1 §2: `--` alone is reserved; whitespace inside name is invalid.
        if name.len() <= 2
            || name.chars().any(char::is_whitespace)
            || value.is_empty()
            || !has_valid_css_tokens(value, true)
        {
            return None;
        }
        let cleaned = strip_outer_css_comments(value);
        if cleaned.is_empty() || cleaned.bytes().all(|b| b.is_ascii_whitespace()) {
            return Some(" ".to_string());
        }
        return Some(
            cleaned
                .trim_matches(|c: char| c.is_ascii_whitespace())
                .to_string(),
        );
    }
    let cleaned = strip_outer_css_comments(value);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || !has_valid_css_tokens(trimmed, false) {
        return None;
    }
    let lower_name = name.to_ascii_lowercase();
    let prop_kind = PropertyIdKind::from_name(&lower_name)?;

    let lower_val = trimmed.to_ascii_lowercase();
    if GlobalKeyword::from_ident(&lower_val).is_some() {
        return Some(lower_val);
    }

    // CSS Variables L2 §3 / CSS Values L5: validate `var()` syntax & `{}`-wrapped arguments.
    if contains_var_or_env_ci(trimmed) {
        if !validate_var_and_braces_syntax(trimmed) {
            return None;
        }
        return Some(trimmed.to_string());
    }

    // CSS Cascade L5 §3.1: `all` accepts ONLY CSS-wide keywords (or `var()`).
    if prop_kind == PropertyIdKind::All {
        return None;
    }
    if let Some(extra_ser) = parse_and_serialize_extra_css2_property(prop_kind, trimmed) {
        return extra_ser;
    }

    // CSS Color Level 4 & 5: parse and serialize color longhands via our native Color 4/5
    // engine so wide-gamut spaces, `none`, `color-mix()`, relative colors, `alpha()`,
    // `contrast-color()`, `light-dark()`, and system colors follow W3C spec strictly.
    if is_color_longhand(prop_kind) {
        return crate::color::parse_and_serialize_color_specified(trimmed);
    }
    if prop_kind == PropertyIdKind::Opacity {
        return crate::color::parse_and_serialize_opacity_specified(trimmed).or_else(|| {
            crate::properties::parse_values4_property(prop_kind, trimmed).map(|(_, s)| s)
        });
    }
    if prop_kind == PropertyIdKind::BackgroundLayers
        && (lower_val.starts_with("light-dark(")
            || lower_val.starts_with("image-set(")
            || lower_val.starts_with("-webkit-image-set(")
            || lower_val.starts_with("src(")
            || lower_val.starts_with("image("))
    {
        if lower_val.starts_with("image-set(")
            && !lower_val.contains("light-dark(")
            && crate::values_math::has_values4_syntax(trimmed)
        {
            return crate::values_math::parse_image_set_resolution_specified(trimmed);
        }
        return crate::substitution::parse_and_serialize_bg_image_fn(
            trimmed,
            &crate::values::EvalContext::default(),
        )
        .map(|_| trimmed.to_string());
    }
    if matches!(
        prop_kind,
        PropertyIdKind::ListStyleImage
            | PropertyIdKind::BorderImageSource
            | PropertyIdKind::Content
    ) && lower_val.starts_with("light-dark(")
    {
        return crate::substitution::resolve_light_dark_image_fn(
            trimmed,
            false,
            &crate::values::EvalContext::default(),
            false,
        )
        .map(|_| trimmed.to_string());
    }
    if prop_kind == PropertyIdKind::Display {
        return crate::values::DisplayMode::parse_css(trimmed)
            .map(|d| d.to_css_string().to_string());
    }
    if prop_kind == PropertyIdKind::Flex {
        return parse_flex_shorthand_cssom(trimmed).map(|(g, s, b)| format!("{g} {s} {b}"));
    }
    if prop_kind == PropertyIdKind::FlexFlow {
        return parse_flex_flow_cssom(trimmed).map(|(dir, wrap)| {
            if dir == "row" && wrap == "nowrap" {
                "row".to_string()
            } else if dir == "row" {
                wrap
            } else if wrap == "nowrap" {
                dir
            } else {
                format!("{dir} {wrap}")
            }
        });
    }
    if prop_kind == PropertyIdKind::GridRow || prop_kind == PropertyIdKind::GridColumn {
        return crate::grid_css::parse_grid_row_or_col_shorthand(trimmed)
            .map(|(s, e)| crate::grid_css::serialize_grid_row_or_col_shortest(&s, &e, None));
    }
    if prop_kind == PropertyIdKind::GridArea {
        return crate::grid_css::parse_grid_area_shorthand(trimmed).map(|(rs, cs, re, ce)| {
            crate::grid_css::serialize_grid_area_shortest(&rs, &cs, &re, &ce, None)
        });
    }
    if prop_kind == PropertyIdKind::GridTemplate {
        return crate::grid_css::parse_grid_template_shorthand(trimmed).and_then(|(r, c, a)| {
            crate::grid_css::serialize_grid_template_from_values(&r, &c, &a)
        });
    }
    if prop_kind == PropertyIdKind::Grid {
        return crate::grid_css::parse_grid_shorthand(trimmed).and_then(|(r, c, a, ar, ac, af)| {
            crate::grid_css::serialize_grid_from_values(&r, &c, &a, &ar, &ac, af)
        });
    }
    if prop_kind == PropertyIdKind::GridLanes {
        return crate::grid_css::parse_grid_lanes_shorthand(trimmed).and_then(|(c, r, a, d)| {
            crate::grid_css::serialize_grid_lanes_from_values(&c, &r, &a, d, None)
        });
    }
    if prop_kind == PropertyIdKind::WhiteSpace {
        return crate::text_css::parse_white_space_shorthand(trimmed)
            .map(|(c, w, t)| crate::text_css::serialize_white_space(c, w, t));
    }
    if prop_kind == PropertyIdKind::TextWrap {
        return crate::text_css::parse_text_wrap_shorthand(trimmed)
            .map(|(m, s)| crate::text_css::serialize_text_wrap(m, s));
    }
    if prop_kind == PropertyIdKind::TextSpacing {
        return crate::text_css::parse_text_spacing_shorthand(trimmed)
            .map(|(t, a)| crate::text_css::serialize_text_spacing(t, a));
    }
    if prop_kind == PropertyIdKind::ViewTransitionName {
        if lower_val == "none" {
            return Some("none".to_string());
        }
        let mut pi = cssparser::ParserInput::new(trimmed);
        let mut p = cssparser::Parser::new(&mut pi);
        if let Ok(id) = p.expect_ident() {
            let s = id.as_ref().to_string();
            if p.is_exhausted() && GlobalKeyword::from_ident(&s).is_none() {
                return Some(crate::substitution::escape_css_ident(&s));
            }
        }
        return None;
    }
    if crate::properties::should_use_values4_parser(prop_kind, trimmed) {
        return crate::properties::parse_values4_property(prop_kind, trimmed).map(|(_, s)| s);
    }

    // CSS Standards Mode & Logical Properties: reject bare non-zero unitless numbers on lengths.
    if prop_disallows_unitless_nonzero(prop_kind) && has_top_level_unitless_nonzero(trimmed) {
        return None;
    }
    // CSS Flexbox L1 §7.2.3: negative literal lengths/percentages are invalid for `flex-basis`
    // (while `calc(...)` expressions are valid at parse time and clamped at computed-value time).
    if prop_kind == PropertyIdKind::FlexBasis && trimmed.starts_with('-') {
        return None;
    }

    let parsed_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let opts = ParserOptions::default();
        match Property::parse_string(PropertyId::from(lower_name.as_str()), trimmed, opts) {
            Ok(Property::Unparsed(_)) => None,
            Ok(Property::Custom(custom)) => {
                let mut out = Vec::new();
                let mut order = 0u32;
                let mut missing = Vec::new();
                compile_extra_standard_property(
                    &lower_name,
                    &custom.value,
                    false,
                    &mut order,
                    &mut out,
                    &mut missing,
                );
                if out.is_empty() && prop_kind != PropertyIdKind::Custom {
                    None
                } else {
                    Some(trimmed.to_ascii_lowercase())
                }
            }
            Ok(parsed) => {
                if property_has_disallowed_value(&parsed) {
                    return None;
                }
                if let Some(serialized) = serialize_cssom_property(&parsed, trimmed) {
                    return Some(serialized);
                }
                let raw_out = parsed
                    .value_to_css_string(PrinterOptions::default())
                    .ok()
                    .unwrap_or_else(|| trimmed.to_string());
                let canon = canonicalize_cssom_calc(&raw_out);
                if prop_disallows_unitless_nonzero(prop_kind) {
                    Some(canonicalize_zero_lengths(&normalize_cssom_urls(&canon)))
                } else {
                    Some(normalize_cssom_urls(&canon))
                }
            }
            Err(_) => None,
        }
    }));
    parsed_res.ok().flatten()
}

/// Validates and expands a CSS declaration for CSSOM `CSSStyleDeclaration.setProperty`,
/// returning `(property_name, canonical_value)` longhand pairs (or `[(name, canonical)]`
/// for non-shorthand properties or `var()`-bearing shorthands).
#[must_use]
pub fn parse_and_expand_declaration(name: &str, value: &str) -> Option<Vec<(String, String)>> {
    let canonical = parse_and_serialize_declaration(name, value)?;
    let lower_name = name.to_ascii_lowercase();
    if lower_name.starts_with("--") || contains_var_or_env_ci(&canonical) {
        let out_name = if lower_name.starts_with("--") {
            name.to_string()
        } else if let Some(kind) = PropertyIdKind::from_name(&lower_name) {
            kind.css_name().to_string()
        } else {
            lower_name
        };
        return Some(vec![(out_name, canonical)]);
    }
    let prop_kind = PropertyIdKind::from_name(&lower_name)?;
    let canonical_prop = prop_kind.css_name().to_string();
    let lower_val = canonical.to_ascii_lowercase();
    if GlobalKeyword::from_ident(&lower_val).is_some() {
        let longhands = prop_kind.shorthand_longhands();
        if longhands.len() > 1 && prop_kind != PropertyIdKind::Background {
            return Some(
                longhands
                    .into_iter()
                    .map(|lh| (lh.css_name().to_string(), lower_val.clone()))
                    .collect(),
            );
        }
        return Some(vec![(canonical_prop, lower_val)]);
    }
    let cleaned = strip_outer_css_comments(value);
    if prop_kind == PropertyIdKind::GridRow {
        let (s, e) = crate::grid_css::parse_grid_row_or_col_shorthand(cleaned.trim())?;
        return Some(vec![
            (
                "grid-row-start".to_string(),
                crate::grid_css::serialize_grid_line(&s, None),
            ),
            (
                "grid-row-end".to_string(),
                crate::grid_css::serialize_grid_line(&e, None),
            ),
        ]);
    }
    if prop_kind == PropertyIdKind::GridColumn {
        let (s, e) = crate::grid_css::parse_grid_row_or_col_shorthand(cleaned.trim())?;
        return Some(vec![
            (
                "grid-column-start".to_string(),
                crate::grid_css::serialize_grid_line(&s, None),
            ),
            (
                "grid-column-end".to_string(),
                crate::grid_css::serialize_grid_line(&e, None),
            ),
        ]);
    }
    if prop_kind == PropertyIdKind::GridArea {
        let (rs, cs, re, ce) = crate::grid_css::parse_grid_area_shorthand(cleaned.trim())?;
        return Some(vec![
            (
                "grid-row-start".to_string(),
                crate::grid_css::serialize_grid_line(&rs, None),
            ),
            (
                "grid-column-start".to_string(),
                crate::grid_css::serialize_grid_line(&cs, None),
            ),
            (
                "grid-row-end".to_string(),
                crate::grid_css::serialize_grid_line(&re, None),
            ),
            (
                "grid-column-end".to_string(),
                crate::grid_css::serialize_grid_line(&ce, None),
            ),
        ]);
    }
    if prop_kind == PropertyIdKind::GridTemplate {
        let (r, c, a) = crate::grid_css::parse_grid_template_shorthand(cleaned.trim())?;
        return Some(vec![
            (
                "grid-template-rows".to_string(),
                crate::grid_css::serialize_track_list(&r, None),
            ),
            (
                "grid-template-columns".to_string(),
                crate::grid_css::serialize_track_list(&c, None),
            ),
            (
                "grid-template-areas".to_string(),
                crate::grid_css::serialize_grid_areas(&a),
            ),
        ]);
    }
    if prop_kind == PropertyIdKind::Grid {
        let (r, c, a, ar, ac, af) = crate::grid_css::parse_grid_shorthand(cleaned.trim())?;
        return Some(vec![
            (
                "grid-template-rows".to_string(),
                crate::grid_css::serialize_track_list(&r, None),
            ),
            (
                "grid-template-columns".to_string(),
                crate::grid_css::serialize_track_list(&c, None),
            ),
            (
                "grid-template-areas".to_string(),
                crate::grid_css::serialize_grid_areas(&a),
            ),
            (
                "grid-auto-rows".to_string(),
                crate::grid_css::serialize_auto_tracks(&ar, None),
            ),
            (
                "grid-auto-columns".to_string(),
                crate::grid_css::serialize_auto_tracks(&ac, None),
            ),
            (
                "grid-auto-flow".to_string(),
                crate::grid_css::serialize_grid_auto_flow(af).to_string(),
            ),
        ]);
    }
    if prop_kind == PropertyIdKind::GridLanes {
        let (c, r, a, d) = crate::grid_css::parse_grid_lanes_shorthand(cleaned.trim())?;
        return Some(vec![
            (
                "grid-template-columns".to_string(),
                crate::grid_css::serialize_track_list(&c, None),
            ),
            (
                "grid-template-rows".to_string(),
                crate::grid_css::serialize_track_list(&r, None),
            ),
            (
                "grid-template-areas".to_string(),
                crate::grid_css::serialize_grid_areas(&a),
            ),
            (
                "grid-lanes-direction".to_string(),
                crate::grid_css::serialize_grid_lanes_direction(d),
            ),
        ]);
    }
    if prop_kind == PropertyIdKind::WhiteSpace {
        let (c, w, t) = crate::text_css::parse_white_space_shorthand(cleaned.trim())?;
        return Some(vec![
            ("white-space-collapse".to_string(), c.as_str().to_string()),
            ("text-wrap-mode".to_string(), w.as_str().to_string()),
            ("white-space-trim".to_string(), t.serialize()),
        ]);
    }
    if prop_kind == PropertyIdKind::TextWrap {
        let (m, s) = crate::text_css::parse_text_wrap_shorthand(cleaned.trim())?;
        return Some(vec![
            ("text-wrap-mode".to_string(), m.as_str().to_string()),
            ("text-wrap-style".to_string(), s.as_str().to_string()),
        ]);
    }
    if prop_kind == PropertyIdKind::TextSpacing {
        let (t, a) = crate::text_css::parse_text_spacing_shorthand(cleaned.trim())?;
        return Some(vec![
            ("text-spacing-trim".to_string(), t.as_str().to_string()),
            ("text-autospace".to_string(), a.serialize()),
        ]);
    }
    if prop_kind == PropertyIdKind::Transition {
        let (specs, _) = crate::properties::parse_transition_shorthand_pub(cleaned.trim())?;
        return Some(crate::anim_css::expand_transition_longhands(&specs));
    }
    if prop_kind == PropertyIdKind::Animation {
        let (specs, _) = crate::anim_css::parse_animation_shorthand_full(cleaned.trim())?;
        return Some(crate::anim_css::expand_animation_longhands(&specs));
    }
    if prop_kind == PropertyIdKind::AnimationRange {
        let (starts, ends, _) = crate::anim_css::parse_animation_range_shorthand(cleaned.trim())?;
        let s_str = starts
            .iter()
            .map(|p| p.serialize(false, None))
            .collect::<Vec<_>>()
            .join(", ");
        let e_str = ends
            .iter()
            .map(|p| p.serialize(true, None))
            .collect::<Vec<_>>()
            .join(", ");
        return Some(vec![
            ("animation-range-start".to_string(), s_str),
            ("animation-range-end".to_string(), e_str),
        ]);
    }
    if is_color_longhand(prop_kind)
        || prop_kind == PropertyIdKind::Opacity
        || crate::properties::should_use_values4_parser(prop_kind, &canonical)
    {
        return Some(vec![(canonical_prop, canonical)]);
    }
    if prop_kind == PropertyIdKind::Flex {
        let (g, s, b) = parse_flex_shorthand_cssom(cleaned.trim())?;
        return Some(vec![
            ("flex-grow".to_string(), g),
            ("flex-shrink".to_string(), s),
            ("flex-basis".to_string(), b),
        ]);
    }
    if prop_kind == PropertyIdKind::FlexFlow {
        let (dir, wrap) = parse_flex_flow_cssom(cleaned.trim())?;
        return Some(vec![
            ("flex-direction".to_string(), dir),
            ("flex-wrap".to_string(), wrap),
        ]);
    }
    let expanded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let opts = ParserOptions::default();
        let parsed =
            Property::parse_string(PropertyId::from(lower_name.as_str()), cleaned.trim(), opts)
                .ok()?;
        expand_lightning_shorthand_cssom(&parsed, cleaned.trim())
    }))
    .ok()
    .flatten();
    if let Some(pairs) = expanded {
        return Some(pairs);
    }
    Some(vec![(canonical_prop, canonical)])
}

/// Parses W3C CSS Flexbox L1 `flex` shorthand (`none | [ <flex-grow> <flex-shrink>? || <flex-basis> ]`)
/// into canonical `(flex-grow, flex-shrink, flex-basis)` strings.
fn parse_flex_shorthand_cssom(raw: &str) -> Option<(String, String, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.eq_ignore_ascii_case("none") {
        return Some(("0".to_string(), "0".to_string(), "auto".to_string()));
    }
    // Split into top-level whitespace-separated components (preserving parentheses like `calc(...)`).
    let mut parts: Vec<&str> = Vec::new();
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        let mut depth = 0usize;
        while i < bytes.len() {
            match bytes[i] {
                b'(' => depth += 1,
                b')' => depth = depth.saturating_sub(1),
                b if b.is_ascii_whitespace() && depth == 0 => break,
                _ => {}
            }
            i += 1;
        }
        parts.push(&trimmed[start..i]);
    }
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let parse_nonneg_num = |s: &str| -> Option<String> {
        let low = s.to_ascii_lowercase();
        if let Ok(v) = low.parse::<f64>() {
            if v.is_finite() && v >= 0.0 {
                return Some(crate::values_math::format_math_f64(v));
            }
            return None;
        }
        if low.contains('(') {
            let pm =
                crate::values_math::parse_math_value(s, crate::values_math::ValueKind::Number)?;
            // Per CSS Values 4 §10.9, `calc(...)` with negative result is valid at parse time
            // and clamped to 0 at computed-value time.
            return Some(pm.node.serialize_specified(pm.was_math_fn));
        }
        None
    };
    let parse_basis = |s: &str, allow_unitless_zero: bool| -> Option<String> {
        // Reject bare unitless numbers (except literal `0` when 2 numbers already precede it,
        // per CSS Flexbox §7.1: `flex: 1 2 0` -> `1 2 0px`, while `flex: 1 2 calc(0)` is invalid).
        if let Ok(v) = s.parse::<f64>() {
            if allow_unitless_zero && v == 0.0 && !s.starts_with('-') {
                return Some("0px".to_string());
            }
            return None;
        }
        let ser = parse_and_serialize_declaration("flex-basis", s)?;
        Some(if ser == "0" { "0px".to_string() } else { ser })
    };
    let mut nums: Vec<String> = Vec::new();
    let mut basis: Option<String> = None;
    let mut saw_basis_between_nums = false;
    for part in parts {
        if nums.len() < 2 && !saw_basis_between_nums {
            if let Some(n) = parse_nonneg_num(part) {
                nums.push(n);
                continue;
            }
        }
        if basis.is_none() {
            let b = parse_basis(part, nums.len() == 2)?;
            if !nums.is_empty() {
                saw_basis_between_nums = true;
            }
            basis = Some(b);
        } else {
            return None;
        }
    }
    let grow = nums.first().cloned().unwrap_or_else(|| "1".to_string());
    let shrink = nums.get(1).cloned().unwrap_or_else(|| "1".to_string());
    let basis_str = basis.unwrap_or_else(|| "0%".to_string());
    Some((grow, shrink, basis_str))
}

/// Parses W3C CSS Flexbox L1 `flex-flow` shorthand (`<flex-direction> || <flex-wrap>`).
fn parse_flex_flow_cssom(raw: &str) -> Option<(String, String)> {
    let toks: Vec<String> = raw
        .split_ascii_whitespace()
        .map(str::to_ascii_lowercase)
        .collect();
    if toks.is_empty() || toks.len() > 2 {
        return None;
    }
    let (mut dir, mut wrap) = (None::<String>, None::<String>);
    for t in toks {
        match t.as_str() {
            "row" | "row-reverse" | "column" | "column-reverse" => {
                if dir.is_some() {
                    return None;
                }
                dir = Some(t);
            }
            "nowrap" | "wrap" | "wrap-reverse" => {
                if wrap.is_some() {
                    return None;
                }
                wrap = Some(t);
            }
            _ => return None,
        }
    }
    Some((
        dir.unwrap_or_else(|| "row".to_string()),
        wrap.unwrap_or_else(|| "nowrap".to_string()),
    ))
}

/// Strips leading and trailing CSS comments (`/* ... */`) while preserving interior comments.
#[must_use]
pub fn strip_outer_css_comments(input: &str) -> &str {
    let mut s = input;
    loop {
        let t = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
        if let Some(after) = t.strip_prefix("/*") {
            if let Some(end) = after.find("*/") {
                s = &after[end + 2..];
                continue;
            }
        }
        break;
    }
    loop {
        let t = s.trim_end_matches(|c: char| c.is_ascii_whitespace());
        if let Some(before) = t.strip_suffix("*/") {
            if let Some(start) = before.rfind("/*") {
                // Ensure this `/*` is not inside a quoted string by scanning simply.
                if !is_inside_string(before, start) {
                    s = &before[..start];
                    continue;
                }
            }
        }
        break;
    }
    s
}

fn is_inside_string(s: &str, target: usize) -> bool {
    let bytes = s.as_bytes();
    let mut in_str: Option<u8> = None;
    let mut i = 0;
    while i < target && i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                in_str = None;
            }
        } else if b == b'\\' {
            i += 2;
            continue;
        } else if b == b'"' || b == b'\'' {
            in_str = Some(b);
        }
        i += 1;
    }
    in_str.is_some()
}

#[must_use]
pub fn is_color_longhand(kind: PropertyIdKind) -> bool {
    matches!(
        kind,
        PropertyIdKind::Color
            | PropertyIdKind::BackgroundColor
            | PropertyIdKind::BorderTopColor
            | PropertyIdKind::BorderRightColor
            | PropertyIdKind::BorderBottomColor
            | PropertyIdKind::BorderLeftColor
            | PropertyIdKind::BorderBlockStartColor
            | PropertyIdKind::BorderBlockEndColor
            | PropertyIdKind::BorderInlineStartColor
            | PropertyIdKind::BorderInlineEndColor
            | PropertyIdKind::OutlineColor
            | PropertyIdKind::TextDecorationColor
            | PropertyIdKind::ColumnRuleColor
    )
}

pub(crate) fn contains_var_or_env_ci(s: &str) -> bool {
    crate::substitution::contains_subst_fn_str(s)
}

fn prop_disallows_unitless_nonzero(kind: PropertyIdKind) -> bool {
    let n = kind.css_name();
    if matches!(n, "line-height" | "tab-size") {
        return false;
    }
    n.starts_with("margin")
        || n.starts_with("padding")
        || n.starts_with("inset")
        || n.ends_with("-size")
        || n.ends_with("-width")
        || n.ends_with("-height")
        || n.ends_with("-radius")
        || n.ends_with("-gap")
        || n.ends_with("-offset")
        || matches!(
            n,
            "width"
                | "height"
                | "top"
                | "right"
                | "bottom"
                | "left"
                | "gap"
                | "border-spacing"
                | "text-indent"
                | "letter-spacing"
                | "word-spacing"
                | "flex-basis"
        )
}

fn has_top_level_unitless_nonzero(input: &str) -> bool {
    let bytes = input.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' | b'{' => {
                depth += 1;
                i += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            c if depth == 0 && !c.is_ascii_whitespace() && c != b'/' => {
                let start = i;
                while i < bytes.len()
                    && !bytes[i].is_ascii_whitespace()
                    && !matches!(bytes[i], b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'/')
                {
                    i += 1;
                }
                if let Ok(num) = input[start..i].parse::<f64>() {
                    if num != 0.0 {
                        return true;
                    }
                }
            }
            _ => i += 1,
        }
    }
    false
}

/// Validates CSS Variables L2 `var()` and CSS Values L5 arbitrary substitution syntax.
fn validate_var_and_braces_syntax(s: &str) -> bool {
    crate::substitution::validate_subst_syntax(s)
}

pub(crate) fn format_cssom_color(
    color: &lightningcss::values::color::CssColor,
    raw_hint: Option<&str>,
) -> String {
    if matches!(color, lightningcss::values::color::CssColor::CurrentColor) {
        return "currentcolor".to_string();
    }
    if let Some(raw) = raw_hint {
        for tok in raw.split(|c: char| !c.is_ascii_alphabetic()) {
            if tok.len() >= 3 {
                let lower = tok.to_ascii_lowercase();
                if lower == "transparent"
                    && matches!(
                        color,
                        lightningcss::values::color::CssColor::RGBA(rgba) if rgba.alpha == 0
                    )
                {
                    return "transparent".to_string();
                }
                let matches = matches!(
                    Property::parse_string(
                        PropertyId::from("color"),
                        &lower,
                        ParserOptions::default(),
                    ),
                    Ok(Property::Color(ref parsed_c)) if parsed_c == color
                );
                if matches {
                    return lower;
                }
            }
        }
    }
    if let lightningcss::values::color::CssColor::RGBA(rgba) = color {
        return crate::color::format_legacy_rgba(crate::values::ColorRgba::rgba(
            rgba.red, rgba.green, rgba.blue, rgba.alpha,
        ));
    }
    use lightningcss::traits::ToCss as _;
    let mut s = String::new();
    let mut p = lightningcss::printer::Printer::new(&mut s, PrinterOptions::default());
    let _ = color.to_css(&mut p);
    s
}

/// Converts bare `"0"` and `"-0px"` length tokens to canonical CSSOM `"0px"`.
fn canonicalize_zero_lengths(input: &str) -> String {
    if input == "0" || input == "-0px" || input == "-0" {
        return "0px".to_string();
    }
    if !input.contains('0') || input.contains("calc(") {
        return input.to_string();
    }
    input
        .split_ascii_whitespace()
        .map(|tok| {
            if tok == "0" || tok == "-0px" || tok == "-0" {
                "0px"
            } else {
                tok
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Ensures `url(foo)` serializes with CSSOM double quotes (`url("foo")`).
fn normalize_cssom_urls(input: &str) -> String {
    if !input.contains("url(") && !input.contains("URL(") {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len() + 4);
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if input[i..].len() >= 4 && input[i..i + 4].eq_ignore_ascii_case("url(") {
            let start = i + 4;
            if let Some(rel_end) = input[start..].find(')') {
                let inner = input[start..start + rel_end].trim();
                out.push_str("url(");
                if (inner.starts_with('"') && inner.ends_with('"') && inner.len() >= 2)
                    || (inner.starts_with('\'') && inner.ends_with('\'') && inner.len() >= 2)
                {
                    let unquoted = &inner[1..inner.len() - 1];
                    out.push('"');
                    out.push_str(&unquoted.replace('"', "\\\""));
                    out.push('"');
                } else {
                    out.push('"');
                    out.push_str(&inner.replace('"', "\\\""));
                    out.push('"');
                }
                out.push(')');
                i = start + rel_end + 1;
                continue;
            }
        }
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Validates and canonically serializes CSS2/CSS3 properties that `lightningcss` leaves as `Custom`.
/// Returns `Some(Some(canon))` if valid, `Some(None)` if explicitly invalid, or `None` to fall through.
fn parse_and_serialize_extra_css2_property(
    kind: PropertyIdKind,
    trimmed: &str,
) -> Option<Option<String>> {
    let low = trimmed.to_ascii_lowercase();
    match kind {
        PropertyIdKind::AlignmentBaseline => {
            let ok = matches!(
                low.as_str(),
                "baseline"
                    | "alphabetic"
                    | "ideographic"
                    | "middle"
                    | "central"
                    | "mathematical"
                    | "text-before-edge"
                    | "text-after-edge"
                    | "before-edge"
                    | "after-edge"
                    | "hanging"
                    | "auto"
                    | "top"
                    | "center"
                    | "bottom"
                    | "text-top"
                    | "text-bottom"
            );
            Some(ok.then_some(low))
        }
        PropertyIdKind::BaselineSource => {
            let ok = matches!(low.as_str(), "auto" | "first" | "last");
            Some(ok.then_some(low))
        }
        PropertyIdKind::BaselineShift => {
            if matches!(
                low.as_str(),
                "baseline" | "sub" | "super" | "top" | "center" | "bottom"
            ) {
                return Some(Some(low));
            }
            // Also accepts `<length-percentage>` (including negative lengths like `-0px`, `-.1em`, `-.5%`).
            let canon = parse_and_serialize_declaration("top", trimmed)?;
            Some(Some(canon))
        }
        PropertyIdKind::UnicodeBidi => {
            let ok = matches!(
                low.as_str(),
                "normal" | "embed" | "isolate" | "bidi-override" | "isolate-override" | "plaintext"
            );
            Some(ok.then_some(low))
        }
        PropertyIdKind::PageBreakAfter | PropertyIdKind::PageBreakBefore => {
            let ok = matches!(low.as_str(), "auto" | "always" | "avoid" | "left" | "right");
            Some(ok.then_some(low))
        }
        PropertyIdKind::PageBreakInside => {
            let ok = matches!(low.as_str(), "auto" | "avoid");
            Some(ok.then_some(low))
        }
        PropertyIdKind::FontVariant => {
            let ok = matches!(low.as_str(), "normal" | "small-caps" | "none");
            if ok {
                Some(Some(low))
            } else {
                None
            }
        }
        PropertyIdKind::Clip => {
            if low == "auto" {
                return Some(Some("auto".to_string()));
            }
            if let Some(inner) = low.strip_prefix("rect(").and_then(|s| s.strip_suffix(')')) {
                let raw_parts: Vec<&str> = if inner.contains(',') {
                    inner.split(',').map(str::trim).collect()
                } else {
                    inner.split_ascii_whitespace().collect()
                };
                if raw_parts.len() == 4 {
                    let mut norm = Vec::with_capacity(4);
                    for p in raw_parts {
                        if p == "auto" {
                            norm.push("auto".to_string());
                        } else if let Some(l) = parse_and_serialize_declaration("top", p) {
                            if !l.ends_with('%') {
                                norm.push(l);
                            } else {
                                return Some(None);
                            }
                        } else {
                            return Some(None);
                        }
                    }
                    return Some(Some(format!("rect({})", norm.join(", "))));
                }
            }
            Some(None)
        }
        PropertyIdKind::BorderImage => {
            if low == "none" {
                return Some(Some("none".to_string()));
            }
            None
        }
        PropertyIdKind::BackgroundAttachment => {
            let parts: Vec<&str> = low.split(',').map(str::trim).collect();
            if parts
                .iter()
                .all(|p| matches!(*p, "scroll" | "fixed" | "local"))
            {
                Some(Some(parts.join(", ")))
            } else {
                Some(None)
            }
        }
        PropertyIdKind::BackgroundRepeat => {
            let parts: Vec<&str> = low.split(',').map(str::trim).collect();
            let mut norm = Vec::with_capacity(parts.len());
            for p in parts {
                let toks: Vec<&str> = p.split_ascii_whitespace().collect();
                match toks.as_slice() {
                    ["repeat" | "repeat-x" | "repeat-y" | "space" | "round" | "no-repeat"] => {
                        norm.push(toks[0].to_string());
                    }
                    [a, b]
                        if matches!(*a, "repeat" | "space" | "round" | "no-repeat")
                            && matches!(*b, "repeat" | "space" | "round" | "no-repeat") =>
                    {
                        if *a == "repeat" && *b == "no-repeat" {
                            norm.push("repeat-x".to_string());
                        } else if *a == "no-repeat" && *b == "repeat" {
                            norm.push("repeat-y".to_string());
                        } else if a == b {
                            norm.push((*a).to_string());
                        } else {
                            norm.push(format!("{a} {b}"));
                        }
                    }
                    _ => return Some(None),
                }
            }
            Some(Some(norm.join(", ")))
        }
        PropertyIdKind::ListStyleImage | PropertyIdKind::BorderImageSource => {
            if low == "none" {
                return Some(Some("none".to_string()));
            }
            if low.starts_with("light-dark(") {
                return Some(
                    crate::substitution::resolve_light_dark_image_fn(
                        trimmed,
                        false,
                        &crate::values::EvalContext::default(),
                        false,
                    )
                    .map(|_| trimmed.to_string()),
                );
            }
            if low.starts_with("url(") && low.ends_with(')') {
                return Some(Some(normalize_cssom_urls(trimmed)));
            }
            None
        }
        PropertyIdKind::CaretColor => {
            if low == "auto" {
                Some(Some("auto".to_string()))
            } else {
                Some(crate::color::parse_and_serialize_color_specified(trimmed))
            }
        }
        PropertyIdKind::SvgClipRule => {
            let ok = matches!(low.as_str(), "nonzero" | "evenodd");
            Some(ok.then_some(low))
        }
        PropertyIdKind::BackgroundOrigin => {
            let ok = matches!(low.as_str(), "border-box" | "padding-box" | "content-box");
            Some(ok.then_some(low))
        }
        PropertyIdKind::ListStylePosition => {
            let ok = matches!(low.as_str(), "inside" | "outside");
            Some(ok.then_some(low))
        }
        PropertyIdKind::ListStyleType => {
            if matches!(
                low.as_str(),
                "none"
                    | "disc"
                    | "circle"
                    | "square"
                    | "decimal"
                    | "decimal-leading-zero"
                    | "lower-roman"
                    | "upper-roman"
                    | "lower-greek"
                    | "lower-latin"
                    | "upper-latin"
                    | "armenian"
                    | "georgian"
                    | "lower-alpha"
                    | "upper-alpha"
                    | "disclosure-open"
                    | "disclosure-closed"
            ) {
                return Some(Some(low));
            }
            None
        }
        PropertyIdKind::Appearance => Some(
            crate::values::AppearanceMode::parse_css(trimmed).map(|m| m.as_css_str().to_string()),
        ),
        PropertyIdKind::AccentColor => {
            if low == "auto" {
                Some(Some("auto".to_string()))
            } else {
                Some(crate::color::parse_and_serialize_color_specified(trimmed))
            }
        }
        _ => None,
    }
}

/// Restores leading `0` before decimals (`.5em` -> `0.5em`) and sorts additive `calc(...)`
/// terms into CSS Values & Units L4 §10.9.3 canonical order (number < `%` < alphabetical unit).
fn canonicalize_cssom_calc(input: &str) -> String {
    if !input.contains("calc(") && !input.contains('.') {
        return input.to_string();
    }
    let with_zeros = restore_leading_zeros(input);
    if !with_zeros.contains("calc(") {
        return with_zeros;
    }
    let mut out = String::with_capacity(with_zeros.len());
    let bytes = with_zeros.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if with_zeros[i..].starts_with("calc(") {
            let start = i + 5;
            let mut depth = 1usize;
            let mut j = start;
            while j < bytes.len() && depth > 0 {
                match bytes[j] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            if depth == 0 {
                let inner = &with_zeros[start..j];
                out.push_str("calc(");
                out.push_str(&canonicalize_calc_sum(inner));
                out.push(')');
                i = j + 1;
                continue;
            }
        }
        let ch = with_zeros[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn restore_leading_zeros(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + 4);
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'.' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            let prev_is_digit = i > 0 && bytes[i - 1].is_ascii_digit();
            if !prev_is_digit {
                out.push('0');
            }
        }
        out.push(b as char);
    }
    out
}

fn canonicalize_calc_sum(inner: &str) -> String {
    let trimmed = inner.trim();
    if trimmed.contains('*') || trimmed.contains('/') || trimmed.contains('(') {
        return trimmed.to_string();
    }
    let mut terms: Vec<(bool, String)> = Vec::new();
    let mut cur = String::new();
    let mut neg = false;
    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if (b == b'+' || b == b'-')
            && i > 0
            && bytes[i - 1].is_ascii_whitespace()
            && i + 1 < bytes.len()
            && bytes[i + 1].is_ascii_whitespace()
        {
            let t = cur.trim();
            if !t.is_empty() {
                if let Some(rest) = t.strip_prefix('-') {
                    terms.push((!neg, rest.to_string()));
                } else if let Some(rest) = t.strip_prefix('+') {
                    terms.push((neg, rest.to_string()));
                } else {
                    terms.push((neg, t.to_string()));
                }
            }
            cur.clear();
            neg = b == b'-';
            i += 1;
            continue;
        }
        cur.push(b as char);
        i += 1;
    }
    let t = cur.trim();
    if !t.is_empty() {
        if let Some(rest) = t.strip_prefix('-') {
            terms.push((!neg, rest.to_string()));
        } else if let Some(rest) = t.strip_prefix('+') {
            terms.push((neg, rest.to_string()));
        } else {
            terms.push((neg, t.to_string()));
        }
    }
    if terms.len() < 2 {
        return trimmed.to_string();
    }
    fn term_sort_key(abs_term: &str) -> (u8, String) {
        let unit_start = abs_term
            .find(|c: char| !c.is_ascii_digit() && c != '.' && c != 'e' && c != 'E')
            .unwrap_or(abs_term.len());
        let unit = &abs_term[unit_start..];
        if unit.is_empty() {
            (0, String::new())
        } else if unit == "%" {
            (1, "%".to_string())
        } else {
            (2, unit.to_ascii_lowercase())
        }
    }
    terms.sort_by_key(|(_, a)| term_sort_key(a));
    let mut out = String::new();
    for (idx, (is_neg, abs)) in terms.into_iter().enumerate() {
        if idx == 0 {
            if is_neg {
                out.push('-');
            }
            out.push_str(&abs);
        } else if is_neg {
            out.push_str(" - ");
            out.push_str(&abs);
        } else {
            out.push_str(" + ");
            out.push_str(&abs);
        }
    }
    out
}

fn format_cssom_to_css<T: lightningcss::traits::ToCss>(val: &T) -> String {
    let mut s = String::new();
    let mut p = lightningcss::printer::Printer::new(&mut s, PrinterOptions::default());
    let _ = val.to_css(&mut p);
    canonicalize_cssom_calc(&s)
}

fn format_cssom_len_to_css<T: lightningcss::traits::ToCss>(val: &T) -> String {
    canonicalize_zero_lengths(&format_cssom_to_css(val))
}

fn format_cssom_generic_border<const P: u8>(
    b: &lightningcss::properties::border::GenericBorder<
        lightningcss::properties::border::LineStyle,
        P,
    >,
    raw_hint: Option<&str>,
) -> String {
    let w = format_cssom_len_to_css(&b.width);
    let st = format_cssom_to_css(&b.style);
    let col = format_cssom_color(&b.color, raw_hint);
    let mut parts = Vec::with_capacity(3);
    if w != "medium" {
        parts.push(w);
    }
    if st != "none" {
        parts.push(st);
    }
    if col != "currentcolor" {
        parts.push(col);
    }
    if parts.is_empty() {
        parts.push("none".to_string());
    }
    parts.join(" ")
}

pub(crate) fn serialize_cssom_property(prop: &Property<'_>, raw_input: &str) -> Option<String> {
    let hint = Some(raw_input);
    match prop {
        Property::Color(c)
        | Property::BackgroundColor(c)
        | Property::BorderTopColor(c)
        | Property::BorderRightColor(c)
        | Property::BorderBottomColor(c)
        | Property::BorderLeftColor(c)
        | Property::BorderBlockStartColor(c)
        | Property::BorderBlockEndColor(c)
        | Property::BorderInlineStartColor(c)
        | Property::BorderInlineEndColor(c)
        | Property::OutlineColor(c)
        | Property::TextDecorationColor(c, _) => Some(format_cssom_color(c, hint)),
        Property::Background(layers)
            if layers.len() == 1
                && matches!(layers[0].image, lightningcss::values::image::Image::None)
                && raw_input.bytes().all(|b| b.is_ascii_alphabetic()) =>
        {
            Some(format_cssom_color(&layers[0].color, hint))
        }
        Property::BorderBlockColor(bc) => {
            let s = format_cssom_color(&bc.start, hint);
            let e = format_cssom_color(&bc.end, hint);
            Some(if s == e { s } else { format!("{s} {e}") })
        }
        Property::BorderInlineColor(bc) => {
            let s = format_cssom_color(&bc.start, hint);
            let e = format_cssom_color(&bc.end, hint);
            Some(if s == e { s } else { format!("{s} {e}") })
        }
        Property::BorderColor(bc) => {
            let t = format_cssom_color(&bc.top, hint);
            let r = format_cssom_color(&bc.right, hint);
            let b = format_cssom_color(&bc.bottom, hint);
            let l = format_cssom_color(&bc.left, hint);
            Some(collapse_4_sides(&t, &r, &b, &l))
        }
        Property::Border(b) => Some(format_cssom_generic_border(b, hint)),
        Property::BorderTop(b) => Some(format_cssom_generic_border(b, hint)),
        Property::BorderRight(b) => Some(format_cssom_generic_border(b, hint)),
        Property::BorderBottom(b) => Some(format_cssom_generic_border(b, hint)),
        Property::BorderLeft(b) => Some(format_cssom_generic_border(b, hint)),
        Property::BorderBlock(b) => Some(format_cssom_generic_border(b, hint)),
        Property::BorderBlockStart(b) => Some(format_cssom_generic_border(b, hint)),
        Property::BorderBlockEnd(b) => Some(format_cssom_generic_border(b, hint)),
        Property::BorderInline(b) => Some(format_cssom_generic_border(b, hint)),
        Property::BorderInlineStart(b) => Some(format_cssom_generic_border(b, hint)),
        Property::BorderInlineEnd(b) => Some(format_cssom_generic_border(b, hint)),
        Property::BackgroundPosition(positions) => {
            use lightningcss::values::position::{
                HorizontalPosition, HorizontalPositionKeyword, VerticalPosition,
                VerticalPositionKeyword,
            };
            let formatted: Vec<String> = positions
                .iter()
                .map(|pos| {
                    let x = match &pos.x {
                        HorizontalPosition::Center => "center".to_string(),
                        HorizontalPosition::Length(lp) => format_cssom_len_to_css(lp),
                        HorizontalPosition::Side { side, offset } => {
                            let kw = match side {
                                HorizontalPositionKeyword::Left => "left",
                                HorizontalPositionKeyword::Right => "right",
                            };
                            match offset {
                                Some(lp) => format!("{kw} {}", format_cssom_len_to_css(lp)),
                                None => kw.to_string(),
                            }
                        }
                    };
                    let y = match &pos.y {
                        VerticalPosition::Center => "center".to_string(),
                        VerticalPosition::Length(lp) => format_cssom_len_to_css(lp),
                        VerticalPosition::Side { side, offset } => {
                            let kw = match side {
                                VerticalPositionKeyword::Top => "top",
                                VerticalPositionKeyword::Bottom => "bottom",
                            };
                            match offset {
                                Some(lp) => format!("{kw} {}", format_cssom_len_to_css(lp)),
                                None => kw.to_string(),
                            }
                        }
                    };
                    format!("{x} {y}")
                })
                .collect();
            Some(formatted.join(", "))
        }
        _ => None,
    }
}

fn collapse_4_sides(t: &str, r: &str, b: &str, l: &str) -> String {
    if t == r && r == b && b == l {
        t.to_string()
    } else if t == b && r == l {
        format!("{t} {r}")
    } else if r == l {
        format!("{t} {r} {b}")
    } else {
        format!("{t} {r} {b} {l}")
    }
}

fn expand_border_sides<const P: u8>(
    prefixes: &[&str],
    b: &lightningcss::properties::border::GenericBorder<
        lightningcss::properties::border::LineStyle,
        P,
    >,
    hint: Option<&str>,
) -> Vec<(String, String)> {
    let (w, s, c) = (
        format_cssom_len_to_css(&b.width),
        format_cssom_to_css(&b.style),
        format_cssom_color(&b.color, hint),
    );
    let mut out = Vec::with_capacity(prefixes.len() * 3);
    for p in prefixes {
        out.push((format!("{p}-width"), w.clone()));
    }
    for p in prefixes {
        out.push((format!("{p}-style"), s.clone()));
    }
    for p in prefixes {
        out.push((format!("{p}-color"), c.clone()));
    }
    out
}

fn expand_lightning_shorthand_cssom(
    prop: &Property<'_>,
    raw_input: &str,
) -> Option<Vec<(String, String)>> {
    let hint = Some(raw_input);
    use format_cssom_len_to_css as f;
    use format_cssom_to_css as g;
    let c = |col| format_cssom_color(col, hint);
    match prop {
        Property::Border(b) => {
            let mut out = expand_border_sides(
                &["border-top", "border-right", "border-bottom", "border-left"],
                b,
                hint,
            );
            out.push(("border-image".to_string(), "none".to_string()));
            return Some(out);
        }
        Property::BorderTop(b) => return Some(expand_border_sides(&["border-top"], b, hint)),
        Property::BorderRight(b) => return Some(expand_border_sides(&["border-right"], b, hint)),
        Property::BorderBottom(b) => return Some(expand_border_sides(&["border-bottom"], b, hint)),
        Property::BorderLeft(b) => return Some(expand_border_sides(&["border-left"], b, hint)),
        Property::BorderBlock(b) => {
            return Some(expand_border_sides(
                &["border-block-start", "border-block-end"],
                b,
                hint,
            ));
        }
        Property::BorderBlockStart(b) => {
            return Some(expand_border_sides(&["border-block-start"], b, hint));
        }
        Property::BorderBlockEnd(b) => {
            return Some(expand_border_sides(&["border-block-end"], b, hint));
        }
        Property::BorderInline(b) => {
            return Some(expand_border_sides(
                &["border-inline-start", "border-inline-end"],
                b,
                hint,
            ));
        }
        Property::BorderInlineStart(b) => {
            return Some(expand_border_sides(&["border-inline-start"], b, hint));
        }
        Property::BorderInlineEnd(b) => {
            return Some(expand_border_sides(&["border-inline-end"], b, hint));
        }
        _ => {}
    }
    let pairs: Vec<(&str, String)> = match prop {
        Property::Margin(m) => vec![
            ("margin-top", f(&m.top)),
            ("margin-right", f(&m.right)),
            ("margin-bottom", f(&m.bottom)),
            ("margin-left", f(&m.left)),
        ],
        Property::MarginBlock(m) => vec![
            ("margin-block-start", f(&m.block_start)),
            ("margin-block-end", f(&m.block_end)),
        ],
        Property::MarginInline(m) => vec![
            ("margin-inline-start", f(&m.inline_start)),
            ("margin-inline-end", f(&m.inline_end)),
        ],
        Property::Padding(p) => vec![
            ("padding-top", f(&p.top)),
            ("padding-right", f(&p.right)),
            ("padding-bottom", f(&p.bottom)),
            ("padding-left", f(&p.left)),
        ],
        Property::PaddingBlock(p) => vec![
            ("padding-block-start", f(&p.block_start)),
            ("padding-block-end", f(&p.block_end)),
        ],
        Property::PaddingInline(p) => vec![
            ("padding-inline-start", f(&p.inline_start)),
            ("padding-inline-end", f(&p.inline_end)),
        ],
        Property::Inset(i) => vec![
            ("top", f(&i.top)),
            ("right", f(&i.right)),
            ("bottom", f(&i.bottom)),
            ("left", f(&i.left)),
        ],
        Property::InsetBlock(i) => vec![
            ("inset-block-start", f(&i.block_start)),
            ("inset-block-end", f(&i.block_end)),
        ],
        Property::InsetInline(i) => vec![
            ("inset-inline-start", f(&i.inline_start)),
            ("inset-inline-end", f(&i.inline_end)),
        ],
        Property::BorderWidth(bw) => vec![
            ("border-top-width", f(&bw.top)),
            ("border-right-width", f(&bw.right)),
            ("border-bottom-width", f(&bw.bottom)),
            ("border-left-width", f(&bw.left)),
        ],
        Property::BorderBlockWidth(bw) => vec![
            ("border-block-start-width", f(&bw.start)),
            ("border-block-end-width", f(&bw.end)),
        ],
        Property::BorderInlineWidth(bw) => vec![
            ("border-inline-start-width", f(&bw.start)),
            ("border-inline-end-width", f(&bw.end)),
        ],
        Property::BorderStyle(bs) => vec![
            ("border-top-style", g(&bs.top)),
            ("border-right-style", g(&bs.right)),
            ("border-bottom-style", g(&bs.bottom)),
            ("border-left-style", g(&bs.left)),
        ],
        Property::BorderBlockStyle(bs) => vec![
            ("border-block-start-style", g(&bs.start)),
            ("border-block-end-style", g(&bs.end)),
        ],
        Property::BorderInlineStyle(bs) => vec![
            ("border-inline-start-style", g(&bs.start)),
            ("border-inline-end-style", g(&bs.end)),
        ],
        Property::BorderColor(bc) => vec![
            ("border-top-color", c(&bc.top)),
            ("border-right-color", c(&bc.right)),
            ("border-bottom-color", c(&bc.bottom)),
            ("border-left-color", c(&bc.left)),
        ],
        Property::BorderBlockColor(bc) => vec![
            ("border-block-start-color", c(&bc.start)),
            ("border-block-end-color", c(&bc.end)),
        ],
        Property::BorderInlineColor(bc) => vec![
            ("border-inline-start-color", c(&bc.start)),
            ("border-inline-end-color", c(&bc.end)),
        ],
        Property::BorderRadius(br, _) => vec![
            ("border-top-left-radius", f(&br.top_left)),
            ("border-top-right-radius", f(&br.top_right)),
            ("border-bottom-right-radius", f(&br.bottom_right)),
            ("border-bottom-left-radius", f(&br.bottom_left)),
        ],
        Property::Overflow(o) => vec![("overflow-x", g(&o.x)), ("overflow-y", g(&o.y))],
        Property::Outline(o) => vec![
            ("outline-width", f(&o.width)),
            ("outline-style", g(&o.style)),
            ("outline-color", c(&o.color)),
        ],
        Property::ListStyle(ls) => vec![
            ("list-style-position", g(&ls.position)),
            ("list-style-image", normalize_cssom_urls(&g(&ls.image))),
            ("list-style-type", g(&ls.list_style_type)),
        ],
        _ => return None,
    };
    Some(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// Validates that `input` has balanced strings/delimiters and no top-level `;` or `!`.
pub(crate) fn has_valid_css_tokens_pub(input: &str, require_closed: bool) -> bool {
    has_valid_css_tokens(input, require_closed)
}

/// Normalizes unitless zero length tokens (`0` -> `0px`) for CSSOM serialization.
pub(crate) fn canonicalize_zero_lengths_pub(input: &str) -> String {
    canonicalize_zero_lengths(input)
}

/// Returns true if `prop_name` is a length-valued CSS property whose `0` serializes as `0px`.
pub(crate) fn prop_name_uses_zero_px(prop_name: &str) -> bool {
    PropertyIdKind::from_name(prop_name).is_some_and(prop_disallows_unitless_nonzero)
}

fn has_valid_css_tokens(input: &str, require_closed: bool) -> bool {
    let mut stack: SmallVec<[u8; 8]> = SmallVec::new();
    let bytes = input.as_bytes();
    let mut i = 0usize;
    let mut in_str: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                in_str = None;
            } else if b == b'\n' || b == b'\r' {
                return false;
            }
            i += 1;
            continue;
        }
        match b {
            b'\\' => {
                i += 2;
                continue;
            }
            b'"' | b'\'' => in_str = Some(b),
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            b'(' => stack.push(b')'),
            b'[' => stack.push(b']'),
            b'{' => stack.push(b'}'),
            b')' | b']' | b'}' if stack.pop() != Some(b) => return false,
            b';' | b'!' if stack.is_empty() => return false,
            _ => {}
        }
        i += 1;
    }
    in_str.is_none() && (!require_closed || stack.is_empty())
}
