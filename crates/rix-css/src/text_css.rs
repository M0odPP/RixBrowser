//! W3C CSS Text Level 3 / Level 4 / Level 5 value types, parsers, shorthand
//! expanders, canonical serializers, and animation interpolation helpers.

use serde::{Deserialize, Serialize};

use crate::values::{
    EvalContext, LengthPercentage, TextJustifyMode, WhiteSpaceMode, WordBreakMode,
};
use crate::values_math::{parse_math_value, ValueKind};

/// W3C CSS Text L4 §3.2 `white-space-collapse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum WhiteSpaceCollapseMode {
    #[default]
    Collapse,
    Discard,
    Preserve,
    PreserveBreaks,
    PreserveSpaces,
    BreakSpaces,
}

impl WhiteSpaceCollapseMode {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "collapse" => Some(Self::Collapse),
            "discard" => Some(Self::Discard),
            "preserve" => Some(Self::Preserve),
            "preserve-breaks" => Some(Self::PreserveBreaks),
            "preserve-spaces" => Some(Self::PreserveSpaces),
            "break-spaces" => Some(Self::BreakSpaces),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Collapse => "collapse",
            Self::Discard => "discard",
            Self::Preserve => "preserve",
            Self::PreserveBreaks => "preserve-breaks",
            Self::PreserveSpaces => "preserve-spaces",
            Self::BreakSpaces => "break-spaces",
        }
    }
}

/// W3C CSS Text L4 §5.1.1 `text-wrap-mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum TextWrapMode {
    #[default]
    Wrap,
    Nowrap,
}

impl TextWrapMode {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "wrap" => Some(Self::Wrap),
            "nowrap" => Some(Self::Nowrap),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wrap => "wrap",
            Self::Nowrap => "nowrap",
        }
    }
}

/// W3C CSS Text L4 §5.1.2 `text-wrap-style`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum TextWrapStyle {
    #[default]
    Auto,
    Balance,
    Stable,
    Pretty,
    AvoidOrphans,
}

impl TextWrapStyle {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "balance" => Some(Self::Balance),
            "stable" => Some(Self::Stable),
            "pretty" => Some(Self::Pretty),
            "avoid-orphans" => Some(Self::AvoidOrphans),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Balance => "balance",
            Self::Stable => "stable",
            Self::Pretty => "pretty",
            Self::AvoidOrphans => "avoid-orphans",
        }
    }
}

/// W3C CSS Text L4 §3.4 `white-space-trim` (non-inherited bitmask).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct WhiteSpaceTrim(pub u8);

impl WhiteSpaceTrim {
    pub const NONE: Self = Self(0);
    pub const DISCARD_BEFORE: u8 = 1 << 0;
    pub const DISCARD_AFTER: u8 = 1 << 1;
    pub const DISCARD_INNER: u8 = 1 << 2;

    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let lower = raw.trim().to_ascii_lowercase();
        let toks: Vec<&str> = lower.split_ascii_whitespace().collect();
        if toks.is_empty() {
            return None;
        }
        if toks.len() == 1 && toks[0] == "none" {
            return Some(Self::NONE);
        }
        let mut bits = 0u8;
        for tok in toks {
            let bit = match tok {
                "discard-before" => Self::DISCARD_BEFORE,
                "discard-after" => Self::DISCARD_AFTER,
                "discard-inner" => Self::DISCARD_INNER,
                _ => return None,
            };
            if bits & bit != 0 {
                return None;
            }
            bits |= bit;
        }
        Some(Self(bits))
    }

    #[must_use]
    pub fn serialize(self) -> String {
        if self.0 == 0 {
            return "none".to_string();
        }
        let mut parts = Vec::with_capacity(3);
        if self.0 & Self::DISCARD_BEFORE != 0 {
            parts.push("discard-before");
        }
        if self.0 & Self::DISCARD_AFTER != 0 {
            parts.push("discard-after");
        }
        if self.0 & Self::DISCARD_INNER != 0 {
            parts.push("discard-inner");
        }
        parts.join(" ")
    }
}

/// Derives the legacy 6-state [`WhiteSpaceMode`] from CSS Text L4 `white-space-collapse`
/// and `text-wrap-mode` so layout formatting contexts stay synchronized.
#[must_use]
pub const fn derive_white_space_mode(
    collapse: WhiteSpaceCollapseMode,
    wrap: TextWrapMode,
) -> WhiteSpaceMode {
    match (collapse, wrap) {
        (
            WhiteSpaceCollapseMode::Collapse | WhiteSpaceCollapseMode::Discard,
            TextWrapMode::Wrap,
        ) => WhiteSpaceMode::Normal,
        (
            WhiteSpaceCollapseMode::Collapse | WhiteSpaceCollapseMode::Discard,
            TextWrapMode::Nowrap,
        ) => WhiteSpaceMode::Nowrap,
        (
            WhiteSpaceCollapseMode::Preserve | WhiteSpaceCollapseMode::PreserveSpaces,
            TextWrapMode::Nowrap,
        ) => WhiteSpaceMode::Pre,
        (
            WhiteSpaceCollapseMode::Preserve | WhiteSpaceCollapseMode::PreserveSpaces,
            TextWrapMode::Wrap,
        ) => WhiteSpaceMode::PreWrap,
        (WhiteSpaceCollapseMode::PreserveBreaks, TextWrapMode::Wrap) => WhiteSpaceMode::PreLine,
        (WhiteSpaceCollapseMode::PreserveBreaks, TextWrapMode::Nowrap) => WhiteSpaceMode::Nowrap,
        (WhiteSpaceCollapseMode::BreakSpaces, TextWrapMode::Wrap) => WhiteSpaceMode::BreakSpaces,
        (WhiteSpaceCollapseMode::BreakSpaces, TextWrapMode::Nowrap) => WhiteSpaceMode::Pre,
    }
}

