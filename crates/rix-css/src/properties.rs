//! Typed CSS property declarations, shorthand expansion, and token streams.
//!
//! Zero CSS-text round-tripping: declarations compile directly from `lightningcss`
//! AST nodes into typed [`PropertyValue`] enums or owned [`CssToken`] streams
//! for `var()` substitution.

use std::fmt::Write as _;

use lightningcss::declaration::DeclarationBlock;
use lightningcss::properties::{
    custom::{CustomPropertyName, Token as LightningToken, TokenList, TokenOrValue},
    size::BoxSizing,
    Property, PropertyId,
};
use lightningcss::stylesheet::{ParserOptions, PrinterOptions};
use rix_core::{intern_str, InternedStr};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::values::{
    angle_to_deg, convert_align_content, convert_align_items, convert_align_self, convert_alpha,
    convert_animation, convert_animation_direction, convert_animation_fill,
    convert_animation_iteration, convert_animation_name, convert_animation_play_state,
    convert_background_layer, convert_blend_mode, convert_border_width, convert_box_shadow,
    convert_clip_path, convert_color, convert_corner_radius, convert_cursor, convert_direction,
    convert_display, convert_easing, convert_filter, convert_flex_direction, convert_flex_wrap,
    convert_font_family_name, convert_font_size, convert_font_style, convert_font_weight,
    convert_grid_auto_flow, convert_image, convert_justify_content, convert_justify_items,
    convert_justify_self, convert_length, convert_length_percentage,
    convert_length_percentage_or_auto, convert_length_value, convert_line_height,
    convert_line_style, convert_max_size, convert_overflow, convert_overflow_wrap,
    convert_position, convert_size, convert_spacing, convert_text_align,
    convert_text_decoration_line, convert_text_transform, convert_track_size, convert_track_sizing,
    convert_transform, convert_transform_origin, convert_transition, convert_user_select,
    convert_visibility, convert_white_space, convert_word_break, convert_z_index, time_to_ms,
    AlignValue, AnimationDirectionMode, AnimationFillModeKind, AnimationPlayStateMode,
    AnimationSpec, BackgroundImageValue, BackgroundLayerValue, BorderCollapseMode, BoxShadowValue,
    BoxSizingMode, ClearMode, ClipPathValue, CornerRadius, CssColor, CursorKind, DirectionMode,
    DisplayMode, FilterOp, FlexDirectionMode, FlexWrapMode, FloatMode, FontStyleMode,
    GridAutoFlowMode, GridPlacementValue, GridTemplateAreasValue, GridTrackSize, LengthPercentage,
    LengthPercentageAuto, LengthUnit, LineHeightValue, LineStyle, MixBlendMode, OverflowMode,
    OverflowWrapMode, PointerEventsMode, PositionMode, SizeValue, TableLayoutMode, TextAlignMode,
    TextDecorationLine, TextOverflowMode, TextTransformMode, TimingFunction, TransformOp,
    TransformOrigin, TransitionSpec, UserSelectMode, VerticalAlignMode, VisibilityMode,
    WhiteSpaceMode, WordBreakMode, ZIndexValue,
};

/// Category of an unsupported CSS feature recorded during compilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MissingCssKind {
    AtRule,
    Property,
    Selector,
    Value,
}

/// Deduplicated telemetry entry for an unsupported CSS feature.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MissingCssFeature {
    pub kind: MissingCssKind,
    pub name: InternedStr,
}

impl MissingCssFeature {
    #[must_use]
    pub fn new(kind: MissingCssKind, name: &str) -> Self {
        Self {
            kind,
            name: intern_str(name),
        }
    }
}

/// CSS-wide cascading keyword.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum GlobalKeyword {
    Initial,
    Inherit,
    Unset,
    Revert,
    RevertLayer,
}

impl GlobalKeyword {
    #[must_use]
    pub fn from_ident(ident: &str) -> Option<Self> {
        match ident.to_ascii_lowercase().as_str() {
            "initial" => Some(Self::Initial),
            "inherit" => Some(Self::Inherit),
            "unset" => Some(Self::Unset),
            "revert" => Some(Self::Revert),
            "revert-layer" => Some(Self::RevertLayer),
            _ => None,
        }
    }
}

/// Owned CSS token for custom properties and deferred `var()` substitution.
#[derive(Debug, Clone, PartialEq)]
pub enum CssToken {
    Ident(InternedStr),
    Number(f32),
    Percentage(f32),
    Dimension(f32, InternedStr),
    Color(CssColor),
    String(InternedStr),
    Url(InternedStr),
    Hash(InternedStr),
    Delim(char),
    Comma,
    Colon,
    Space,
    ParenOpen,
    ParenClose,
    Function {
        name: InternedStr,
        args: Vec<CssToken>,
    },
    Var {
        name: InternedStr,
        fallback: Option<Vec<CssToken>>,
    },
}

/// Converts a `lightningcss` [`TokenList`] directly into owned [`CssToken`]s.
#[must_use]
pub fn convert_token_list(list: &TokenList<'_>) -> Vec<CssToken> {
    let mut out = Vec::with_capacity(list.0.len());
    for item in &list.0 {
        convert_token_or_value(item, &mut out);
    }
    out
}

fn convert_token_or_value(item: &TokenOrValue<'_>, out: &mut Vec<CssToken>) {
    match item {
        TokenOrValue::Token(tok) => match tok {
            LightningToken::Ident(s) => out.push(CssToken::Ident(intern_str(s.as_ref()))),
            LightningToken::Number { value, .. } => out.push(CssToken::Number(*value)),
            LightningToken::Percentage { unit_value, .. } => {
                out.push(CssToken::Percentage(*unit_value));
            }
            LightningToken::Dimension { value, unit, .. } => {
                out.push(CssToken::Dimension(*value, intern_str(unit.as_ref())));
            }
            LightningToken::String(s) => out.push(CssToken::String(intern_str(s.as_ref()))),
            LightningToken::UnquotedUrl(u) => out.push(CssToken::Url(intern_str(u.as_ref()))),
            LightningToken::Hash(h) | LightningToken::IDHash(h) => {
                out.push(CssToken::Hash(intern_str(h.as_ref())));
            }
            LightningToken::Delim(c) => out.push(CssToken::Delim(*c)),
            LightningToken::Comma => out.push(CssToken::Comma),
            LightningToken::Colon => out.push(CssToken::Colon),
            LightningToken::WhiteSpace(_) => out.push(CssToken::Space),
            LightningToken::ParenthesisBlock => out.push(CssToken::ParenOpen),
            LightningToken::CloseParenthesis => out.push(CssToken::ParenClose),
            LightningToken::Function(name) => out.push(CssToken::Ident(intern_str(name.as_ref()))),
            _ => {}
        },
        TokenOrValue::Color(c) => {
            if let Some(color) = convert_color(c) {
                out.push(CssToken::Color(color));
            }
        }
        TokenOrValue::Url(u) => out.push(CssToken::Url(intern_str(u.url.as_ref()))),
        TokenOrValue::Var(v) => {
            let fallback = v.fallback.as_ref().map(convert_token_list);
            out.push(CssToken::Var {
                name: intern_str(v.name.ident.as_ref()),
                fallback,
            });
        }
        TokenOrValue::Function(f) => out.push(CssToken::Function {
            name: intern_str(f.name.as_ref()),
            args: convert_token_list(&f.arguments),
        }),
        TokenOrValue::Length(lv) => {
            let (v, u) = lv.to_unit_value();
            out.push(CssToken::Dimension(v, intern_str(u)));
        }
        TokenOrValue::Angle(a) => {
            out.push(CssToken::Dimension(angle_to_deg(a), intern_str("deg")));
        }
        TokenOrValue::Time(t) => {
            let ms = match t {
                lightningcss::values::time::Time::Seconds(s) => *s * 1000.0,
                lightningcss::values::time::Time::Milliseconds(m) => *m,
            };
            out.push(CssToken::Dimension(ms, intern_str("ms")));
        }
        TokenOrValue::DashedIdent(id) => out.push(CssToken::Ident(intern_str(id.as_ref()))),
        TokenOrValue::Resolution(_)
        | TokenOrValue::Env(_)
        | TokenOrValue::UnresolvedColor(_)
        | TokenOrValue::AnimationName(_) => {}
    }
}

/// Formats a resolved [`CssToken`] slice directly into a reusable string buffer
/// for re-parsing complex multi-token declarations after `var()` expansion.
pub fn write_tokens_to_buf(tokens: &[CssToken], buf: &mut String) {
    for tok in tokens {
        match tok {
            CssToken::Ident(s) => buf.push_str(s.as_str()),
            CssToken::Number(n) => {
                let _ = write!(buf, "{n}");
            }
            CssToken::Percentage(p) => {
                let _ = write!(buf, "{}%", p * 100.0);
            }
            CssToken::Dimension(v, u) => {
                let _ = write!(buf, "{v}{}", u.as_str());
            }
            CssToken::Color(c) => match c {
                CssColor::CurrentColor => buf.push_str("currentColor"),
                CssColor::Rgba(rgba) => {
                    if rgba.a == 255 {
                        let _ = write!(buf, "rgb({}, {}, {})", rgba.r, rgba.g, rgba.b);
                    } else {
                        let a = f32::from(rgba.a) / 255.0;
                        let _ = write!(buf, "rgba({}, {}, {}, {a})", rgba.r, rgba.g, rgba.b);
                    }
                }
                CssColor::LightDark(l, d) => {
                    let _ = write!(
                        buf,
                        "light-dark(rgb({},{},{}), rgb({},{},{}))",
                        l.r, l.g, l.b, d.r, d.g, d.b
                    );
                }
                CssColor::SystemAccent => buf.push_str("rgb(0, 96, 223)"),
            },
            CssToken::String(s) => {
                buf.push('"');
                buf.push_str(s.as_str());
                buf.push('"');
            }
            CssToken::Url(u) => {
                let _ = write!(buf, "url(\"{}\")", u.as_str());
            }
            CssToken::Hash(h) => {
                buf.push('#');
                buf.push_str(h.as_str());
            }
            CssToken::Delim(c) => buf.push(*c),
            CssToken::Comma => buf.push(','),
            CssToken::Colon => buf.push(':'),
            CssToken::Space => buf.push(' '),
            CssToken::ParenOpen => buf.push('('),
            CssToken::ParenClose => buf.push(')'),
            CssToken::Function { name, args } => {
                buf.push_str(name.as_str());
                buf.push('(');
                write_tokens_to_buf(args, buf);
                buf.push(')');
            }
            CssToken::Var { name, fallback } => {
                buf.push_str("var(");
                buf.push_str(name.as_str());
                if let Some(fb) = fallback {
                    buf.push_str(", ");
                    write_tokens_to_buf(fb, buf);
                }
                buf.push(')');
            }
        }
    }
}

