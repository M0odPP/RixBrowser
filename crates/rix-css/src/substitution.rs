//! W3C CSS Values and Units Level 5 arbitrary substitution functions:
//! `var()`, `inherit()`, `attr()`, `if()`, `random-item()`, and `ident()`,
//! with Dynamic Tarjan SCC cycle detection (CSSWG #11500) and token-level
//! `attr()`-tainting URL security enforcement (§9.4.1).

use std::collections::HashMap;
use std::fmt::Write as _;

use cssparser::{Parser, ParserInput, Token};
use rix_core::InternedStr;
use rix_dom::QualName;

use crate::computed::{ComputedStyle, CustomPropertyMap};
use crate::properties::{
    serialize_css_tokens, tokenize_custom_value_str, CssToken, GlobalKeyword, PropertyIdKind,
};
use crate::stylesheet::{evaluate_supports_condition_str, MediaContext, RegisteredProperty};
use crate::values::{ColorRgba, EvalContext};
use crate::values_math::{
    format_math_f64, hash_random_f64, parse_math_value, parse_math_value_from_parser, BaseDim,
    ValueKind,
};

const MAX_SUBST_DEPTH: u8 = 48;
const MAX_SUBST_BYTES: usize = 256 * 1024;

/// Live element environment supplied to CSS Values L5 arbitrary substitution.
pub struct SubstEnv<'a> {
    pub elem_attrs: &'a [(QualName, String)],
    pub parent_style: Option<&'a ComputedStyle>,
    pub registered_props: Option<&'a HashMap<InternedStr, RegisteredProperty>>,
    pub media: &'a MediaContext,
    pub eval: &'a EvalContext,
}

/// Output of substituting a CSS token stream, carrying byte ranges that are `attr()`-tainted.
#[derive(Debug, Clone, Default)]
pub struct SubstOutput {
    pub text: String,
    pub tainted_ranges: Vec<(usize, usize)>,
}

#[rustfmt::skip]
impl SubstOutput {
    #[must_use]
    pub fn is_tainted(&self) -> bool { !self.tainted_ranges.is_empty() }

    fn push_plain(&mut self, s: &str) { self.text.push_str(s); }

    fn push_tainted(&mut self, s: &str) {
        let start = self.text.len();
        self.text.push_str(s);
        let end = self.text.len();
        if end > start { self.tainted_ranges.push((start, end)); }
    }

    fn append_sub(&mut self, other: &Self, boundary_check: bool) {
        if other.text.is_empty() { return; }
        if boundary_check && needs_token_boundary_comment(&self.text, &other.text) { self.text.push_str("/**/"); }
        let off = self.text.len();
        self.text.push_str(&other.text);
        for &(a, b) in &other.tainted_ranges { self.tainted_ranges.push((a + off, b + off)); }
    }

    fn mark_all_tainted(&mut self) {
        self.tainted_ranges.clear();
        if !self.text.is_empty() { self.tainted_ranges.push((0, self.text.len())); }
    }

    fn overlaps_taint(&self, lo: usize, hi: usize) -> bool { self.tainted_ranges.iter().any(|&(a, b)| a < hi && lo < b) }
}

// ---------------------------------------------------------------------------
// Shared CSS Scanner & Parse-Time Validation Helpers
// ---------------------------------------------------------------------------

#[inline]
#[rustfmt::skip]
fn is_ident_byte(b: u8) -> bool { b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b >= 0x80 }

#[inline]
#[rustfmt::skip]
fn match_fn_at(bytes: &[u8], i: usize, name: &[u8]) -> bool {
    let n = name.len();
    i + n < bytes.len() && bytes[i + n] == b'('
        && (i == 0 || !is_ident_byte(bytes[i - 1])) && bytes[i..i + n].eq_ignore_ascii_case(name)
}

/// If `bytes[i]` starts an escape (`\x`), quoted string, or `/*...*/` comment, returns the byte
/// index immediately following that trivia; otherwise returns `None`.
#[inline]
#[rustfmt::skip]
fn skip_css_trivia(bytes: &[u8], i: usize) -> Option<usize> {
    let b = bytes[i];
    if b == b'\\' { return Some((i + 2).min(bytes.len())); }
    if b == b'"' || b == b'\'' {
        let mut j = i + 1;
        while j < bytes.len() {
            if bytes[j] == b'\\' {
                j = (j + 2).min(bytes.len());
                continue;
            }
            if bytes[j] == b { return Some(j + 1); }
            j += 1;
        }
        return Some(bytes.len());
    }
    if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
        let mut j = i + 2;
        while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') { j += 1; }
        return Some((j + 2).min(bytes.len()));
    }
    None
}

#[rustfmt::skip]
fn scan_for_fn_names(s: &str, names: &[&[u8]]) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(next) = skip_css_trivia(bytes, i) {
            i = next;
            continue;
        }
        if names.iter().any(|n| match_fn_at(bytes, i, n)) { return true; }
        i += 1;
    }
    false
}

/// Returns `true` if `s` contains any CSS arbitrary substitution function (`var`, `env`, `if`,
/// `attr`, `random-item`, `ident`, `inherit`) outside strings and comments.
#[must_use]
#[rustfmt::skip]
pub fn contains_subst_fn_str(s: &str) -> bool {
    scan_for_fn_names(s, &[
            b"var", b"env", b"if", b"attr", b"random-item", b"ident", b"inherit", ],
    )
}

/// Returns `true` if `s` contains a CSS Values L5 substitution function that `lightningcss`
/// does not natively parse (`if`, `attr`, `random-item`, `ident`, `inherit`).
#[must_use]
#[rustfmt::skip]
pub fn contains_values5_subst_fn_str(s: &str) -> bool {
    scan_for_fn_names(s, &[b"if", b"attr", b"random-item", b"ident", b"inherit"])
}

/// Returns `true` if `s` contains `attr(` outside strings and comments.
#[must_use]
#[rustfmt::skip]
pub fn contains_attr_fn_str(s: &str) -> bool { scan_for_fn_names(s, &[b"attr"]) }

/// Returns `true` if any token in `tokens` invokes a CSS substitution function.
#[must_use]
#[rustfmt::skip]
pub fn tokens_contain_subst(tokens: &[CssToken]) -> bool { tokens.iter().any(token_has_subst) }

#[must_use]
#[rustfmt::skip]
pub fn token_has_subst(tok: &CssToken) -> bool {
    match tok {
        CssToken::Var { .. } => true,
        CssToken::Function { name, args } => {
            let n = name.as_str();
            matches!(n.to_ascii_lowercase().as_str(), "var" | "if" | "attr" | "random-item" | "ident" | "inherit"
            ) || args.iter().any(token_has_subst)
        }
        _ => false,
    }
}

/// Collects lowercase attribute names statically referenced by `attr(...)` in `tokens`.
#[rustfmt::skip]
pub fn collect_attr_names_from_tokens(tokens: &[CssToken], out: &mut Vec<String>) {
    let raw = serialize_css_tokens(tokens);
    if !contains_attr_fn_str(&raw) { return; }
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(next) = skip_css_trivia(bytes, i) {
            i = next;
            continue;
        }
        if match_fn_at(bytes, i, b"attr") {
            let end = find_matching_paren(bytes, i + 4, true);
            let inner = &raw[i + 5..end];
            let head = split_first_top_level(inner, b',').map_or(inner, |(h, _)| h).trim();
            let first = head.split_ascii_whitespace().next().unwrap_or("");
            if !first.is_empty() && !first.contains('(') {
                let lower = first.to_ascii_lowercase();
                if !out.contains(&lower) { out.push(lower); }
            } else if !out.iter().any(|s| s == "*") { out.push("*".to_string()); }
        }
        i += 1;
    }
}

#[rustfmt::skip]
fn find_matching_paren(bytes: &[u8], open_idx: usize, allow_eof: bool) -> usize {
    let (mut depth, mut i) = (0usize, open_idx);
    while i < bytes.len() {
        if let Some(next) = skip_css_trivia(bytes, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 { return i; }
            }
            _ => {}
        }
        i += 1;
    }
    if allow_eof {
        bytes.len()
    } else { open_idx }
}

#[rustfmt::skip]
fn split_top_level(s: &str, sep: u8) -> Vec<&str> {
    let bytes = s.as_bytes();
    let (mut out, mut start, mut depth, mut i) = (Vec::new(), 0usize, 0usize, 0usize);
    while i < bytes.len() {
        if let Some(next) = skip_css_trivia(bytes, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            c if c == sep && depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&s[start..]);
    out
}

#[rustfmt::skip]
fn split_first_top_level(s: &str, sep: u8) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let (mut depth, mut i) = (0usize, 0usize);
    while i < bytes.len() {
        if let Some(next) = skip_css_trivia(bytes, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            c if c == sep && depth == 0 => return Some((&s[..i], &s[i + 1..])),
            _ => {}
        }
        i += 1;
    }
    None
}

#[rustfmt::skip]
fn strip_outer_pair(s: &str, open: u8, close: u8) -> Option<&str> {
    let t = s.trim();
    let bytes = t.as_bytes();
    if bytes.len() < 2 || bytes[0] != open || bytes[bytes.len() - 1] != close { return None; }
    let (mut depth, mut i) = (0usize, 0usize);
    while i < bytes.len() {
        if let Some(next) = skip_css_trivia(bytes, i) {
            i = next;
            continue;
        }
        if bytes[i] == open {
            depth += 1;
        } else if bytes[i] == close {
            depth = depth.saturating_sub(1);
            if depth == 0 && i + 1 != bytes.len() { return None; }
        }
        i += 1;
    }
    (depth == 0).then_some(&t[1..t.len() - 1])
}