/// Maps a legacy [`WhiteSpaceMode`] into `(white-space-collapse, text-wrap-mode)`.
#[must_use]
pub const fn decompose_legacy_white_space(
    ws: WhiteSpaceMode,
) -> (WhiteSpaceCollapseMode, TextWrapMode) {
    match ws {
        WhiteSpaceMode::Normal => (WhiteSpaceCollapseMode::Collapse, TextWrapMode::Wrap),
        WhiteSpaceMode::Nowrap => (WhiteSpaceCollapseMode::Collapse, TextWrapMode::Nowrap),
        WhiteSpaceMode::Pre => (WhiteSpaceCollapseMode::Preserve, TextWrapMode::Nowrap),
        WhiteSpaceMode::PreWrap => (WhiteSpaceCollapseMode::Preserve, TextWrapMode::Wrap),
        WhiteSpaceMode::PreLine => (WhiteSpaceCollapseMode::PreserveBreaks, TextWrapMode::Wrap),
        WhiteSpaceMode::BreakSpaces => (WhiteSpaceCollapseMode::BreakSpaces, TextWrapMode::Wrap),
    }
}

/// Parses CSS Text L4 `white-space` shorthand into
/// `(white-space-collapse, text-wrap-mode, white-space-trim)`.
#[must_use]
pub fn parse_white_space_shorthand(
    raw: &str,
) -> Option<(WhiteSpaceCollapseMode, TextWrapMode, WhiteSpaceTrim)> {
    let lower = raw.trim().to_ascii_lowercase();
    let toks: Vec<&str> = lower.split_ascii_whitespace().collect();
    if toks.is_empty() {
        return None;
    }
    if toks.len() == 1 {
        match toks[0] {
            "normal" => {
                return Some((
                    WhiteSpaceCollapseMode::Collapse,
                    TextWrapMode::Wrap,
                    WhiteSpaceTrim::NONE,
                ));
            }
            "pre" => {
                return Some((
                    WhiteSpaceCollapseMode::Preserve,
                    TextWrapMode::Nowrap,
                    WhiteSpaceTrim::NONE,
                ));
            }
            "pre-wrap" => {
                return Some((
                    WhiteSpaceCollapseMode::Preserve,
                    TextWrapMode::Wrap,
                    WhiteSpaceTrim::NONE,
                ));
            }
            "pre-line" => {
                return Some((
                    WhiteSpaceCollapseMode::PreserveBreaks,
                    TextWrapMode::Wrap,
                    WhiteSpaceTrim::NONE,
                ));
            }
            _ => {}
        }
    }
    let mut collapse: Option<WhiteSpaceCollapseMode> = None;
    let mut wrap: Option<TextWrapMode> = None;
    let mut trim_bits = 0u8;
    for tok in toks {
        if let Some(c) = WhiteSpaceCollapseMode::parse(tok) {
            if collapse.is_some() {
                return None;
            }
            collapse = Some(c);
        } else if let Some(w) = TextWrapMode::parse(tok) {
            if wrap.is_some() {
                return None;
            }
            wrap = Some(w);
        } else {
            let bit = match tok {
                "discard-before" => WhiteSpaceTrim::DISCARD_BEFORE,
                "discard-after" => WhiteSpaceTrim::DISCARD_AFTER,
                "discard-inner" => WhiteSpaceTrim::DISCARD_INNER,
                _ => return None,
            };
            if trim_bits & bit != 0 {
                return None;
            }
            trim_bits |= bit;
        }
    }
    Some((
        collapse.unwrap_or(WhiteSpaceCollapseMode::Collapse),
        wrap.unwrap_or(TextWrapMode::Wrap),
        WhiteSpaceTrim(trim_bits),
    ))
}

