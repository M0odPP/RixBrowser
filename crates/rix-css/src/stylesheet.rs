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
use selectors::parser::{AncestorHashes, Selector};

use crate::properties::{
    compile_declaration_block, MissingCssFeature, MissingCssKind, PropertyIdKind, PropertyValue,
    SpecifiedDeclaration,
};
use crate::selector::{
    compile_lightning_selector_list_with_namespaces, parse_selector_list_strict,
    parse_selector_list_with_namespaces, serialize_selector_list, CssAtom, RixSelectorImpl,
    SelectorNamespaceMap,
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
    origin: u8,
    viewport_w: u32,
    viewport_h: u32,
    dark_mode: bool,
    reduced_motion: bool,
}

fn hash_css_bytes(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

impl Stylesheet {
    /// Parses and compiles a CSS stylesheet against the given origin and media environment.
    #[must_use]
    pub fn parse(css: &str, origin: CascadeOrigin, media: &MediaContext) -> Self {
        let rewritten = crate::properties::rewrite_dynamic_var_names(css);
        let opts = ParserOptions {
            error_recovery: true,
            ..ParserOptions::default()
        };
        let mut compiler = StylesheetCompiler::new(origin, *media);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Ok(sheet) = StyleSheet::parse(&rewritten, opts) {
                compiler.compile_rule_list(&sheet.rules, None, LayerId::UNLAYERED, None);
            }
        }));
        compiler.finish()
    }

    /// Parses a stylesheet through a process-wide shared compiled cache keyed by content hash
    /// and active media environment, avoiding redundant CSS parsing across navigations & tabs.
    #[must_use]
    pub fn parse_cached(css: &str, origin: CascadeOrigin, media: &MediaContext) -> Arc<Self> {
        static CACHE: OnceLock<RwLock<HashMap<StylesheetCacheKey, Arc<Stylesheet>>>> =
            OnceLock::new();
        let cache = CACHE.get_or_init(|| RwLock::new(HashMap::new()));
        let key = StylesheetCacheKey {
            hash: hash_css_bytes(css.as_bytes()),
            len: css.len(),
            origin: origin as u8,
            viewport_w: media.viewport_width.round().max(0.0) as u32,
            viewport_h: media.viewport_height.round().max(0.0) as u32,
            dark_mode: media.dark_mode,
            reduced_motion: media.reduced_motion,
        };
        if let Ok(guard) = cache.read() {
            if let Some(hit) = guard.get(&key) {
                return Arc::clone(hit);
            }
        }
        let compiled = Arc::new(Self::parse(css, origin, media));
        if let Ok(mut guard) = cache.write() {
            if guard.len() >= 256 {
                guard.clear();
            }
            guard.insert(key, Arc::clone(&compiled));
        }
        compiled
    }

    /// Returns all unique `background-image: url(...)` references compiled in this stylesheet.
    #[must_use]
    pub fn referenced_image_urls(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for rule in &self.rules {
            for decl in rule.declarations.iter() {
                if decl.property == PropertyIdKind::BackgroundLayers {
                    if let PropertyValue::BackgroundLayers(layers) = &decl.value {
                        for layer in layers {
                            if let BackgroundImageValue::Url(url) = &layer.image {
                                let trimmed = url.as_str().trim();
                                if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                                    out.push(trimmed.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        out
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
    registered_properties: Vec<RegisteredProperty>,
    imports: Vec<String>,
    missing_seen: HashSet<MissingCssFeature>,
    missing_features: Vec<MissingCssFeature>,
    next_source_order: u32,
    selector_scratch: String,
    namespaces: SelectorNamespaceMap,
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
                CssRule::Property(pr) => self.compile_property_rule(pr, current_layer),
                CssRule::Import(imp) => {
                    if self.media.matches_media_list(&imp.media) {
                        let url = imp.url.as_ref().trim();
                        if !url.is_empty() {
                            self.imports.push(url.to_string());
                        }
                    }
                }
                CssRule::Namespace(ns) => {
                    let url = CssAtom::new(ns.url.as_ref());
                    if let Some(ref pref) = ns.prefix {
                        self.namespaces
                            .prefixes
                            .insert(pref.as_ref().to_string(), url);
                    } else {
                        self.namespaces.default_namespace = Some(url);
                    }
                }
                CssRule::Ignored => {}
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
        let ns_opt = (!self.namespaces.is_empty()).then_some(&self.namespaces);
        if let Ok(parsed) = parse_selector_list_with_namespaces(selector_str, ns_opt) {
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

/// Structured CSSOM rule representation exposed to `CSSStyleSheet.cssRules` and `insertRule`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CssomRuleKind {
    Style {
        selector_text: String,
        declarations_text: String,
    },
    Media {
        condition_text: String,
        rules: Vec<CssomRuleKind>,
    },
    Supports {
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
        prelude: String,
        rules: Vec<CssomRuleKind>,
    },
    Namespace {
        prefix: Option<String>,
        url: String,
    },
    Keyframes {
        name: String,
        body_text: String,
    },
    Property {
        name: String,
        css_text: String,
    },
    Other {
        css_text: String,
    },
}

fn join_cssom_rules(rules: &[CssomRuleKind]) -> String {
    let mut out = String::new();
    for (i, r) in rules.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&r.to_css_text());
    }
    out
}

impl CssomRuleKind {
    /// Returns child rules if this is a `CSSGroupingRule` (`@media`, `@supports`, `@layer`, `@scope`).
    #[must_use]
    pub fn child_rules(&self) -> Option<&[CssomRuleKind]> {
        match self {
            Self::Media { rules, .. }
            | Self::Supports { rules, .. }
            | Self::LayerBlock { rules, .. }
            | Self::Scope { rules, .. } => Some(rules),
            _ => None,
        }
    }

    /// Returns mutable child rules if this is a `CSSGroupingRule`.
    pub fn child_rules_mut(&mut self) -> Option<&mut Vec<CssomRuleKind>> {
        match self {
            Self::Media { rules, .. }
            | Self::Supports { rules, .. }
            | Self::LayerBlock { rules, .. }
            | Self::Scope { rules, .. } => Some(rules),
            _ => None,
        }
    }

    /// Serializes this CSSOM rule back into valid CSS text.
    #[must_use]
    pub fn to_css_text(&self) -> String {
        match self {
            Self::Style {
                selector_text,
                declarations_text,
            } => {
                if declarations_text.trim().is_empty() {
                    format!("{selector_text} {{ }}")
                } else {
                    format!("{selector_text} {{ {declarations_text} }}")
                }
            }
            Self::Media {
                condition_text,
                rules,
            } => format!("@media {condition_text} {{ {} }}", join_cssom_rules(rules)),
            Self::Supports {
                condition_text,
                rules,
            } => format!(
                "@supports {condition_text} {{ {} }}",
                join_cssom_rules(rules)
            ),
            Self::LayerBlock { name, rules } => {
                let body = join_cssom_rules(rules);
                if name.is_empty() {
                    format!("@layer {{ {body} }}")
                } else {
                    format!("@layer {name} {{ {body} }}")
                }
            }
            Self::LayerStatement { names } => format!("@layer {};", names.join(", ")),
            Self::Scope { prelude, rules } => {
                let body = join_cssom_rules(rules);
                if prelude.is_empty() {
                    format!("@scope {{ {body} }}")
                } else {
                    format!("@scope {prelude} {{ {body} }}")
                }
            }
            Self::Namespace { prefix, url } => {
                let escaped = url.replace('\\', "\\\\").replace('"', "\\\"");
                match prefix {
                    Some(p) if !p.is_empty() => format!("@namespace {p} url(\"{escaped}\");"),
                    _ => format!("@namespace url(\"{escaped}\");"),
                }
            }
            Self::Keyframes { name, body_text } => format!("@keyframes {name} {{ {body_text} }}"),
            Self::Property { css_text, .. } | Self::Other { css_text } => css_text.clone(),
        }
    }
}

fn lightning_to_css_string<T: LightningToCss>(val: &T) -> String {
    let mut out = String::new();
    let mut printer = Printer::new(&mut out, PrinterOptions::default());
    let _ = val.to_css(&mut printer);
    out
}

fn convert_lightning_rule_list_to_cssom(
    list: &CssRuleList<'_>,
    namespaces: &mut SelectorNamespaceMap,
) -> Vec<CssomRuleKind> {
    list.0
        .iter()
        .filter_map(|r| convert_lightning_rule_to_cssom(r, false, namespaces).ok())
        .collect()
}

fn convert_lightning_rule_to_cssom(
    rule: &CssRule<'_>,
    _strict: bool,
    namespaces: &mut SelectorNamespaceMap,
) -> Result<CssomRuleKind, String> {
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
        CssRule::Style(sr) => {
            let raw_sel = lightning_to_css_string(&sr.selectors);
            // Per CSS Syntax L3 & CSSOM, style rules with invalid selectors are always dropped.
            let ns_opt = (!namespaces.is_empty()).then_some(&*namespaces);
            let parsed = parse_selector_list_with_namespaces(&raw_sel, ns_opt)?;
            let selector_text = serialize_selector_list(&parsed);
            let declarations_text = lightning_to_css_string(&sr.declarations);
            Ok(CssomRuleKind::Style {
                selector_text,
                declarations_text,
            })
        }
        CssRule::Media(mr) => Ok(CssomRuleKind::Media {
            condition_text: lightning_to_css_string(&mr.query),
            rules: convert_lightning_rule_list_to_cssom(&mr.rules, namespaces),
        }),
        CssRule::Supports(sr) => Ok(CssomRuleKind::Supports {
            condition_text: lightning_to_css_string(&sr.condition),
            rules: convert_lightning_rule_list_to_cssom(&sr.rules, namespaces),
        }),
        CssRule::LayerBlock(lb) => Ok(CssomRuleKind::LayerBlock {
            name: lb.name.as_ref().map(join_layer_name).unwrap_or_default(),
            rules: convert_lightning_rule_list_to_cssom(&lb.rules, namespaces),
        }),
        CssRule::LayerStatement(ls) => Ok(CssomRuleKind::LayerStatement {
            names: ls.names.iter().map(join_layer_name).collect(),
        }),
        CssRule::Scope(sc) => {
            let mut prelude = String::new();
            if let Some(ref start) = sc.scope_start {
                prelude.push('(');
                prelude.push_str(&lightning_to_css_string(start));
                prelude.push(')');
            }
            if let Some(ref end) = sc.scope_end {
                if !prelude.is_empty() {
                    prelude.push(' ');
                }
                prelude.push_str("to (");
                prelude.push_str(&lightning_to_css_string(end));
                prelude.push(')');
            }
            Ok(CssomRuleKind::Scope {
                prelude,
                rules: convert_lightning_rule_list_to_cssom(&sc.rules, namespaces),
            })
        }
        CssRule::Keyframes(kf) => {
            let name = match &kf.name {
                KeyframesName::Ident(id) => id.as_ref().to_string(),
                KeyframesName::Custom(s) => s.as_ref().to_string(),
            };
            let full = lightning_to_css_string(kf);
            let body_text = full
                .find('{')
                .and_then(|s| full.rfind('}').map(|e| full[s + 1..e].trim().to_string()))
                .unwrap_or_default();
            Ok(CssomRuleKind::Keyframes { name, body_text })
        }
        CssRule::Property(pr) => Ok(CssomRuleKind::Property {
            name: pr.name.as_ref().to_string(),
            css_text: lightning_to_css_string(pr),
        }),
        other => Ok(CssomRuleKind::Other {
            css_text: lightning_to_css_string(other),
        }),
    }
}

/// Parses a CSS stylesheet into top-level [`CssomRuleKind`] items (error-recovering).
#[must_use]
pub fn parse_cssom_rules(css: &str) -> Vec<CssomRuleKind> {
    let trimmed = css.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    // Fast path for a single top-level style rule so canonical selector serialization
    // (`:nth-child(n)`, `:is(:is(div))`, etc.) is not simplified by `lightningcss`.
    if !trimmed.starts_with('@') && split_single_style_rule(trimmed).is_some() {
        return match parse_single_cssom_rule(trimmed) {
            Ok(rule) => vec![rule],
            Err(_) => Vec::new(),
        };
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let opts = ParserOptions {
            error_recovery: true,
            ..ParserOptions::default()
        };
        let Ok(parsed) = StyleSheet::parse(css, opts) else {
            return Vec::new();
        };
        let mut namespaces = SelectorNamespaceMap::default();
        convert_lightning_rule_list_to_cssom(&parsed.rules, &mut namespaces)
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
    let trimmed = rule_css.trim();
    if trimmed.is_empty() {
        return Err("Empty CSS rule".to_string());
    }
    // For standard style rules (not starting with `@`), parse the selector prelude directly
    // through Servo `selectors` BEFORE `lightningcss` so canonical forms like `:is(:is(div))`,
    // `a:is(:not(:hover))`, and forgiving `:has(:is(:has(*)))` are preserved verbatim without
    // minification/rewriting!
    if !trimmed.starts_with('@') {
        if let Some((raw_sel, raw_body)) = split_single_style_rule(trimmed) {
            let parsed_sel = if let Some(ns) = namespaces {
                parse_selector_list_with_namespaces(raw_sel, Some(ns))?
            } else {
                parse_selector_list_strict(raw_sel)?
            };
            let selector_text = serialize_selector_list(&parsed_sel);
            // Validate declaration block syntax via lightningcss (empty blocks are valid).
            let body_trimmed = raw_body.trim();
            let declarations_text = if body_trimmed.is_empty() {
                String::new()
            } else {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let opts = ParserOptions {
                        error_recovery: true,
                        ..ParserOptions::default()
                    };
                    lightningcss::declaration::DeclarationBlock::parse_string(body_trimmed, opts)
                        .map(|b| lightning_to_css_string(&b))
                        .unwrap_or_else(|_| body_trimmed.to_string())
                }))
                .unwrap_or_else(|_| body_trimmed.to_string())
            };
            return Ok(CssomRuleKind::Style {
                selector_text,
                declarations_text,
            });
        }
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let opts = ParserOptions {
            error_recovery: false,
            ..ParserOptions::default()
        };
        let parsed = StyleSheet::parse(trimmed, opts)
            .map_err(|e| format!("Failed to parse CSS rule '{trimmed}': {e}"))?;
        if parsed.rules.0.len() != 1 {
            return Err(format!(
                "Expected exactly 1 CSS rule, got {}",
                parsed.rules.0.len()
            ));
        }
        let mut ns = namespaces.cloned().unwrap_or_default();
        convert_lightning_rule_to_cssom(&parsed.rules.0[0], true, &mut ns)
    }))
    .unwrap_or_else(|_| Err(format!("Internal CSS parser rejected rule '{trimmed}'")))
}

/// Splits a single top-level CSS style rule `<selector> { <decls> }` if balanced and containing
/// no trailing tokens or nested rules.
fn split_single_style_rule(input: &str) -> Option<(&str, &str)> {
    let bytes = input.as_bytes();
    let mut i = 0usize;
    let mut in_str: Option<u8> = None;
    let mut paren_depth = 0usize;
    let mut open_brace = None;
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
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
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
    let mut depth = 0usize;
    let mut j = start_brace;
    in_str = None;
    while j < bytes.len() {
        let b = bytes[j];
        if let Some(q) = in_str {
            if b == b'\\' {
                j += 2;
                continue;
            }
            if b == q {
                in_str = None;
            }
            j += 1;
            continue;
        }
        match b {
            b'\\' => {
                j += 2;
                continue;
            }
            b'"' | b'\'' => in_str = Some(b),
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    // Must have no trailing non-whitespace/comment tokens after closing `}`.
                    if !input[j + 1..].trim().is_empty() {
                        return None;
                    }
                    let body = &input[start_brace + 1..j];
                    // If body contains nested `{`, delegate to lightningcss nesting parser.
                    if body.contains('{') {
                        return None;
                    }
                    return Some((input[..start_brace].trim(), body));
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}