/// Serializes a slice of [`CssToken`]s into CSS text (for CSSOM custom property getters).
#[must_use]
pub fn serialize_css_tokens(tokens: &[CssToken]) -> String {
    let mut buf = String::with_capacity(tokens.len() * 6);
    write_tokens_to_buf(tokens, &mut buf);
    buf
}

/// Validates and canonically serializes a single CSS property declaration (`name: value`).
///
/// Returns `None` if `name` is unsupported or `value` fails CSS syntax validation.
/// Follows CSSOM §6.7.2 (`setProperty` / `CSS.supports(property, value)`).
#[must_use]
pub fn parse_and_serialize_declaration(name: &str, value: &str) -> Option<String> {
    if name.starts_with("--") {
        // CSS Custom Properties L1 §2: `--` alone is reserved and not a valid custom property name.
        if name.len() <= 2 || value.is_empty() || !has_balanced_css_tokens(value) {
            return None;
        }
        return Some(value.to_string());
    }
    let trimmed = value.trim();
    if trimmed.is_empty() || !has_balanced_css_tokens(trimmed) {
        return None;
    }
    let lower_name = name.to_ascii_lowercase();
    let prop_kind = PropertyIdKind::from_name(&lower_name)?;

    // Reject `!important` inside property value (CSSOM requires priority as 3rd arg of setProperty).
    if has_top_level_bang_or_semicolon(trimmed) {
        return None;
    }

    let lower_val = trimmed.to_ascii_lowercase();
    if GlobalKeyword::from_ident(&lower_val).is_some() {
        return Some(lower_val);
    }

    let parsed_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let opts = ParserOptions::default();
        match Property::parse_string(PropertyId::from(lower_name.as_str()), trimmed, opts) {
            Ok(Property::Unparsed(_)) => {
                if trimmed.contains("var(") || trimmed.contains("env(") {
                    Some(trimmed.to_string())
                } else {
                    None
                }
            }
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
                    Some(trimmed.to_string())
                }
            }
            Ok(parsed) => {
                let _ = prop_kind;
                if let Some(serialized) = serialize_cssom_color_property(&parsed, trimmed) {
                    return Some(serialized);
                }
                parsed
                    .value_to_css_string(PrinterOptions::default())
                    .ok()
                    .or_else(|| Some(trimmed.to_string()))
            }
            Err(_) => None,
        }
    }));
    parsed_res.ok().flatten()
}

fn serialize_cssom_color_property(prop: &Property<'_>, raw_input: &str) -> Option<String> {
    let color = match prop {
        Property::Color(c)
        | Property::BackgroundColor(c)
        | Property::BorderTopColor(c)
        | Property::BorderRightColor(c)
        | Property::BorderBottomColor(c)
        | Property::BorderLeftColor(c)
        | Property::OutlineColor(c)
        | Property::TextDecorationColor(c, _) => c,
        _ => return None,
    };
    if matches!(color, lightningcss::values::color::CssColor::CurrentColor) {
        return Some("currentcolor".to_string());
    }
    // Preserve specified CSS named color keywords (e.g. `red`, `transparent`) per CSSOM.
    if raw_input.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Some(raw_input.to_ascii_lowercase());
    }
    if let lightningcss::values::color::CssColor::RGBA(rgba) = color {
        if rgba.alpha == 255 {
            return Some(format!("rgb({}, {}, {})", rgba.red, rgba.green, rgba.blue));
        }
        let a = if rgba.alpha == 0 {
            "0".to_string()
        } else {
            let rounded = (f64::from(rgba.alpha) / 255.0 * 1000.0).round() / 1000.0;
            format!("{rounded}")
        };
        return Some(format!(
            "rgba({}, {}, {}, {a})",
            rgba.red, rgba.green, rgba.blue
        ));
    }
    None
}

fn has_balanced_css_tokens(input: &str) -> bool {
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
            _ => {}
        }
        i += 1;
    }
    in_str.is_none() && stack.is_empty()
}