/// Canonical W3C CSS Text L4 serialization of `white-space`.
#[must_use]
pub fn serialize_white_space(
    collapse: WhiteSpaceCollapseMode,
    wrap: TextWrapMode,
    trim: WhiteSpaceTrim,
) -> String {
    if trim.is_none() {
        return match (collapse, wrap) {
            (WhiteSpaceCollapseMode::Collapse, TextWrapMode::Wrap) => "normal".to_string(),
            (WhiteSpaceCollapseMode::Preserve, TextWrapMode::Nowrap) => "pre".to_string(),
            (WhiteSpaceCollapseMode::Collapse, TextWrapMode::Nowrap) => "nowrap".to_string(),
            (WhiteSpaceCollapseMode::Preserve, TextWrapMode::Wrap) => "pre-wrap".to_string(),
            (WhiteSpaceCollapseMode::PreserveBreaks, TextWrapMode::Wrap) => "pre-line".to_string(),
            (WhiteSpaceCollapseMode::BreakSpaces, TextWrapMode::Wrap) => "break-spaces".to_string(),
            _ => {
                let mut parts = Vec::with_capacity(2);
                if collapse != WhiteSpaceCollapseMode::Collapse {
                    parts.push(collapse.as_str());
                }
                if wrap != TextWrapMode::Wrap {
                    parts.push(wrap.as_str());
                }
                parts.join(" ")
            }
        };
    }
    let mut parts = Vec::with_capacity(3);
    if collapse != WhiteSpaceCollapseMode::Collapse {
        parts.push(collapse.as_str().to_string());
    }
    if wrap != TextWrapMode::Wrap {
        parts.push(wrap.as_str().to_string());
    }
    parts.push(trim.serialize());
    parts.join(" ")
}

/// Parses CSS Text L4 `text-wrap` shorthand (`<text-wrap-mode> || <text-wrap-style>`).
#[must_use]
pub fn parse_text_wrap_shorthand(raw: &str) -> Option<(TextWrapMode, TextWrapStyle)> {
    let lower = raw.trim().to_ascii_lowercase();
    let toks: Vec<&str> = lower.split_ascii_whitespace().collect();
    if toks.is_empty() || toks.len() > 2 {
        return None;
    }
    let mut mode: Option<TextWrapMode> = None;
    let mut style: Option<TextWrapStyle> = None;
    for tok in toks {
        if let Some(m) = TextWrapMode::parse(tok) {
            if mode.is_some() {
                return None;
            }
            mode = Some(m);
        } else {
            let s = TextWrapStyle::parse(tok)?;
            if style.is_some() {
                return None;
            }
            style = Some(s);
        }
    }
    Some((
        mode.unwrap_or(TextWrapMode::Wrap),
        style.unwrap_or(TextWrapStyle::Auto),
    ))
}

/// Canonical serialization of `text-wrap`.
#[must_use]
pub fn serialize_text_wrap(mode: TextWrapMode, style: TextWrapStyle) -> String {
    match (mode, style) {
        (TextWrapMode::Wrap, TextWrapStyle::Auto) => "wrap".to_string(),
        (TextWrapMode::Wrap, s) => s.as_str().to_string(),
        (TextWrapMode::Nowrap, TextWrapStyle::Auto) => "nowrap".to_string(),
        (TextWrapMode::Nowrap, s) => format!("nowrap {}", s.as_str()),
    }
}

/// W3C CSS Text L4 `text-autospace`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum TextAutospaceValue {
    #[default]
    Normal,
    NoAutospace,
    Auto,
    Flags(u8),
}

impl TextAutospaceValue {
    pub const IDEOGRAPH_ALPHA: u8 = 1 << 0;
    pub const IDEOGRAPH_NUMERIC: u8 = 1 << 1;
    pub const PUNCTUATION: u8 = 1 << 2;
    pub const INSERT: u8 = 1 << 3;
    pub const REPLACE: u8 = 1 << 4;

    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let lower = raw.trim().to_ascii_lowercase();
        let toks: Vec<&str> = lower.split_ascii_whitespace().collect();
        if toks.is_empty() {
            return None;
        }
        if toks.len() == 1 {
            match toks[0] {
                "normal" => return Some(Self::Normal),
                "no-autospace" => return Some(Self::NoAutospace),
                "auto" => return Some(Self::Auto),
                _ => {}
            }
        }
        let mut bits = 0u8;
        for tok in toks {
            let bit = match tok {
                "ideograph-alpha" => Self::IDEOGRAPH_ALPHA,
                "ideograph-numeric" => Self::IDEOGRAPH_NUMERIC,
                "punctuation" => Self::PUNCTUATION,
                "insert" => Self::INSERT,
                "replace" => Self::REPLACE,
                _ => return None,
            };
            if bits & bit != 0 {
                return None;
            }
            if (bit == Self::INSERT && bits & Self::REPLACE != 0)
                || (bit == Self::REPLACE && bits & Self::INSERT != 0)
            {
                return None;
            }
            bits |= bit;
        }
        let class_mask = Self::IDEOGRAPH_ALPHA | Self::IDEOGRAPH_NUMERIC | Self::PUNCTUATION;
        if bits & class_mask == 0 {
            return None;
        }
        Some(Self::Flags(bits))
    }

    #[must_use]
    pub fn serialize(self) -> String {
        match self {
            Self::Normal => "normal".to_string(),
            Self::NoAutospace => "no-autospace".to_string(),
            Self::Auto => "auto".to_string(),
            Self::Flags(bits) => {
                let mut parts = Vec::with_capacity(4);
                if bits & Self::IDEOGRAPH_ALPHA != 0 {
                    parts.push("ideograph-alpha");
                }
                if bits & Self::IDEOGRAPH_NUMERIC != 0 {
                    parts.push("ideograph-numeric");
                }
                if bits & Self::PUNCTUATION != 0 {
                    parts.push("punctuation");
                }
                if bits & Self::INSERT != 0 {
                    parts.push("insert");
                } else if bits & Self::REPLACE != 0 {
                    parts.push("replace");
                }
                parts.join(" ")
            }
        }
    }
}