#[rustfmt::skip]
fn has_top_level_braces(s: &str) -> bool {
    let bytes = s.as_bytes();
    let (mut depth, mut i) = (0usize, 0usize);
    while i < bytes.len() {
        if let Some(next) = skip_css_trivia(bytes, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' => depth += 1, b')' | b']' => depth = depth.saturating_sub(1),
            b'{' | b'}' if depth == 0 => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

#[rustfmt::skip]
fn has_top_level_bang(s: &str) -> bool { split_first_top_level(s, b'!').is_some() }

/// Validates parse-time syntax of CSS Values L5 substitution functions on a standard property.
#[must_use]
#[rustfmt::skip]
pub fn validate_subst_syntax(s: &str) -> bool {
    type ArgValidator = (&'static [u8], fn(&str) -> bool);
    let validators: &[ArgValidator] = &[
        (b"var", validate_var_args), (b"inherit", validate_inherit_args),
        (b"ident", validate_ident_args), (b"random-item", validate_random_item_args),
        (b"attr", validate_attr_args), (b"if", validate_if_args), ];
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(next) = skip_css_trivia(bytes, i) {
            i = next;
            continue;
        }
        if bytes[i] == b'{' || bytes[i] == b'}' { return false; }
        if let Some((name, check)) = validators.iter().find(|(n, _)| match_fn_at(bytes, i, n)) {
            let open = i + name.len();
            let end = find_matching_paren(bytes, open, true);
            if !check(&s[open + 1..end]) { return false; }
            i = (end + 1).min(bytes.len());
            continue;
        }
        i += 1;
    }
    true
}

#[rustfmt::skip]
fn is_valid_custom_prop_ident(s: &str) -> bool {
    let t = s.trim();
    t.starts_with("--") && t.len() > 2 && t.bytes().all(|b| is_ident_byte(b) || b == b'\\')
}

#[rustfmt::skip]
fn validate_var_args(inner: &str) -> bool {
    let (name_raw, fb_opt) = split_first_top_level(inner, b',').map_or((inner, None), |(a, b)| (a, Some(b)));
    let name_t = name_raw.trim();
    if name_t.is_empty() || has_top_level_bang(name_t) || split_first_top_level(name_t, b';').is_some()
        || (has_top_level_braces(name_t) && strip_outer_pair(name_t, b'{', b'}').is_none())
    { return false; }
    let unwrapped = strip_outer_pair(name_t, b'{', b'}').map_or(name_t, str::trim);
    if contains_subst_fn_str(unwrapped) && !validate_subst_syntax(unwrapped) { return false; }
    if let Some(fb) = fb_opt {
        let fbt = fb.trim();
        if has_top_level_braces(fbt) && strip_outer_pair(fbt, b'{', b'}').is_none() { return false; }
        if contains_subst_fn_str(fbt) && !validate_subst_syntax(fbt) { return false; }
    }
    true
}

#[rustfmt::skip]
fn validate_inherit_args(inner: &str) -> bool {
    let (name_raw, fb_opt) = split_first_top_level(inner, b',').map_or((inner, None), |(a, b)| (a, Some(b)));
    let name_t = name_raw.trim();
    let name_ok = !name_t.is_empty() && (is_valid_custom_prop_ident(name_t)
            || (contains_subst_fn_str(name_t) && validate_subst_syntax(name_t)));
    name_ok && fb_opt.is_none_or(|fb| !contains_subst_fn_str(fb) || validate_subst_syntax(fb))
}

#[rustfmt::skip]
fn validate_ident_args(inner: &str) -> bool {
    let t = inner.trim();
    !t.is_empty() && split_first_top_level(t, b',').is_none()
        && !has_top_level_braces(t) && (!contains_subst_fn_str(t) || validate_subst_syntax(t))
}

#[rustfmt::skip]
fn validate_random_item_args(inner: &str) -> bool {
    let parts = split_top_level(inner, b',');
    if parts.len() < 2 || parts[0].trim().is_empty() || has_top_level_braces(parts[0]) || has_top_level_bang(parts[0])
        || split_first_top_level(parts[0], b';').is_some()
    { return false; }
    for item in &parts[1..] {
        let trimmed = item.trim();
        let body = if let Some(b) = strip_outer_pair(trimmed, b'{', b'}') {
            b
        } else if has_top_level_braces(trimmed) {
            return false;
        } else { trimmed };
        if has_top_level_bang(body) || split_first_top_level(body, b';').is_some()
            || (contains_subst_fn_str(body) && !validate_subst_syntax(body))
        { return false; }
    }
    true
}

#[rustfmt::skip]
fn validate_attr_args(inner: &str) -> bool {
    let (head_raw, fb_opt) = split_first_top_level(inner, b',').map_or((inner, None), |(a, b)| (a, Some(b)));
    let head = head_raw.trim();
    if head.is_empty() { return false; }
    if !contains_subst_fn_str(head) {
        if let Some(ty_pos) = find_ci_fn(head, b"type") {
            let end = find_matching_paren(head.as_bytes(), ty_pos + 4, true);
            if has_invalid_angle_syntax_type(head[ty_pos + 5..end].trim()) { return false; }
        }
    }
    if let Some(fb) = fb_opt {
        let fbt = fb.trim();
        if has_top_level_braces(fbt) && strip_outer_pair(fbt, b'{', b'}').is_none() { return false; }
        let unwrapped = strip_outer_pair(fbt, b'{', b'}').unwrap_or(fbt);
        if contains_subst_fn_str(unwrapped) && !validate_subst_syntax(unwrapped) { return false; }
    }
    true
}

#[rustfmt::skip]
fn validate_if_args(inner: &str) -> bool {
    let t = inner.trim();
    if t.is_empty() { return false; }
    let branches = split_top_level(t, b';');
    let mut saw_branch = false;
    for (idx, br) in branches.iter().enumerate() {
        let bt = br.trim();
        if bt.is_empty() {
            if idx + 1 == branches.len() && saw_branch { break; }
            return false;
        }
        let Some((cond, _)) = split_first_top_level(bt, b':') else { return false; };
        if cond.trim().is_empty() { return false; }
        saw_branch = true;
    }
    saw_branch
}

#[rustfmt::skip]
fn find_ci_fn(s: &str, name: &[u8]) -> Option<usize> {
    let bytes = s.as_bytes();
    (0..bytes.len()).find(|&i| match_fn_at(bytes, i, name))
}

#[rustfmt::skip]
fn is_known_css_syntax_type(name: &str) -> bool {
    // CSS Values L5 §9.2: `<url>` is explicitly forbidden in `attr(... type(...))` for security.
    matches!(name.to_ascii_lowercase().as_str(),
        "angle" | "color" | "custom-ident" | "ident" | "image" | "integer" | "length"
            | "length-percentage" | "number" | "percentage" | "resolution" | "string"
            | "time" | "frequency" | "transform-function" | "transform-list"
    )
}

#[rustfmt::skip]
fn has_invalid_angle_syntax_type(syn: &str) -> bool {
    let bytes = syn.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let start = i + 1;
            let Some(rel) = syn[start..].find('>') else { return true; };
            if !is_known_css_syntax_type(&syn[start..start + rel]) { return true; }
            i = start + rel + 1;
            continue;
        }
        i += 1;
    }
    false
}

#[rustfmt::skip]
fn needs_token_boundary_comment(left: &str, right: &str) -> bool {
    let (Some(lb), Some(rb)) = (left.bytes().last(), right.bytes().next()) else { return false; };
    if lb.is_ascii_whitespace() || rb.is_ascii_whitespace() { return false; }
    let l_word = is_ident_byte(lb);
    let r_word = is_ident_byte(rb) || matches!(rb, b'(' | b'%' | b'+');
    (l_word && r_word) || (matches!(lb, b'+' | b'-' | b'.') && r_word)
        || (lb == b'/' && rb == b'*') || (lb == b'-' && rb == b'-')
}

/// Serializes `raw` as a CSS `<string>` token per CSSOM §6.7.1.
#[must_use]
#[rustfmt::skip]
pub fn quote_css_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""), '\\' => out.push_str("\\\\"),
            '\0' => out.push('\u{FFFD}'),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\{:x} ", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Serializes `raw` as a CSS `<ident>` token per CSSOM §6.7.1.
#[must_use]
#[rustfmt::skip]
pub fn escape_css_ident(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let chars: Vec<char> = raw.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        if ch == '\0' {
            out.push('\u{FFFD}');
        } else if (ch as u32) <= 0x1f || ch as u32 == 0x7f
            || (i == 0 && ch.is_ascii_digit()) || (i == 1 && ch.is_ascii_digit() && chars[0] == '-')
        {
            let _ = write!(out, "\\{:x} ", ch as u32);
        } else if i == 0 && ch == '-' && chars.len() == 1 {
            out.push_str("\\-");
        } else if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || !ch.is_ascii() {
            out.push(ch);
        } else {
            out.push('\\');
            out.push(ch);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Token-Level `attr()`-Tainted URL Security Enforcement (CSS Values L5 §9.4.1)
// ---------------------------------------------------------------------------

/// Returns `true` if `out` contains an `attr()`-tainted `<url>` in a URL-accepting property
/// (CSS Values L5 §9.4.1 `#attr-security`), which makes the declaration IACVT.
#[must_use]
#[rustfmt::skip]
pub fn is_tainted_url_violation(prop: PropertyIdKind, out: &SubstOutput) -> bool {
    out.is_tainted() && matches!(prop, PropertyIdKind::BackgroundLayers | PropertyIdKind::Background
                | PropertyIdKind::Cursor | PropertyIdKind::Content
        ) && has_tainted_url_in_output(out)
}

#[must_use]
#[rustfmt::skip]
pub fn has_tainted_url_in_output(out: &SubstOutput) -> bool {
    if !out.is_tainted() { return false; }
    let s = out.text.as_str();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(next) = skip_css_trivia(bytes, i) {
            i = next;
            continue;
        }
        if let Some(len) = [b"url".as_slice(), b"src".as_slice(), b"image".as_slice()].into_iter()
            .find(|n| match_fn_at(bytes, i, n)).map(<[u8]>::len)
        {
            let end = find_matching_paren(bytes, i + len, true);
            if out.overlaps_taint(i, (end + 1).min(bytes.len())) { return true; }
            i = (end + 1).min(bytes.len());
            continue;
        }
        if let Some(len) = [b"image-set".as_slice(), b"-webkit-image-set".as_slice()].into_iter()
            .find(|n| match_fn_at(bytes, i, n)).map(<[u8]>::len)
        {
            let end = find_matching_paren(bytes, i + len, true);
            if image_set_has_tainted_url(s, i + len + 1, end, out) { return true; }
            i = (end + 1).min(bytes.len());
            continue;
        }
        i += 1;
    }
    false
}

#[rustfmt::skip]
fn image_set_has_tainted_url(s: &str, start: usize, end: usize, out: &SubstOutput) -> bool {
    let mut cursor = start;
    for part in split_top_level(&s[start..end], b',') {
        let (part_start, part_end) = (cursor, cursor + part.len());
        cursor = part_end + 1;
        let trimmed = part.trim_start();
        let item_start = part_start + (part.len() - trimmed.len());
        let tb = trimmed.as_bytes();
        if tb.is_empty() { continue; }
        // CSS Values L5 §9.3: inside `image-set()`, attr()-taint in URLs, resolutions, or `type()`
        // is forbidden; only non-URL `<gradient>` arguments are exempt from tainting option selection.
        let grad_len = [
            b"linear-gradient".as_slice(), b"radial-gradient".as_slice(),
            b"conic-gradient".as_slice(), b"repeating-linear-gradient".as_slice(),
            b"repeating-radial-gradient".as_slice(), b"repeating-conic-gradient".as_slice(),
        ].into_iter().find(|n| match_fn_at(tb, 0, n)).map(<[u8]>::len);
        if let Some(glen) = grad_len {
            let after_grad = item_start + (find_matching_paren(tb, glen, true) + 1).min(tb.len());
            if after_grad < part_end && out.overlaps_taint(after_grad, part_end) { return true; }
        } else if out.overlaps_taint(part_start, part_end) { return true; }
    }
    false
}

// ---------------------------------------------------------------------------
// Unified Dynamic Tarjan SCC Substitution Engine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvalStatus {
    Valid,
    Invalid,
    PropCycle(usize),
    AttrCycle(usize),
}

