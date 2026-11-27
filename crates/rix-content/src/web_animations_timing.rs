//! Pure W3C Web Animations Level 1 timing, keyframe normalization, easing parsing,
//! and pseudo-element validation utilities.
//!
//! Reference: https://drafts.csswg.org/web-animations-1/

use rix_css::{
    parse_inline_style, parse_single_timing_function, AnimationCompositionMode,
    AnimationDirectionMode, AnimationFillModeKind, SpecifiedDeclaration, TimingFunction,
};

/// Keyframe-level composite mode (`auto` inherits from the owning `KeyframeEffect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum KeyframeComposite {
    #[default]
    Auto,
    Replace,
    Add,
    Accumulate,
}

impl KeyframeComposite {
    pub(crate) fn parse_exact(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "replace" => Some(Self::Replace),
            "add" => Some(Self::Add),
            "accumulate" => Some(Self::Accumulate),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Replace => "replace",
            Self::Add => "add",
            Self::Accumulate => "accumulate",
        }
    }

    pub(crate) fn to_css_mode(self) -> Option<AnimationCompositionMode> {
        match self {
            Self::Auto => None,
            Self::Replace => Some(AnimationCompositionMode::Replace),
            Self::Add => Some(AnimationCompositionMode::Add),
            Self::Accumulate => Some(AnimationCompositionMode::Accumulate),
        }
    }
}

/// Parses an exact case-sensitive `CompositeOperation` (`replace | add | accumulate`).
pub(crate) fn parse_composite_exact(s: &str) -> Option<AnimationCompositionMode> {
    match s {
        "replace" => Some(AnimationCompositionMode::Replace),
        "add" => Some(AnimationCompositionMode::Add),
        "accumulate" => Some(AnimationCompositionMode::Accumulate),
        _ => None,
    }
}

pub(crate) fn composite_as_str(c: AnimationCompositionMode) -> &'static str {
    match c {
        AnimationCompositionMode::Replace => "replace",
        AnimationCompositionMode::Add => "add",
        AnimationCompositionMode::Accumulate => "accumulate",
    }
}

/// Parses an exact case-sensitive `IterationCompositeOperation` (`replace | accumulate`).
pub(crate) fn parse_iteration_composite_exact(s: &str) -> Option<AnimationCompositionMode> {
    match s {
        "replace" => Some(AnimationCompositionMode::Replace),
        "accumulate" => Some(AnimationCompositionMode::Accumulate),
        _ => None,
    }
}

/// Parses an exact case-sensitive `FillMode` (`none | forwards | backwards | both | auto`).
pub(crate) fn parse_fill_mode_exact(s: &str) -> Option<(AnimationFillModeKind, bool)> {
    match s {
        "auto" => Some((AnimationFillModeKind::None, true)),
        "none" => Some((AnimationFillModeKind::None, false)),
        "forwards" => Some((AnimationFillModeKind::Forwards, false)),
        "backwards" => Some((AnimationFillModeKind::Backwards, false)),
        "both" => Some((AnimationFillModeKind::Both, false)),
        _ => None,
    }
}

pub(crate) fn fill_mode_as_str(mode: AnimationFillModeKind, is_auto: bool) -> &'static str {
    if is_auto {
        return "auto";
    }
    match mode {
        AnimationFillModeKind::None => "none",
        AnimationFillModeKind::Forwards => "forwards",
        AnimationFillModeKind::Backwards => "backwards",
        AnimationFillModeKind::Both => "both",
    }
}

/// Parses an exact case-sensitive `PlaybackDirection`.
pub(crate) fn parse_direction_exact(s: &str) -> Option<AnimationDirectionMode> {
    match s {
        "normal" => Some(AnimationDirectionMode::Normal),
        "reverse" => Some(AnimationDirectionMode::Reverse),
        "alternate" => Some(AnimationDirectionMode::Alternate),
        "alternate-reverse" => Some(AnimationDirectionMode::AlternateReverse),
        _ => None,
    }
}