/// W3C CSS Text L4 `text-spacing-trim`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum TextSpacingTrimMode {
    #[default]
    Normal,
    Auto,
    SpaceAll,
    SpaceFirst,
    TrimStart,
    TrimBoth,
    TrimAll,
}

impl TextSpacingTrimMode {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "normal" => Some(Self::Normal),
            "auto" => Some(Self::Auto),
            "space-all" => Some(Self::SpaceAll),
            "space-first" => Some(Self::SpaceFirst),
            "trim-start" => Some(Self::TrimStart),
            "trim-both" => Some(Self::TrimBoth),
            "trim-all" => Some(Self::TrimAll),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Auto => "auto",
            Self::SpaceAll => "space-all",
            Self::SpaceFirst => "space-first",
            Self::TrimStart => "trim-start",
            Self::TrimBoth => "trim-both",
            Self::TrimAll => "trim-all",
        }
    }
}

/// Parses CSS Text L4 `text-spacing` shorthand (`none | auto | <text-spacing-trim> || <text-autospace>`).
#[must_use]
pub fn parse_text_spacing_shorthand(
    raw: &str,
) -> Option<(TextSpacingTrimMode, TextAutospaceValue)> {
    let lower = raw.trim().to_ascii_lowercase();
    let toks: Vec<&str> = lower.split_ascii_whitespace().collect();
    if toks.is_empty() {
        return None;
    }
    if toks.len() == 1 {
        match toks[0] {
            "none" => {
                return Some((
                    TextSpacingTrimMode::SpaceAll,
                    TextAutospaceValue::NoAutospace,
                ));
            }
            "normal" => {
                return Some((TextSpacingTrimMode::Normal, TextAutospaceValue::Normal));
            }
            "auto" => {
                return Some((TextSpacingTrimMode::Auto, TextAutospaceValue::Auto));
            }
            _ => {}
        }
    }
    let mut trim: Option<TextSpacingTrimMode> = None;
    let mut auto_toks: Vec<&str> = Vec::new();
    let mut normal_count = 0usize;
    for tok in toks {
        match tok {
            "none" | "auto" => return None,
            "normal" => normal_count += 1,
            "space-all" | "space-first" | "trim-start" | "trim-both" | "trim-all" => {
                if trim.is_some() {
                    return None;
                }
                trim = TextSpacingTrimMode::parse(tok);
            }
            "no-autospace" | "ideograph-alpha" | "ideograph-numeric" | "punctuation" | "insert"
            | "replace" => {
                auto_toks.push(tok);
            }
            _ => return None,
        }
    }
    let autospace = if !auto_toks.is_empty() {
        Some(TextAutospaceValue::parse(&auto_toks.join(" "))?)
    } else {
        None
    };
    let mut t = trim;
    let mut a = autospace;
    for _ in 0..normal_count {
        if t.is_none() {
            t = Some(TextSpacingTrimMode::Normal);
        } else if a.is_none() {
            a = Some(TextAutospaceValue::Normal);
        } else {
            return None;
        }
    }
    Some((
        t.unwrap_or(TextSpacingTrimMode::Normal),
        a.unwrap_or(TextAutospaceValue::Normal),
    ))
}

/// Canonical serialization of `text-spacing`.
#[must_use]
pub fn serialize_text_spacing(trim: TextSpacingTrimMode, autospace: TextAutospaceValue) -> String {
    match (trim, autospace) {
        (TextSpacingTrimMode::Normal, TextAutospaceValue::Normal) => "normal".to_string(),
        (TextSpacingTrimMode::SpaceAll, TextAutospaceValue::NoAutospace) => "none".to_string(),
        (TextSpacingTrimMode::Auto, TextAutospaceValue::Auto) => "auto".to_string(),
        (TextSpacingTrimMode::Normal, a) => a.serialize(),
        (t, TextAutospaceValue::Normal) => t.as_str().to_string(),
        (t, a) => format!("{} {}", t.as_str(), a.serialize()),
    }
}

/// W3C CSS Text L3 `hanging-punctuation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct HangingPunctuation(pub u8);

impl HangingPunctuation {
    pub const NONE: Self = Self(0);
    pub const FIRST: u8 = 1 << 0;
    pub const FORCE_END: u8 = 1 << 1;
    pub const ALLOW_END: u8 = 1 << 2;
    pub const LAST: u8 = 1 << 3;

    /// Parses `hanging-punctuation`, returning `(bitmask, preserved_specified_string)`.
    #[must_use]
    pub fn parse_with_specified(raw: &str) -> Option<(Self, String)> {
        let lower = raw.trim().to_ascii_lowercase();
        let toks: Vec<&str> = lower.split_ascii_whitespace().collect();
        if toks.is_empty() {
            return None;
        }
        if toks.len() == 1 && toks[0] == "none" {
            return Some((Self::NONE, "none".to_string()));
        }
        let mut bits = 0u8;
        for &tok in &toks {
            let bit = match tok {
                "first" => Self::FIRST,
                "force-end" => Self::FORCE_END,
                "allow-end" => Self::ALLOW_END,
                "last" => Self::LAST,
                _ => return None,
            };
            if bits & bit != 0 {
                return None;
            }
            if (bit == Self::FORCE_END && bits & Self::ALLOW_END != 0)
                || (bit == Self::ALLOW_END && bits & Self::FORCE_END != 0)
            {
                return None;
            }
            bits |= bit;
        }
        Some((Self(bits), toks.join(" ")))
    }