struct SubstEngine<'a> {
    raw_map: &'a HashMap<InternedStr, String>,
    raw_tainted: &'a HashMap<InternedStr, Vec<(usize, usize)>>,
    env: &'a SubstEnv<'a>,
    next_index: usize,
    indices: HashMap<InternedStr, usize>,
    lowlink: HashMap<InternedStr, usize>,
    on_stack: HashMap<InternedStr, bool>,
    tarjan_stack: Vec<InternedStr>,
    resolved: HashMap<InternedStr, Option<SubstOutput>>,
    active_attrs: Vec<String>,
    random_item_counter: u32,
    early_attr_random_counter: u32,
    cond_random_counter: u32,
}

#[rustfmt::skip]
impl<'a> SubstEngine<'a> {
    fn new(raw_map: &'a HashMap<InternedStr, String>,
        raw_tainted: &'a HashMap<InternedStr, Vec<(usize, usize)>>, env: &'a SubstEnv<'a>,
    ) -> Self {
        let cap = raw_map.len();
        Self {
            raw_map, raw_tainted, env, next_index: 0,
            indices: HashMap::with_capacity(cap), lowlink: HashMap::with_capacity(cap),
            on_stack: HashMap::with_capacity(cap), tarjan_stack: Vec::with_capacity(cap),
            resolved: HashMap::with_capacity(cap), active_attrs: Vec::new(),
            random_item_counter: 0, early_attr_random_counter: 0, cond_random_counter: 0,
        }
    }

    fn append_budgeted(out: &mut SubstOutput, val: &SubstOutput, budget: &mut usize, boundary: bool) -> EvalStatus {
        if val.text.len() > *budget {
            *budget = 0;
            return EvalStatus::Invalid;
        }
        *budget -= val.text.len();
        out.append_sub(val, boundary);
        EvalStatus::Valid
    }

    fn eval_fallback(&mut self, owner: Option<&InternedStr>, prop_name: &str,
        fb_opt: Option<&str>, depth: u8, out: &mut SubstOutput, budget: &mut usize,
    ) -> EvalStatus {
        fb_opt.map_or(EvalStatus::Invalid, |fb| {
            let fb_src = strip_outer_pair(fb.trim(), b'{', b'}').unwrap_or(fb);
            self.eval_str(owner, prop_name, fb_src, depth + 1, out, budget)
        })
    }

    fn resolve_prop(&mut self, name: &InternedStr, depth: u8) -> Option<SubstOutput> {
        if let Some(res) = self.resolved.get(name) { return res.clone(); }
        let raw = self.raw_map.get(name)?;
        if !contains_subst_fn_str(raw) {
            let out = SubstOutput { text: raw.clone(), tainted_ranges: self.raw_tainted.get(name).cloned().unwrap_or_default(), };
            self.resolved.insert(name.clone(), Some(out.clone()));
            return Some(out);
        }
        self.strongconnect(name, depth);
        self.resolved.get(name).cloned().flatten()
    }

    fn strongconnect(&mut self, name: &InternedStr, depth: u8) {
        let my_idx = self.next_index;
        self.next_index += 1;
        self.indices.insert(name.clone(), my_idx);
        self.lowlink.insert(name.clone(), my_idx);
        self.on_stack.insert(name.clone(), true);
        self.tarjan_stack.push(name.clone());

        let raw = self.raw_map.get(name).map_or("", String::as_str);
        let (mut out, mut budget) = (SubstOutput::default(), MAX_SUBST_BYTES);
        let saved = (self.random_item_counter, self.early_attr_random_counter, self.cond_random_counter,
            std::mem::take(&mut self.active_attrs), );
        self.random_item_counter = 0;
        self.early_attr_random_counter = 0;
        self.cond_random_counter = 0;

        let status = self.eval_str(Some(name), name.as_str(), raw, depth, &mut out, &mut budget);

        (self.random_item_counter, self.early_attr_random_counter, self.cond_random_counter, self.active_attrs, ) = saved;

        if self.lowlink.get(name).copied().unwrap_or(my_idx) == my_idx {
            let mut scc = Vec::new();
            while let Some(w) = self.tarjan_stack.pop() {
                self.on_stack.insert(w.clone(), false);
                let is_root = &w == name;
                scc.push(w);
                if is_root { break; }
            }
            if scc.len() > 1 || matches!(status, EvalStatus::PropCycle(_)) {
                for node in scc { self.resolved.insert(node, None); }
            } else if status != EvalStatus::Valid {
                self.resolved.insert(name.clone(), None);
            } else { self.resolved.insert(name.clone(), Some(out)); }
        }
    }

    fn eval_str(&mut self, owner: Option<&InternedStr>, prop_name: &str,
        src: &str, depth: u8, out: &mut SubstOutput, budget: &mut usize,
    ) -> EvalStatus {
        if depth > MAX_SUBST_DEPTH || *budget == 0 {
            *budget = 0;
            return EvalStatus::Invalid;
        }
        let bytes = src.as_bytes();
        let (mut i, mut slice_status, mut just_substituted) = (0usize, EvalStatus::Valid, false);

        while i < bytes.len() {
            if let Some(next) = skip_css_trivia(bytes, i) {
                if slice_status == EvalStatus::Valid {
                    if just_substituted {
                        just_substituted = false;
                        if needs_token_boundary_comment(&out.text, &src[i..next]) { out.push_plain("/**/"); }
                    }
                    out.push_plain(&src[i..next]);
                }
                i = next;
                continue;
            }
            let fn_kind = [
                (0u8, b"var".as_slice()), (0u8, b"__rix_var__".as_slice()),
                (1u8, b"inherit".as_slice()), (2u8, b"attr".as_slice()),
                (3u8, b"if".as_slice()), (4u8, b"random-item".as_slice()), (5u8, b"ident".as_slice()), ].into_iter()
            .find(|(_, n)| match_fn_at(bytes, i, n)).map(|(k, n)| (k, n.len()));

            if let Some((kind, name_len)) = fn_kind {
                let open = i + name_len;
                let close = find_matching_paren(bytes, open, true);
                let inner = &src[open + 1..close];
                let mut sub = SubstOutput::default();
                let st = match kind {
                    0 => self.eval_var(owner, prop_name, inner, depth + 1, &mut sub, budget),
                    1 => self.eval_inherit(owner, prop_name, inner, depth + 1, &mut sub, budget),
                    2 => self.eval_attr(owner, prop_name, inner, depth + 1, &mut sub, budget),
                    3 => self.eval_if(owner, prop_name, inner, depth + 1, &mut sub, budget),
                    4 => { self.eval_random_item(owner, prop_name, inner, depth + 1, &mut sub, budget) }
                    _ => self.eval_ident(owner, prop_name, inner, depth + 1, &mut sub, budget),
                };
                match st {
                    EvalStatus::Valid if slice_status == EvalStatus::Valid => {
                        if Self::append_budgeted(out, &sub, budget, true) == EvalStatus::Invalid {
                            slice_status = EvalStatus::Invalid;
                        } else { just_substituted = true; }
                    }
                    EvalStatus::Valid => {}
                    EvalStatus::PropCycle(low) => {
                        if let Some(cur) = owner.and_then(|ow| self.lowlink.get_mut(ow)) { *cur = (*cur).min(low); }
                        slice_status = match slice_status {
                            EvalStatus::PropCycle(prev) => EvalStatus::PropCycle(prev.min(low)),
                            _ => EvalStatus::PropCycle(low),
                        };
                    }
                    EvalStatus::AttrCycle(d) => {
                        if !matches!(slice_status, EvalStatus::PropCycle(_)) {
                            slice_status = match slice_status {
                                EvalStatus::AttrCycle(p) => EvalStatus::AttrCycle(p.min(d)), _ => EvalStatus::AttrCycle(d),
                            };
                        }
                    }
                    EvalStatus::Invalid if slice_status == EvalStatus::Valid => { slice_status = EvalStatus::Invalid; }
                    EvalStatus::Invalid => {}
                }
                i = (close + 1).min(bytes.len());
                continue;
            }

            let ch_len = src[i..].chars().next().map_or(1, char::len_utf8);
            if slice_status == EvalStatus::Valid {
                if just_substituted {
                    just_substituted = false;
                    if needs_token_boundary_comment(&out.text, &src[i..i + ch_len]) { out.push_plain("/**/"); }
                }
                if ch_len > *budget {
                    *budget = 0;
                    slice_status = EvalStatus::Invalid;
                } else {
                    *budget -= ch_len;
                    out.push_plain(&src[i..i + ch_len]);
                }
            }
            i += ch_len;
        }
        slice_status
    }

