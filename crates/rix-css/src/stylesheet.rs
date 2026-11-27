//! Typed CSS stylesheet compiler supporting `@media`, `@supports`, `@layer`,
//! `@font-face`, `@keyframes`, and CSS Nesting flattening.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock};

use lightningcss::media_query::{
    MediaCondition, MediaFeatureId, MediaFeatureName, MediaFeatureValue, MediaList, MediaQuery,
    MediaType, Operator, Qualifier, QueryFeature,
};
use lightningcss::printer::Printer;
use lightningcss::rules::{
    font_face::{FontFaceProperty, FontFaceRule as LightningFontFace, Source},
    keyframes::{KeyframeSelector, KeyframesName, KeyframesRule as LightningKeyframes},
    layer::LayerName,
    supports::SupportsCondition,
    CssRule, CssRuleList,
};
use lightningcss::stylesheet::{ParserOptions, PrinterOptions, StyleSheet};
use lightningcss::traits::ToCss as LightningToCss;
use rix_core::{intern_str, InternedStr};
use selectors::context::QuirksMode;
use selectors::parser::{AncestorHashes, ParseRelative, Selector, SelectorList};

use crate::properties::{
    compile_declaration_block, restore_revert_rule_sentinel, MissingCssFeature, MissingCssKind,
    PropertyValue, SpecifiedDeclaration,
};
use crate::selector::{
    compile_lightning_selector_list_with_namespaces, parse_selector_list_non_forgiving,
    parse_selector_list_relative, parse_selector_list_strict, parse_selector_list_with_namespaces,
    serialize_selector_list, CssAtom, RixSelectorImpl, SelectorNamespaceMap,
};
use crate::values::{convert_font_family_name, convert_length, BackgroundImageValue, EvalContext};

/// CSS Cascade origin priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum CascadeOrigin {
    UserAgent = 0,
    User = 1,
    Author = 2,
}

/// Registered `@layer` ordering handle within a stylesheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LayerId(pub u32);

impl LayerId {
    /// Sentinel layer assigned to unlayered rules (highest normal priority, lowest `!important`).
    pub const UNLAYERED: Self = Self(u32::MAX);

    #[must_use]
    pub const fn is_unlayered(self) -> bool {
        self.0 == u32::MAX
    }
}

/// Media type for evaluating `@media` queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MediaTypeKind {
    #[default]
    Screen,
    Print,
    All,
}

/// Viewport and user-preference environment for `@media` and unit resolution.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MediaContext {
    pub viewport_width: f32,
    pub viewport_height: f32,
    pub dpi: f32,
    pub dark_mode: bool,
    pub reduced_motion: bool,
    pub media_type: MediaTypeKind,
    /// Optional nearest CSS Containment L3 size-query container dimensions (for `cqw`/`cqh`/`cqi`/`cqb`).
    pub container_width: Option<f32>,
    pub container_height: Option<f32>,
    /// True when the owning document is in HTML Quirks Mode (`doc.quirks_mode() == 2`),
    /// enabling Quirks Mode Living Standard §3.1 (hashless hex colors) and §3.2 (unitless lengths).
    pub quirks_mode: bool,
}

impl Default for MediaContext {
    fn default() -> Self {
        Self {
            viewport_width: 1280.0,
            viewport_height: 720.0,
            dpi: 96.0,
            dark_mode: false,
            reduced_motion: false,
            media_type: MediaTypeKind::Screen,
            container_width: None,
            container_height: None,
            quirks_mode: false,
        }
    }
}

impl MediaContext {
    #[must_use]
    pub fn eval_context(&self, font_size_px: f32, root_font_size_px: f32) -> EvalContext {
        EvalContext {
            font_size_px,
            root_font_size_px,
            viewport_width_px: self.viewport_width,
            viewport_height_px: self.viewport_height,
            container_width_px: self.container_width.unwrap_or(self.viewport_width),
            container_height_px: self.container_height.unwrap_or(self.viewport_height),
            ..EvalContext::default()
        }
    }

    #[must_use]
    pub fn matches_media_list(&self, list: &MediaList<'_>) -> bool {
        if list.media_queries.is_empty() {
            return true;
        }
        list.media_queries
            .iter()
            .any(|q| self.matches_media_query(q))
    }

    fn matches_media_query(&self, query: &MediaQuery<'_>) -> bool {
        let type_matches = match &query.media_type {
            MediaType::All => true,
            MediaType::Screen => {
                matches!(self.media_type, MediaTypeKind::Screen | MediaTypeKind::All)
            }
            MediaType::Print => {
                matches!(self.media_type, MediaTypeKind::Print | MediaTypeKind::All)
            }
            MediaType::Custom(_) => false,
        };
        let cond_matches = query
            .condition
            .as_ref()
            .is_none_or(|c| self.matches_media_condition(c));
        let raw = type_matches && cond_matches;
        match query.qualifier {
            Some(Qualifier::Not) => !raw,
            Some(Qualifier::Only) | None => raw,
        }
    }

    fn matches_media_condition(&self, cond: &MediaCondition<'_>) -> bool {
        match cond {
            MediaCondition::Feature(feat) => self.matches_media_feature(feat),
            MediaCondition::Not(inner) => !self.matches_media_condition(inner),
            MediaCondition::Operation {
                operator,
                conditions,
            } => match operator {
                Operator::And => conditions.iter().all(|c| self.matches_media_condition(c)),
                Operator::Or => conditions.iter().any(|c| self.matches_media_condition(c)),
            },
            MediaCondition::Unknown(_) => false,
        }
    }

    fn matches_media_feature(&self, feat: &QueryFeature<'_, MediaFeatureId>) -> bool {
        match feat {
            QueryFeature::Boolean { name } => self.eval_boolean_feature(name),
            QueryFeature::Plain { name, value } => self.eval_plain_feature(name, value),
            QueryFeature::Range {
                name,
                operator,
                value,
            } => {
                let Some(actual) = self.numeric_feature_value(name) else {
                    return false;
                };
                let Some(expected) = self.feature_value_to_f32(value) else {
                    return false;
                };
                compare_f32(actual, *operator, expected)
            }
            QueryFeature::Interval {
                name,
                start,
                start_operator,
                end,
                end_operator,
            } => {
                let Some(actual) = self.numeric_feature_value(name) else {
                    return false;
                };
                let (Some(s), Some(e)) = (
                    self.feature_value_to_f32(start),
                    self.feature_value_to_f32(end),
                ) else {
                    return false;
                };
                compare_f32(s, *start_operator, actual) && compare_f32(actual, *end_operator, e)
            }
        }
    }

    fn eval_boolean_feature(&self, name: &MediaFeatureName<'_, MediaFeatureId>) -> bool {
        match name {
            MediaFeatureName::Standard(id) => matches!(
                id,
                MediaFeatureId::Color
                    | MediaFeatureId::Hover
                    | MediaFeatureId::AnyHover
                    | MediaFeatureId::Pointer
                    | MediaFeatureId::AnyPointer
                    | MediaFeatureId::Width
                    | MediaFeatureId::Height
            ),
            _ => false,
        }
    }

    fn eval_plain_feature(
        &self,
        name: &MediaFeatureName<'_, MediaFeatureId>,
        value: &MediaFeatureValue<'_>,
    ) -> bool {
        match name {
            MediaFeatureName::Standard(MediaFeatureId::PrefersColorScheme) => {
                if let MediaFeatureValue::Ident(ident) = value {
                    let dark = ident.eq_ignore_ascii_case("dark");
                    let light = ident.eq_ignore_ascii_case("light");
                    return (dark && self.dark_mode) || (light && !self.dark_mode);
                }
                false
            }
            MediaFeatureName::Standard(MediaFeatureId::PrefersReducedMotion) => {
                if let MediaFeatureValue::Ident(ident) = value {
                    return (ident.eq_ignore_ascii_case("reduce") && self.reduced_motion)
                        || (ident.eq_ignore_ascii_case("no-preference") && !self.reduced_motion);
                }
                false
            }
            MediaFeatureName::Standard(MediaFeatureId::Orientation) => {
                if let MediaFeatureValue::Ident(ident) = value {
                    let portrait = self.viewport_height >= self.viewport_width;
                    return (ident.eq_ignore_ascii_case("portrait") && portrait)
                        || (ident.eq_ignore_ascii_case("landscape") && !portrait);
                }
                false
            }
            MediaFeatureName::Standard(MediaFeatureId::Hover | MediaFeatureId::AnyHover) => {
                matches!(value, MediaFeatureValue::Ident(id) if id.eq_ignore_ascii_case("hover"))
            }
            MediaFeatureName::Standard(MediaFeatureId::Pointer | MediaFeatureId::AnyPointer) => {
                matches!(value, MediaFeatureValue::Ident(id) if id.eq_ignore_ascii_case("fine"))
            }
            _ => {
                let Some(actual) = self.numeric_feature_value(name) else {
                    return false;
                };
                let Some(expected) = self.feature_value_to_f32(value) else {
                    return false;
                };
                (actual - expected).abs() < 0.01
            }
        }
    }

    fn numeric_feature_value(&self, name: &MediaFeatureName<'_, MediaFeatureId>) -> Option<f32> {
        match name {
            MediaFeatureName::Standard(id) => match id {
                MediaFeatureId::Width | MediaFeatureId::DeviceWidth => Some(self.viewport_width),
                MediaFeatureId::Height | MediaFeatureId::DeviceHeight => Some(self.viewport_height),
                MediaFeatureId::AspectRatio | MediaFeatureId::DeviceAspectRatio => {
                    Some(self.viewport_width / self.viewport_height.max(1.0))
                }
                MediaFeatureId::Resolution => Some(self.dpi),
                MediaFeatureId::WebKitDevicePixelRatio | MediaFeatureId::MozDevicePixelRatio => {
                    Some(self.dpi / 96.0)
                }
                MediaFeatureId::Color => Some(8.0),
                _ => None,
            },
            _ => None,
        }
    }

    fn feature_value_to_f32(&self, value: &MediaFeatureValue<'_>) -> Option<f32> {
        let eval = self.eval_context(16.0, 16.0);
        match value {
            MediaFeatureValue::Length(len) => Some(convert_length(len).resolve_px(0.0, &eval)),
            MediaFeatureValue::Number(n) => Some(*n),
            MediaFeatureValue::Integer(i) => Some(*i as f32),
            MediaFeatureValue::Ratio(r) => Some(r.0 / r.1.max(0.0001)),
            MediaFeatureValue::Resolution(res) => Some(match res {
                lightningcss::values::resolution::Resolution::Dpi(d) => *d,
                lightningcss::values::resolution::Resolution::Dpcm(d) => *d * 2.54,
                lightningcss::values::resolution::Resolution::Dppx(d) => *d * 96.0,
            }),
            _ => None,
        }
    }

    /// Evaluates a CSS media query string (e.g. `if(media(height <= 999999px): ...)` or
    /// `<link media="screen, print">`).
    #[must_use]
    pub fn evaluate_media_query_str(&self, query: &str) -> bool {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return false;
        }
        let lower = trimmed.to_ascii_lowercase();
        let first_word = lower
            .split(|c: char| c.is_ascii_whitespace() || c == ',' || c == '(')
            .next();
        // CSS Media Queries L4 §3: comma-separated `<media-query-list>` (`screen, print`) and
        // queries starting with a media type or `not`/`only`/`(` are already complete media
        // queries. Only wrap bare single `<media-feature>` strings (e.g. `height <= 999999px`).
        let needs_parens = !trimmed.starts_with('(')
            && !trimmed.contains(',')
            && !lower.starts_with("not ")
            && !lower.starts_with("not(")
            && !lower.starts_with("only ")
            && !matches!(first_word, Some("all" | "screen" | "print" | "speech"));
        let wrapped = if needs_parens {
            format!("@media ({trimmed}) {{ a {{ color: red; }} }}")
        } else {
            format!("@media {trimmed} {{ a {{ color: red; }} }}")
        };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let opts = ParserOptions::default();
            let Ok(parsed) = StyleSheet::parse(&wrapped, opts) else {
                return false;
            };
            for rule in &parsed.rules.0 {
                if let CssRule::Media(mr) = rule {
                    return self.matches_media_list(&mr.query);
                }
            }
            false
        }))
        .unwrap_or(false)
    }
}

fn compare_f32(lhs: f32, op: lightningcss::media_query::MediaFeatureComparison, rhs: f32) -> bool {
    use lightningcss::media_query::MediaFeatureComparison as Cmp;
    match op {
        Cmp::Equal => (lhs - rhs).abs() < 0.01,
        Cmp::GreaterThan => lhs > rhs,
        Cmp::GreaterThanEqual => lhs >= rhs,
        Cmp::LessThan => lhs < rhs,
        Cmp::LessThanEqual => lhs <= rhs,
    }
}

/// Evaluates an `@supports` condition against RixBrowser's supported properties & selectors.
#[must_use]
pub fn evaluate_supports(cond: &SupportsCondition<'_>) -> bool {
    match cond {
        SupportsCondition::Not(inner) => !evaluate_supports(inner),
        SupportsCondition::And(list) => list.iter().all(evaluate_supports),
        SupportsCondition::Or(list) => list.iter().any(evaluate_supports),
        SupportsCondition::Declaration { property_id, value } => {
            crate::cssom_decl::parse_and_serialize_declaration(property_id.name(), value.as_ref())
                .is_some()
        }
        SupportsCondition::Selector(sel) => {
            crate::selector::parse_selector_list_non_forgiving(sel.as_ref()).is_ok()
        }
        SupportsCondition::Unknown(_) => false,
    }
}

/// Evaluates a CSS Conditional Rules L3/L4 condition string for `CSS.supports(conditionText)`.
#[must_use]
pub fn evaluate_supports_condition_str(condition: &str) -> bool {
    let trimmed = condition.trim();
    if trimmed.is_empty() {
        return false;
    }
    // CSS.supports("(color: red)") or CSS.supports("color: red") or CSS.supports("selector(:has(*))")
    let wrapped = if trimmed.starts_with('(')
        || trimmed.to_ascii_lowercase().starts_with("not ")
        || trimmed.to_ascii_lowercase().starts_with("not(")
        || trimmed.to_ascii_lowercase().starts_with("selector(")
    {
        format!("@supports {trimmed} {{ a {{ color: red; }} }}")
    } else {
        format!("@supports ({trimmed}) {{ a {{ color: red; }} }}")
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let opts = ParserOptions::default();
        let Ok(parsed) = StyleSheet::parse(&wrapped, opts) else {
            return false;
        };
        for rule in &parsed.rules.0 {
            if let CssRule::Supports(sr) = rule {
                return evaluate_supports(&sr.condition);
            }
        }
        false
    }))
    .unwrap_or(false)
}

/// Font source entry parsed from `@font-face`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FontFaceSource {
    Url {
        url: InternedStr,
        format: Option<InternedStr>,
    },
    Local(InternedStr),
}

/// One segment of a hierarchical `@layer` path (named sublayer or unique anonymous block).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LayerSeg {
    Named(InternedStr),
    Anon(u32),
}

/// Compiled `@scope (<start>) [to (<end>)]` scoping boundary attached to a [`CompiledRule`].
#[derive(Debug, Clone)]
pub struct CompiledScope {
    /// Explicit `<scope-start>` selector list, or `None` for implicit `@scope` (owner `<style>` parent).
    pub start: Option<SelectorList<RixSelectorImpl>>,
    /// Optional `<scope-end>` limit selector list (evaluated with `:scope` = scoping root).
    pub end: Option<SelectorList<RixSelectorImpl>>,
    /// Owner `<style>` element's parent `NodeId` for implicit `@scope`.
    pub implicit_root: Option<rix_dom::NodeId>,
}