    #[must_use]
    pub fn serialize_canonical(self) -> String {
        if self.0 == 0 {
            return "none".to_string();
        }
        let mut parts = Vec::with_capacity(3);
        if self.0 & Self::FIRST != 0 {
            parts.push("first");
        }
        if self.0 & Self::FORCE_END != 0 {
            parts.push("force-end");
        } else if self.0 & Self::ALLOW_END != 0 {
            parts.push("allow-end");
        }
        if self.0 & Self::LAST != 0 {
            parts.push("last");
        }
        parts.join(" ")
    }
}

/// W3C CSS Text L3 `hyphens`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum HyphensMode {
    None,
    #[default]
    Manual,
    Auto,
}

impl HyphensMode {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "manual" => Some(Self::Manual),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Manual => "manual",
            Self::Auto => "auto",
        }
    }
}

/// W3C CSS Text L3 §5.3 `line-break`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum LineBreakMode {
    #[default]
    Auto,
    Loose,
    Normal,
    Strict,
    Anywhere,
}

impl LineBreakMode {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "loose" => Some(Self::Loose),
            "normal" => Some(Self::Normal),
            "strict" => Some(Self::Strict),
            "anywhere" => Some(Self::Anywhere),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Loose => "loose",
            Self::Normal => "normal",
            Self::Strict => "strict",
            Self::Anywhere => "anywhere",
        }
    }
}

/// W3C CSS Text L4 §6.3 `hyphenate-limit-chars` (`[ auto | <integer> ]{1,3}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct HyphenateLimitChars {
    pub word: Option<i32>,
    pub before: Option<i32>,
    pub after: Option<i32>,
}

impl HyphenateLimitChars {
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let parts = split_top_level_whitespace(raw);
        if parts.is_empty() || parts.len() > 3 {
            return None;
        }
        let mut parsed = [None; 3];
        for (i, part) in parts.iter().enumerate() {
            if part.eq_ignore_ascii_case("auto") {
                parsed[i] = None;
            } else {
                let pm = parse_math_value(part, ValueKind::Integer)?;
                let v = pm.node.eval_scalar(&EvalContext::default(), None);
                if !v.is_finite() {
                    return None;
                }
                let n = (v + 0.5).floor() as i32;
                parsed[i] = Some(n);
            }
        }
        let (word, before, after) = match parts.len() {
            1 => (parsed[0], None, None),
            2 => (parsed[0], parsed[1], parsed[1]),
            _ => (parsed[0], parsed[1], parsed[2]),
        };
        Some(Self {
            word,
            before,
            after,
        })
    }

    #[must_use]
    pub fn serialize(self) -> String {
        let fmt = |v: Option<i32>| match v {
            None => "auto".to_string(),
            Some(n) => n.to_string(),
        };
        if self.before == self.after {
            if self.before.is_none() {
                fmt(self.word)
            } else {
                format!("{} {}", fmt(self.word), fmt(self.before))
            }
        } else {
            format!(
                "{} {} {}",
                fmt(self.word),
                fmt(self.before),
                fmt(self.after)
            )
        }
    }

    #[must_use]
    pub fn interpolate(self, to: Self, p: f64, use_from_disc: bool) -> Self {
        let interp_opt = |a: Option<i32>, b: Option<i32>| -> Option<Option<i32>> {
            match (a, b) {
                (None, None) => Some(None),
                (Some(av), Some(bv)) => {
                    let v = ((f64::from(av) * (1.0 - p) + f64::from(bv) * p) + 0.5).floor() as i32;
                    Some(Some(v))
                }
                _ => None,
            }
        };
        if let (Some(w), Some(b), Some(a)) = (
            interp_opt(self.word, to.word),
            interp_opt(self.before, to.before),
            interp_opt(self.after, to.after),
        ) {
            Self {
                word: w,
                before: b,
                after: a,
            }
        } else if use_from_disc {
            self
        } else {
            to
        }
    }
}

/// W3C CSS Text L3/L4 + MathML Core `text-transform` bitmask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct TextTransformFlags(pub u8);

impl TextTransformFlags {
    pub const NONE: Self = Self(0);
    pub const CASE_MASK: u8 = 0b11;
    pub const CAPITALIZE: u8 = 1;
    pub const UPPERCASE: u8 = 2;
    pub const LOWERCASE: u8 = 3;
    pub const FULL_WIDTH: u8 = 1 << 2;
    pub const FULL_SIZE_KANA: u8 = 1 << 3;
    pub const MATH_AUTO: u8 = 1 << 4;

    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn case_bits(self) -> u8 {
        self.0 & Self::CASE_MASK
    }

