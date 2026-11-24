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
use lightningcss::stylesheet::ParserOptions;
use rix_core::{intern_str, InternedStr};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::values::{
    angle_to_deg, convert_align_content, convert_align_items, convert_align_self, convert_alpha,
    convert_animation, convert_animation_direction, convert_animation_fill,
    convert_animation_iteration, convert_animation_name, convert_animation_play_state,
    convert_background_layer, convert_blend_mode, convert_border_width, convert_border_width_lp,
    convert_box_shadow, convert_clip_path, convert_color, convert_corner_radius, convert_cursor,
    convert_direction, convert_display, convert_easing, convert_filter, convert_flex_direction,
    convert_flex_wrap, convert_font_family_name, convert_font_size, convert_font_style,
    convert_font_weight, convert_grid_auto_flow, convert_image, convert_justify_content,
    convert_justify_items, convert_justify_self, convert_length, convert_length_percentage,
    convert_length_percentage_or_auto, convert_length_value, convert_line_height,
    convert_line_style, convert_max_size, convert_overflow, convert_overflow_wrap,
    convert_position, convert_size, convert_spacing, convert_text_align,
    convert_text_decoration_line, convert_text_transform, convert_track_size, convert_track_sizing,
    convert_transform, convert_transform_origin, convert_transition, convert_user_select,
    convert_visibility, convert_white_space, convert_word_break, convert_z_index, time_to_ms,
    AlignValue, AnimationCompositionMode, AnimationDirectionMode, AnimationFillModeKind,
    AnimationPlayStateMode, AnimationSpec, BackgroundImageValue, BackgroundLayerValue,
    BorderCollapseMode, BoxShadowValue, BoxSizingMode, CaptionSideMode, ClearMode, ClipPathValue,
    CornerRadius, CssColor, CursorKind, DirectionMode, DisplayMode, FilterOp, FlexDirectionMode,
    FlexWrapMode, FloatMode, FontStyleMode, GridAutoFlowMode, GridPlacementValue,
    GridTemplateAreasValue, GridTrackSize, InterpolateSizeMode, LengthPercentage,
    LengthPercentageAuto, LengthUnit, LineHeightValue, LineStyle, MixBlendMode, OverflowMode,
    OverflowWrapMode, OverlayMode, OverscrollBehaviorMode, PointerEventsMode, PositionMode,
    ReadingFlowMode, ResizeMode, SizeValue, TableLayoutMode, TextAlignLastMode, TextAlignMode,
    TextDecorationLine, TextJustifyMode, TextOverflowMode, TextTransformMode, TimingFunction,
    TransformOp, TransformOrigin, TransitionBehaviorMode, TransitionSpec, UserSelectMode,
    VerticalAlignMode, VisibilityMode, WhiteSpaceMode, WordBreakMode, WritingMode, ZIndexValue,
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
    RevertRule,
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
            "revert-rule" => Some(Self::RevertRule),
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

/// Tokenizes a raw CSS value string into owned [`CssToken`]s (used by `@property` and
/// `CSS.registerProperty` `initial-value` parsing).
#[must_use]
pub fn tokenize_custom_value_str(raw: &str) -> Vec<CssToken> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let opts = ParserOptions {
            error_recovery: true,
            ..ParserOptions::default()
        };
        if let Ok(Property::Custom(custom)) =
            Property::parse_string(PropertyId::from("--_rix_init"), trimmed, opts)
        {
            return convert_token_list(&custom.value);
        }
        vec![CssToken::Ident(intern_str(trimmed))]
    }))
    .unwrap_or_default()
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
            LightningToken::Semicolon => out.push(CssToken::Delim(';')),
            LightningToken::CurlyBracketBlock => out.push(CssToken::Delim('{')),
            LightningToken::CloseCurlyBracket => out.push(CssToken::Delim('}')),
            LightningToken::SquareBracketBlock => out.push(CssToken::Delim('[')),
            LightningToken::CloseSquareBracket => out.push(CssToken::Delim(']')),
            LightningToken::WhiteSpace(ws) => {
                for _ in 0..ws.len().clamp(1, 64) {
                    out.push(CssToken::Space);
                }
            }
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
        TokenOrValue::Function(f) => {
            let fname = if f.name.as_ref() == "__rix_var__" {
                intern_str("var")
            } else {
                intern_str(f.name.as_ref())
            };
            out.push(CssToken::Function {
                name: fname,
                args: convert_token_list(&f.arguments),
            });
        }
        TokenOrValue::Length(lv) => {
            let (v, u) = lv.to_unit_value();
            out.push(CssToken::Dimension(v, intern_str(u)));
        }
        TokenOrValue::Angle(a) => {
            out.push(CssToken::Dimension(angle_to_deg(a), intern_str("deg")));
        }
        TokenOrValue::Time(t) => {
            let secs = match t {
                lightningcss::values::time::Time::Seconds(s) => *s,
                lightningcss::values::time::Time::Milliseconds(m) => *m / 1000.0,
            };
            out.push(CssToken::Dimension(secs, intern_str("s")));
        }
        TokenOrValue::DashedIdent(id) => out.push(CssToken::Ident(intern_str(id.as_ref()))),
        TokenOrValue::Resolution(r) => {
            let dppx = match r {
                lightningcss::values::resolution::Resolution::Dpi(v) => *v / 96.0,
                lightningcss::values::resolution::Resolution::Dpcm(v) => *v * 2.54 / 96.0,
                lightningcss::values::resolution::Resolution::Dppx(v) => *v,
            };
            out.push(CssToken::Dimension(dppx, intern_str("dppx")));
        }
        TokenOrValue::Env(_)
        | TokenOrValue::UnresolvedColor(_)
        | TokenOrValue::AnimationName(_) => {}
    }
}