pub(crate) fn direction_as_str(d: AnimationDirectionMode) -> &'static str {
    match d {
        AnimationDirectionMode::Normal => "normal",
        AnimationDirectionMode::Reverse => "reverse",
        AnimationDirectionMode::Alternate => "alternate",
        AnimationDirectionMode::AlternateReverse => "alternate-reverse",
    }
}

/// Strictly parses a CSS Easing Level 1 easing string per Web Animations §5.4.5.
pub(crate) fn parse_easing_strict(raw: &str) -> Option<(TimingFunction, String)> {
    let (tf, canon) = parse_single_timing_function(raw)?;
    Some((tf, canon))
}

/// Validates and normalizes a `KeyframeEffect.pseudoElement` selector per W3C Web Animations §5.5.
///
/// Returns `Ok(None)` for `None` or empty/null input, `Ok(Some(normalized))` for valid pseudo-elements,
/// and `Err(msg)` (caller throws `SyntaxError` DOMException) for syntactically invalid selectors.
pub(crate) fn validate_pseudo_element(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(s) = raw else {
        return Ok(None);
    };
    if s.is_empty() {
        return Err("Pseudo-element selector must not be empty".to_string());
    }
    if !s.starts_with(':') {
        return Err(format!("Invalid pseudo-element selector '{s}'"));
    }
    // Reject whitespace or combinators or non-pseudo-element selectors.
    if s.chars().any(char::is_whitespace) || s.contains(',') || s.contains('>') {
        return Err(format!("Invalid pseudo-element selector '{s}'"));
    }
    // Single-colon legacy pseudo-elements (§5.5: normalized to double-colon).
    if !s.starts_with("::") {
        let lower = s.to_ascii_lowercase();
        return match lower.as_str() {
            ":before" => Ok(Some("::before".to_string())),
            ":after" => Ok(Some("::after".to_string())),
            ":first-line" => Ok(Some("::first-line".to_string())),
            ":first-letter" => Ok(Some("::first-letter".to_string())),
            _ => Err(format!("Unsupported single-colon pseudo-element '{s}'")),
        };
    }
    let rest = &s[2..];
    if rest.is_empty() {
        return Err("Empty pseudo-element name".to_string());
    }
    // Functional pseudo-elements like `::highlight(foo)` or `::view-transition-group(root)`
    if let Some(open) = rest.find('(') {
        if !rest.ends_with(')') {
            return Err(format!("Invalid functional pseudo-element '{s}'"));
        }
        let fname = rest[..open].to_ascii_lowercase();
        let arg = &rest[open + 1..rest.len() - 1];
        if arg.is_empty() || !is_css_ident(arg) {
            return Err(format!("Invalid functional pseudo-element argument '{s}'"));
        }
        let valid_fn = matches!(
            fname.as_str(),
            "highlight"
                | "part"
                | "slotted"
                | "view-transition-group"
                | "view-transition-image-pair"
                | "view-transition-old"
                | "view-transition-new"
        );
        if !valid_fn {
            return Err(format!("Unknown functional pseudo-element '{s}'"));
        }
        return Ok(Some(format!("::{fname}({arg})")));
    }
    // Reject chained pseudo-elements like `::before::after` unless known valid.
    if rest.contains(':') {
        return Err(format!("Invalid chained pseudo-element '{s}'"));
    }
    if !is_css_ident(rest) {
        return Err(format!("Invalid pseudo-element identifier '{s}'"));
    }
    let lower = rest.to_ascii_lowercase();
    let known = matches!(
        lower.as_str(),
        "before"
            | "after"
            | "first-line"
            | "first-letter"
            | "marker"
            | "backdrop"
            | "placeholder"
            | "selection"
            | "target-text"
            | "spelling-error"
            | "grammar-error"
            | "file-selector-button"
            | "cue"
            | "view-transition"
            | "scroll-marker"
            | "scroll-marker-group"
    );
    if !known {
        return Err(format!("Unknown pseudo-element '::{rest}'"));
    }
    Ok(Some(format!("::{lower}")))
}