    #[must_use]
    pub const fn is_full_width(self) -> bool {
        self.0 & Self::FULL_WIDTH != 0
    }

    #[must_use]
    pub const fn is_full_size_kana(self) -> bool {
        self.0 & Self::FULL_SIZE_KANA != 0
    }

    #[must_use]
    pub const fn is_math_auto(self) -> bool {
        self.0 & Self::MATH_AUTO != 0
    }

    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let lower = raw.trim().to_ascii_lowercase();
        let toks: Vec<&str> = lower.split_ascii_whitespace().collect();
        if toks.is_empty() {
            return None;
        }
        if toks.len() == 1 {
            match toks[0] {
                "none" => return Some(Self::NONE),
                "math-auto" => return Some(Self(Self::MATH_AUTO)),
                _ => {}
            }
        }
        let mut bits = 0u8;
        for tok in toks {
            match tok {
                "capitalize" => {
                    if bits & Self::CASE_MASK != 0 {
                        return None;
                    }
                    bits |= Self::CAPITALIZE;
                }
                "uppercase" => {
                    if bits & Self::CASE_MASK != 0 {
                        return None;
                    }
                    bits |= Self::UPPERCASE;
                }
                "lowercase" => {
                    if bits & Self::CASE_MASK != 0 {
                        return None;
                    }
                    bits |= Self::LOWERCASE;
                }
                "full-width" => {
                    if bits & Self::FULL_WIDTH != 0 {
                        return None;
                    }
                    bits |= Self::FULL_WIDTH;
                }
                "full-size-kana" => {
                    if bits & Self::FULL_SIZE_KANA != 0 {
                        return None;
                    }
                    bits |= Self::FULL_SIZE_KANA;
                }
                _ => return None,
            }
        }
        Some(Self(bits))
    }

    #[must_use]
    pub fn serialize(self) -> String {
        if self.0 == 0 {
            return "none".to_string();
        }
        if self.is_math_auto() {
            return "math-auto".to_string();
        }
        let mut parts = Vec::with_capacity(3);
        match self.case_bits() {
            Self::CAPITALIZE => parts.push("capitalize"),
            Self::UPPERCASE => parts.push("uppercase"),
            Self::LOWERCASE => parts.push("lowercase"),
            _ => {}
        }
        if self.is_full_width() {
            parts.push("full-width");
        }
        if self.is_full_size_kana() {
            parts.push("full-size-kana");
        }
        parts.join(" ")
    }
}

/// W3C CSS Text L5 `text-fit` method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum TextFitMethod {
    #[default]
    Consistent,
    PerLine,
    PerLineAll,
}

impl TextFitMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Consistent => "consistent",
            Self::PerLine => "per-line",
            Self::PerLineAll => "per-line-all",
        }
    }
}

/// W3C CSS Text L5 `text-fit` (`none | [ grow | shrink ] [ consistent | per-line | per-line-all ]? <percentage>?`).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum TextFitValue {
    #[default]
    None,
    Fit {
        grow: bool,
        method: TextFitMethod,
        explicit_method: bool,
        limit_pct: Option<f32>,
    },
}

impl TextFitValue {
    #[must_use]
    pub fn parse_with_specified(raw: &str) -> Option<(Self, String)> {
        let lower = raw.trim().to_ascii_lowercase();
        let toks: Vec<&str> = lower.split_ascii_whitespace().collect();
        if toks.is_empty() || toks.len() > 3 {
            return None;
        }
        if toks.len() == 1 && toks[0] == "none" {
            return Some((Self::None, "none".to_string()));
        }
        let grow = match toks[0] {
            "grow" => true,
            "shrink" => false,
            _ => return None,
        };
        let mut method = TextFitMethod::Consistent;
        let mut explicit_method = false;
        let mut limit_pct: Option<f32> = None;
        for &tok in &toks[1..] {
            match tok {
                "consistent" if !explicit_method && limit_pct.is_none() => {
                    method = TextFitMethod::Consistent;
                    explicit_method = true;
                }
                "per-line" if !explicit_method && limit_pct.is_none() => {
                    method = TextFitMethod::PerLine;
                    explicit_method = true;
                }
                "per-line-all" if !explicit_method && limit_pct.is_none() => {
                    method = TextFitMethod::PerLineAll;
                    explicit_method = true;
                }
                _ if limit_pct.is_none() => {
                    let num_str = tok.strip_suffix('%')?;
                    let v: f32 = num_str.parse().ok()?;
                    if !v.is_finite() || v < 0.0 {
                        return None;
                    }
                    limit_pct = Some(v);
                }
                _ => return None,
            }
        }
        let val = Self::Fit {
            grow,
            method,
            explicit_method,
            limit_pct,
        };
        Some((val, val.serialize_specified()))
    }

    #[must_use]
    pub fn serialize_specified(self) -> String {
        match self {
            Self::None => "none".to_string(),
            Self::Fit {
                grow,
                method,
                explicit_method,
                limit_pct,
            } => {
                let mut parts = vec![if grow { "grow" } else { "shrink" }.to_string()];
                if explicit_method {
                    parts.push(method.as_str().to_string());
                }
                if let Some(p) = limit_pct {
                    parts.push(format!(
                        "{}%",
                        crate::values_math::format_math_f64(f64::from(p))
                    ));
                }
                parts.join(" ")
            }
        }
    }