/// Compiled `@font-face` descriptor rule.
#[derive(Debug, Clone, PartialEq)]
pub struct FontFaceRule {
    pub family: Option<InternedStr>,
    pub sources: Vec<FontFaceSource>,
    pub weight_range: (f32, f32),
    pub italic: bool,
    /// Optional `unicode-range` inclusive codepoint intervals `(start, end)` (CSS Fonts L4 §4.5).
    pub unicode_ranges: Vec<(u32, u32)>,
    pub layer_id: LayerId,
}

/// Single step in an `@keyframes` animation rule.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyframeStep {
    pub offsets: Vec<f32>,
    pub declarations: Vec<SpecifiedDeclaration>,
}

/// Compiled `@keyframes` rule.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyframesRule {
    pub name: InternedStr,
    pub steps: Vec<KeyframeStep>,
    pub layer_id: LayerId,
}

/// Comparison operator for CSS Containment Level 3 `@container` size/ratio queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerCmpOp {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
}

impl ContainerCmpOp {
    #[inline]
    #[must_use]
    pub fn eval(self, actual: f32, target: f32) -> bool {
        match self {
            Self::Lt => actual < target - 0.001,
            Self::Le => actual <= target + 0.001,
            Self::Eq => (actual - target).abs() <= 0.5,
            Self::Ge => actual >= target - 0.001,
            Self::Gt => actual > target + 0.001,
        }
    }
}

/// Dimension axis targeted by a CSS `@container` size feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerAxis {
    Width,
    Height,
    InlineSize,
    BlockSize,
    AspectRatio,
}

/// Parsed CSS Containment L3 `@container` query condition AST.
#[derive(Debug, Clone, PartialEq)]
pub enum ContainerCondition {
    And(Vec<ContainerCondition>),
    Or(Vec<ContainerCondition>),
    Not(Box<ContainerCondition>),
    SizeCmp {
        axis: ContainerAxis,
        op: ContainerCmpOp,
        value: f32,
        unit: crate::values::LengthUnit,
    },
    OrientationPortrait,
    OrientationLandscape,
    StyleCustomProp {
        name: InternedStr,
        expected: Option<String>,
    },
    Unsupported,
}

/// Compiled `@container [<name>] <condition>` constraint attached to a [`CompiledRule`].
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledContainerQuery {
    pub name: Option<InternedStr>,
    pub condition: ContainerCondition,
}

impl CompiledContainerQuery {
    /// Parses a CSS Containment L3 `@container` query string (name + condition).
    #[must_use]
    pub fn parse(name: Option<&str>, cond_str: &str) -> Self {
        let cname = name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(intern_str);
        let condition = parse_container_condition(cond_str);
        Self {
            name: cname,
            condition,
        }
    }

    /// Returns `true` if this query uses a size-containment axis (requiring `container-type: size | inline-size`).
    #[must_use]
    pub fn requires_size_container(&self) -> bool {
        self.condition.requires_size_container()
    }

    /// Evaluates this `@container` query against a candidate container's laid-out `(width, height)`,
    /// typography basis, and custom properties.
    #[must_use]
    pub fn matches(
        &self,
        width: f32,
        height: f32,
        root_font_size: f32,
        elem_font_size: f32,
        custom_props: &crate::computed::CustomPropertyMap,
    ) -> bool {
        self.condition
            .eval(width, height, root_font_size, elem_font_size, custom_props)
    }
}

impl ContainerCondition {
    #[must_use]
    pub fn requires_size_container(&self) -> bool {
        match self {
            Self::And(v) | Self::Or(v) => v.iter().any(Self::requires_size_container),
            Self::Not(inner) => inner.requires_size_container(),
            Self::SizeCmp { .. } | Self::OrientationPortrait | Self::OrientationLandscape => true,
            Self::StyleCustomProp { .. } | Self::Unsupported => false,
        }
    }

    #[must_use]
    pub fn eval(
        &self,
        width: f32,
        height: f32,
        root_font_size: f32,
        elem_font_size: f32,
        custom_props: &crate::computed::CustomPropertyMap,
    ) -> bool {
        match self {
            Self::And(items) => items
                .iter()
                .all(|c| c.eval(width, height, root_font_size, elem_font_size, custom_props)),
            Self::Or(items) => items
                .iter()
                .any(|c| c.eval(width, height, root_font_size, elem_font_size, custom_props)),
            Self::Not(inner) => {
                !inner.eval(width, height, root_font_size, elem_font_size, custom_props)
            }
            Self::SizeCmp {
                axis,
                op,
                value,
                unit,
            } => {
                let eval_ctx = EvalContext {
                    font_size_px: elem_font_size,
                    root_font_size_px: root_font_size,
                    viewport_width_px: width,
                    viewport_height_px: height,
                    container_width_px: width,
                    container_height_px: height,
                    ..EvalContext::default()
                };
                match axis {
                    ContainerAxis::Width | ContainerAxis::InlineSize => {
                        op.eval(width, unit.resolve_px(*value, &eval_ctx))
                    }
                    ContainerAxis::Height | ContainerAxis::BlockSize => {
                        op.eval(height, unit.resolve_px(*value, &eval_ctx))
                    }
                    ContainerAxis::AspectRatio => {
                        let actual = if height > 0.001 { width / height } else { 0.0 };
                        op.eval(actual, *value)
                    }
                }
            }
            Self::OrientationPortrait => height >= width,
            Self::OrientationLandscape => width > height,
            Self::StyleCustomProp { name, expected } => {
                let actual = custom_props
                    .get_entry(name)
                    .map(|e| e.serialized.trim().to_string());
                match (actual, expected) {
                    (Some(a), Some(exp)) => a.eq_ignore_ascii_case(exp.trim()),
                    (Some(a), None) => !a.is_empty(),
                    (None, _) => false,
                }
            }
            Self::Unsupported => false,
        }
    }
}

fn strip_outer_parens(s: &str) -> &str {
    let mut cur = s.trim();
    loop {
        if !(cur.starts_with('(') && cur.ends_with(')')) {
            return cur;
        }
        let mut depth = 0i32;
        let mut wraps_whole = true;
        for (idx, b) in cur.bytes().enumerate() {
            match b {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 && idx + 1 < cur.len() {
                        wraps_whole = false;
                        break;
                    }
                }
                _ => {}
            }
        }
        if wraps_whole && depth == 0 {
            cur = cur[1..cur.len() - 1].trim();
        } else {
            return cur;
        }
    }
}

fn split_top_level_keyword<'a>(s: &'a str, kw: &str) -> Vec<&'a str> {
    let bytes = s.as_bytes();
    let kw_len = kw.len();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth = (depth - 1).max(0),
            _ if depth == 0 && i + kw_len <= bytes.len() => {
                let prev_ok = i == 0 || bytes[i - 1].is_ascii_whitespace() || bytes[i - 1] == b')';
                let next_ok = i + kw_len == bytes.len()
                    || bytes[i + kw_len].is_ascii_whitespace()
                    || bytes[i + kw_len] == b'(';
                if prev_ok && next_ok && s[i..i + kw_len].eq_ignore_ascii_case(kw) {
                    out.push(s[start..i].trim());
                    start = i + kw_len;
                    i = start;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if !out.is_empty() {
        out.push(s[start..].trim());
    }
    out
}

fn parse_container_length(raw: &str) -> Option<(f32, crate::values::LengthUnit)> {
    let s = raw.trim().to_ascii_lowercase();
    if s == "0" {
        return Some((0.0, crate::values::LengthUnit::Px));
    }
    let split_idx = s
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != '+')
        .unwrap_or(s.len());
    let num: f32 = s[..split_idx].parse().ok()?;
    let unit_str = s[split_idx..].trim();
    let unit = match unit_str {
        "" | "px" => crate::values::LengthUnit::Px,
        "em" => crate::values::LengthUnit::Em,
        "rem" => crate::values::LengthUnit::Rem,
        "ch" => crate::values::LengthUnit::Ch,
        "vw" => crate::values::LengthUnit::Vw,
        "vh" => crate::values::LengthUnit::Vh,
        "cqw" => crate::values::LengthUnit::Cqw,
        "cqh" => crate::values::LengthUnit::Cqh,
        "cqi" => crate::values::LengthUnit::Cqi,
        "cqb" => crate::values::LengthUnit::Cqb,
        _ => return None,
    };
    Some((num, unit))
}

fn parse_ratio_val(raw: &str) -> Option<f32> {
    let s = raw.trim();
    if let Some((a, b)) = s.split_once('/') {
        let num: f32 = a.trim().parse().ok()?;
        let den: f32 = b.trim().parse().ok()?;
        (den > 0.0).then_some(num / den)
    } else {
        s.parse::<f32>().ok()
    }
}

fn parse_container_axis(name: &str) -> Option<ContainerAxis> {
    match name.trim().to_ascii_lowercase().as_str() {
        "width" => Some(ContainerAxis::Width),
        "height" => Some(ContainerAxis::Height),
        "inline-size" => Some(ContainerAxis::InlineSize),
        "block-size" => Some(ContainerAxis::BlockSize),
        "aspect-ratio" => Some(ContainerAxis::AspectRatio),
        _ => None,
    }
}

fn invert_cmp(op: ContainerCmpOp) -> ContainerCmpOp {
    match op {
        ContainerCmpOp::Lt => ContainerCmpOp::Gt,
        ContainerCmpOp::Le => ContainerCmpOp::Ge,
        ContainerCmpOp::Eq => ContainerCmpOp::Eq,
        ContainerCmpOp::Ge => ContainerCmpOp::Le,
        ContainerCmpOp::Gt => ContainerCmpOp::Lt,
    }
}

/// Parses a CSS Containment Level 3 `@container` condition string into [`ContainerCondition`].
#[must_use]
pub fn parse_container_condition(raw: &str) -> ContainerCondition {
    let s = strip_outer_parens(raw);
    if s.is_empty() {
        return ContainerCondition::Unsupported;
    }
    let or_parts = split_top_level_keyword(s, "or");
    if or_parts.len() > 1 {
        return ContainerCondition::Or(
            or_parts
                .into_iter()
                .map(parse_container_condition)
                .collect(),
        );
    }
    let and_parts = split_top_level_keyword(s, "and");
    if and_parts.len() > 1 {
        return ContainerCondition::And(
            and_parts
                .into_iter()
                .map(parse_container_condition)
                .collect(),
        );
    }
    if s.len() > 3
        && s[..3].eq_ignore_ascii_case("not")
        && s.as_bytes()
            .get(3)
            .is_some_and(|b| b.is_ascii_whitespace() || *b == b'(')
    {
        return ContainerCondition::Not(Box::new(parse_container_condition(&s[3..])));
    }
    // `style(--prop: value)` / `style(--prop)`
    if s.len() > 6 && s[..6].eq_ignore_ascii_case("style(") && s.ends_with(')') {
        let inner = s[6..s.len() - 1].trim();
        if let Some((prop, val)) = inner.split_once(':') {
            let p = prop.trim();
            if p.starts_with("--") {
                return ContainerCondition::StyleCustomProp {
                    name: intern_str(p),
                    expected: Some(val.trim().to_string()),
                };
            }
        } else if inner.starts_with("--") {
            return ContainerCondition::StyleCustomProp {
                name: intern_str(inner),
                expected: None,
            };
        }
        return ContainerCondition::Unsupported;
    }
    // Traditional colon syntax: `min-width: 400px`, `max-inline-size: 60rem`, `orientation: landscape`
    if let Some((lhs, rhs)) = s.split_once(':') {
        let name = lhs.trim().to_ascii_lowercase();
        let val = rhs.trim();
        if name == "orientation" {
            return if val.eq_ignore_ascii_case("portrait") {
                ContainerCondition::OrientationPortrait
            } else if val.eq_ignore_ascii_case("landscape") {
                ContainerCondition::OrientationLandscape
            } else {
                ContainerCondition::Unsupported
            };
        }
        let (op, axis_name) = if let Some(rest) = name.strip_prefix("min-") {
            (ContainerCmpOp::Ge, rest)
        } else if let Some(rest) = name.strip_prefix("max-") {
            (ContainerCmpOp::Le, rest)
        } else {
            (ContainerCmpOp::Eq, name.as_str())
        };
        let Some(axis) = parse_container_axis(axis_name) else {
            return ContainerCondition::Unsupported;
        };
        if axis == ContainerAxis::AspectRatio {
            let Some(ratio) = parse_ratio_val(val) else {
                return ContainerCondition::Unsupported;
            };
            return ContainerCondition::SizeCmp {
                axis,
                op,
                value: ratio,
                unit: crate::values::LengthUnit::Px,
            };
        }
        let Some((num, unit)) = parse_container_length(val) else {
            return ContainerCondition::Unsupported;
        };
        return ContainerCondition::SizeCmp {
            axis,
            op,
            value: num,
            unit,
        };
    }
    // Range syntax: `width >= 400px`, `400px <= width <= 800px`, `inline-size > 30rem`
    let mut tokens: Vec<&str> = Vec::new();
    let mut ops: Vec<ContainerCmpOp> = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let matched = if bytes[i] == b'<' && bytes.get(i + 1) == Some(&b'=') {
            Some((ContainerCmpOp::Le, 2))
        } else if bytes[i] == b'>' && bytes.get(i + 1) == Some(&b'=') {
            Some((ContainerCmpOp::Ge, 2))
        } else if bytes[i] == b'<' {
            Some((ContainerCmpOp::Lt, 1))
        } else if bytes[i] == b'>' {
            Some((ContainerCmpOp::Gt, 1))
        } else if bytes[i] == b'=' {
            Some((ContainerCmpOp::Eq, 1))
        } else {
            None
        };
        if let Some((op, len)) = matched {
            tokens.push(s[start..i].trim());
            ops.push(op);
            i += len;
            start = i;
        } else {
            i += 1;
        }
    }
    tokens.push(s[start..].trim());

    let make_cmp =
        |axis: ContainerAxis, op: ContainerCmpOp, val_str: &str| -> Option<ContainerCondition> {
            if axis == ContainerAxis::AspectRatio {
                let ratio = parse_ratio_val(val_str)?;
                Some(ContainerCondition::SizeCmp {
                    axis,
                    op,
                    value: ratio,
                    unit: crate::values::LengthUnit::Px,
                })
            } else {
                let (num, unit) = parse_container_length(val_str)?;
                Some(ContainerCondition::SizeCmp {
                    axis,
                    op,
                    value: num,
                    unit,
                })
            }
        };

    if ops.is_empty() && tokens.len() == 1 {
        // CSS Containment L3 §5.2: boolean size feature context `(width)` / `(inline-size)` means `> 0`.
        if let Some(axis) = parse_container_axis(tokens[0]) {
            return ContainerCondition::SizeCmp {
                axis,
                op: ContainerCmpOp::Gt,
                value: 0.0,
                unit: crate::values::LengthUnit::Px,
            };
        }
    }
    if ops.len() == 1 && tokens.len() == 2 {
        if let Some(axis) = parse_container_axis(tokens[0]) {
            if let Some(c) = make_cmp(axis, ops[0], tokens[1]) {
                return c;
            }
        } else if let Some(axis) = parse_container_axis(tokens[1]) {
            if let Some(c) = make_cmp(axis, invert_cmp(ops[0]), tokens[0]) {
                return c;
            }
        }
    } else if ops.len() == 2 && tokens.len() == 3 {
        if let Some(axis) = parse_container_axis(tokens[1]) {
            if let (Some(c1), Some(c2)) = (
                make_cmp(axis, invert_cmp(ops[0]), tokens[0]),
                make_cmp(axis, ops[1], tokens[2]),
            ) {
                return ContainerCondition::And(vec![c1, c2]);
            }
        }
    }
    ContainerCondition::Unsupported
}