/// Formats a resolved [`CssToken`] slice directly into a reusable string buffer
/// for re-parsing complex multi-token declarations after `var()` expansion.
pub fn write_tokens_to_buf(tokens: &[CssToken], buf: &mut String) {
    for tok in tokens {
        match tok {
            CssToken::Ident(s) => {
                let raw = s.as_str();
                let b = raw.as_bytes();
                let needs_escape = b.first().is_some_and(u8::is_ascii_digit)
                    || (b.len() >= 2 && b[0] == b'-' && b[1].is_ascii_digit());
                if needs_escape {
                    buf.push_str(&crate::substitution::escape_css_ident(raw));
                } else {
                    buf.push_str(raw);
                }
            }
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
                let raw = u.as_str();
                let needs_quote = raw.is_empty()
                    || raw.bytes().any(|b| {
                        b.is_ascii_whitespace() || matches!(b, b'"' | b'\'' | b'(' | b')' | b'\\')
                    });
                if needs_quote {
                    let _ = write!(buf, "url({})", crate::substitution::quote_css_string(raw));
                } else {
                    let _ = write!(buf, "url({raw})");
                }
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

fn needs_token_boundary_comment(prev: &CssToken, next: &CssToken) -> bool {
    matches!(
        (prev, next),
        (
            CssToken::Number(_) | CssToken::Dimension(_, _) | CssToken::Percentage(_),
            CssToken::Ident(_)
                | CssToken::Number(_)
                | CssToken::Dimension(_, _)
                | CssToken::Percentage(_)
                | CssToken::Delim('%' | '+' | '-')
                | CssToken::Function { .. }
        ) | (
            CssToken::Ident(_) | CssToken::Hash(_),
            CssToken::Ident(_)
                | CssToken::Number(_)
                | CssToken::Dimension(_, _)
                | CssToken::Percentage(_)
                | CssToken::Function { .. }
                | CssToken::ParenOpen
                | CssToken::Delim('-')
        ) | (
            CssToken::Delim('+' | '-' | '.' | '/'),
            CssToken::Number(_)
                | CssToken::Dimension(_, _)
                | CssToken::Percentage(_)
                | CssToken::Ident(_)
                | CssToken::Delim('*' | '-')
        )
    )
}

/// Formats tokens for re-parsing after `var()` expansion, inserting `/**/` separators
/// between adjacent tokens that would otherwise fuse into a single CSS token (CSS Syntax L3 §6).
fn write_tokens_for_reparse(tokens: &[CssToken], buf: &mut String) {
    let mut prev: Option<&CssToken> = None;
    for tok in tokens {
        if let Some(p) = prev {
            if needs_token_boundary_comment(p, tok) {
                buf.push_str("/**/");
            }
        }
        match tok {
            CssToken::Function { name, args } => {
                buf.push_str(name.as_str());
                buf.push('(');
                write_tokens_for_reparse(args, buf);
                buf.push(')');
            }
            _ => write_tokens_to_buf(std::slice::from_ref(tok), buf),
        }
        prev = Some(tok);
    }
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
    BackgroundPosition,
    BackgroundPositionX,
    BackgroundPositionY,
    ObjectPosition,
    OffsetDistance,
    ShapeMargin,
    StrokeWidth,
    StrokeDashoffset,
    StrokeDasharray,
    ScrollPaddingTop,
    ScrollPaddingRight,
    ScrollPaddingBottom,
    ScrollPaddingLeft,
    SvgCx,
    SvgCy,
    SvgRx,
    SvgRy,
    SvgX,
    SvgY,
    BorderImageSlice,
    AspectRatio,
    BoxShadow,
    Transform,
    TransformOrigin,
    Scale,
    Rotate,
    Translate,
    ImageResolution,
    Orphans,
    Widows,
    TabSize,
    ColumnCount,
    Filter,
    ClipPath,
    MixBlendMode,
    Transition,
    TransitionProperty,
    TransitionDuration,
    TransitionDelay,
    TransitionTimingFunction,
    TransitionBehavior,
    Animation,
    AnimationName,
    AnimationDuration,
    AnimationDelay,
    AnimationTimingFunction,
    AnimationIterationCount,
    AnimationDirection,
    AnimationFillMode,
    AnimationPlayState,
    AnimationComposition,
    InterpolateSize,
    ViewTransitionName,
    Overlay,
    ReadingFlow,
    ReadingOrder,
    TextJustify,
    TextAlignLast,
    HyphenateCharacter,
    HyphenateLimitLines,
    Contain,
    // Typography
    FontSize,
    FontWeight,
    FontStyle,
    FontFamily,
    LineHeight,
    LetterSpacing,
    WordSpacing,
    TextIndent,
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
    // CSS Writing Modes & Logical Properties Level 1
    WritingMode,
    Resize,
    CaptionSide,
    OverscrollBehaviorX,
    OverscrollBehaviorY,
    OverscrollBehaviorInline,
    OverscrollBehaviorBlock,
    OverscrollBehavior,
    InlineSize,
    BlockSize,
    MinInlineSize,
    MinBlockSize,
    MaxInlineSize,
    MaxBlockSize,
    MarginBlockStart,
    MarginBlockEnd,
    MarginInlineStart,
    MarginInlineEnd,
    MarginBlock,
    MarginInline,
    PaddingBlockStart,
    PaddingBlockEnd,
    PaddingInlineStart,
    PaddingInlineEnd,
    PaddingBlock,
    PaddingInline,
    InsetBlockStart,
    InsetBlockEnd,
    InsetInlineStart,
    InsetInlineEnd,
    InsetBlock,
    InsetInline,
    BorderBlockStartWidth,
    BorderBlockEndWidth,
    BorderInlineStartWidth,
    BorderInlineEndWidth,
    BorderBlockWidth,
    BorderInlineWidth,
    BorderBlockStartStyle,
    BorderBlockEndStyle,
    BorderInlineStartStyle,
    BorderInlineEndStyle,
    BorderBlockStyle,
    BorderInlineStyle,
    BorderBlockStartColor,
    BorderBlockEndColor,
    BorderInlineStartColor,
    BorderInlineEndColor,
    BorderBlockColor,
    BorderInlineColor,
    BorderBlock,
    BorderBlockStart,
    BorderBlockEnd,
    BorderInline,
    BorderInlineStart,
    BorderInlineEnd,
    BorderStartStartRadius,
    BorderStartEndRadius,
    BorderEndStartRadius,
    BorderEndEndRadius,
    // CSS Cascade L5 `all` shorthand
    All,
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
            "width" => Self::Width,
            "height" => Self::Height,
            "min-width" => Self::MinWidth,
            "min-height" => Self::MinHeight,
            "max-width" => Self::MaxWidth,
            "max-height" => Self::MaxHeight,
            "margin-top" => Self::MarginTop,
            "margin-right" => Self::MarginRight,
            "margin-bottom" => Self::MarginBottom,
            "margin-left" => Self::MarginLeft,
            "padding-top" => Self::PaddingTop,
            "padding-right" => Self::PaddingRight,
            "padding-bottom" => Self::PaddingBottom,
            "padding-left" => Self::PaddingLeft,
            "top" => Self::Top,
            "right" => Self::Right,
            "bottom" => Self::Bottom,
            "left" => Self::Left,
            // CSS Logical Properties Level 1 & Writing Modes
            "writing-mode" => Self::WritingMode,
            "resize" => Self::Resize,
            "caption-side" => Self::CaptionSide,
            "overscroll-behavior-x" => Self::OverscrollBehaviorX,
            "overscroll-behavior-y" => Self::OverscrollBehaviorY,
            "overscroll-behavior-inline" => Self::OverscrollBehaviorInline,
            "overscroll-behavior-block" => Self::OverscrollBehaviorBlock,
            "overscroll-behavior" => Self::OverscrollBehavior,
            "inline-size" => Self::InlineSize,
            "block-size" => Self::BlockSize,
            "min-inline-size" => Self::MinInlineSize,
            "min-block-size" => Self::MinBlockSize,
            "max-inline-size" => Self::MaxInlineSize,
            "max-block-size" => Self::MaxBlockSize,
            "margin-block-start" => Self::MarginBlockStart,
            "margin-block-end" => Self::MarginBlockEnd,
            "margin-inline-start" => Self::MarginInlineStart,
            "margin-inline-end" => Self::MarginInlineEnd,
            "margin-block" => Self::MarginBlock,
            "margin-inline" => Self::MarginInline,
            "padding-block-start" => Self::PaddingBlockStart,
            "padding-block-end" => Self::PaddingBlockEnd,
            "padding-inline-start" => Self::PaddingInlineStart,
            "padding-inline-end" => Self::PaddingInlineEnd,
            "padding-block" => Self::PaddingBlock,
            "padding-inline" => Self::PaddingInline,
            "inset-block-start" => Self::InsetBlockStart,
            "inset-block-end" => Self::InsetBlockEnd,
            "inset-inline-start" => Self::InsetInlineStart,
            "inset-inline-end" => Self::InsetInlineEnd,
            "inset-block" => Self::InsetBlock,
            "inset-inline" => Self::InsetInline,
            "border-block-start-width" => Self::BorderBlockStartWidth,
            "border-block-end-width" => Self::BorderBlockEndWidth,
            "border-inline-start-width" => Self::BorderInlineStartWidth,
            "border-inline-end-width" => Self::BorderInlineEndWidth,
            "border-block-width" => Self::BorderBlockWidth,
            "border-inline-width" => Self::BorderInlineWidth,
            "border-block-start-style" => Self::BorderBlockStartStyle,
            "border-block-end-style" => Self::BorderBlockEndStyle,
            "border-inline-start-style" => Self::BorderInlineStartStyle,
            "border-inline-end-style" => Self::BorderInlineEndStyle,
            "border-block-style" => Self::BorderBlockStyle,
            "border-inline-style" => Self::BorderInlineStyle,
            "border-block-start-color" => Self::BorderBlockStartColor,
            "border-block-end-color" => Self::BorderBlockEndColor,
            "border-inline-start-color" => Self::BorderInlineStartColor,
            "border-inline-end-color" => Self::BorderInlineEndColor,
            "border-block-color" => Self::BorderBlockColor,
            "border-inline-color" => Self::BorderInlineColor,
            "border-block" => Self::BorderBlock,
            "border-block-start" => Self::BorderBlockStart,
            "border-block-end" => Self::BorderBlockEnd,
            "border-inline" => Self::BorderInline,
            "border-inline-start" => Self::BorderInlineStart,
            "border-inline-end" => Self::BorderInlineEnd,
            "border-start-start-radius" => Self::BorderStartStartRadius,
            "border-start-end-radius" => Self::BorderStartEndRadius,
            "border-end-start-radius" => Self::BorderEndStartRadius,
            "border-end-end-radius" => Self::BorderEndEndRadius,
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
            "background-position" => Self::BackgroundPosition,
            "background-position-x" => Self::BackgroundPositionX,
            "background-position-y" => Self::BackgroundPositionY,
            "object-position" => Self::ObjectPosition,
            "offset-distance" => Self::OffsetDistance,
            "shape-margin" => Self::ShapeMargin,
            "stroke-width" => Self::StrokeWidth,
            "stroke-dashoffset" => Self::StrokeDashoffset,
            "stroke-dasharray" => Self::StrokeDasharray,
            "scroll-padding-top" => Self::ScrollPaddingTop,
            "scroll-padding-right" => Self::ScrollPaddingRight,
            "scroll-padding-bottom" => Self::ScrollPaddingBottom,
            "scroll-padding-left" => Self::ScrollPaddingLeft,
            "cx" => Self::SvgCx,
            "cy" => Self::SvgCy,
            "rx" => Self::SvgRx,
            "ry" => Self::SvgRy,
            "x" => Self::SvgX,
            "y" => Self::SvgY,
            "border-image-slice" => Self::BorderImageSlice,
            "aspect-ratio" => Self::AspectRatio,
            "box-shadow" => Self::BoxShadow,
            "transform" => Self::Transform,
            "transform-origin" => Self::TransformOrigin,
            "scale" => Self::Scale,
            "rotate" => Self::Rotate,
            "translate" => Self::Translate,
            "image-resolution" => Self::ImageResolution,
            "orphans" => Self::Orphans,
            "widows" => Self::Widows,
            "tab-size" => Self::TabSize,
            "column-count" => Self::ColumnCount,
            "filter" => Self::Filter,
            "clip-path" => Self::ClipPath,
            "mix-blend-mode" => Self::MixBlendMode,
            "transition" => Self::Transition,
            "transition-property" => Self::TransitionProperty,
            "transition-duration" => Self::TransitionDuration,
            "transition-delay" => Self::TransitionDelay,
            "transition-timing-function" => Self::TransitionTimingFunction,
            "transition-behavior" => Self::TransitionBehavior,
            "animation" => Self::Animation,
            "animation-name" => Self::AnimationName,
            "animation-duration" => Self::AnimationDuration,
            "animation-delay" => Self::AnimationDelay,
            "animation-timing-function" => Self::AnimationTimingFunction,
            "animation-iteration-count" => Self::AnimationIterationCount,
            "animation-direction" => Self::AnimationDirection,
            "animation-fill-mode" => Self::AnimationFillMode,
            "animation-play-state" => Self::AnimationPlayState,
            "animation-composition" => Self::AnimationComposition,
            "interpolate-size" => Self::InterpolateSize,
            "view-transition-name" => Self::ViewTransitionName,
            "overlay" => Self::Overlay,
            "reading-flow" => Self::ReadingFlow,
            "reading-order" => Self::ReadingOrder,
            "text-justify" => Self::TextJustify,
            "text-align-last" => Self::TextAlignLast,
            "hyphenate-character" => Self::HyphenateCharacter,
            "hyphenate-limit-lines" => Self::HyphenateLimitLines,
            "contain" => Self::Contain,
            "font-size" => Self::FontSize,
            "font-weight" => Self::FontWeight,
            "font-style" => Self::FontStyle,
            "font-family" => Self::FontFamily,
            "line-height" => Self::LineHeight,
            "letter-spacing" => Self::LetterSpacing,
            "word-spacing" => Self::WordSpacing,
            "text-indent" => Self::TextIndent,
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
            "all" => Self::All,
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
            Self::BackgroundPosition => "background-position",
            Self::BackgroundPositionX => "background-position-x",
            Self::BackgroundPositionY => "background-position-y",
            Self::ObjectPosition => "object-position",
            Self::OffsetDistance => "offset-distance",
            Self::ShapeMargin => "shape-margin",
            Self::StrokeWidth => "stroke-width",
            Self::StrokeDashoffset => "stroke-dashoffset",
            Self::StrokeDasharray => "stroke-dasharray",
            Self::ScrollPaddingTop => "scroll-padding-top",
            Self::ScrollPaddingRight => "scroll-padding-right",
            Self::ScrollPaddingBottom => "scroll-padding-bottom",
            Self::ScrollPaddingLeft => "scroll-padding-left",
            Self::SvgCx => "cx",
            Self::SvgCy => "cy",
            Self::SvgRx => "rx",
            Self::SvgRy => "ry",
            Self::SvgX => "x",
            Self::SvgY => "y",
            Self::BorderImageSlice => "border-image-slice",
            Self::AspectRatio => "aspect-ratio",
            Self::BoxShadow => "box-shadow",
            Self::Transform => "transform",
            Self::TransformOrigin => "transform-origin",
            Self::Scale => "scale",
            Self::Rotate => "rotate",
            Self::Translate => "translate",
            Self::ImageResolution => "image-resolution",
            Self::Orphans => "orphans",
            Self::Widows => "widows",
            Self::TabSize => "tab-size",
            Self::ColumnCount => "column-count",
            Self::Filter => "filter",
            Self::ClipPath => "clip-path",
            Self::MixBlendMode => "mix-blend-mode",
            Self::Transition => "transition",
            Self::TransitionProperty => "transition-property",
            Self::TransitionDuration => "transition-duration",
            Self::TransitionDelay => "transition-delay",
            Self::TransitionTimingFunction => "transition-timing-function",
            Self::TransitionBehavior => "transition-behavior",
            Self::Animation => "animation",
            Self::AnimationName => "animation-name",
            Self::AnimationDuration => "animation-duration",
            Self::AnimationDelay => "animation-delay",
            Self::AnimationTimingFunction => "animation-timing-function",
            Self::AnimationIterationCount => "animation-iteration-count",
            Self::AnimationDirection => "animation-direction",
            Self::AnimationFillMode => "animation-fill-mode",
            Self::AnimationPlayState => "animation-play-state",
            Self::AnimationComposition => "animation-composition",
            Self::InterpolateSize => "interpolate-size",
            Self::ViewTransitionName => "view-transition-name",
            Self::Overlay => "overlay",
            Self::ReadingFlow => "reading-flow",
            Self::ReadingOrder => "reading-order",
            Self::TextJustify => "text-justify",
            Self::TextAlignLast => "text-align-last",
            Self::HyphenateCharacter => "hyphenate-character",
            Self::HyphenateLimitLines => "hyphenate-limit-lines",
            Self::Contain => "contain",
            Self::FontSize => "font-size",
            Self::FontWeight => "font-weight",
            Self::FontStyle => "font-style",
            Self::FontFamily => "font-family",
            Self::LineHeight => "line-height",
            Self::LetterSpacing => "letter-spacing",
            Self::WordSpacing => "word-spacing",
            Self::TextIndent => "text-indent",
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
            Self::WritingMode => "writing-mode",
            Self::Resize => "resize",
            Self::CaptionSide => "caption-side",
            Self::OverscrollBehaviorX => "overscroll-behavior-x",
            Self::OverscrollBehaviorY => "overscroll-behavior-y",
            Self::OverscrollBehaviorInline => "overscroll-behavior-inline",
            Self::OverscrollBehaviorBlock => "overscroll-behavior-block",
            Self::OverscrollBehavior => "overscroll-behavior",
            Self::InlineSize => "inline-size",
            Self::BlockSize => "block-size",
            Self::MinInlineSize => "min-inline-size",
            Self::MinBlockSize => "min-block-size",
            Self::MaxInlineSize => "max-inline-size",
            Self::MaxBlockSize => "max-block-size",
            Self::MarginBlockStart => "margin-block-start",
            Self::MarginBlockEnd => "margin-block-end",
            Self::MarginInlineStart => "margin-inline-start",
            Self::MarginInlineEnd => "margin-inline-end",
            Self::MarginBlock => "margin-block",
            Self::MarginInline => "margin-inline",
            Self::PaddingBlockStart => "padding-block-start",
            Self::PaddingBlockEnd => "padding-block-end",
            Self::PaddingInlineStart => "padding-inline-start",
            Self::PaddingInlineEnd => "padding-inline-end",
            Self::PaddingBlock => "padding-block",
            Self::PaddingInline => "padding-inline",
            Self::InsetBlockStart => "inset-block-start",
            Self::InsetBlockEnd => "inset-block-end",
            Self::InsetInlineStart => "inset-inline-start",
            Self::InsetInlineEnd => "inset-inline-end",
            Self::InsetBlock => "inset-block",
            Self::InsetInline => "inset-inline",
            Self::BorderBlockStartWidth => "border-block-start-width",
            Self::BorderBlockEndWidth => "border-block-end-width",
            Self::BorderInlineStartWidth => "border-inline-start-width",
            Self::BorderInlineEndWidth => "border-inline-end-width",
            Self::BorderBlockWidth => "border-block-width",
            Self::BorderInlineWidth => "border-inline-width",
            Self::BorderBlockStartStyle => "border-block-start-style",
            Self::BorderBlockEndStyle => "border-block-end-style",
            Self::BorderInlineStartStyle => "border-inline-start-style",
            Self::BorderInlineEndStyle => "border-inline-end-style",
            Self::BorderBlockStyle => "border-block-style",
            Self::BorderInlineStyle => "border-inline-style",
            Self::BorderBlockStartColor => "border-block-start-color",
            Self::BorderBlockEndColor => "border-block-end-color",
            Self::BorderInlineStartColor => "border-inline-start-color",
            Self::BorderInlineEndColor => "border-inline-end-color",
            Self::BorderBlockColor => "border-block-color",
            Self::BorderInlineColor => "border-inline-color",
            Self::BorderBlock => "border-block",
            Self::BorderBlockStart => "border-block-start",
            Self::BorderBlockEnd => "border-block-end",
            Self::BorderInline => "border-inline",
            Self::BorderInlineStart => "border-inline-start",
            Self::BorderInlineEnd => "border-inline-end",
            Self::BorderStartStartRadius => "border-start-start-radius",
            Self::BorderStartEndRadius => "border-start-end-radius",
            Self::BorderEndStartRadius => "border-end-start-radius",
            Self::BorderEndEndRadius => "border-end-end-radius",
            Self::All => "all",
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
                | Self::TextIndent
                | Self::TextAlign
                | Self::TextTransform
                | Self::WhiteSpace
                | Self::WordBreak
                | Self::OverflowWrap
                | Self::Direction
                | Self::WritingMode
                | Self::CaptionSide
                | Self::Orphans
                | Self::Widows
                | Self::TabSize
                | Self::InterpolateSize
                | Self::TextJustify
                | Self::TextAlignLast
                | Self::HyphenateCharacter
                | Self::HyphenateLimitLines
                | Self::BorderCollapse
                | Self::BorderSpacing
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
            Self::MarginBlock => {
                SmallVec::from_slice(&[Self::MarginBlockStart, Self::MarginBlockEnd])
            }
            Self::MarginInline => {
                SmallVec::from_slice(&[Self::MarginInlineStart, Self::MarginInlineEnd])
            }
            Self::Padding => SmallVec::from_slice(&[
                Self::PaddingTop,
                Self::PaddingRight,
                Self::PaddingBottom,
                Self::PaddingLeft,
            ]),
            Self::PaddingBlock => {
                SmallVec::from_slice(&[Self::PaddingBlockStart, Self::PaddingBlockEnd])
            }
            Self::PaddingInline => {
                SmallVec::from_slice(&[Self::PaddingInlineStart, Self::PaddingInlineEnd])
            }
            Self::Inset => {
                SmallVec::from_slice(&[Self::Top, Self::Right, Self::Bottom, Self::Left])
            }
            Self::InsetBlock => SmallVec::from_slice(&[Self::InsetBlockStart, Self::InsetBlockEnd]),
            Self::InsetInline => {
                SmallVec::from_slice(&[Self::InsetInlineStart, Self::InsetInlineEnd])
            }
            Self::BorderWidth => SmallVec::from_slice(&[
                Self::BorderTopWidth,
                Self::BorderRightWidth,
                Self::BorderBottomWidth,
                Self::BorderLeftWidth,
            ]),
            Self::BorderBlockWidth => {
                SmallVec::from_slice(&[Self::BorderBlockStartWidth, Self::BorderBlockEndWidth])
            }
            Self::BorderInlineWidth => {
                SmallVec::from_slice(&[Self::BorderInlineStartWidth, Self::BorderInlineEndWidth])
            }
            Self::BorderStyle => SmallVec::from_slice(&[
                Self::BorderTopStyle,
                Self::BorderRightStyle,
                Self::BorderBottomStyle,
                Self::BorderLeftStyle,
            ]),
            Self::BorderBlockStyle => {
                SmallVec::from_slice(&[Self::BorderBlockStartStyle, Self::BorderBlockEndStyle])
            }
            Self::BorderInlineStyle => {
                SmallVec::from_slice(&[Self::BorderInlineStartStyle, Self::BorderInlineEndStyle])
            }
            Self::BorderColor => SmallVec::from_slice(&[
                Self::BorderTopColor,
                Self::BorderRightColor,
                Self::BorderBottomColor,
                Self::BorderLeftColor,
            ]),
            Self::BorderBlockColor => {
                SmallVec::from_slice(&[Self::BorderBlockStartColor, Self::BorderBlockEndColor])
            }
            Self::BorderInlineColor => {
                SmallVec::from_slice(&[Self::BorderInlineStartColor, Self::BorderInlineEndColor])
            }
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
            Self::BorderBlockStart => SmallVec::from_slice(&[
                Self::BorderBlockStartWidth,
                Self::BorderBlockStartStyle,
                Self::BorderBlockStartColor,
            ]),
            Self::BorderBlockEnd => SmallVec::from_slice(&[
                Self::BorderBlockEndWidth,
                Self::BorderBlockEndStyle,
                Self::BorderBlockEndColor,
            ]),
            Self::BorderInlineStart => SmallVec::from_slice(&[
                Self::BorderInlineStartWidth,
                Self::BorderInlineStartStyle,
                Self::BorderInlineStartColor,
            ]),
            Self::BorderInlineEnd => SmallVec::from_slice(&[
                Self::BorderInlineEndWidth,
                Self::BorderInlineEndStyle,
                Self::BorderInlineEndColor,
            ]),
            Self::BorderBlock => SmallVec::from_slice(&[
                Self::BorderBlockStartWidth,
                Self::BorderBlockEndWidth,
                Self::BorderBlockStartStyle,
                Self::BorderBlockEndStyle,
                Self::BorderBlockStartColor,
                Self::BorderBlockEndColor,
            ]),
            Self::BorderInline => SmallVec::from_slice(&[
                Self::BorderInlineStartWidth,
                Self::BorderInlineEndWidth,
                Self::BorderInlineStartStyle,
                Self::BorderInlineEndStyle,
                Self::BorderInlineStartColor,
                Self::BorderInlineEndColor,
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
            Self::OverscrollBehavior => {
                SmallVec::from_slice(&[Self::OverscrollBehaviorX, Self::OverscrollBehaviorY])
            }
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
            Self::Transition => SmallVec::from_slice(&[
                Self::TransitionProperty,
                Self::TransitionDuration,
                Self::TransitionTimingFunction,
                Self::TransitionDelay,
                Self::TransitionBehavior,
            ]),
            Self::Animation => SmallVec::from_slice(&[
                Self::AnimationName,
                Self::AnimationDuration,
                Self::AnimationTimingFunction,
                Self::AnimationDelay,
                Self::AnimationIterationCount,
                Self::AnimationDirection,
                Self::AnimationFillMode,
                Self::AnimationPlayState,
            ]),
            other => {
                let mut v = SmallVec::new();
                v.push(other);
                v
            }
        }
    }
}

/// Complete list of supported CSS longhand property names exposed by `getComputedStyle(el)`
/// (`length`, `item(i)`, `ownKeys`, `getOwnPropertyDescriptor`).
pub const COMPUTED_STYLE_LONGHANDS: &[&str] = &[
    "align-content",
    "align-items",
    "align-self",
    "animation-composition",
    "animation-delay",
    "animation-direction",
    "animation-duration",
    "animation-fill-mode",
    "animation-iteration-count",
    "animation-name",
    "animation-play-state",
    "animation-timing-function",
    "background-color",
    "background-image",
    "background-position",
    "block-size",
    "border-block-end-color",
    "border-block-end-style",
    "border-block-end-width",
    "border-block-start-color",
    "border-block-start-style",
    "border-block-start-width",
    "border-bottom-color",
    "border-bottom-left-radius",
    "border-bottom-right-radius",
    "border-bottom-style",
    "border-bottom-width",
    "border-collapse",
    "border-end-end-radius",
    "border-end-start-radius",
    "border-inline-end-color",
    "border-inline-end-style",
    "border-inline-end-width",
    "border-inline-start-color",
    "border-inline-start-style",
    "border-inline-start-width",
    "border-left-color",
    "border-left-style",
    "border-left-width",
    "border-right-color",
    "border-right-style",
    "border-right-width",
    "border-spacing",
    "border-start-end-radius",
    "border-start-start-radius",
    "border-top-color",
    "border-top-left-radius",
    "border-top-right-radius",
    "border-top-style",
    "border-top-width",
    "bottom",
    "box-shadow",
    "box-sizing",
    "caption-side",
    "clear",
    "clip-path",
    "color",
    "color-scheme",
    "column-count",
    "column-gap",
    "content",
    "cursor",
    "direction",
    "display",
    "filter",
    "flex-basis",
    "flex-direction",
    "flex-grow",
    "flex-shrink",
    "flex-wrap",
    "float",
    "font-family",
    "font-size",
    "font-style",
    "font-weight",
    "grid-auto-columns",
    "grid-auto-flow",
    "grid-auto-rows",
    "grid-column-end",
    "grid-column-start",
    "grid-row-end",
    "grid-row-start",
    "grid-template-areas",
    "grid-template-columns",
    "grid-template-rows",
    "height",
    "image-resolution",
    "inline-size",
    "inset-block-end",
    "inset-block-start",
    "inset-inline-end",
    "inset-inline-start",
    "interpolate-size",
    "overlay",
    "reading-flow",
    "reading-order",
    "text-justify",
    "text-align-last",
    "hyphenate-character",
    "hyphenate-limit-lines",
    "justify-content",
    "justify-items",
    "justify-self",
    "left",
    "letter-spacing",
    "line-height",
    "margin-block-end",
    "margin-block-start",
    "margin-bottom",
    "margin-inline-end",
    "margin-inline-start",
    "margin-left",
    "margin-right",
    "margin-top",
    "max-block-size",
    "max-height",
    "max-inline-size",
    "max-width",
    "min-block-size",
    "min-height",
    "min-inline-size",
    "min-width",
    "mix-blend-mode",
    "object-position",
    "offset-distance",
    "opacity",
    "scroll-padding-bottom",
    "scroll-padding-left",
    "scroll-padding-right",
    "scroll-padding-top",
    "shape-margin",
    "stroke-dasharray",
    "stroke-dashoffset",
    "stroke-width",
    "background-position-x",
    "background-position-y",
    "border-image-slice",
    "aspect-ratio",
    "cx",
    "cy",
    "rx",
    "ry",
    "x",
    "y",
    "order",
    "orphans",
    "outline-color",
    "outline-offset",
    "outline-style",
    "outline-width",
    "overflow-wrap",
    "overflow-x",
    "overflow-y",
    "overscroll-behavior-block",
    "overscroll-behavior-inline",
    "overscroll-behavior-x",
    "overscroll-behavior-y",
    "padding-block-end",
    "padding-block-start",
    "padding-bottom",
    "padding-inline-end",
    "padding-inline-start",
    "padding-left",
    "padding-right",
    "padding-top",
    "pointer-events",
    "position",
    "resize",
    "right",
    "rotate",
    "row-gap",
    "scale",
    "tab-size",
    "table-layout",
    "text-align",
    "text-decoration-color",
    "text-decoration-line",
    "text-indent",
    "text-overflow",
    "text-transform",
    "top",
    "transform",
    "transform-origin",
    "transition-behavior",
    "transition-delay",
    "transition-duration",
    "transition-property",
    "transition-timing-function",
    "translate",
    "user-select",
    "vertical-align",
    "visibility",
    "white-space",
    "widows",
    "width",
    "word-break",
    "word-spacing",
    "writing-mode",
    "z-index",
];

/// All physical CSS longhand properties reset by the CSS Cascade Level 5 `all` shorthand
/// (excludes `direction`, `unicode-bidi`, and custom properties per §3.1).
pub const ALL_RESETTABLE_LONGHANDS: &[PropertyIdKind] = &[
    PropertyIdKind::Display,
    PropertyIdKind::Position,
    PropertyIdKind::Float,
    PropertyIdKind::Clear,
    PropertyIdKind::BoxSizing,
    PropertyIdKind::OverflowX,
    PropertyIdKind::OverflowY,
    PropertyIdKind::Visibility,
    PropertyIdKind::PointerEvents,
    PropertyIdKind::Cursor,
    PropertyIdKind::ZIndex,
    PropertyIdKind::Opacity,
    PropertyIdKind::Width,
    PropertyIdKind::Height,
    PropertyIdKind::MinWidth,
    PropertyIdKind::MinHeight,
    PropertyIdKind::MaxWidth,
    PropertyIdKind::MaxHeight,
    PropertyIdKind::MarginTop,
    PropertyIdKind::MarginRight,
    PropertyIdKind::MarginBottom,
    PropertyIdKind::MarginLeft,
    PropertyIdKind::PaddingTop,
    PropertyIdKind::PaddingRight,
    PropertyIdKind::PaddingBottom,
    PropertyIdKind::PaddingLeft,
    PropertyIdKind::Top,
    PropertyIdKind::Right,
    PropertyIdKind::Bottom,
    PropertyIdKind::Left,
    PropertyIdKind::BorderTopWidth,
    PropertyIdKind::BorderRightWidth,
    PropertyIdKind::BorderBottomWidth,
    PropertyIdKind::BorderLeftWidth,
    PropertyIdKind::BorderTopStyle,
    PropertyIdKind::BorderRightStyle,
    PropertyIdKind::BorderBottomStyle,
    PropertyIdKind::BorderLeftStyle,
    PropertyIdKind::BorderTopColor,
    PropertyIdKind::BorderRightColor,
    PropertyIdKind::BorderBottomColor,
    PropertyIdKind::BorderLeftColor,
    PropertyIdKind::BorderTopLeftRadius,
    PropertyIdKind::BorderTopRightRadius,
    PropertyIdKind::BorderBottomRightRadius,
    PropertyIdKind::BorderBottomLeftRadius,
    PropertyIdKind::OutlineColor,
    PropertyIdKind::OutlineStyle,
    PropertyIdKind::OutlineWidth,
    PropertyIdKind::OutlineOffset,
    PropertyIdKind::Color,
    PropertyIdKind::BackgroundColor,
    PropertyIdKind::BackgroundLayers,
    PropertyIdKind::BackgroundPosition,
    PropertyIdKind::ObjectPosition,
    PropertyIdKind::BoxShadow,
    PropertyIdKind::Transform,
    PropertyIdKind::TransformOrigin,
    PropertyIdKind::Scale,
    PropertyIdKind::Rotate,
    PropertyIdKind::Translate,
    PropertyIdKind::ImageResolution,
    PropertyIdKind::Orphans,
    PropertyIdKind::Widows,
    PropertyIdKind::TabSize,
    PropertyIdKind::ColumnCount,
    PropertyIdKind::Filter,
    PropertyIdKind::ClipPath,
    PropertyIdKind::MixBlendMode,
    PropertyIdKind::Transition,
    PropertyIdKind::TransitionProperty,
    PropertyIdKind::TransitionDuration,
    PropertyIdKind::TransitionDelay,
    PropertyIdKind::TransitionTimingFunction,
    PropertyIdKind::TransitionBehavior,
    PropertyIdKind::Animation,
    PropertyIdKind::AnimationName,
    PropertyIdKind::AnimationDuration,
    PropertyIdKind::AnimationDelay,
    PropertyIdKind::AnimationTimingFunction,
    PropertyIdKind::AnimationIterationCount,
    PropertyIdKind::AnimationDirection,
    PropertyIdKind::AnimationFillMode,
    PropertyIdKind::AnimationPlayState,
    PropertyIdKind::AnimationComposition,
    PropertyIdKind::InterpolateSize,
    PropertyIdKind::Overlay,
    PropertyIdKind::ReadingFlow,
    PropertyIdKind::ReadingOrder,
    PropertyIdKind::TextJustify,
    PropertyIdKind::TextAlignLast,
    PropertyIdKind::HyphenateCharacter,
    PropertyIdKind::HyphenateLimitLines,
    PropertyIdKind::Contain,
    PropertyIdKind::FontSize,
    PropertyIdKind::FontWeight,
    PropertyIdKind::FontStyle,
    PropertyIdKind::FontFamily,
    PropertyIdKind::LineHeight,
    PropertyIdKind::LetterSpacing,
    PropertyIdKind::WordSpacing,
    PropertyIdKind::TextIndent,
    PropertyIdKind::TextAlign,
    PropertyIdKind::TextDecorationLine,
    PropertyIdKind::TextDecorationColor,
    PropertyIdKind::TextTransform,
    PropertyIdKind::WhiteSpace,
    PropertyIdKind::WordBreak,
    PropertyIdKind::OverflowWrap,
    PropertyIdKind::UserSelect,
    PropertyIdKind::FlexDirection,
    PropertyIdKind::FlexWrap,
    PropertyIdKind::FlexGrow,
    PropertyIdKind::FlexShrink,
    PropertyIdKind::FlexBasis,
    PropertyIdKind::Order,
    PropertyIdKind::AlignItems,
    PropertyIdKind::AlignSelf,
    PropertyIdKind::AlignContent,
    PropertyIdKind::JustifyContent,
    PropertyIdKind::JustifyItems,
    PropertyIdKind::JustifySelf,
    PropertyIdKind::RowGap,
    PropertyIdKind::ColumnGap,
    PropertyIdKind::GridTemplateColumns,
    PropertyIdKind::GridTemplateRows,
    PropertyIdKind::GridTemplateAreas,
    PropertyIdKind::GridAutoColumns,
    PropertyIdKind::GridAutoRows,
    PropertyIdKind::GridAutoFlow,
    PropertyIdKind::GridRowStart,
    PropertyIdKind::GridRowEnd,
    PropertyIdKind::GridColumnStart,
    PropertyIdKind::GridColumnEnd,
    PropertyIdKind::VerticalAlign,
    PropertyIdKind::TextOverflow,
    PropertyIdKind::BorderCollapse,
    PropertyIdKind::BorderSpacing,
    PropertyIdKind::TableLayout,
    PropertyIdKind::ColorScheme,
    PropertyIdKind::Content,
    PropertyIdKind::WritingMode,
    PropertyIdKind::Resize,
    PropertyIdKind::CaptionSide,
    PropertyIdKind::OverscrollBehaviorX,
    PropertyIdKind::OverscrollBehaviorY,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhysicalSide {
    Top,
    Right,
    Bottom,
    Left,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhysicalCorner {
    TopLeft,
    TopRight,
    BottomRight,
    BottomLeft,
}

/// Exact CSS Writing Modes L4 / CSS Logical Properties L1 side mapping for all
/// `(WritingMode, DirectionMode)` pairs: returns `(block_start, block_end, inline_start, inline_end)`.
const fn logical_sides(
    wm: WritingMode,
    dir: DirectionMode,
) -> (PhysicalSide, PhysicalSide, PhysicalSide, PhysicalSide) {
    use DirectionMode::{Ltr, Rtl};
    use PhysicalSide::{Bottom, Left, Right, Top};
    use WritingMode::{HorizontalTb, SidewaysLr, SidewaysRl, VerticalLr, VerticalRl};
    match (wm, dir) {
        (HorizontalTb, Ltr) => (Top, Bottom, Left, Right),
        (HorizontalTb, Rtl) => (Top, Bottom, Right, Left),
        (VerticalRl | SidewaysRl, Ltr) => (Right, Left, Top, Bottom),
        (VerticalRl | SidewaysRl, Rtl) => (Right, Left, Bottom, Top),
        (VerticalLr, Ltr) => (Left, Right, Top, Bottom),
        (VerticalLr, Rtl) => (Left, Right, Bottom, Top),
        (SidewaysLr, Ltr) => (Left, Right, Bottom, Top),
        (SidewaysLr, Rtl) => (Left, Right, Top, Bottom),
    }
}

const fn combine_sides_to_corner(a: PhysicalSide, b: PhysicalSide) -> PhysicalCorner {
    use PhysicalCorner::{BottomLeft, BottomRight, TopLeft, TopRight};
    use PhysicalSide::{Bottom, Left, Right, Top};
    match (a, b) {
        (Top, Left) | (Left, Top) => TopLeft,
        (Top, Right) | (Right, Top) => TopRight,
        (Bottom, Right) | (Right, Bottom) => BottomRight,
        _ => BottomLeft,
    }
}

/// Returns `true` if `prop` is a flow-relative logical longhand property.
#[must_use]
pub const fn is_logical_property(prop: PropertyIdKind) -> bool {
    matches!(
        prop,
        PropertyIdKind::InlineSize
            | PropertyIdKind::BlockSize
            | PropertyIdKind::MinInlineSize
            | PropertyIdKind::MinBlockSize
            | PropertyIdKind::MaxInlineSize
            | PropertyIdKind::MaxBlockSize
            | PropertyIdKind::MarginBlockStart
            | PropertyIdKind::MarginBlockEnd
            | PropertyIdKind::MarginInlineStart
            | PropertyIdKind::MarginInlineEnd
            | PropertyIdKind::PaddingBlockStart
            | PropertyIdKind::PaddingBlockEnd
            | PropertyIdKind::PaddingInlineStart
            | PropertyIdKind::PaddingInlineEnd
            | PropertyIdKind::InsetBlockStart
            | PropertyIdKind::InsetBlockEnd
            | PropertyIdKind::InsetInlineStart
            | PropertyIdKind::InsetInlineEnd
            | PropertyIdKind::BorderBlockStartWidth
            | PropertyIdKind::BorderBlockEndWidth
            | PropertyIdKind::BorderInlineStartWidth
            | PropertyIdKind::BorderInlineEndWidth
            | PropertyIdKind::BorderBlockStartStyle
            | PropertyIdKind::BorderBlockEndStyle
            | PropertyIdKind::BorderInlineStartStyle
            | PropertyIdKind::BorderInlineEndStyle
            | PropertyIdKind::BorderBlockStartColor
            | PropertyIdKind::BorderBlockEndColor
            | PropertyIdKind::BorderInlineStartColor
            | PropertyIdKind::BorderInlineEndColor
            | PropertyIdKind::BorderStartStartRadius
            | PropertyIdKind::BorderStartEndRadius
            | PropertyIdKind::BorderEndStartRadius
            | PropertyIdKind::BorderEndEndRadius
            | PropertyIdKind::OverscrollBehaviorInline
            | PropertyIdKind::OverscrollBehaviorBlock
    )
}

const fn pick_side(side: PhysicalSide, tblr: [PropertyIdKind; 4]) -> PropertyIdKind {
    match side {
        PhysicalSide::Top => tblr[0],
        PhysicalSide::Right => tblr[1],
        PhysicalSide::Bottom => tblr[2],
        PhysicalSide::Left => tblr[3],
    }
}

/// Resolves any CSS Logical Properties Level 1 longhand to its corresponding physical
/// [`PropertyIdKind`] given the element's computed `writing-mode` and `direction`.
#[must_use]
pub const fn resolve_logical_to_physical(
    prop: PropertyIdKind,
    wm: WritingMode,
    dir: DirectionMode,
) -> PropertyIdKind {
    use PropertyIdKind as P;
    let (bs, be, is, ie) = logical_sides(wm, dir);
    let vert = wm.is_vertical();
    const M: [P; 4] = [P::MarginTop, P::MarginRight, P::MarginBottom, P::MarginLeft];
    const PA: [P; 4] = [
        P::PaddingTop,
        P::PaddingRight,
        P::PaddingBottom,
        P::PaddingLeft,
    ];
    const IN: [P; 4] = [P::Top, P::Right, P::Bottom, P::Left];
    const BW: [P; 4] = [
        P::BorderTopWidth,
        P::BorderRightWidth,
        P::BorderBottomWidth,
        P::BorderLeftWidth,
    ];
    const BS: [P; 4] = [
        P::BorderTopStyle,
        P::BorderRightStyle,
        P::BorderBottomStyle,
        P::BorderLeftStyle,
    ];
    const BC: [P; 4] = [
        P::BorderTopColor,
        P::BorderRightColor,
        P::BorderBottomColor,
        P::BorderLeftColor,
    ];
    match prop {
        P::InlineSize => {
            if vert {
                P::Height
            } else {
                P::Width
            }
        }
        P::BlockSize => {
            if vert {
                P::Width
            } else {
                P::Height
            }
        }
        P::MinInlineSize => {
            if vert {
                P::MinHeight
            } else {
                P::MinWidth
            }
        }
        P::MinBlockSize => {
            if vert {
                P::MinWidth
            } else {
                P::MinHeight
            }
        }
        P::MaxInlineSize => {
            if vert {
                P::MaxHeight
            } else {
                P::MaxWidth
            }
        }
        P::MaxBlockSize => {
            if vert {
                P::MaxWidth
            } else {
                P::MaxHeight
            }
        }
        P::OverscrollBehaviorInline => {
            if vert {
                P::OverscrollBehaviorY
            } else {
                P::OverscrollBehaviorX
            }
        }
        P::OverscrollBehaviorBlock => {
            if vert {
                P::OverscrollBehaviorX
            } else {
                P::OverscrollBehaviorY
            }
        }
        P::MarginBlockStart => pick_side(bs, M),
        P::MarginBlockEnd => pick_side(be, M),
        P::MarginInlineStart => pick_side(is, M),
        P::MarginInlineEnd => pick_side(ie, M),
        P::PaddingBlockStart => pick_side(bs, PA),
        P::PaddingBlockEnd => pick_side(be, PA),
        P::PaddingInlineStart => pick_side(is, PA),
        P::PaddingInlineEnd => pick_side(ie, PA),
        P::InsetBlockStart => pick_side(bs, IN),
        P::InsetBlockEnd => pick_side(be, IN),
        P::InsetInlineStart => pick_side(is, IN),
        P::InsetInlineEnd => pick_side(ie, IN),
        P::BorderBlockStartWidth => pick_side(bs, BW),
        P::BorderBlockEndWidth => pick_side(be, BW),
        P::BorderInlineStartWidth => pick_side(is, BW),
        P::BorderInlineEndWidth => pick_side(ie, BW),
        P::BorderBlockStartStyle => pick_side(bs, BS),
        P::BorderBlockEndStyle => pick_side(be, BS),
        P::BorderInlineStartStyle => pick_side(is, BS),
        P::BorderInlineEndStyle => pick_side(ie, BS),
        P::BorderBlockStartColor => pick_side(bs, BC),
        P::BorderBlockEndColor => pick_side(be, BC),
        P::BorderInlineStartColor => pick_side(is, BC),
        P::BorderInlineEndColor => pick_side(ie, BC),
        P::BorderStartStartRadius
        | P::BorderStartEndRadius
        | P::BorderEndStartRadius
        | P::BorderEndEndRadius => {
            let corner = match prop {
                P::BorderStartStartRadius => combine_sides_to_corner(bs, is),
                P::BorderStartEndRadius => combine_sides_to_corner(bs, ie),
                P::BorderEndStartRadius => combine_sides_to_corner(be, is),
                _ => combine_sides_to_corner(be, ie),
            };
            match corner {
                PhysicalCorner::TopLeft => P::BorderTopLeftRadius,
                PhysicalCorner::TopRight => P::BorderTopRightRadius,
                PhysicalCorner::BottomRight => P::BorderBottomRightRadius,
                PhysicalCorner::BottomLeft => P::BorderBottomLeftRadius,
            }
        }
        other => other,
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
    Color4(Box<crate::color::CssColorExpr>),
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
    WritingMode(WritingMode),
    Resize(ResizeMode),
    CaptionSide(CaptionSideMode),
    OverscrollBehavior(OverscrollBehaviorMode),
    MathVal(Box<crate::values_math::MathNode>),
    Scale(crate::values_math::ScaleSpec),
    Rotate(crate::values_math::RotateSpec),
    Translate(crate::values_math::TranslateSpec),
    TransformRaw(String),
    BgPosition(LengthPercentage, LengthPercentage),
    ObjectPos(Box<crate::values_math::PositionSpec>),
    AspectRatioMath(
        Option<(
            Box<crate::values_math::MathNode>,
            Option<Box<crate::values_math::MathNode>>,
        )>,
    ),
    InterpolateSize(InterpolateSizeMode),
    TransitionBehaviorList(Vec<TransitionBehaviorMode>),
    AnimationCompositionList(Vec<AnimationCompositionMode>),
    ViewTransitionName(Option<InternedStr>),
    Overlay(OverlayMode),
    ReadingFlow(ReadingFlowMode),
    TextJustify(TextJustifyMode),
    TextAlignLast(TextAlignLastMode),
    HyphenateChar(Option<InternedStr>),
    HyphenateLimitLines(Option<i32>),
    Contain(u8),
    BackgroundImageRaw(String),
    FontStyleObliqueMath(Box<crate::values_math::MathNode>),
}

impl PropertyValue {
    /// Returns `true` if this declaration's value depends on `sibling-index()` or `sibling-count()`.
    #[must_use]
    pub fn contains_sibling_fn(&self) -> bool {
        let node_has =
            |n: &crate::values_math::MathNode| n.contains_sibling_fn() || n.contains_random();
        let lp_has = |lp: &LengthPercentage| {
            matches!(
                lp,
                LengthPercentage::Calc(c) if matches!(c.as_ref(), crate::values::CalcExpr::Math(n) if node_has(n))
            )
        };
        match self {
            Self::MathVal(n) => node_has(n),
            Self::Length(lp)
            | Self::LengthAuto(LengthPercentageAuto::Value(lp))
            | Self::Size(SizeValue::Length(lp)) => lp_has(lp),
            Self::Size(SizeValue::CalcSize(cs)) => cs.contains_sibling_fn(),
            Self::CornerRadius(cr) => lp_has(&cr.x) || lp_has(&cr.y),
            Self::BgPosition(x, y) => lp_has(x) || lp_has(y),
            Self::Scale(crate::values_math::ScaleSpec::Values(v)) => v.iter().any(node_has),
            Self::Rotate(crate::values_math::RotateSpec::Angle { axis, angle }) => {
                node_has(angle) || axis.as_ref().is_some_and(|a| a.iter().any(node_has))
            }
            Self::Translate(crate::values_math::TranslateSpec::Values { x, y, z }) => {
                node_has(x) || y.as_ref().is_some_and(node_has) || z.as_ref().is_some_and(node_has)
            }
            Self::TransformRaw(s) => {
                let l = s.to_ascii_lowercase();
                l.contains("sibling-") || l.contains("random(")
            }
            Self::GridTracks(v) => v.iter().any(|t| match t {
                GridTrackSize::FrMath(n) => node_has(n),
                GridTrackSize::Length(lp) => lp_has(lp),
                _ => false,
            }),
            Self::Color4(c) => {
                let s = c.to_specified_css_string();
                s.contains("sibling-") || s.contains("random(")
            }
            Self::AspectRatioMath(Some((a, b))) => {
                node_has(a) || b.as_deref().is_some_and(node_has)
            }
            Self::Custom(toks) | Self::Unresolved(toks) => {
                tokens_contain_sibling_fn(toks) || crate::substitution::tokens_contain_subst(toks)
            }
            Self::FontStyleObliqueMath(n) => node_has(n),
            _ => false,
        }
    }
}

fn tokens_contain_sibling_fn(toks: &[CssToken]) -> bool {
    toks.iter().any(|t| match t {
        CssToken::Function { name, args } => {
            let l = name.as_str().to_ascii_lowercase();
            l == "sibling-index"
                || l == "sibling-count"
                || l == "random"
                || tokens_contain_sibling_fn(args)
        }
        CssToken::Var {
            fallback: Some(fb), ..
        } => tokens_contain_sibling_fn(fb),
        _ => false,
    })
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

/// Rewrites CSS Variables L2 dynamic `var(...)` name arguments (`var(var(--x))`, `var({--x})`)
/// and CSS Cascade L5 `revert-rule` declaration values into forms accepted by `lightningcss`,
/// preserving exact spec semantics. Zero-allocation fast path when neither occurs.
#[must_use]
pub(crate) fn rewrite_dynamic_var_names(css: &str) -> std::borrow::Cow<'_, str> {
    let color_wrapped = wrap_color4_declarations(css);
    let src: &str = &color_wrapped;
    let bytes = src.as_bytes();
    let has_var = bytes.windows(4).any(|w| w.eq_ignore_ascii_case(b"var("));
    let has_rr = bytes
        .windows(11)
        .any(|w| w.eq_ignore_ascii_case(b"revert-rule"));
    if !has_var && !has_rr {
        return color_wrapped;
    }
    let mut out: Option<String> = None;
    let mut last = 0usize;
    let mut i = 0usize;
    let mut in_str: Option<u8> = None;
    let mut paren_depth = 0usize;
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
        if b == b'\\' {
            i += 2;
            continue;
        }
        if b == b'"' || b == b'\'' {
            in_str = Some(b);
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        if b == b'(' {
            paren_depth += 1;
        } else if b == b')' {
            paren_depth = paren_depth.saturating_sub(1);
        }
        if has_rr
            && paren_depth == 0
            && i + 11 <= bytes.len()
            && bytes[i..i + 11].eq_ignore_ascii_case(b"revert-rule")
        {
            // Rewrite standalone declaration value `: revert-rule` (before `;`, `}`, `!`, or EOF).
            let mut p = i;
            while p > 0 && bytes[p - 1].is_ascii_whitespace() {
                p -= 1;
            }
            let after_colon = p > 0 && bytes[p - 1] == b':';
            let mut q = i + 11;
            while q < bytes.len() && bytes[q].is_ascii_whitespace() {
                q += 1;
            }
            let before_end = q == bytes.len() || matches!(bytes[q], b';' | b'}' | b'!');
            if after_colon && before_end {
                let buf = out.get_or_insert_with(|| String::with_capacity(src.len() + 24));
                buf.push_str(&src[last..i]);
                buf.push_str("var(--__rix_revert_rule__)");
                i += 11;
                last = i;
                continue;
            }
        }
        if has_var
            && i + 4 <= bytes.len()
            && bytes[i..i + 4].eq_ignore_ascii_case(b"var(")
            && (i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'-'
                    || bytes[i - 1] == b'_'))
        {
            let mut k = i + 4;
            while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            // Case 1: `var(<subst-fn>(...))` or `var({<subst-fn>(...)})` -> rewrite outer `var(` to `__rix_var__(`
            let is_braced_plain_custom = bytes[k] == b'{'
                && src[k + 1..].find('}').is_some_and(|rel| {
                    let ins = src[k + 1..k + 1 + rel].trim();
                    ins.starts_with("--")
                        && ins.len() > 2
                        && !ins.contains(|c: char| c.is_ascii_whitespace() || c == ',' || c == '(')
                });
            if k < bytes.len()
                && !bytes[k..].starts_with(b"--")
                && !is_braced_plain_custom
                && crate::substitution::contains_subst_fn_str(&src[k..])
            {
                let buf = out.get_or_insert_with(|| String::with_capacity(src.len() + 16));
                buf.push_str(&src[last..i]);
                buf.push_str("__rix_var__(");
                i += 4;
                last = i;
                continue;
            }
            // Case 2: `var({--ident} ...)` -> unwrap `{` and `}` around single dashed-ident name
            if k < bytes.len() && bytes[k] == b'{' {
                if let Some(rel_close) = src[k + 1..].find('}') {
                    let inside = src[k + 1..k + 1 + rel_close].trim();
                    if inside.starts_with("--")
                        && inside.len() > 2
                        && !inside.contains(|c: char| c.is_ascii_whitespace() || c == ',')
                    {
                        let buf = out.get_or_insert_with(|| String::with_capacity(src.len()));
                        buf.push_str(&src[last..k]);
                        buf.push_str(inside);
                        i = k + 1 + rel_close + 1;
                        last = i;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    match out {
        Some(mut buf) => {
            buf.push_str(&src[last..]);
            std::borrow::Cow::Owned(buf)
        }
        None => color_wrapped,
    }
}

/// Returns `true` for CSS sizing properties supporting `calc-size()` and `stretch`.
#[must_use]
pub const fn is_sizing_property(prop: PropertyIdKind) -> bool {
    matches!(
        prop,
        PropertyIdKind::Width
            | PropertyIdKind::Height
            | PropertyIdKind::MinWidth
            | PropertyIdKind::MinHeight
            | PropertyIdKind::MaxWidth
            | PropertyIdKind::MaxHeight
            | PropertyIdKind::InlineSize
            | PropertyIdKind::BlockSize
            | PropertyIdKind::MinInlineSize
            | PropertyIdKind::MinBlockSize
            | PropertyIdKind::MaxInlineSize
            | PropertyIdKind::MaxBlockSize
            | PropertyIdKind::FlexBasis
    )
}

/// Returns `true` for CSS properties parsed natively by our CSS Values L4/L5 engine.
#[must_use]
pub fn is_values4_native_property(prop: PropertyIdKind) -> bool {
    matches!(
        prop,
        PropertyIdKind::Scale
            | PropertyIdKind::Rotate
            | PropertyIdKind::Translate
            | PropertyIdKind::ImageResolution
            | PropertyIdKind::BackgroundPosition
            | PropertyIdKind::BackgroundPositionX
            | PropertyIdKind::BackgroundPositionY
            | PropertyIdKind::ObjectPosition
            | PropertyIdKind::OffsetDistance
            | PropertyIdKind::ShapeMargin
            | PropertyIdKind::StrokeWidth
            | PropertyIdKind::StrokeDashoffset
            | PropertyIdKind::StrokeDasharray
            | PropertyIdKind::ScrollPaddingTop
            | PropertyIdKind::ScrollPaddingRight
            | PropertyIdKind::ScrollPaddingBottom
            | PropertyIdKind::ScrollPaddingLeft
            | PropertyIdKind::SvgCx
            | PropertyIdKind::SvgCy
            | PropertyIdKind::SvgRx
            | PropertyIdKind::SvgRy
            | PropertyIdKind::SvgX
            | PropertyIdKind::SvgY
            | PropertyIdKind::BorderImageSlice
            | PropertyIdKind::AspectRatio
            | PropertyIdKind::Orphans
            | PropertyIdKind::Widows
            | PropertyIdKind::TabSize
            | PropertyIdKind::ColumnCount
            | PropertyIdKind::InterpolateSize
            | PropertyIdKind::TransitionBehavior
            | PropertyIdKind::AnimationComposition
            | PropertyIdKind::TransitionProperty
            | PropertyIdKind::TransitionTimingFunction
            | PropertyIdKind::AnimationTimingFunction
            | PropertyIdKind::ViewTransitionName
            | PropertyIdKind::Overlay
            | PropertyIdKind::ReadingFlow
            | PropertyIdKind::ReadingOrder
            | PropertyIdKind::TextJustify
            | PropertyIdKind::TextAlignLast
            | PropertyIdKind::HyphenateCharacter
            | PropertyIdKind::HyphenateLimitLines
            | PropertyIdKind::Contain
            | PropertyIdKind::Display
    )
}

/// Returns `true` if `prop: raw` should be handled by `parse_values4_property` instead of `lightningcss`.
#[must_use]
pub fn should_use_values4_parser(prop: PropertyIdKind, raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() || GlobalKeyword::from_ident(&trimmed.to_ascii_lowercase()).is_some() {
        return false;
    }
    if is_values4_native_property(prop) {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    if prop == PropertyIdKind::Display
        && (lower.contains(' ')
            || matches!(
                lower.as_str(),
                "math" | "run-in" | "flow" | "ruby" | "ruby-base" | "ruby-text"
            ))
    {
        return true;
    }
    if lower.starts_with("fit-content(") {
        return false;
    }
    if prop == PropertyIdKind::BackgroundLayers
        && (lower.starts_with("image-set(")
            || lower.starts_with("-webkit-image-set(")
            || lower.starts_with("src(")
            || lower.starts_with("image("))
    {
        return true;
    }
    if prop == PropertyIdKind::FontStyle && lower.starts_with("oblique") && lower.contains('(') {
        return true;
    }
    if is_sizing_property(prop)
        && (lower.starts_with("calc-size(")
            || lower == "stretch"
            || (prop == PropertyIdKind::FlexBasis
                && matches!(
                    lower.as_str(),
                    "content" | "min-content" | "max-content" | "fit-content"
                )))
    {
        return true;
    }
    if prop == PropertyIdKind::Transition
        && (lower.contains("allow-discrete")
            || lower.contains("block-size")
            || lower.contains("inline-size")
            || lower.contains("interpolate-size")
            || lower.contains("overlay")
            || lower.contains("--"))
    {
        return true;
    }
    if prop == PropertyIdKind::Animation && lower.contains("--") {
        return true;
    }
    supports_values4_math(prop) && crate::values_math::has_values4_syntax(trimmed)
}

/// Returns `(ValueKind, non_negative)` for single-value CSS properties supporting
/// CSS Values L4/L5 typed math expressions.
#[must_use]
#[rustfmt::skip]
pub fn values4_property_kind(prop: PropertyIdKind) -> Option<(crate::values_math::ValueKind, bool)> {
    use crate::values_math::ValueKind as VK;
    use PropertyIdKind as P;
    match prop {
        P::Width | P::Height | P::MinWidth | P::MinHeight | P::MaxWidth | P::MaxHeight
        | P::InlineSize | P::BlockSize | P::MinInlineSize | P::MinBlockSize
        | P::MaxInlineSize | P::MaxBlockSize | P::PaddingTop | P::PaddingRight
        | P::PaddingBottom | P::PaddingLeft | P::PaddingBlockStart | P::PaddingBlockEnd
        | P::PaddingInlineStart | P::PaddingInlineEnd | P::FlexBasis | P::RowGap
        | P::ColumnGap | P::FontSize | P::BorderTopLeftRadius | P::BorderTopRightRadius
        | P::BorderBottomRightRadius | P::BorderBottomLeftRadius
        | P::BorderStartStartRadius | P::BorderStartEndRadius
        | P::BorderEndStartRadius | P::BorderEndEndRadius => Some((VK::LengthPercentage, true)),
        P::MarginTop | P::MarginRight | P::MarginBottom | P::MarginLeft
        | P::MarginBlockStart | P::MarginBlockEnd | P::MarginInlineStart | P::MarginInlineEnd
        | P::Top | P::Right | P::Bottom | P::Left | P::InsetBlockStart | P::InsetBlockEnd
        | P::InsetInlineStart | P::InsetInlineEnd | P::TextIndent | P::OutlineOffset
        | P::WordSpacing | P::LetterSpacing | P::OffsetDistance | P::BackgroundPositionX
        | P::BackgroundPositionY | P::StrokeDashoffset | P::SvgCx | P::SvgCy | P::SvgX | P::SvgY => Some((VK::LengthPercentage, false)),
        P::ShapeMargin | P::StrokeWidth | P::StrokeDasharray | P::ScrollPaddingTop
        | P::ScrollPaddingRight | P::ScrollPaddingBottom | P::ScrollPaddingLeft
        | P::SvgRx | P::SvgRy => Some((VK::LengthPercentage, true)),
        P::BorderImageSlice => Some((VK::NumberOrPercentage, true)),
        P::BorderTopWidth | P::BorderRightWidth | P::BorderBottomWidth | P::BorderLeftWidth
        | P::BorderBlockStartWidth | P::BorderBlockEndWidth | P::BorderInlineStartWidth
        | P::BorderInlineEndWidth | P::OutlineWidth => Some((VK::Length, true)),
        P::ZIndex | P::Order => Some((VK::Integer, false)),
        P::Orphans | P::Widows | P::ColumnCount => Some((VK::Integer, true)),
        P::FlexGrow | P::FlexShrink | P::FontWeight | P::AnimationIterationCount => Some((VK::Number, true)),
        P::Opacity => Some((VK::NumberOrPercentage, false)),
        P::TransitionDelay | P::AnimationDelay => Some((VK::Time, false)),
        P::TransitionDuration | P::AnimationDuration => Some((VK::Time, true)),
        _ => None,
    }
}

/// Returns `true` if `prop` should be routed through `parse_values4_property` when its value
/// contains CSS Values L4/L5 math functions or extended units.
#[must_use]
pub fn supports_values4_math(prop: PropertyIdKind) -> bool {
    is_values4_native_property(prop)
        || values4_property_kind(prop).is_some()
        || matches!(
            prop,
            PropertyIdKind::Transform
                | PropertyIdKind::GridTemplateRows
                | PropertyIdKind::GridTemplateColumns
                | PropertyIdKind::LineHeight
        )
}

/// Parses a CSS Values L4/L5 declaration value for `prop`, returning `(PropertyValue, specified_css)`.
#[must_use]
pub fn parse_values4_property(prop: PropertyIdKind, raw: &str) -> Option<(PropertyValue, String)> {
    use crate::values::CalcExpr;
    use crate::values_math::{
        parse_and_serialize_transform, parse_background_position, parse_grid_tracks_math,
        parse_image_resolution, parse_math_value, parse_math_value_from_parser, MathNode,
        RotateSpec, ScaleSpec, TranslateSpec, ValueKind,
    };
    let trimmed = raw.trim();
    match prop {
        PropertyIdKind::InterpolateSize => {
            let mode = match trimmed.to_ascii_lowercase().as_str() {
                "numeric-only" => InterpolateSizeMode::NumericOnly,
                "allow-keywords" => InterpolateSizeMode::AllowKeywords,
                _ => return None,
            };
            let s = match mode {
                InterpolateSizeMode::NumericOnly => "numeric-only",
                InterpolateSizeMode::AllowKeywords => "allow-keywords",
            };
            return Some((PropertyValue::InterpolateSize(mode), s.to_string()));
        }
        PropertyIdKind::TransitionBehavior => {
            let mut list = Vec::new();
            let mut texts = Vec::new();
            for part in trimmed.split(',') {
                let p = part.trim().to_ascii_lowercase();
                let m = match p.as_str() {
                    "normal" => TransitionBehaviorMode::Normal,
                    "allow-discrete" => TransitionBehaviorMode::AllowDiscrete,
                    _ => return None,
                };
                list.push(m);
                texts.push(p);
            }
            if list.is_empty() {
                return None;
            }
            return Some((
                PropertyValue::TransitionBehaviorList(list),
                texts.join(", "),
            ));
        }
        PropertyIdKind::AnimationComposition => {
            let mut list = Vec::new();
            let mut texts = Vec::new();
            for part in trimmed.split(',') {
                let p = part.trim().to_ascii_lowercase();
                let m = match p.as_str() {
                    "replace" => AnimationCompositionMode::Replace,
                    "add" => AnimationCompositionMode::Add,
                    "accumulate" => AnimationCompositionMode::Accumulate,
                    _ => return None,
                };
                list.push(m);
                texts.push(p);
            }
            if list.is_empty() {
                return None;
            }
            return Some((
                PropertyValue::AnimationCompositionList(list),
                texts.join(", "),
            ));
        }
        PropertyIdKind::TransitionProperty => {
            let mut names = Vec::new();
            let mut texts = Vec::new();
            let parts: Vec<&str> = trimmed.split(',').collect();
            for part in &parts {
                let p = part.trim();
                if p.is_empty() || p.chars().any(char::is_whitespace) {
                    return None;
                }
                let lower = p.to_ascii_lowercase();
                if GlobalKeyword::from_ident(&lower).is_some() {
                    return None;
                }
                if lower == "none" && parts.len() > 1 {
                    return None;
                }
                if !p.starts_with("--")
                    && !p
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return None;
                }
                let stored = if p.starts_with("--") {
                    p.to_string()
                } else {
                    lower
                };
                names.push(intern_str(&stored));
                texts.push(stored);
            }
            if names.is_empty() {
                return None;
            }
            return Some((PropertyValue::NameList(names), texts.join(", ")));
        }
        PropertyIdKind::TransitionTimingFunction | PropertyIdKind::AnimationTimingFunction => {
            let (fns, spec) = parse_timing_function_list(trimmed)?;
            return Some((PropertyValue::TimingList(fns), spec));
        }
        PropertyIdKind::Transition => {
            let (specs, spec) = parse_transition_shorthand(trimmed)?;
            return Some((PropertyValue::Transitions(specs), spec));
        }
        PropertyIdKind::Animation => {
            let (specs, spec) = parse_animation_shorthand(trimmed)?;
            return Some((PropertyValue::Animations(specs), spec));
        }
        p if is_sizing_property(p) && trimmed.to_ascii_lowercase().starts_with("calc-size(") => {
            let allow_auto = !matches!(
                p,
                PropertyIdKind::MaxWidth
                    | PropertyIdKind::MaxHeight
                    | PropertyIdKind::MaxInlineSize
                    | PropertyIdKind::MaxBlockSize
            );
            let allow_content = p == PropertyIdKind::FlexBasis;
            let (expr, spec) =
                crate::values_math::parse_calc_size(trimmed, allow_auto, allow_content)?;
            return Some((
                PropertyValue::Size(SizeValue::CalcSize(Box::new(expr))),
                spec,
            ));
        }
        p if is_sizing_property(p) && trimmed.eq_ignore_ascii_case("stretch") => {
            return Some((
                PropertyValue::Size(SizeValue::Stretch),
                "stretch".to_string(),
            ));
        }
        PropertyIdKind::FlexBasis
            if matches!(
                trimmed.to_ascii_lowercase().as_str(),
                "content" | "min-content" | "max-content" | "fit-content"
            ) =>
        {
            let lower = trimmed.to_ascii_lowercase();
            let sv = match lower.as_str() {
                "min-content" => SizeValue::MinContent,
                "max-content" => SizeValue::MaxContent,
                "fit-content" => SizeValue::FitContent(None),
                _ => SizeValue::Content,
            };
            return Some((PropertyValue::Size(sv), lower));
        }
        PropertyIdKind::ObjectPosition => {
            let (pos, s) = crate::values_math::PositionSpec::parse(trimmed)?;
            return Some((PropertyValue::ObjectPos(Box::new(pos)), s));
        }
        PropertyIdKind::Display => {
            let d = DisplayMode::parse_css(trimmed)?;
            return Some((PropertyValue::Display(d), d.to_css_string().to_string()));
        }
        PropertyIdKind::Overlay => {
            let mode = match trimmed.to_ascii_lowercase().as_str() {
                "none" => OverlayMode::None,
                "auto" => OverlayMode::Auto,
                _ => return None,
            };
            let s = match mode {
                OverlayMode::None => "none",
                OverlayMode::Auto => "auto",
            };
            return Some((PropertyValue::Overlay(mode), s.to_string()));
        }
        PropertyIdKind::ReadingFlow => {
            let mode = match trimmed.to_ascii_lowercase().as_str() {
                "normal" => ReadingFlowMode::Normal,
                "flex-visual" => ReadingFlowMode::FlexVisual,
                "flex-flow" => ReadingFlowMode::FlexFlow,
                "grid-rows" => ReadingFlowMode::GridRows,
                "grid-columns" => ReadingFlowMode::GridColumns,
                "grid-order" => ReadingFlowMode::GridOrder,
                "source-order" => ReadingFlowMode::SourceOrder,
                _ => return None,
            };
            return Some((
                PropertyValue::ReadingFlow(mode),
                trimmed.to_ascii_lowercase(),
            ));
        }
        PropertyIdKind::ReadingOrder => {
            let pm = parse_math_value(trimmed, ValueKind::Integer)?;
            let s = pm.node.serialize_specified(pm.was_math_fn);
            return Some((PropertyValue::MathVal(Box::new(pm.node)), s));
        }
        PropertyIdKind::TextJustify => {
            let mode = match trimmed.to_ascii_lowercase().as_str() {
                "auto" => TextJustifyMode::Auto,
                "none" => TextJustifyMode::None,
                "inter-word" => TextJustifyMode::InterWord,
                "inter-character" => TextJustifyMode::InterCharacter,
                _ => return None,
            };
            return Some((
                PropertyValue::TextJustify(mode),
                trimmed.to_ascii_lowercase(),
            ));
        }
        PropertyIdKind::TextAlignLast => {
            let mode = match trimmed.to_ascii_lowercase().as_str() {
                "auto" => TextAlignLastMode::Auto,
                "start" => TextAlignLastMode::Start,
                "end" => TextAlignLastMode::End,
                "left" => TextAlignLastMode::Left,
                "right" => TextAlignLastMode::Right,
                "center" => TextAlignLastMode::Center,
                "justify" => TextAlignLastMode::Justify,
                "match-parent" => TextAlignLastMode::MatchParent,
                _ => return None,
            };
            return Some((
                PropertyValue::TextAlignLast(mode),
                trimmed.to_ascii_lowercase(),
            ));
        }
        PropertyIdKind::HyphenateCharacter => {
            if trimmed.eq_ignore_ascii_case("auto") {
                return Some((PropertyValue::HyphenateChar(None), "auto".to_string()));
            }
            let mut pi = cssparser::ParserInput::new(trimmed);
            let mut p = cssparser::Parser::new(&mut pi);
            let s = p.expect_string().ok()?.as_ref().to_string();
            if !p.is_exhausted() {
                return None;
            }
            let quoted = crate::substitution::quote_css_string(&s);
            return Some((PropertyValue::HyphenateChar(Some(intern_str(&s))), quoted));
        }
        PropertyIdKind::HyphenateLimitLines => {
            if trimmed.eq_ignore_ascii_case("no-limit") {
                return Some((
                    PropertyValue::HyphenateLimitLines(None),
                    "no-limit".to_string(),
                ));
            }
            let pm = parse_math_value(trimmed, ValueKind::Integer)?;
            let v = pm
                .node
                .eval_scalar(&crate::values::EvalContext::default(), None);
            if !v.is_finite() || (!pm.was_math_fn && v < 1.0) {
                return None;
            }
            let n = (v + 0.5).floor().max(1.0) as i32;
            let s = pm.node.serialize_specified(pm.was_math_fn);
            return Some((PropertyValue::HyphenateLimitLines(Some(n)), s));
        }
        PropertyIdKind::Contain => {
            let lower = trimmed.to_ascii_lowercase();
            let toks: Vec<&str> = lower.split_ascii_whitespace().collect();
            if toks.is_empty() {
                return None;
            }
            if toks.len() == 1 {
                match toks[0] {
                    "none" => return Some((PropertyValue::Contain(0), "none".to_string())),
                    "strict" => return Some((PropertyValue::Contain(0x1D), "strict".to_string())),
                    "content" => {
                        return Some((PropertyValue::Contain(0x1C), "content".to_string()))
                    }
                    _ => {}
                }
            }
            let mut bits = 0u8;
            for t in toks {
                let bit = match t {
                    "size" => 0x01,
                    "inline-size" => 0x02,
                    "layout" => 0x04,
                    "style" => 0x08,
                    "paint" => 0x10,
                    _ => return None,
                };
                if bits & bit != 0 {
                    return None;
                }
                bits |= bit;
            }
            if bits & 0x03 == 0x03 {
                return None;
            }
            let mut parts = Vec::new();
            if bits & 0x01 != 0 {
                parts.push("size");
            }
            if bits & 0x02 != 0 {
                parts.push("inline-size");
            }
            if bits & 0x04 != 0 {
                parts.push("layout");
            }
            if bits & 0x08 != 0 {
                parts.push("style");
            }
            if bits & 0x10 != 0 {
                parts.push("paint");
            }
            return Some((PropertyValue::Contain(bits), parts.join(" ")));
        }
        PropertyIdKind::ViewTransitionName => {
            if trimmed.eq_ignore_ascii_case("none") {
                return Some((PropertyValue::ViewTransitionName(None), "none".to_string()));
            }
            let mut pi = cssparser::ParserInput::new(trimmed);
            let mut p = cssparser::Parser::new(&mut pi);
            let id = p.expect_ident().ok()?.as_ref().to_string();
            if !p.is_exhausted() || GlobalKeyword::from_ident(&id).is_some() {
                return None;
            }
            let esc = crate::substitution::escape_css_ident(&id);
            return Some((
                PropertyValue::ViewTransitionName(Some(intern_str(&esc))),
                esc,
            ));
        }
        PropertyIdKind::BackgroundLayers => {
            let l = trimmed.to_ascii_lowercase();
            if l.starts_with("image-set(")
                || l.starts_with("-webkit-image-set(")
                || l.starts_with("src(")
                || l.starts_with("image(")
            {
                let _ = crate::substitution::parse_and_serialize_bg_image_fn(
                    trimmed,
                    &crate::values::EvalContext::default(),
                )?;
                return Some((
                    PropertyValue::BackgroundImageRaw(trimmed.to_string()),
                    trimmed.to_string(),
                ));
            }
            return None;
        }
        PropertyIdKind::FontStyle => {
            let l = trimmed.to_ascii_lowercase();
            let rest = l.strip_prefix("oblique")?.trim();
            if rest.is_empty() {
                return None;
            }
            let rest_orig = trimmed[trimmed.len() - rest.len()..].trim();
            let mut pm = parse_math_value(rest_orig, ValueKind::Angle)?;
            let mut rc = 0u32;
            pm.node.bind_random_property("font-style", &mut rc);
            let spec = pm.node.serialize_specified(pm.was_math_fn);
            return Some((
                PropertyValue::FontStyleObliqueMath(Box::new(pm.node)),
                format!("oblique {spec}"),
            ));
        }
        PropertyIdKind::AspectRatio => {
            if trimmed.eq_ignore_ascii_case("auto") {
                return Some((PropertyValue::AspectRatioMath(None), "auto".to_string()));
            }
            let mut pi = cssparser::ParserInput::new(trimmed);
            let mut p = cssparser::Parser::new(&mut pi);
            let mut rc = 0u32;
            let mut a = parse_math_value_from_parser(&mut p, ValueKind::Number)?;
            a.node.bind_random_property("aspect-ratio", &mut rc);
            let sa = a.node.serialize_specified(a.was_math_fn);
            if p.is_exhausted() {
                return Some((
                    PropertyValue::AspectRatioMath(Some((Box::new(a.node), None))),
                    sa,
                ));
            }
            if p.expect_delim('/').is_err() {
                return None;
            }
            let mut b = parse_math_value_from_parser(&mut p, ValueKind::Number)?;
            if !p.is_exhausted() {
                return None;
            }
            b.node.bind_random_property("aspect-ratio", &mut rc);
            let sb = b.node.serialize_specified(b.was_math_fn);
            return Some((
                PropertyValue::AspectRatioMath(Some((Box::new(a.node), Some(Box::new(b.node))))),
                format!("{sa} / {sb}"),
            ));
        }
        PropertyIdKind::Scale => {
            let (spec, s) = ScaleSpec::parse(trimmed)?;
            return Some((PropertyValue::Scale(spec), s));
        }
        PropertyIdKind::Rotate => {
            let (spec, s) = RotateSpec::parse(trimmed)?;
            return Some((PropertyValue::Rotate(spec), s));
        }
        PropertyIdKind::Translate => {
            let (spec, s) = TranslateSpec::parse(trimmed)?;
            return Some((PropertyValue::Translate(spec), s));
        }
        PropertyIdKind::ImageResolution => {
            let (node, s) = parse_image_resolution(trimmed)?;
            return Some((PropertyValue::MathVal(Box::new(node)), s));
        }
        PropertyIdKind::BackgroundPosition => {
            let (x, y, s) = parse_background_position(trimmed)?;
            return Some((PropertyValue::BgPosition(x, y), s));
        }
        PropertyIdKind::Transform => {
            let s = parse_and_serialize_transform(trimmed, None)?;
            return Some((PropertyValue::TransformRaw(trimmed.to_string()), s));
        }
        PropertyIdKind::GridTemplateRows | PropertyIdKind::GridTemplateColumns => {
            let (tracks, s) = parse_grid_tracks_math(trimmed)?;
            return Some((PropertyValue::GridTracks(tracks), s));
        }
        PropertyIdKind::ColumnCount if trimmed.eq_ignore_ascii_case("auto") => {
            return Some((PropertyValue::ZIndex(ZIndexValue::Auto), "auto".to_string()));
        }
        PropertyIdKind::TabSize => {
            let pm = parse_math_value(trimmed, ValueKind::Number)
                .or_else(|| parse_math_value(trimmed, ValueKind::Length))?;
            if !pm.was_math_fn {
                if let MathNode::Number(v) | MathNode::Dimension(v, _) = pm.node {
                    if v < 0.0 {
                        return None;
                    }
                }
            }
            let s = pm.node.serialize_specified(pm.was_math_fn);
            return Some((PropertyValue::MathVal(Box::new(pm.node)), s));
        }
        PropertyIdKind::LineHeight => {
            let pm = parse_math_value(trimmed, ValueKind::LengthPercentage)
                .or_else(|| parse_math_value(trimmed, ValueKind::Number))
                .or_else(|| {
                    // Typed arithmetic on `line-height`: e.g. `calc(10% / 1px)` resolves `%`
                    // as Length (relative to font-size) to yield a dimensionless `<number>`.
                    let mut pi = cssparser::ParserInput::new(trimmed);
                    let mut p = cssparser::Parser::new(&mut pi);
                    let node = parse_math_value_from_parser(&mut p, ValueKind::NumberOrPercentage);
                    if node.is_some() && p.is_exhausted() {
                        return node;
                    }
                    let mut pi2 = cssparser::ParserInput::new(trimmed);
                    let mut p2 = cssparser::Parser::new(&mut pi2);
                    let tok = p2.next().ok()?.clone();
                    let cssparser::Token::Function(fname) = tok else {
                        return None;
                    };
                    if !crate::values_math::is_css_math_fn(fname.as_ref()) {
                        return None;
                    }
                    let lower = fname.to_ascii_lowercase();
                    let n = crate::values_math::parse_math_fn_hinted(&lower, &mut p2)?;
                    let ct = n
                        .calc_type()?
                        .apply_hint(crate::values_math::BaseDim::Length)?;
                    (p2.is_exhausted() && ct.dims == [0; 6] && ct.percent == 0).then_some(
                        crate::values_math::ParsedMath {
                            node: n,
                            calc_type: ct,
                            was_math_fn: true,
                        },
                    )
                })?;
            if !pm.was_math_fn {
                match pm.node {
                    MathNode::Number(v) | MathNode::Percent(v) | MathNode::Dimension(v, _)
                        if v < 0.0 =>
                    {
                        return None;
                    }
                    _ => {}
                }
            }
            let s = pm.node.serialize_specified(pm.was_math_fn);
            return Some((PropertyValue::MathVal(Box::new(pm.node)), s));
        }
        PropertyIdKind::BorderTopLeftRadius
        | PropertyIdKind::BorderTopRightRadius
        | PropertyIdKind::BorderBottomRightRadius
        | PropertyIdKind::BorderBottomLeftRadius
        | PropertyIdKind::BorderStartStartRadius
        | PropertyIdKind::BorderStartEndRadius
        | PropertyIdKind::BorderEndStartRadius
        | PropertyIdKind::BorderEndEndRadius => {
            let mut pi = cssparser::ParserInput::new(trimmed);
            let mut p = cssparser::Parser::new(&mut pi);
            let px = parse_math_value_from_parser(&mut p, ValueKind::LengthPercentage)?;
            if !px.was_math_fn {
                if let MathNode::Dimension(v, _) | MathNode::Percent(v) = px.node {
                    if v < 0.0 {
                        return None;
                    }
                }
            }
            let xs = px.node.serialize_specified(px.was_math_fn);
            let lx = LengthPercentage::Calc(Box::new(CalcExpr::Math(Box::new(px.node))));
            if p.is_exhausted() {
                return Some((
                    PropertyValue::CornerRadius(CornerRadius {
                        x: lx.clone(),
                        y: lx,
                    }),
                    xs,
                ));
            }
            let py = parse_math_value_from_parser(&mut p, ValueKind::LengthPercentage)?;
            if !p.is_exhausted() {
                return None;
            }
            if !py.was_math_fn {
                if let MathNode::Dimension(v, _) | MathNode::Percent(v) = py.node {
                    if v < 0.0 {
                        return None;
                    }
                }
            }
            let ys = py.node.serialize_specified(py.was_math_fn);
            let ly = LengthPercentage::Calc(Box::new(CalcExpr::Math(Box::new(py.node))));
            let spec = if xs == ys { xs } else { format!("{xs} {ys}") };
            return Some((
                PropertyValue::CornerRadius(CornerRadius { x: lx, y: ly }),
                spec,
            ));
        }
        _ => {}
    }

    let (kind, nonneg) = values4_property_kind(prop)?;
    let mut pm = parse_math_value(trimmed, kind)?;
    if nonneg && !pm.was_math_fn {
        let min_v = if matches!(
            prop,
            PropertyIdKind::FontWeight
                | PropertyIdKind::Orphans
                | PropertyIdKind::Widows
                | PropertyIdKind::ColumnCount
        ) {
            1.0
        } else {
            0.0
        };
        match pm.node {
            MathNode::Number(v) | MathNode::Percent(v) | MathNode::Dimension(v, _) if v < min_v => {
                return None;
            }
            _ => {}
        }
    }
    let mut rand_counter = 0u32;
    pm.node
        .bind_random_property(prop.css_name(), &mut rand_counter);
    let spec = pm.node.serialize_specified(pm.was_math_fn);
    let lp = || LengthPercentage::Calc(Box::new(CalcExpr::Math(Box::new(pm.node.clone()))));
    let val = match prop {
        PropertyIdKind::Width
        | PropertyIdKind::Height
        | PropertyIdKind::MinWidth
        | PropertyIdKind::MinHeight
        | PropertyIdKind::MaxWidth
        | PropertyIdKind::MaxHeight
        | PropertyIdKind::InlineSize
        | PropertyIdKind::BlockSize
        | PropertyIdKind::MinInlineSize
        | PropertyIdKind::MinBlockSize
        | PropertyIdKind::MaxInlineSize
        | PropertyIdKind::MaxBlockSize => PropertyValue::Size(SizeValue::Length(lp())),
        PropertyIdKind::MarginTop
        | PropertyIdKind::MarginRight
        | PropertyIdKind::MarginBottom
        | PropertyIdKind::MarginLeft
        | PropertyIdKind::MarginBlockStart
        | PropertyIdKind::MarginBlockEnd
        | PropertyIdKind::MarginInlineStart
        | PropertyIdKind::MarginInlineEnd
        | PropertyIdKind::Top
        | PropertyIdKind::Right
        | PropertyIdKind::Bottom
        | PropertyIdKind::Left
        | PropertyIdKind::InsetBlockStart
        | PropertyIdKind::InsetBlockEnd
        | PropertyIdKind::InsetInlineStart
        | PropertyIdKind::InsetInlineEnd
        | PropertyIdKind::FlexBasis => PropertyValue::LengthAuto(LengthPercentageAuto::Value(lp())),
        PropertyIdKind::PaddingTop
        | PropertyIdKind::PaddingRight
        | PropertyIdKind::PaddingBottom
        | PropertyIdKind::PaddingLeft
        | PropertyIdKind::PaddingBlockStart
        | PropertyIdKind::PaddingBlockEnd
        | PropertyIdKind::PaddingInlineStart
        | PropertyIdKind::PaddingInlineEnd
        | PropertyIdKind::RowGap
        | PropertyIdKind::ColumnGap
        | PropertyIdKind::TextIndent
        | PropertyIdKind::LetterSpacing
        | PropertyIdKind::WordSpacing
        | PropertyIdKind::OutlineOffset => PropertyValue::Length(lp()),
        _ => PropertyValue::MathVal(Box::new(pm.node)),
    };
    Some((val, spec))
}

/// Parses a single CSS Easing Level 1/2 `<easing-function>` (`linear`, `ease*`, `step-*`, `steps()`, `cubic-bezier()`).
#[must_use]
#[rustfmt::skip]
pub fn parse_single_timing_function(input: &str) -> Option<(TimingFunction, String)> {
    let mut pi = cssparser::ParserInput::new(input);
    let mut p = cssparser::Parser::new(&mut pi);
    let tf = parse_timing_fn_from_parser(&mut p)?;
    p.is_exhausted().then_some(tf)
}

#[rustfmt::skip]
fn parse_timing_fn_from_parser<'i, 't>(p: &mut cssparser::Parser<'i, 't>) -> Option<(TimingFunction, String)> {
    let st = p.state();
    match p.next().ok()?.clone() {
        cssparser::Token::Ident(id) => match id.to_ascii_lowercase().as_str() {
            "linear" => Some((TimingFunction::Linear, "linear".to_string())),
            "ease" => Some((TimingFunction::Ease, "ease".to_string())),
            "ease-in" => Some((TimingFunction::EaseIn, "ease-in".to_string())),
            "ease-out" => Some((TimingFunction::EaseOut, "ease-out".to_string())),
            "ease-in-out" => Some((TimingFunction::EaseInOut, "ease-in-out".to_string())),
            "step-start" => Some((TimingFunction::Steps(1, true), "steps(1, start)".to_string())),
            "step-end" => Some((TimingFunction::Steps(1, false), "steps(1, end)".to_string())),
            _ => { p.reset(&st); None }
        },
        cssparser::Token::Function(ref fname) if fname.eq_ignore_ascii_case("cubic-bezier") => {
            let res = p.parse_nested_block(|n| -> Result<(TimingFunction, String), cssparser::ParseError<'i, ()>> {
                let next_f64 = |n: &mut cssparser::Parser<'i, '_>| -> Result<f64, cssparser::ParseError<'i, ()>> {
                    let s = n.position();
                    let fb = n.expect_number()?;
                    Ok(n.slice_from(s).trim().parse::<f64>().unwrap_or(f64::from(fb)))
                };
                let x1 = next_f64(n)?; n.expect_comma()?;
                let y1 = next_f64(n)?; n.expect_comma()?;
                let x2 = next_f64(n)?; n.expect_comma()?;
                let y2 = next_f64(n)?;
                if !n.is_exhausted() || !(0.0..=1.0).contains(&x1) || !(0.0..=1.0).contains(&x2)
                    || !y1.is_finite() || !y2.is_finite() {
                    return Err(n.new_custom_error(()));
                }
                let fmt = |v: f64| if v == 0.0 { "0".to_string() } else { format!("{v}") };
                Ok((TimingFunction::CubicBezier(x1, y1, x2, y2), format!("cubic-bezier({}, {}, {}, {})", fmt(x1), fmt(y1), fmt(x2), fmt(y2))))
            }).ok();
            if res.is_none() { p.reset(&st); }
            res
        }
        cssparser::Token::Function(ref fname) if fname.eq_ignore_ascii_case("steps") => {
            let res = p.parse_nested_block(|n| -> Result<(TimingFunction, String), cssparser::ParseError<'i, ()>> {
                let cnt = n.expect_integer()?;
                if cnt < 1 { return Err(n.new_custom_error(())); }
                let (jump_start, pos_s) = if n.try_parse(cssparser::Parser::expect_comma).is_ok() {
                    let pos = n.expect_ident()?.to_ascii_lowercase();
                    match pos.as_str() {
                        "start" | "jump-start" | "jump-both" => (true, pos),
                        "end" | "jump-end" | "jump-none" => (false, pos),
                        _ => return Err(n.new_custom_error(())),
                    }
                } else { (false, "end".to_string()) };
                if !n.is_exhausted() { return Err(n.new_custom_error(())); }
                let spec = if pos_s == "end" || pos_s == "jump-end" { format!("steps({cnt})") } else { format!("steps({cnt}, {pos_s})") };
                Ok((TimingFunction::Steps(cnt as u32, jump_start), spec))
            }).ok();
            if res.is_none() { p.reset(&st); }
            res
        }
        _ => { p.reset(&st); None }
    }
}

fn parse_timing_function_list(input: &str) -> Option<(Vec<TimingFunction>, String)> {
    let mut pi = cssparser::ParserInput::new(input);
    let mut p = cssparser::Parser::new(&mut pi);
    let mut fns = Vec::new();
    let mut texts = Vec::new();
    loop {
        let (tf, s) = parse_timing_fn_from_parser(&mut p)?;
        fns.push(tf);
        texts.push(s);
        if p.try_parse(cssparser::Parser::expect_comma).is_err() {
            break;
        }
    }
    (!fns.is_empty() && p.is_exhausted()).then(|| (fns, texts.join(", ")))
}

#[rustfmt::skip]
fn parse_time_ms_from_parser<'i, 't>(p: &mut cssparser::Parser<'i, 't>) -> Option<f32> {
    let st = p.state();
    match p.next().ok()?.clone() {
        cssparser::Token::Dimension { value, unit, .. } => {
            match unit.to_ascii_lowercase().as_str() {
                "s" => Some(value * 1000.0),
                "ms" => Some(value),
                _ => { p.reset(&st); None }
            }
        }
        _ => { p.reset(&st); None }
    }
}

#[rustfmt::skip]
fn parse_transition_shorthand(input: &str) -> Option<(Vec<TransitionSpec>, String)> {
    let mut pi = cssparser::ParserInput::new(input);
    let mut p = cssparser::Parser::new(&mut pi);
    let mut out = Vec::new();
    loop {
        let (mut prop, mut dur, mut del, mut timing, mut behavior) = (None, None, None, None, None);
        let mut any = false;
        while !p.is_exhausted() {
            let st = p.state();
            if matches!(p.next(), Ok(cssparser::Token::Comma)) { p.reset(&st); break; }
            p.reset(&st);
            if let Some(ms) = parse_time_ms_from_parser(&mut p) {
                if dur.is_none() { if ms < 0.0 { return None; } dur = Some(ms); }
                else if del.is_none() { del = Some(ms); }
                else { return None; }
                any = true; continue;
            }
            if timing.is_none() {
                if let Some((tf, _)) = parse_timing_fn_from_parser(&mut p) { timing = Some(tf); any = true; continue; }
            }
            if let Ok(ident) = p.try_parse(|p| p.expect_ident().map(|s| s.to_string())) {
                let lower = ident.to_ascii_lowercase();
                if behavior.is_none() && (lower == "normal" || lower == "allow-discrete") {
                    behavior = Some(if lower == "allow-discrete" { TransitionBehaviorMode::AllowDiscrete } else { TransitionBehaviorMode::Normal });
                    any = true; continue;
                }
                if prop.is_none() && GlobalKeyword::from_ident(&lower).is_none() {
                    prop = Some(intern_str(if ident.starts_with("--") { &ident } else { &lower }));
                    any = true; continue;
                }
                return None;
            }
            return None;
        }
        if !any { return None; }
        out.push(TransitionSpec {
            property: prop.unwrap_or_else(|| intern_str("all")),
            duration_ms: dur.unwrap_or(0.0),
            delay_ms: del.unwrap_or(0.0),
            timing: timing.unwrap_or(TimingFunction::Ease),
            behavior: behavior.unwrap_or(TransitionBehaviorMode::Normal),
        });
        if p.try_parse(cssparser::Parser::expect_comma).is_err() { break; }
    }
    (!out.is_empty() && p.is_exhausted()).then(|| (out, input.trim().to_string()))
}

#[rustfmt::skip]
fn parse_animation_shorthand(input: &str) -> Option<(Vec<AnimationSpec>, String)> {
    let mut pi = cssparser::ParserInput::new(input);
    let mut p = cssparser::Parser::new(&mut pi);
    let mut out = Vec::new();
    loop {
        let (mut name, mut dur, mut del, mut timing, mut iter, mut dir, mut fill, mut state, mut comp) =
            (None, None, None, None, None, None, None, None, None);
        let mut any = false;
        while !p.is_exhausted() {
            let st = p.state();
            if matches!(p.next(), Ok(cssparser::Token::Comma)) { p.reset(&st); break; }
            p.reset(&st);
            if let Some(ms) = parse_time_ms_from_parser(&mut p) {
                if dur.is_none() { if ms < 0.0 { return None; } dur = Some(ms); }
                else if del.is_none() { del = Some(ms); }
                else { return None; }
                any = true; continue;
            }
            if timing.is_none() {
                if let Some((tf, _)) = parse_timing_fn_from_parser(&mut p) { timing = Some(tf); any = true; continue; }
            }
            if iter.is_none() {
                if let Ok(n) = p.try_parse(cssparser::Parser::expect_number) {
                    if n < 0.0 { return None; }
                    iter = Some(n); any = true; continue;
                }
            }
            if let Ok(ident) = p.try_parse(|p| p.expect_ident().map(|s| s.to_string())) {
                let lower = ident.to_ascii_lowercase();
                if iter.is_none() && lower == "infinite" { iter = Some(f32::INFINITY); any = true; continue; }
                if dir.is_none() {
                    let d = match lower.as_str() {
                        "normal" => Some(AnimationDirectionMode::Normal),
                        "reverse" => Some(AnimationDirectionMode::Reverse),
                        "alternate" => Some(AnimationDirectionMode::Alternate),
                        "alternate-reverse" => Some(AnimationDirectionMode::AlternateReverse),
                        _ => None,
                    };
                    if let Some(d) = d { dir = Some(d); any = true; continue; }
                }
                if fill.is_none() {
                    let f = match lower.as_str() {
                        "forwards" => Some(AnimationFillModeKind::Forwards),
                        "backwards" => Some(AnimationFillModeKind::Backwards),
                        "both" => Some(AnimationFillModeKind::Both),
                        _ => None,
                    };
                    if let Some(f) = f { fill = Some(f); any = true; continue; }
                }
                if state.is_none() && (lower == "running" || lower == "paused") {
                    state = Some(if lower == "paused" { AnimationPlayStateMode::Paused } else { AnimationPlayStateMode::Running });
                    any = true; continue;
                }
                if comp.is_none() && matches!(lower.as_str(), "replace" | "add" | "accumulate") {
                    comp = Some(match lower.as_str() { "add" => AnimationCompositionMode::Add, "accumulate" => AnimationCompositionMode::Accumulate, _ => AnimationCompositionMode::Replace });
                    any = true; continue;
                }
                if name.is_none() { name = Some(intern_str(&ident)); any = true; continue; }
                return None;
            }
            return None;
        }
        if !any { return None; }
        out.push(AnimationSpec {
            name: name.unwrap_or_else(|| intern_str("none")),
            duration_ms: dur.unwrap_or(0.0),
            delay_ms: del.unwrap_or(0.0),
            timing: timing.unwrap_or(TimingFunction::Ease),
            iteration_count: iter.unwrap_or(1.0),
            direction: dir.unwrap_or(AnimationDirectionMode::Normal),
            fill_mode: fill.unwrap_or(AnimationFillModeKind::None),
            play_state: state.unwrap_or(AnimationPlayStateMode::Running),
            composition: comp.unwrap_or(AnimationCompositionMode::Replace),
        });
        if p.try_parse(cssparser::Parser::expect_comma).is_err() { break; }
    }
    (!out.is_empty() && p.is_exhausted()).then(|| (out, input.trim().to_string()))
}

/// Wraps CSS Color 4/5 functional & system-color longhand declarations into an opaque
/// quoted sentinel `var(--__rix_color4__, "...")` and CSS Values L4/L5 math/unit/transform
/// declarations into `var(--__rix_val4__, "...")` so `lightningcss` never drops or corrupts them.
fn wrap_color4_declarations(css: &str) -> std::borrow::Cow<'_, str> {
    if !css.contains(':') {
        return std::borrow::Cow::Borrowed(css);
    }
    let bytes = css.as_bytes();
    let mut out: Option<String> = None;
    let (mut last, mut i, mut paren_depth, mut in_str) = (0usize, 0usize, 0usize, None::<u8>);
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            i = if b == b'\\' {
                (i + 2).min(bytes.len())
            } else {
                i + 1
            };
            if b == q {
                in_str = None;
            }
            continue;
        }
        if b == b'\\' {
            i = (i + 2).min(bytes.len());
            continue;
        }
        if b == b'"' || b == b'\'' {
            in_str = Some(b);
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        if b == b'(' {
            paren_depth += 1;
            i += 1;
            continue;
        }
        if b == b')' {
            paren_depth = paren_depth.saturating_sub(1);
            i += 1;
            continue;
        }
        if b == b':' && paren_depth == 0 {
            let mut p = i;
            while p > 0 && bytes[p - 1].is_ascii_whitespace() {
                p -= 1;
            }
            let name_end = p;
            while p > 0 && (bytes[p - 1].is_ascii_alphanumeric() || bytes[p - 1] == b'-') {
                p -= 1;
            }
            let prop_name = &css[p..name_end];
            let prop_kind = PropertyIdKind::from_name(&prop_name.to_ascii_lowercase());
            let is_std_prop = prop_kind.is_some_and(|pk| pk != PropertyIdKind::Custom);
            let is_color_prop = prop_kind.is_some_and(crate::cssom_decl::is_color_longhand);
            let is_val4_prop = prop_kind.is_some_and(|pk| {
                supports_values4_math(pk)
                    || is_values4_native_property(pk)
                    || matches!(
                        pk,
                        PropertyIdKind::Transition
                            | PropertyIdKind::Animation
                            | PropertyIdKind::BackgroundLayers
                            | PropertyIdKind::FontStyle
                    )
            });
            if is_std_prop {
                // Scan value to top-level `;`, `}`, `!`, or EOF (abort if `{` -> selector).
                let val_start = i + 1;
                let mut j = val_start;
                let mut vp = 0usize;
                let mut vs: Option<u8> = None;
                let mut saw_brace = false;
                while j < bytes.len() {
                    let c = bytes[j];
                    if let Some(q) = vs {
                        j = if c == b'\\' {
                            (j + 2).min(bytes.len())
                        } else {
                            j + 1
                        };
                        if c == q {
                            vs = None;
                        }
                        continue;
                    }
                    if c == b'\\' {
                        j = (j + 2).min(bytes.len());
                        continue;
                    }
                    if c == b'"' || c == b'\'' {
                        vs = Some(c);
                        j += 1;
                        continue;
                    }
                    if c == b'/' && j + 1 < bytes.len() && bytes[j + 1] == b'*' {
                        j += 2;
                        while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                            j += 1;
                        }
                        j = (j + 2).min(bytes.len());
                        continue;
                    }
                    match c {
                        b'(' => vp += 1,
                        b')' => {
                            if vp == 0 {
                                saw_brace = true;
                                break;
                            }
                            vp -= 1;
                        }
                        b'{' if vp == 0 => {
                            saw_brace = true;
                            break;
                        }
                        b';' | b'}' | b'!' if vp == 0 => break,
                        _ => {}
                    }
                    j += 1;
                }
                if !saw_brace && (vp == 0 || j == bytes.len()) && vs.is_none() {
                    let raw = css[val_start..j].trim();
                    let sentinel = if !raw.is_empty()
                        && !raw.contains("--__rix_")
                        && crate::substitution::contains_values5_subst_fn_str(raw)
                    {
                        Some("--__rix_subst__")
                    } else if !raw.is_empty() && !crate::cssom_decl::contains_var_or_env_ci(raw) {
                        if is_color_prop
                            && (raw.contains('(')
                                || crate::color::SystemColor::from_ident(raw).is_some())
                        {
                            Some("--__rix_color4__")
                        } else if is_val4_prop
                            && prop_kind.is_some_and(|pk| should_use_values4_parser(pk, raw))
                        {
                            Some("--__rix_val4__")
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some(sent_var) = sentinel {
                        let buf = out.get_or_insert_with(|| String::with_capacity(css.len() + 32));
                        buf.push_str(&css[last..val_start]);
                        buf.push_str(" var(");
                        buf.push_str(sent_var);
                        buf.push_str(", \"");
                        for ch in raw.chars() {
                            match ch {
                                '\\' => buf.push_str("\\\\"),
                                '"' => buf.push_str("\\\""),
                                '\n' | '\r' => buf.push(' '),
                                _ => buf.push(ch),
                            }
                        }
                        for _ in 0..vp {
                            buf.push(')');
                        }
                        buf.push_str("\")");
                        last = j;
                        i = j;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    match out {
        Some(mut buf) => {
            buf.push_str(&css[last..]);
            std::borrow::Cow::Owned(buf)
        }
        None => std::borrow::Cow::Borrowed(css),
    }
}

/// Restores internal `var(--__rix_revert_rule__)` and `var(--__rix_color4__, "...")`
/// sentinels back to canonical CSSOM text.
#[must_use]
pub(crate) fn restore_revert_rule_sentinel(text: &str) -> String {
    let mut s = if text.contains("--__rix_revert_rule__") {
        text.replace("var(--__rix_revert_rule__)", "revert-rule")
    } else {
        text.to_string()
    };
    for (tag, is_color) in [
        ("var(--__rix_color4__", true),
        ("var(--__rix_val4__", false),
        ("var(--__rix_subst__", false),
    ] {
        if let Some(pos) = s.find(tag) {
            if let Some(q1) = s[pos..].find('"') {
                let start = pos + q1 + 1;
                let bytes = s.as_bytes();
                let mut k = start;
                let mut unescaped = String::new();
                while k < bytes.len() {
                    if bytes[k] == b'\\' && k + 1 < bytes.len() {
                        unescaped.push(bytes[k + 1] as char);
                        k += 2;
                        continue;
                    }
                    if bytes[k] == b'"' {
                        break;
                    }
                    let ch = s[k..].chars().next().unwrap();
                    unescaped.push(ch);
                    k += ch.len_utf8();
                }
                if let Some(end_paren) = s[k..].find(')') {
                    let replacement = if is_color {
                        crate::color::parse_and_serialize_color_specified(&unescaped)
                            .unwrap_or_default()
                    } else {
                        unescaped
                    };
                    if replacement.is_empty() {
                        return String::new();
                    }
                    let mut rebuilt = String::with_capacity(s.len());
                    rebuilt.push_str(&s[..pos]);
                    rebuilt.push_str(&replacement);
                    rebuilt.push_str(&s[k + end_paren + 1..]);
                    s = rebuilt;
                }
            }
        }
    }
    s
}

/// Parses an HTML `style="..."` attribute into typed [`SpecifiedDeclaration`]s.
#[must_use]
pub fn parse_inline_style(
    style_attr: &str,
    missing: &mut Vec<MissingCssFeature>,
) -> Vec<SpecifiedDeclaration> {
    let rewritten = rewrite_dynamic_var_names(style_attr);
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let opts = ParserOptions {
            error_recovery: true,
            ..ParserOptions::default()
        };
        let Ok(block) = DeclarationBlock::parse_string(&rewritten, opts) else {
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
                return prop
                    .shorthand_longhands()
                    .into_iter()
                    .map(|target| SpecifiedDeclaration {
                        property: target,
                        custom_name: None,
                        value: PropertyValue::Global(kw),
                        important,
                        order: 0,
                    })
                    .collect();
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
    write_tokens_for_reparse(tokens, &mut buf);
    parse_property_from_str(prop, &buf, important)
}

/// Parses a substituted CSS declaration value string for `prop` directly without a lossy
/// intermediate token round-trip (preserving wide-gamut colors, `image-set()`, and `/**/` boundaries).
#[must_use]
pub fn parse_property_from_str(
    prop: PropertyIdKind,
    raw: &str,
    important: bool,
) -> Vec<SpecifiedDeclaration> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if let Some(kw) = GlobalKeyword::from_ident(trimmed) {
        return prop
            .shorthand_longhands()
            .into_iter()
            .map(|target| SpecifiedDeclaration {
                property: target,
                custom_name: None,
                value: PropertyValue::Global(kw),
                important,
                order: 0,
            })
            .collect();
    }
    if crate::cssom_decl::is_color_longhand(prop) {
        if let Some(mut expr) = crate::color::parse_css_color(trimmed) {
            expr.bind_random_property(prop.css_name());
            return vec![SpecifiedDeclaration {
                property: prop,
                custom_name: None,
                value: PropertyValue::Color4(Box::new(expr)),
                important,
                order: 0,
            }];
        }
        return Vec::new();
    }
    if prop == PropertyIdKind::Background {
        let lower = trimmed.to_ascii_lowercase();
        let fn_prefix = ["image-set(", "-webkit-image-set(", "src(", "image("]
            .into_iter()
            .find(|p| lower.starts_with(p));
        if let Some(pfx) = fn_prefix {
            let bytes = trimmed.as_bytes();
            let mut depth = 0usize;
            let mut end = None;
            let mut in_str: Option<u8> = None;
            for (idx, &b) in bytes.iter().enumerate().skip(pfx.len() - 1) {
                if let Some(q) = in_str {
                    if b == q && (idx == 0 || bytes[idx - 1] != b'\\') {
                        in_str = None;
                    }
                    continue;
                }
                match b {
                    b'"' | b'\'' => in_str = Some(b),
                    b'(' => depth += 1,
                    b')' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            end = Some(idx);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(close) = end {
                let img_part = &trimmed[..=close];
                let rest = trimmed[close + 1..].trim();
                if crate::substitution::parse_and_serialize_bg_image_fn(
                    img_part,
                    &crate::values::EvalContext::default(),
                )
                .is_some()
                {
                    let mut decls = vec![SpecifiedDeclaration {
                        property: PropertyIdKind::BackgroundLayers,
                        custom_name: None,
                        value: PropertyValue::BackgroundImageRaw(img_part.to_string()),
                        important,
                        order: 0,
                    }];
                    if rest.is_empty() {
                        decls.push(SpecifiedDeclaration {
                            property: PropertyIdKind::BackgroundColor,
                            custom_name: None,
                            value: PropertyValue::Color(CssColor::Rgba(
                                crate::values::ColorRgba::TRANSPARENT,
                            )),
                            important,
                            order: 1,
                        });
                        return decls;
                    } else if let Some(mut expr) = crate::color::parse_css_color(rest) {
                        expr.bind_random_property("background-color");
                        decls.push(SpecifiedDeclaration {
                            property: PropertyIdKind::BackgroundColor,
                            custom_name: None,
                            value: PropertyValue::Color4(Box::new(expr)),
                            important,
                            order: 1,
                        });
                        return decls;
                    }
                }
            }
        }
    }
    if should_use_values4_parser(prop, trimmed)
        || is_values4_native_property(prop)
        || (supports_values4_math(prop) && crate::values_math::has_values4_syntax(trimmed))
    {
        if let Some((val, _)) = parse_values4_property(prop, trimmed) {
            return vec![SpecifiedDeclaration {
                property: prop,
                custom_name: None,
                value: val,
                important,
                order: 0,
            }];
        }
        if is_values4_native_property(prop) {
            return Vec::new();
        }
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let opts = ParserOptions {
            error_recovery: true,
            ..ParserOptions::default()
        };
        let mut out = Vec::new();
        let mut order = 0u32;
        let mut ignored_missing = Vec::new();
        if let Ok(parsed) = Property::parse_string(PropertyId::from(prop.css_name()), trimmed, opts)
        {
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
        | PropertyIdKind::BorderBlockStartColor
        | PropertyIdKind::BorderBlockEndColor
        | PropertyIdKind::BorderInlineStartColor
        | PropertyIdKind::BorderInlineEndColor
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
        | PropertyIdKind::MaxHeight
        | PropertyIdKind::InlineSize
        | PropertyIdKind::BlockSize
        | PropertyIdKind::MinInlineSize
        | PropertyIdKind::MinBlockSize
        | PropertyIdKind::MaxInlineSize
        | PropertyIdKind::MaxBlockSize => match tok {
            CssToken::Dimension(v, u) if *v >= 0.0 => {
                LengthUnit::from_unit_str(u.as_str()).map(|unit| {
                    PropertyValue::Size(SizeValue::Length(LengthPercentage::Length(*v, unit)))
                })
            }
            CssToken::Percentage(p) if *p >= 0.0 => Some(PropertyValue::Size(SizeValue::Length(
                LengthPercentage::Percent(*p),
            ))),
            _ => None,
        },
        PropertyIdKind::FontSize
        | PropertyIdKind::PaddingTop
        | PropertyIdKind::PaddingRight
        | PropertyIdKind::PaddingBottom
        | PropertyIdKind::PaddingLeft
        | PropertyIdKind::PaddingBlockStart
        | PropertyIdKind::PaddingBlockEnd
        | PropertyIdKind::PaddingInlineStart
        | PropertyIdKind::PaddingInlineEnd
        | PropertyIdKind::RowGap
        | PropertyIdKind::ColumnGap => match tok {
            CssToken::Dimension(v, u) if *v >= 0.0 => LengthUnit::from_unit_str(u.as_str())
                .map(|unit| PropertyValue::Length(LengthPercentage::Length(*v, unit))),
            CssToken::Percentage(p) if *p >= 0.0 => {
                Some(PropertyValue::Length(LengthPercentage::Percent(*p)))
            }
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn property_has_disallowed_value(prop: &Property<'_>) -> bool {
    use lightningcss::properties::border::BorderSideWidth as BSW;
    use lightningcss::properties::size::{MaxSize, Size};
    use lightningcss::values::length::{
        Length, LengthPercentage as LP, LengthPercentageOrAuto as LPA,
    };
    let lp_neg = |lp: &LP| match lp {
        LP::Dimension(lv) => convert_length_value(lv).is_negative_literal(),
        LP::Percentage(p) => p.0 < 0.0,
        LP::Calc(_) => false,
    };
    let pad_bad = |v: &LPA| match v {
        LPA::Auto => true, // CSS Box Model L3: padding-* never accepts `auto`.
        LPA::LengthPercentage(lp) => lp_neg(lp),
    };
    let bw_neg = |w: &BSW| match w {
        BSW::Length(Length::Value(lv)) => convert_length_value(lv).is_negative_literal(),
        _ => false,
    };
    match prop {
        Property::Width(s)
        | Property::Height(s)
        | Property::InlineSize(s)
        | Property::BlockSize(s)
        | Property::MinWidth(s)
        | Property::MinHeight(s)
        | Property::MinInlineSize(s)
        | Property::MinBlockSize(s) => matches!(s, Size::LengthPercentage(lp) if lp_neg(lp)),
        Property::MaxWidth(s)
        | Property::MaxHeight(s)
        | Property::MaxInlineSize(s)
        | Property::MaxBlockSize(s) => matches!(s, MaxSize::LengthPercentage(lp) if lp_neg(lp)),
        Property::PaddingTop(v)
        | Property::PaddingRight(v)
        | Property::PaddingBottom(v)
        | Property::PaddingLeft(v)
        | Property::PaddingBlockStart(v)
        | Property::PaddingBlockEnd(v)
        | Property::PaddingInlineStart(v)
        | Property::PaddingInlineEnd(v) => pad_bad(v),
        Property::Padding(p) => {
            pad_bad(&p.top) || pad_bad(&p.right) || pad_bad(&p.bottom) || pad_bad(&p.left)
        }
        Property::PaddingBlock(p) => pad_bad(&p.block_start) || pad_bad(&p.block_end),
        Property::PaddingInline(p) => pad_bad(&p.inline_start) || pad_bad(&p.inline_end),
        Property::BorderTopWidth(w)
        | Property::BorderRightWidth(w)
        | Property::BorderBottomWidth(w)
        | Property::BorderLeftWidth(w)
        | Property::BorderBlockStartWidth(w)
        | Property::BorderBlockEndWidth(w)
        | Property::BorderInlineStartWidth(w)
        | Property::BorderInlineEndWidth(w)
        | Property::OutlineWidth(w) => bw_neg(w),
        Property::BorderWidth(b) => {
            bw_neg(&b.top) || bw_neg(&b.right) || bw_neg(&b.bottom) || bw_neg(&b.left)
        }
        Property::BorderBlockWidth(b) => bw_neg(&b.start) || bw_neg(&b.end),
        Property::BorderInlineWidth(b) => bw_neg(&b.start) || bw_neg(&b.end),
        Property::BorderTopLeftRadius(r, _)
        | Property::BorderTopRightRadius(r, _)
        | Property::BorderBottomRightRadius(r, _)
        | Property::BorderBottomLeftRadius(r, _)
        | Property::BorderStartStartRadius(r)
        | Property::BorderStartEndRadius(r)
        | Property::BorderEndStartRadius(r)
        | Property::BorderEndEndRadius(r) => lp_neg(&r.0) || lp_neg(&r.1),
        _ => false,
    }
}

fn compile_property(
    prop: &Property<'_>,
    imp: bool,
    ord: &mut u32,
    out: &mut Vec<SpecifiedDeclaration>,
    missing: &mut Vec<MissingCssFeature>,
) {
    if property_has_disallowed_value(prop) {
        return;
    }
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
        Property::Width(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::Width,
                PropertyValue::Size(convert_size(s)),
                imp,
            );
        }
        Property::InlineSize(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::InlineSize,
                PropertyValue::Size(convert_size(s)),
                imp,
            );
        }
        Property::Height(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::Height,
                PropertyValue::Size(convert_size(s)),
                imp,
            );
        }
        Property::BlockSize(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BlockSize,
                PropertyValue::Size(convert_size(s)),
                imp,
            );
        }
        Property::MinWidth(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MinWidth,
                PropertyValue::Size(convert_size(s)),
                imp,
            );
        }
        Property::MinInlineSize(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MinInlineSize,
                PropertyValue::Size(convert_size(s)),
                imp,
            );
        }
        Property::MinHeight(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MinHeight,
                PropertyValue::Size(convert_size(s)),
                imp,
            );
        }
        Property::MinBlockSize(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MinBlockSize,
                PropertyValue::Size(convert_size(s)),
                imp,
            );
        }
        Property::MaxWidth(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MaxWidth,
                PropertyValue::Size(convert_max_size(s)),
                imp,
            );
        }
        Property::MaxInlineSize(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MaxInlineSize,
                PropertyValue::Size(convert_max_size(s)),
                imp,
            );
        }
        Property::MaxHeight(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MaxHeight,
                PropertyValue::Size(convert_max_size(s)),
                imp,
            );
        }
        Property::MaxBlockSize(s) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MaxBlockSize,
                PropertyValue::Size(convert_max_size(s)),
                imp,
            );
        }
        Property::MarginTop(v) => push_decl(
            out,
            ord,
            PropertyIdKind::MarginTop,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::MarginRight(v) => push_decl(
            out,
            ord,
            PropertyIdKind::MarginRight,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::MarginBottom(v) => push_decl(
            out,
            ord,
            PropertyIdKind::MarginBottom,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::MarginLeft(v) => push_decl(
            out,
            ord,
            PropertyIdKind::MarginLeft,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::MarginBlockStart(v) => push_decl(
            out,
            ord,
            PropertyIdKind::MarginBlockStart,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::MarginBlockEnd(v) => push_decl(
            out,
            ord,
            PropertyIdKind::MarginBlockEnd,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::MarginInlineStart(v) => push_decl(
            out,
            ord,
            PropertyIdKind::MarginInlineStart,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::MarginInlineEnd(v) => push_decl(
            out,
            ord,
            PropertyIdKind::MarginInlineEnd,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
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
                PropertyIdKind::MarginBlockStart,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&m.block_start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::MarginBlockEnd,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&m.block_end)),
                imp,
            );
        }
        Property::MarginInline(m) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::MarginInlineStart,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&m.inline_start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::MarginInlineEnd,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&m.inline_end)),
                imp,
            );
        }
        Property::PaddingTop(v) => push_decl(
            out,
            ord,
            PropertyIdKind::PaddingTop,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::PaddingRight(v) => push_decl(
            out,
            ord,
            PropertyIdKind::PaddingRight,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::PaddingBottom(v) => push_decl(
            out,
            ord,
            PropertyIdKind::PaddingBottom,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::PaddingLeft(v) => push_decl(
            out,
            ord,
            PropertyIdKind::PaddingLeft,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::PaddingBlockStart(v) => push_decl(
            out,
            ord,
            PropertyIdKind::PaddingBlockStart,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::PaddingBlockEnd(v) => push_decl(
            out,
            ord,
            PropertyIdKind::PaddingBlockEnd,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::PaddingInlineStart(v) => push_decl(
            out,
            ord,
            PropertyIdKind::PaddingInlineStart,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::PaddingInlineEnd(v) => push_decl(
            out,
            ord,
            PropertyIdKind::PaddingInlineEnd,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
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
                PropertyIdKind::PaddingBlockStart,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&p.block_start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::PaddingBlockEnd,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&p.block_end)),
                imp,
            );
        }
        Property::PaddingInline(p) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::PaddingInlineStart,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&p.inline_start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::PaddingInlineEnd,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&p.inline_end)),
                imp,
            );
        }
        Property::Top(v) => push_decl(
            out,
            ord,
            PropertyIdKind::Top,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::Right(v) => push_decl(
            out,
            ord,
            PropertyIdKind::Right,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::Bottom(v) => push_decl(
            out,
            ord,
            PropertyIdKind::Bottom,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::Left(v) => push_decl(
            out,
            ord,
            PropertyIdKind::Left,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::InsetBlockStart(v) => push_decl(
            out,
            ord,
            PropertyIdKind::InsetBlockStart,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::InsetBlockEnd(v) => push_decl(
            out,
            ord,
            PropertyIdKind::InsetBlockEnd,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::InsetInlineStart(v) => push_decl(
            out,
            ord,
            PropertyIdKind::InsetInlineStart,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
        Property::InsetInlineEnd(v) => push_decl(
            out,
            ord,
            PropertyIdKind::InsetInlineEnd,
            PropertyValue::LengthAuto(convert_length_percentage_or_auto(v)),
            imp,
        ),
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
        Property::InsetBlock(i) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::InsetBlockStart,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&i.block_start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::InsetBlockEnd,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&i.block_end)),
                imp,
            );
        }
        Property::InsetInline(i) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::InsetInlineStart,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&i.inline_start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::InsetInlineEnd,
                PropertyValue::LengthAuto(convert_length_percentage_or_auto(&i.inline_end)),
                imp,
            );
        }
        Property::BorderTopWidth(w) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderTopWidth,
            PropertyValue::Length(convert_border_width_lp(w)),
            imp,
        ),
        Property::BorderRightWidth(w) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderRightWidth,
            PropertyValue::Length(convert_border_width_lp(w)),
            imp,
        ),
        Property::BorderBottomWidth(w) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderBottomWidth,
            PropertyValue::Length(convert_border_width_lp(w)),
            imp,
        ),
        Property::BorderLeftWidth(w) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderLeftWidth,
            PropertyValue::Length(convert_border_width_lp(w)),
            imp,
        ),
        Property::BorderBlockStartWidth(w) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderBlockStartWidth,
            PropertyValue::Length(convert_border_width_lp(w)),
            imp,
        ),
        Property::BorderBlockEndWidth(w) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderBlockEndWidth,
            PropertyValue::Length(convert_border_width_lp(w)),
            imp,
        ),
        Property::BorderInlineStartWidth(w) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderInlineStartWidth,
            PropertyValue::Length(convert_border_width_lp(w)),
            imp,
        ),
        Property::BorderInlineEndWidth(w) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderInlineEndWidth,
            PropertyValue::Length(convert_border_width_lp(w)),
            imp,
        ),
        Property::BorderWidth(bw) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderTopWidth,
                PropertyValue::Length(convert_border_width_lp(&bw.top)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderRightWidth,
                PropertyValue::Length(convert_border_width_lp(&bw.right)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderBottomWidth,
                PropertyValue::Length(convert_border_width_lp(&bw.bottom)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderLeftWidth,
                PropertyValue::Length(convert_border_width_lp(&bw.left)),
                imp,
            );
        }
        Property::BorderBlockWidth(bw) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderBlockStartWidth,
                PropertyValue::Length(convert_border_width_lp(&bw.start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderBlockEndWidth,
                PropertyValue::Length(convert_border_width_lp(&bw.end)),
                imp,
            );
        }
        Property::BorderInlineWidth(bw) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderInlineStartWidth,
                PropertyValue::Length(convert_border_width_lp(&bw.start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderInlineEndWidth,
                PropertyValue::Length(convert_border_width_lp(&bw.end)),
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
            let w = convert_border_width_lp(&b.width);
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
                push_decl(out, ord, wk, PropertyValue::Length(w.clone()), imp);
                push_decl(out, ord, sk, PropertyValue::LineStyle(s), imp);
                push_decl(out, ord, ck, PropertyValue::Color(c), imp);
            }
        }
        Property::BorderTop(b) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderTopWidth,
                PropertyValue::Length(convert_border_width_lp(&b.width)),
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
                PropertyValue::Length(convert_border_width_lp(&b.width)),
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
                PropertyValue::Length(convert_border_width_lp(&b.width)),
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
                PropertyValue::Length(convert_border_width_lp(&b.width)),
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
        Property::BorderBlockStartStyle(s) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderBlockStartStyle,
            PropertyValue::LineStyle(convert_line_style(s)),
            imp,
        ),
        Property::BorderBlockEndStyle(s) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderBlockEndStyle,
            PropertyValue::LineStyle(convert_line_style(s)),
            imp,
        ),
        Property::BorderInlineStartStyle(s) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderInlineStartStyle,
            PropertyValue::LineStyle(convert_line_style(s)),
            imp,
        ),
        Property::BorderInlineEndStyle(s) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderInlineEndStyle,
            PropertyValue::LineStyle(convert_line_style(s)),
            imp,
        ),
        Property::BorderBlockStyle(bs) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderBlockStartStyle,
                PropertyValue::LineStyle(convert_line_style(&bs.start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderBlockEndStyle,
                PropertyValue::LineStyle(convert_line_style(&bs.end)),
                imp,
            );
        }
        Property::BorderInlineStyle(bs) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderInlineStartStyle,
                PropertyValue::LineStyle(convert_line_style(&bs.start)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderInlineEndStyle,
                PropertyValue::LineStyle(convert_line_style(&bs.end)),
                imp,
            );
        }
        Property::BorderBlockStartColor(c) => {
            if let Some(v) = convert_color(c) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderBlockStartColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
        }
        Property::BorderBlockEndColor(c) => {
            if let Some(v) = convert_color(c) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderBlockEndColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
        }
        Property::BorderInlineStartColor(c) => {
            if let Some(v) = convert_color(c) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderInlineStartColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
        }
        Property::BorderInlineEndColor(c) => {
            if let Some(v) = convert_color(c) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderInlineEndColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
        }
        Property::BorderBlockColor(bc) => {
            if let Some(v) = convert_color(&bc.start) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderBlockStartColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
            if let Some(v) = convert_color(&bc.end) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderBlockEndColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
        }
        Property::BorderInlineColor(bc) => {
            if let Some(v) = convert_color(&bc.start) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderInlineStartColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
            if let Some(v) = convert_color(&bc.end) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderInlineEndColor,
                    PropertyValue::Color(v),
                    imp,
                );
            }
        }
        Property::BorderBlockStart(b) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderBlockStartWidth,
                PropertyValue::Length(convert_border_width_lp(&b.width)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderBlockStartStyle,
                PropertyValue::LineStyle(convert_line_style(&b.style)),
                imp,
            );
            if let Some(c) = convert_color(&b.color) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderBlockStartColor,
                    PropertyValue::Color(c),
                    imp,
                );
            }
        }
        Property::BorderBlockEnd(b) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderBlockEndWidth,
                PropertyValue::Length(convert_border_width_lp(&b.width)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderBlockEndStyle,
                PropertyValue::LineStyle(convert_line_style(&b.style)),
                imp,
            );
            if let Some(c) = convert_color(&b.color) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderBlockEndColor,
                    PropertyValue::Color(c),
                    imp,
                );
            }
        }
        Property::BorderInlineStart(b) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderInlineStartWidth,
                PropertyValue::Length(convert_border_width_lp(&b.width)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderInlineStartStyle,
                PropertyValue::LineStyle(convert_line_style(&b.style)),
                imp,
            );
            if let Some(c) = convert_color(&b.color) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderInlineStartColor,
                    PropertyValue::Color(c),
                    imp,
                );
            }
        }
        Property::BorderInlineEnd(b) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderInlineEndWidth,
                PropertyValue::Length(convert_border_width_lp(&b.width)),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderInlineEndStyle,
                PropertyValue::LineStyle(convert_line_style(&b.style)),
                imp,
            );
            if let Some(c) = convert_color(&b.color) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BorderInlineEndColor,
                    PropertyValue::Color(c),
                    imp,
                );
            }
        }
        Property::BorderBlock(b) => {
            let w = convert_border_width_lp(&b.width);
            let s = convert_line_style(&b.style);
            let c = convert_color(&b.color);
            for (wk, sk, ck) in [
                (
                    PropertyIdKind::BorderBlockStartWidth,
                    PropertyIdKind::BorderBlockStartStyle,
                    PropertyIdKind::BorderBlockStartColor,
                ),
                (
                    PropertyIdKind::BorderBlockEndWidth,
                    PropertyIdKind::BorderBlockEndStyle,
                    PropertyIdKind::BorderBlockEndColor,
                ),
            ] {
                push_decl(out, ord, wk, PropertyValue::Length(w.clone()), imp);
                push_decl(out, ord, sk, PropertyValue::LineStyle(s), imp);
                if let Some(col) = c {
                    push_decl(out, ord, ck, PropertyValue::Color(col), imp);
                }
            }
        }
        Property::BorderInline(b) => {
            let w = convert_border_width_lp(&b.width);
            let s = convert_line_style(&b.style);
            let c = convert_color(&b.color);
            for (wk, sk, ck) in [
                (
                    PropertyIdKind::BorderInlineStartWidth,
                    PropertyIdKind::BorderInlineStartStyle,
                    PropertyIdKind::BorderInlineStartColor,
                ),
                (
                    PropertyIdKind::BorderInlineEndWidth,
                    PropertyIdKind::BorderInlineEndStyle,
                    PropertyIdKind::BorderInlineEndColor,
                ),
            ] {
                push_decl(out, ord, wk, PropertyValue::Length(w.clone()), imp);
                push_decl(out, ord, sk, PropertyValue::LineStyle(s), imp);
                if let Some(col) = c {
                    push_decl(out, ord, ck, PropertyValue::Color(col), imp);
                }
            }
        }
        Property::BorderStartStartRadius(r) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderStartStartRadius,
            PropertyValue::CornerRadius(convert_corner_radius(r)),
            imp,
        ),
        Property::BorderStartEndRadius(r) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderStartEndRadius,
            PropertyValue::CornerRadius(convert_corner_radius(r)),
            imp,
        ),
        Property::BorderEndStartRadius(r) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderEndStartRadius,
            PropertyValue::CornerRadius(convert_corner_radius(r)),
            imp,
        ),
        Property::BorderEndEndRadius(r) => push_decl(
            out,
            ord,
            PropertyIdKind::BorderEndEndRadius,
            PropertyValue::CornerRadius(convert_corner_radius(r)),
            imp,
        ),
        Property::Resize(r) => {
            use lightningcss::properties::ui::Resize as LR;
            let mode = match r {
                LR::None => ResizeMode::None,
                LR::Both => ResizeMode::Both,
                LR::Horizontal => ResizeMode::Horizontal,
                LR::Vertical => ResizeMode::Vertical,
                LR::Block => ResizeMode::Block,
                LR::Inline => ResizeMode::Inline,
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::Resize,
                PropertyValue::Resize(mode),
                imp,
            );
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
        Property::TextIndent(ti) => push_decl(
            out,
            ord,
            PropertyIdKind::TextIndent,
            PropertyValue::Length(convert_length_percentage(&ti.value)),
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
            let has_light = cs.contains(lightningcss::properties::ui::ColorScheme::Light);
            let has_dark = cs.contains(lightningcss::properties::ui::ColorScheme::Dark);
            let mode = match (has_light, has_dark) {
                (false, false) => 0.0,
                (true, false) => 1.0,
                (false, true) => 2.0,
                (true, true) => 3.0,
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::ColorScheme,
                PropertyValue::Number(mode),
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
        Property::All(kw) => {
            let gk = match kw {
                lightningcss::properties::CSSWideKeyword::Initial => GlobalKeyword::Initial,
                lightningcss::properties::CSSWideKeyword::Inherit => GlobalKeyword::Inherit,
                lightningcss::properties::CSSWideKeyword::Unset => GlobalKeyword::Unset,
                lightningcss::properties::CSSWideKeyword::Revert => GlobalKeyword::Revert,
                lightningcss::properties::CSSWideKeyword::RevertLayer => GlobalKeyword::RevertLayer,
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::All,
                PropertyValue::Global(gk),
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
                    match non_ws[0] {
                        CssToken::Ident(id) => GlobalKeyword::from_ident(id.as_str()),
                        CssToken::Var {
                            name,
                            fallback: None,
                        } if name.as_str() == "--__rix_revert_rule__" => {
                            Some(GlobalKeyword::RevertRule)
                        }
                        _ => None,
                    }
                } else {
                    None
                }
            };
            if let Some(kw) = global_kw {
                for target in kind.shorthand_longhands() {
                    push_decl(out, ord, target, PropertyValue::Global(kw), imp);
                }
                return;
            }
            // Unwrap CSS Color 4/5 (`--__rix_color4__`) and CSS Values L4/L5 (`--__rix_val4__`) sentinels.
            let sentinel_raw: Option<(&str, &str)> = {
                let non_ws: SmallVec<[&CssToken; 2]> = tokens
                    .iter()
                    .filter(|t| !matches!(t, CssToken::Space))
                    .collect();
                if non_ws.len() == 1 {
                    if let CssToken::Var {
                        name,
                        fallback: Some(fb),
                    } = non_ws[0]
                    {
                        if matches!(
                            name.as_str(),
                            "--__rix_color4__" | "--__rix_val4__" | "--__rix_subst__"
                        ) {
                            fb.iter().find_map(|t| match t {
                                CssToken::String(s) => Some((name.as_str(), s.as_str())),
                                _ => None,
                            })
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some((sent_name, raw_val)) = sentinel_raw {
                if sent_name == "--__rix_subst__" {
                    push_decl(
                        out,
                        ord,
                        kind,
                        PropertyValue::Unresolved(tokenize_custom_value_str(raw_val)),
                        imp,
                    );
                } else if sent_name == "--__rix_color4__" {
                    if let Some(mut expr) = crate::color::parse_css_color(raw_val) {
                        expr.bind_random_property(kind.css_name());
                        push_decl(out, ord, kind, PropertyValue::Color4(Box::new(expr)), imp);
                    }
                } else if let Some((val, _)) = parse_values4_property(kind, raw_val) {
                    push_decl(out, ord, kind, val, imp);
                }
                return;
            }
            push_decl(out, ord, kind, PropertyValue::Unresolved(tokens), imp);
        }
        Property::Custom(custom) => match &custom.name {
            CustomPropertyName::Custom(dashed) => {
                if dashed.as_ref().len() <= 2 || token_list_has_unbalanced_delims(&custom.value) {
                    return;
                }
                let idx = *ord;
                *ord = ord.saturating_add(1);
                let mut tokens = convert_token_list(&custom.value);
                if tokens.is_empty() || tokens.iter().all(|t| matches!(t, CssToken::Space)) {
                    // CSS Custom Properties L1 §2: empty/whitespace-only custom property
                    // serializes as a single space (" ").
                    tokens.clear();
                    tokens.push(CssToken::Space);
                } else {
                    // CSS Syntax L3 §5.4.5: trim leading/trailing whitespace tokens while
                    // preserving interior whitespace.
                    let start = tokens
                        .iter()
                        .position(|t| !matches!(t, CssToken::Space))
                        .unwrap_or(0);
                    let end = tokens
                        .iter()
                        .rposition(|t| !matches!(t, CssToken::Space))
                        .map_or(start, |p| p + 1);
                    tokens = tokens[start..end].to_vec();
                }
                let global_kw = {
                    let mut iter = tokens.iter().filter(|t| !matches!(t, CssToken::Space));
                    match (iter.next(), iter.next()) {
                        (Some(CssToken::Ident(id)), None) => GlobalKeyword::from_ident(id.as_str()),
                        (
                            Some(CssToken::Var {
                                name,
                                fallback: None,
                            }),
                            None,
                        ) if name.as_str() == "--__rix_revert_rule__" => {
                            Some(GlobalKeyword::RevertRule)
                        }
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
                if let Some(kind) = PropertyIdKind::from_name(&ident.as_ref().to_ascii_lowercase())
                {
                    let tokens = convert_token_list(&custom.value);
                    let subst_unwrapped = {
                        let non_ws: SmallVec<[&CssToken; 2]> = tokens
                            .iter()
                            .filter(|t| !matches!(t, CssToken::Space))
                            .collect();
                        if non_ws.len() == 1 {
                            if let CssToken::Var {
                                name,
                                fallback: Some(fb),
                            } = non_ws[0]
                            {
                                if name.as_str() == "--__rix_subst__" {
                                    fb.iter().find_map(|t| match t {
                                        CssToken::String(s) => Some(s.as_str()),
                                        _ => None,
                                    })
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };
                    if let Some(raw_val) = subst_unwrapped {
                        push_decl(
                            out,
                            ord,
                            kind,
                            PropertyValue::Unresolved(tokenize_custom_value_str(raw_val)),
                            imp,
                        );
                        return;
                    }
                    if is_values4_native_property(kind) {
                        let mut handled = false;
                        {
                            let non_ws: SmallVec<[&CssToken; 2]> = tokens
                                .iter()
                                .filter(|t| !matches!(t, CssToken::Space))
                                .collect();
                            if non_ws.len() == 1 {
                                match non_ws[0] {
                                    CssToken::Ident(id) => {
                                        if let Some(kw) = GlobalKeyword::from_ident(id.as_str()) {
                                            push_decl(
                                                out,
                                                ord,
                                                kind,
                                                PropertyValue::Global(kw),
                                                imp,
                                            );
                                            handled = true;
                                        }
                                    }
                                    CssToken::Var {
                                        name,
                                        fallback: None,
                                    } if name.as_str() == "--__rix_revert_rule__" => {
                                        push_decl(
                                            out,
                                            ord,
                                            kind,
                                            PropertyValue::Global(GlobalKeyword::RevertRule),
                                            imp,
                                        );
                                        handled = true;
                                    }
                                    CssToken::Var {
                                        name,
                                        fallback: Some(fb),
                                    } if name.as_str() == "--__rix_val4__" => {
                                        if let Some(raw_val) = fb.iter().find_map(|t| match t {
                                            CssToken::String(s) => Some(s.as_str()),
                                            _ => None,
                                        }) {
                                            if let Some((val, _)) =
                                                parse_values4_property(kind, raw_val)
                                            {
                                                push_decl(out, ord, kind, val, imp);
                                            }
                                        }
                                        handled = true;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        if handled {
                            return;
                        }
                        if tokens.iter().any(css_token_has_var) {
                            push_decl(out, ord, kind, PropertyValue::Unresolved(tokens), imp);
                            return;
                        }
                        let mut buf = String::new();
                        write_tokens_for_reparse(&tokens, &mut buf);
                        if let Some((val, _)) = parse_values4_property(kind, buf.trim()) {
                            push_decl(out, ord, kind, val, imp);
                        }
                        return;
                    }
                }
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

fn token_list_has_unbalanced_delims(list: &TokenList<'_>) -> bool {
    let mut stack: SmallVec<[u8; 8]> = SmallVec::new();
    for item in &list.0 {
        match item {
            TokenOrValue::Token(tok) => match tok {
                LightningToken::ParenthesisBlock => stack.push(b')'),
                LightningToken::SquareBracketBlock => stack.push(b']'),
                LightningToken::CurlyBracketBlock => stack.push(b'}'),
                LightningToken::CloseParenthesis if stack.pop() != Some(b')') => return true,
                LightningToken::CloseSquareBracket if stack.pop() != Some(b']') => return true,
                LightningToken::CloseCurlyBracket if stack.pop() != Some(b'}') => return true,
                _ => {}
            },
            TokenOrValue::Function(f) => {
                if token_list_has_unbalanced_delims(&f.arguments) {
                    return true;
                }
            }
            TokenOrValue::Var(v) => {
                if let Some(fb) = &v.fallback {
                    if token_list_has_unbalanced_delims(fb) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    !stack.is_empty()
}

fn css_token_has_var(tok: &CssToken) -> bool {
    crate::substitution::token_has_subst(tok)
}

fn parse_overscroll_mode(ident: &str) -> Option<OverscrollBehaviorMode> {
    match ident.to_ascii_lowercase().as_str() {
        "auto" => Some(OverscrollBehaviorMode::Auto),
        "contain" => Some(OverscrollBehaviorMode::Contain),
        "none" => Some(OverscrollBehaviorMode::None),
        _ => None,
    }
}

pub(crate) fn compile_extra_standard_property(
    name: &str,
    tokens: &TokenList<'_>,
    imp: bool,
    ord: &mut u32,
    out: &mut Vec<SpecifiedDeclaration>,
    missing: &mut Vec<MissingCssFeature>,
) {
    let lower = name.to_ascii_lowercase();
    let owned = convert_token_list(tokens);
    if owned.iter().any(css_token_has_var) {
        if let Some(kind) = PropertyIdKind::from_name(&lower) {
            push_decl(out, ord, kind, PropertyValue::Unresolved(owned), imp);
            return;
        }
    }
    let non_ws: SmallVec<[&CssToken; 4]> = owned
        .iter()
        .filter(|t| !matches!(t, CssToken::Space))
        .collect();
    let single_ident = if non_ws.len() == 1 {
        match non_ws[0] {
            CssToken::Ident(s) => Some(s.as_str()),
            _ => None,
        }
    } else {
        None
    };
    if let Some(id) = single_ident {
        if let Some(kw) = GlobalKeyword::from_ident(id) {
            if let Some(kind) = PropertyIdKind::from_name(&lower) {
                for target in kind.shorthand_longhands() {
                    push_decl(out, ord, target, PropertyValue::Global(kw), imp);
                }
                return;
            }
        }
    }
    match lower.as_str() {
        "writing-mode" => {
            let Some(mode) = single_ident.and_then(|s| match s.to_ascii_lowercase().as_str() {
                "horizontal-tb" => Some(WritingMode::HorizontalTb),
                "vertical-rl" => Some(WritingMode::VerticalRl),
                "vertical-lr" => Some(WritingMode::VerticalLr),
                "sideways-rl" => Some(WritingMode::SidewaysRl),
                "sideways-lr" => Some(WritingMode::SidewaysLr),
                _ => None,
            }) else {
                return;
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::WritingMode,
                PropertyValue::WritingMode(mode),
                imp,
            );
        }
        "caption-side" => {
            let Some(mode) = single_ident.and_then(|s| match s.to_ascii_lowercase().as_str() {
                "top" => Some(CaptionSideMode::Top),
                "bottom" => Some(CaptionSideMode::Bottom),
                "block-start" => Some(CaptionSideMode::BlockStart),
                "block-end" => Some(CaptionSideMode::BlockEnd),
                "inline-start" => Some(CaptionSideMode::InlineStart),
                "inline-end" => Some(CaptionSideMode::InlineEnd),
                _ => None,
            }) else {
                return;
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::CaptionSide,
                PropertyValue::CaptionSide(mode),
                imp,
            );
        }
        "overscroll-behavior-x"
        | "overscroll-behavior-y"
        | "overscroll-behavior-inline"
        | "overscroll-behavior-block" => {
            let Some(mode) = single_ident.and_then(parse_overscroll_mode) else {
                return;
            };
            let kind = PropertyIdKind::from_name(&lower).unwrap();
            push_decl(out, ord, kind, PropertyValue::OverscrollBehavior(mode), imp);
        }
        "overscroll-behavior" => {
            let idents: SmallVec<[OverscrollBehaviorMode; 2]> = non_ws
                .iter()
                .filter_map(|t| match t {
                    CssToken::Ident(s) => parse_overscroll_mode(s.as_str()),
                    _ => None,
                })
                .collect();
            if idents.is_empty() || idents.len() != non_ws.len() || idents.len() > 2 {
                return;
            }
            let x = idents[0];
            let y = *idents.get(1).unwrap_or(&x);
            push_decl(
                out,
                ord,
                PropertyIdKind::OverscrollBehaviorX,
                PropertyValue::OverscrollBehavior(x),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::OverscrollBehaviorY,
                PropertyValue::OverscrollBehavior(y),
                imp,
            );
        }
        "float" => {
            let Some(mode) = single_ident.and_then(|s| match s.to_ascii_lowercase().as_str() {
                "none" => Some(FloatMode::None),
                "left" => Some(FloatMode::Left),
                "right" => Some(FloatMode::Right),
                "inline-start" => Some(FloatMode::InlineStart),
                "inline-end" => Some(FloatMode::InlineEnd),
                _ => None,
            }) else {
                return;
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
            let Some(mode) = single_ident.and_then(|s| match s.to_ascii_lowercase().as_str() {
                "none" => Some(ClearMode::None),
                "left" => Some(ClearMode::Left),
                "right" => Some(ClearMode::Right),
                "both" => Some(ClearMode::Both),
                "inline-start" => Some(ClearMode::InlineStart),
                "inline-end" => Some(ClearMode::InlineEnd),
                _ => None,
            }) else {
                return;
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
            let Some(mode) = single_ident.and_then(|s| match s.to_ascii_lowercase().as_str() {
                "none" => Some(PointerEventsMode::None),
                "auto" | "all" | "visiblepainted" | "visiblefill" | "visiblestroke" | "visible"
                | "painted" | "fill" | "stroke" => Some(PointerEventsMode::Auto),
                _ => None,
            }) else {
                return;
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
            let Some(mode) = single_ident.and_then(|s| match s.to_ascii_lowercase().as_str() {
                "collapse" => Some(BorderCollapseMode::Collapse),
                "separate" => Some(BorderCollapseMode::Separate),
                _ => None,
            }) else {
                return;
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
            let Some(mode) = single_ident.and_then(|s| match s.to_ascii_lowercase().as_str() {
                "fixed" => Some(TableLayoutMode::Fixed),
                "auto" => Some(TableLayoutMode::Auto),
                _ => None,
            }) else {
                return;
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
            let has_light = owned.iter().any(
                |t| matches!(t, CssToken::Ident(id) if id.as_str().eq_ignore_ascii_case("light")),
            );
            let has_dark = owned.iter().any(
                |t| matches!(t, CssToken::Ident(id) if id.as_str().eq_ignore_ascii_case("dark")),
            );
            let mode = match (has_light, has_dark) {
                (false, false) => 0.0,
                (true, false) => 1.0,
                (false, true) => 2.0,
                (true, true) => 3.0,
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::ColorScheme,
                PropertyValue::Number(mode),
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