    fn eval_var_lookup(&mut self, owner: Option<&InternedStr>, dep: &InternedStr,
        depth: u8, out: &mut SubstOutput, budget: &mut usize,
    ) -> EvalStatus {
        if depth > MAX_SUBST_DEPTH {
            *budget = 0;
            return EvalStatus::Invalid;
        }
        if let Some(resolved_opt) = self.resolved.get(dep) {
            return resolved_opt.as_ref().map_or(EvalStatus::Invalid, |val| {
                Self::append_budgeted(out, val, budget, false)
            });
        }
        if self.on_stack.get(dep).copied().unwrap_or(false) {
            let dep_idx = self.indices.get(dep).copied().unwrap_or(0);
            if let Some(cur) = owner.and_then(|ow| self.lowlink.get_mut(ow)) { *cur = (*cur).min(dep_idx); }
            return EvalStatus::PropCycle(dep_idx);
        }
        let Some(dep_raw) = self.raw_map.get(dep) else { return EvalStatus::Invalid; };
        if !contains_subst_fn_str(dep_raw) {
            let val = SubstOutput {
                text: dep_raw.clone(), tainted_ranges: self.raw_tainted.get(dep).cloned().unwrap_or_default(),
            };
            self.resolved.insert(dep.clone(), Some(val.clone()));
            return Self::append_budgeted(out, &val, budget, false);
        }
        self.strongconnect(dep, depth);
        if self.on_stack.get(dep).copied().unwrap_or(false) {
            let dep_low = self.lowlink.get(dep).copied().unwrap_or(0);
            if let Some(cur) = owner.and_then(|ow| self.lowlink.get_mut(ow)) { *cur = (*cur).min(dep_low); }
            return EvalStatus::PropCycle(dep_low);
        }
        self.resolved.get(dep)
            .and_then(Option::as_ref).map_or(EvalStatus::Invalid, |val| {
                Self::append_budgeted(out, val, budget, false)
            })
    }

    fn eval_var(&mut self, owner: Option<&InternedStr>, prop_name: &str,
        inner: &str, depth: u8, out: &mut SubstOutput, budget: &mut usize,
    ) -> EvalStatus {
        let (name_raw, fb_opt) = split_first_top_level(inner, b',').map_or((inner, None), |(a, b)| (a, Some(b)));
        let name_src = strip_outer_pair(name_raw.trim(), b'{', b'}').unwrap_or(name_raw);
        let mut name_sub = SubstOutput::default();
        let name_st = self.eval_str(owner, prop_name, name_src, depth + 1, &mut name_sub, budget);
        let name_tainted = name_sub.is_tainted();
        let dep_st = match name_st {
            EvalStatus::Valid => {
                let nt = name_sub.text.trim();
                if is_valid_custom_prop_ident(nt) {
                    let dep = rix_core::intern_str(nt);
                    // CSS Properties & Values L1 §3.2: referencing a registered font-relative
                    // custom property while resolving `font-size` is a dependency cycle.
                    if owner.is_none() && prop_name.eq_ignore_ascii_case("font-size")
                        && self.is_registered_font_relative_dep(&dep)
                    { return EvalStatus::Invalid; }
                    let mut val_out = SubstOutput::default();
                    let st = self.eval_var_lookup(owner, &dep, depth + 1, &mut val_out, budget);
                    if st == EvalStatus::Valid {
                        if name_tainted { val_out.mark_all_tainted(); }
                        out.append_sub(&val_out, false);
                    }
                    st
                } else { EvalStatus::Invalid }
            }
            other => other,
        };
        if dep_st != EvalStatus::Invalid { return dep_st; }
        let mut fb_out = SubstOutput::default();
        let st = self.eval_fallback(owner, prop_name, fb_opt, depth, &mut fb_out, budget);
        if st == EvalStatus::Valid {
            if name_tainted { fb_out.mark_all_tainted(); }
            out.append_sub(&fb_out, false);
        }
        st
    }

    fn eval_inherit(&mut self, owner: Option<&InternedStr>, prop_name: &str,
        inner: &str, depth: u8, out: &mut SubstOutput, budget: &mut usize,
    ) -> EvalStatus {
        let (name_raw, fb_opt) = split_first_top_level(inner, b',').map_or((inner, None), |(a, b)| (a, Some(b)));
        let mut name_sub = SubstOutput::default();
        let name_st = self.eval_str(owner, prop_name, name_raw, depth + 1, &mut name_sub, budget);
        if !matches!(name_st, EvalStatus::Valid) {
            return match name_st {
                EvalStatus::PropCycle(l) => EvalStatus::PropCycle(l), EvalStatus::AttrCycle(d) => EvalStatus::AttrCycle(d),
                _ => self.eval_fallback(owner, prop_name, fb_opt, depth, out, budget),
            };
        }
        let nt = name_sub.text.trim();
        if is_valid_custom_prop_ident(nt) {
            let key = rix_core::intern_str(nt);
            if let Some(toks) = self.env.parent_style.and_then(|p| p.custom_properties.get(&key))
            {
                let mut pval = SubstOutput {
                    text: serialize_css_tokens(toks), tainted_ranges: self.env
                        .parent_style.and_then(|p| p.custom_properties.tainted_ranges.get(&key))
                        .cloned().unwrap_or_default(),
                };
                if name_sub.is_tainted() { pval.mark_all_tainted(); }
                out.append_sub(&pval, false);
                return EvalStatus::Valid;
            }
        }
        self.eval_fallback(owner, prop_name, fb_opt, depth, out, budget)
    }

    fn is_registered_font_relative_dep(&self, dep: &InternedStr) -> bool {
        let Some(reg) = self.env.registered_props.and_then(|m| m.get(dep)) else { return false; };
        let syn_l = reg.syntax.to_ascii_lowercase();
        if !syn_l.contains("<length>") && !syn_l.contains("<length-percentage>") { return false; }
        let Some(raw) = self.raw_map.get(dep) else { return false; };
        let mut pi = ParserInput::new(raw);
        let mut p = Parser::new(&mut pi);
        while let Ok(tok) = p.next() {
            if let Token::Dimension { unit, .. } = tok {
                if matches!(unit.to_ascii_lowercase().as_str(), "em" | "ex" | "cap" | "ch" | "ic" | "lh"
                ) { return true; }
            }
        }
        false
    }

