//! Typed CSS stylesheet compiler supporting `@media`, `@supports`, `@layer`,
//! `@font-face`, `@keyframes`, and CSS Nesting flattening.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use lightningcss::media_query::{
    MediaCondition, MediaFeatureId, MediaFeatureName, MediaFeatureValue, MediaList, MediaQuery,
    MediaType, Operator, Qualifier, QueryFeature,
};
use lightningcss::printer::Printer;
use lightningcss::properties::{Property, PropertyId};
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
use selectors::parser::{AncestorHashes, Selector};

use crate::properties::{
    compile_declaration_block, MissingCssFeature, MissingCssKind, PropertyIdKind,
    SpecifiedDeclaration,
};
use crate::selector::{compile_lightning_selector_list, parse_selector_list, RixSelectorImpl};
use crate::values::{convert_font_family_name, convert_length, EvalContext};

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
            container_width_px: self.viewport_width,
            container_height_px: self.viewport_height,
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
            let name = property_id.name();
            if PropertyIdKind::from_name(name).is_none() {
                return false;
            }
            if name.starts_with("--") {
                return true;
            }
            let opts = ParserOptions::default();
            match Property::parse_string(PropertyId::from(name), value.as_ref(), opts) {
                Ok(Property::Unparsed(_)) => value.contains("var("),
                Ok(_) => true,
                Err(_) => false,
            }
        }
        SupportsCondition::Selector(sel) => parse_selector_list(sel.as_ref()).is_some(),
        SupportsCondition::Unknown(_) => false,
    }
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

/// Compiled `@font-face` descriptor rule.
#[derive(Debug, Clone, PartialEq)]
pub struct FontFaceRule {
    pub family: Option<InternedStr>,
    pub sources: Vec<FontFaceSource>,
    pub weight_range: (f32, f32),
    pub italic: bool,
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
}

/// Compiled CSS stylesheet with flattened nesting, evaluated conditions,
/// registered `@layer` ordering, `@font-face`, `@keyframes`, and deduplicated telemetry.
#[derive(Debug, Clone)]
pub struct Stylesheet {
    pub origin: CascadeOrigin,
    pub rules: Vec<CompiledRule>,
    pub layers: Vec<InternedStr>,
    pub font_faces: Vec<FontFaceRule>,
    pub keyframes: Vec<KeyframesRule>,
    pub missing_features: Vec<MissingCssFeature>,
}

impl Stylesheet {
    /// Parses and compiles a CSS stylesheet against the given origin and media environment.
    #[must_use]
    pub fn parse(css: &str, origin: CascadeOrigin, media: &MediaContext) -> Self {
        let opts = ParserOptions {
            error_recovery: true,
            ..ParserOptions::default()
        };
        let mut compiler = StylesheetCompiler::new(origin, *media);
        if let Ok(sheet) = StyleSheet::parse(css, opts) {
            compiler.compile_rule_list(&sheet.rules, None, LayerId::UNLAYERED, None);
        }
        compiler.finish()
    }
}

struct StylesheetCompiler {
    origin: CascadeOrigin,
    media: MediaContext,
    rules: Vec<CompiledRule>,
    layers: Vec<InternedStr>,
    layer_map: HashMap<InternedStr, LayerId>,
    anon_layer_counter: u32,
    font_faces: Vec<FontFaceRule>,
    keyframes: Vec<KeyframesRule>,
    missing_seen: HashSet<MissingCssFeature>,
    missing_features: Vec<MissingCssFeature>,
    next_source_order: u32,
    selector_scratch: String,
}

impl StylesheetCompiler {
    fn new(origin: CascadeOrigin, media: MediaContext) -> Self {
        Self {
            origin,
            media,
            rules: Vec::new(),
            layers: Vec::new(),
            layer_map: HashMap::new(),
            anon_layer_counter: 0,
            font_faces: Vec::new(),
            keyframes: Vec::new(),
            missing_seen: HashSet::new(),
            missing_features: Vec::new(),
            next_source_order: 0,
            selector_scratch: String::with_capacity(128),
        }
    }

    fn record_missing(&mut self, feat: MissingCssFeature) {
        if self.missing_seen.insert(feat.clone()) {
            self.missing_features.push(feat);
        }
    }

    fn register_layer(&mut self, prefix: Option<&str>, name: Option<&LayerName<'_>>) -> LayerId {
        let full = match (prefix, name) {
            (Some(p), Some(n)) => format!("{p}.{}", join_layer_name(n)),
            (None, Some(n)) => join_layer_name(n),
            (Some(p), None) => {
                let id = self.anon_layer_counter;
                self.anon_layer_counter += 1;
                format!("{p}.__anon_{id}")
            }
            (None, None) => {
                let id = self.anon_layer_counter;
                self.anon_layer_counter += 1;
                format!("__anon_{id}")
            }
        };
        let interned = intern_str(&full);
        if let Some(&id) = self.layer_map.get(&interned) {
            return id;
        }
        let id = LayerId(self.layers.len() as u32);
        self.layers.push(interned.clone());
        self.layer_map.insert(interned, id);
        id
    }