fn has_top_level_bang_or_semicolon(input: &str) -> bool {
    let mut depth = 0usize;
    let mut in_str: Option<u8> = None;
    let bytes = input.as_bytes();
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
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b';' | b'!' if depth == 0 => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Supported CSS property identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum PropertyIdKind {
    // Core display & flow
    Display,
    Position,
    Float,
    Clear,
    BoxSizing,
    OverflowX,
    OverflowY,
    Visibility,
    PointerEvents,
    Cursor,
    ZIndex,
    Opacity,
    // Sizing & Box model
    Width,
    Height,
    MinWidth,
    MinHeight,
    MaxWidth,
    MaxHeight,
    MarginTop,
    MarginRight,
    MarginBottom,
    MarginLeft,
    PaddingTop,
    PaddingRight,
    PaddingBottom,
    PaddingLeft,
    Top,
    Right,
    Bottom,
    Left,
    // Borders & Outline
    BorderTopWidth,
    BorderRightWidth,
    BorderBottomWidth,
    BorderLeftWidth,
    BorderTopStyle,
    BorderRightStyle,
    BorderBottomStyle,
    BorderLeftStyle,
    BorderTopColor,
    BorderRightColor,
    BorderBottomColor,
    BorderLeftColor,
    BorderTopLeftRadius,
    BorderTopRightRadius,
    BorderBottomRightRadius,
    BorderBottomLeftRadius,
    OutlineColor,
    OutlineStyle,
    OutlineWidth,
    OutlineOffset,
    // Visual & Effects
    Color,
    BackgroundColor,
    BackgroundLayers,
    BoxShadow,
    Transform,
    TransformOrigin,
    Filter,
    ClipPath,
    MixBlendMode,
    Transition,
    TransitionProperty,
    TransitionDuration,
    TransitionDelay,
    TransitionTimingFunction,
    Animation,
    AnimationName,
    AnimationDuration,
    AnimationDelay,
    AnimationTimingFunction,
    AnimationIterationCount,
    AnimationDirection,
    AnimationFillMode,
    AnimationPlayState,
    // Typography
    FontSize,
    FontWeight,
    FontStyle,
    FontFamily,
    LineHeight,
    LetterSpacing,
    WordSpacing,
    TextAlign,
    TextDecorationLine,
    TextDecorationColor,
    TextTransform,
    WhiteSpace,
    WordBreak,
    OverflowWrap,
    Direction,
    UserSelect,
    // Flex & Grid
    FlexDirection,
    FlexWrap,
    FlexGrow,
    FlexShrink,
    FlexBasis,
    Order,
    AlignItems,
    AlignSelf,
    AlignContent,
    JustifyContent,
    JustifyItems,
    JustifySelf,
    RowGap,
    ColumnGap,
    GridTemplateColumns,
    GridTemplateRows,
    GridTemplateAreas,
    GridAutoColumns,
    GridAutoRows,
    GridAutoFlow,
    GridRowStart,
    GridRowEnd,
    GridColumnStart,
    GridColumnEnd,
    // Shorthands (retained when unresolved via `var()`)
    Margin,
    Padding,
    Inset,
    Border,
    BorderTop,
    BorderRight,
    BorderBottom,
    BorderLeft,
    BorderWidth,
    BorderStyle,
    BorderColor,
    BorderRadius,
    Flex,
    FlexFlow,
    Gap,
    Background,
    Overflow,
    Font,
    TextDecoration,
    Outline,
    PlaceContent,
    PlaceItems,
    PlaceSelf,
    VerticalAlign,
    TextOverflow,
    BorderCollapse,
    BorderSpacing,
    TableLayout,
    ColorScheme,
    Content,
    // Custom property (`--*`)
    Custom,
}

impl PropertyIdKind {
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        if name.starts_with("--") {
            return Some(Self::Custom);
        }
        Some(match name.to_ascii_lowercase().as_str() {
            "display" => Self::Display,
            "position" => Self::Position,
            "float" => Self::Float,
            "clear" => Self::Clear,
            "box-sizing" => Self::BoxSizing,
            "overflow-x" => Self::OverflowX,
            "overflow-y" => Self::OverflowY,
            "visibility" => Self::Visibility,
            "pointer-events" => Self::PointerEvents,
            "cursor" => Self::Cursor,
            "z-index" => Self::ZIndex,
            "opacity" => Self::Opacity,
            "width" | "inline-size" => Self::Width,
            "height" | "block-size" => Self::Height,
            "min-width" | "min-inline-size" => Self::MinWidth,
            "min-height" | "min-block-size" => Self::MinHeight,
            "max-width" | "max-inline-size" => Self::MaxWidth,
            "max-height" | "max-block-size" => Self::MaxHeight,
            "margin-top" | "margin-block-start" => Self::MarginTop,
            "margin-right" | "margin-inline-end" => Self::MarginRight,
            "margin-bottom" | "margin-block-end" => Self::MarginBottom,
            "margin-left" | "margin-inline-start" => Self::MarginLeft,
            "padding-top" | "padding-block-start" => Self::PaddingTop,
            "padding-right" | "padding-inline-end" => Self::PaddingRight,
            "padding-bottom" | "padding-block-end" => Self::PaddingBottom,
            "padding-left" | "padding-inline-start" => Self::PaddingLeft,
            "top" | "inset-block-start" => Self::Top,
            "right" | "inset-inline-end" => Self::Right,
            "bottom" | "inset-block-end" => Self::Bottom,
            "left" | "inset-inline-start" => Self::Left,
            "border-top-width" => Self::BorderTopWidth,
            "border-right-width" => Self::BorderRightWidth,
            "border-bottom-width" => Self::BorderBottomWidth,
            "border-left-width" => Self::BorderLeftWidth,
            "border-top-style" => Self::BorderTopStyle,
            "border-right-style" => Self::BorderRightStyle,
            "border-bottom-style" => Self::BorderBottomStyle,
            "border-left-style" => Self::BorderLeftStyle,
            "border-top-color" => Self::BorderTopColor,
            "border-right-color" => Self::BorderRightColor,
            "border-bottom-color" => Self::BorderBottomColor,
            "border-left-color" => Self::BorderLeftColor,
            "border-top-left-radius" => Self::BorderTopLeftRadius,
            "border-top-right-radius" => Self::BorderTopRightRadius,
            "border-bottom-right-radius" => Self::BorderBottomRightRadius,
            "border-bottom-left-radius" => Self::BorderBottomLeftRadius,
            "outline-color" => Self::OutlineColor,
            "outline-style" => Self::OutlineStyle,
            "outline-width" => Self::OutlineWidth,
            "outline-offset" => Self::OutlineOffset,
            "color" => Self::Color,
            "background-color" => Self::BackgroundColor,
            "background-image" => Self::BackgroundLayers,
            "box-shadow" => Self::BoxShadow,
            "transform" => Self::Transform,
            "transform-origin" => Self::TransformOrigin,
            "filter" => Self::Filter,
            "clip-path" => Self::ClipPath,
            "mix-blend-mode" => Self::MixBlendMode,
            "transition" => Self::Transition,
            "transition-property" => Self::TransitionProperty,
            "transition-duration" => Self::TransitionDuration,
            "transition-delay" => Self::TransitionDelay,
            "transition-timing-function" => Self::TransitionTimingFunction,
            "animation" => Self::Animation,
            "animation-name" => Self::AnimationName,
            "animation-duration" => Self::AnimationDuration,
            "animation-delay" => Self::AnimationDelay,
            "animation-timing-function" => Self::AnimationTimingFunction,
            "animation-iteration-count" => Self::AnimationIterationCount,
            "animation-direction" => Self::AnimationDirection,
            "animation-fill-mode" => Self::AnimationFillMode,
            "animation-play-state" => Self::AnimationPlayState,
            "font-size" => Self::FontSize,
            "font-weight" => Self::FontWeight,
            "font-style" => Self::FontStyle,
            "font-family" => Self::FontFamily,
            "line-height" => Self::LineHeight,
            "letter-spacing" => Self::LetterSpacing,
            "word-spacing" => Self::WordSpacing,
            "text-align" => Self::TextAlign,
            "text-decoration-line" => Self::TextDecorationLine,
            "text-decoration-color" => Self::TextDecorationColor,
            "text-transform" => Self::TextTransform,
            "white-space" => Self::WhiteSpace,
            "word-break" => Self::WordBreak,
            "overflow-wrap" | "word-wrap" => Self::OverflowWrap,
            "direction" => Self::Direction,
            "user-select" => Self::UserSelect,
            "flex-direction" => Self::FlexDirection,
            "flex-wrap" => Self::FlexWrap,
            "flex-grow" => Self::FlexGrow,
            "flex-shrink" => Self::FlexShrink,
            "flex-basis" => Self::FlexBasis,
            "order" => Self::Order,
            "align-items" => Self::AlignItems,
            "align-self" => Self::AlignSelf,
            "align-content" => Self::AlignContent,
            "justify-content" => Self::JustifyContent,
            "justify-items" => Self::JustifyItems,
            "justify-self" => Self::JustifySelf,
            "row-gap" => Self::RowGap,
            "column-gap" => Self::ColumnGap,
            "grid-template-columns" => Self::GridTemplateColumns,
            "grid-template-rows" => Self::GridTemplateRows,
            "grid-template-areas" => Self::GridTemplateAreas,
            "grid-auto-columns" => Self::GridAutoColumns,
            "grid-auto-rows" => Self::GridAutoRows,
            "grid-auto-flow" => Self::GridAutoFlow,
            "grid-row-start" => Self::GridRowStart,
            "grid-row-end" => Self::GridRowEnd,
            "grid-column-start" => Self::GridColumnStart,
            "grid-column-end" => Self::GridColumnEnd,
            "margin" => Self::Margin,
            "padding" => Self::Padding,
            "inset" => Self::Inset,
            "border" => Self::Border,
            "border-top" => Self::BorderTop,
            "border-right" => Self::BorderRight,
            "border-bottom" => Self::BorderBottom,
            "border-left" => Self::BorderLeft,
            "border-width" => Self::BorderWidth,
            "border-style" => Self::BorderStyle,
            "border-color" => Self::BorderColor,
            "border-radius" => Self::BorderRadius,
            "flex" => Self::Flex,
            "flex-flow" => Self::FlexFlow,
            "gap" => Self::Gap,
            "background" => Self::Background,
            "overflow" => Self::Overflow,
            "font" => Self::Font,
            "text-decoration" => Self::TextDecoration,
            "outline" => Self::Outline,
            "place-content" => Self::PlaceContent,
            "place-items" => Self::PlaceItems,
            "place-self" => Self::PlaceSelf,
            "vertical-align" => Self::VerticalAlign,
            "text-overflow" => Self::TextOverflow,
            "border-collapse" => Self::BorderCollapse,
            "border-spacing" => Self::BorderSpacing,
            "table-layout" => Self::TableLayout,
            "color-scheme" => Self::ColorScheme,
            "content" => Self::Content,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn css_name(self) -> &'static str {
        match self {
            Self::Display => "display",
            Self::Position => "position",
            Self::Float => "float",
            Self::Clear => "clear",
            Self::BoxSizing => "box-sizing",
            Self::OverflowX => "overflow-x",
            Self::OverflowY => "overflow-y",
            Self::Visibility => "visibility",
            Self::PointerEvents => "pointer-events",
            Self::Cursor => "cursor",
            Self::ZIndex => "z-index",
            Self::Opacity => "opacity",
            Self::Width => "width",
            Self::Height => "height",
            Self::MinWidth => "min-width",
            Self::MinHeight => "min-height",
            Self::MaxWidth => "max-width",
            Self::MaxHeight => "max-height",
            Self::MarginTop => "margin-top",
            Self::MarginRight => "margin-right",
            Self::MarginBottom => "margin-bottom",
            Self::MarginLeft => "margin-left",
            Self::PaddingTop => "padding-top",
            Self::PaddingRight => "padding-right",
            Self::PaddingBottom => "padding-bottom",
            Self::PaddingLeft => "padding-left",
            Self::Top => "top",
            Self::Right => "right",
            Self::Bottom => "bottom",
            Self::Left => "left",
            Self::BorderTopWidth => "border-top-width",
            Self::BorderRightWidth => "border-right-width",
            Self::BorderBottomWidth => "border-bottom-width",
            Self::BorderLeftWidth => "border-left-width",
            Self::BorderTopStyle => "border-top-style",
            Self::BorderRightStyle => "border-right-style",
            Self::BorderBottomStyle => "border-bottom-style",
            Self::BorderLeftStyle => "border-left-style",
            Self::BorderTopColor => "border-top-color",
            Self::BorderRightColor => "border-right-color",
            Self::BorderBottomColor => "border-bottom-color",
            Self::BorderLeftColor => "border-left-color",
            Self::BorderTopLeftRadius => "border-top-left-radius",
            Self::BorderTopRightRadius => "border-top-right-radius",
            Self::BorderBottomRightRadius => "border-bottom-right-radius",
            Self::BorderBottomLeftRadius => "border-bottom-left-radius",
            Self::OutlineColor => "outline-color",
            Self::OutlineStyle => "outline-style",
            Self::OutlineWidth => "outline-width",
            Self::OutlineOffset => "outline-offset",
            Self::Color => "color",
            Self::BackgroundColor => "background-color",
            Self::BackgroundLayers => "background-image",
            Self::BoxShadow => "box-shadow",
            Self::Transform => "transform",
            Self::TransformOrigin => "transform-origin",
            Self::Filter => "filter",
            Self::ClipPath => "clip-path",
            Self::MixBlendMode => "mix-blend-mode",
            Self::Transition => "transition",
            Self::TransitionProperty => "transition-property",
            Self::TransitionDuration => "transition-duration",
            Self::TransitionDelay => "transition-delay",
            Self::TransitionTimingFunction => "transition-timing-function",
            Self::Animation => "animation",
            Self::AnimationName => "animation-name",
            Self::AnimationDuration => "animation-duration",
            Self::AnimationDelay => "animation-delay",
            Self::AnimationTimingFunction => "animation-timing-function",
            Self::AnimationIterationCount => "animation-iteration-count",
            Self::AnimationDirection => "animation-direction",
            Self::AnimationFillMode => "animation-fill-mode",
            Self::AnimationPlayState => "animation-play-state",
            Self::FontSize => "font-size",
            Self::FontWeight => "font-weight",
            Self::FontStyle => "font-style",
            Self::FontFamily => "font-family",
            Self::LineHeight => "line-height",
            Self::LetterSpacing => "letter-spacing",
            Self::WordSpacing => "word-spacing",
            Self::TextAlign => "text-align",
            Self::TextDecorationLine => "text-decoration-line",
            Self::TextDecorationColor => "text-decoration-color",
            Self::TextTransform => "text-transform",
            Self::WhiteSpace => "white-space",
            Self::WordBreak => "word-break",
            Self::OverflowWrap => "overflow-wrap",
            Self::Direction => "direction",
            Self::UserSelect => "user-select",
            Self::FlexDirection => "flex-direction",
            Self::FlexWrap => "flex-wrap",
            Self::FlexGrow => "flex-grow",
            Self::FlexShrink => "flex-shrink",
            Self::FlexBasis => "flex-basis",
            Self::Order => "order",
            Self::AlignItems => "align-items",
            Self::AlignSelf => "align-self",
            Self::AlignContent => "align-content",
            Self::JustifyContent => "justify-content",
            Self::JustifyItems => "justify-items",
            Self::JustifySelf => "justify-self",
            Self::RowGap => "row-gap",
            Self::ColumnGap => "column-gap",
            Self::GridTemplateColumns => "grid-template-columns",
            Self::GridTemplateRows => "grid-template-rows",
            Self::GridTemplateAreas => "grid-template-areas",
            Self::GridAutoColumns => "grid-auto-columns",
            Self::GridAutoRows => "grid-auto-rows",
            Self::GridAutoFlow => "grid-auto-flow",
            Self::GridRowStart => "grid-row-start",
            Self::GridRowEnd => "grid-row-end",
            Self::GridColumnStart => "grid-column-start",
            Self::GridColumnEnd => "grid-column-end",
            Self::Margin => "margin",
            Self::Padding => "padding",
            Self::Inset => "inset",
            Self::Border => "border",
            Self::BorderTop => "border-top",
            Self::BorderRight => "border-right",
            Self::BorderBottom => "border-bottom",
            Self::BorderLeft => "border-left",
            Self::BorderWidth => "border-width",
            Self::BorderStyle => "border-style",
            Self::BorderColor => "border-color",
            Self::BorderRadius => "border-radius",
            Self::Flex => "flex",
            Self::FlexFlow => "flex-flow",
            Self::Gap => "gap",
            Self::Background => "background",
            Self::Overflow => "overflow",
            Self::Font => "font",
            Self::TextDecoration => "text-decoration",
            Self::Outline => "outline",
            Self::PlaceContent => "place-content",
            Self::PlaceItems => "place-items",
            Self::PlaceSelf => "place-self",
            Self::VerticalAlign => "vertical-align",
            Self::TextOverflow => "text-overflow",
            Self::BorderCollapse => "border-collapse",
            Self::BorderSpacing => "border-spacing",
            Self::TableLayout => "table-layout",
            Self::ColorScheme => "color-scheme",
            Self::Content => "content",
            Self::Custom => "--custom",
        }
    }

    #[must_use]
    pub const fn is_inherited(self) -> bool {
        matches!(
            self,
            Self::Color
                | Self::ColorScheme
                | Self::Visibility
                | Self::PointerEvents
                | Self::Cursor
                | Self::FontSize
                | Self::FontWeight
                | Self::FontStyle
                | Self::FontFamily
                | Self::LineHeight
                | Self::LetterSpacing
                | Self::WordSpacing
                | Self::TextAlign
                | Self::TextTransform
                | Self::WhiteSpace
                | Self::WordBreak
                | Self::OverflowWrap
                | Self::Direction
                | Self::Custom
        )
    }

    /// Returns the constituent longhand properties for a shorthand (or `[self]` for longhands).
    #[must_use]
    pub fn shorthand_longhands(self) -> SmallVec<[Self; 8]> {
        match self {
            Self::Margin => SmallVec::from_slice(&[
                Self::MarginTop,
                Self::MarginRight,
                Self::MarginBottom,
                Self::MarginLeft,
            ]),
            Self::Padding => SmallVec::from_slice(&[
                Self::PaddingTop,
                Self::PaddingRight,
                Self::PaddingBottom,
                Self::PaddingLeft,
            ]),
            Self::Inset => {
                SmallVec::from_slice(&[Self::Top, Self::Right, Self::Bottom, Self::Left])
            }
            Self::BorderWidth => SmallVec::from_slice(&[
                Self::BorderTopWidth,
                Self::BorderRightWidth,
                Self::BorderBottomWidth,
                Self::BorderLeftWidth,
            ]),
            Self::BorderStyle => SmallVec::from_slice(&[
                Self::BorderTopStyle,
                Self::BorderRightStyle,
                Self::BorderBottomStyle,
                Self::BorderLeftStyle,
            ]),
            Self::BorderColor => SmallVec::from_slice(&[
                Self::BorderTopColor,
                Self::BorderRightColor,
                Self::BorderBottomColor,
                Self::BorderLeftColor,
            ]),
            Self::BorderTop => SmallVec::from_slice(&[
                Self::BorderTopWidth,
                Self::BorderTopStyle,
                Self::BorderTopColor,
            ]),
            Self::BorderRight => SmallVec::from_slice(&[
                Self::BorderRightWidth,
                Self::BorderRightStyle,
                Self::BorderRightColor,
            ]),
            Self::BorderBottom => SmallVec::from_slice(&[
                Self::BorderBottomWidth,
                Self::BorderBottomStyle,
                Self::BorderBottomColor,
            ]),
            Self::BorderLeft => SmallVec::from_slice(&[
                Self::BorderLeftWidth,
                Self::BorderLeftStyle,
                Self::BorderLeftColor,
            ]),
            Self::Border => SmallVec::from_slice(&[
                Self::BorderTopWidth,
                Self::BorderRightWidth,
                Self::BorderBottomWidth,
                Self::BorderLeftWidth,
                Self::BorderTopStyle,
                Self::BorderRightStyle,
                Self::BorderBottomStyle,
                Self::BorderLeftStyle,
                Self::BorderTopColor,
                Self::BorderRightColor,
                Self::BorderBottomColor,
                Self::BorderLeftColor,
            ]),
            Self::BorderRadius => SmallVec::from_slice(&[
                Self::BorderTopLeftRadius,
                Self::BorderTopRightRadius,
                Self::BorderBottomRightRadius,
                Self::BorderBottomLeftRadius,
            ]),
            Self::Flex => {
                SmallVec::from_slice(&[Self::FlexGrow, Self::FlexShrink, Self::FlexBasis])
            }
            Self::FlexFlow => SmallVec::from_slice(&[Self::FlexDirection, Self::FlexWrap]),
            Self::Gap => SmallVec::from_slice(&[Self::RowGap, Self::ColumnGap]),
            Self::Overflow => SmallVec::from_slice(&[Self::OverflowX, Self::OverflowY]),
            Self::Background => {
                SmallVec::from_slice(&[Self::BackgroundColor, Self::BackgroundLayers])
            }
            Self::Outline => {
                SmallVec::from_slice(&[Self::OutlineWidth, Self::OutlineStyle, Self::OutlineColor])
            }
            Self::PlaceContent => SmallVec::from_slice(&[Self::AlignContent, Self::JustifyContent]),
            Self::PlaceItems => SmallVec::from_slice(&[Self::AlignItems, Self::JustifyItems]),
            Self::PlaceSelf => SmallVec::from_slice(&[Self::AlignSelf, Self::JustifySelf]),
            Self::TextDecoration => {
                SmallVec::from_slice(&[Self::TextDecorationLine, Self::TextDecorationColor])
            }
            other => {
                let mut v = SmallVec::new();
                v.push(other);
                v
            }
        }
    }
}

/// Typed specified value for any supported CSS declaration.
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyValue {
    Global(GlobalKeyword),
    Unresolved(Vec<CssToken>),
    Custom(Vec<CssToken>),
    Display(DisplayMode),
    Position(PositionMode),
    Float(FloatMode),
    Clear(ClearMode),
    BoxSizing(BoxSizingMode),
    Overflow(OverflowMode),
    Visibility(VisibilityMode),
    PointerEvents(PointerEventsMode),
    Cursor(CursorKind),
    ZIndex(ZIndexValue),
    Number(f32),
    Integer(i32),
    Color(CssColor),
    Size(SizeValue),
    Length(LengthPercentage),
    LengthAuto(LengthPercentageAuto),
    LineStyle(LineStyle),
    CornerRadius(CornerRadius),
    FontStyle(FontStyleMode),
    FontFamily(SmallVec<[InternedStr; 4]>),
    LineHeight(LineHeightValue),
    TextAlign(TextAlignMode),
    TextDecorationLine(TextDecorationLine),
    TextTransform(TextTransformMode),
    WhiteSpace(WhiteSpaceMode),
    WordBreak(WordBreakMode),
    OverflowWrap(OverflowWrapMode),
    Direction(DirectionMode),
    UserSelect(UserSelectMode),
    FlexDirection(FlexDirectionMode),
    FlexWrap(FlexWrapMode),
    Align(AlignValue),
    GridAutoFlow(GridAutoFlowMode),
    GridTracks(Vec<GridTrackSize>),
    GridLine(Option<i32>, Option<i32>),
    GridPlacement(GridPlacementValue),
    GridTemplateAreas(GridTemplateAreasValue),
    BoxShadow(Vec<BoxShadowValue>),
    BackgroundLayers(Vec<BackgroundLayerValue>),
    Transform(Vec<TransformOp>),
    TransformOrigin(TransformOrigin),
    Filter(Vec<FilterOp>),
    ClipPath(ClipPathValue),
    MixBlendMode(MixBlendMode),
    Transitions(Vec<TransitionSpec>),
    Animations(Vec<AnimationSpec>),
    NameList(Vec<InternedStr>),
    DurationList(Vec<f32>),
    TimingList(Vec<TimingFunction>),
    DirectionList(Vec<AnimationDirectionMode>),
    FillModeList(Vec<AnimationFillModeKind>),
    PlayStateList(Vec<AnimationPlayStateMode>),
    VerticalAlign(VerticalAlignMode),
    TextOverflow(TextOverflowMode),
    BorderCollapse(BorderCollapseMode),
    BorderSpacing(LengthPercentage, LengthPercentage),
    TableLayout(TableLayoutMode),
    Content(Option<InternedStr>),
}

/// A compiled single-property CSS declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct SpecifiedDeclaration {
    pub property: PropertyIdKind,
    pub custom_name: Option<InternedStr>,
    pub value: PropertyValue,
    pub important: bool,
    pub order: u32,
}

fn push_decl(
    out: &mut Vec<SpecifiedDeclaration>,
    order: &mut u32,
    property: PropertyIdKind,
    value: PropertyValue,
    important: bool,
) {
    let idx = *order;
    *order = order.saturating_add(1);
    out.push(SpecifiedDeclaration {
        property,
        custom_name: None,
        value,
        important,
        order: idx,
    });
}

/// Compiles a `lightningcss` [`DeclarationBlock`] into typed [`SpecifiedDeclaration`]s.
pub fn compile_declaration_block(
    block: &DeclarationBlock<'_>,
    out: &mut Vec<SpecifiedDeclaration>,
    missing: &mut Vec<MissingCssFeature>,
) {
    let mut order = 0u32;
    for prop in &block.declarations {
        compile_property(prop, false, &mut order, out, missing);
    }
    for prop in &block.important_declarations {
        compile_property(prop, true, &mut order, out, missing);
    }
}

/// Parses an HTML `style="..."` attribute into typed [`SpecifiedDeclaration`]s.
#[must_use]
pub fn parse_inline_style(
    style_attr: &str,
    missing: &mut Vec<MissingCssFeature>,
) -> Vec<SpecifiedDeclaration> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let opts = ParserOptions {
            error_recovery: true,
            ..ParserOptions::default()
        };
        let Ok(block) = DeclarationBlock::parse_string(style_attr, opts) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        compile_declaration_block(&block, &mut out, missing);
        out
    }))
    .unwrap_or_default()
}