/// A compiled single-selector CSS style rule ready for indexing and matching.
#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub selector: Selector<RixSelectorImpl>,
    pub ancestor_hashes: AncestorHashes,
    pub origin: CascadeOrigin,
    pub layer_id: LayerId,
    pub specificity: u32,
    pub source_order: u32,
    pub declarations: Arc<[SpecifiedDeclaration]>,
    pub scopes: Option<Arc<[CompiledScope]>>,
    pub container_queries: Option<Arc<[CompiledContainerQuery]>>,
}

/// Compiled `@property` / `CSS.registerProperty` custom property registration
/// (CSS Properties and Values API Level 1).
#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredProperty {
    pub name: InternedStr,
    pub syntax: String,
    pub inherits: bool,
    pub initial_value: Option<Vec<crate::properties::CssToken>>,
    pub layer_id: LayerId,
}

/// Compiled CSS stylesheet with flattened nesting, evaluated conditions,
/// registered `@layer` ordering, `@font-face`, `@keyframes`, `@property`, and telemetry.
#[derive(Debug, Clone)]
pub struct Stylesheet {
    pub origin: CascadeOrigin,
    pub rules: Vec<CompiledRule>,
    pub layers: Vec<InternedStr>,
    pub layer_paths: Vec<Vec<LayerSeg>>,
    pub font_faces: Vec<FontFaceRule>,
    pub keyframes: Vec<KeyframesRule>,
    pub registered_properties: Vec<RegisteredProperty>,
    pub imports: Vec<String>,
    pub missing_features: Vec<MissingCssFeature>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct StylesheetCacheKey {
    hash: u64,
    len: usize,
    base_url_hash: u64,
    origin: u8,
    viewport_w: u32,
    viewport_h: u32,
    dark_mode: bool,
    reduced_motion: bool,
    quirks_mode: bool,
}

fn hash_css_bytes(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Resolves a CSS relative `url(...)` against a stylesheet base URL per CSS Values L3 §4.5
/// and RFC 3986 §5.2, leaving absolute, `data:`, `blob:`, and fragment-only URLs untouched.
pub(crate) fn resolve_css_relative_url(base_url: &str, raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("data:")
        || lower.starts_with("blob:")
        || lower.starts_with("file://")
        || lower.starts_with("about:")
    {
        return None;
    }
    let scheme_end = base_url.find("://")?;
    let scheme = &base_url[..scheme_end];
    let after_scheme = &base_url[scheme_end + 3..];
    if let Some(rest) = trimmed.strip_prefix("//") {
        return Some(format!("{scheme}://{rest}"));
    }
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let origin = &base_url[..scheme_end + 3 + authority_end];
    let base_path_raw = &after_scheme[authority_end..];
    let base_path = base_path_raw.split(['?', '#']).next().unwrap_or("/");

    let (rel_path, suffix) = match trimmed.find(['?', '#']) {
        Some(idx) => (&trimmed[..idx], &trimmed[idx..]),
        None => (trimmed, ""),
    };
    let merged = if rel_path.starts_with('/') {
        rel_path.to_string()
    } else if rel_path.is_empty() {
        base_path.to_string()
    } else {
        let dir = match base_path.rfind('/') {
            Some(slash) => &base_path[..=slash],
            None => "/",
        };
        format!("{dir}{rel_path}")
    };

    // Normalize dot segments (RFC 3986 §5.2.4).
    let trailing_slash =
        merged.ends_with('/') || rel_path.ends_with("/.") || rel_path.ends_with("/..");
    let mut segs: Vec<&str> = Vec::new();
    for seg in merged.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            other => segs.push(other),
        }
    }
    let mut norm = String::with_capacity(origin.len() + merged.len() + suffix.len() + 2);
    norm.push_str(origin);
    norm.push('/');
    for (i, seg) in segs.iter().enumerate() {
        if i > 0 {
            norm.push('/');
        }
        norm.push_str(seg);
    }
    if trailing_slash && !segs.is_empty() {
        norm.push('/');
    }
    norm.push_str(suffix);
    Some(norm)
}

fn rewrite_stylesheet_relative_urls(rules: &mut [CompiledRule], base_url: &str) {
    fn rewrite_tokens(tokens: &mut [crate::properties::CssToken], base_url: &str) {
        for tok in tokens {
            match tok {
                crate::properties::CssToken::Url(u) => {
                    if let Some(abs) = resolve_css_relative_url(base_url, u.as_str()) {
                        *u = intern_str(&abs);
                    }
                }
                crate::properties::CssToken::Function { args, .. } => {
                    rewrite_tokens(args, base_url);
                }
                crate::properties::CssToken::Var {
                    fallback: Some(fb), ..
                } => {
                    rewrite_tokens(fb, base_url);
                }
                _ => {}
            }
        }
    }
    for rule in rules {
        let mut changed = false;
        let mut decls: Vec<SpecifiedDeclaration> = rule.declarations.iter().cloned().collect();
        for decl in &mut decls {
            match &mut decl.value {
                PropertyValue::BackgroundLayers(layers) => {
                    for layer in layers.iter_mut() {
                        if let BackgroundImageValue::Url(u) = &mut layer.image {
                            if let Some(abs) = resolve_css_relative_url(base_url, u.as_str()) {
                                let interned = intern_str(&abs);
                                *u = interned.clone();
                                layer.image_url = Some(interned);
                                changed = true;
                            }
                        }
                    }
                }
                PropertyValue::MaskImage(BackgroundImageValue::Url(u)) => {
                    if let Some(abs) = resolve_css_relative_url(base_url, u.as_str()) {
                        *u = intern_str(&abs);
                        changed = true;
                    }
                }
                PropertyValue::Unresolved(tokens) => {
                    let before = tokens.clone();
                    rewrite_tokens(tokens, base_url);
                    if *tokens != before {
                        changed = true;
                    }
                }
                _ => {}
            }
        }
        if changed {
            rule.declarations = Arc::from(decls);
        }
    }
}

impl Stylesheet {
    /// Parses and compiles a CSS stylesheet against the given origin and media environment.
    #[must_use]
    pub fn parse(css: &str, origin: CascadeOrigin, media: &MediaContext) -> Self {
        Self::parse_with_owner_and_base(css, origin, media, None, None)
    }

    /// Parses and compiles a CSS stylesheet with an optional owner `<style>` parent node
    /// for implicit `@scope` rules (CSS Cascade Level 6 §2.1).
    #[must_use]
    pub fn parse_with_owner(
        css: &str,
        origin: CascadeOrigin,
        media: &MediaContext,
        owner_parent: Option<rix_dom::NodeId>,
    ) -> Self {
        Self::parse_with_owner_and_base(css, origin, media, owner_parent, None)
    }

    /// Parses and compiles a CSS stylesheet with optional `@scope` owner and base URL for
    /// resolving relative `url(...)` values per CSS Values L3 §4.5.
    #[must_use]
    pub fn parse_with_owner_and_base(
        css: &str,
        origin: CascadeOrigin,
        media: &MediaContext,
        owner_parent: Option<rix_dom::NodeId>,
        base_url: Option<&str>,
    ) -> Self {
        let quirks_norm = if origin == CascadeOrigin::Author {
            crate::properties::preprocess_quirks_and_strict_lengths(css, media.quirks_mode)
        } else {
            std::borrow::Cow::Borrowed(css)
        };
        let cleaned_custom = crate::properties::strip_unbalanced_custom_decls(&quirks_norm);
        let rewritten = crate::properties::rewrite_dynamic_var_names(&cleaned_custom);
        let wrapped = wrap_top_level_scope_rules(&rewritten);
        let opts = ParserOptions {
            error_recovery: true,
            ..ParserOptions::default()
        };
        let mut compiler = StylesheetCompiler::new(origin, *media, owner_parent, &wrapped);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Ok(sheet) = StyleSheet::parse(&wrapped, opts) {
                compiler.compile_rule_list(
                    &sheet.rules,
                    CompileNesting::TopLevel,
                    LayerId::UNLAYERED,
                    None,
                );
            }
        }));
        let mut sheet = compiler.finish();
        if let Some(base) = base_url {
            rewrite_stylesheet_relative_urls(&mut sheet.rules, base);
        }
        sheet
    }

    /// Parses a stylesheet through a process-wide shared compiled cache keyed by content hash
    /// and active media environment, avoiding redundant CSS parsing across navigations & tabs.
    #[must_use]
    pub fn parse_cached(css: &str, origin: CascadeOrigin, media: &MediaContext) -> Arc<Self> {
        Self::parse_cached_with_base(css, origin, media, None)
    }

    /// Parses a stylesheet through the shared cache while resolving relative `url(...)` values
    /// against `base_url` per CSS Values L3 §4.5.
    #[must_use]
    pub fn parse_cached_with_base(
        css: &str,
        origin: CascadeOrigin,
        media: &MediaContext,
        base_url: Option<&str>,
    ) -> Arc<Self> {
        static CACHE: OnceLock<RwLock<HashMap<StylesheetCacheKey, Arc<Stylesheet>>>> =
            OnceLock::new();
        let cache = CACHE.get_or_init(|| RwLock::new(HashMap::new()));
        let key = StylesheetCacheKey {
            hash: hash_css_bytes(css.as_bytes()),
            len: css.len(),
            base_url_hash: base_url.map_or(0, |b| hash_css_bytes(b.as_bytes())),
            origin: origin as u8,
            viewport_w: media.viewport_width.round().max(0.0) as u32,
            viewport_h: media.viewport_height.round().max(0.0) as u32,
            dark_mode: media.dark_mode,
            reduced_motion: media.reduced_motion,
            quirks_mode: media.quirks_mode,
        };
        if let Ok(guard) = cache.read() {
            if let Some(hit) = guard.get(&key) {
                return Arc::clone(hit);
            }
        }
        let compiled = Arc::new(Self::parse_with_owner_and_base(
            css, origin, media, None, base_url,
        ));
        if let Ok(mut guard) = cache.write() {
            if guard.len() >= 256 {
                guard.clear();
            }
            guard.insert(key, Arc::clone(&compiled));
        }
        compiled
    }

    /// Returns all unique image `url(...)` references compiled in this stylesheet, including
    /// `background-image`, `mask-image`, and `url(...)` values stored inside custom properties
    /// (`--logo: url(...)`) or unparsed `var()` declarations.
    #[must_use]
    pub fn referenced_image_urls(&self) -> Vec<String> {
        fn collect_urls_from_tokens(
            tokens: &[crate::properties::CssToken],
            seen: &mut HashSet<String>,
            out: &mut Vec<String>,
        ) {
            for tok in tokens {
                match tok {
                    crate::properties::CssToken::Url(url) => {
                        let trimmed = url.as_str().trim();
                        if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                            out.push(trimmed.to_string());
                        }
                    }
                    crate::properties::CssToken::Function { args, .. } => {
                        collect_urls_from_tokens(args, seen, out);
                    }
                    crate::properties::CssToken::Var {
                        fallback: Some(fb), ..
                    } => {
                        collect_urls_from_tokens(fb, seen, out);
                    }
                    _ => {}
                }
            }
        }
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for rule in &self.rules {
            for decl in rule.declarations.iter() {
                match &decl.value {
                    PropertyValue::BackgroundLayers(layers) => {
                        for layer in layers {
                            if let BackgroundImageValue::Url(url) = &layer.image {
                                let trimmed = url.as_str().trim();
                                if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                                    out.push(trimmed.to_string());
                                }
                            }
                        }
                    }
                    PropertyValue::MaskImage(BackgroundImageValue::Url(url)) => {
                        let trimmed = url.as_str().trim();
                        if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                            out.push(trimmed.to_string());
                        }
                    }
                    PropertyValue::Custom(tokens) | PropertyValue::Unresolved(tokens) => {
                        collect_urls_from_tokens(tokens, &mut seen, &mut out);
                    }
                    _ => {}
                }
            }
        }
        out
    }
}

#[derive(Clone, Copy)]
enum CompileNesting<'a> {
    TopLevel,
    StyleRule { parent_selector: &'a str },
    ScopeRule,
}

struct StylesheetCompiler<'a> {
    origin: CascadeOrigin,
    media: MediaContext,
    owner_parent: Option<rix_dom::NodeId>,
    source_css: &'a str,
    line_index: LineIndex,
    rules: Vec<CompiledRule>,
    layers: Vec<InternedStr>,
    layer_paths: Vec<Vec<LayerSeg>>,
    layer_path_map: HashMap<Vec<LayerSeg>, LayerId>,
    anon_layer_counter: u32,
    scope_stack: Vec<CompiledScope>,
    container_stack: Vec<CompiledContainerQuery>,
    font_faces: Vec<FontFaceRule>,
    keyframes: Vec<KeyframesRule>,
    registered_properties: Vec<RegisteredProperty>,
    imports: Vec<String>,
    missing_seen: HashSet<MissingCssFeature>,
    missing_features: Vec<MissingCssFeature>,
    next_source_order: u32,
    selector_scratch: String,
    namespaces: SelectorNamespaceMap,
}

impl<'a> StylesheetCompiler<'a> {
    fn new(
        origin: CascadeOrigin,
        media: MediaContext,
        owner_parent: Option<rix_dom::NodeId>,
        source_css: &'a str,
    ) -> Self {
        Self {
            origin,
            media,
            owner_parent,
            source_css,
            line_index: LineIndex::new(source_css),
            rules: Vec::new(),
            layers: Vec::new(),
            layer_paths: Vec::new(),
            layer_path_map: HashMap::new(),
            anon_layer_counter: 0,
            scope_stack: Vec::new(),
            container_stack: Vec::new(),
            font_faces: Vec::new(),
            keyframes: Vec::new(),
            registered_properties: Vec::new(),
            imports: Vec::new(),
            missing_seen: HashSet::new(),
            missing_features: Vec::new(),
            next_source_order: 0,
            selector_scratch: String::with_capacity(128),
            namespaces: SelectorNamespaceMap::default(),
        }
    }

    fn record_missing(&mut self, feat: MissingCssFeature) {
        if self.missing_seen.insert(feat.clone()) {
            self.missing_features.push(feat);
        }
    }

    fn format_layer_path(path: &[LayerSeg]) -> InternedStr {
        let mut out = String::new();
        for (i, seg) in path.iter().enumerate() {
            if i > 0 {
                out.push('.');
            }
            match seg {
                LayerSeg::Named(s) => out.push_str(s.as_str()),
                LayerSeg::Anon(id) => {
                    use std::fmt::Write as _;
                    let _ = write!(out, "__anon_{id}");
                }
            }
        }
        intern_str(&out)
    }

    fn ensure_layer_path(&mut self, path: Vec<LayerSeg>) -> LayerId {
        if let Some(&id) = self.layer_path_map.get(&path) {
            return id;
        }
        let id = LayerId(self.layer_paths.len() as u32);
        let label = Self::format_layer_path(&path);
        self.layers.push(label);
        self.layer_paths.push(path.clone());
        self.layer_path_map.insert(path, id);
        id
    }

    fn register_layer(
        &mut self,
        prefix_layer: Option<LayerId>,
        name: Option<&LayerName<'_>>,
    ) -> LayerId {
        let mut path = prefix_layer
            .and_then(|id| self.layer_paths.get(id.0 as usize).cloned())
            .unwrap_or_default();
        if let Some(n) = name {
            let mut last_id = LayerId::UNLAYERED;
            for part in &n.0 {
                path.push(LayerSeg::Named(intern_str(part.as_ref())));
                last_id = self.ensure_layer_path(path.clone());
            }
            last_id
        } else {
            let aid = self.anon_layer_counter;
            self.anon_layer_counter += 1;
            path.push(LayerSeg::Anon(aid));
            let id = LayerId(self.layer_paths.len() as u32);
            let label = Self::format_layer_path(&path);
            self.layers.push(label);
            self.layer_paths.push(path.clone());
            self.layer_path_map.insert(path, id);
            id
        }
    }