    fn lookup_attr_value(&self, attr_name: &str) -> Option<&'a str> {
        self.env.elem_attrs.iter().find(|(q, _)| q.local.as_str().eq_ignore_ascii_case(attr_name)).map(|(_, v)| v.as_str())
    }

    fn eval_attr(&mut self, owner: Option<&InternedStr>, prop_name: &str,
        inner: &str, depth: u8, out: &mut SubstOutput, budget: &mut usize,
    ) -> EvalStatus {
        let (head_raw, fb_opt) = split_first_top_level(inner, b',').map_or((inner, None), |(a, b)| (a, Some(b)));
        let mut head_sub = SubstOutput::default();
        match self.eval_str(owner, prop_name, head_raw, depth + 1, &mut head_sub, budget) {
            EvalStatus::PropCycle(l) => return EvalStatus::PropCycle(l),
            EvalStatus::AttrCycle(d) => return EvalStatus::AttrCycle(d),
            EvalStatus::Invalid => { return self.eval_fallback(owner, prop_name, fb_opt, depth, out, budget); }
            EvalStatus::Valid => {}
        }
        let head_t = head_sub.text.trim();
        if split_first_top_level(head_t, b',').is_some() { return EvalStatus::Invalid; }
        let Some((attr_name, attr_type)) = parse_attr_head(head_t) else {
            return self.eval_fallback(owner, prop_name, fb_opt, depth, out, budget);
        };
        let lower_attr = attr_name.to_ascii_lowercase();
        if let Some(pos) = self.active_attrs.iter().position(|a| a == &lower_attr) { return EvalStatus::AttrCycle(pos); }

        let raw_attr = self.lookup_attr_value(&attr_name);
        match attr_type {
            AttrType::RawString => {
                if let Some(val) = raw_attr {
                    out.push_tainted(&quote_css_string(val));
                    EvalStatus::Valid
                } else if fb_opt.is_some() {
                    self.eval_fallback(owner, prop_name, fb_opt, depth, out, budget)
                } else {
                    out.push_tainted("\"\"");
                    EvalStatus::Valid
                }
            }
            AttrType::Number => match raw_attr.and_then(parse_attr_number) {
                Some(val) => {
                    out.push_tainted(&format_math_f64(val));
                    EvalStatus::Valid
                }
                None => self.eval_fallback(owner, prop_name, fb_opt, depth, out, budget),
            },
            AttrType::Unit(unit) => match raw_attr.and_then(parse_attr_number) {
                Some(val) => {
                    out.push_tainted(&format!("{}{unit}", format_math_f64(val)));
                    EvalStatus::Valid
                }
                None => self.eval_fallback(owner, prop_name, fb_opt, depth, out, budget),
            },
            AttrType::Type(syntax) => {
                let Some(attr_val) = raw_attr.filter(|v| crate::cssom_decl::has_valid_css_tokens_pub(v, true))
                else { return self.eval_fallback(owner, prop_name, fb_opt, depth, out, budget); };
                let my_attr_pos = self.active_attrs.len();
                self.active_attrs.push(lower_attr);
                let mut val_sub = SubstOutput::default();
                let st = self.eval_str(owner, prop_name, attr_val, depth + 1, &mut val_sub, budget);
                self.active_attrs.pop();

                match st {
                    EvalStatus::PropCycle(l) => return EvalStatus::PropCycle(l),
                    EvalStatus::AttrCycle(pos) if pos < my_attr_pos => { return EvalStatus::AttrCycle(pos); }
                    EvalStatus::AttrCycle(_) | EvalStatus::Invalid => {
                        return self.eval_fallback(owner, prop_name, fb_opt, depth, out, budget);
                    }
                    EvalStatus::Valid => {}
                }
                if let Some(final_text) = validate_and_eval_typed_attr(&syntax, val_sub.text.trim(), prop_name,
                    self.env.eval, &mut self.early_attr_random_counter, false,
                ) {
                    out.push_tainted(&final_text);
                    EvalStatus::Valid
                } else { self.eval_fallback(owner, prop_name, fb_opt, depth, out, budget) }
            }
        }
    }

    fn eval_if(&mut self, owner: Option<&InternedStr>, prop_name: &str,
        inner: &str, depth: u8, out: &mut SubstOutput, budget: &mut usize,
    ) -> EvalStatus {
        if inner.trim().is_empty() { return EvalStatus::Invalid; }
        let branches = split_top_level(inner, b';');
        let mut parsed = Vec::with_capacity(branches.len());
        for (idx, br) in branches.iter().enumerate() {
            if br.trim().is_empty() {
                if idx + 1 == branches.len() && !parsed.is_empty() { break; }
                return EvalStatus::Invalid;
            }
            let Some((cond, val)) = split_first_top_level(br, b':') else { return EvalStatus::Invalid; };
            let cond_t = cond.trim();
            if cond_t.is_empty() || val.trim_end().ends_with('!') { return EvalStatus::Invalid; }
            parsed.push((cond_t, val));
        }
        if parsed.is_empty() { return EvalStatus::Invalid; }

        let mut cond_tainted = false;
        for (cond_raw, val_raw) in parsed {
            let (matched, c_taint, c_status) = self.eval_if_condition(owner, prop_name, cond_raw, depth + 1, budget);
            cond_tainted |= c_taint;
            if c_status != EvalStatus::Valid { return c_status; }
            if matched {
                let val_trimmed = val_raw.trim_start_matches(|c: char| c.is_ascii_whitespace());
                let mut val_out = SubstOutput::default();
                let st = self.eval_str(owner, prop_name, val_trimmed, depth + 1, &mut val_out, budget);
                if st != EvalStatus::Valid { return st; }
                if cond_tainted { val_out.mark_all_tainted(); }
                out.append_sub(&val_out, false);
                return EvalStatus::Valid;
            }
        }
        EvalStatus::Invalid
    }

    fn eval_if_condition(&mut self, owner: Option<&InternedStr>, prop_name: &str,
        cond_raw: &str, depth: u8, budget: &mut usize,
    ) -> (bool, bool, EvalStatus) {
        let mut t = cond_raw.trim();
        let owned_sub: String;
        let mut tainted = false;
        if !starts_with_if_cond_kw(t) && contains_subst_fn_str(t) {
            let mut sub = SubstOutput::default();
            let st = self.eval_str(owner, prop_name, t, depth + 1, &mut sub, budget);
            tainted |= sub.is_tainted();
            if st != EvalStatus::Valid { return (false, tainted, st); }
            owned_sub = sub.text;
            t = owned_sub.trim();
        }
        if t.eq_ignore_ascii_case("else") { return (true, tainted, EvalStatus::Valid); }
        if t.starts_with('!') { return (false, tainted, EvalStatus::Invalid); }
        let (res, c_taint, st) = self.eval_if_bool_expr(owner, prop_name, t, depth + 1, budget);
        (res.unwrap_or(false), tainted || c_taint, st)
    }

    fn eval_if_bool_expr(&mut self, owner: Option<&InternedStr>, prop_name: &str, expr: &str, depth: u8, budget: &mut usize,
    ) -> (Option<bool>, bool, EvalStatus) {
        let t = expr.trim();
        if t.is_empty() { return (None, false, EvalStatus::Valid); }
        if let Some(inner) = strip_outer_pair(t, b'(', b')') {
            return self.eval_if_bool_expr(owner, prop_name, inner, depth + 1, budget);
        }
        let (ops, parts) = split_bool_combinators(t);
        if !ops.is_empty() {
            let first_op = ops[0];
            if ops.iter().any(|&o| o != first_op) { return (None, false, EvalStatus::Valid); }
            let (mut tainted, mut acc, mut worst_st) = (false, Some(first_op == BoolOp::And), EvalStatus::Valid);
            for part in parts {
                let (v, pt, st) = self.eval_if_bool_expr(owner, prop_name, part, depth + 1, budget);
                tainted |= pt;
                if matches!(st, EvalStatus::PropCycle(_) | EvalStatus::AttrCycle(_)) { return (None, tainted, st); }
                if st != EvalStatus::Valid { worst_st = st; }
                acc = match first_op { BoolOp::And => kleene_and(acc, v), BoolOp::Or => kleene_or(acc, v), };
            }
            return (acc, tainted, worst_st);
        }
        if let Some(rest) = strip_leading_kw_ci(t, "not") {
            let (v, pt, st) = self.eval_if_bool_expr(owner, prop_name, rest, depth + 1, budget);
            return (v.map(|b| !b), pt, st);
        }
        let tb = t.as_bytes();
        if match_fn_at(tb, 0, b"style") && find_matching_paren(tb, 5, false) + 1 == tb.len() {
            return self.eval_style_query(owner, prop_name, &t[6..tb.len() - 1], depth + 1, budget);
        }
        if match_fn_at(tb, 0, b"media") && find_matching_paren(tb, 5, false) + 1 == tb.len() {
            let ok = self.env.media.evaluate_media_query_str(t[6..tb.len() - 1].trim());
            return (Some(ok), false, EvalStatus::Valid);
        }
        if match_fn_at(tb, 0, b"supports") && find_matching_paren(tb, 8, false) + 1 == tb.len() {
            let ok = evaluate_supports_condition_str(t[9..tb.len() - 1].trim());
            return (Some(ok), false, EvalStatus::Valid);
        }
        (None, false, EvalStatus::Valid)
    }

    fn eval_style_query(&mut self, owner: Option<&InternedStr>, prop_name: &str, expr: &str, depth: u8, budget: &mut usize,
    ) -> (Option<bool>, bool, EvalStatus) {
        let t = expr.trim();
        if t.is_empty() { return (None, false, EvalStatus::Valid); }
        if let Some(inner) = strip_outer_pair(t, b'(', b')') {
            return self.eval_style_query(owner, prop_name, inner, depth + 1, budget);
        }
        let (ops, parts) = split_bool_combinators(t);
        if !ops.is_empty() {
            let first_op = ops[0];
            let p0 = parts[0].trim_start().to_ascii_lowercase();
            if ops.iter().any(|&o| o != first_op) || p0.starts_with("not ") || p0.starts_with("not(")
            { return (None, false, EvalStatus::Valid); }
            let (mut tainted, mut acc) = (false, Some(first_op == BoolOp::And));
            for part in parts {
                let (v, pt, st) = self.eval_style_query(owner, prop_name, part, depth + 1, budget);
                tainted |= pt;
                if matches!(st, EvalStatus::PropCycle(_) | EvalStatus::AttrCycle(_)) { return (None, tainted, st); }
                acc = match first_op { BoolOp::And => kleene_and(acc, v), BoolOp::Or => kleene_or(acc, v), };
            }
            return (acc, tainted, EvalStatus::Valid);
        }
        if let Some(rest) = strip_leading_kw_ci(t, "not") {
            let (v, pt, st) = self.eval_style_query(owner, prop_name, rest, depth + 1, budget);
            return (v.map(|b| !b), pt, st);
        }
        if has_top_level_bang(t) { return (Some(false), false, EvalStatus::Valid); }
        if let Some((lhs, rhs)) = split_first_top_level(t, b':') {
            let prop_t = lhs.trim();
            if !is_valid_custom_prop_ident(prop_t) { return (Some(false), false, EvalStatus::Valid); }
            return self.eval_style_prop_eq(owner, prop_name, prop_t, rhs.trim(), depth + 1, budget);
        }
        if let Some(range_parts) = split_style_range(t) {
            return self.eval_style_range(owner, prop_name, &range_parts, depth + 1, budget);
        }
        if is_valid_custom_prop_ident(t) {
            let key = rix_core::intern_str(t);
            let mut val = SubstOutput::default();
            return match self.eval_var_lookup(owner, &key, depth + 1, &mut val, budget) {
                EvalStatus::PropCycle(l) => (None, val.is_tainted(), EvalStatus::PropCycle(l)),
                EvalStatus::AttrCycle(d) => (None, val.is_tainted(), EvalStatus::AttrCycle(d)),
                EvalStatus::Valid => {
                    let is_init = self.custom_prop_matches_initial(&key, &val.text);
                    (Some(!is_init), val.is_tainted(), EvalStatus::Valid)
                }
                EvalStatus::Invalid => (Some(false), false, EvalStatus::Valid),
            };
        }
        (Some(false), false, EvalStatus::Valid)
    }

    fn custom_prop_matches_initial(&self, key: &InternedStr, val_text: &str) -> bool {
        if let Some(reg) = self.env.registered_props.and_then(|m| m.get(key)) {
            let init_s = reg.initial_value.as_deref().map(serialize_css_tokens).unwrap_or_default();
            return compute_registered_str(&reg.syntax, val_text.trim(), self.env.eval)
                == compute_registered_str(&reg.syntax, init_s.trim(), self.env.eval);
        }
        val_text.is_empty()
    }

    fn eval_style_prop_eq(&mut self, owner: Option<&InternedStr>, prop_name: &str,
        target_prop: &str, rhs_raw: &str, depth: u8, budget: &mut usize,
    ) -> (Option<bool>, bool, EvalStatus) {
        let key = rix_core::intern_str(target_prop);
        let mut actual_out = SubstOutput::default();
        let actual_st = self.eval_var_lookup(owner, &key, depth + 1, &mut actual_out, budget);
        if matches!(actual_st, EvalStatus::PropCycle(_) | EvalStatus::AttrCycle(_)
        ) { return (None, actual_out.is_tainted(), actual_st); }
        let mut rhs_out = SubstOutput::default();
        let rhs_st = self.eval_str(owner, prop_name, rhs_raw, depth + 1, &mut rhs_out, budget);
        let tainted = actual_out.is_tainted() || rhs_out.is_tainted();
        if matches!(rhs_st, EvalStatus::PropCycle(_) | EvalStatus::AttrCycle(_)) { return (None, tainted, rhs_st); }
        if rhs_st != EvalStatus::Valid { return (Some(false), tainted, EvalStatus::Valid); }
        let rhs_t = rhs_out.text.trim();
        let reg = self.env.registered_props.and_then(|m| m.get(&key));
        let actual_opt = (actual_st == EvalStatus::Valid).then_some(actual_out.text.as_str());

        if let Some(kw) = GlobalKeyword::from_ident(rhs_t) {
            let init_str = || reg.and_then(|r| r.initial_value.as_deref().map(serialize_css_tokens));
            let parent_str = || { self.env.parent_style.and_then(|p| p.custom_properties.get(&key)).map(serialize_css_tokens) };
            let matched = match kw {
                GlobalKeyword::Revert | GlobalKeyword::RevertLayer | GlobalKeyword::RevertRule => { false }
                GlobalKeyword::Initial => match (reg, actual_opt) {
                    (None, None) | (Some(_), None) => true, (None, Some(_)) => false,
                    (Some(_), Some(act)) => self.custom_prop_matches_initial(&key, act),
                },
                GlobalKeyword::Inherit | GlobalKeyword::Unset => {
                    let expected = if kw == GlobalKeyword::Inherit || reg.is_none_or(|r| r.inherits)
                    {
                        parent_str().or_else(init_str)
                    } else { init_str() };
                    match (actual_opt, expected.as_deref()) {
                        (None, None) => true, (Some(a), Some(e)) => compare_prop_values(reg, a, e, self.env.eval),
                        _ => false,
                    }
                }
            };
            return (Some(matched), tainted, EvalStatus::Valid);
        }

        let matched = actual_opt.is_some_and(|a| compare_prop_values(reg, a, rhs_t, self.env.eval));
        (Some(matched), tainted, EvalStatus::Valid)
    }

    fn eval_style_range(&mut self, owner: Option<&InternedStr>, prop_name: &str,
        parts: &StyleRange<'_>, depth: u8, budget: &mut usize,
    ) -> (Option<bool>, bool, EvalStatus) {
        let (mut tainted, mut vals) = (false, Vec::with_capacity(parts.operands.len()));
        for &op_raw in &parts.operands {
            let ot = op_raw.trim();
            if GlobalKeyword::from_ident(ot).is_some() { return (Some(false), tainted, EvalStatus::Valid); }
            let mut vout = SubstOutput::default();
            let st = if is_valid_custom_prop_ident(ot) {
                let key = rix_core::intern_str(ot);
                self.eval_var_lookup(owner, &key, depth + 1, &mut vout, budget)
            } else { self.eval_str(owner, prop_name, ot, depth + 1, &mut vout, budget) };
            tainted |= vout.is_tainted();
            if matches!(st, EvalStatus::PropCycle(_) | EvalStatus::AttrCycle(_)) { return (None, tainted, st); }
            if st != EvalStatus::Valid { return (Some(false), tainted, EvalStatus::Valid); }
            let Some(rv) = eval_range_operand(vout.text.trim(), prop_name, self.env.eval, &mut self.cond_random_counter,
            ) else { return (Some(false), tainted, EvalStatus::Valid); };
            vals.push(rv);
        }
        let ok = match parts.ops.len() {
            1 => compare_range_pair(vals[0], parts.ops[0], vals[1]),
            2 => { compare_range_pair(vals[0], parts.ops[0], vals[1]) && compare_range_pair(vals[1], parts.ops[1], vals[2]) }
            _ => false,
        };
        (Some(ok), tainted, EvalStatus::Valid)
    }

    fn eval_random_item(&mut self, owner: Option<&InternedStr>, prop_name: &str,
        inner: &str, depth: u8, out: &mut SubstOutput, budget: &mut usize,
    ) -> EvalStatus {
        let parts = split_top_level(inner, b',');
        if parts.len() < 2 { return EvalStatus::Invalid; }
        let mut key_out = SubstOutput::default();
        let key_st = self.eval_str(owner, prop_name, parts[0], depth + 1, &mut key_out, budget);
        if key_st != EvalStatus::Valid { return key_st; }
        let Some(rv) = eval_random_item_key(key_out.text.trim(), prop_name, self.env.eval, &mut self.random_item_counter,
        ) else { return EvalStatus::Invalid; };
        if rv.is_nan() { return EvalStatus::Invalid; }
        let items = &parts[1..];
        let idx = ((rv * items.len() as f64).floor() as isize).clamp(0, (items.len() - 1) as isize)
            as usize;
        let raw_sel = items[idx].trim();
        let unwrapped = strip_outer_pair(raw_sel, b'{', b'}').map_or(raw_sel, str::trim);
        if unwrapped.is_empty() { return EvalStatus::Invalid; }
        let mut sel_out = SubstOutput::default();
        let st = self.eval_str(owner, prop_name, unwrapped, depth + 1, &mut sel_out, budget);
        if st != EvalStatus::Valid { return st; }
        if key_out.is_tainted() { sel_out.mark_all_tainted(); }
        out.append_sub(&sel_out, false);
        EvalStatus::Valid
    }

    fn eval_ident(&mut self, owner: Option<&InternedStr>, prop_name: &str,
        inner: &str, depth: u8, out: &mut SubstOutput, budget: &mut usize,
    ) -> EvalStatus {
        let mut arg_out = SubstOutput::default();
        let st = self.eval_str(owner, prop_name, inner, depth + 1, &mut arg_out, budget);
        if st != EvalStatus::Valid { return st; }
        let Some(built) = build_ident_from_args(arg_out.text.trim(), prop_name, self.env.eval)
        else { return EvalStatus::Invalid; };
        if arg_out.is_tainted() {
            out.push_tainted(&built);
        } else { out.push_plain(&built); }
        EvalStatus::Valid
    }
}