/// Re-parses a resolved `var()` token stream into one or more typed declarations.
#[must_use]
pub fn parse_property_from_tokens(
    prop: PropertyIdKind,
    tokens: &[CssToken],
    important: bool,
) -> Vec<SpecifiedDeclaration> {
    let non_ws: SmallVec<[&CssToken; 4]> = tokens
        .iter()
        .filter(|t| !matches!(t, CssToken::Space))
        .collect();
    if non_ws.is_empty() {
        return Vec::new();
    }
    if non_ws.len() == 1 {
        if let CssToken::Ident(id) = non_ws[0] {
            if let Some(kw) = GlobalKeyword::from_ident(id.as_str()) {
                return vec![SpecifiedDeclaration {
                    property: prop,
                    custom_name: None,
                    value: PropertyValue::Global(kw),
                    important,
                    order: 0,
                }];
            }
        }
        // Fast path for single-token resolved color/length/number without string re-parsing.
        if let Some(val) = fast_parse_single_token(prop, non_ws[0]) {
            return vec![SpecifiedDeclaration {
                property: prop,
                custom_name: None,
                value: val,
                important,
                order: 0,
            }];
        }
    }

    let mut buf = String::with_capacity(64);
    write_tokens_to_buf(tokens, &mut buf);
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let opts = ParserOptions {
            error_recovery: true,
            ..ParserOptions::default()
        };
        let mut out = Vec::new();
        let mut order = 0u32;
        let mut ignored_missing = Vec::new();
        if let Ok(parsed) = Property::parse_string(PropertyId::from(prop.css_name()), &buf, opts) {
            if !matches!(parsed, Property::Unparsed(_)) {
                compile_property(
                    &parsed,
                    important,
                    &mut order,
                    &mut out,
                    &mut ignored_missing,
                );
            }
        }
        out
    }))
    .unwrap_or_default()
}