    fn compile_rule_list(
        &mut self,
        list: &CssRuleList<'_>,
        nesting: CompileNesting<'_>,
        current_layer: LayerId,
        prefix_layer: Option<LayerId>,
    ) {
        let in_style_nesting = matches!(nesting, CompileNesting::StyleRule { .. });
        for rule in &list.0 {
            match rule {
                CssRule::Style(style) => {
                    self.compile_style_rule(style, nesting, current_layer, prefix_layer);
                }
                CssRule::Nesting(nest) => {
                    self.compile_style_rule(&nest.style, nesting, current_layer, prefix_layer);
                }
                CssRule::NestedDeclarations(nested) => match nesting {
                    CompileNesting::StyleRule { parent_selector } => {
                        // CSS Nesting L1 §2.2: CSSNestedDeclarations inherits top-level
                        // selector specificity (each comma-separated parent selector separately).
                        self.emit_rules_for_selector_str(
                            parent_selector,
                            &nested.declarations,
                            current_layer,
                            ParseRelative::No,
                        );
                    }
                    CompileNesting::ScopeRule => {
                        // CSS Cascade L6 §2.5: bare declarations in `@scope` match `:where(:scope)`.
                        self.emit_rules_for_selector_str(
                            ":where(:scope)",
                            &nested.declarations,
                            current_layer,
                            ParseRelative::No,
                        );
                    }
                    CompileNesting::TopLevel => {}
                },
                CssRule::Media(media_rule) => {
                    if self.media.matches_media_list(&media_rule.query) {
                        self.compile_rule_list(
                            &media_rule.rules,
                            nesting,
                            current_layer,
                            prefix_layer,
                        );
                    }
                }
                CssRule::Supports(sup_rule) => {
                    if evaluate_supports(&sup_rule.condition) {
                        self.compile_rule_list(
                            &sup_rule.rules,
                            nesting,
                            current_layer,
                            prefix_layer,
                        );
                    }
                }
                CssRule::LayerStatement(stmt) if !in_style_nesting => {
                    for name in &stmt.names {
                        let _ = self.register_layer(prefix_layer, Some(name));
                    }
                }
                CssRule::LayerBlock(block) => {
                    let layer_id = self.register_layer(prefix_layer, block.name.as_ref());
                    self.compile_rule_list(&block.rules, nesting, layer_id, Some(layer_id));
                }
                CssRule::Scope(sc) => {
                    let parent_sel = match nesting {
                        CompileNesting::StyleRule { parent_selector } => Some(parent_selector),
                        _ => None,
                    };
                    if let Some(compiled_scope) = self.compile_scope_prelude(sc, parent_sel) {
                        self.scope_stack.push(compiled_scope);
                        self.compile_rule_list(
                            &sc.rules,
                            CompileNesting::ScopeRule,
                            current_layer,
                            prefix_layer,
                        );
                        self.scope_stack.pop();
                    }
                }
                CssRule::Container(cr) => {
                    let cname = cr.name.as_ref().map(lightning_to_css_string);
                    let cond_str = lightning_to_css_string(&cr.condition);
                    let compiled_cq = CompiledContainerQuery::parse(cname.as_deref(), &cond_str);
                    self.container_stack.push(compiled_cq);
                    self.compile_rule_list(&cr.rules, nesting, current_layer, prefix_layer);
                    self.container_stack.pop();
                }
                CssRule::FontFace(ff) if !in_style_nesting => {
                    self.compile_font_face(ff, current_layer);
                }
                CssRule::Keyframes(kf) if !in_style_nesting => {
                    self.compile_keyframes(kf, current_layer);
                }
                CssRule::Property(pr) if !in_style_nesting => {
                    self.compile_property_rule(pr, current_layer);
                }
                CssRule::Import(imp) if !in_style_nesting => {
                    if self.media.matches_media_list(&imp.media) {
                        let url = imp.url.as_ref().trim();
                        if !url.is_empty() {
                            self.imports.push(url.to_string());
                        }
                    }
                }
                CssRule::Namespace(ns) if !in_style_nesting => {
                    let url = CssAtom::new(ns.url.as_ref());
                    if let Some(ref pref) = ns.prefix {
                        self.namespaces
                            .prefixes
                            .insert(pref.as_ref().to_string(), url);
                    } else {
                        self.namespaces.default_namespace = Some(url);
                    }
                }
                CssRule::Ignored
                | CssRule::FontFace(_)
                | CssRule::Keyframes(_)
                | CssRule::Property(_)
                | CssRule::Import(_)
                | CssRule::Namespace(_)
                | CssRule::LayerStatement(_) => {}
                CssRule::Unknown(u) => {
                    self.record_missing(MissingCssFeature::new(
                        MissingCssKind::AtRule,
                        u.name.as_ref(),
                    ));
                }
                _ => {
                    self.record_missing(MissingCssFeature::new(MissingCssKind::AtRule, "@other"));
                }
            }
        }
    }

    fn compile_scope_prelude(
        &self,
        sc: &lightningcss::rules::scope::ScopeRule<'_>,
        parent_selector: Option<&str>,
    ) -> Option<CompiledScope> {
        let ns_opt = (!self.namespaces.is_empty()).then_some(&self.namespaces);
        let start = if let Some(ref s) = sc.scope_start {
            let raw = lightning_to_css_string(s);
            if raw.trim().is_empty() {
                return None;
            }
            let expanded = if let Some(parent) = parent_selector {
                replace_ampersand_in_selector(&raw, parent)
            } else {
                raw
            };
            let list =
                parse_selector_list_relative(&expanded, false, ns_opt, ParseRelative::No).ok()?;
            if list.slice().is_empty() || list.slice().iter().any(Selector::has_pseudo_element) {
                return None;
            }
            Some(list)
        } else {
            None
        };
        let end = if let Some(ref e) = sc.scope_end {
            let raw = lightning_to_css_string(e);
            if raw.trim().is_empty() {
                return None;
            }
            let list =
                parse_selector_list_relative(&raw, false, ns_opt, ParseRelative::ForScope).ok()?;
            if list.slice().is_empty() || list.slice().iter().any(Selector::has_pseudo_element) {
                return None;
            }
            Some(list)
        } else {
            None
        };
        let implicit_root = if start.is_none() {
            self.owner_parent
        } else {
            None
        };
        Some(CompiledScope {
            start,
            end,
            implicit_root,
        })
    }

    fn compile_style_rule(
        &mut self,
        style: &lightningcss::rules::style::StyleRule<'_>,
        nesting: CompileNesting<'_>,
        layer: LayerId,
        prefix_layer: Option<LayerId>,
    ) {
        let mut raw = String::new();
        {
            let mut printer = Printer::new(&mut raw, PrinterOptions::default());
            if style.selectors.to_css(&mut printer).is_err() {
                return;
            }
        }
        if raw == ".__rix_top_scope__" {
            self.compile_rule_list(&style.rules, nesting, layer, prefix_layer);
            return;
        }
        let (resolved_selector, parse_rel) = match nesting {
            CompileNesting::StyleRule { parent_selector } => (
                flatten_nested_selector(parent_selector, &raw),
                ParseRelative::No,
            ),
            CompileNesting::ScopeRule => {
                let raw_src = extract_raw_selector_at_loc(
                    self.source_css,
                    &self.line_index,
                    style.loc.line,
                    style.loc.column,
                );
                let cleaned = strip_synthetic_scope_ampersands(&raw, raw_src);
                // CSSWG #9740: `&` inside `@scope` behaves like `:where(:scope)` (0 specificity).
                let replaced = replace_ampersand_in_selector(&cleaned, ":where(:scope)");
                (replaced, ParseRelative::ForScope)
            }
            CompileNesting::TopLevel => (raw, ParseRelative::No),
        };

        if !style.declarations.declarations.is_empty()
            || !style.declarations.important_declarations.is_empty()
        {
            if matches!(nesting, CompileNesting::TopLevel) {
                let ns_opt = (!self.namespaces.is_empty()).then_some(&self.namespaces);
                if let Some(parsed) = compile_lightning_selector_list_with_namespaces(
                    &style.selectors,
                    &mut self.selector_scratch,
                    ns_opt,
                ) {
                    self.emit_compiled_selectors(parsed.slice(), &style.declarations, layer);
                } else {
                    self.record_missing(MissingCssFeature::new(
                        MissingCssKind::Selector,
                        &resolved_selector,
                    ));
                }
            } else {
                self.emit_rules_for_selector_str(
                    &resolved_selector,
                    &style.declarations,
                    layer,
                    parse_rel,
                );
            }
        }

        if !style.rules.0.is_empty() {
            self.compile_rule_list(
                &style.rules,
                CompileNesting::StyleRule {
                    parent_selector: &resolved_selector,
                },
                layer,
                prefix_layer,
            );
        }
    }

    fn emit_rules_for_selector_str(
        &mut self,
        selector_str: &str,
        block: &lightningcss::declaration::DeclarationBlock<'_>,
        layer: LayerId,
        parse_rel: ParseRelative,
    ) {
        let ns_opt = (!self.namespaces.is_empty()).then_some(&self.namespaces);
        if let Ok(parsed) = parse_selector_list_relative(selector_str, true, ns_opt, parse_rel) {
            self.emit_compiled_selectors(parsed.slice(), block, layer);
        } else {
            self.record_missing(MissingCssFeature::new(
                MissingCssKind::Selector,
                selector_str,
            ));
        }
    }

    fn emit_compiled_selectors(
        &mut self,
        selectors: &[Selector<RixSelectorImpl>],
        block: &lightningcss::declaration::DeclarationBlock<'_>,
        layer: LayerId,
    ) {
        let mut decls = Vec::new();
        let mut local_missing = Vec::new();
        compile_declaration_block(block, &mut decls, &mut local_missing);
        for m in local_missing {
            self.record_missing(m);
        }
        if decls.is_empty() {
            return;
        }
        let shared_decls: Arc<[SpecifiedDeclaration]> = Arc::from(decls);
        let scopes = if self.scope_stack.is_empty() {
            None
        } else {
            Some(Arc::<[CompiledScope]>::from(self.scope_stack.as_slice()))
        };
        let container_queries = if self.container_stack.is_empty() {
            None
        } else {
            Some(Arc::<[CompiledContainerQuery]>::from(
                self.container_stack.as_slice(),
            ))
        };
        for sel in selectors {
            let source_order = self.next_source_order;
            self.next_source_order = self.next_source_order.saturating_add(1);
            let ancestor_hashes = AncestorHashes::new(sel, QuirksMode::NoQuirks);
            self.rules.push(CompiledRule {
                selector: sel.clone(),
                ancestor_hashes,
                origin: self.origin,
                layer_id: layer,
                specificity: sel.specificity(),
                source_order,
                declarations: Arc::clone(&shared_decls),
                scopes: scopes.clone(),
                container_queries: container_queries.clone(),
            });
        }
    }

    fn compile_font_face(&mut self, ff: &LightningFontFace<'_>, layer_id: LayerId) {
        let mut family = None;
        let mut sources = Vec::new();
        let mut weight_range = (400.0, 400.0);
        let mut italic = false;
        let mut unicode_ranges = Vec::new();
        for prop in &ff.properties {
            match prop {
                FontFaceProperty::FontFamily(f) => family = Some(convert_font_family_name(f)),
                FontFaceProperty::Source(srcs) => {
                    for s in srcs {
                        match s {
                            Source::Url(u) => {
                                let fmt = u.format.as_ref().map(|f| {
                                    let mut buf = String::new();
                                    let mut printer =
                                        Printer::new(&mut buf, PrinterOptions::default());
                                    let _ = LightningToCss::to_css(f, &mut printer);
                                    intern_str(buf.trim_matches('"').trim_matches('\''))
                                });
                                sources.push(FontFaceSource::Url {
                                    url: intern_str(u.url.url.as_ref()),
                                    format: fmt,
                                });
                            }
                            Source::Local(loc) => {
                                sources.push(FontFaceSource::Local(convert_font_family_name(loc)));
                            }
                        }
                    }
                }
                FontFaceProperty::FontWeight(range) => {
                    weight_range = (
                        crate::values::convert_font_weight(&range.0),
                        crate::values::convert_font_weight(&range.1),
                    );
                }
                FontFaceProperty::FontStyle(st) => {
                    italic = !matches!(st, lightningcss::rules::font_face::FontStyle::Normal);
                }
                FontFaceProperty::UnicodeRange(ranges) => {
                    unicode_ranges = ranges.iter().map(|r| (r.start, r.end)).collect();
                }
                _ => {}
            }
        }
        self.font_faces.push(FontFaceRule {
            family,
            sources,
            weight_range,
            italic,
            unicode_ranges,
            layer_id,
        });
    }

    fn compile_keyframes(&mut self, kf: &LightningKeyframes<'_>, layer_id: LayerId) {
        let name = match &kf.name {
            KeyframesName::Ident(id) => intern_str(id.as_ref()),
            KeyframesName::Custom(s) => {
                if s.as_ref().is_empty() {
                    return;
                }
                intern_str(s.as_ref())
            }
        };
        let mut steps = Vec::with_capacity(kf.keyframes.len());
        for frame in &kf.keyframes {
            let offsets = frame
                .selectors
                .iter()
                .map(|sel| match sel {
                    KeyframeSelector::Percentage(p) => p.0,
                    KeyframeSelector::From => 0.0,
                    KeyframeSelector::To => 1.0,
                    KeyframeSelector::TimelineRangePercentage(_) => 0.0,
                })
                .collect();
            let mut decls = Vec::new();
            let mut local_missing = Vec::new();
            compile_declaration_block(&frame.declarations, &mut decls, &mut local_missing);
            for m in local_missing {
                self.record_missing(m);
            }
            steps.push(KeyframeStep {
                offsets,
                declarations: decls,
            });
        }
        self.keyframes.push(KeyframesRule {
            name,
            steps,
            layer_id,
        });
    }

    fn compile_property_rule(
        &mut self,
        pr: &lightningcss::rules::property::PropertyRule<'_>,
        layer_id: LayerId,
    ) {
        let name = intern_str(pr.name.as_ref());
        let syntax = lightning_to_css_string(&pr.syntax);
        let initial_value = pr.initial_value.as_ref().map(|iv| {
            let s = lightning_to_css_string(iv);
            crate::properties::tokenize_custom_value_str(&s)
        });
        self.registered_properties.push(RegisteredProperty {
            name,
            syntax,
            inherits: pr.inherits,
            initial_value,
            layer_id,
        });
    }

    fn finish(self) -> Stylesheet {
        Stylesheet {
            origin: self.origin,
            rules: self.rules,
            layers: self.layers,
            layer_paths: self.layer_paths,
            font_faces: self.font_faces,
            keyframes: self.keyframes,
            registered_properties: self.registered_properties,
            imports: self.imports,
            missing_features: self.missing_features,
        }
    }
}

fn join_layer_name(name: &LayerName<'_>) -> String {
    let mut out = String::new();
    for (i, part) in name.0.iter().enumerate() {
        if i > 0 {
            out.push('.');
        }
        out.push_str(part.as_ref());
    }
    out
}

#[inline]
fn skip_css_ignored(bytes: &[u8], i: &mut usize, in_str: &mut Option<u8>) -> bool {
    let b = bytes[*i];
    if let Some(q) = *in_str {
        *i = if b == b'\\' {
            (*i + 2).min(bytes.len())
        } else {
            *i + 1
        };
        if b == q {
            *in_str = None;
        }
        return true;
    }
    if b == b'\\' {
        *i = (*i + 2).min(bytes.len());
        return true;
    }
    if b == b'"' || b == b'\'' {
        *in_str = Some(b);
        *i += 1;
        return true;
    }
    if b == b'/' && *i + 1 < bytes.len() && bytes[*i + 1] == b'*' {
        *i += 2;
        while *i + 1 < bytes.len() && !(bytes[*i] == b'*' && bytes[*i + 1] == b'/') {
            *i += 1;
        }
        *i = (*i + 2).min(bytes.len());
        return true;
    }
    false
}