    fn compile_rule_list(
        &mut self,
        list: &CssRuleList<'_>,
        parent_selector: Option<&str>,
        current_layer: LayerId,
        layer_prefix: Option<&str>,
    ) {
        for rule in &list.0 {
            match rule {
                CssRule::Style(style) => {
                    self.compile_style_rule(style, parent_selector, current_layer, layer_prefix);
                }
                CssRule::Nesting(nest) => {
                    self.compile_style_rule(
                        &nest.style,
                        parent_selector,
                        current_layer,
                        layer_prefix,
                    );
                }
                CssRule::NestedDeclarations(nested) => {
                    if let Some(parent_sel) = parent_selector {
                        self.emit_rules_for_selector_str(
                            parent_sel,
                            &nested.declarations,
                            current_layer,
                        );
                    }
                }
                CssRule::Media(media_rule) => {
                    if self.media.matches_media_list(&media_rule.query) {
                        self.compile_rule_list(
                            &media_rule.rules,
                            parent_selector,
                            current_layer,
                            layer_prefix,
                        );
                    }
                }
                CssRule::Supports(sup_rule) => {
                    if evaluate_supports(&sup_rule.condition) {
                        self.compile_rule_list(
                            &sup_rule.rules,
                            parent_selector,
                            current_layer,
                            layer_prefix,
                        );
                    }
                }
                CssRule::LayerStatement(stmt) => {
                    for name in &stmt.names {
                        let _ = self.register_layer(layer_prefix, Some(name));
                    }
                }
                CssRule::LayerBlock(block) => {
                    let layer_id = self.register_layer(layer_prefix, block.name.as_ref());
                    let next_prefix = self.layers[layer_id.0 as usize].as_str().to_owned();
                    self.compile_rule_list(
                        &block.rules,
                        parent_selector,
                        layer_id,
                        Some(&next_prefix),
                    );
                }
                CssRule::FontFace(ff) => self.compile_font_face(ff),
                CssRule::Keyframes(kf) => self.compile_keyframes(kf),
                CssRule::Import(_) | CssRule::Namespace(_) | CssRule::Ignored => {}
                CssRule::Container(_) => {
                    self.record_missing(MissingCssFeature::new(
                        MissingCssKind::AtRule,
                        "@container",
                    ));
                }
                CssRule::Scope(_) => {
                    self.record_missing(MissingCssFeature::new(MissingCssKind::AtRule, "@scope"));
                }
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

    fn compile_style_rule(
        &mut self,
        style: &lightningcss::rules::style::StyleRule<'_>,
        parent_selector: Option<&str>,
        layer: LayerId,
        layer_prefix: Option<&str>,
    ) {
        let resolved_selector = if let Some(parent) = parent_selector {
            let mut raw = String::new();
            {
                let mut printer = Printer::new(&mut raw, PrinterOptions::default());
                if style.selectors.to_css(&mut printer).is_err() {
                    return;
                }
            }
            flatten_nested_selector(parent, &raw)
        } else {
            let mut raw = String::new();
            {
                let mut printer = Printer::new(&mut raw, PrinterOptions::default());
                if style.selectors.to_css(&mut printer).is_err() {
                    return;
                }
            }
            raw
        };

        if !style.declarations.declarations.is_empty()
            || !style.declarations.important_declarations.is_empty()
        {
            if parent_selector.is_none() {
                if let Some(parsed) =
                    compile_lightning_selector_list(&style.selectors, &mut self.selector_scratch)
                {
                    self.emit_compiled_selectors(parsed.slice(), &style.declarations, layer);
                } else {
                    self.record_missing(MissingCssFeature::new(
                        MissingCssKind::Selector,
                        &resolved_selector,
                    ));
                }
            } else {
                self.emit_rules_for_selector_str(&resolved_selector, &style.declarations, layer);
            }
        }

        if !style.rules.0.is_empty() {
            self.compile_rule_list(&style.rules, Some(&resolved_selector), layer, layer_prefix);
        }
    }

    fn emit_rules_for_selector_str(
        &mut self,
        selector_str: &str,
        block: &lightningcss::declaration::DeclarationBlock<'_>,
        layer: LayerId,
    ) {
        if let Some(parsed) = parse_selector_list(selector_str) {
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
            });
        }
    }

    fn compile_font_face(&mut self, ff: &LightningFontFace<'_>) {
        let mut family = None;
        let mut sources = Vec::new();
        let mut weight_range = (400.0, 400.0);
        let mut italic = false;
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
                _ => {}
            }
        }
        self.font_faces.push(FontFaceRule {
            family,
            sources,
            weight_range,
            italic,
        });
    }

    fn compile_keyframes(&mut self, kf: &LightningKeyframes<'_>) {
        let name = match &kf.name {
            KeyframesName::Ident(id) => intern_str(id.as_ref()),
            KeyframesName::Custom(s) => intern_str(s.as_ref()),
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
        self.keyframes.push(KeyframesRule { name, steps });
    }

    fn finish(self) -> Stylesheet {
        Stylesheet {
            origin: self.origin,
            rules: self.rules,
            layers: self.layers,
            font_faces: self.font_faces,
            keyframes: self.keyframes,
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

fn flatten_nested_selector(parent: &str, child_list: &str) -> String {
    let wrapped_parent = if parent.contains(',') {
        format!(":is({parent})")
    } else {
        parent.to_owned()
    };
    let mut parts = Vec::new();
    for child in child_list.split(',') {
        let trimmed = child.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.contains('&') {
            parts.push(trimmed.replace('&', &wrapped_parent));
        } else {
            parts.push(format!("{wrapped_parent} {trimmed}"));
        }
    }
    parts.join(", ")
}