    #[must_use]
    pub fn serialize_computed(self) -> String {
        match self {
            Self::None => "none".to_string(),
            Self::Fit {
                grow,
                method,
                limit_pct,
                ..
            } => {
                let mut parts = vec![if grow { "grow" } else { "shrink" }.to_string()];
                if method != TextFitMethod::Consistent {
                    parts.push(method.as_str().to_string());
                }
                if let Some(p) = limit_pct {
                    parts.push(format!(
                        "{}%",
                        crate::values_math::format_math_f64(f64::from(p))
                    ));
                }
                parts.join(" ")
            }
        }
    }
}

/// W3C CSS Text L4 `text-group-align`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum TextGroupAlignMode {
    #[default]
    None,
    Start,
    End,
    Left,
    Right,
    Center,
}

impl TextGroupAlignMode {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "start" => Some(Self::Start),
            "end" => Some(Self::End),
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "center" => Some(Self::Center),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Start => "start",
            Self::End => "end",
            Self::Left => "left",
            Self::Right => "right",
            Self::Center => "center",
        }
    }
}

/// W3C CSS Text L4 `word-space-transform` (`none | [ space | ideographic-space ] && auto-phrase?`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum WordSpaceTransformMode {
    #[default]
    None,
    Space,
    IdeographicSpace,
    SpaceAutoPhrase,
    IdeographicSpaceAutoPhrase,
}

impl WordSpaceTransformMode {
    #[must_use]
    pub fn parse_with_specified(raw: &str) -> Option<(Self, String)> {
        let lower = raw.trim().to_ascii_lowercase();
        let toks: Vec<&str> = lower.split_ascii_whitespace().collect();
        if toks.is_empty() || toks.len() > 2 {
            return None;
        }
        if toks.len() == 1 && toks[0] == "none" {
            return Some((Self::None, "none".to_string()));
        }
        let mut kind: Option<bool> = None; // false=space, true=ideographic-space
        let mut auto_phrase = false;
        for &tok in &toks {
            match tok {
                "space" if kind.is_none() => kind = Some(false),
                "ideographic-space" if kind.is_none() => kind = Some(true),
                "auto-phrase" if !auto_phrase => auto_phrase = true,
                _ => return None,
            }
        }
        let is_ideo = kind?;
        let mode = match (is_ideo, auto_phrase) {
            (false, false) => Self::Space,
            (true, false) => Self::IdeographicSpace,
            (false, true) => Self::SpaceAutoPhrase,
            (true, true) => Self::IdeographicSpaceAutoPhrase,
        };
        Some((mode, toks.join(" ")))
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Space => "space",
            Self::IdeographicSpace => "ideographic-space",
            Self::SpaceAutoPhrase => "space auto-phrase",
            Self::IdeographicSpaceAutoPhrase => "ideographic-space auto-phrase",
        }
    }
}

/// W3C CSS Text L4 `wrap-inside` (non-inherited: `auto | avoid`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum WrapInsideMode {
    #[default]
    Auto,
    Avoid,
}

impl WrapInsideMode {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "avoid" => Some(Self::Avoid),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Avoid => "avoid",
        }
    }
}

/// Parses `word-break` (`normal | break-all | keep-all | manual | auto-phrase | break-word`).
#[must_use]
pub fn parse_word_break(s: &str) -> Option<(WordBreakMode, &'static str)> {
    match s.trim().to_ascii_lowercase().as_str() {
        "normal" => Some((WordBreakMode::Normal, "normal")),
        "break-all" => Some((WordBreakMode::BreakAll, "break-all")),
        "keep-all" => Some((WordBreakMode::KeepAll, "keep-all")),
        "manual" => Some((WordBreakMode::Manual, "manual")),
        "auto-phrase" => Some((WordBreakMode::AutoPhrase, "auto-phrase")),
        "break-word" => Some((WordBreakMode::BreakWord, "break-word")),
        _ => None,
    }
}

/// Parses `text-justify` (`auto | none | inter-word | inter-character | distribute`).
/// Per CSS Text L3 / CSSWG #6156, `distribute` is a parse-time alias of `inter-character`.
#[must_use]
pub fn parse_text_justify(s: &str) -> Option<(TextJustifyMode, &'static str)> {
    match s.trim().to_ascii_lowercase().as_str() {
        "auto" => Some((TextJustifyMode::Auto, "auto")),
        "none" => Some((TextJustifyMode::None, "none")),
        "inter-word" => Some((TextJustifyMode::InterWord, "inter-word")),
        "inter-character" | "distribute" => {
            Some((TextJustifyMode::InterCharacter, "inter-character"))
        }
        _ => None,
    }
}