fn fast_parse_single_token(prop: PropertyIdKind, tok: &CssToken) -> Option<PropertyValue> {
    match prop {
        PropertyIdKind::Color
        | PropertyIdKind::BackgroundColor
        | PropertyIdKind::BorderTopColor
        | PropertyIdKind::BorderRightColor
        | PropertyIdKind::BorderBottomColor
        | PropertyIdKind::BorderLeftColor
        | PropertyIdKind::OutlineColor
        | PropertyIdKind::TextDecorationColor => match tok {
            CssToken::Color(c) => Some(PropertyValue::Color(*c)),
            _ => None,
        },
        PropertyIdKind::Opacity | PropertyIdKind::FlexGrow | PropertyIdKind::FlexShrink => {
            match tok {
                CssToken::Number(n) => Some(PropertyValue::Number(*n)),
                _ => None,
            }
        }
        PropertyIdKind::Width
        | PropertyIdKind::Height
        | PropertyIdKind::MinWidth
        | PropertyIdKind::MinHeight
        | PropertyIdKind::MaxWidth
        | PropertyIdKind::MaxHeight => match tok {
            CssToken::Dimension(v, u) => LengthUnit::from_unit_str(u.as_str()).map(|unit| {
                PropertyValue::Size(SizeValue::Length(LengthPercentage::Length(*v, unit)))
            }),
            CssToken::Percentage(p) => Some(PropertyValue::Size(SizeValue::Length(
                LengthPercentage::Percent(*p),
            ))),
            _ => None,
        },
        PropertyIdKind::FontSize
        | PropertyIdKind::PaddingTop
        | PropertyIdKind::PaddingRight
        | PropertyIdKind::PaddingBottom
        | PropertyIdKind::PaddingLeft
        | PropertyIdKind::RowGap
        | PropertyIdKind::ColumnGap => match tok {
            CssToken::Dimension(v, u) => LengthUnit::from_unit_str(u.as_str())
                .map(|unit| PropertyValue::Length(LengthPercentage::Length(*v, unit))),
            CssToken::Percentage(p) => Some(PropertyValue::Length(LengthPercentage::Percent(*p))),
            _ => None,
        },
        _ => None,
    }
}