// ---------------------------------------------------------------------------
// Helpers: `attr()`, `if()` style queries, `random-item()`, `ident()`
// ---------------------------------------------------------------------------

enum AttrType {
    RawString,
    Number,
    Unit(&'static str),
    Type(String),
}

#[rustfmt::skip]
fn parse_attr_head(head: &str) -> Option<(String, AttrType)> {
    let mut pi = ParserInput::new(head);
    let mut p = Parser::new(&mut pi);
    let attr_name = p.expect_ident().ok()?.as_ref().to_string();
    if p.is_exhausted() { return Some((attr_name, AttrType::RawString)); }
    let ty = match p.next().ok()?.clone() {
        Token::Ident(id) if p.is_exhausted() => match id.to_ascii_lowercase().as_str() {
            "raw-string" | "string" => AttrType::RawString, "number" => AttrType::Number,
            other => AttrType::Unit(canonical_attr_unit(other)?),
        }, Token::Delim('%') if p.is_exhausted() => AttrType::Unit("%"),
        Token::Function(fname) if fname.eq_ignore_ascii_case("type") => {
            let syn = p.parse_nested_block(|n| {
                    let start = n.position();
                    while n.next().is_ok() {}
                    Ok::<_, cssparser::ParseError<'_, ()>>(n.slice_from(start).trim().to_string())
                }).ok()?;
            if !p.is_exhausted() || syn.is_empty() || !is_valid_attr_syntax_grammar(&syn) { return None; }
            AttrType::Type(syn)
        }
        _ => return None,
    };
    Some((attr_name, ty))
}

#[rustfmt::skip]
fn canonical_attr_unit(u: &str) -> Option<&'static str> {
    match u {
        "px" => Some("px"), "em" => Some("em"), "ex" => Some("ex"), "cap" => Some("cap"),
        "ch" => Some("ch"), "ic" => Some("ic"), "rem" => Some("rem"), "lh" => Some("lh"),
        "rlh" => Some("rlh"), "vw" => Some("vw"), "vh" => Some("vh"), "vi" => Some("vi"),
        "vb" => Some("vb"), "vmin" => Some("vmin"), "vmax" => Some("vmax"),
        "cm" => Some("cm"), "mm" => Some("mm"), "q" => Some("q"), "in" => Some("in"), "pc" => Some("pc"), "pt" => Some("pt"),
        "deg" => Some("deg"), "grad" => Some("grad"), "rad" => Some("rad"), "turn" => Some("turn"),
        "s" => Some("s"), "ms" => Some("ms"), "hz" => Some("Hz"), "khz" => Some("kHz"),
        "dpi" => Some("dpi"), "dpcm" => Some("dpcm"), "dppx" => Some("dppx"), "x" => Some("x"),
        "fr" => Some("fr"), "cqw" => Some("cqw"), "cqh" => Some("cqh"),
        "cqi" => Some("cqi"), "cqb" => Some("cqb"), "cqmin" => Some("cqmin"), "cqmax" => Some("cqmax"), _ => None,
    }
}

#[rustfmt::skip]
fn is_valid_attr_syntax_grammar(syn: &str) -> bool {
    syn == "*" || (!has_invalid_angle_syntax_type(syn) && syn.split('|').all(|a| !a.trim().is_empty()))
}

#[rustfmt::skip]
fn parse_attr_number(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() { return None; }
    let mut pi = ParserInput::new(t);
    let mut p = Parser::new(&mut pi);
    let start = p.position();
    let Token::Number { value, .. } = p.next().ok()?.clone() else { return None; };
    if !p.is_exhausted() { return None; }
    let exact = p.slice_from(start).trim().parse::<f64>().unwrap_or(f64::from(value));
    exact.is_finite().then_some(exact)
}

#[rustfmt::skip]
fn validate_and_eval_typed_attr(syntax: &str, val: &str, prop_name: &str,
    eval: &EvalContext, early_rand_counter: &mut u32, compute_units: bool,
) -> Option<String> {
    let syn = syntax.trim();
    if syn == "*" { return Some(val.to_string()); }
    if val.is_empty() || GlobalKeyword::from_ident(val).is_some() { return None; }
    let early_prop = format!("early-{prop_name}");
    for alt in syn.split('|').map(str::trim) {
        let is_list = alt.ends_with('+') || alt.ends_with('#');
        let comma = alt.ends_with('#');
        let base = alt.trim_end_matches(['+', '#']).trim();
        if let Some(vk) = syntax_to_value_kind(base) {
            if !is_list {
                if let Some(mut pm) = parse_math_value(val, vk) {
                    if compute_units || pm.node.contains_random() {
                        pm.node.bind_random_property(&early_prop, early_rand_counter);
                        return Some(pm.node.serialize_computed(eval, None, vk, None, None));
                    }
                    return Some(pm.node.serialize_specified(pm.was_math_fn));
                }
            } else if let Some(list_s) = eval_math_list(val, vk, comma, &early_prop, eval, early_rand_counter)
            { return Some(list_s); }
            continue;
        }
        match base {
            "<color>" if !is_list => {
                if let Some(mut c) = crate::color::parse_css_color(val) {
                    c.bind_random_property(&early_prop);
                    return Some(c.resolve( crate::color::ResolvedColor::from_rgba(ColorRgba::BLACK), false, eval)
                        .to_computed_css_string(), );
                }
            }
            "<string>" if validate_string_tokens(val, is_list, comma) => { return Some(val.to_string()); }
            "<custom-ident>" | "<ident>" if !is_list => {
                let mut pi = ParserInput::new(val);
                let mut p = Parser::new(&mut pi);
                if let Ok(id) = p.expect_ident() {
                    let s = id.as_ref().to_string();
                    if p.is_exhausted() && GlobalKeyword::from_ident(&s).is_none() { return Some(escape_css_ident(&s)); }
                }
            }
            "<url>" if !is_list => {
                let l = val.to_ascii_lowercase();
                if (l.starts_with("url(") || l.starts_with("src(")) && val.ends_with(')') { return Some(val.to_string()); }
            }
            "<image>" if !is_list => {
                let l = val.to_ascii_lowercase();
                if l.starts_with("url(") || l.starts_with("src(")
                    || l.starts_with("image-set(") || l.starts_with("linear-gradient(")
                    || l.starts_with("radial-gradient(") || l.starts_with("conic-gradient(")
                { return Some(val.to_string()); }
            }
            "<frequency>" if !is_list => {
                if let Some(pm) = parse_math_value(val, ValueKind::Frequency) {
                    return Some(pm.node.serialize_computed(eval, None, ValueKind::Frequency, None, None));
                }
            }
            kw if !kw.starts_with('<') && val.eq_ignore_ascii_case(kw) => { return Some(kw.to_string()); }
            _ => {}
        }
    }
    None
}