fn split_top_level_commas(input: &str) -> Vec<&str> {
    let bytes = input.as_bytes();
    let (mut parts, mut start, mut depth, mut in_str, mut i) = (Vec::new(), 0, 0usize, None, 0);
    while i < bytes.len() {
        if skip_css_ignored(bytes, &mut i, &mut in_str) {
            continue;
        }
        match bytes[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(&input[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&input[start..]);
    parts
}

fn selector_has_ampersand(sel: &str) -> bool {
    let bytes = sel.as_bytes();
    let (mut in_str, mut i) = (None, 0);
    while i < bytes.len() {
        if skip_css_ignored(bytes, &mut i, &mut in_str) {
            continue;
        }
        if bytes[i] == b'&' {
            return true;
        }
        i += 1;
    }
    false
}

fn replace_ampersand_in_selector(sel: &str, replacement: &str) -> String {
    if !selector_has_ampersand(sel) {
        return sel.to_owned();
    }
    let bytes = sel.as_bytes();
    let mut out = String::with_capacity(sel.len() + replacement.len());
    let (mut in_str, mut i, mut last) = (None, 0, 0);
    while i < bytes.len() {
        if skip_css_ignored(bytes, &mut i, &mut in_str) {
            continue;
        }
        if bytes[i] == b'&' {
            out.push_str(&sel[last..i]);
            out.push_str(replacement);
            last = i + 1;
        }
        i += 1;
    }
    out.push_str(&sel[last..]);
    out
}

fn flatten_nested_selector(parent: &str, child_list: &str) -> String {
    let wrapped_parent = format!(":is({parent})");
    let mut parts = Vec::new();
    for child in split_top_level_commas(child_list) {
        let trimmed = child.trim();
        if trimmed.is_empty() {
            continue;
        }
        let starts_with_comb =
            trimmed.starts_with('>') || trimmed.starts_with('+') || trimmed.starts_with('~');
        if starts_with_comb {
            let expanded_rest = replace_ampersand_in_selector(trimmed, &wrapped_parent);
            parts.push(format!("{wrapped_parent} {expanded_rest}"));
        } else if selector_has_ampersand(trimmed) {
            parts.push(replace_ampersand_in_selector(trimmed, &wrapped_parent));
        } else {
            parts.push(format!("{wrapped_parent} {trimmed}"));
        }
    }
    parts.join(", ")
}

/// Normalizes a nested style rule selector list per CSS Nesting Level 1 §2.1:
/// prepends `& ` to relative selectors (starting with `>`, `+`, `~` or lacking `&`).
#[must_use]
pub fn normalize_nested_selector_text(raw_list: &str) -> String {
    let mut out = Vec::new();
    for part in split_top_level_commas(raw_list) {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let starts_with_comb =
            trimmed.starts_with('>') || trimmed.starts_with('+') || trimmed.starts_with('~');
        if starts_with_comb || !selector_has_ampersand(trimmed) {
            out.push(format!("& {trimmed}"));
        } else {
            out.push(trimmed.to_owned());
        }
    }
    out.join(", ")
}

/// Validates and canonically serializes a `CSSStyleRule.selectorText` assignment
/// respecting its enclosing [`CssomNestingContext`].
pub fn parse_and_serialize_selector_for_context(
    raw: &str,
    namespaces: Option<&SelectorNamespaceMap>,
    nesting: CssomNestingContext,
) -> Result<String, String> {
    match nesting {
        CssomNestingContext::TopLevel => {
            let parsed = parse_selector_list_with_namespaces(raw, namespaces)?;
            Ok(serialize_selector_list(&parsed))
        }
        CssomNestingContext::InsideStyleRule => {
            let normalized = normalize_nested_selector_text(raw);
            let parsed = parse_selector_list_relative(
                &normalized,
                true,
                namespaces,
                ParseRelative::ForNesting,
            )?;
            Ok(serialize_selector_list(&parsed))
        }
        CssomNestingContext::InsideScopeRule => {
            let parsed =
                parse_selector_list_relative(raw, true, namespaces, ParseRelative::ForScope)?;
            Ok(serialize_selector_list(&parsed))
        }
    }
}

/// Nesting context determining valid child rules and relative selector serialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CssomNestingContext {
    TopLevel,
    InsideStyleRule,
    InsideScopeRule,
}

/// Error returned by [`parse_single_cssom_rule_with_context`] distinguishing `DOMException` types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CssomParseError {
    SyntaxError(String),
    HierarchyRequestError(String),
}

/// Structured CSSOM rule representation exposed to `CSSStyleSheet.cssRules` and `insertRule`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CssomRuleKind {
    Style {
        selector_text: String,
        declarations_text: String,
        rules: Vec<CssomRuleKind>,
    },
    NestedDeclarations {
        declarations_text: String,
    },
    Import {
        url: String,
        /// `None` = no layer, `Some(None)` = anonymous `layer`, `Some(Some(name))` = `layer(name)`.
        layer: Option<Option<String>>,
        supports: Option<String>,
        media: String,
    },
    Media {
        condition_text: String,
        rules: Vec<CssomRuleKind>,
    },
    Supports {
        condition_text: String,
        rules: Vec<CssomRuleKind>,
    },
    Container {
        container_name: Option<String>,
        container_query: String,
        condition_text: String,
        rules: Vec<CssomRuleKind>,
    },
    LayerBlock {
        name: String,
        rules: Vec<CssomRuleKind>,
    },
    LayerStatement {
        names: Vec<String>,
    },
    Scope {
        start: Option<String>,
        end: Option<String>,
        rules: Vec<CssomRuleKind>,
    },
    Namespace {
        prefix: Option<String>,
        url: String,
    },
    FontFace {
        declarations_text: String,
    },
    Page {
        selector_text: String,
        declarations_text: String,
        rules: Vec<CssomRuleKind>,
    },
    Keyframes {
        name: String,
        rules: Vec<CssomRuleKind>,
    },
    Keyframe {
        key_text: String,
        declarations_text: String,
    },
    Property {
        name: String,
        css_text: String,
    },
    Other {
        css_text: String,
    },
}

fn serialize_grouping_block(
    header: &str,
    decls: Option<&str>,
    rules: &[CssomRuleKind],
    is_style_rule: bool,
) -> String {
    let decls_trimmed = decls.map(str::trim).unwrap_or("");
    let non_empty_children: Vec<String> = rules
        .iter()
        .map(CssomRuleKind::to_css_text)
        .filter(|s| !s.is_empty())
        .collect();

    if is_style_rule {
        if non_empty_children.is_empty() {
            if decls_trimmed.is_empty() {
                return format!("{header} {{ }}");
            }
            return format!("{header} {{ {decls_trimmed} }}");
        }
        let mut out = String::new();
        out.push_str(header);
        out.push_str(" {\n");
        if !decls_trimmed.is_empty() {
            out.push_str("  ");
            out.push_str(decls_trimmed);
            out.push('\n');
        }
        for child in non_empty_children {
            out.push_str("  ");
            out.push_str(&child);
            out.push('\n');
        }
        out.push('}');
        out
    } else {
        if non_empty_children.is_empty() {
            return format!("{header} {{\n}}");
        }
        let mut out = String::new();
        out.push_str(header);
        out.push_str(" {\n");
        for child in non_empty_children {
            out.push_str("  ");
            out.push_str(&child);
            out.push('\n');
        }
        out.push('}');
        out
    }
}

impl CssomRuleKind {
    /// Returns child rules if this is a `CSSGroupingRule` (including `CSSStyleRule`, `CSSPageRule`, `CSSKeyframesRule`).
    #[must_use]
    pub fn child_rules(&self) -> Option<&[CssomRuleKind]> {
        match self {
            Self::Style { rules, .. }
            | Self::Media { rules, .. }
            | Self::Supports { rules, .. }
            | Self::Container { rules, .. }
            | Self::LayerBlock { rules, .. }
            | Self::Scope { rules, .. }
            | Self::Page { rules, .. }
            | Self::Keyframes { rules, .. } => Some(rules),
            _ => None,
        }
    }

    /// Returns mutable child rules if this is a `CSSGroupingRule` (including `CSSStyleRule`, `CSSPageRule`, `CSSKeyframesRule`).
    pub fn child_rules_mut(&mut self) -> Option<&mut Vec<CssomRuleKind>> {
        match self {
            Self::Style { rules, .. }
            | Self::Media { rules, .. }
            | Self::Supports { rules, .. }
            | Self::Container { rules, .. }
            | Self::LayerBlock { rules, .. }
            | Self::Scope { rules, .. }
            | Self::Page { rules, .. }
            | Self::Keyframes { rules, .. } => Some(rules),
            _ => None,
        }
    }

    /// Returns the declaration text for `Style`, `NestedDeclarations`, `Page`, `FontFace`, or `Keyframe` rules.
    #[must_use]
    pub fn declarations_text(&self) -> Option<&str> {
        match self {
            Self::Style {
                declarations_text, ..
            }
            | Self::NestedDeclarations { declarations_text }
            | Self::Page {
                declarations_text, ..
            }
            | Self::FontFace { declarations_text }
            | Self::Keyframe {
                declarations_text, ..
            } => Some(declarations_text),
            _ => None,
        }
    }

    /// Returns mutable declaration text for `Style`, `NestedDeclarations`, `Page`, `FontFace`, or `Keyframe` rules.
    pub fn declarations_text_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Style {
                declarations_text, ..
            }
            | Self::NestedDeclarations { declarations_text }
            | Self::Page {
                declarations_text, ..
            }
            | Self::FontFace { declarations_text }
            | Self::Keyframe {
                declarations_text, ..
            } => Some(declarations_text),
            _ => None,
        }
    }

    /// Serializes this CSSOM rule back into canonical CSS text.
    #[must_use]
    pub fn to_css_text(&self) -> String {
        match self {
            Self::Style {
                selector_text,
                declarations_text,
                rules,
            } => serialize_grouping_block(selector_text, Some(declarations_text), rules, true),
            Self::NestedDeclarations { declarations_text } => declarations_text.trim().to_string(),
            Self::Import {
                url,
                layer,
                supports,
                media,
            } => {
                let escaped = url.replace('\\', "\\\\").replace('"', "\\\"");
                let mut out = format!("@import url(\"{escaped}\")");
                if let Some(layer_opt) = layer {
                    out.push_str(" layer");
                    if let Some(name) = layer_opt {
                        out.push('(');
                        out.push_str(name);
                        out.push(')');
                    }
                }
                if let Some(sup) = supports {
                    out.push_str(" supports(");
                    out.push_str(sup);
                    out.push(')');
                }
                if !media.trim().is_empty() {
                    out.push(' ');
                    out.push_str(media.trim());
                }
                out.push(';');
                out
            }
            Self::Media {
                condition_text,
                rules,
            } => serialize_grouping_block(&format!("@media {condition_text}"), None, rules, false),
            Self::Supports {
                condition_text,
                rules,
            } => {
                serialize_grouping_block(&format!("@supports {condition_text}"), None, rules, false)
            }
            Self::Container {
                condition_text,
                rules,
                ..
            } => serialize_grouping_block(
                &format!("@container {condition_text}"),
                None,
                rules,
                false,
            ),
            Self::LayerBlock { name, rules } => {
                let header = if name.is_empty() {
                    "@layer".to_string()
                } else {
                    format!("@layer {name}")
                };
                serialize_grouping_block(&header, None, rules, false)
            }
            Self::LayerStatement { names } => format!("@layer {};", names.join(", ")),
            Self::Scope { start, end, rules } => {
                let mut header = "@scope".to_string();
                if let Some(s) = start {
                    header.push_str(" (");
                    header.push_str(s);
                    header.push(')');
                }
                if let Some(e) = end {
                    header.push_str(" to (");
                    header.push_str(e);
                    header.push(')');
                }
                serialize_grouping_block(&header, None, rules, false)
            }
            Self::Namespace { prefix, url } => {
                let escaped = url.replace('\\', "\\\\").replace('"', "\\\"");
                match prefix {
                    Some(p) if !p.is_empty() => format!("@namespace {p} url(\"{escaped}\");"),
                    _ => format!("@namespace url(\"{escaped}\");"),
                }
            }
            Self::FontFace { declarations_text } => {
                let d = declarations_text.trim();
                if d.is_empty() {
                    "@font-face { }".to_string()
                } else {
                    format!("@font-face {{ {d} }}")
                }
            }
            Self::Page {
                selector_text,
                declarations_text,
                rules,
            } => {
                let header = if selector_text.is_empty() {
                    "@page".to_string()
                } else {
                    format!("@page {selector_text}")
                };
                serialize_grouping_block(&header, Some(declarations_text), rules, true)
            }
            Self::Keyframes { name, rules } => {
                let ser_name = serialize_keyframes_name(name);
                if rules.is_empty() {
                    format!("@keyframes {ser_name} {{ }}")
                } else {
                    let body = rules
                        .iter()
                        .map(Self::to_css_text)
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!("@keyframes {ser_name} {{ {body} }}")
                }
            }
            Self::Keyframe {
                key_text,
                declarations_text,
            } => {
                let d = declarations_text.trim();
                if d.is_empty() {
                    format!("{key_text} {{ }}")
                } else {
                    format!("{key_text} {{ {d} }}")
                }
            }
            Self::Property { css_text, .. } | Self::Other { css_text } => css_text.clone(),
        }
    }
}

fn serialize_keyframes_name(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let is_reserved = matches!(
        lower.as_str(),
        "initial" | "inherit" | "unset" | "revert" | "revert-layer" | "none" | "default"
    );
    let is_valid_ident = !name.is_empty()
        && !is_reserved
        && !name.as_bytes()[0].is_ascii_digit()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || !c.is_ascii());
    if is_valid_ident {
        name.to_string()
    } else {
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    }
}

/// Parses and canonically serializes a CSS Paged Media L3 `@page` selector list
/// (`<page-selector-list>`). Returns `None` if syntactically invalid.
#[must_use]
pub fn parse_and_serialize_page_selector(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(String::new());
    }
    let mut out = Vec::new();
    for part in trimmed.split(',') {
        let item = part.trim();
        if item.is_empty() || item.chars().any(char::is_whitespace) {
            return None;
        }
        let mut chunks = item.split(':');
        let page_name = chunks.next().unwrap_or("");
        if !page_name.is_empty() {
            let first = page_name.chars().next()?;
            if first.is_ascii_digit()
                || !page_name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || !c.is_ascii())
            {
                return None;
            }
        }
        let mut serialized = page_name.to_string();
        let mut saw_pseudo = false;
        for pseudo in chunks {
            saw_pseudo = true;
            let low = pseudo.to_ascii_lowercase();
            if !matches!(low.as_str(), "left" | "right" | "first" | "blank") {
                return None;
            }
            serialized.push(':');
            serialized.push_str(&low);
        }
        if page_name.is_empty() && !saw_pseudo {
            return None;
        }
        out.push(serialized);
    }
    Some(out.join(", "))
}

/// Parses and canonically serializes a CSS Animations L1 keyframe selector list
/// (`from` -> `0%`, `to` -> `100%`, comma-separated percentages). Returns `None` if invalid.
#[must_use]
pub fn parse_and_serialize_keyframe_selector(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for part in trimmed.split(',') {
        let p = part.trim();
        if p.is_empty() {
            return None;
        }
        if p.eq_ignore_ascii_case("from") {
            out.push("0%".to_string());
        } else if p.eq_ignore_ascii_case("to") {
            out.push("100%".to_string());
        } else {
            let num_str = p.strip_suffix('%')?;
            if num_str.is_empty() || num_str.chars().any(char::is_whitespace) {
                return None;
            }
            let val: f64 = num_str.parse().ok()?;
            if !val.is_finite() || !(0.0..=100.0).contains(&val) {
                return None;
            }
            let norm = if val == 0.0 { 0.0 } else { val };
            out.push(format!("{}%", crate::values_math::format_math_f64(norm)));
        }
    }
    Some(out.join(", "))
}

