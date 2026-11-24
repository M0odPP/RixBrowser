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
        && lower_val.starts_with("image-set(")
        && crate::values_math::has_values4_syntax(trimmed)
    {
        return crate::values_math::parse_image_set_resolution_specified(trimmed);
    }
    if crate::properties::should_use_values4_parser(prop_kind, trimmed) {
        return crate::properties::parse_values4_property(prop_kind, trimmed).map(|(_, s)| s);
    }

    // CSS Standards Mode & Logical Properties: reject bare non-zero unitless numbers on lengths.
    if prop_disallows_unitless_nonzero(prop_kind) && has_top_level_unitless_nonzero(trimmed) {
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
                    Some(canonicalize_zero_lengths(&canon))
                } else {
                    Some(canon)
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
        return Some(vec![(
            if lower_name.starts_with("--") {
                name.to_string()
            } else {
                lower_name
            },
            canonical,
        )]);
    }
    let prop_kind = PropertyIdKind::from_name(&lower_name)?;
    if is_color_longhand(prop_kind)
        || prop_kind == PropertyIdKind::Opacity
        || crate::properties::should_use_values4_parser(prop_kind, &canonical)
    {
        return Some(vec![(lower_name, canonical)]);
    }
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
        return Some(vec![(lower_name, lower_val)]);
    }
    let cleaned = strip_outer_css_comments(value);
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
    Some(vec![(lower_name, canonical)])
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
    )
}

pub(crate) fn contains_var_or_env_ci(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("var(") || lower.contains("env(")
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

/// Validates CSS Variables L2 `var(<declaration-value> [, <declaration-value>?]?)`
/// and CSS Values L5 `{...}`-wrapped component function arguments.
fn validate_var_and_braces_syntax(s: &str) -> bool {
    let bytes = s.as_bytes();
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
            }
            i += 1;
            continue;
        }
        match b {
            b'\\' => {
                i += 2;
                continue;
            }
            b'"' | b'\'' => {
                in_str = Some(b);
                i += 1;
                continue;
            }
            b'{' | b'}' => return false,
            _ => {}
        }
        if i + 4 <= bytes.len()
            && bytes[i..i + 4].eq_ignore_ascii_case(b"var(")
            && (i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'-'
                    || bytes[i - 1] == b'_'))
        {
            let arg_start = i + 4;
            let mut depth = 1usize;
            let mut j = arg_start;
            let mut s_q: Option<u8> = None;
            let mut first_comma = None;
            while j < bytes.len() && depth > 0 {
                let c = bytes[j];
                if let Some(q) = s_q {
                    if c == b'\\' {
                        j += 2;
                        continue;
                    }
                    if c == q {
                        s_q = None;
                    }
                    j += 1;
                    continue;
                }
                match c {
                    b'\\' => {
                        j += 2;
                        continue;
                    }
                    b'"' | b'\'' => s_q = Some(c),
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    b',' if depth == 1 && first_comma.is_none() => first_comma = Some(j),
                    _ => {}
                }
                j += 1;
            }
            if depth != 0 {
                return false;
            }
            let inner_end = j;
            let name_part = s[arg_start..first_comma.unwrap_or(inner_end)].trim();
            if name_part.is_empty() {
                return false;
            }
            // If name argument contains `{` or `}`, it must be a single `{...}` wrapping a non-empty value.
            if name_part.contains('{') || name_part.contains('}') {
                if !is_single_brace_wrapped(name_part) {
                    return false;
                }
                let inside = name_part[1..name_part.len() - 1].trim();
                if inside.is_empty() {
                    return false;
                }
            }
            if let Some(comma) = first_comma {
                let fb = s[comma + 1..inner_end].trim();
                if (fb.contains('{') || fb.contains('}')) && !is_single_brace_wrapped(fb) {
                    return false;
                }
            }
            i = inner_end + 1;
            continue;
        }
        i += 1;
    }
    true
}

fn is_single_brace_wrapped(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'{' || bytes[bytes.len() - 1] != b'}' {
        return false;
    }
    let mut depth = 0usize;
    let mut in_str: Option<u8> = None;
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
            b'\\' => {
                i += 2;
                continue;
            }
            b'"' | b'\'' => in_str = Some(b),
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 && i + 1 != bytes.len() {
                    return false;
                }
            }
            _ => {}
        }
        i += 1;
    }
    depth == 0
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

/// Converts bare `"0"` length tokens to canonical CSSOM `"0px"`.
fn canonicalize_zero_lengths(input: &str) -> String {
    if input == "0" {
        return "0px".to_string();
    }
    if !input.contains('0') || input.contains("calc(") {
        return input.to_string();
    }
    input
        .split_ascii_whitespace()
        .map(|tok| if tok == "0" { "0px" } else { tok })
        .collect::<Vec<_>>()
        .join(" ")
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
            return Some(expand_border_sides(
                &["border-top", "border-right", "border-bottom", "border-left"],
                b,
                hint,
            ));
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
        _ => return None,
    };
    Some(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// Validates that `input` has balanced strings/delimiters and no top-level `;` or `!`.
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