#[rustfmt::skip]
fn syntax_to_value_kind(base: &str) -> Option<ValueKind> {
    match base {
        "<number>" => Some(ValueKind::Number), "<integer>" => Some(ValueKind::Integer),
        "<length>" => Some(ValueKind::Length), "<percentage>" => Some(ValueKind::Percentage),
        "<length-percentage>" => Some(ValueKind::LengthPercentage),
        "<angle>" => Some(ValueKind::Angle), "<time>" => Some(ValueKind::Time),
        "<resolution>" => Some(ValueKind::Resolution), _ => None,
    }
}

#[rustfmt::skip]
fn eval_math_list(val: &str, vk: ValueKind, comma: bool, prop_name: &str, eval: &EvalContext, counter: &mut u32,
) -> Option<String> {
    let mut pi = ParserInput::new(val);
    let mut p = Parser::new(&mut pi);
    let mut parts = Vec::new();
    while !p.is_exhausted() {
        let mut pm = parse_math_value_from_parser(&mut p, vk)?;
        pm.node.bind_random_property(prop_name, counter);
        parts.push(pm.node.serialize_computed(eval, None, vk, None, None));
        if comma && !p.is_exhausted() && p.expect_comma().is_err() { return None; }
    }
    (!parts.is_empty()).then(|| parts.join(if comma { ", " } else { " " }))
}

#[rustfmt::skip]
fn validate_string_tokens(val: &str, is_list: bool, comma: bool) -> bool {
    let mut pi = ParserInput::new(val);
    let mut p = Parser::new(&mut pi);
    let mut count = 0usize;
    while !p.is_exhausted() {
        if p.expect_string().is_err() { return false; }
        count += 1;
        if !is_list { return p.is_exhausted(); }
        if comma && !p.is_exhausted() && p.expect_comma().is_err() { return false; }
    }
    count > 0
}

#[rustfmt::skip]
fn compute_registered_str(syntax: &str, val: &str, eval: &EvalContext) -> Option<String> {
    let syn = syntax.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    if syn.is_empty() || syn == "*" { return Some(val.to_string()); }
    let mut c = 0u32;
    validate_and_eval_typed_attr(syn, val, "", eval, &mut c, true)
}

#[rustfmt::skip]
fn compare_prop_values(reg: Option<&RegisteredProperty>, actual: &str, expected: &str, eval: &EvalContext) -> bool {
    if let Some(r) = reg {
        let syn = r.syntax.trim().trim_matches(|c| c == '"' || c == '\'').trim();
        if syn != "*" {
            let ca = compute_registered_str(syn, actual.trim(), eval);
            let ce = compute_registered_str(syn, expected.trim(), eval);
            return ca.is_some() && ca == ce;
        }
    }
    actual.trim() == expected.trim()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoolOp {
    And,
    Or,
}

#[rustfmt::skip]
fn kleene_and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) { (Some(false), _) | (_, Some(false)) => Some(false), (Some(true), Some(true)) => Some(true), _ => None, }
}

#[rustfmt::skip]
fn kleene_or(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) { (Some(true), _) | (_, Some(true)) => Some(true), (Some(false), Some(false)) => Some(false), _ => None, }
}