/// Parses a single `@keyframes` child rule (`<keyframe-selector> { <decls> }`) for
/// `CSSKeyframesRule.appendRule()`. Returns `None` if syntactically invalid or trailing garbage exists.
#[must_use]
pub fn parse_single_keyframe_rule(raw: &str) -> Option<CssomRuleKind> {
    let trimmed = raw.trim();
    let (raw_sel, raw_body) = split_single_style_rule(trimmed)?;
    let key_text = parse_and_serialize_keyframe_selector(raw_sel)?;
    let body_trimmed = raw_body.trim();
    let declarations_text = if body_trimmed.is_empty() {
        String::new()
    } else {
        let rewritten = crate::properties::rewrite_dynamic_var_names(body_trimmed);
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let opts = ParserOptions {
                error_recovery: true,
                ..ParserOptions::default()
            };
            lightningcss::declaration::DeclarationBlock::parse_string(&rewritten, opts)
                .map(|b| serialize_lightning_decl_block_cssom(&b, Some(body_trimmed)))
                .unwrap_or_else(|_| body_trimmed.to_string())
        }))
        .unwrap_or_else(|_| body_trimmed.to_string())
    };
    Some(CssomRuleKind::Keyframe {
        key_text,
        declarations_text,
    })
}

fn lightning_to_css_string<T: LightningToCss>(val: &T) -> String {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut out = String::new();
        let mut printer = Printer::new(&mut out, PrinterOptions::default());
        let _ = val.to_css(&mut printer);
        out
    }))
    .unwrap_or_default()
}

fn serialize_lightning_decl_block_cssom(
    block: &lightningcss::declaration::DeclarationBlock<'_>,
    raw_hint: Option<&str>,
) -> String {
    let mut parts = Vec::new();
    let mut push_prop = |prop: &lightningcss::properties::Property<'_>, important: bool| {
        let prop_id = prop.property_id();
        let prop_name = prop_id.name();
        if let Some(hint_block) = raw_hint {
            if let Some(val_hint) = find_raw_decl_value(hint_block, prop_name) {
                if let Some(val) = crate::cssom_decl::serialize_cssom_property(prop, val_hint) {
                    if important {
                        parts.push(format!("{prop_name}: {val} !important;"));
                    } else {
                        parts.push(format!("{prop_name}: {val};"));
                    }
                    return;
                }
            }
        }
        let raw_css = prop
            .to_css_string(important, PrinterOptions::default())
            .unwrap_or_default();
        let raw = restore_revert_rule_sentinel(&raw_css);
        let trimmed = raw.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            return;
        }
        // Normalize `prop:val` -> `prop: val` for custom properties emitted without space by lightningcss,
        // and canonicalize unitless `0` -> `0px` on length-valued properties.
        let formatted = if let Some((k, v)) = trimmed.split_once(':') {
            let k_t = k.trim();
            if k_t.starts_with("--") {
                if v.starts_with(' ') || v.is_empty() {
                    format!("{k_t}:{v}")
                } else {
                    format!("{k_t}: {v}")
                }
            } else if crate::cssom_decl::prop_name_uses_zero_px(k_t) {
                let v_t = v.trim();
                let (val_part, imp_part) = match v_t.rsplit_once('!') {
                    Some((before, after)) if after.trim().eq_ignore_ascii_case("important") => {
                        (before.trim(), " !important")
                    }
                    _ => (v_t, ""),
                };
                let norm = crate::cssom_decl::canonicalize_zero_lengths_pub(val_part);
                format!("{k_t}: {norm}{imp_part}")
            } else {
                trimmed.to_string()
            }
        } else {
            trimmed.to_string()
        };
        parts.push(format!("{formatted};"));
    };
    for prop in &block.declarations {
        push_prop(prop, false);
    }
    for prop in &block.important_declarations {
        push_prop(prop, true);
    }
    parts.join(" ")
}

/// Precomputed O(1) line-start byte index for a CSS source string so repeated
/// `(line, col) -> byte_offset` lookups across thousands of rules never degrade to O(N^2).
struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(css: &str) -> Self {
        let mut starts = Vec::with_capacity(64);
        starts.push(0);
        for (idx, &b) in css.as_bytes().iter().enumerate() {
            if b == b'\n' {
                starts.push(idx + 1);
            }
        }
        Self { starts }
    }

    #[inline]
    fn byte_offset(&self, css_len: usize, line: u32, col: u32) -> usize {
        let base = self.starts.get(line as usize).copied().unwrap_or(css_len);
        (base + col.saturating_sub(1) as usize).min(css_len)
    }
}

/// Wraps non-nested `@scope ... { ... }` blocks in `.__rix_top_scope__ { ... }` so
/// `lightningcss` parses bare declarations and relative selectors inside `@scope`
/// via its `NestedRuleParser`. Zero-allocation fast path when CSS has no `@scope`.
fn wrap_top_level_scope_rules(css: &str) -> std::borrow::Cow<'_, str> {
    if !css
        .as_bytes()
        .windows(6)
        .any(|w| w.eq_ignore_ascii_case(b"@scope"))
    {
        return std::borrow::Cow::Borrowed(css);
    }
    let bytes = css.as_bytes();
    let mut out = String::with_capacity(css.len() + 32);
    let (mut last, mut i, mut in_str) = (0usize, 0usize, None);
    let (mut block_is_style, mut stmt_is_at, mut stmt_started, mut wrapped_any) =
        (Vec::<bool>::new(), false, false, false);
    while i < bytes.len() {
        let was_in_str = in_str.is_some();
        if skip_css_ignored(bytes, &mut i, &mut in_str) {
            if !was_in_str && in_str.is_some() {
                stmt_started = true;
            }
            continue;
        }
        match bytes[i] {
            b'@' if !stmt_started => {
                stmt_started = true;
                stmt_is_at = true;
                let not_in_style = block_is_style.iter().all(|&s| !s);
                if not_in_style
                    && i + 6 <= bytes.len()
                    && bytes[i..i + 6].eq_ignore_ascii_case(b"@scope")
                    && (i + 6 == bytes.len()
                        || matches!(
                            bytes[i + 6],
                            b' ' | b'\t' | b'\n' | b'\r' | b'(' | b'{' | b'/'
                        ))
                {
                    if let Some(end) = find_at_rule_block_end(css, i) {
                        let slice = &css[i..end];
                        let has_layer_stmt = slice
                            .as_bytes()
                            .windows(6)
                            .any(|w| w.eq_ignore_ascii_case(b"@layer"));
                        if !has_layer_stmt {
                            out.push_str(&css[last..i]);
                            out.push_str(".__rix_top_scope__ {");
                            out.push_str(slice);
                            out.push('}');
                            last = end;
                            i = end;
                            stmt_started = false;
                            stmt_is_at = false;
                            wrapped_any = true;
                            continue;
                        }
                    }
                }
            }
            b'{' => {
                block_is_style.push(!stmt_is_at);
                stmt_started = false;
                stmt_is_at = false;
            }
            b'}' => {
                block_is_style.pop();
                stmt_started = false;
                stmt_is_at = false;
            }
            b';' => {
                stmt_started = false;
                stmt_is_at = false;
            }
            c if !c.is_ascii_whitespace() && !stmt_started => {
                stmt_started = true;
                stmt_is_at = false;
            }
            _ => {}
        }
        i += 1;
    }
    if !wrapped_any {
        return std::borrow::Cow::Borrowed(css);
    }
    out.push_str(&css[last..]);
    std::borrow::Cow::Owned(out)
}

fn find_at_rule_block_end(css: &str, start: usize) -> Option<usize> {
    let bytes = css.as_bytes();
    let (mut i, mut in_str, mut depth, mut saw_open) = (start, None, 0usize, false);
    while i < bytes.len() {
        if skip_css_ignored(bytes, &mut i, &mut in_str) {
            continue;
        }
        match bytes[i] {
            b'{' => {
                saw_open = true;
                depth += 1;
            }
            b'}' if saw_open => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            b';' if !saw_open => return None,
            _ => {}
        }
        i += 1;
    }
    saw_open.then_some(bytes.len())
}

fn extract_raw_selector_at_loc<'a>(
    css: &'a str,
    line_index: &LineIndex,
    line: u32,
    col: u32,
) -> Option<&'a str> {
    let rest = &css[line_index.byte_offset(css.len(), line, col)..];
    let bytes = rest.as_bytes();
    let (mut i, mut in_str, mut depth) = (0, None, 0usize);
    while i < bytes.len() {
        if skip_css_ignored(bytes, &mut i, &mut in_str) {
            continue;
        }
        match bytes[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b'{' if depth == 0 => return Some(rest[..i].trim()),
            b';' | b'}' if depth == 0 => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

fn strip_synthetic_scope_ampersands(lightning_sel: &str, raw_sel: Option<&str>) -> String {
    let l_parts = split_top_level_commas(lightning_sel);
    let raw_parts = raw_sel.map(split_top_level_commas);
    let mut out = Vec::with_capacity(l_parts.len());
    for (idx, lp) in l_parts.iter().enumerate() {
        let lt = lp.trim();
        let raw_has_leading_amp = raw_parts
            .as_ref()
            .and_then(|rp| rp.get(idx))
            .is_some_and(|r| r.trim().starts_with('&'));
        let stripped = if lt.starts_with('&') && !raw_has_leading_amp && raw_sel.is_some() {
            let after = lt[1..].trim_start();
            if after.is_empty() {
                lt
            } else {
                after
            }
        } else {
            lt
        };
        out.push(stripped);
    }
    out.join(", ")
}

fn extract_rule_decls_slice<'a>(
    css: &'a str,
    line_index: &LineIndex,
    line: u32,
    col: u32,
    is_nested_decls: bool,
) -> Option<&'a str> {
    let mut i = line_index.byte_offset(css.len(), line, col);
    let bytes = css.as_bytes();
    if !is_nested_decls {
        let (mut in_str, mut paren, mut open) = (None, 0usize, None);
        while i < bytes.len() {
            if skip_css_ignored(bytes, &mut i, &mut in_str) {
                continue;
            }
            match bytes[i] {
                b'(' | b'[' => paren += 1,
                b')' | b']' => paren = paren.saturating_sub(1),
                b'{' if paren == 0 => {
                    open = Some(i + 1);
                    break;
                }
                _ => {}
            }
            i += 1;
        }
        i = open?;
    }
    let (start, mut in_str, mut paren) = (i, None, 0usize);
    while i < bytes.len() {
        if skip_css_ignored(bytes, &mut i, &mut in_str) {
            continue;
        }
        match bytes[i] {
            b'(' | b'[' => paren += 1,
            b')' | b']' => paren = paren.saturating_sub(1),
            b'{' | b'}' if paren == 0 => return Some(&css[start..i]),
            _ => {}
        }
        i += 1;
    }
    Some(&css[start..])
}

fn find_raw_decl_value<'a>(decls_slice: &'a str, prop_name: &str) -> Option<&'a str> {
    let mut matched = None;
    for part in decls_slice.split(';') {
        if let Some((k, v)) = part.split_once(':') {
            if k.trim().eq_ignore_ascii_case(prop_name) {
                matched = Some(v.trim());
            }
        }
    }
    matched
}

fn extract_raw_import_supports(
    css: &str,
    line_index: &LineIndex,
    line: u32,
    col: u32,
) -> Option<String> {
    let rest = &css[line_index.byte_offset(css.len(), line, col)..];
    let stmt = &rest[..rest.find(';').unwrap_or(rest.len())];
    let bytes = stmt.as_bytes();
    let (mut i, mut in_str) = (0, None);
    while i + 9 <= bytes.len() {
        if skip_css_ignored(bytes, &mut i, &mut in_str) {
            continue;
        }
        if bytes[i..i + 9].eq_ignore_ascii_case(b"supports(")
            && (i == 0 || bytes[i - 1].is_ascii_whitespace() || bytes[i - 1] == b')')
        {
            if let Some(close) = find_matching_paren(&stmt[i + 8..]) {
                return Some(stmt[i + 9..i + 8 + close].to_string());
            }
        }
        i += 1;
    }
    None
}