fn is_css_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '-') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Converts a JS keyframe property name to CSS property name per W3C Web Animations §5.5.1.
pub(crate) fn js_keyframe_prop_to_css(key: &str) -> Option<String> {
    if key.starts_with("--") {
        return Some(key.to_string());
    }
    if key == "cssFloat" {
        return Some("float".to_string());
    }
    if key == "cssOffset" {
        return Some("offset".to_string());
    }
    if key == "float" || key == "offset" || key == "easing" || key == "composite" {
        return None;
    }
    // Reject dashed standard CSS property names (e.g. `"margin-left"` is ignored in JS keyframes
    // per W3C Web Animations §5.5.1 unless it's a custom property starting with `--`).
    if key.contains('-') {
        return None;
    }
    let mut out = String::with_capacity(key.len() + 4);
    for ch in key.chars() {
        if ch.is_ascii_uppercase() {
            out.push('-');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

/// Converts a CSS property name back to its JS keyframe camelCase property name.
pub(crate) fn css_prop_to_js_keyframe(prop: &str) -> String {
    if prop.starts_with("--") {
        return prop.to_string();
    }
    if prop == "float" {
        return "cssFloat".to_string();
    }
    if prop == "offset" {
        return "cssOffset".to_string();
    }
    let mut out = String::with_capacity(prop.len());
    let mut cap_next = false;
    for ch in prop.chars() {
        if ch == '-' {
            cap_next = true;
        } else if cap_next {
            out.push(ch.to_ascii_uppercase());
            cap_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

/// Normalized keyframe entry storing both JS-visible serialized properties (for `getKeyframes()`)
/// and compiled `rix-css` declarations (for style cascade application).
#[derive(Debug, Clone)]
pub(crate) struct ProcessedKeyframe {
    pub(crate) offset: Option<f64>,
    pub(crate) computed_offset: f64,
    pub(crate) easing: TimingFunction,
    pub(crate) easing_str: String,
    pub(crate) composite: KeyframeComposite,
    /// `(js_prop_name, css_prop_name, serialized_value)` pairs.
    pub(crate) props: Vec<(String, String, String)>,
    pub(crate) declarations: Vec<SpecifiedDeclaration>,
}

/// Returns whether `css_prop` is a supported animatable CSS property in RixBrowser
/// per W3C Web Animations Level 1 §5.5.2 (unsupported or non-animatable properties MUST NOT
/// even be accessed on JS keyframe objects).
pub(crate) fn is_supported_animatable_css_prop(css_prop: &str) -> bool {
    use rix_css::PropertyIdKind as P;
    if css_prop.starts_with("--") {
        return true;
    }
    let Some(pk) = P::from_name(css_prop) else {
        return false;
    };
    if rix_css::animations::is_property_not_animatable(pk) {
        return false;
    }
    // Exclude CSSOM-only placeholder properties that do not yet have ComputedStyle backing.
    !matches!(
        pk,
        P::AlignmentBaseline
            | P::BackgroundAttachment
            | P::BackgroundRepeat
            | P::BaselineShift
            | P::BaselineSource
            | P::BorderImage
            | P::Clip
            | P::FontVariant
            | P::ListStyle
            | P::ListStyleImage
            | P::ListStylePosition
            | P::ListStyleType
            | P::PageBreakAfter
            | P::PageBreakBefore
            | P::PageBreakInside
            | P::ColorScheme
    )
}

/// Computes W3C Web Animations §5.5.2 / CSS Logical Properties declaration priority rank
/// for a JS keyframe property (higher rank wins on collision):
/// - Longhands always beat shorthands.
/// - Physical properties always beat logical properties within the same category.
/// - Shorthands with fewer constituent longhands beat shorthands with more.
fn web_keyframe_prop_priority(css_prop: &str) -> u32 {
    let Some(pk) = rix_css::PropertyIdKind::from_name(css_prop) else {
        return 100;
    };
    let lhs = pk.shorthand_longhands();
    let count = lhs.len().max(1) as u32;
    let is_logical =
        rix_css::is_logical_property(pk) || lhs.iter().any(|&lh| rix_css::is_logical_property(lh));
    if count == 1 {
        if is_logical {
            900
        } else {
            1000
        }
    } else {
        let base = if is_logical { 100 } else { 500 };
        base + 100u32.saturating_sub(count.min(100))
    }
}

/// Parses and validates a single CSS property value for a keyframe, returning
/// `(serialized_value, compiled_declarations)` if valid (or `None` if invalid/unsupported).
pub(crate) fn parse_keyframe_css_prop(
    css_prop: &str,
    raw_val: &str,
) -> Option<(String, Vec<SpecifiedDeclaration>)> {
    let trimmed = raw_val.trim();
    if trimmed.is_empty() {
        return None;
    }
    let prio = web_keyframe_prop_priority(css_prop);
    let stamp = |mut decls: Vec<SpecifiedDeclaration>| {
        for d in &mut decls {
            d.order = prio;
        }
        decls
    };
    if css_prop.starts_with("--") {
        let mut missing = Vec::new();
        let decls = stamp(parse_inline_style(
            &format!("{css_prop}: {raw_val}"),
            &mut missing,
        ));
        return Some((raw_val.to_string(), decls));
    }
    // Reject `!important` in keyframes per W3C Web Animations §5.5.2.
    if trimmed.to_ascii_lowercase().contains("!important") {
        return None;
    }
    // Preserve `var(...)` values cleanly (validated as token streams at computed-value time).
    if trimmed.contains("var(") {
        let mut missing = Vec::new();
        let decls = stamp(parse_inline_style(
            &format!("{css_prop}: {trimmed}"),
            &mut missing,
        ));
        return Some((trimmed.to_string(), decls));
    }
    // Reject non-animatable animation/transition properties in keyframes? Actually Web Animations
    // allows them in `getKeyframes()` unless invalid CSS, EXCEPT `animation-timing-function` etc.
    // Preserve unclamped `<number>` / `<percentage>` values for `opacity` keyframes per W3C Web Animations
    // (`clamping-001.tentative.html`: keyframes must not clamp prior to interpolation/composition).
    if css_prop == "opacity" {
        if let Some(pct_str) = trimmed.strip_suffix('%') {
            if let Ok(pct) = pct_str.trim().parse::<f32>() {
                if pct.is_finite() {
                    let v = pct / 100.0;
                    let ser = rix_css::values_math::format_math_f64(f64::from(v.clamp(0.0, 1.0)));
                    return Some((
                        ser,
                        vec![SpecifiedDeclaration {
                            property: rix_css::PropertyIdKind::Opacity,
                            custom_name: None,
                            value: rix_css::PropertyValue::Number(v),
                            important: false,
                            order: prio,
                        }],
                    ));
                }
            }
        } else if let Ok(v) = trimmed.parse::<f32>() {
            if v.is_finite() {
                let ser = rix_css::values_math::format_math_f64(f64::from(v.clamp(0.0, 1.0)));
                return Some((
                    ser,
                    vec![SpecifiedDeclaration {
                        property: rix_css::PropertyIdKind::Opacity,
                        custom_name: None,
                        value: rix_css::PropertyValue::Number(v),
                        important: false,
                        order: prio,
                    }],
                ));
            }
        }
    }
    // Validate via `parse_and_serialize_declaration` first so invalid values like `"invalid"` drop!
    let mut missing = Vec::new();
    let decls = stamp(parse_inline_style(
        &format!("{css_prop}: {trimmed}"),
        &mut missing,
    ));
    if let Some(ser) = rix_css::parse_and_serialize_declaration(css_prop, trimmed) {
        return Some((ser, decls));
    }
    if !decls.is_empty() {
        return Some((trimmed.to_string(), decls));
    }
    None
}

/// Computes `computed_offset` for each keyframe per W3C Web Animations Level 1 §5.5.4
/// ("Computing missing keyframe offsets").
pub(crate) fn compute_missing_keyframe_offsets(frames: &mut [ProcessedKeyframe]) {
    let n = frames.len();
    if n == 0 {
        return;
    }
    for f in frames.iter_mut() {
        f.computed_offset = f.offset.unwrap_or(f64::NAN);
    }
    if n == 1 {
        if frames[0].computed_offset.is_nan() {
            frames[0].computed_offset = 1.0;
        }
        return;
    }
    if frames[n - 1].computed_offset.is_nan() {
        frames[n - 1].computed_offset = 1.0;
    }
    if frames[0].computed_offset.is_nan() {
        frames[0].computed_offset = 0.0;
    }

    let mut last_idx = 0usize;
    let mut i = 1usize;
    while i < n {
        if !frames[i].computed_offset.is_nan() {
            let gap = i - last_idx;
            if gap > 1 {
                let start_off = frames[last_idx].computed_offset;
                let end_off = frames[i].computed_offset;
                for j in 1..gap {
                    frames[last_idx + j].computed_offset =
                        start_off + (end_off - start_off) * (j as f64 / gap as f64);
                }
            }
            last_idx = i;
        }
        i += 1;
    }
}

/// Validates that explicit offsets are in `[0.0, 1.0]` and loosely sorted per W3C §5.5.2.
pub(crate) fn validate_keyframe_offsets(frames: &[ProcessedKeyframe]) -> Result<(), String> {
    let mut prev: Option<f64> = None;
    for f in frames {
        if let Some(off) = f.offset {
            if !off.is_finite() || !(0.0..=1.0).contains(&off) {
                return Err(format!("Keyframe offset {off} must be within [0, 1]"));
            }
            if let Some(p) = prev {
                if off < p {
                    return Err("Keyframe offsets must be loosely sorted".to_string());
                }
            }
            prev = Some(off);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rix_css::{compute_effect_timing, EffectPhase, EffectTimingInput};

    #[test]
    fn compute_missing_offsets_matches_w3c_spec() {
        let mk = |off: Option<f64>| ProcessedKeyframe {
            offset: off,
            computed_offset: f64::NAN,
            easing: TimingFunction::Linear,
            easing_str: "linear".to_string(),
            composite: KeyframeComposite::Auto,
            props: Vec::new(),
            declarations: Vec::new(),
        };
        let mut f = vec![mk(None), mk(Some(0.25)), mk(None)];
        compute_missing_keyframe_offsets(&mut f);
        assert_eq!(f[0].computed_offset, 0.0);
        assert_eq!(f[1].computed_offset, 0.25);
        assert_eq!(f[2].computed_offset, 1.0);

        let mut single = vec![mk(None)];
        compute_missing_keyframe_offsets(&mut single);
        assert_eq!(single[0].computed_offset, 1.0);
    }

    #[test]
    fn effect_timing_handles_step_before_flag_and_end_delay() {
        let timing = EffectTimingInput {
            delay_ms: 100.0,
            end_delay_ms: 50.0,
            fill_mode: AnimationFillModeKind::Both,
            fill_is_auto: false,
            iteration_start: 0.0,
            iterations: 1.0,
            duration_ms: Some(1000.0),
            direction: AnimationDirectionMode::Reverse,
            easing: TimingFunction::Steps(1, rix_css::StepPositionMode::Start),
        };
        let ct = compute_effect_timing(&timing, Some(1200.0), 1.0);
        assert_eq!(ct.phase, EffectPhase::After);
        assert_eq!(ct.end_time, 1150.0);
        assert!(ct.before_flag);
        assert_eq!(ct.progress, Some(0.0));
    }

    #[test]
    fn validate_pseudo_element_normalizes_and_rejects_invalid() {
        assert_eq!(validate_pseudo_element(None).unwrap(), None);
        assert_eq!(
            validate_pseudo_element(Some(":before")).unwrap(),
            Some("::before".to_string())
        );
        assert_eq!(
            validate_pseudo_element(Some("::after")).unwrap(),
            Some("::after".to_string())
        );
        assert!(validate_pseudo_element(Some("before")).is_err());
        assert!(validate_pseudo_element(Some("::unknown-pseudo")).is_err());
        assert!(validate_pseudo_element(Some("::before::after")).is_err());
    }
}