/// Parses `text-indent` (`<length-percentage> && hanging? && each-line?`),
/// returning `(lp, hanging, each_line, canonical_specified)`.
#[must_use]
pub fn parse_text_indent(raw: &str) -> Option<(LengthPercentage, bool, bool, String)> {
    let parts = split_top_level_whitespace(raw);
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut lp_val: Option<(LengthPercentage, String)> = None;
    let mut hanging = false;
    let mut each_line = false;
    for part in parts {
        if part.eq_ignore_ascii_case("hanging") {
            if hanging {
                return None;
            }
            hanging = true;
        } else if part.eq_ignore_ascii_case("each-line") {
            if each_line {
                return None;
            }
            each_line = true;
        } else {
            if lp_val.is_some() {
                return None;
            }
            let (lp, ser) = parse_strict_lp(part)?;
            lp_val = Some((lp, ser));
        }
    }
    let (lp, mut ser) = lp_val?;
    if hanging {
        ser.push_str(" hanging");
    }
    if each_line {
        ser.push_str(" each-line");
    }
    Some((lp, hanging, each_line, ser))
}

/// Parses `word-spacing` or `letter-spacing` (`normal | <length-percentage>`).
/// Rejects bare non-zero unitless numbers and canonicalizes `0` -> `0px`.
#[must_use]
pub fn parse_spacing_value(raw: &str) -> Option<(LengthPercentage, String)> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("normal") {
        return Some((LengthPercentage::Zero, "normal".to_string()));
    }
    parse_strict_lp(trimmed)
}

/// Parses a strict `<length-percentage>` rejecting bare non-zero unitless numbers and
/// canonicalizing `0` -> `0px`.
fn parse_strict_lp(raw: &str) -> Option<(LengthPercentage, String)> {
    let trimmed = raw.trim();
    if let Ok(n) = trimmed.parse::<f64>() {
        if n == 0.0 {
            return Some((LengthPercentage::Zero, "0px".to_string()));
        }
        return None;
    }
    let pm = parse_math_value(trimmed, ValueKind::LengthPercentage)?;
    let mut ser = pm.node.serialize_specified(pm.was_math_fn);
    if ser == "0" {
        ser = "0px".to_string();
    }
    let lp = LengthPercentage::Calc(Box::new(crate::values::CalcExpr::Math(Box::new(pm.node))));
    Some((lp, ser))
}

/// Splits `s` on ASCII whitespace at parenthesis depth 0 (preserving `calc(...)` intact).
#[must_use]
pub fn split_top_level_whitespace(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => {
                if start.is_none() {
                    start = Some(i);
                }
                depth += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
            }
            c if depth == 0 && c.is_ascii_whitespace() => {
                if let Some(st) = start.take() {
                    out.push(&s[st..i]);
                }
            }
            _ => {
                if start.is_none() {
                    start = Some(i);
                }
            }
        }
    }
    if let Some(st) = start {
        out.push(&s[st..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_space_shorthand_parsing_and_canonical_serialization() {
        assert_eq!(
            parse_white_space_shorthand("collapse wrap")
                .map(|(c, w, t)| serialize_white_space(c, w, t)),
            Some("normal".to_string())
        );
        assert_eq!(
            parse_white_space_shorthand("nowrap preserve")
                .map(|(c, w, t)| serialize_white_space(c, w, t)),
            Some("pre".to_string())
        );
        assert_eq!(
            parse_white_space_shorthand(
                "discard-inner discard-after discard-before preserve nowrap"
            )
            .map(|(c, w, t)| serialize_white_space(c, w, t)),
            Some("preserve nowrap discard-before discard-after discard-inner".to_string())
        );
        assert!(parse_white_space_shorthand("normal discard-before").is_none());
        assert!(parse_white_space_shorthand("preserve balance").is_none());
    }

    #[test]
    fn text_wrap_and_text_spacing_shorthands_serialize_canonically() {
        assert_eq!(
            parse_text_wrap_shorthand("pretty wrap").map(|(m, s)| serialize_text_wrap(m, s)),
            Some("pretty".to_string())
        );
        assert_eq!(
            parse_text_wrap_shorthand("balance nowrap").map(|(m, s)| serialize_text_wrap(m, s)),
            Some("nowrap balance".to_string())
        );
        assert_eq!(
            parse_text_spacing_shorthand("no-autospace space-all")
                .map(|(t, a)| serialize_text_spacing(t, a)),
            Some("none".to_string())
        );
        assert_eq!(
            parse_text_spacing_shorthand("no-autospace trim-start")
                .map(|(t, a)| serialize_text_spacing(t, a)),
            Some("trim-start no-autospace".to_string())
        );
    }

    #[test]
    fn hyphenate_limit_chars_parses_serializes_and_interpolates() {
        let a = HyphenateLimitChars::parse("5 1 2").unwrap();
        let b = HyphenateLimitChars::parse("17 5 10").unwrap();
        assert_eq!(a.interpolate(b, -0.5, true).serialize(), "-1 -1 -2");
        assert_eq!(a.interpolate(b, -0.25, true).serialize(), "2 0");
        assert_eq!(
            HyphenateLimitChars::parse("auto auto auto")
                .unwrap()
                .serialize(),
            "auto"
        );
        assert_eq!(
            HyphenateLimitChars::parse("auto 2 2").unwrap().serialize(),
            "auto 2"
        );
        assert_eq!(
            HyphenateLimitChars::parse("auto 2 auto")
                .unwrap()
                .serialize(),
            "auto 2 auto"
        );
    }
}