fn extract_raw_at_rule_prelude<'a>(
    css: &'a str,
    line_index: &LineIndex,
    line: u32,
    col: u32,
) -> Option<&'a str> {
    let rest = &css[line_index.byte_offset(css.len(), line, col)..];
    let after_at = &rest[rest.find('@')? + 1..];
    let name_end = after_at
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(after_at.len());
    let prelude_start = &after_at[name_end..];
    let pb = prelude_start.as_bytes();
    let (mut i, mut in_str, mut paren_depth) = (0, None, 0usize);
    while i < pb.len() {
        if skip_css_ignored(pb, &mut i, &mut in_str) {
            continue;
        }
        match pb[i] {
            b'(' | b'[' => paren_depth += 1,
            b')' | b']' => paren_depth = paren_depth.saturating_sub(1),
            b'{' if paren_depth == 0 => return Some(prelude_start[..i].trim()),
            b';' | b'}' if paren_depth == 0 => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

fn find_matching_paren(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'(') {
        return None;
    }
    let (mut depth, mut in_str, mut i) = (0usize, None, 0);
    while i < bytes.len() {
        if skip_css_ignored(bytes, &mut i, &mut in_str) {
            continue;
        }
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn validate_raw_scope_prelude(
    raw_prelude: &str,
    namespaces: Option<&SelectorNamespaceMap>,
) -> Result<(Option<String>, Option<String>), String> {
    let mut rest = raw_prelude.trim();
    if rest.is_empty() {
        return Ok((None, None));
    }
    let start = if rest.starts_with('(') {
        let close = find_matching_paren(rest).ok_or_else(|| "Unclosed @scope (".to_string())?;
        let inner = rest[1..close].trim();
        if inner.is_empty() {
            return Err("Empty @scope () prelude".to_string());
        }
        let parsed = parse_selector_list_relative(inner, false, namespaces, ParseRelative::No)?;
        if parsed.slice().is_empty() || parsed.slice().iter().any(Selector::has_pseudo_element) {
            return Err("Invalid @scope start selector".to_string());
        }
        rest = rest[close + 1..].trim();
        Some(serialize_selector_list(&parsed))
    } else {
        None
    };

    if rest.is_empty() {
        return Ok((start, None));
    }

    // Must be `to (<selector-list>)` with nothing trailing.
    if rest.len() < 2 || !rest[..2].eq_ignore_ascii_case("to") {
        return Err(format!("Unexpected token in @scope prelude: '{rest}'"));
    }
    let after_to = &rest[2..];
    if !after_to.starts_with(|c: char| c.is_ascii_whitespace() || c == '(') {
        return Err(format!(
            "Expected whitespace or '(' after 'to' in @scope: '{rest}'"
        ));
    }
    let end_part = after_to.trim();
    if !end_part.starts_with('(') {
        return Err(format!("Expected '(' after 'to' in @scope: '{rest}'"));
    }
    let close = find_matching_paren(end_part).ok_or_else(|| "Unclosed @scope to (".to_string())?;
    if !end_part[close + 1..].trim().is_empty() {
        return Err("Trailing tokens in @scope prelude".to_string());
    }
    let inner = end_part[1..close].trim();
    if inner.is_empty() {
        return Err("Empty @scope to () prelude".to_string());
    }
    let parsed = parse_selector_list_relative(inner, false, namespaces, ParseRelative::ForScope)?;
    if parsed.slice().is_empty() || parsed.slice().iter().any(Selector::has_pseudo_element) {
        return Err("Invalid @scope end selector".to_string());
    }
    Ok((start, Some(serialize_selector_list(&parsed))))
}

fn convert_lightning_rule_list_to_cssom(
    list: &CssRuleList<'_>,
    namespaces: &mut SelectorNamespaceMap,
    nesting: CssomNestingContext,
    source_css: &str,
    line_index: &LineIndex,
) -> Vec<CssomRuleKind> {
    let mut out = Vec::new();
    for r in &list.0 {
        if let CssRule::Style(sr) = r {
            if lightning_to_css_string(&sr.selectors) == ".__rix_top_scope__" {
                out.extend(convert_lightning_rule_list_to_cssom(
                    &sr.rules, namespaces, nesting, source_css, line_index,
                ));
                continue;
            }
        }
        if let Ok(rule) =
            convert_lightning_rule_to_cssom(r, namespaces, nesting, source_css, line_index)
        {
            out.push(rule);
        }
    }
    out
}

fn is_disallowed_in_style_nesting(rule: &CssRule<'_>) -> bool {
    matches!(
        rule,
        CssRule::FontFace(_)
            | CssRule::Keyframes(_)
            | CssRule::Property(_)
            | CssRule::Import(_)
            | CssRule::Namespace(_)
            | CssRule::Page(_)
            | CssRule::CounterStyle(_)
            | CssRule::LayerStatement(_)
    )
}

fn convert_lightning_rule_to_cssom(
    rule: &CssRule<'_>,
    namespaces: &mut SelectorNamespaceMap,
    nesting: CssomNestingContext,
    source_css: &str,
    line_index: &LineIndex,
) -> Result<CssomRuleKind, CssomParseError> {
    if nesting == CssomNestingContext::InsideStyleRule && is_disallowed_in_style_nesting(rule) {
        return Err(CssomParseError::HierarchyRequestError(
            "Rule not allowed inside nested style rule".to_string(),
        ));
    }
    match rule {
        CssRule::Namespace(ns) => {
            let url = ns.url.as_ref().to_string();
            let prefix = ns.prefix.as_ref().map(|p| p.as_ref().to_string());
            let atom = CssAtom::new(&url);
            if let Some(ref p) = prefix {
                namespaces.prefixes.insert(p.clone(), atom);
            } else {
                namespaces.default_namespace = Some(atom);
            }
            Ok(CssomRuleKind::Namespace { prefix, url })
        }
        CssRule::Import(imp) => {
            let url = imp.url.as_ref().to_string();
            let layer = imp
                .layer
                .as_ref()
                .map(|inner| inner.as_ref().map(join_layer_name));
            let supports = imp.supports.as_ref().map(|sup| {
                if let Some(raw) = extract_raw_import_supports(
                    source_css,
                    line_index,
                    imp.loc.line,
                    imp.loc.column,
                ) {
                    return raw;
                }
                let s = lightning_to_css_string(sup);
                s.trim()
                    .strip_prefix('(')
                    .and_then(|t| t.strip_suffix(')'))
                    .filter(|_| matches!(sup, SupportsCondition::Declaration { .. }))
                    .unwrap_or(&s)
                    .to_string()
            });
            let media = if imp.media.media_queries.is_empty() {
                String::new()
            } else {
                lightning_to_css_string(&imp.media)
            };
            Ok(CssomRuleKind::Import {
                url,
                layer,
                supports,
                media,
            })
        }
        CssRule::Style(sr) => {
            let raw_sel = lightning_to_css_string(&sr.selectors);
            let ns_opt = (!namespaces.is_empty()).then_some(&*namespaces);
            let selector_text = match nesting {
                CssomNestingContext::TopLevel => {
                    let parsed = parse_selector_list_with_namespaces(&raw_sel, ns_opt)
                        .map_err(CssomParseError::SyntaxError)?;
                    serialize_selector_list(&parsed)
                }
                CssomNestingContext::InsideStyleRule => {
                    let raw_src = extract_raw_selector_at_loc(
                        source_css,
                        line_index,
                        sr.loc.line,
                        sr.loc.column,
                    );
                    let normalized = normalize_nested_selector_text(&raw_sel);
                    let parsed = parse_selector_list_relative(
                        &normalized,
                        true,
                        ns_opt,
                        ParseRelative::ForNesting,
                    )
                    .map_err(CssomParseError::SyntaxError)?;
                    let serialized = serialize_selector_list(&parsed);
                    if let Some(src) = raw_src {
                        if selector_has_ampersand(src) && !selector_has_ampersand(&serialized) {
                            src.to_string()
                        } else {
                            serialized
                        }
                    } else {
                        serialized
                    }
                }
                CssomNestingContext::InsideScopeRule => {
                    let raw_src = extract_raw_selector_at_loc(
                        source_css,
                        line_index,
                        sr.loc.line,
                        sr.loc.column,
                    );
                    let cleaned = strip_synthetic_scope_ampersands(&raw_sel, raw_src);
                    let parsed = parse_selector_list_relative(
                        &cleaned,
                        true,
                        ns_opt,
                        ParseRelative::ForScope,
                    )
                    .map_err(CssomParseError::SyntaxError)?;
                    serialize_selector_list(&parsed)
                }
            };
            let decl_hint =
                extract_rule_decls_slice(source_css, line_index, sr.loc.line, sr.loc.column, false);
            let declarations_text =
                serialize_lightning_decl_block_cssom(&sr.declarations, decl_hint);
            let rules = convert_lightning_rule_list_to_cssom(
                &sr.rules,
                namespaces,
                CssomNestingContext::InsideStyleRule,
                source_css,
                line_index,
            );
            Ok(CssomRuleKind::Style {
                selector_text,
                declarations_text,
                rules,
            })
        }
        CssRule::Nesting(nr) => convert_lightning_rule_to_cssom(
            &CssRule::Style(nr.style.clone()),
            namespaces,
            nesting,
            source_css,
            line_index,
        ),
        CssRule::NestedDeclarations(nd) => Ok(CssomRuleKind::NestedDeclarations {
            declarations_text: serialize_lightning_decl_block_cssom(
                &nd.declarations,
                extract_rule_decls_slice(source_css, line_index, nd.loc.line, nd.loc.column, true),
            ),
        }),
        CssRule::Media(mr) => Ok(CssomRuleKind::Media {
            condition_text: lightning_to_css_string(&mr.query),
            rules: convert_lightning_rule_list_to_cssom(
                &mr.rules, namespaces, nesting, source_css, line_index,
            ),
        }),
        CssRule::Supports(sr) => Ok(CssomRuleKind::Supports {
            condition_text: lightning_to_css_string(&sr.condition),
            rules: convert_lightning_rule_list_to_cssom(
                &sr.rules, namespaces, nesting, source_css, line_index,
            ),
        }),
        CssRule::Container(cr) => {
            let container_name = cr.name.as_ref().map(lightning_to_css_string);
            let container_query = lightning_to_css_string(&cr.condition);
            let condition_text = match &container_name {
                Some(name) => format!("{name} {container_query}"),
                None => container_query.clone(),
            };
            Ok(CssomRuleKind::Container {
                container_name,
                container_query,
                condition_text,
                rules: convert_lightning_rule_list_to_cssom(
                    &cr.rules, namespaces, nesting, source_css, line_index,
                ),
            })
        }
        CssRule::LayerBlock(lb) => Ok(CssomRuleKind::LayerBlock {
            name: lb.name.as_ref().map(join_layer_name).unwrap_or_default(),
            rules: convert_lightning_rule_list_to_cssom(
                &lb.rules, namespaces, nesting, source_css, line_index,
            ),
        }),
        CssRule::LayerStatement(ls) => Ok(CssomRuleKind::LayerStatement {
            names: ls.names.iter().map(join_layer_name).collect(),
        }),
        CssRule::Scope(sc) => {
            let ns_opt = (!namespaces.is_empty()).then_some(&*namespaces);
            let (start, end) = if let Some(raw_prelude) =
                extract_raw_at_rule_prelude(source_css, line_index, sc.loc.line, sc.loc.column)
            {
                validate_raw_scope_prelude(raw_prelude, ns_opt)
                    .map_err(CssomParseError::SyntaxError)?
            } else {
                let start = match &sc.scope_start {
                    Some(s) => {
                        let raw = lightning_to_css_string(s);
                        let p = parse_selector_list_non_forgiving(&raw)
                            .map_err(CssomParseError::SyntaxError)?;
                        if p.slice().is_empty()
                            || p.slice().iter().any(Selector::has_pseudo_element)
                        {
                            return Err(CssomParseError::SyntaxError(
                                "Invalid @scope start".to_string(),
                            ));
                        }
                        Some(serialize_selector_list(&p))
                    }
                    None => None,
                };
                let end = match &sc.scope_end {
                    Some(e) => {
                        let raw = lightning_to_css_string(e);
                        let p = parse_selector_list_relative(
                            &raw,
                            false,
                            ns_opt,
                            ParseRelative::ForScope,
                        )
                        .map_err(CssomParseError::SyntaxError)?;
                        if p.slice().is_empty()
                            || p.slice().iter().any(Selector::has_pseudo_element)
                        {
                            return Err(CssomParseError::SyntaxError(
                                "Invalid @scope end".to_string(),
                            ));
                        }
                        Some(serialize_selector_list(&p))
                    }
                    None => None,
                };
                (start, end)
            };
            Ok(CssomRuleKind::Scope {
                start,
                end,
                rules: convert_lightning_rule_list_to_cssom(
                    &sc.rules,
                    namespaces,
                    CssomNestingContext::InsideScopeRule,
                    source_css,
                    line_index,
                ),
            })
        }
        CssRule::FontFace(ff) => {
            let full = lightning_to_css_string(ff);
            let body = full
                .find('{')
                .and_then(|s| full.rfind('}').map(|e| &full[s + 1..e]))
                .unwrap_or("");
            let declarations_text = body.split_whitespace().collect::<Vec<_>>().join(" ");
            Ok(CssomRuleKind::FontFace { declarations_text })
        }
        CssRule::Page(pr) => {
            let raw_prelude =
                extract_raw_at_rule_prelude(source_css, line_index, pr.loc.line, pr.loc.column)
                    .unwrap_or("");
            let selector_text =
                parse_and_serialize_page_selector(raw_prelude).unwrap_or_else(|| {
                    pr.selectors
                        .iter()
                        .map(lightning_to_css_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                });
            let decl_hint =
                extract_rule_decls_slice(source_css, line_index, pr.loc.line, pr.loc.column, false);
            let declarations_text = serialize_page_decl_block(&pr.declarations, decl_hint);
            Ok(CssomRuleKind::Page {
                selector_text,
                declarations_text,
                rules: Vec::new(),
            })
        }
        CssRule::Keyframes(kf) => {
            let name = match &kf.name {
                KeyframesName::Ident(id) => id.as_ref().to_string(),
                KeyframesName::Custom(s) => {
                    if s.as_ref().is_empty() {
                        return Err(CssomParseError::SyntaxError(
                            "Empty @keyframes string name is invalid".to_string(),
                        ));
                    }
                    s.as_ref().to_string()
                }
            };
            let mut rules = Vec::with_capacity(kf.keyframes.len());
            for frame in &kf.keyframes {
                let key_text = frame
                    .selectors
                    .iter()
                    .map(|sel| match sel {
                        KeyframeSelector::From => "0%".to_string(),
                        KeyframeSelector::To => "100%".to_string(),
                        KeyframeSelector::Percentage(p) => {
                            let pct = (f64::from(p.0) * 100.0 * 1e6).round() / 1e6;
                            format!("{}%", crate::values_math::format_math_f64(pct))
                        }
                        KeyframeSelector::TimelineRangePercentage(_) => {
                            lightning_to_css_string(sel)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut filtered_block = frame.declarations.clone();
                let keep_kf_prop = |prop: &lightningcss::properties::Property<'_>| {
                    let pid = prop.property_id();
                    let n = pid.name();
                    !n.eq_ignore_ascii_case("animation")
                        && (!n.to_ascii_lowercase().starts_with("animation-")
                            || n.eq_ignore_ascii_case("animation-timing-function")
                            || n.eq_ignore_ascii_case("animation-composition"))
                };
                filtered_block.declarations.retain(keep_kf_prop);
                filtered_block.important_declarations.retain(keep_kf_prop);
                let declarations_text = serialize_lightning_decl_block_cssom(&filtered_block, None);
                rules.push(CssomRuleKind::Keyframe {
                    key_text,
                    declarations_text,
                });
            }
            Ok(CssomRuleKind::Keyframes { name, rules })
        }
        CssRule::Property(pr) => Ok(CssomRuleKind::Property {
            name: pr.name.as_ref().to_string(),
            css_text: lightning_to_css_string(pr),
        }),
        CssRule::CounterStyle(cs) => {
            let collapsed = lightning_to_css_string(cs)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            Ok(CssomRuleKind::Other {
                css_text: collapsed,
            })
        }
        CssRule::Ignored | CssRule::Unknown(_) => {
            Err(CssomParseError::SyntaxError("Ignored CSS rule".to_string()))
        }
        other => {
            let mut css_text = lightning_to_css_string(other);
            if css_text == "@starting-style {\n  \n}" {
                css_text = "@starting-style {\n}".to_string();
            }
            Ok(CssomRuleKind::Other { css_text })
        }
    }
}

fn serialize_page_decl_block(
    block: &lightningcss::declaration::DeclarationBlock<'_>,
    raw_hint: Option<&str>,
) -> String {
    let mut parts = Vec::new();
    if let Some(raw) = raw_hint {
        for piece in raw.split(';') {
            let p = piece.trim();
            if p.is_empty() {
                continue;
            }
            if let Some((k, v)) = p.split_once(':') {
                let key = k.trim().to_ascii_lowercase();
                let val = v.trim();
                if let Some(ser) = validate_and_serialize_page_descriptor(&key, val) {
                    if key == "margin" {
                        if let Some(exp) =
                            crate::cssom_decl::parse_and_expand_declaration("margin", val)
                        {
                            for (lk, lv) in exp {
                                parts.push(format!("{lk}: {lv};"));
                            }
                            continue;
                        }
                    }
                    parts.push(format!("{key}: {ser};"));
                }
            }
        }
        if !parts.is_empty() {
            return parts.join(" ");
        }
    }
    serialize_lightning_decl_block_cssom(block, raw_hint)
}

/// Validates and canonically serializes a CSS Paged Media L3 `@page` descriptor
/// (`size`, `page-orientation`, `margin`, `margin-top/right/bottom/left`).
#[must_use]
pub fn validate_and_serialize_page_descriptor(name: &str, value: &str) -> Option<String> {
    let key = name.trim().to_ascii_lowercase();
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    match key.as_str() {
        "page-orientation" => {
            matches!(lower.as_str(), "upright" | "rotate-left" | "rotate-right").then_some(lower)
        }
        "size" => {
            if matches!(lower.as_str(), "auto" | "portrait" | "landscape") {
                return Some(lower);
            }
            let toks: Vec<&str> = lower.split_ascii_whitespace().collect();
            if toks.is_empty() || toks.len() > 2 {
                return None;
            }
            const PAGE_SIZES: &[&str] = &[
                "a5", "a4", "a3", "b5", "b4", "jis-b5", "jis-b4", "letter", "legal", "ledger",
            ];
            let is_orient = |s: &str| matches!(s, "portrait" | "landscape");
            let is_page_size = |s: &str| PAGE_SIZES.contains(&s);
            let is_len = |s: &str| {
                crate::cssom_decl::parse_and_serialize_declaration("width", s)
                    .is_some_and(|v| !matches!(v.as_str(), "auto" | "min-content" | "max-content"))
            };
            if toks.len() == 1 {
                if is_page_size(toks[0]) {
                    return Some(toks[0].to_string());
                }
                if is_len(toks[0]) {
                    return crate::cssom_decl::parse_and_serialize_declaration("width", toks[0]);
                }
                return None;
            }
            if is_page_size(toks[0]) && is_orient(toks[1]) {
                return Some(format!("{} {}", toks[0], toks[1]));
            }
            if is_orient(toks[0]) && is_page_size(toks[1]) {
                return Some(format!("{} {}", toks[1], toks[0]));
            }
            if is_len(toks[0]) && is_len(toks[1]) {
                let a = crate::cssom_decl::parse_and_serialize_declaration("width", toks[0])?;
                let b = crate::cssom_decl::parse_and_serialize_declaration("width", toks[1])?;
                return Some(if a == b { a } else { format!("{a} {b}") });
            }
            None
        }
        "margin" | "margin-top" | "margin-right" | "margin-bottom" | "margin-left" => {
            crate::cssom_decl::parse_and_serialize_declaration(&key, trimmed)
        }
        _ => None,
    }
}

/// Parses a CSS stylesheet into top-level [`CssomRuleKind`] items (error-recovering).
#[must_use]
pub fn parse_cssom_rules(css: &str) -> Vec<CssomRuleKind> {
    let trimmed = css.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    // Fast path for a single top-level style rule without nested rules so canonical selector
    // serialization (`:nth-child(n)`, `:is(:is(div))`, etc.) is not simplified by `lightningcss`.
    if !trimmed.starts_with('@') && split_single_style_rule(trimmed).is_some() {
        return match parse_single_cssom_rule(trimmed) {
            Ok(rule) => vec![rule],
            Err(_) => Vec::new(),
        };
    }
    let rewritten = crate::properties::rewrite_dynamic_var_names(css);
    let wrapped = wrap_top_level_scope_rules(&rewritten);
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let opts = ParserOptions {
            error_recovery: true,
            ..ParserOptions::default()
        };
        let Ok(parsed) = StyleSheet::parse(&wrapped, opts) else {
            return Vec::new();
        };
        let mut namespaces = SelectorNamespaceMap::default();
        let line_index = LineIndex::new(&wrapped);
        convert_lightning_rule_list_to_cssom(
            &parsed.rules,
            &mut namespaces,
            CssomNestingContext::TopLevel,
            &wrapped,
            &line_index,
        )
    }))
    .unwrap_or_default()
}

/// Strictly parses a single CSS rule for `CSSStyleSheet.insertRule(rule, index)`.
pub fn parse_single_cssom_rule(rule_css: &str) -> Result<CssomRuleKind, String> {
    parse_single_cssom_rule_with_namespaces(rule_css, None)
}

/// Strictly parses a single CSS rule with optional active stylesheet `@namespace` declarations.
pub fn parse_single_cssom_rule_with_namespaces(
    rule_css: &str,
    namespaces: Option<&SelectorNamespaceMap>,
) -> Result<CssomRuleKind, String> {
    parse_single_cssom_rule_with_context(rule_css, namespaces, CssomNestingContext::TopLevel)
        .map_err(|e| match e {
            CssomParseError::SyntaxError(s) | CssomParseError::HierarchyRequestError(s) => s,
        })
}

/// Strictly parses a single CSS rule inside `nesting` (`TopLevel`, `InsideStyleRule`, `InsideScopeRule`),
/// returning [`CssomParseError::HierarchyRequestError`] for rules disallowed inside nesting.
pub fn parse_single_cssom_rule_with_context(
    rule_css: &str,
    namespaces: Option<&SelectorNamespaceMap>,
    nesting: CssomNestingContext,
) -> Result<CssomRuleKind, CssomParseError> {
    let trimmed = rule_css.trim();
    if trimmed.is_empty() {
        return Err(CssomParseError::SyntaxError("Empty CSS rule".to_string()));
    }
    // Inside a nesting context, bare declarations without `{` (e.g. `rule.insertRule('z-index: 3;')`)
    // parse into a `CSSNestedDeclarations` rule (CSSWG #10520: throw SyntaxError if no valid decl).
    if nesting != CssomNestingContext::TopLevel
        && !trimmed.starts_with('@')
        && !trimmed.contains('{')
    {
        let mut valid_parts = Vec::new();
        for piece in trimmed.split(';') {
            let p = piece.trim();
            if p.is_empty() {
                continue;
            }
            if let Some((k, v)) = p.split_once(':') {
                let k_t = k.trim();
                let v_t = v.trim();
                let (val_no_imp, imp) = match v_t.rsplit_once('!') {
                    Some((before, after)) if after.trim().eq_ignore_ascii_case("important") => {
                        (before.trim(), true)
                    }
                    _ => (v_t, false),
                };
                if let Some(canon) =
                    crate::cssom_decl::parse_and_serialize_declaration(k_t, val_no_imp)
                {
                    let key_out = if k_t.starts_with("--") {
                        k_t.to_string()
                    } else {
                        k_t.to_ascii_lowercase()
                    };
                    if imp {
                        valid_parts.push(format!("{key_out}: {canon} !important;"));
                    } else {
                        valid_parts.push(format!("{key_out}: {canon};"));
                    }
                }
            }
        }
        if valid_parts.is_empty() {
            return Err(CssomParseError::SyntaxError(format!(
                "No valid declarations in nested insertRule('{trimmed}')"
            )));
        }
        return Ok(CssomRuleKind::NestedDeclarations {
            declarations_text: valid_parts.join(" "),
        });
    }

    // Fast path for a single flat style rule without nested `{`: parse via Servo `selectors`
    // directly so canonical forms (`:is(:is(div))`, `> .foo`, etc.) are preserved verbatim.
    if !trimmed.starts_with('@') {
        if let Some((raw_sel, raw_body)) = split_single_style_rule(trimmed) {
            let selector_text = match nesting {
                CssomNestingContext::TopLevel => {
                    let parsed_sel = if let Some(ns) = namespaces {
                        parse_selector_list_with_namespaces(raw_sel, Some(ns))
                    } else {
                        parse_selector_list_strict(raw_sel)
                    }
                    .map_err(CssomParseError::SyntaxError)?;
                    serialize_selector_list(&parsed_sel)
                }
                CssomNestingContext::InsideStyleRule => {
                    let normalized = normalize_nested_selector_text(raw_sel);
                    let parsed_sel = parse_selector_list_relative(
                        &normalized,
                        true,
                        namespaces,
                        ParseRelative::ForNesting,
                    )
                    .map_err(CssomParseError::SyntaxError)?;
                    serialize_selector_list(&parsed_sel)
                }
                CssomNestingContext::InsideScopeRule => {
                    let parsed_sel = parse_selector_list_relative(
                        raw_sel,
                        true,
                        namespaces,
                        ParseRelative::ForScope,
                    )
                    .map_err(CssomParseError::SyntaxError)?;
                    serialize_selector_list(&parsed_sel)
                }
            };
            let body_trimmed = raw_body.trim();
            let declarations_text = if body_trimmed.is_empty() {
                String::new()
            } else {
                let rewritten_body = crate::properties::rewrite_dynamic_var_names(body_trimmed);
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let opts = ParserOptions {
                        error_recovery: true,
                        ..ParserOptions::default()
                    };
                    lightningcss::declaration::DeclarationBlock::parse_string(&rewritten_body, opts)
                        .map(|b| serialize_lightning_decl_block_cssom(&b, Some(body_trimmed)))
                        .unwrap_or_else(|_| body_trimmed.to_string())
                }))
                .unwrap_or_else(|_| body_trimmed.to_string())
            };
            return Ok(CssomRuleKind::Style {
                selector_text,
                declarations_text,
                rules: Vec::new(),
            });
        }
    }

    // Handle `@media {}` (empty media query list) which lightningcss rejects in strict mode.
    if trimmed.len() >= 6 && trimmed[..6].eq_ignore_ascii_case("@media") {
        if let Some((prelude, body)) = split_single_style_rule(trimmed) {
            if prelude[6..].trim().is_empty() {
                let rules = if body.trim().is_empty() {
                    Vec::new()
                } else {
                    parse_cssom_rules(body)
                };
                return Ok(CssomRuleKind::Media {
                    condition_text: String::new(),
                    rules,
                });
            }
        }
    }

    // Handle `@import` rules with `supports(...)` or invalid `layer(...)` fallback per CSS Cascade L5.
    if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("@import") {
        if let Some(imp_rule) = parse_cssom_import_rule(trimmed) {
            return match nesting {
                CssomNestingContext::InsideStyleRule => Err(
                    CssomParseError::HierarchyRequestError("@import not allowed in nesting".into()),
                ),
                _ => Ok(imp_rule),
            };
        }
    }

    let rewritten = crate::properties::rewrite_dynamic_var_names(trimmed);
    // CSSOM §6.4.4 / CSS Nesting L1: valid top-level-only at-rules throw HierarchyRequestError
    // inside style nesting rather than SyntaxError from lightningcss's NestedRuleParser.
    if nesting == CssomNestingContext::InsideStyleRule && trimmed.starts_with('@') {
        let at_name = trimmed[1..]
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(
            at_name.as_str(),
            "font-face"
                | "keyframes"
                | "property"
                | "import"
                | "namespace"
                | "charset"
                | "page"
                | "counter-style"
        ) {
            let ok_top = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let opts = ParserOptions {
                    error_recovery: false,
                    ..ParserOptions::default()
                };
                StyleSheet::parse(&rewritten, opts).is_ok_and(|s| s.rules.0.len() == 1)
            }))
            .unwrap_or(false);
            if ok_top {
                return Err(CssomParseError::HierarchyRequestError(format!(
                    "@{at_name} is not allowed inside a nested style rule"
                )));
            }
        }
    }

    let is_top_scope = nesting == CssomNestingContext::TopLevel
        && trimmed.len() >= 6
        && trimmed[..6].eq_ignore_ascii_case("@scope");
    let wrapped_for_parse = if nesting != CssomNestingContext::TopLevel || is_top_scope {
        Some(format!(".__rix_nest_wrap__ {{ {rewritten} }}"))
    } else {
        None
    };

    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let opts = ParserOptions {
            error_recovery: false,
            ..ParserOptions::default()
        };
        if let Some(ref wrapped) = wrapped_for_parse {
            let parsed = StyleSheet::parse(wrapped, opts).map_err(|e| {
                CssomParseError::SyntaxError(format!("Failed to parse CSS rule '{trimmed}': {e}"))
            })?;
            let Some(CssRule::Style(outer)) = parsed.rules.0.first() else {
                return Err(CssomParseError::SyntaxError(format!(
                    "Expected 1 nested CSS rule in '{trimmed}'"
                )));
            };
            if outer.rules.0.len() != 1 {
                return Err(CssomParseError::SyntaxError(format!(
                    "Expected exactly 1 CSS rule, got {}",
                    outer.rules.0.len()
                )));
            }
            let mut ns = namespaces.cloned().unwrap_or_default();
            let li = LineIndex::new(wrapped);
            return convert_lightning_rule_to_cssom(
                &outer.rules.0[0],
                &mut ns,
                nesting,
                wrapped,
                &li,
            );
        }
        let parsed = StyleSheet::parse(&rewritten, opts).map_err(|e| {
            CssomParseError::SyntaxError(format!("Failed to parse CSS rule '{trimmed}': {e}"))
        })?;
        if parsed.rules.0.len() != 1 {
            return Err(CssomParseError::SyntaxError(format!(
                "Expected exactly 1 CSS rule, got {}",
                parsed.rules.0.len()
            )));
        }
        let mut ns = namespaces.cloned().unwrap_or_default();
        let li = LineIndex::new(&rewritten);
        convert_lightning_rule_to_cssom(&parsed.rules.0[0], &mut ns, nesting, &rewritten, &li)
    }))
    .unwrap_or_else(|_| {
        Err(CssomParseError::SyntaxError(format!(
            "Internal CSS parser rejected rule '{trimmed}'"
        )))
    })
}