#[rustfmt::skip]
fn split_bool_combinators(s: &str) -> (Vec<BoolOp>, Vec<&str>) {
    let bytes = s.as_bytes();
    let (mut ops, mut parts, mut start, mut depth, mut i) = (Vec::new(), Vec::new(), 0usize, 0usize, 0usize);
    while i < bytes.len() {
        if let Some(next) = skip_css_trivia(bytes, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ if depth == 0 && i > 0 && bytes[i - 1].is_ascii_whitespace() => {
                if let Some((op, len)) = [
                    (BoolOp::And, b"and".as_slice()), (BoolOp::Or, b"or".as_slice()), ].into_iter()
                .find(|(_, kw)| {
                    i + kw.len() < bytes.len() && bytes[i..i + kw.len()].eq_ignore_ascii_case(kw)
                        && (bytes[i + kw.len()].is_ascii_whitespace() || bytes[i + kw.len()] == b'(')
                }).map(|(op, kw)| (op, kw.len()))
                {
                    parts.push(s[start..i].trim());
                    ops.push(op);
                    start = i + len;
                    i += len;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if !ops.is_empty() { parts.push(s[start..].trim()); }
    (ops, parts)
}

#[rustfmt::skip]
fn strip_leading_kw_ci<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let t = s.trim_start();
    let n = kw.len();
    (t.len() > n && t.as_bytes()[..n].eq_ignore_ascii_case(kw.as_bytes())
        && (t.as_bytes()[n].is_ascii_whitespace() || t.as_bytes()[n] == b'(')).then(|| t[n..].trim_start())
}

#[rustfmt::skip]
fn starts_with_if_cond_kw(t: &str) -> bool {
    let b = t.as_bytes();
    b.starts_with(b"(") || match_fn_at(b, 0, b"style") || match_fn_at(b, 0, b"media") || match_fn_at(b, 0, b"supports")
        || strip_leading_kw_ci(t, "not").is_some() || t.eq_ignore_ascii_case("else")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeCmp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

struct StyleRange<'a> {
    operands: Vec<&'a str>,
    ops: Vec<RangeCmp>,
}

#[rustfmt::skip]
fn split_style_range(s: &str) -> Option<StyleRange<'_>> {
    let bytes = s.as_bytes();
    let (mut operands, mut ops, mut start, mut depth, mut i) = (Vec::new(), Vec::new(), 0usize, 0usize, 0usize);
    while i < bytes.len() {
        if let Some(next) = skip_css_trivia(bytes, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b'<' | b'>' | b'=' if depth == 0 => {
                let (op, len) = match (bytes[i], bytes.get(i + 1).copied()) {
                    (b'<', Some(b'=')) => (RangeCmp::Le, 2), (b'<', _) => (RangeCmp::Lt, 1),
                    (b'>', Some(b'=')) => (RangeCmp::Ge, 2), (b'>', _) => (RangeCmp::Gt, 1), _ => (RangeCmp::Eq, 1),
                };
                let lhs = s[start..i].trim();
                if lhs.is_empty() { return None; }
                operands.push(lhs);
                ops.push(op);
                start = i + len;
                i += len;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    let last = s[start..].trim();
    if ops.is_empty() || ops.len() > 2 || last.is_empty() { return None; }
    operands.push(last);
    if ops.len() == 2 {
        let same_dir = (matches!(ops[0], RangeCmp::Lt | RangeCmp::Le)
            && matches!(ops[1], RangeCmp::Lt | RangeCmp::Le)) || (matches!(ops[0], RangeCmp::Gt | RangeCmp::Ge)
                && matches!(ops[1], RangeCmp::Gt | RangeCmp::Ge));
        if !same_dir { return None; }
    }
    Some(StyleRange { operands, ops })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeUnit {
    Number,
    LengthPx,
    Percent,
    AngleDeg,
    TimeSec,
    FreqHz,
    ResDppx,
}

#[rustfmt::skip]
fn eval_range_operand(s: &str, prop_name: &str, eval: &EvalContext, rand_counter: &mut u32) -> Option<(f64, RangeUnit)> {
    for (vk, unit) in [
        (ValueKind::Number, RangeUnit::Number), (ValueKind::Length, RangeUnit::LengthPx),
        (ValueKind::Percentage, RangeUnit::Percent), (ValueKind::Angle, RangeUnit::AngleDeg),
        (ValueKind::Time, RangeUnit::TimeSec), (ValueKind::Frequency, RangeUnit::FreqHz),
        (ValueKind::Resolution, RangeUnit::ResDppx),
    ] {
        if let Some(mut pm) = parse_math_value(s, vk) {
            if (vk == ValueKind::Number && !pm.calc_type.is_number()) || (vk == ValueKind::Length
                    && pm.calc_type.single_base_dim() != Some(BaseDim::Length))
            { continue; }
            pm.node.bind_random_property(prop_name, rand_counter);
            let v = pm.node.eval_scalar(eval, None);
            if v.is_finite() { return Some((v, unit)); }
        }
    }
    None
}

#[rustfmt::skip]
fn compare_range_pair(a: (f64, RangeUnit), op: RangeCmp, b: (f64, RangeUnit)) -> bool {
    let compatible = a.1 == b.1 || (a.1 == RangeUnit::Number && a.0 == 0.0 && b.1 == RangeUnit::LengthPx)
        || (b.1 == RangeUnit::Number && b.0 == 0.0 && a.1 == RangeUnit::LengthPx);
    if !compatible { return false; }
    let eps = 1e-9;
    match op {
        RangeCmp::Lt => a.0 < b.0 - eps, RangeCmp::Le => a.0 <= b.0 + eps,
        RangeCmp::Gt => a.0 > b.0 + eps, RangeCmp::Ge => a.0 >= b.0 - eps, RangeCmp::Eq => (a.0 - b.0).abs() <= eps,
    }
}

#[rustfmt::skip]
fn eval_random_item_key(key: &str, prop_name: &str, eval: &EvalContext, counter: &mut u32) -> Option<f64> {
    let t = key.trim();
    if t.is_empty() { return None; }
    let mut pi = ParserInput::new(t);
    let mut p = Parser::new(&mut pi);
    if p.try_parse(|p| p.expect_ident_matching("fixed")).is_ok() {
        let pm = parse_math_value_from_parser(&mut p, ValueKind::Number)?;
        if !p.is_exhausted() { return None; }
        let v = pm.node.eval_scalar(eval, None);
        if !v.is_finite() || (!pm.was_math_fn && !(0.0..=1.0).contains(&v)) { return None; }
        return Some(v.clamp(0.0, 1.0 - 1e-9));
    }
    if p.try_parse(|p| p.expect_ident_matching("auto")).is_ok() && p.is_exhausted() {
        *counter += 1;
        let ua = format!("ua-{prop_name}-item-{counter}");
        return Some(hash_random_f64(eval.random_doc_seed, eval.random_node_id, None, Some(&ua)));
    }
    let (mut dashed, mut elem_scoped, mut prop_scoped, mut prop_idx_scoped, mut ua_ident) =
        (None::<String>, false, false, false, None::<String>);
    let mut count = 0usize;
    while !p.is_exhausted() {
        let id = p.expect_ident().ok()?.as_ref().to_string();
        let lower = id.to_ascii_lowercase();
        let has_prop_scope = prop_scoped || prop_idx_scoped || ua_ident.is_some();
        if id.starts_with("--") && dashed.is_none() {
            dashed = Some(id);
        } else if lower == "element-scoped" && !elem_scoped {
            elem_scoped = true;
        } else if lower == "property-scoped" && !has_prop_scope {
            prop_scoped = true;
        } else if lower == "property-index-scoped" && !has_prop_scope {
            prop_idx_scoped = true;
        } else if id.starts_with("ua-") && id.len() > 3 && !has_prop_scope {
            ua_ident = Some(id);
        } else { return None; }
        count += 1;
    }
    if count == 0 { return None; }
    if ua_ident.is_none() {
        if prop_idx_scoped || (!prop_scoped && dashed.is_none()) {
            *counter += 1;
            ua_ident = Some(format!("ua-{prop_name}-item-{counter}"));
        } else if prop_scoped {
            ua_ident = Some(format!("ua-{prop_name}-item"));
        }
    }
    let nid = if elem_scoped { eval.random_node_id } else { 0 };
    Some(hash_random_f64(eval.random_doc_seed, nid, dashed.as_deref(), ua_ident.as_deref()))
}

#[rustfmt::skip]
fn build_ident_from_args(args_str: &str, prop_name: &str, eval: &EvalContext) -> Option<String> {
    if args_str.is_empty() { return None; }
    let mut pi = ParserInput::new(args_str);
    let mut p = Parser::new(&mut pi);
    let (mut raw, mut rc, mut count) = (String::new(), 0u32, 0usize);
    while !p.is_exhausted() {
        let st = p.state();
        match p.next().ok()?.clone() {
            Token::QuotedString(s) => raw.push_str(s.as_ref()), Token::Ident(id) => raw.push_str(id.as_ref()),
            Token::Number {
                int_value: Some(n), ..
            } => {
                let _ = write!(raw, "{n}");
            }
            Token::Function(_) => {
                p.reset(&st);
                let mut pm = parse_math_value_from_parser(&mut p, ValueKind::Integer)?;
                pm.node.bind_random_property(prop_name, &mut rc);
                let v = pm.node.eval_scalar(eval, None);
                if !v.is_finite() { return None; }
                let _ = write!(raw, "{}", (v + 0.5).floor() as i64);
            }
            _ => return None,
        }
        count += 1;
    }
    (count > 0 && !raw.is_empty()).then(|| escape_css_ident(&raw))
}

// ---------------------------------------------------------------------------
// Public Entry Points Called by `computed.rs` & `cssom.rs`
// ---------------------------------------------------------------------------

/// Resolves all custom properties declared on an element using the unified CSS Values L5
/// substitution engine with Dynamic Tarjan SCC cycle detection and taint propagation.
#[rustfmt::skip]
pub fn resolve_custom_properties_l5(custom_map: &CustomPropertyMap, declared_keys: &[InternedStr], env: &SubstEnv<'_>,
) -> HashMap<InternedStr, Option<SubstOutput>> {
    let raw_map: HashMap<InternedStr, String> = custom_map.entries
        .iter().map(|(k, toks)| (k.clone(), serialize_css_tokens(toks))).collect();
    let mut engine = SubstEngine::new(&raw_map, &custom_map.tainted_ranges, env);
    declared_keys.iter().map(|key| (key.clone(), engine.resolve_prop(key, 0))).collect()
}

/// Substitutes all CSS Values L5 arbitrary substitution functions in a standard property
/// declaration's token stream against the element's finalized custom properties and live attributes.
#[must_use]
#[rustfmt::skip]
pub fn substitute_standard_property_l5(tokens: &[CssToken], prop: PropertyIdKind, custom_props: &CustomPropertyMap,
    env: &SubstEnv<'_>,
) -> Option<SubstOutput> {
    let raw_src = serialize_css_tokens(tokens);
    let raw_map: HashMap<InternedStr, String> = custom_props.entries
        .iter().map(|(k, toks)| (k.clone(), serialize_css_tokens(toks))).collect();
    let mut engine = SubstEngine::new(&raw_map, &custom_props.tainted_ranges, env);
    let (mut out, mut budget) = (SubstOutput::default(), MAX_SUBST_BYTES);
    let st = engine.eval_str(None, prop.css_name(), &raw_src, 0, &mut out, &mut budget);
    (st == EvalStatus::Valid && !is_tainted_url_violation(prop, &out)).then_some(out)
}

#[rustfmt::skip]
fn parse_bg_url_token(p: &mut Parser<'_, '_>, eval: &EvalContext, allow_gradient: bool) -> Option<String> {
    match p.next().ok()?.clone() {
        Token::QuotedString(s) => Some(format!("url({})", quote_css_string(s.as_ref()))),
        Token::UnquotedUrl(u) => Some(format!("url({})", quote_css_string(u.as_ref()))),
        Token::Function(f) if f.eq_ignore_ascii_case("url") || f.eq_ignore_ascii_case("src") => {
            let s = p.parse_nested_block(|n| {
                    Ok::<_, cssparser::ParseError<'_, ()>>(n.expect_string()?.as_ref().to_string())
                }).ok()?;
            Some(format!("url({})", quote_css_string(&s)))
        }
        Token::Function(f) if allow_gradient && f.eq_ignore_ascii_case("linear-gradient") => {
            let args_raw = p.parse_nested_block(|n| {
                    let st = n.position();
                    while n.next().is_ok() {}
                    Ok::<_, cssparser::ParseError<'_, ()>>(n.slice_from(st).trim().to_string())
                }).ok()?;
            let gparts: Vec<String> = split_top_level(&args_raw, b',').into_iter()
                .map(|gp| {
                    let gt = gp.trim();
                    crate::color::parse_css_color(gt).map_or_else( || gt.to_string(),
                        |c| {
                            c.resolve(crate::color::ResolvedColor::from_rgba(ColorRgba::BLACK), false, eval)
                            .to_computed_css_string()
                        }, )
                }).collect();
            Some(format!("linear-gradient({})", gparts.join(", ")))
        }
        _ => None,
    }
}

/// Evaluates a CSS `background-image` `image-set(...)`, `-webkit-image-set(...)`, `src(...)`, or
/// `image(...)` declaration into canonical CSSOM computed form.
#[must_use]
#[rustfmt::skip]
pub fn parse_and_serialize_bg_image_fn(raw: &str, eval: &EvalContext) -> Option<String> {
    let t = raw.trim();
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("src(") && t.ends_with(')') {
        let mut pi = ParserInput::new(t[4..t.len() - 1].trim());
        let mut p = Parser::new(&mut pi);
        let s = p.expect_string().ok()?.as_ref().to_string();
        return p.is_exhausted()
            .then(|| format!("url({})", quote_css_string(&s)));
    }
    if lower.starts_with("image(") && t.ends_with(')') {
        let mut pi = ParserInput::new(t[6..t.len() - 1].trim());
        let mut p = Parser::new(&mut pi);
        let img = parse_bg_url_token(&mut p, eval, false)?;
        return p.is_exhausted().then(|| format!("image({img})"));
    }
    let inner = if lower.starts_with("image-set(") && t.ends_with(')') {
        &t[10..t.len() - 1]
    } else if lower.starts_with("-webkit-image-set(") && t.ends_with(')') {
        &t[18..t.len() - 1]
    } else { return None; };
    let mut out_items = Vec::new();
    for part in split_top_level(inner.trim(), b',') {
        let pt = part.trim();
        if pt.is_empty() { return None; }
        let mut pi = ParserInput::new(pt);
        let mut p = Parser::new(&mut pi);
        let img = parse_bg_url_token(&mut p, eval, true)?;
        let (mut res_str, mut type_str) = ("1dppx".to_string(), None::<String>);
        while !p.is_exhausted() {
            let st = p.state();
            if let Ok(Token::Function(ref f)) = p.next().cloned() {
                if f.eq_ignore_ascii_case("type") {
                    let s = p.parse_nested_block(|n| {
                            Ok::<_, cssparser::ParseError<'_, ()>>(n.expect_string()?.as_ref().to_string())
                        }).ok()?;
                    type_str = Some(format!("type({})", quote_css_string(&s)));
                    continue;
                }
            }
            p.reset(&st);
            let pm = parse_math_value_from_parser(&mut p, ValueKind::Resolution)?;
            res_str = pm.node.serialize_computed(eval, None, ValueKind::Resolution, Some(0.0), None);
        }
        out_items.push(match type_str {
            Some(ts) => format!("{img} {res_str} {ts}"),
            None => format!("{img} {res_str}"),
        });
    }
    (!out_items.is_empty()).then(|| format!("image-set({})", out_items.join(", ")))
}

/// Tokenizes a substituted CSS value string back into `Vec<CssToken>` for storage/re-parsing.
#[must_use]
#[rustfmt::skip]
pub fn subst_output_to_tokens(out: &SubstOutput) -> Vec<CssToken> { tokenize_custom_value_str(&out.text) }