fn compile_property(
    prop: &Property<'_>,
    imp: bool,
    ord: &mut u32,
    out: &mut Vec<SpecifiedDeclaration>,
    missing: &mut Vec<MissingCssFeature>,
) {
    match prop {
        Property::Display(d) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::Display,
                PropertyValue::Display(convert_display(d)),
                imp,
            );
        }
        Property::Position(p) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::Position,
                PropertyValue::Position(convert_position(p)),
                imp,
            );
        }
        Property::BoxSizing(bs, _) => {
            let mode = match bs {
                BoxSizing::ContentBox => BoxSizingMode::ContentBox,
                BoxSizing::BorderBox => BoxSizingMode::BorderBox,
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::BoxSizing,
                PropertyValue::BoxSizing(mode),
                imp,
            );
        }
        Property::OverflowX(ov) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::OverflowX,
                PropertyValue::Overflow(convert_overflow(ov)),
                imp,
            );
        }
        Property::OverflowY(ov) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::OverflowY,
                PropertyValue::Overflow(convert_overflow(ov)),
                imp,
            );
        }
        Property::Overflow(ov) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::OverflowX,
                PropertyValue::Overflow(convert_overflow(&ov.x)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::OverflowY,
                PropertyValue::Overflow(convert_overflow(&ov.y)),
                imp,
            );
        }
        Property::Visibility(v) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::Visibility,
                PropertyValue::Visibility(convert_visibility(v)),
                imp,
            );
        }
        Property::Cursor(c) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::Cursor,
                PropertyValue::Cursor(convert_cursor(c)),
                imp,
            );
        }
        Property::ZIndex(z) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::ZIndex,
                PropertyValue::ZIndex(convert_z_index(z)),
                imp,
            );
        }
        Property::Opacity(a) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::Opacity,
                PropertyValue::Number(convert_alpha(a)),
                imp,
            );
        }
        Property::Color(c) => {
            if let Some(color) = convert_color(c) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::Color,
                    PropertyValue::Color(color),
                    imp,
                );
            }
        }
        Property::BackgroundColor(c) => {
            if let Some(color) = convert_color(c) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BackgroundColor,
                    PropertyValue::Color(color),
                    imp,
                );
            }
        }
        Property::Background(bgs) => {
            if let Some(last) = bgs.last() {
                if let Some(color) = convert_color(&last.color) {
                    push_decl(
                        out,
                        ord,
                        PropertyIdKind::BackgroundColor,
                        PropertyValue::Color(color),
                        imp,
                    );
                }
            }
            let layers = bgs.iter().map(convert_background_layer).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::BackgroundLayers,
                PropertyValue::BackgroundLayers(layers),
                imp,
            );
        }
        Property::BackgroundImage(imgs) => {
            let layers = imgs
                .iter()
                .map(|img| {
                    let image = convert_image(img);
                    let image_url = match &image {
                        BackgroundImageValue::Url(u) => Some(u.clone()),
                        _ => None,
                    };
                    BackgroundLayerValue {
                        image_url,
                        image,
                        repeat_x: true,
                        repeat_y: true,
                        ..BackgroundLayerValue::default()
                    }
                })
                .collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::BackgroundLayers,
                PropertyValue::BackgroundLayers(layers),
                imp,
            );
        }
        Property::Width(s) | Property::InlineSize(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::Width,
                PropertyValue::Size(convert_size(s)),
                imp,
            );
        }
        Property::Height(s) | Property::BlockSize(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::Height,
                PropertyValue::Size(convert_size(s)),
                imp,
            );
        }
        Property::MinWidth(s) | Property::MinInlineSize(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MinWidth,
                PropertyValue::Size(convert_size(s)),
                imp,
            );
        }
        Property::MinHeight(s) | Property::MinBlockSize(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MinHeight,
                PropertyValue::Size(convert_size(s)),
                imp,
            );
        }
        Property::MaxWidth(s) | Property::MaxInlineSize(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MaxWidth,
                PropertyValue::Size(convert_max_size(s)),
                imp,
            );
        }
        Property::MaxHeight(s) | Property::MaxBlockSize(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MaxHeight,
                PropertyValue::Size(convert_max_size(s)),
                imp,
            );
        }
        Property::MarginTop(v) | Property::MarginBlockStart(v) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MarginTop,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
                imp,
            );
        }
        Property::MarginRight(v) | Property::MarginInlineEnd(v) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MarginRight,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
                imp,
            );
        }
        Property::MarginBottom(v) | Property::MarginBlockEnd(v) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MarginBottom,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
                imp,
            );
        }
        Property::MarginLeft(v) | Property::MarginInlineStart(v) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MarginLeft,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
                imp,
            );
        }
        Property::Margin(m) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MarginTop,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&m.top)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::MarginRight,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&m.right)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::MarginBottom,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&m.bottom)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::MarginLeft,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&m.left)),
                imp,
            );
        }
        Property::MarginBlock(m) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MarginTop,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&m.block_start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::MarginBottom,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&m.block_end)),
                imp,
            );
        }
        Property::MarginInline(m) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MarginLeft,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&m.inline_start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::MarginRight,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&m.inline_end)),
                imp,
            );
        }
        Property::PaddingTop(v) | Property::PaddingBlockStart(v) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::PaddingTop,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
                imp,
            );
        }
        Property::PaddingRight(v) | Property::PaddingInlineEnd(v) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::PaddingRight,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
                imp,
            );
        }
        Property::PaddingBottom(v) | Property::PaddingBlockEnd(v) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::PaddingBottom,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
                imp,
            );
        }
        Property::PaddingLeft(v) | Property::PaddingInlineStart(v) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::PaddingLeft,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
                imp,
            );
        }
        Property::Padding(p) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::PaddingTop,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&p.top)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::PaddingRight,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&p.right)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::PaddingBottom,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&p.bottom)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::PaddingLeft,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&p.left)),
                imp,
            );
        }
        Property::PaddingBlock(p) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::PaddingTop,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&p.block_start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::PaddingBottom,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&p.block_end)),
                imp,
            );
        }
        Property::PaddingInline(p) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::PaddingLeft,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&p.inline_start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::PaddingRight,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&p.inline_end)),
                imp,
            );
        }
        Property::Top(v) | Property::InsetBlockStart(v) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::Top,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
                imp,
            );
        }
        Property::Right(v) | Property::InsetInlineEnd(v) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::Right,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
                imp,
            );
        }
        Property::Bottom(v) | Property::InsetBlockEnd(v) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::Bottom,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
                imp,
            );
        }
        Property::Left(v) | Property::InsetInlineStart(v) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::Left,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
                imp,
            );
        }
        Property::Inset(i) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::Top,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&i.top)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::Right,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&i.right)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::Bottom,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&i.bottom)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::Left,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&i.left)),
                imp,
            );
        }
        Property::BorderTopWidth(w) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderTopWidth,
            PropertyValue::Number(convert_border_width(w)),
            imp,
        ),
        Property::BorderRightWidth(w) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderRightWidth,
            PropertyValue::Number(convert_border_width(w)),
            imp,
        ),
        Property::BorderBottomWidth(w) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderBottomWidth,
            PropertyValue::Number(convert_border_width(w)),
            imp,
        ),
        Property::BorderLeftWidth(w) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderLeftWidth,
            PropertyValue::Number(convert_border_width(w)),
            imp,
        ),
        Property::BorderWidth(bw) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderTopWidth,
                PropertyValue::Number(convert_border_width(&bw.top)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderRightWidth,
                PropertyValue::Number(convert_border_width(&bw.right)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderBottomWidth,
                PropertyValue::Number(convert_border_width(&bw.bottom)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderLeftWidth,
                PropertyValue::Number(convert_border_width(&bw.left)),
                imp,
            );
        }
        Property::BorderTopStyle(s) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderTopStyle,
            PropertyValue::LineStyle(convert_line_style(s)),
            imp,
        ),
        Property::BorderRightStyle(s) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderRightStyle,
            PropertyValue::LineStyle(convert_line_style(s)),
            imp,
        ),
        Property::BorderBottomStyle(s) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderBottomStyle,
            PropertyValue::LineStyle(convert_line_style(s)),
            imp,
        ),
        Property::BorderLeftStyle(s) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderLeftStyle,
            PropertyValue::LineStyle(convert_line_style(s)),
            imp,
        ),
        Property::BorderStyle(bs) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderTopStyle,
                PropertyValue::LineStyle(convert_line_style(&bs.top)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderRightStyle,
                PropertyValue::LineStyle(convert_line_style(&bs.right)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderBottomStyle,
                PropertyValue::LineStyle(convert_line_style(&bs.bottom)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderLeftStyle,
                PropertyValue::LineStyle(convert_line_style(&bs.left)),
                imp,
            );
        }
        Property::BorderTopColor(c) => {
            if let Some(v) = convert_color(c) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderTopColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
        }
        Property::BorderRightColor(c) => {
            if let Some(v) = convert_color(c) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderRightColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
        }
        Property::BorderBottomColor(c) => {
            if let Some(v) = convert_color(c) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderBottomColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
        }
        Property::BorderLeftColor(c) => {
            if let Some(v) = convert_color(c) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderLeftColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
        }
        Property::BorderColor(bc) => {
            if let Some(v) = convert_color(&bc.top) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderTopColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
            if let Some(v) = convert_color(&bc.right) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderRightColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
            if let Some(v) = convert_color(&bc.bottom) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderBottomColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
            if let Some(v) = convert_color(&bc.left) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderLeftColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
        }
        Property::Border(b) => {
            let w = convert_border_width(&b.width);
            let s = convert_line_style(&b.style);
            let c = convert_color(&b.color).unwrap_or(CssColor::CurrentColor);
            for (wk, sk, ck) in [
                (
                    PropertyIdKind::BorderTopWidth,
                    PropertyIdKind::BorderTopStyle,
                    PropertyIdKind::BorderTopColor,
                ),
                (
                    PropertyIdKind::BorderRightWidth,
                    PropertyIdKind::BorderRightStyle,
                    PropertyIdKind::BorderRightColor,
                ),
                (
                    PropertyIdKind::BorderBottomWidth,
                    PropertyIdKind::BorderBottomStyle,
                    PropertyIdKind::BorderBottomColor,
                ),
                (
                    PropertyIdKind::BorderLeftWidth,
                    PropertyIdKind::BorderLeftStyle,
                    PropertyIdKind::BorderLeftColor,
                ),
            ] {
                push_decl(out, ord, wk, PropertyValue::Number(w), imp);
                push_decl(out, ord, sk, PropertyValue::LineStyle(s), imp);
                push_decl(out, ord, ck, PropertyValue::Color(c), imp);
            }
        }
        Property::BorderTop(b) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderTopWidth,
                PropertyValue::Number(convert_border_width(&b.width)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderTopStyle,
                PropertyValue::LineStyle(convert_line_style(&b.style)),
                imp,
            );
            if let Some(c) = convert_color(&b.color) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderTopColor,
                    PropertyValue::Color(c),
                    imp,
                );
            }
        }
        Property::BorderRight(b) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderRightWidth,
                PropertyValue::Number(convert_border_width(&b.width)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderRightStyle,
                PropertyValue::LineStyle(convert_line_style(&b.style)),
                imp,
            );
            if let Some(c) = convert_color(&b.color) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderRightColor,
                    PropertyValue::Color(c),
                    imp,
                );
            }
        }
        Property::BorderBottom(b) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderBottomWidth,
                PropertyValue::Number(convert_border_width(&b.width)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderBottomStyle,
                PropertyValue::LineStyle(convert_line_style(&b.style)),
                imp,
            );
            if let Some(c) = convert_color(&b.color) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderBottomColor,
                    PropertyValue::Color(c),
                    imp,
                );
            }
        }
        Property::BorderLeft(b) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderLeftWidth,
                PropertyValue::Number(convert_border_width(&b.width)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderLeftStyle,
                PropertyValue::LineStyle(convert_line_style(&b.style)),
                imp,
            );
            if let Some(c) = convert_color(&b.color) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderLeftColor,
                    PropertyValue::Color(c),
                    imp,
                );
            }
        }
        Property::BorderTopLeftRadius(r, _) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderTopLeftRadius,
            PropertyValue::CornerRadius(convert_corner_radius(r)),
            imp,
        ),
        Property::BorderTopRightRadius(r, _) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderTopRightRadius,
            PropertyValue::CornerRadius(convert_corner_radius(r)),
            imp,
        ),
        Property::BorderBottomRightRadius(r, _) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderBottomRightRadius,
            PropertyValue::CornerRadius(convert_corner_radius(r)),
            imp,
        ),
        Property::BorderBottomLeftRadius(r, _) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderBottomLeftRadius,
            PropertyValue::CornerRadius(convert_corner_radius(r)),
            imp,
        ),
        Property::BorderRadius(br, _) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderTopLeftRadius,
                PropertyValue::CornerRadius(convert_corner_radius(&br.top_left)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderTopRightRadius,
                PropertyValue::CornerRadius(convert_corner_radius(&br.top_right)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderBottomRightRadius,
                PropertyValue::CornerRadius(convert_corner_radius(&br.bottom_right)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderBottomLeftRadius,
                PropertyValue::CornerRadius(convert_corner_radius(&br.bottom_left)),
                imp,
            );
        }
        Property::OutlineWidth(w) => push_decl(
            out,
            ord,
            PropertyIdKind::OutlineWidth,
            PropertyValue::Number(convert_border_width(w)),
            imp,
        ),
        Property::OutlineColor(c) => {
            if let Some(v) = convert_color(c) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::OutlineColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
        }
        Property::Outline(o) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::OutlineWidth,
                PropertyValue::Number(convert_border_width(&o.width)),
                imp,
            );
            if let Some(c) = convert_color(&o.color) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::OutlineColor,
                    PropertyValue::Color(c),
                    imp,
                );
            }
            let st = match &o.style {
                lightningcss::properties::outline::OutlineStyle::Auto => LineStyle::Solid,
                lightningcss::properties::outline::OutlineStyle::LineStyle(ls) => {
                    convert_line_style(ls)
                }
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::OutlineStyle,
                PropertyValue::LineStyle(st),
                imp,
            );
        }
        Property::OutlineStyle(os) => {
            let st = match os {
                lightningcss::properties::outline::OutlineStyle::Auto => LineStyle::Solid,
                lightningcss::properties::outline::OutlineStyle::LineStyle(ls) => {
                    convert_line_style(ls)
                }
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::OutlineStyle,
                PropertyValue::LineStyle(st),
                imp,
            );
        }
        Property::FontSize(fs) => push_decl(
            out,
            ord,
            PropertyIdKind::FontSize,
            PropertyValue::Length(convert_font_size(fs)),
            imp,
        ),
        Property::FontWeight(fw) => push_decl(
            out,
            ord,
            PropertyIdKind::FontWeight,
            PropertyValue::Number(convert_font_weight(fw)),
            imp,
        ),
        Property::FontStyle(fs) => push_decl(
            out,
            ord,
            PropertyIdKind::FontStyle,
            PropertyValue::FontStyle(convert_font_style(fs)),
            imp,
        ),
        Property::FontFamily(fams) => {
            let names = fams.iter().map(convert_font_family_name).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::FontFamily,
                PropertyValue::FontFamily(names),
                imp,
            );
        }
        Property::LineHeight(lh) => push_decl(
            out,
            ord,
            PropertyIdKind::LineHeight,
            PropertyValue::LineHeight(convert_line_height(lh)),
            imp,
        ),
        Property::Font(f) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::FontSize,
                PropertyValue::Length(convert_font_size(&f.size)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::FontWeight,
                PropertyValue::Number(convert_font_weight(&f.weight)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::FontStyle,
                PropertyValue::FontStyle(convert_font_style(&f.style)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::LineHeight,
                PropertyValue::LineHeight(convert_line_height(&f.line_height)),
                imp,
            );
            let names = f.family.iter().map(convert_font_family_name).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::FontFamily,
                PropertyValue::FontFamily(names),
                imp,
            );
        }
        Property::LetterSpacing(sp) => push_decl(
            out,
            ord,
            PropertyIdKind::LetterSpacing,
            PropertyValue::Length(convert_spacing(sp)),
            imp,
        ),
        Property::WordSpacing(sp) => push_decl(
            out,
            ord,
            PropertyIdKind::WordSpacing,
            PropertyValue::Length(convert_spacing(sp)),
            imp,
        ),
        Property::TextAlign(ta) => push_decl(
            out,
            ord,
            PropertyIdKind::TextAlign,
            PropertyValue::TextAlign(convert_text_align(ta)),
            imp,
        ),
        Property::TextDecorationLine(l, _) => push_decl(
            out,
            ord,
            PropertyIdKind::TextDecorationLine,
            PropertyValue::TextDecorationLine(convert_text_decoration_line(l)),
            imp,
        ),
        Property::TextDecorationColor(c, _) => {
            if let Some(v) = convert_color(c) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::TextDecorationColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
        }
        Property::TextDecoration(td, _) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::TextDecorationLine,
                PropertyValue::TextDecorationLine(convert_text_decoration_line(&td.line)),
                imp,
            );
            if let Some(c) = convert_color(&td.color) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::TextDecorationColor,
                    PropertyValue::Color(c),
                    imp,
                );
            }
        }
        Property::TextTransform(tt) => push_decl(
            out,
            ord,
            PropertyIdKind::TextTransform,
            PropertyValue::TextTransform(convert_text_transform(tt)),
            imp,
        ),
        Property::WhiteSpace(ws) => push_decl(
            out,
            ord,
            PropertyIdKind::WhiteSpace,
            PropertyValue::WhiteSpace(convert_white_space(ws)),
            imp,
        ),
        Property::WordBreak(wb) => push_decl(
            out,
            ord,
            PropertyIdKind::WordBreak,
            PropertyValue::WordBreak(convert_word_break(wb)),
            imp,
        ),
        Property::OverflowWrap(ow) | Property::WordWrap(ow) => push_decl(
            out,
            ord,
            PropertyIdKind::OverflowWrap,
            PropertyValue::OverflowWrap(convert_overflow_wrap(ow)),
            imp,
        ),
        Property::Direction(d) => push_decl(
            out,
            ord,
            PropertyIdKind::Direction,
            PropertyValue::Direction(convert_direction(d)),
            imp,
        ),
        Property::UserSelect(u, _) => push_decl(
            out,
            ord,
            PropertyIdKind::UserSelect,
            PropertyValue::UserSelect(convert_user_select(u)),
            imp,
        ),
        Property::FlexDirection(fd, _) => push_decl(
            out,
            ord,
            PropertyIdKind::FlexDirection,
            PropertyValue::FlexDirection(convert_flex_direction(fd)),
            imp,
        ),
        Property::FlexWrap(fw, _) => push_decl(
            out,
            ord,
            PropertyIdKind::FlexWrap,
            PropertyValue::FlexWrap(convert_flex_wrap(fw)),
            imp,
        ),
        Property::FlexFlow(ff, _) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::FlexDirection,
                PropertyValue::FlexDirection(convert_flex_direction(&ff.direction)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::FlexWrap,
                PropertyValue::FlexWrap(convert_flex_wrap(&ff.wrap)),
                imp,
            );
        }
        Property::FlexGrow(g, _) => push_decl(
            out,
            ord,
            PropertyIdKind::FlexGrow,
            PropertyValue::Number(*g),
            imp,
        ),
        Property::FlexShrink(s, _) => push_decl(
            out,
            ord,
            PropertyIdKind::FlexShrink,
            PropertyValue::Number(*s),
            imp,
        ),
        Property::FlexBasis(b, _) => push_decl(
            out,
            ord,
            PropertyIdKind::FlexBasis,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(b)),
            imp,
        ),
        Property::Flex(fl, _) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::FlexGrow,
                PropertyValue::Number(fl.grow),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::FlexShrink,
                PropertyValue::Number(fl.shrink),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::FlexBasis,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&fl.basis)),
                imp,
            );
        }
        Property::Order(o, _) => push_decl(
            out,
            ord,
            PropertyIdKind::Order,
            PropertyValue::Integer(*o),
            imp,
        ),
        Property::AlignItems(ai, _) => push_decl(
            out,
            ord,
            PropertyIdKind::AlignItems,
            PropertyValue::Align(convert_align_items(ai)),
            imp,
        ),
        Property::AlignSelf(as_, _) => push_decl(
            out,
            ord,
            PropertyIdKind::AlignSelf,
            PropertyValue::Align(convert_align_self(as_)),
            imp,
        ),
        Property::AlignContent(ac, _) => push_decl(
            out,
            ord,
            PropertyIdKind::AlignContent,
            PropertyValue::Align(convert_align_content(ac)),
            imp,
        ),
        Property::JustifyContent(jc, _) => push_decl(
            out,
            ord,
            PropertyIdKind::JustifyContent,
            PropertyValue::Align(convert_justify_content(jc)),
            imp,
        ),
        Property::JustifyItems(ji) => push_decl(
            out,
            ord,
            PropertyIdKind::JustifyItems,
            PropertyValue::Align(convert_justify_items(ji)),
            imp,
        ),
        Property::JustifySelf(js) => push_decl(
            out,
            ord,
            PropertyIdKind::JustifySelf,
            PropertyValue::Align(convert_justify_self(js)),
            imp,
        ),
        Property::PlaceContent(pc) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::AlignContent,
                PropertyValue::Align(convert_align_content(&pc.align)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::JustifyContent,
                PropertyValue::Align(convert_justify_content(&pc.justify)),
                imp,
            );
        }
        Property::PlaceItems(pi) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::AlignItems,
                PropertyValue::Align(convert_align_items(&pi.align)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::JustifyItems,
                PropertyValue::Align(convert_justify_items(&pi.justify)),
                imp,
            );
        }
        Property::PlaceSelf(ps) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::AlignSelf,
                PropertyValue::Align(convert_align_self(&ps.align)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::JustifySelf,
                PropertyValue::Align(convert_justify_self(&ps.justify)),
                imp,
            );
        }
        Property::RowGap(g) => push_decl(
            out,
            ord,
            PropertyIdKind::RowGap,
            PropertyValue::Length(convert_gap_value(g)),
            imp,
        ),
        Property::ColumnGap(g) => push_decl(
            out,
            ord,
            PropertyIdKind::ColumnGap,
            PropertyValue::Length(convert_gap_value(g)),
            imp,
        ),
        Property::Gap(g) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::RowGap,
                PropertyValue::Length(convert_gap_value(&g.row)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::ColumnGap,
                PropertyValue::Length(convert_gap_value(&g.column)),
                imp,
            );
        }
        Property::GridTemplateColumns(ts) => push_decl(
            out,
            ord,
            PropertyIdKind::GridTemplateColumns,
            PropertyValue::GridTracks(convert_track_sizing(ts)),
            imp,
        ),
        Property::GridTemplateRows(ts) => push_decl(
            out,
            ord,
            PropertyIdKind::GridTemplateRows,
            PropertyValue::GridTracks(convert_track_sizing(ts)),
            imp,
        ),
        Property::GridTemplateAreas(areas) => push_decl(
            out,
            ord,
            PropertyIdKind::GridTemplateAreas,
            PropertyValue::GridTemplateAreas(convert_grid_template_areas(areas)),
            imp,
        ),
        Property::GridTemplate(gt) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::GridTemplateRows,
                PropertyValue::GridTracks(convert_track_sizing(&gt.rows)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::GridTemplateColumns,
                PropertyValue::GridTracks(convert_track_sizing(&gt.columns)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::GridTemplateAreas,
                PropertyValue::GridTemplateAreas(convert_grid_template_areas(&gt.areas)),
                imp,
            );
        }
        Property::GridRowStart(line) => push_decl(
            out,
            ord,
            PropertyIdKind::GridRowStart,
            PropertyValue::GridPlacement(convert_grid_line(line)),
            imp,
        ),
        Property::GridRowEnd(line) => push_decl(
            out,
            ord,
            PropertyIdKind::GridRowEnd,
            PropertyValue::GridPlacement(convert_grid_line(line)),
            imp,
        ),
        Property::GridRow(row) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::GridRowStart,
                PropertyValue::GridPlacement(convert_grid_line(&row.start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::GridRowEnd,
                PropertyValue::GridPlacement(convert_grid_line(&row.end)),
                imp,
            );
        }
        Property::GridColumnStart(line) => push_decl(
            out,
            ord,
            PropertyIdKind::GridColumnStart,
            PropertyValue::GridPlacement(convert_grid_line(line)),
            imp,
        ),
        Property::GridColumnEnd(line) => push_decl(
            out,
            ord,
            PropertyIdKind::GridColumnEnd,
            PropertyValue::GridPlacement(convert_grid_line(line)),
            imp,
        ),
        Property::GridColumn(col) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::GridColumnStart,
                PropertyValue::GridPlacement(convert_grid_line(&col.start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::GridColumnEnd,
                PropertyValue::GridPlacement(convert_grid_line(&col.end)),
                imp,
            );
        }
        Property::GridArea(area) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::GridRowStart,
                PropertyValue::GridPlacement(convert_grid_line(&area.row_start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::GridColumnStart,
                PropertyValue::GridPlacement(convert_grid_line(&area.column_start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::GridRowEnd,
                PropertyValue::GridPlacement(convert_grid_line(&area.row_end)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::GridColumnEnd,
                PropertyValue::GridPlacement(convert_grid_line(&area.column_end)),
                imp,
            );
        }
        Property::GridAutoColumns(tsl) => push_decl(
            out,
            ord,
            PropertyIdKind::GridAutoColumns,
            PropertyValue::GridTracks(tsl.0.iter().map(convert_track_size).collect()),
            imp,
        ),
        Property::GridAutoRows(tsl) => push_decl(
            out,
            ord,
            PropertyIdKind::GridAutoRows,
            PropertyValue::GridTracks(tsl.0.iter().map(convert_track_size).collect()),
            imp,
        ),
        Property::GridAutoFlow(f) => push_decl(
            out,
            ord,
            PropertyIdKind::GridAutoFlow,
            PropertyValue::GridAutoFlow(convert_grid_auto_flow(f)),
            imp,
        ),
        Property::ColorScheme(cs) => {
            let supports_dark = cs.contains(lightningcss::properties::ui::ColorScheme::Dark);
            push_decl(
                out,
                ord,
                PropertyIdKind::ColorScheme,
                PropertyValue::Number(if supports_dark { 1.0 } else { 0.0 }),
                imp,
            );
        }
        Property::BoxShadow(bs, _) => {
            let shadows = bs.iter().map(convert_box_shadow).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::BoxShadow,
                PropertyValue::BoxShadow(shadows),
                imp,
            );
        }
        Property::Transform(tl, _) => {
            let ops = tl.0.iter().filter_map(convert_transform).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::Transform,
                PropertyValue::Transform(ops),
                imp,
            );
        }
        Property::TransformOrigin(pos, _) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::TransformOrigin,
                PropertyValue::TransformOrigin(convert_transform_origin(pos)),
                imp,
            );
        }
        Property::Filter(fl, _) => {
            let ops = match fl {
                lightningcss::properties::effects::FilterList::None => Vec::new(),
                lightningcss::properties::effects::FilterList::Filters(list) => {
                    list.iter().filter_map(convert_filter).collect()
                }
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::Filter,
                PropertyValue::Filter(ops),
                imp,
            );
        }
        Property::ClipPath(cp, _) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::ClipPath,
                PropertyValue::ClipPath(convert_clip_path(cp)),
                imp,
            );
        }
        Property::MixBlendMode(bm) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MixBlendMode,
                PropertyValue::MixBlendMode(convert_blend_mode(bm)),
                imp,
            );
        }
        Property::Transition(list, _) => {
            let specs = list.iter().map(convert_transition).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::Transition,
                PropertyValue::Transitions(specs),
                imp,
            );
        }
        Property::TransitionProperty(props, _) => {
            let names = props.iter().map(|p| intern_str(p.name())).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::TransitionProperty,
                PropertyValue::NameList(names),
                imp,
            );
        }
        Property::TransitionDuration(times, _) => {
            let ms = times.iter().map(|t| time_to_ms(t).max(0.0)).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::TransitionDuration,
                PropertyValue::DurationList(ms),
                imp,
            );
        }
        Property::TransitionDelay(times, _) => {
            let ms = times.iter().map(time_to_ms).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::TransitionDelay,
                PropertyValue::DurationList(ms),
                imp,
            );
        }
        Property::TransitionTimingFunction(fns, _) => {
            let timings = fns.iter().map(convert_easing).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::TransitionTimingFunction,
                PropertyValue::TimingList(timings),
                imp,
            );
        }
        Property::Animation(list, _) => {
            let specs = list.iter().map(convert_animation).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::Animation,
                PropertyValue::Animations(specs),
                imp,
            );
        }
        Property::AnimationName(list, _) => {
            let names = list.iter().map(convert_animation_name).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::AnimationName,
                PropertyValue::NameList(names),
                imp,
            );
        }
        Property::AnimationDuration(times, _) => {
            let ms = times.iter().map(|t| time_to_ms(t).max(0.0)).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::AnimationDuration,
                PropertyValue::DurationList(ms),
                imp,
            );
        }
        Property::AnimationDelay(times, _) => {
            let ms = times.iter().map(time_to_ms).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::AnimationDelay,
                PropertyValue::DurationList(ms),
                imp,
            );
        }
        Property::AnimationTimingFunction(fns, _) => {
            let timings = fns.iter().map(convert_easing).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::AnimationTimingFunction,
                PropertyValue::TimingList(timings),
                imp,
            );
        }
        Property::AnimationIterationCount(counts, _) => {
            let vals = counts.iter().map(convert_animation_iteration).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::AnimationIterationCount,
                PropertyValue::DurationList(vals),
                imp,
            );
        }
        Property::AnimationDirection(dirs, _) => {
            let vals = dirs.iter().map(convert_animation_direction).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::AnimationDirection,
                PropertyValue::DirectionList(vals),
                imp,
            );
        }
        Property::AnimationFillMode(fills, _) => {
            let vals = fills.iter().map(convert_animation_fill).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::AnimationFillMode,
                PropertyValue::FillModeList(vals),
                imp,
            );
        }
        Property::AnimationPlayState(states, _) => {
            let vals = states.iter().map(convert_animation_play_state).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::AnimationPlayState,
                PropertyValue::PlayStateList(vals),
                imp,
            );
        }
        Property::TextOverflow(to, _) => {
            let mode = match to {
                lightningcss::properties::overflow::TextOverflow::Clip => TextOverflowMode::Clip,
                lightningcss::properties::overflow::TextOverflow::Ellipsis => {
                    TextOverflowMode::Ellipsis
                }
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::TextOverflow,
                PropertyValue::TextOverflow(mode),
                imp,
            );
        }
        Property::VerticalAlign(va) => {
            use lightningcss::properties::font::{VerticalAlign as Lva, VerticalAlignKeyword};
            let mode = match va {
                Lva::Keyword(VerticalAlignKeyword::Baseline) => VerticalAlignMode::Baseline,
                Lva::Keyword(VerticalAlignKeyword::Top) => VerticalAlignMode::Top,
                Lva::Keyword(VerticalAlignKeyword::Middle) => VerticalAlignMode::Middle,
                Lva::Keyword(VerticalAlignKeyword::Bottom) => VerticalAlignMode::Bottom,
                Lva::Keyword(VerticalAlignKeyword::TextTop) => VerticalAlignMode::TextTop,
                Lva::Keyword(VerticalAlignKeyword::TextBottom) => VerticalAlignMode::TextBottom,
                Lva::Keyword(VerticalAlignKeyword::Sub) => VerticalAlignMode::Sub,
                Lva::Keyword(VerticalAlignKeyword::Super) => VerticalAlignMode::Super,
                Lva::Length(lp) => {
                    let px = convert_length_percentage(lp)
                        .resolve_px(16.0, &crate::values::EvalContext::default());
                    VerticalAlignMode::LengthPx(px)
                }
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::VerticalAlign,
                PropertyValue::VerticalAlign(mode),
                imp,
            );
        }
        Property::BorderSpacing(bs) => {
            let x = convert_length(&bs.0);
            let y = convert_length(&bs.1);
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderSpacing,
                PropertyValue::BorderSpacing(x, y),
                imp,
            );
        }
        Property::Unparsed(u) => {
            let Some(kind) = PropertyIdKind::from_name(u.property_id.name()) else {
                missing.push(MissingCssFeature::new(
                    MissingCssKind::Property,
                    u.property_id.name(),
                ));
                return;
            };
            let tokens = convert_token_list(&u.value);
            let global_kw = {
                let non_ws: SmallVec<[&CssToken; 2]> = tokens
                    .iter()
                    .filter(|t| !matches!(t, CssToken::Space))
                    .collect();
                if non_ws.len() == 1 {
                    if let CssToken::Ident(id) = non_ws[0] {
                        GlobalKeyword::from_ident(id.as_str())
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some(kw) = global_kw {
                push_decl(out, ord, kind, PropertyValue::Global(kw), imp);
                return;
            }
            push_decl(out, ord, kind, PropertyValue::Unresolved(tokens), imp);
        }
        Property::Custom(custom) => match &custom.name {
            CustomPropertyName::Custom(dashed) => {
                let idx = *ord;
                *ord = ord.saturating_add(1);
                let tokens = convert_token_list(&custom.value);
                let global_kw = {
                    let mut iter = tokens.iter().filter(|t| !matches!(t, CssToken::Space));
                    match (iter.next(), iter.next()) {
                        (Some(CssToken::Ident(id)), None) => GlobalKeyword::from_ident(id.as_str()),
                        _ => None,
                    }
                };
                let value = match global_kw {
                    Some(kw) => PropertyValue::Global(kw),
                    None => PropertyValue::Custom(tokens),
                };
                out.push(SpecifiedDeclaration {
                    property: PropertyIdKind::Custom,
                    custom_name: Some(intern_str(dashed.as_ref())),
                    value,
                    important: imp,
                    order: idx,
                });
            }
            CustomPropertyName::Unknown(ident) => {
                compile_extra_standard_property(
                    ident.as_ref(),
                    &custom.value,
                    imp,
                    ord,
                    out,
                    missing,
                );
            }
        },
        other => {
            let prop_id = other.property_id();
            missing.push(MissingCssFeature::new(
                MissingCssKind::Property,
                prop_id.name(),
            ));
        }
    }
}

fn convert_gap_value(gap: &lightningcss::properties::align::GapValue) -> LengthPercentage {
    match gap {
        lightningcss::properties::align::GapValue::Normal => LengthPercentage::Zero,
        lightningcss::properties::align::GapValue::LengthPercentage(lp) => {
            convert_length_percentage(lp)
        }
    }
}

fn convert_grid_template_areas(
    areas: &lightningcss::properties::grid::GridTemplateAreas,
) -> GridTemplateAreasValue {
    match areas {
        lightningcss::properties::grid::GridTemplateAreas::None => {
            GridTemplateAreasValue::default()
        }
        lightningcss::properties::grid::GridTemplateAreas::Areas { columns, areas } => {
            GridTemplateAreasValue {
                columns: *columns,
                areas: areas
                    .iter()
                    .map(|opt| opt.as_deref().map(intern_str))
                    .collect(),
            }
        }
    }
}

fn convert_grid_line(line: &lightningcss::properties::grid::GridLine<'_>) -> GridPlacementValue {
    match line {
        lightningcss::properties::grid::GridLine::Auto => GridPlacementValue::Auto,
        lightningcss::properties::grid::GridLine::Area { name } => {
            GridPlacementValue::Area(intern_str(name.0.as_ref()))
        }
        lightningcss::properties::grid::GridLine::Line { index, .. } => {
            GridPlacementValue::Line((*index).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16)
        }
        lightningcss::properties::grid::GridLine::Span { index, .. } => {
            GridPlacementValue::Span((*index).clamp(1, i32::from(u16::MAX)) as u16)
        }
    }
}

fn compile_extra_standard_property(
    name: &str,
    tokens: &TokenList<'_>,
    imp: bool,
    ord: &mut u32,
    out: &mut Vec<SpecifiedDeclaration>,
    missing: &mut Vec<MissingCssFeature>,
) {
    let lower = name.to_ascii_lowercase();
    let owned = convert_token_list(tokens);
    let first_ident = owned.iter().find_map(|t| match t {
        CssToken::Ident(s) => Some(s.as_str()),
        _ => None,
    });
    if let Some(id) = first_ident {
        if let Some(kw) = GlobalKeyword::from_ident(id) {
            if let Some(kind) = PropertyIdKind::from_name(&lower) {
                push_decl(out, ord, kind, PropertyValue::Global(kw), imp);
                return;
            }
        }
    }
    match lower.as_str() {
        "float" => {
            let mode = match first_ident.map(str::to_ascii_lowercase).as_deref() {
                Some("left") => FloatMode::Left,
                Some("right") => FloatMode::Right,
                Some("inline-start") => FloatMode::InlineStart,
                Some("inline-end") => FloatMode::InlineEnd,
                _ => FloatMode::None,
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::Float,
                PropertyValue::Float(mode),
                imp,
            );
        }
        "clear" => {
            let mode = match first_ident.map(str::to_ascii_lowercase).as_deref() {
                Some("left") => ClearMode::Left,
                Some("right") => ClearMode::Right,
                Some("both") => ClearMode::Both,
                Some("inline-start") => ClearMode::InlineStart,
                Some("inline-end") => ClearMode::InlineEnd,
                _ => ClearMode::None,
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::Clear,
                PropertyValue::Clear(mode),
                imp,
            );
        }
        "pointer-events" => {
            let mode = match first_ident.map(str::to_ascii_lowercase).as_deref() {
                Some("none") => PointerEventsMode::None,
                _ => PointerEventsMode::Auto,
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::PointerEvents,
                PropertyValue::PointerEvents(mode),
                imp,
            );
        }
        "outline-offset" => {
            let len = tokens
                .0
                .iter()
                .find_map(|t| match t {
                    TokenOrValue::Length(lv) => Some(convert_length_value(lv)),
                    _ => None,
                })
                .unwrap_or(LengthPercentage::Zero);
            push_decl(
                out,
                ord,
                PropertyIdKind::OutlineOffset,
                PropertyValue::Length(len),
                imp,
            );
        }
        "border-collapse" => {
            let mode = match first_ident.map(str::to_ascii_lowercase).as_deref() {
                Some("collapse") => BorderCollapseMode::Collapse,
                _ => BorderCollapseMode::Separate,
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderCollapse,
                PropertyValue::BorderCollapse(mode),
                imp,
            );
        }
        "table-layout" => {
            let mode = match first_ident.map(str::to_ascii_lowercase).as_deref() {
                Some("fixed") => TableLayoutMode::Fixed,
                _ => TableLayoutMode::Auto,
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::TableLayout,
                PropertyValue::TableLayout(mode),
                imp,
            );
        }
        "color-scheme" => {
            let supports_dark = owned.iter().any(
                |t| matches!(t, CssToken::Ident(id) if id.as_str().eq_ignore_ascii_case("dark")),
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::ColorScheme,
                PropertyValue::Number(if supports_dark { 1.0 } else { 0.0 }),
                imp,
            );
        }
        "content" => {
            let value = owned.iter().find_map(|t| match t {
                CssToken::String(s) => Some(s.clone()),
                _ => None,
            });
            push_decl(
                out,
                ord,
                PropertyIdKind::Content,
                PropertyValue::Content(value),
                imp,
            );
        }
        _ => missing.push(MissingCssFeature::new(MissingCssKind::Property, name)),
    }
}