fn parse_cssom_import_rule(trimmed: &str) -> Option<CssomRuleKind> {
    let opts = ParserOptions {
        error_recovery: false,
        ..ParserOptions::default()
    };
    if let Ok(parsed) = StyleSheet::parse(trimmed, opts) {
        if parsed.rules.0.len() == 1 {
            if let CssRule::Import(imp) = &parsed.rules.0[0] {
                let mut ns = SelectorNamespaceMap::default();
                let li = LineIndex::new(trimmed);
                return convert_lightning_rule_to_cssom(
                    &CssRule::Import(imp.clone()),
                    &mut ns,
                    CssomNestingContext::TopLevel,
                    trimmed,
                    &li,
                )
                .ok();
            }
        }
    }
    // Fallback for `@import` with CSS Conditional L4/L5 `supports(...)` (`font-tech()`,
    // `font-format()`, `at-rule()`, `!important`) or invalid `layer(...)` media fallback.
    let body = trimmed
        .strip_prefix("@import")
        .or_else(|| trimmed.strip_prefix("@IMPORT"))?
        .trim()
        .strip_suffix(';')?
        .trim();
    let (url, mut rest) = if let Some(u) = body.strip_prefix("url(") {
        let close = u.find(')')?;
        let raw_u = u[..close].trim().trim_matches(|c| c == '"' || c == '\'');
        (raw_u.to_string(), u[close + 1..].trim())
    } else if body.starts_with('"') || body.starts_with('\'') {
        let q = body.as_bytes()[0] as char;
        let close = body[1..].find(q)?;
        (body[1..1 + close].to_string(), body[close + 2..].trim())
    } else {
        return None;
    };
    let mut layer: Option<Option<String>> = None;
    let mut supports: Option<String> = None;
    if rest.len() >= 6 && rest[..6].eq_ignore_ascii_case("layer(") {
        if let Some(close) = find_matching_paren(&rest[5..]) {
            let name = rest[6..5 + close].trim();
            if !name.is_empty() && !name.contains(' ') {
                layer = Some(Some(name.to_string()));
                rest = rest[6 + close..].trim();
            }
        }
    } else if rest.len() >= 5
        && rest[..5].eq_ignore_ascii_case("layer")
        && (rest.len() == 5 || rest.as_bytes()[5].is_ascii_whitespace())
    {
        layer = Some(None);
        rest = rest[5..].trim();
    }
    if rest.len() >= 9 && rest[..9].eq_ignore_ascii_case("supports(") {
        let close = find_matching_paren(&rest[8..])?;
        let inner = &rest[9..8 + close];
        if inner.trim().is_empty() {
            return None;
        }
        supports = Some(inner.to_string());
        rest = rest[9 + close..].trim();
    }
    if layer.is_none() && supports.is_none() && rest.is_empty() {
        return None;
    }
    Some(CssomRuleKind::Import {
        url,
        layer,
        supports,
        media: rest.to_string(),
    })
}

/// Splits a single top-level CSS style rule `<selector> { <decls> }` if balanced and containing
/// no trailing tokens or nested rules (ignoring `{`/`}` inside strings and comments).
fn split_single_style_rule(input: &str) -> Option<(&str, &str)> {
    let bytes = input.as_bytes();
    let (mut i, mut in_str, mut paren_depth, mut open_brace) = (0, None, 0usize, None);
    while i < bytes.len() {
        if skip_css_ignored(bytes, &mut i, &mut in_str) {
            continue;
        }
        match bytes[i] {
            b'(' | b'[' => paren_depth += 1,
            b')' | b']' => paren_depth = paren_depth.saturating_sub(1),
            b'{' if paren_depth == 0 => {
                open_brace = Some(i);
                break;
            }
            _ => {}
        }
        i += 1;
    }
    let start_brace = open_brace?;
    let (mut depth, mut has_nested_brace, mut j, mut in_str) = (0usize, false, start_brace, None);
    while j < bytes.len() {
        if skip_css_ignored(bytes, &mut j, &mut in_str) {
            continue;
        }
        match bytes[j] {
            b'{' => {
                has_nested_brace |= depth >= 1;
                depth += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if !input[j + 1..].trim().is_empty() || has_nested_brace {
                        return None;
                    }
                    return Some((input[..start_brace].trim(), &input[start_brace + 1..j]));
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}
