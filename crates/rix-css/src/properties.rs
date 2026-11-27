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
    convert_background_clip, convert_background_layer, convert_background_repeat,
    convert_background_size, convert_blend_mode, convert_border_width_lp, convert_box_shadow,
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
    AlignValue, AnimationCompositionMode, AnimationDirectionMode, AnimationFillModeKind,
    AnimationPlayStateMode, AnimationSpec, AppearanceMode, BackgroundClipMode,
    BackgroundImageValue, BackgroundLayerValue, BackgroundRepeatPair, BackgroundSizeValue,
    BorderCollapseMode, BoxShadowValue, BoxSizingMode, CaptionSideMode, ClearMode, ClipPathValue,
    ColorInterpolationFiltersMode, ContentVisibilityMode, CornerRadius, CssColor, CursorKind,
    DirectionMode, DisplayMode, EmptyCellsMode, FilterOp, FilterSpecOp, FlexDirectionMode,
    FlexWrapMode, FloatMode, FontStyleMode, GridAutoFlowMode, GridPlacementValue,
    GridTemplateAreasValue, GridTrackSize, InterpolateSizeMode, LengthPercentage,
    LengthPercentageAuto, LengthUnit, LineHeightValue, LineStyle, ListStylePositionMode,
    ListStyleTypeMode, MixBlendMode, OverflowMode, OverflowWrapMode, OverlayMode,
    OverscrollBehaviorMode, PointerEventsMode, PositionMode, ReadingFlowMode, ResizeMode,
    ScrollBehaviorMode, SizeValue, TableLayoutMode, TextAlignLastMode, TextAlignMode,
    TextDecorationLine, TextJustifyMode, TextOrientationMode, TextOverflowMode, TextTransformMode,
    TimingFunction, TransformOp, TransformOrigin, TransitionBehaviorMode, TransitionSpec,
    UserSelectMode, VerticalAlignMode, VisibilityMode, WhiteSpaceMode, WordBreakMode, WritingMode,
    ZIndexValue,
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

/// Tokenizes raw CSS text directly via `cssparser` into [`CssToken`]s without invoking
/// `lightningcss` color coercion (preventing recursion and preserving CSS Color 4 functions).
fn tokenize_raw_css_parser(parser: &mut cssparser::Parser<'_, '_>, out: &mut Vec<CssToken>) {
    use cssparser::Token;
    while let Ok(tok) = parser.next_including_whitespace().cloned() {
        match tok {
            Token::Ident(s) => out.push(CssToken::Ident(intern_str(s.as_ref()))),
            Token::Number { value, .. } => out.push(CssToken::Number(value)),
            Token::Percentage { unit_value, .. } => out.push(CssToken::Percentage(unit_value)),
            Token::Dimension { value, unit, .. } => {
                out.push(CssToken::Dimension(value, intern_str(unit.as_ref())));
            }
            Token::QuotedString(s) => out.push(CssToken::String(intern_str(s.as_ref()))),
            Token::UnquotedUrl(u) => out.push(CssToken::Url(intern_str(u.as_ref()))),
            Token::Hash(h) | Token::IDHash(h) => out.push(CssToken::Hash(intern_str(h.as_ref()))),
            Token::Delim(c) => out.push(CssToken::Delim(c)),
            Token::Comma => out.push(CssToken::Comma),
            Token::Colon => out.push(CssToken::Colon),
            Token::Semicolon => out.push(CssToken::Delim(';')),
            Token::WhiteSpace(_) => out.push(CssToken::Space),
            Token::Function(name) => {
                let mut args = Vec::new();
                let _ = parser.parse_nested_block(|nested| {
                    tokenize_raw_css_parser(nested, &mut args);
                    Ok::<(), cssparser::ParseError<'_, ()>>(())
                });
                out.push(CssToken::Function {
                    name: intern_str(name.as_ref()),
                    args,
                });
            }
            Token::ParenthesisBlock => {
                out.push(CssToken::ParenOpen);
                let _ = parser.parse_nested_block(|nested| {
                    tokenize_raw_css_parser(nested, out);
                    Ok::<(), cssparser::ParseError<'_, ()>>(())
                });
                out.push(CssToken::ParenClose);
            }
            Token::SquareBracketBlock => {
                out.push(CssToken::Delim('['));
                let _ = parser.parse_nested_block(|nested| {
                    tokenize_raw_css_parser(nested, out);
                    Ok::<(), cssparser::ParseError<'_, ()>>(())
                });
                out.push(CssToken::Delim(']'));
            }
            Token::CurlyBracketBlock => {
                out.push(CssToken::Delim('{'));
                let _ = parser.parse_nested_block(|nested| {
                    tokenize_raw_css_parser(nested, out);
                    Ok::<(), cssparser::ParseError<'_, ()>>(())
                });
                out.push(CssToken::Delim('}'));
            }
            _ => {}
        }
    }
}

fn tokenize_raw_css_str(raw: &str, out: &mut Vec<CssToken>) {
    let mut input = cssparser::ParserInput::new(raw);
    let mut parser = cssparser::Parser::new(&mut input);
    tokenize_raw_css_parser(&mut parser, out);
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
            use lightningcss::values::color::CssColor as LcColor;
            match c {
                LcColor::RGBA(_) | LcColor::CurrentColor | LcColor::System(_) => {
                    if let Some(color) = convert_color(c) {
                        out.push(CssToken::Color(color));
                    }
                }
                other => {
                    // Preserve CSS Color 4 wide-gamut / perceptual / light-dark colors as raw CSS
                    // tokens so custom properties (`--color-white: oklch(1 0 0)`) never lose
                    // gamut or color-space identity across `var(...)` substitution.
                    if let Ok(css_str) = lightningcss::traits::ToCss::to_css_string(
                        other,
                        lightningcss::printer::PrinterOptions::default(),
                    ) {
                        tokenize_raw_css_str(&css_str, out);
                    } else if let Some(color) = convert_color(other) {
                        out.push(CssToken::Color(color));
                    }
                }
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
        TokenOrValue::UnresolvedColor(uc) => match uc {
            lightningcss::properties::custom::UnresolvedColor::RGB { r, g, b, alpha } => {
                let mut args = vec![
                    CssToken::Number(*r),
                    CssToken::Space,
                    CssToken::Number(*g),
                    CssToken::Space,
                    CssToken::Number(*b),
                    CssToken::Space,
                    CssToken::Delim('/'),
                    CssToken::Space,
                ];
                args.extend(convert_token_list(alpha));
                out.push(CssToken::Function {
                    name: intern_str("rgb"),
                    args,
                });
            }
            lightningcss::properties::custom::UnresolvedColor::HSL { h, s, l, alpha } => {
                let mut args = vec![
                    CssToken::Number(*h),
                    CssToken::Space,
                    CssToken::Percentage(*s / 100.0),
                    CssToken::Space,
                    CssToken::Percentage(*l / 100.0),
                    CssToken::Space,
                    CssToken::Delim('/'),
                    CssToken::Space,
                ];
                args.extend(convert_token_list(alpha));
                out.push(CssToken::Function {
                    name: intern_str("hsl"),
                    args,
                });
            }
            lightningcss::properties::custom::UnresolvedColor::LightDark { light, dark } => {
                let mut args = convert_token_list(light);
                args.push(CssToken::Comma);
                args.push(CssToken::Space);
                args.extend(convert_token_list(dark));
                out.push(CssToken::Function {
                    name: intern_str("light-dark"),
                    args,
                });
            }
        },
        TokenOrValue::Env(_) | TokenOrValue::AnimationName(_) => {}
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
                CssColor::System(sys) => {
                    let rgba = sys.resolve(false);
                    let _ = write!(buf, "rgb({}, {}, {})", rgba.r, rgba.g, rgba.b);
                }
            },
            CssToken::String(s) => {
                buf.push('"');
                buf.push_str(s.as_str());
                buf.push('"');
            }
            CssToken::Url(u) => {
                // CSSOM §6.7.2: serialize a URL as `url("...")`.
                let _ = write!(
                    buf,
                    "url({})",
                    crate::substitution::quote_css_string(u.as_str())
                );
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
#[repr(u16)]
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
    BackgroundSize,
    BackgroundClip,
    BackgroundPosition,
    BackgroundPositionX,
    BackgroundPositionY,
    MaskImage,
    ObjectPosition,
    OffsetDistance,
    ShapeMargin,
    StrokeWidth,
    StrokeDashoffset,
    StrokeDasharray,
    SvgFill,
    SvgStroke,
    SvgFillRule,
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
    BackdropFilter,
    ColorInterpolationFilters,
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
    AnimationTimeline,
    AnimationRange,
    AnimationRangeStart,
    AnimationRangeEnd,
    TextShadow,
    TextDecorationInset,
    InterpolateSize,
    ViewTransitionName,
    Overlay,
    ReadingFlow,
    ReadingOrder,
    TextJustify,
    TextAlignLast,
    TextAlignAll,
    HyphenateCharacter,
    HyphenateLimitLines,
    HyphenateLimitChars,
    Hyphens,
    LineBreak,
    WhiteSpaceCollapse,
    WhiteSpaceTrim,
    TextWrap,
    TextWrapMode,
    TextWrapStyle,
    TextAutospace,
    TextAutospaceSize,
    TextSpacing,
    TextSpacingTrim,
    HangingPunctuation,
    TextFit,
    TextGroupAlign,
    WordSpaceTransform,
    WrapInside,
    Contain,
    ContainerType,
    ContainerName,
    Container,
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
    FitTolerance,
    GridLanesDirection,
    GridLanesPack,
    // Shorthands (retained when unresolved via `var()`)
    Grid,
    GridTemplate,
    GridRow,
    GridColumn,
    GridArea,
    GridLanes,
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
    EmptyCells,
    TableLayout,
    ColorScheme,
    Content,
    // CSS Writing Modes & Logical Properties Level 1
    WritingMode,
    TextOrientation,
    Resize,
    Appearance,
    AccentColor,
    CaptionSide,
    OverscrollBehaviorX,
    OverscrollBehaviorY,
    OverscrollBehaviorInline,
    OverscrollBehaviorBlock,
    OverscrollBehavior,
    ScrollBehavior,
    ContentVisibility,
    Zoom,
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
    // Additional standard CSS2/CSS3 properties (CSSOM serialization & shorthands)
    AlignmentBaseline,
    BackgroundAttachment,
    BackgroundOrigin,
    BackgroundRepeat,
    BaselineShift,
    BaselineSource,
    BorderImage,
    BorderImageSource,
    CaretColor,
    Clip,
    ColumnRuleColor,
    FontFeatureSettings,
    FontSizeAdjust,
    FontStretch,
    FontVariant,
    FontVariationSettings,
    ListStyle,
    ListStyleImage,
    ListStylePosition,
    ListStyleType,
    PageBreakAfter,
    PageBreakBefore,
    PageBreakInside,
    PerspectiveOrigin,
    SvgClipRule,
    SvgDominantBaseline,
    SvgFillOpacity,
    SvgFloodOpacity,
    SvgStopOpacity,
    SvgStrokeLinecap,
    SvgStrokeLinejoin,
    SvgStrokeMiterlimit,
    SvgStrokeOpacity,
    SvgTextAnchor,
    TextDecorationStyle,
    UnicodeBidi,
    WillChange,
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
            "text-orientation" => Self::TextOrientation,
            "resize" => Self::Resize,
            "appearance" | "-webkit-appearance" | "-moz-appearance" => Self::Appearance,
            "accent-color" => Self::AccentColor,
            "caption-side" => Self::CaptionSide,
            "overscroll-behavior-x" => Self::OverscrollBehaviorX,
            "overscroll-behavior-y" => Self::OverscrollBehaviorY,
            "overscroll-behavior-inline" => Self::OverscrollBehaviorInline,
            "overscroll-behavior-block" => Self::OverscrollBehaviorBlock,
            "overscroll-behavior" => Self::OverscrollBehavior,
            "scroll-behavior" => Self::ScrollBehavior,
            "content-visibility" => Self::ContentVisibility,
            "zoom" => Self::Zoom,
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
            "background-size" => Self::BackgroundSize,
            "background-clip" | "-webkit-background-clip" => Self::BackgroundClip,
            "background-position" => Self::BackgroundPosition,
            "background-position-x" => Self::BackgroundPositionX,
            "background-position-y" => Self::BackgroundPositionY,
            "mask-image" | "-webkit-mask-image" | "mask" | "-webkit-mask" => Self::MaskImage,
            "object-position" => Self::ObjectPosition,
            "offset-distance" => Self::OffsetDistance,
            "shape-margin" => Self::ShapeMargin,
            "stroke-width" => Self::StrokeWidth,
            "stroke-dashoffset" => Self::StrokeDashoffset,
            "stroke-dasharray" => Self::StrokeDasharray,
            "fill" => Self::SvgFill,
            "stroke" => Self::SvgStroke,
            "fill-rule" => Self::SvgFillRule,
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
            "backdrop-filter" | "-webkit-backdrop-filter" => Self::BackdropFilter,
            "color-interpolation-filters" => Self::ColorInterpolationFilters,
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
            "animation-timeline" => Self::AnimationTimeline,
            "animation-range" => Self::AnimationRange,
            "animation-range-start" => Self::AnimationRangeStart,
            "animation-range-end" => Self::AnimationRangeEnd,
            "text-shadow" => Self::TextShadow,
            "text-decoration-inset" => Self::TextDecorationInset,
            "interpolate-size" => Self::InterpolateSize,
            "view-transition-name" => Self::ViewTransitionName,
            "overlay" => Self::Overlay,
            "reading-flow" => Self::ReadingFlow,
            "reading-order" => Self::ReadingOrder,
            "text-justify" => Self::TextJustify,
            "text-align-last" => Self::TextAlignLast,
            "text-align-all" => Self::TextAlignAll,
            "hyphenate-character" => Self::HyphenateCharacter,
            "hyphenate-limit-lines" => Self::HyphenateLimitLines,
            "hyphenate-limit-chars" => Self::HyphenateLimitChars,
            "hyphens" => Self::Hyphens,
            "line-break" => Self::LineBreak,
            "white-space-collapse" => Self::WhiteSpaceCollapse,
            "white-space-trim" => Self::WhiteSpaceTrim,
            "text-wrap" => Self::TextWrap,
            "text-wrap-mode" => Self::TextWrapMode,
            "text-wrap-style" => Self::TextWrapStyle,
            "text-autospace" => Self::TextAutospace,
            "text-autospace-size" => Self::TextAutospaceSize,
            "text-spacing" => Self::TextSpacing,
            "text-spacing-trim" => Self::TextSpacingTrim,
            "hanging-punctuation" => Self::HangingPunctuation,
            "text-fit" => Self::TextFit,
            "text-group-align" => Self::TextGroupAlign,
            "word-space-transform" => Self::WordSpaceTransform,
            "wrap-inside" => Self::WrapInside,
            "contain" => Self::Contain,
            "container-type" => Self::ContainerType,
            "container-name" => Self::ContainerName,
            "container" => Self::Container,
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
            "fit-tolerance" => Self::FitTolerance,
            "grid-lanes-direction" => Self::GridLanesDirection,
            "grid-lanes-pack" => Self::GridLanesPack,
            "grid" => Self::Grid,
            "grid-template" => Self::GridTemplate,
            "grid-row" => Self::GridRow,
            "grid-column" => Self::GridColumn,
            "grid-area" => Self::GridArea,
            "grid-lanes" => Self::GridLanes,
            "grid-gap" => Self::Gap,
            "grid-row-gap" => Self::RowGap,
            "grid-column-gap" => Self::ColumnGap,
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
            "empty-cells" => Self::EmptyCells,
            "table-layout" => Self::TableLayout,
            "color-scheme" => Self::ColorScheme,
            "content" => Self::Content,
            "alignment-baseline" => Self::AlignmentBaseline,
            "background-attachment" => Self::BackgroundAttachment,
            "background-origin" => Self::BackgroundOrigin,
            "background-repeat" => Self::BackgroundRepeat,
            "baseline-shift" => Self::BaselineShift,
            "baseline-source" => Self::BaselineSource,
            "border-image" => Self::BorderImage,
            "border-image-source" => Self::BorderImageSource,
            "caret-color" => Self::CaretColor,
            "clip" => Self::Clip,
            "clip-rule" => Self::SvgClipRule,
            "column-rule-color" => Self::ColumnRuleColor,
            "dominant-baseline" => Self::SvgDominantBaseline,
            "fill-opacity" => Self::SvgFillOpacity,
            "flood-opacity" => Self::SvgFloodOpacity,
            "font-feature-settings" => Self::FontFeatureSettings,
            "font-size-adjust" => Self::FontSizeAdjust,
            "font-stretch" => Self::FontStretch,
            "font-variant" => Self::FontVariant,
            "font-variation-settings" => Self::FontVariationSettings,
            "list-style" => Self::ListStyle,
            "list-style-image" => Self::ListStyleImage,
            "list-style-position" => Self::ListStylePosition,
            "list-style-type" => Self::ListStyleType,
            "page-break-after" => Self::PageBreakAfter,
            "page-break-before" => Self::PageBreakBefore,
            "page-break-inside" => Self::PageBreakInside,
            "perspective-origin" => Self::PerspectiveOrigin,
            "stop-opacity" => Self::SvgStopOpacity,
            "stroke-linecap" => Self::SvgStrokeLinecap,
            "stroke-linejoin" => Self::SvgStrokeLinejoin,
            "stroke-miterlimit" => Self::SvgStrokeMiterlimit,
            "stroke-opacity" => Self::SvgStrokeOpacity,
            "text-anchor" => Self::SvgTextAnchor,
            "text-decoration-style" => Self::TextDecorationStyle,
            "unicode-bidi" => Self::UnicodeBidi,
            "will-change" => Self::WillChange,
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
            Self::BackgroundSize => "background-size",
            Self::BackgroundClip => "background-clip",
            Self::BackgroundPosition => "background-position",
            Self::BackgroundPositionX => "background-position-x",
            Self::BackgroundPositionY => "background-position-y",
            Self::MaskImage => "mask-image",
            Self::ObjectPosition => "object-position",
            Self::OffsetDistance => "offset-distance",
            Self::ShapeMargin => "shape-margin",
            Self::StrokeWidth => "stroke-width",
            Self::StrokeDashoffset => "stroke-dashoffset",
            Self::StrokeDasharray => "stroke-dasharray",
            Self::SvgFill => "fill",
            Self::SvgStroke => "stroke",
            Self::SvgFillRule => "fill-rule",
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
            Self::BackdropFilter => "backdrop-filter",
            Self::ColorInterpolationFilters => "color-interpolation-filters",
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
            Self::AnimationTimeline => "animation-timeline",
            Self::AnimationRange => "animation-range",
            Self::AnimationRangeStart => "animation-range-start",
            Self::AnimationRangeEnd => "animation-range-end",
            Self::TextShadow => "text-shadow",
            Self::TextDecorationInset => "text-decoration-inset",
            Self::InterpolateSize => "interpolate-size",
            Self::ViewTransitionName => "view-transition-name",
            Self::Overlay => "overlay",
            Self::ReadingFlow => "reading-flow",
            Self::ReadingOrder => "reading-order",
            Self::TextJustify => "text-justify",
            Self::TextAlignLast => "text-align-last",
            Self::TextAlignAll => "text-align-all",
            Self::HyphenateCharacter => "hyphenate-character",
            Self::HyphenateLimitLines => "hyphenate-limit-lines",
            Self::HyphenateLimitChars => "hyphenate-limit-chars",
            Self::Hyphens => "hyphens",
            Self::LineBreak => "line-break",
            Self::WhiteSpaceCollapse => "white-space-collapse",
            Self::WhiteSpaceTrim => "white-space-trim",
            Self::TextWrap => "text-wrap",
            Self::TextWrapMode => "text-wrap-mode",
            Self::TextWrapStyle => "text-wrap-style",
            Self::TextAutospace => "text-autospace",
            Self::TextAutospaceSize => "text-autospace-size",
            Self::TextSpacing => "text-spacing",
            Self::TextSpacingTrim => "text-spacing-trim",
            Self::HangingPunctuation => "hanging-punctuation",
            Self::TextFit => "text-fit",
            Self::TextGroupAlign => "text-group-align",
            Self::WordSpaceTransform => "word-space-transform",
            Self::WrapInside => "wrap-inside",
            Self::Contain => "contain",
            Self::ContainerType => "container-type",
            Self::ContainerName => "container-name",
            Self::Container => "container",
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
            Self::FitTolerance => "fit-tolerance",
            Self::GridLanesDirection => "grid-lanes-direction",
            Self::GridLanesPack => "grid-lanes-pack",
            Self::Grid => "grid",
            Self::GridTemplate => "grid-template",
            Self::GridRow => "grid-row",
            Self::GridColumn => "grid-column",
            Self::GridArea => "grid-area",
            Self::GridLanes => "grid-lanes",
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
            Self::EmptyCells => "empty-cells",
            Self::TableLayout => "table-layout",
            Self::ColorScheme => "color-scheme",
            Self::Content => "content",
            Self::WritingMode => "writing-mode",
            Self::TextOrientation => "text-orientation",
            Self::Resize => "resize",
            Self::Appearance => "appearance",
            Self::AccentColor => "accent-color",
            Self::CaptionSide => "caption-side",
            Self::OverscrollBehaviorX => "overscroll-behavior-x",
            Self::OverscrollBehaviorY => "overscroll-behavior-y",
            Self::OverscrollBehaviorInline => "overscroll-behavior-inline",
            Self::OverscrollBehaviorBlock => "overscroll-behavior-block",
            Self::OverscrollBehavior => "overscroll-behavior",
            Self::ScrollBehavior => "scroll-behavior",
            Self::ContentVisibility => "content-visibility",
            Self::Zoom => "zoom",
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
            Self::AlignmentBaseline => "alignment-baseline",
            Self::BackgroundAttachment => "background-attachment",
            Self::BackgroundOrigin => "background-origin",
            Self::BackgroundRepeat => "background-repeat",
            Self::BaselineShift => "baseline-shift",
            Self::BaselineSource => "baseline-source",
            Self::BorderImage => "border-image",
            Self::BorderImageSource => "border-image-source",
            Self::CaretColor => "caret-color",
            Self::Clip => "clip",
            Self::SvgClipRule => "clip-rule",
            Self::ColumnRuleColor => "column-rule-color",
            Self::SvgDominantBaseline => "dominant-baseline",
            Self::SvgFillOpacity => "fill-opacity",
            Self::SvgFloodOpacity => "flood-opacity",
            Self::FontFeatureSettings => "font-feature-settings",
            Self::FontSizeAdjust => "font-size-adjust",
            Self::FontStretch => "font-stretch",
            Self::FontVariant => "font-variant",
            Self::FontVariationSettings => "font-variation-settings",
            Self::ListStyle => "list-style",
            Self::ListStyleImage => "list-style-image",
            Self::ListStylePosition => "list-style-position",
            Self::ListStyleType => "list-style-type",
            Self::PageBreakAfter => "page-break-after",
            Self::PageBreakBefore => "page-break-before",
            Self::PageBreakInside => "page-break-inside",
            Self::PerspectiveOrigin => "perspective-origin",
            Self::SvgStopOpacity => "stop-opacity",
            Self::SvgStrokeLinecap => "stroke-linecap",
            Self::SvgStrokeLinejoin => "stroke-linejoin",
            Self::SvgStrokeMiterlimit => "stroke-miterlimit",
            Self::SvgStrokeOpacity => "stroke-opacity",
            Self::SvgTextAnchor => "text-anchor",
            Self::TextDecorationStyle => "text-decoration-style",
            Self::UnicodeBidi => "unicode-bidi",
            Self::WillChange => "will-change",
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
                | Self::AccentColor
                | Self::CaretColor
                | Self::SvgFill
                | Self::SvgStroke
                | Self::SvgFillRule
                | Self::SvgClipRule
                | Self::Visibility
                | Self::PointerEvents
                | Self::Cursor
                | Self::FontSize
                | Self::FontFeatureSettings
                | Self::FontSizeAdjust
                | Self::FontStretch
                | Self::FontVariationSettings
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
                | Self::TextOrientation
                | Self::CaptionSide
                | Self::Orphans
                | Self::Widows
                | Self::TabSize
                | Self::InterpolateSize
                | Self::TextJustify
                | Self::TextAlignLast
                | Self::TextAlignAll
                | Self::HyphenateCharacter
                | Self::HyphenateLimitLines
                | Self::HyphenateLimitChars
                | Self::Hyphens
                | Self::LineBreak
                | Self::WhiteSpaceCollapse
                | Self::TextWrap
                | Self::TextWrapMode
                | Self::TextWrapStyle
                | Self::TextAutospace
                | Self::TextAutospaceSize
                | Self::TextSpacing
                | Self::TextSpacingTrim
                | Self::HangingPunctuation
                | Self::TextFit
                | Self::WordSpaceTransform
                | Self::BorderCollapse
                | Self::BorderSpacing
                | Self::EmptyCells
                | Self::ColorInterpolationFilters
                | Self::SvgDominantBaseline
                | Self::SvgFillOpacity
                | Self::SvgStrokeLinecap
                | Self::SvgStrokeLinejoin
                | Self::SvgStrokeMiterlimit
                | Self::SvgStrokeOpacity
                | Self::SvgTextAnchor
                | Self::TextShadow
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
                Self::BorderImage,
            ]),
            Self::ListStyle => SmallVec::from_slice(&[
                Self::ListStylePosition,
                Self::ListStyleImage,
                Self::ListStyleType,
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
            Self::GridRow => SmallVec::from_slice(&[Self::GridRowStart, Self::GridRowEnd]),
            Self::GridColumn => SmallVec::from_slice(&[Self::GridColumnStart, Self::GridColumnEnd]),
            Self::GridArea => SmallVec::from_slice(&[
                Self::GridRowStart,
                Self::GridColumnStart,
                Self::GridRowEnd,
                Self::GridColumnEnd,
            ]),
            Self::GridTemplate => SmallVec::from_slice(&[
                Self::GridTemplateRows,
                Self::GridTemplateColumns,
                Self::GridTemplateAreas,
            ]),
            Self::Grid => SmallVec::from_slice(&[
                Self::GridTemplateRows,
                Self::GridTemplateColumns,
                Self::GridTemplateAreas,
                Self::GridAutoRows,
                Self::GridAutoColumns,
                Self::GridAutoFlow,
            ]),
            Self::GridLanes => SmallVec::from_slice(&[
                Self::GridTemplateColumns,
                Self::GridTemplateRows,
                Self::GridTemplateAreas,
                Self::GridLanesDirection,
            ]),
            Self::Overflow => SmallVec::from_slice(&[Self::OverflowX, Self::OverflowY]),
            Self::Background => SmallVec::from_slice(&[
                Self::BackgroundColor,
                Self::BackgroundLayers,
                Self::BackgroundSize,
                Self::BackgroundClip,
            ]),
            Self::Outline => {
                SmallVec::from_slice(&[Self::OutlineWidth, Self::OutlineStyle, Self::OutlineColor])
            }
            Self::PlaceContent => SmallVec::from_slice(&[Self::AlignContent, Self::JustifyContent]),
            Self::PlaceItems => SmallVec::from_slice(&[Self::AlignItems, Self::JustifyItems]),
            Self::PlaceSelf => SmallVec::from_slice(&[Self::AlignSelf, Self::JustifySelf]),
            Self::TextDecoration => {
                SmallVec::from_slice(&[Self::TextDecorationLine, Self::TextDecorationColor])
            }
            Self::WhiteSpace => SmallVec::from_slice(&[
                Self::WhiteSpaceCollapse,
                Self::TextWrapMode,
                Self::WhiteSpaceTrim,
            ]),
            Self::TextWrap => SmallVec::from_slice(&[Self::TextWrapMode, Self::TextWrapStyle]),
            Self::TextSpacing => {
                SmallVec::from_slice(&[Self::TextSpacingTrim, Self::TextAutospace])
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
                Self::AnimationTimeline,
                Self::AnimationRangeStart,
                Self::AnimationRangeEnd,
            ]),
            Self::AnimationRange => {
                SmallVec::from_slice(&[Self::AnimationRangeStart, Self::AnimationRangeEnd])
            }
            Self::Font => SmallVec::from_slice(&[
                Self::FontStyle,
                Self::FontWeight,
                Self::FontSize,
                Self::LineHeight,
                Self::FontFamily,
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
    "accent-color",
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
    "animation-range-end",
    "animation-range-start",
    "animation-timeline",
    "animation-timing-function",
    "appearance",
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
    "color-interpolation-filters",
    "color-scheme",
    "column-count",
    "column-gap",
    "content",
    "cursor",
    "direction",
    "display",
    "empty-cells",
    "backdrop-filter",
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
    "grid-lanes-direction",
    "grid-lanes-pack",
    "grid-row-end",
    "grid-row-start",
    "grid-template-areas",
    "grid-template-columns",
    "grid-template-rows",
    "fit-tolerance",
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
    "text-align-all",
    "hyphenate-character",
    "hyphenate-limit-lines",
    "hyphenate-limit-chars",
    "hyphens",
    "line-break",
    "white-space-collapse",
    "white-space-trim",
    "text-wrap-mode",
    "text-wrap-style",
    "text-autospace",
    "text-autospace-size",
    "text-spacing-trim",
    "hanging-punctuation",
    "text-fit",
    "text-group-align",
    "word-space-transform",
    "wrap-inside",
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
    "scroll-behavior",
    "content-visibility",
    "zoom",
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
    "text-decoration-inset",
    "text-decoration-line",
    "text-indent",
    "text-overflow",
    "text-shadow",
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
    PropertyIdKind::BackgroundSize,
    PropertyIdKind::BackgroundClip,
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
    PropertyIdKind::BackdropFilter,
    PropertyIdKind::ColorInterpolationFilters,
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
    PropertyIdKind::AnimationTimeline,
    PropertyIdKind::AnimationRangeStart,
    PropertyIdKind::AnimationRangeEnd,
    PropertyIdKind::TextShadow,
    PropertyIdKind::TextDecorationInset,
    PropertyIdKind::InterpolateSize,
    PropertyIdKind::Overlay,
    PropertyIdKind::ReadingFlow,
    PropertyIdKind::ReadingOrder,
    PropertyIdKind::TextJustify,
    PropertyIdKind::TextAlignLast,
    PropertyIdKind::TextAlignAll,
    PropertyIdKind::HyphenateCharacter,
    PropertyIdKind::HyphenateLimitLines,
    PropertyIdKind::HyphenateLimitChars,
    PropertyIdKind::Hyphens,
    PropertyIdKind::LineBreak,
    PropertyIdKind::WhiteSpaceCollapse,
    PropertyIdKind::WhiteSpaceTrim,
    PropertyIdKind::TextWrapMode,
    PropertyIdKind::TextWrapStyle,
    PropertyIdKind::TextAutospace,
    PropertyIdKind::TextAutospaceSize,
    PropertyIdKind::TextSpacingTrim,
    PropertyIdKind::HangingPunctuation,
    PropertyIdKind::TextFit,
    PropertyIdKind::TextGroupAlign,
    PropertyIdKind::WordSpaceTransform,
    PropertyIdKind::WrapInside,
    PropertyIdKind::Contain,
    PropertyIdKind::ContainerType,
    PropertyIdKind::ContainerName,
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
    PropertyIdKind::FitTolerance,
    PropertyIdKind::GridLanesDirection,
    PropertyIdKind::GridLanesPack,
    PropertyIdKind::VerticalAlign,
    PropertyIdKind::TextOverflow,
    PropertyIdKind::BorderCollapse,
    PropertyIdKind::BorderSpacing,
    PropertyIdKind::EmptyCells,
    PropertyIdKind::TableLayout,
    PropertyIdKind::ColorScheme,
    PropertyIdKind::Content,
    PropertyIdKind::WritingMode,
    PropertyIdKind::TextOrientation,
    PropertyIdKind::Resize,
    PropertyIdKind::Appearance,
    PropertyIdKind::AccentColor,
    PropertyIdKind::CaptionSide,
    PropertyIdKind::OverscrollBehaviorX,
    PropertyIdKind::OverscrollBehaviorY,
    PropertyIdKind::ScrollBehavior,
    PropertyIdKind::ContentVisibility,
    PropertyIdKind::Zoom,
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
    CursorWithUrls(Vec<(InternedStr, Option<(f32, f32)>)>, CursorKind),
    ZIndex(ZIndexValue),
    Number(f32),
    Integer(i32),
    Color(CssColor),
    Color4(Box<crate::color::CssColorExpr>),
    SvgPaint(Option<CssColor>),
    SvgFillRuleEvenOdd(bool),
    SvgStrokeLinecap(crate::values::SvgStrokeLinecap),
    SvgStrokeLinejoin(crate::values::SvgStrokeLinejoin),
    SvgTextAnchor(crate::values::SvgTextAnchor),
    SvgDominantBaseline(crate::values::SvgDominantBaseline),
    TextDecorationStyle(crate::values::TextDecorationStyleMode),
    OptNumber(Option<f32>),
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
    GridTemplateTracks(crate::grid_css::GridTemplateTrackList),
    GridLine(Option<i32>, Option<i32>),
    GridPlacement(GridPlacementValue),
    GridTemplateAreas(GridTemplateAreasValue),
    FitTolerance(crate::grid_css::FitToleranceValue),
    GridLanesDirection(crate::grid_css::GridLanesDirectionMode),
    GridLanesPack(crate::grid_css::GridLanesPackMode),
    BoxShadow(Vec<BoxShadowValue>),
    BackgroundLayers(Vec<BackgroundLayerValue>),
    BackgroundSize(BackgroundSizeValue),
    BackgroundClip(BackgroundClipMode),
    MaskImage(BackgroundImageValue),
    ClipRect(Option<crate::values::CssClipRect>),
    Transform(Vec<TransformOp>),
    TransformOrigin(TransformOrigin),
    Filter(Vec<FilterOp>),
    FilterExpr(Vec<FilterSpecOp>),
    ColorInterpolationFilters(ColorInterpolationFiltersMode),
    ClipPath(ClipPathValue),
    MixBlendMode(MixBlendMode),
    Transitions(Vec<TransitionSpec>),
    Animations(Vec<AnimationSpec>),
    NameList(Vec<InternedStr>),
    DurationList(Vec<f32>),
    DurationAutoList(Vec<(f32, bool)>),
    TimingList(Vec<TimingFunction>),
    DirectionList(Vec<AnimationDirectionMode>),
    FillModeList(Vec<AnimationFillModeKind>),
    PlayStateList(Vec<AnimationPlayStateMode>),
    AnimationRangeList(Vec<crate::anim_css::AnimationRangePoint>),
    TextShadow(Vec<crate::anim_css::TextShadowSpec>),
    TextDecorationInset(crate::anim_css::TextDecorationInsetValue),
    VerticalAlign(VerticalAlignMode),
    VerticalAlignFull(VerticalAlignMode, Option<LengthPercentage>),
    TextOverflow(TextOverflowMode),
    BorderCollapse(BorderCollapseMode),
    BorderSpacing(LengthPercentage, LengthPercentage),
    EmptyCells(EmptyCellsMode),
    TableLayout(TableLayoutMode),
    Content(Option<InternedStr>),
    ContentCounter(InternedStr, InternedStr),
    WritingMode(WritingMode),
    TextOrientation(TextOrientationMode),
    Resize(ResizeMode),
    Appearance(AppearanceMode),
    AccentColor(Option<CssColor>),
    CaptionSide(CaptionSideMode),
    OverscrollBehavior(OverscrollBehaviorMode),
    ScrollBehavior(ScrollBehaviorMode),
    ContentVisibility(ContentVisibilityMode),
    Zoom(f32),
    MathVal(Box<crate::values_math::MathNode>),
    Scale(crate::values_math::ScaleSpec),
    Rotate(crate::values_math::RotateSpec),
    Translate(crate::values_math::TranslateSpec),
    TransformRaw(String),
    BgPosition(LengthPercentage, LengthPercentage),
    BgPositions(Vec<(LengthPercentage, LengthPercentage)>),
    BackgroundSizes(Vec<BackgroundSizeValue>),
    BackgroundRepeats(Vec<BackgroundRepeatPair>),
    FontWeightRelative(i8),
    GapNormal,
    ObjectPos(Box<crate::values_math::PositionSpec>),
    AspectRatioMath(
        Option<(
            bool,
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
    HyphenateLimitChars(crate::text_css::HyphenateLimitChars),
    Hyphens(crate::text_css::HyphensMode),
    LineBreak(crate::text_css::LineBreakMode),
    WhiteSpaceCollapse(crate::text_css::WhiteSpaceCollapseMode),
    WhiteSpaceTrim(crate::text_css::WhiteSpaceTrim),
    TextWrapMode(crate::text_css::TextWrapMode),
    TextWrapStyle(crate::text_css::TextWrapStyle),
    TextAutospace(crate::text_css::TextAutospaceValue),
    TextSpacingTrim(crate::text_css::TextSpacingTrimMode),
    HangingPunctuation(crate::text_css::HangingPunctuation),
    TextTransformFlags(crate::text_css::TextTransformFlags),
    TextIndentFull(LengthPercentage, bool, bool),
    TextFit(crate::text_css::TextFitValue),
    TextGroupAlign(crate::text_css::TextGroupAlignMode),
    WordSpaceTransform(crate::text_css::WordSpaceTransformMode),
    WrapInside(crate::text_css::WrapInsideMode),
    Contain(u8),
    /// CSS Containment L3 `container-type`: 0=normal, 1=size, 2=inline-size, 4=scroll-state.
    ContainerType(u8),
    /// CSS Containment L3 `container-name`: empty = `none`, otherwise ordered custom-idents.
    ContainerName(Vec<InternedStr>),
    BackgroundImageRaw(String),
    BackgroundAttachment(InternedStr),
    WillChangeCreatesCb(bool),
    FontStyleObliqueMath(Box<crate::values_math::MathNode>),
    /// CSS Fonts L3/L4 `font-feature-settings`: `None` = `normal`, `Some` = `(4-char tag, int math)`.
    FontFeatureSettings(Option<Vec<(InternedStr, crate::values_math::MathNode)>>),
    /// CSS Fonts L4 `font-variation-settings`: `None` = `normal`, `Some` = `(4-char tag, num math)`.
    FontVariationSettings(Option<Vec<(InternedStr, crate::values_math::MathNode)>>),
    ListStyleType(ListStyleTypeMode),
    ListStylePosition(ListStylePositionMode),
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
            Self::FontFeatureSettings(Some(list)) | Self::FontVariationSettings(Some(list)) => {
                list.iter().any(|(_, n)| node_has(n))
            }
            Self::Length(lp)
            | Self::LengthAuto(LengthPercentageAuto::Value(lp))
            | Self::Size(SizeValue::Length(lp)) => lp_has(lp),
            Self::Size(SizeValue::CalcSize(cs)) => cs.contains_sibling_fn(),
            Self::CornerRadius(cr) => lp_has(&cr.x) || lp_has(&cr.y),
            Self::BgPosition(x, y) => lp_has(x) || lp_has(y),
            Self::BgPositions(list) => list.iter().any(|(x, y)| lp_has(x) || lp_has(y)),
            Self::BackgroundSizes(list) => list.iter().any(|sz| match sz {
                BackgroundSizeValue::Explicit(w, h) => {
                    w.as_ref().is_some_and(lp_has) || h.as_ref().is_some_and(lp_has)
                }
                _ => false,
            }),
            Self::Scale(crate::values_math::ScaleSpec::Values(v)) => v.iter().any(node_has),
            Self::Rotate(crate::values_math::RotateSpec::Angle { axis, angle }) => {
                node_has(angle) || axis.as_ref().is_some_and(|a| a.iter().any(node_has))
            }
            Self::Translate(crate::values_math::TranslateSpec::Values { x, y, z }) => {
                node_has(x) || y.as_ref().is_some_and(node_has) || z.as_ref().is_some_and(node_has)
            }
            Self::TransformRaw(s) | Self::BackgroundImageRaw(s) => {
                let l = s.to_ascii_lowercase();
                l.contains("sibling-") || l.contains("random(")
            }
            Self::GridTracks(v) => v.iter().any(|t| match t {
                GridTrackSize::FrMath(n) => node_has(n),
                GridTrackSize::Length(lp) | GridTrackSize::FitContent(lp) => lp_has(lp),
                _ => false,
            }),
            Self::GridTemplateTracks(crate::grid_css::GridTemplateTrackList::Tracks {
                items,
                ..
            }) => items.iter().any(|it| match it {
                crate::grid_css::GridTrackListEntry::Track(t) => match t {
                    GridTrackSize::FrMath(n) => node_has(n),
                    GridTrackSize::Length(lp) | GridTrackSize::FitContent(lp) => lp_has(lp),
                    _ => false,
                },
                crate::grid_css::GridTrackListEntry::Repeat { tracks, .. } => {
                    tracks.iter().any(|t| match t {
                        GridTrackSize::FrMath(n) => node_has(n),
                        GridTrackSize::Length(lp) | GridTrackSize::FitContent(lp) => lp_has(lp),
                        _ => false,
                    })
                }
            }),
            Self::FitTolerance(crate::grid_css::FitToleranceValue::Length(lp)) => lp_has(lp),
            Self::Color4(c) => {
                let s = c.to_specified_css_string();
                s.contains("sibling-") || s.contains("random(")
            }
            Self::AspectRatioMath(Some((_, a, b))) => {
                node_has(a) || b.as_deref().is_some_and(node_has)
            }
            Self::Custom(toks) | Self::Unresolved(toks) => tokens_contain_sibling_fn(toks),
            Self::FontStyleObliqueMath(n) => node_has(n),
            _ => false,
        }
    }

    /// Returns `true` if this declaration's value directly invokes CSS Values L5 `attr(...)`.
    #[must_use]
    pub fn contains_attr_fn(&self) -> bool {
        match self {
            Self::Custom(toks) | Self::Unresolved(toks) => tokens_contain_attr_fn(toks),
            _ => false,
        }
    }

    /// Returns `true` if this declaration's value contains a `var(...)` or `inherit(...)` reference.
    #[must_use]
    pub fn contains_var_fn(&self) -> bool {
        match self {
            Self::Custom(toks) | Self::Unresolved(toks) => tokens_contain_var_fn(toks),
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
                || l == "random-item"
                || tokens_contain_sibling_fn(args)
        }
        CssToken::Var {
            fallback: Some(fb), ..
        } => tokens_contain_sibling_fn(fb),
        _ => false,
    })
}

fn tokens_contain_attr_fn(toks: &[CssToken]) -> bool {
    toks.iter().any(|t| match t {
        CssToken::Function { name, args } => {
            name.as_str().eq_ignore_ascii_case("attr") || tokens_contain_attr_fn(args)
        }
        CssToken::Var {
            fallback: Some(fb), ..
        } => tokens_contain_attr_fn(fb),
        _ => false,
    })
}

fn tokens_contain_var_fn(toks: &[CssToken]) -> bool {
    toks.iter().any(|t| match t {
        CssToken::Var { .. } => true,
        CssToken::Function { name, args } => {
            name.as_str().eq_ignore_ascii_case("var")
                || name.as_str().eq_ignore_ascii_case("inherit")
                || tokens_contain_var_fn(args)
        }
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

fn convert_bg_horiz_pos(
    pos: &lightningcss::values::position::HorizontalPosition,
) -> LengthPercentage {
    use lightningcss::values::position::{HorizontalPosition, HorizontalPositionKeyword};
    match pos {
        HorizontalPosition::Center => LengthPercentage::Percent(0.5),
        HorizontalPosition::Length(lp) => convert_length_percentage(lp),
        HorizontalPosition::Side { side, offset } => {
            if let Some(lp) = offset {
                let l = convert_length_percentage(lp);
                if matches!(side, HorizontalPositionKeyword::Left) {
                    l
                } else {
                    LengthPercentage::Percent(1.0)
                }
            } else {
                match side {
                    HorizontalPositionKeyword::Left => LengthPercentage::Zero,
                    HorizontalPositionKeyword::Right => LengthPercentage::Percent(1.0),
                }
            }
        }
    }
}

fn convert_bg_vert_pos(pos: &lightningcss::values::position::VerticalPosition) -> LengthPercentage {
    use lightningcss::values::position::{VerticalPosition, VerticalPositionKeyword};
    match pos {
        VerticalPosition::Center => LengthPercentage::Percent(0.5),
        VerticalPosition::Length(lp) => convert_length_percentage(lp),
        VerticalPosition::Side { side, offset } => {
            if let Some(lp) = offset {
                let l = convert_length_percentage(lp);
                if matches!(side, VerticalPositionKeyword::Top) {
                    l
                } else {
                    LengthPercentage::Percent(1.0)
                }
            } else {
                match side {
                    VerticalPositionKeyword::Top => LengthPercentage::Zero,
                    VerticalPositionKeyword::Bottom => LengthPercentage::Percent(1.0),
                }
            }
        }
    }
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

/// Strips malformed custom property declarations (`--name: ...;`) whose value contains
/// unbalanced `)`, `]`, or `}` delimiters before `lightningcss` parses the stylesheet.
/// Per CSS Syntax L3 §5, a single syntactically invalid declaration must be ignored without
/// invalidating preceding valid declarations in the same rule block (`lightningcss`'s
/// `NestedRuleParser::parse_nested` clears all prior declarations on `Err`).
#[must_use]
pub(crate) fn strip_unbalanced_custom_decls(css: &str) -> std::borrow::Cow<'_, str> {
    if !css.contains("--") {
        return std::borrow::Cow::Borrowed(css);
    }
    let bytes = css.as_bytes();
    let mut out: Option<String> = None;
    let mut last = 0usize;
    let mut i = 0usize;
    let mut in_str: Option<u8> = None;
    let mut brace_depth = 0usize;
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
        if b == b'{' {
            brace_depth += 1;
            i += 1;
            continue;
        }
        if b == b'}' {
            brace_depth = brace_depth.saturating_sub(1);
            i += 1;
            continue;
        }
        // Detect start of a custom property declaration `--<ident>:` inside a `{ ... }` block.
        if brace_depth > 0
            && b == b'-'
            && i + 2 < bytes.len()
            && bytes[i + 1] == b'-'
            && (i == 0 || matches!(bytes[i - 1], b'{' | b';' | b' ' | b'\t' | b'\n' | b'\r'))
        {
            let decl_start = i;
            let mut p = i + 2;
            while p < bytes.len()
                && !matches!(
                    bytes[p],
                    b':' | b';' | b'{' | b'}' | b' ' | b'\t' | b'\n' | b'\r'
                )
            {
                p += 1;
            }
            while p < bytes.len() && bytes[p].is_ascii_whitespace() {
                p += 1;
            }
            if p < bytes.len() && bytes[p] == b':' {
                let mut k = p + 1;
                let mut stack: SmallVec<[u8; 8]> = SmallVec::new();
                let mut unbalanced = false;
                let mut s_str: Option<u8> = None;
                let mut decl_end = bytes.len();
                while k < bytes.len() {
                    let cb = bytes[k];
                    if let Some(q) = s_str {
                        k = if cb == b'\\' {
                            (k + 2).min(bytes.len())
                        } else {
                            k + 1
                        };
                        if cb == q {
                            s_str = None;
                        }
                        continue;
                    }
                    if cb == b'\\' {
                        k = (k + 2).min(bytes.len());
                        continue;
                    }
                    if cb == b'"' || cb == b'\'' {
                        s_str = Some(cb);
                        k += 1;
                        continue;
                    }
                    if cb == b'/' && k + 1 < bytes.len() && bytes[k + 1] == b'*' {
                        k += 2;
                        while k + 1 < bytes.len() && !(bytes[k] == b'*' && bytes[k + 1] == b'/') {
                            k += 1;
                        }
                        k = (k + 2).min(bytes.len());
                        continue;
                    }
                    if stack.is_empty() && cb == b';' {
                        decl_end = k + 1;
                        break;
                    }
                    if stack.is_empty() && cb == b'}' {
                        decl_end = k;
                        break;
                    }
                    match cb {
                        b'(' | b'[' | b'{' => stack.push(cb),
                        b')' | b']' | b'}' => {
                            let expected = match cb {
                                b')' => b'(',
                                b']' => b'[',
                                _ => b'{',
                            };
                            if stack.pop() != Some(expected) {
                                unbalanced = true;
                                if let Some(pos) = stack.iter().rposition(|&c| c == expected) {
                                    stack.truncate(pos);
                                }
                            }
                        }
                        _ => {}
                    }
                    k += 1;
                }
                if unbalanced {
                    let buf = out.get_or_insert_with(|| String::with_capacity(css.len()));
                    buf.push_str(&css[last..decl_start]);
                    last = decl_end;
                    i = decl_end;
                    continue;
                }
            }
        }
        i += 1;
    }
    if let Some(mut buf) = out {
        buf.push_str(&css[last..]);
        std::borrow::Cow::Owned(buf)
    } else {
        std::borrow::Cow::Borrowed(css)
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
            | PropertyIdKind::TextAlignAll
            | PropertyIdKind::HyphenateCharacter
            | PropertyIdKind::HyphenateLimitLines
            | PropertyIdKind::HyphenateLimitChars
            | PropertyIdKind::Hyphens
            | PropertyIdKind::LineBreak
            | PropertyIdKind::WhiteSpace
            | PropertyIdKind::WhiteSpaceCollapse
            | PropertyIdKind::WhiteSpaceTrim
            | PropertyIdKind::TextWrap
            | PropertyIdKind::TextWrapMode
            | PropertyIdKind::TextWrapStyle
            | PropertyIdKind::TextAutospace
            | PropertyIdKind::TextAutospaceSize
            | PropertyIdKind::TextSpacing
            | PropertyIdKind::TextSpacingTrim
            | PropertyIdKind::HangingPunctuation
            | PropertyIdKind::TextFit
            | PropertyIdKind::TextGroupAlign
            | PropertyIdKind::WordSpaceTransform
            | PropertyIdKind::WrapInside
            | PropertyIdKind::TextIndent
            | PropertyIdKind::LetterSpacing
            | PropertyIdKind::WordSpacing
            | PropertyIdKind::TextTransform
            | PropertyIdKind::WordBreak
            | PropertyIdKind::Contain
            | PropertyIdKind::ContainerType
            | PropertyIdKind::ContainerName
            | PropertyIdKind::Container
            | PropertyIdKind::Display
            | PropertyIdKind::GridTemplateColumns
            | PropertyIdKind::GridTemplateRows
            | PropertyIdKind::GridTemplateAreas
            | PropertyIdKind::GridAutoColumns
            | PropertyIdKind::GridAutoRows
            | PropertyIdKind::GridAutoFlow
            | PropertyIdKind::GridRowStart
            | PropertyIdKind::GridRowEnd
            | PropertyIdKind::GridColumnStart
            | PropertyIdKind::GridColumnEnd
            | PropertyIdKind::FitTolerance
            | PropertyIdKind::GridLanesDirection
            | PropertyIdKind::GridLanesPack
            | PropertyIdKind::Grid
            | PropertyIdKind::GridTemplate
            | PropertyIdKind::GridRow
            | PropertyIdKind::GridColumn
            | PropertyIdKind::GridArea
            | PropertyIdKind::GridLanes
            | PropertyIdKind::Filter
            | PropertyIdKind::BackdropFilter
            | PropertyIdKind::ColorInterpolationFilters
            | PropertyIdKind::Transition
            | PropertyIdKind::TransitionDuration
            | PropertyIdKind::TransitionDelay
            | PropertyIdKind::Animation
            | PropertyIdKind::AnimationName
            | PropertyIdKind::AnimationDuration
            | PropertyIdKind::AnimationDelay
            | PropertyIdKind::AnimationTimeline
            | PropertyIdKind::AnimationRange
            | PropertyIdKind::AnimationRangeStart
            | PropertyIdKind::AnimationRangeEnd
            | PropertyIdKind::TextShadow
            | PropertyIdKind::TextDecorationInset
            | PropertyIdKind::VerticalAlign
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
        return prop == PropertyIdKind::FlexBasis;
    }
    if prop == PropertyIdKind::BackgroundLayers
        && (lower.starts_with("light-dark(")
            || lower.starts_with("image-set(")
            || lower.starts_with("-webkit-image-set(")
            || lower.starts_with("src(")
            || lower.starts_with("image(")
            || ((lower.starts_with("linear-gradient(")
                || lower.starts_with("radial-gradient(")
                || lower.starts_with("conic-gradient(")
                || lower.starts_with("repeating-linear-gradient(")
                || lower.starts_with("repeating-radial-gradient(")
                || lower.starts_with("repeating-conic-gradient("))
                && crate::values_math::has_values4_syntax(trimmed)))
    {
        return true;
    }
    if matches!(
        prop,
        PropertyIdKind::ListStyleImage
            | PropertyIdKind::BorderImageSource
            | PropertyIdKind::Content
    ) && lower.starts_with("light-dark(")
    {
        return true;
    }
    if prop == PropertyIdKind::FontStyle && lower.starts_with("oblique") && lower.contains('(') {
        return true;
    }
    if is_sizing_property(prop)
        && (lower.starts_with("calc-size(")
            || matches!(
                lower.as_str(),
                "stretch" | "-webkit-fill-available" | "-moz-available"
            )
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
    if prop == PropertyIdKind::AnimationDuration
        && (lower.contains("auto") || lower.contains(','))
        && !crate::values_math::has_values4_syntax(trimmed)
    {
        return true;
    }
    if prop == PropertyIdKind::AnimationIterationCount
        && !crate::values_math::has_values4_syntax(trimmed)
    {
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
        P::Opacity | P::SvgFillOpacity => Some((VK::NumberOrPercentage, false)),
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

fn has_top_level_comma_raw(s: &str) -> bool {
    let mut depth = 0usize;
    for b in s.bytes() {
        match b {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

/// Parses a CSS Values L4/L5 declaration value for `prop`, returning `(PropertyValue, specified_css)`.
#[must_use]
pub fn parse_values4_property(prop: PropertyIdKind, raw: &str) -> Option<(PropertyValue, String)> {
    use crate::values::CalcExpr;
    use crate::values_math::{
        parse_and_serialize_transform, parse_background_position_list, parse_image_resolution,
        parse_math_value, parse_math_value_from_parser, MathNode, RotateSpec, ScaleSpec,
        TranslateSpec, ValueKind,
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
            let mut pi = cssparser::ParserInput::new(trimmed);
            let mut p = cssparser::Parser::new(&mut pi);
            let mut names = Vec::new();
            let mut texts = Vec::new();
            let mut saw_none = false;
            loop {
                let id = p.expect_ident().ok()?.as_ref().to_string();
                let lower = id.to_ascii_lowercase();
                if GlobalKeyword::from_ident(&lower).is_some() || lower == "default" {
                    return None;
                }
                if lower == "none" {
                    saw_none = true;
                }
                let stored = if lower == "all"
                    || lower == "none"
                    || PropertyIdKind::from_name(&lower).is_some()
                {
                    lower
                } else {
                    crate::substitution::escape_css_ident(&id)
                };
                names.push(intern_str(&stored));
                texts.push(stored);
                if p.is_exhausted() {
                    break;
                }
                p.expect_comma().ok()?;
            }
            if names.is_empty() || !p.is_exhausted() || (saw_none && names.len() > 1) {
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
        PropertyIdKind::TransitionDuration
            if has_top_level_comma_raw(trimmed)
                || !crate::values_math::has_values4_syntax(trimmed) =>
        {
            let (durs, spec) = crate::anim_css::parse_time_list_values4(trimmed, false, true)?;
            return Some((
                PropertyValue::DurationList(durs.into_iter().map(|(ms, _)| ms).collect()),
                spec,
            ));
        }
        PropertyIdKind::TransitionDelay
            if has_top_level_comma_raw(trimmed)
                || !crate::values_math::has_values4_syntax(trimmed) =>
        {
            let (durs, spec) = crate::anim_css::parse_time_list_values4(trimmed, false, false)?;
            return Some((
                PropertyValue::DurationList(durs.into_iter().map(|(ms, _)| ms).collect()),
                spec,
            ));
        }
        PropertyIdKind::Animation => {
            let (specs, spec) = parse_animation_shorthand(trimmed)?;
            return Some((PropertyValue::Animations(specs), spec));
        }
        PropertyIdKind::AnimationName => {
            let (names, spec) = crate::anim_css::parse_animation_name_list(trimmed)?;
            return Some((PropertyValue::NameList(names), spec));
        }
        PropertyIdKind::AnimationDuration
            if has_top_level_comma_raw(trimmed)
                || !crate::values_math::has_values4_syntax(trimmed) =>
        {
            let (durs, spec) = crate::anim_css::parse_animation_duration_list(trimmed)?;
            return Some((PropertyValue::DurationAutoList(durs), spec));
        }
        PropertyIdKind::AnimationDelay
            if has_top_level_comma_raw(trimmed)
                || !crate::values_math::has_values4_syntax(trimmed) =>
        {
            let (durs, spec) = crate::anim_css::parse_time_list_values4(trimmed, false, false)?;
            return Some((
                PropertyValue::DurationList(durs.into_iter().map(|(ms, _)| ms).collect()),
                spec,
            ));
        }
        PropertyIdKind::AnimationIterationCount
            if !crate::values_math::has_values4_syntax(trimmed) =>
        {
            let (counts, spec) = crate::anim_css::parse_animation_iteration_count_list(trimmed)?;
            return Some((PropertyValue::DurationList(counts), spec));
        }
        PropertyIdKind::AnimationTimeline => {
            let (tls, spec) = crate::anim_css::parse_animation_timeline_list(trimmed)?;
            return Some((PropertyValue::NameList(tls), spec));
        }
        PropertyIdKind::AnimationRangeStart => {
            let (pts, spec) = crate::anim_css::parse_animation_range_point_list(trimmed, false)?;
            return Some((PropertyValue::AnimationRangeList(pts), spec));
        }
        PropertyIdKind::AnimationRangeEnd => {
            let (pts, spec) = crate::anim_css::parse_animation_range_point_list(trimmed, true)?;
            return Some((PropertyValue::AnimationRangeList(pts), spec));
        }
        PropertyIdKind::AnimationRange => {
            let (starts, _, spec) = crate::anim_css::parse_animation_range_shorthand(trimmed)?;
            return Some((PropertyValue::AnimationRangeList(starts), spec));
        }
        PropertyIdKind::TextShadow => {
            let (shadows, spec) = crate::anim_css::parse_text_shadow_list(trimmed)?;
            return Some((PropertyValue::TextShadow(shadows), spec));
        }
        PropertyIdKind::TextDecorationInset => {
            let (val, spec) = crate::anim_css::parse_text_decoration_inset(trimmed)?;
            return Some((PropertyValue::TextDecorationInset(val), spec));
        }
        PropertyIdKind::VerticalAlign => {
            let (va, lp, spec) = crate::anim_css::parse_vertical_align_values4(trimmed)?;
            return Some((PropertyValue::VerticalAlignFull(va, lp), spec));
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
        p if is_sizing_property(p)
            && matches!(
                trimmed.to_ascii_lowercase().as_str(),
                "stretch" | "-webkit-fill-available" | "-moz-available"
            ) =>
        {
            return Some((
                PropertyValue::Size(SizeValue::Stretch),
                trimmed.to_ascii_lowercase(),
            ));
        }
        PropertyIdKind::FlexBasis if trimmed.to_ascii_lowercase().starts_with("fit-content(") => {
            let low = trimmed.to_ascii_lowercase();
            let inner = low
                .strip_prefix("fit-content(")
                .and_then(|s| s.strip_suffix(')'))?
                .trim();
            if inner.is_empty() || inner.eq_ignore_ascii_case("auto") {
                return None;
            }
            let pm = parse_math_value(inner, ValueKind::LengthPercentage)?;
            if !pm.was_math_fn {
                let v = pm
                    .node
                    .eval_scalar(&crate::values::EvalContext::default(), None);
                if !v.is_finite() || v < 0.0 {
                    return None;
                }
            }
            let spec_inner = pm.node.serialize_specified(pm.was_math_fn);
            let lp = LengthPercentage::Calc(Box::new(CalcExpr::Math(Box::new(pm.node))));
            return Some((
                PropertyValue::Size(SizeValue::FitContent(Some(lp))),
                format!("fit-content({spec_inner})"),
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
        PropertyIdKind::Filter | PropertyIdKind::BackdropFilter => {
            let (ops, s) = crate::values_math::parse_css_filter_list(trimmed)?;
            return Some((PropertyValue::FilterExpr(ops), s));
        }
        PropertyIdKind::ColorInterpolationFilters => {
            let mode = ColorInterpolationFiltersMode::parse_css(trimmed)?;
            return Some((
                PropertyValue::ColorInterpolationFilters(mode),
                mode.to_css_string().to_string(),
            ));
        }
        PropertyIdKind::ReadingFlow => {
            let mode = match trimmed.to_ascii_lowercase().as_str() {
                "normal" => ReadingFlowMode::Normal,
                "flex-visual" => ReadingFlowMode::FlexVisual,
                "flex-flow" => ReadingFlowMode::FlexFlow,
                "grid-rows" => ReadingFlowMode::GridRows,
                "grid-columns" => ReadingFlowMode::GridColumns,
                "grid-order" => ReadingFlowMode::GridOrder,
                "grid-lanes-flow" => ReadingFlowMode::GridLanesFlow,
                "grid-lanes-track" => ReadingFlowMode::GridLanesTrack,
                "grid-lanes-order" => ReadingFlowMode::GridLanesOrder,
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
            let (mode, s) = crate::text_css::parse_text_justify(trimmed)?;
            return Some((PropertyValue::TextJustify(mode), s.to_string()));
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
        PropertyIdKind::TextAlignAll => {
            let mode = match trimmed.to_ascii_lowercase().as_str() {
                "start" => TextAlignMode::Start,
                "end" => TextAlignMode::End,
                "left" => TextAlignMode::Left,
                "right" => TextAlignMode::Right,
                "center" => TextAlignMode::Center,
                "justify" => TextAlignMode::Justify,
                "match-parent" => TextAlignMode::MatchParent,
                _ => return None,
            };
            return Some((PropertyValue::TextAlign(mode), trimmed.to_ascii_lowercase()));
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
            if !v.is_finite() || (!pm.was_math_fn && v < 0.0) {
                return None;
            }
            let n = (v + 0.5).floor().max(0.0) as i32;
            let s = pm.node.serialize_specified(pm.was_math_fn);
            return Some((PropertyValue::HyphenateLimitLines(Some(n)), s));
        }
        PropertyIdKind::HyphenateLimitChars => {
            let v = crate::text_css::HyphenateLimitChars::parse(trimmed)?;
            let s = v.serialize();
            return Some((PropertyValue::HyphenateLimitChars(v), s));
        }
        PropertyIdKind::Hyphens => {
            let v = crate::text_css::HyphensMode::parse(trimmed)?;
            return Some((PropertyValue::Hyphens(v), v.as_str().to_string()));
        }
        PropertyIdKind::LineBreak => {
            let v = crate::text_css::LineBreakMode::parse(trimmed)?;
            return Some((PropertyValue::LineBreak(v), v.as_str().to_string()));
        }
        PropertyIdKind::WhiteSpaceCollapse => {
            let v = crate::text_css::WhiteSpaceCollapseMode::parse(trimmed)?;
            return Some((PropertyValue::WhiteSpaceCollapse(v), v.as_str().to_string()));
        }
        PropertyIdKind::WhiteSpaceTrim => {
            let v = crate::text_css::WhiteSpaceTrim::parse(trimmed)?;
            let s = v.serialize();
            return Some((PropertyValue::WhiteSpaceTrim(v), s));
        }
        PropertyIdKind::TextWrapMode => {
            let v = crate::text_css::TextWrapMode::parse(trimmed)?;
            return Some((PropertyValue::TextWrapMode(v), v.as_str().to_string()));
        }
        PropertyIdKind::TextWrapStyle => {
            let v = crate::text_css::TextWrapStyle::parse(trimmed)?;
            return Some((PropertyValue::TextWrapStyle(v), v.as_str().to_string()));
        }
        PropertyIdKind::TextAutospace => {
            let v = crate::text_css::TextAutospaceValue::parse(trimmed)?;
            let s = v.serialize();
            return Some((PropertyValue::TextAutospace(v), s));
        }
        PropertyIdKind::TextAutospaceSize => {
            let pm = parse_math_value(trimmed, ValueKind::Number)?;
            let s = pm.node.serialize_specified(pm.was_math_fn);
            return Some((PropertyValue::MathVal(Box::new(pm.node)), s));
        }
        PropertyIdKind::TextSpacingTrim => {
            let v = crate::text_css::TextSpacingTrimMode::parse(trimmed)?;
            return Some((PropertyValue::TextSpacingTrim(v), v.as_str().to_string()));
        }
        PropertyIdKind::HangingPunctuation => {
            let (v, s) = crate::text_css::HangingPunctuation::parse_with_specified(trimmed)?;
            return Some((PropertyValue::HangingPunctuation(v), s));
        }
        PropertyIdKind::TextFit => {
            let (v, s) = crate::text_css::TextFitValue::parse_with_specified(trimmed)?;
            return Some((PropertyValue::TextFit(v), s));
        }
        PropertyIdKind::TextGroupAlign => {
            let v = crate::text_css::TextGroupAlignMode::parse(trimmed)?;
            return Some((PropertyValue::TextGroupAlign(v), v.as_str().to_string()));
        }
        PropertyIdKind::WordSpaceTransform => {
            let (v, s) = crate::text_css::WordSpaceTransformMode::parse_with_specified(trimmed)?;
            return Some((PropertyValue::WordSpaceTransform(v), s));
        }
        PropertyIdKind::WrapInside => {
            let v = crate::text_css::WrapInsideMode::parse(trimmed)?;
            return Some((PropertyValue::WrapInside(v), v.as_str().to_string()));
        }
        PropertyIdKind::TextTransform => {
            let v = crate::text_css::TextTransformFlags::parse(trimmed)?;
            let s = v.serialize();
            return Some((PropertyValue::TextTransformFlags(v), s));
        }
        PropertyIdKind::WordBreak => {
            let (v, s) = crate::text_css::parse_word_break(trimmed)?;
            return Some((PropertyValue::WordBreak(v), s.to_string()));
        }
        PropertyIdKind::TextIndent => {
            let (lp, hanging, each_line, s) = crate::text_css::parse_text_indent(trimmed)?;
            return Some((PropertyValue::TextIndentFull(lp, hanging, each_line), s));
        }
        PropertyIdKind::LetterSpacing | PropertyIdKind::WordSpacing => {
            let (lp, s) = crate::text_css::parse_spacing_value(trimmed)?;
            return Some((PropertyValue::Length(lp), s));
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
        PropertyIdKind::ContainerType => {
            let lower = trimmed.to_ascii_lowercase();
            let toks: Vec<&str> = lower.split_ascii_whitespace().collect();
            if toks.is_empty() {
                return None;
            }
            if toks.len() == 1 && toks[0] == "normal" {
                return Some((PropertyValue::ContainerType(0), "normal".to_string()));
            }
            let mut bits = 0u8;
            for t in toks {
                let bit = match t {
                    "size" => 0x01,
                    "inline-size" => 0x02,
                    "scroll-state" => 0x04,
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
                parts.push("scroll-state");
            }
            return Some((PropertyValue::ContainerType(bits), parts.join(" ")));
        }
        PropertyIdKind::ContainerName => {
            let toks: Vec<&str> = trimmed.split_ascii_whitespace().collect();
            if toks.is_empty() {
                return None;
            }
            if toks.len() == 1 && toks[0].eq_ignore_ascii_case("none") {
                return Some((PropertyValue::ContainerName(Vec::new()), "none".to_string()));
            }
            let mut names = Vec::with_capacity(toks.len());
            for tok in &toks {
                let lower = tok.to_ascii_lowercase();
                if matches!(lower.as_str(), "none" | "and" | "or" | "not")
                    || GlobalKeyword::from_ident(&lower).is_some()
                {
                    return None;
                }
                names.push(intern_str(tok));
            }
            return Some((PropertyValue::ContainerName(names), toks.join(" ")));
        }
        PropertyIdKind::Container => {
            let (name_part, type_part) = if let Some((l, r)) = trimmed.split_once('/') {
                (l.trim(), Some(r.trim()))
            } else {
                (trimmed, None)
            };
            let (_, name_ser) = parse_values4_property(PropertyIdKind::ContainerName, name_part)?;
            let type_ser = if let Some(tp) = type_part {
                parse_values4_property(PropertyIdKind::ContainerType, tp)?.1
            } else {
                "normal".to_string()
            };
            let ser = if type_ser == "normal" {
                name_ser
            } else {
                format!("{name_ser} / {type_ser}")
            };
            return Some((PropertyValue::ContainerType(0), ser));
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
            if l.starts_with("light-dark(")
                || l.starts_with("image-set(")
                || l.starts_with("-webkit-image-set(")
                || l.starts_with("src(")
                || l.starts_with("image(")
                || ((l.starts_with("linear-gradient(")
                    || l.starts_with("radial-gradient(")
                    || l.starts_with("conic-gradient(")
                    || l.starts_with("repeating-linear-gradient(")
                    || l.starts_with("repeating-radial-gradient(")
                    || l.starts_with("repeating-conic-gradient("))
                    && crate::values_math::has_values4_syntax(trimmed))
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
        PropertyIdKind::ListStyleImage
        | PropertyIdKind::BorderImageSource
        | PropertyIdKind::Content => {
            let l = trimmed.to_ascii_lowercase();
            if l == "none" && prop != PropertyIdKind::Content {
                return Some((
                    PropertyValue::BackgroundImageRaw("none".to_string()),
                    "none".to_string(),
                ));
            }
            if l.starts_with("light-dark(") {
                let _ = crate::substitution::resolve_light_dark_image_fn(
                    trimmed,
                    false,
                    &crate::values::EvalContext::default(),
                    false,
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
            let mut has_auto = p.try_parse(|p| p.expect_ident_matching("auto")).is_ok();
            let mut rc = 0u32;
            let mut a = parse_math_value_from_parser(&mut p, ValueKind::Number)?;
            a.node.bind_random_property("aspect-ratio", &mut rc);
            let sa = a.node.serialize_specified(a.was_math_fn);
            let mut b_opt: Option<(Box<MathNode>, String)> = None;
            if p.try_parse(|p| p.expect_delim('/')).is_ok() {
                let mut b = parse_math_value_from_parser(&mut p, ValueKind::Number)?;
                b.node.bind_random_property("aspect-ratio", &mut rc);
                let sb = b.node.serialize_specified(b.was_math_fn);
                b_opt = Some((Box::new(b.node), sb));
            }
            if !has_auto && p.try_parse(|p| p.expect_ident_matching("auto")).is_ok() {
                has_auto = true;
            }
            if !p.is_exhausted() {
                return None;
            }
            let ratio_str = match &b_opt {
                Some((_, sb)) => format!("{sa} / {sb}"),
                None => sa,
            };
            let spec = if has_auto {
                format!("auto {ratio_str}")
            } else {
                ratio_str
            };
            return Some((
                PropertyValue::AspectRatioMath(Some((
                    has_auto,
                    Box::new(a.node),
                    b_opt.map(|(n, _)| n),
                ))),
                spec,
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
            let (list, s) = parse_background_position_list(trimmed)?;
            return Some((PropertyValue::BgPositions(list), s));
        }
        PropertyIdKind::Transform => {
            let s = parse_and_serialize_transform(trimmed, None)?;
            return Some((PropertyValue::TransformRaw(trimmed.to_string()), s));
        }
        PropertyIdKind::GridTemplateRows | PropertyIdKind::GridTemplateColumns => {
            let tl = crate::grid_css::parse_grid_template_track_list(trimmed)?;
            let s = crate::grid_css::serialize_track_list(&tl, None);
            return Some((PropertyValue::GridTemplateTracks(tl), s));
        }
        PropertyIdKind::GridTemplateAreas => {
            let areas = crate::grid_css::parse_grid_template_areas(trimmed)?;
            let s = crate::grid_css::serialize_grid_areas(&areas);
            return Some((PropertyValue::GridTemplateAreas(areas), s));
        }
        PropertyIdKind::GridAutoColumns | PropertyIdKind::GridAutoRows => {
            let tracks = crate::grid_css::parse_grid_auto_tracks(trimmed)?;
            let s = crate::grid_css::serialize_auto_tracks(&tracks, None);
            return Some((PropertyValue::GridTracks(tracks), s));
        }
        PropertyIdKind::GridAutoFlow => {
            let mode = crate::grid_css::parse_grid_auto_flow(trimmed)?;
            let s = crate::grid_css::serialize_grid_auto_flow(mode).to_string();
            return Some((PropertyValue::GridAutoFlow(mode), s));
        }
        PropertyIdKind::GridRowStart
        | PropertyIdKind::GridRowEnd
        | PropertyIdKind::GridColumnStart
        | PropertyIdKind::GridColumnEnd => {
            let gl = crate::grid_css::parse_grid_line(trimmed)?;
            let s = crate::grid_css::serialize_grid_line(&gl, None);
            return Some((PropertyValue::GridPlacement(gl), s));
        }
        PropertyIdKind::FitTolerance => {
            let ft = crate::grid_css::parse_fit_tolerance(trimmed)?;
            let s = crate::grid_css::serialize_fit_tolerance(&ft, None);
            return Some((PropertyValue::FitTolerance(ft), s));
        }
        PropertyIdKind::GridLanesDirection => {
            let dir = crate::grid_css::parse_grid_lanes_direction(trimmed)?;
            let s = crate::grid_css::serialize_grid_lanes_direction(dir);
            return Some((PropertyValue::GridLanesDirection(dir), s));
        }
        PropertyIdKind::GridLanesPack => {
            let pack = crate::grid_css::parse_grid_lanes_pack(trimmed)?;
            let s = crate::grid_css::serialize_grid_lanes_pack(pack).to_string();
            return Some((PropertyValue::GridLanesPack(pack), s));
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
            "step-start" => Some((TimingFunction::Steps(1, crate::values::StepPositionMode::Start), "steps(1, start)".to_string())),
            "step-end" => Some((TimingFunction::Steps(1, crate::values::StepPositionMode::End), "steps(1)".to_string())),
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
                let tf = TimingFunction::CubicBezier(x1, y1, x2, y2);
                let canon = tf.to_css_string();
                Ok((tf, canon))
            }).ok();
            if res.is_none() { p.reset(&st); }
            res
        }
        cssparser::Token::Function(ref fname) if fname.eq_ignore_ascii_case("linear") => {
            let res = p.parse_nested_block(|n| -> Result<(TimingFunction, String), cssparser::ParseError<'i, ()>> {
                let mut raw_stops: Vec<(f64, Option<f64>)> = Vec::new();
                loop {
                    let s = n.position();
                    let out_f = n.expect_number()?;
                    let out = n.slice_from(s).trim().parse::<f64>().unwrap_or(f64::from(out_f));
                    if !out.is_finite() { return Err(n.new_custom_error(())); }
                    let p1 = n.try_parse(|p| p.expect_percentage().map(f64::from)).ok();
                    let p2 = if p1.is_some() {
                        n.try_parse(|p| p.expect_percentage().map(f64::from)).ok()
                    } else { None };
                    raw_stops.push((out, p1));
                    if let Some(in2) = p2 {
                        raw_stops.push((out, Some(in2)));
                    }
                    if n.try_parse(cssparser::Parser::expect_comma).is_err() { break; }
                }
                if raw_stops.len() < 2 || !n.is_exhausted() { return Err(n.new_custom_error(())); }
                // W3C CSS Easing L2: normalize missing input positions & enforce monotonicity.
                if raw_stops[0].1.is_none() { raw_stops[0].1 = Some(0.0); }
                let last_i = raw_stops.len() - 1;
                if raw_stops[last_i].1.is_none() { raw_stops[last_i].1 = Some(1.0); }
                let mut max_seen = f64::NEG_INFINITY;
                for item in &mut raw_stops {
                    if let Some(v) = item.1 {
                        let clamped = v.max(max_seen);
                        max_seen = clamped;
                        item.1 = Some(clamped);
                    }
                }
                let mut prev_idx = 0usize;
                for i in 1..raw_stops.len() {
                    if let Some(end_in) = raw_stops[i].1 {
                        let start_in = raw_stops[prev_idx].1.unwrap_or(0.0);
                        let span = i - prev_idx;
                        for j in 1..span {
                            raw_stops[prev_idx + j].1 = Some(start_in + (end_in - start_in) * (j as f64 / span as f64));
                        }
                        prev_idx = i;
                    }
                }
                let stops: Box<[(f64, f64)]> = raw_stops.into_iter().map(|(out, inp)| (inp.unwrap_or(0.0), out)).collect();
                let tf = TimingFunction::LinearStops(stops);
                let canon = tf.to_css_string();
                Ok((tf, canon))
            }).ok();
            if res.is_none() { p.reset(&st); }
            res
        }
        cssparser::Token::Function(ref fname) if fname.eq_ignore_ascii_case("steps") => {
            let res = p.parse_nested_block(|n| -> Result<(TimingFunction, String), cssparser::ParseError<'i, ()>> {
                let st_cnt = n.state();
                let (cnt, spec_cnt_opt) = if let Ok(c) = n.try_parse(cssparser::Parser::expect_integer) {
                    if c < 1 { return Err(n.new_custom_error(())); }
                    (c, None)
                } else {
                    n.reset(&st_cnt);
                    let Some(pm) = crate::values_math::parse_math_value_from_parser(n, crate::values_math::ValueKind::Integer) else {
                        return Err(n.new_custom_error(()));
                    };
                    let v = pm.node.eval_scalar(&crate::values::EvalContext::default(), None);
                    let c = if v.is_finite() { v.round() as i32 } else { 1 };
                    if !pm.was_math_fn && c < 1 { return Err(n.new_custom_error(())); }
                    let spec_opt = pm.node.contains_sibling_fn().then(|| pm.node.serialize_specified(pm.was_math_fn));
                    (c.max(1), spec_opt)
                };
                let pos_mode = if n.try_parse(cssparser::Parser::expect_comma).is_ok() {
                    let pos = n.expect_ident()?.to_ascii_lowercase();
                    match pos.as_str() {
                        "start" => crate::values::StepPositionMode::Start,
                        "jump-start" => crate::values::StepPositionMode::JumpStart,
                        "jump-both" => crate::values::StepPositionMode::JumpBoth,
                        "end" => crate::values::StepPositionMode::End,
                        "jump-end" => crate::values::StepPositionMode::JumpEnd,
                        "jump-none" => {
                            if spec_cnt_opt.is_none() && cnt < 2 { return Err(n.new_custom_error(())); }
                            crate::values::StepPositionMode::JumpNone
                        }
                        _ => return Err(n.new_custom_error(())),
                    }
                } else { crate::values::StepPositionMode::End };
                if !n.is_exhausted() { return Err(n.new_custom_error(())); }
                let tf = TimingFunction::Steps(cnt as u32, pos_mode);
                let canon = if let Some(sc) = spec_cnt_opt {
                    match pos_mode {
                        crate::values::StepPositionMode::End | crate::values::StepPositionMode::JumpEnd => format!("steps({sc})"),
                        crate::values::StepPositionMode::Start => format!("steps({sc}, start)"),
                        crate::values::StepPositionMode::JumpStart => format!("steps({sc}, jump-start)"),
                        crate::values::StepPositionMode::JumpNone => format!("steps({sc}, jump-none)"),
                        crate::values::StepPositionMode::JumpBoth => format!("steps({sc}, jump-both)"),
                    }
                } else {
                    tf.to_css_string()
                };
                Ok((tf, canon))
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
                if prop.is_none() && GlobalKeyword::from_ident(&lower).is_none() && lower != "default" {
                    let stored = if lower == "all" || lower == "none" || PropertyIdKind::from_name(&lower).is_some() {
                        lower
                    } else {
                        crate::substitution::escape_css_ident(&ident)
                    };
                    prop = Some(intern_str(&stored));
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
    if !out.is_empty() && p.is_exhausted() {
        let ser = crate::anim_css::serialize_transition_list(&out);
        Some((out, ser))
    } else {
        None
    }
}

pub(crate) fn parse_transition_shorthand_pub(input: &str) -> Option<(Vec<TransitionSpec>, String)> {
    parse_transition_shorthand(input)
}

fn parse_animation_shorthand(input: &str) -> Option<(Vec<AnimationSpec>, String)> {
    crate::anim_css::parse_animation_shorthand_full(input)
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
                            | PropertyIdKind::ListStyleImage
                            | PropertyIdKind::BorderImageSource
                            | PropertyIdKind::Content
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

/// Preprocesses author CSS for Quirks Mode Living Standard §3.1 (hashless hex colors) and §3.2
/// (unitless lengths) when `quirks_mode` is true, AND enforces CSS Values L3 §5.1 strict length
/// validation (rejecting non-zero unitless numbers on length properties where `lightningcss`
/// unconditionally coerces bare numbers to `px`) in both No-Quirks/Limited-Quirks mode and on
/// excluded properties in Quirks Mode.
#[must_use]
pub(crate) fn preprocess_quirks_and_strict_lengths(
    css: &str,
    quirks_mode: bool,
) -> std::borrow::Cow<'_, str> {
    fn is_hashless_hex_prop(name: &str) -> bool {
        matches!(
            name,
            "background-color"
                | "border-color"
                | "border-top-color"
                | "border-right-color"
                | "border-bottom-color"
                | "border-left-color"
                | "color"
        )
    }
    fn is_quirky_len_prop(name: &str) -> bool {
        matches!(
            name,
            "background-position"
                | "border-spacing"
                | "border-top-width"
                | "border-right-width"
                | "border-bottom-width"
                | "border-left-width"
                | "border-width"
                | "bottom"
                | "clip"
                | "font-size"
                | "height"
                | "left"
                | "letter-spacing"
                | "margin-right"
                | "margin-left"
                | "margin-top"
                | "margin-bottom"
                | "margin"
                | "max-height"
                | "max-width"
                | "min-height"
                | "min-width"
                | "padding-top"
                | "padding-right"
                | "padding-bottom"
                | "padding-left"
                | "padding"
                | "right"
                | "text-indent"
                | "top"
                | "vertical-align"
                | "width"
                | "word-spacing"
        )
    }
    fn is_strict_non_quirky_len_prop(name: &str) -> bool {
        matches!(
            name,
            "outline-width"
                | "outline-offset"
                | "outline"
                | "border"
                | "border-top"
                | "border-right"
                | "border-bottom"
                | "border-left"
                | "background"
                | "font"
                | "inset"
                | "inset-block"
                | "inset-block-start"
                | "inset-block-end"
                | "inset-inline"
                | "inset-inline-start"
                | "inset-inline-end"
                | "margin-block"
                | "margin-block-start"
                | "margin-block-end"
                | "margin-inline"
                | "margin-inline-start"
                | "margin-inline-end"
                | "padding-block"
                | "padding-block-start"
                | "padding-block-end"
                | "padding-inline"
                | "padding-inline-start"
                | "padding-inline-end"
                | "inline-size"
                | "block-size"
                | "min-inline-size"
                | "max-inline-size"
                | "min-block-size"
                | "max-block-size"
                | "row-gap"
                | "column-gap"
                | "gap"
                | "border-radius"
                | "border-top-left-radius"
                | "border-top-right-radius"
                | "border-bottom-right-radius"
                | "border-bottom-left-radius"
        )
    }
    fn unescape_css_token(raw: &str) -> Option<String> {
        let bytes = raw.as_bytes();
        let mut out = String::with_capacity(raw.len());
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] == b'\\' {
                i += 1;
                if i >= bytes.len() {
                    return None;
                }
                if bytes[i].is_ascii_hexdigit() {
                    let start = i;
                    let mut count = 0;
                    while i < bytes.len() && count < 6 && bytes[i].is_ascii_hexdigit() {
                        i += 1;
                        count += 1;
                    }
                    let cp = u32::from_str_radix(&raw[start..i], 16).ok()?;
                    let ch = char::from_u32(cp)?;
                    out.push(ch);
                    // Consume single optional trailing whitespace per CSS Syntax §4.3.7.
                    if i < bytes.len() {
                        if bytes[i] == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                            i += 2;
                        } else if matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r' | b'\x0c') {
                            i += 1;
                        }
                    }
                } else {
                    let ch = raw[i..].chars().next()?;
                    out.push(ch);
                    i += ch.len_utf8();
                }
            } else {
                let ch = raw[i..].chars().next()?;
                out.push(ch);
                i += ch.len_utf8();
            }
        }
        Some(out)
    }
    /// Exact Quirks Mode Living Standard §3.1 hashless hex color parser for a single component token.
    fn parse_hashless_hex_token(tok: &str) -> Option<String> {
        if tok.is_empty() || tok.contains("/*") || tok.starts_with('-') || tok.starts_with('#') {
            return None;
        }
        let first = tok.as_bytes()[0];
        if first == b'+' || first.is_ascii_digit() {
            // `<number-token>` or `<dimension-token>`: must be a non-negative integer followed by
            // optional hex-only ident (possibly with CSS escapes in the unit part, e.g. `12\33 `),
            // total 1..=6 hex digits, left-padded with '0' to 6 digits!
            // Reject CSS scientific notation (`<digits>[eE][+-]?<digit>`, e.g. `1e1`, `11e1a`),
            // which CSS Syntax §4.3.3 tokenizes with `type_flag = "number"` (not `"integer"`).
            let rest = tok.strip_prefix('+').unwrap_or(tok);
            if rest.is_empty() || !rest.as_bytes()[0].is_ascii_digit() {
                return None;
            }
            {
                let rb = rest.as_bytes();
                let mut d = 0;
                while d < rb.len() && rb[d].is_ascii_digit() {
                    d += 1;
                }
                if d < rb.len() && (rb[d] == b'e' || rb[d] == b'E') {
                    let after_e = if d + 1 < rb.len() && (rb[d + 1] == b'+' || rb[d + 1] == b'-') {
                        d + 2
                    } else {
                        d + 1
                    };
                    if after_e < rb.len() && rb[after_e].is_ascii_digit() {
                        return None;
                    }
                }
            }
            let unesc = unescape_css_token(rest)?;
            if unesc.is_empty() || unesc.len() > 6 || !unesc.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return None;
            }
            Some(format!("#{unesc:0>6}"))
        } else if first == b'\\' || first.is_ascii_alphabetic() {
            // `<ident-token>` (including escaped leading digit like `\31 23`):
            // valid ONLY if unescaped string has length 3 or 6 and all ASCII hex digits!
            let unesc = unescape_css_token(tok)?;
            if (unesc.len() == 3 || unesc.len() == 6)
                && unesc.bytes().all(|b| b.is_ascii_hexdigit())
            {
                Some(format!("#{unesc}"))
            } else {
                None
            }
        } else {
            None
        }
    }
    fn parse_bare_number(tok: &str) -> Option<f64> {
        if tok.contains("/*") || tok.contains('\\') {
            return None;
        }
        let s = tok
            .strip_prefix('+')
            .or_else(|| tok.strip_prefix('-'))
            .unwrap_or(tok);
        if s.is_empty() || s == "." {
            return None;
        }
        let mut saw_dot = false;
        let mut saw_digit = false;
        for b in s.bytes() {
            if b.is_ascii_digit() {
                saw_digit = true;
            } else if b == b'.' && !saw_dot {
                saw_dot = true;
            } else {
                return None;
            }
        }
        if !saw_digit {
            return None;
        }
        tok.parse::<f64>().ok()
    }
    /// Splits `s` into top-level whitespace-separated tokens, keeping CSS escape sequences like
    /// `\31 23` together as a single token and ignoring content inside `(...)`.
    fn split_css_value_tokens(s: &str) -> Vec<&str> {
        let bytes = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < bytes.len() {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            let start = i;
            let mut paren = 0usize;
            while i < bytes.len() {
                let b = bytes[i];
                if b == b'\\' && i + 1 < bytes.len() {
                    i += 1;
                    if bytes[i].is_ascii_hexdigit() {
                        let mut c = 0;
                        while i < bytes.len() && c < 6 && bytes[i].is_ascii_hexdigit() {
                            i += 1;
                            c += 1;
                        }
                        if i < bytes.len() && bytes[i].is_ascii_whitespace() {
                            i += 1;
                        }
                    } else {
                        i += s[i..].chars().next().map_or(1, char::len_utf8);
                    }
                    continue;
                }
                if b == b'(' {
                    paren += 1;
                } else if b == b')' {
                    paren = paren.saturating_sub(1);
                } else if paren == 0 && b.is_ascii_whitespace() {
                    break;
                }
                i += s[i..].chars().next().map_or(1, char::len_utf8);
            }
            out.push(&s[start..i]);
        }
        out
    }
    fn has_malformed_numeric_escape_or_comment(tok: &str) -> bool {
        if tok.starts_with('#') || tok.starts_with('"') || tok.starts_with('\'') {
            return false;
        }
        // Reject unitless `calc(1)` / `calc(2)` on pure-length properties (CSS Values L3 §8.1.2:
        // a calc() expression with no length/percentage unit resolves to `<number>`, not `<length>`).
        if tok.len() >= 6
            && tok
                .get(..5)
                .is_some_and(|p| p.eq_ignore_ascii_case("calc("))
            && tok.ends_with(')')
        {
            let inner = &tok[5..tok.len() - 1];
            let has_unit_or_ident = inner.bytes().any(|b| b.is_ascii_alphabetic() || b == b'%');
            let has_digit = inner.bytes().any(|b| b.is_ascii_digit());
            if has_digit && !has_unit_or_ident {
                return true;
            }
            return false;
        }
        if tok.contains('(') {
            return false;
        }
        // Catch comment-split signs like `+/**/1` / `-/**/1` and escaped-digit dimensions like `1\31 `
        // or `\31 ` that lightningcss would mis-parse as valid pixel lengths.
        if tok.contains("/*") {
            let stripped = strip_css_comments_inline(tok);
            if parse_bare_number(stripped.trim()).is_some() {
                return true;
            }
        }
        if tok.contains('\\') {
            let s = tok
                .strip_prefix('+')
                .or_else(|| tok.strip_prefix('-'))
                .unwrap_or(tok);
            if s.starts_with('\\') || s.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
                return true;
            }
        }
        false
    }
    fn strip_css_comments_inline(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            } else if let Some(ch) = s[i..].chars().next() {
                out.push(ch);
                i += ch.len_utf8();
            } else {
                break;
            }
        }
        out
    }
    fn has_nonzero_bare_number_outside_parens(body: &str) -> bool {
        split_css_value_tokens(body).into_iter().any(|t| {
            has_malformed_numeric_escape_or_comment(t)
                || (!t.contains('(') && parse_bare_number(t).is_some_and(|n| n != 0.0))
        })
    }
    fn font_shorthand_has_unitless_size(body: &str) -> bool {
        let toks = split_css_value_tokens(body);
        let mut i = 0usize;
        while i < toks.len() {
            let low = toks[i].to_ascii_lowercase();
            if matches!(
                low.as_str(),
                "normal"
                    | "italic"
                    | "oblique"
                    | "small-caps"
                    | "bold"
                    | "bolder"
                    | "lighter"
                    | "ultra-condensed"
                    | "extra-condensed"
                    | "condensed"
                    | "semi-condensed"
                    | "semi-expanded"
                    | "expanded"
                    | "extra-expanded"
                    | "ultra-expanded"
            ) {
                i += 1;
                continue;
            }
            // Only the first numeric token before a size-with-unit could be a font-weight;
            // if a bare non-zero number is immediately followed by a non-size font-family token,
            // it sits in the `font-size` slot and is invalid!
            if parse_bare_number(toks[i]).is_some_and(|n| n != 0.0) {
                let next_is_size = toks.get(i + 1).is_some_and(|nt| {
                    let nl = nt.to_ascii_lowercase();
                    nt.as_bytes()
                        .first()
                        .is_some_and(|b| b.is_ascii_digit() || *b == b'.')
                        || matches!(
                            nl.as_str(),
                            "xx-small"
                                | "x-small"
                                | "small"
                                | "medium"
                                | "large"
                                | "x-large"
                                | "xx-large"
                                | "xxx-large"
                                | "smaller"
                                | "larger"
                        )
                });
                if !next_is_size {
                    return true;
                }
            }
            break;
        }
        false
    }

    let bytes = css.as_bytes();
    let mut out = String::with_capacity(css.len() + 16);
    let mut changed = false;
    let mut i = 0usize;
    let mut in_str: Option<u8> = None;
    let mut in_supports_cond = false;
    let mut paren_depth = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            out.push(b as char);
            if b == b'\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if b == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let start = i;
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            out.push_str(&css[start..i]);
            continue;
        }
        if b == b'"' || b == b'\'' {
            in_str = Some(b);
            out.push(b as char);
            i += 1;
            continue;
        }
        if b == b'@' && css[i..].len() >= 9 && css[i..i + 9].eq_ignore_ascii_case("@supports") {
            in_supports_cond = true;
        }
        if b == b'{' {
            in_supports_cond = false;
            out.push('{');
            i += 1;
            continue;
        }
        if b == b'(' {
            paren_depth += 1;
            out.push('(');
            i += 1;
            continue;
        }
        if b == b')' {
            paren_depth = paren_depth.saturating_sub(1);
            out.push(')');
            i += 1;
            continue;
        }
        // Scan `<prop-name> : <value>` declaration at top-level or inside `@supports (...)`.
        let allow_decl_here = paren_depth == 0 || in_supports_cond;
        if allow_decl_here
            && (b.is_ascii_alphabetic() || b == b'-')
            && (i == 0
                || matches!(
                    bytes[i - 1],
                    b'{' | b';' | b'(' | b' ' | b'\t' | b'\n' | b'\r' | b'/'
                ))
        {
            let name_start = i;
            let mut k = i;
            while k < bytes.len() && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'-') {
                k += 1;
            }
            let prop_raw = &css[name_start..k];
            let mut colon = k;
            while colon < bytes.len() && bytes[colon].is_ascii_whitespace() {
                colon += 1;
            }
            if colon < bytes.len() && bytes[colon] == b':' {
                let prop_lower = prop_raw.to_ascii_lowercase();
                let want_hex = quirks_mode && is_hashless_hex_prop(&prop_lower);
                let is_quirky_len = is_quirky_len_prop(&prop_lower);
                let is_strict_other = is_strict_non_quirky_len_prop(&prop_lower);
                if want_hex || is_quirky_len || is_strict_other {
                    // Find end of declaration value (`;`, `}`, unmatched `)` in `@supports`, or EOF).
                    let val_start = colon + 1;
                    let mut v = val_start;
                    let mut p = 0usize;
                    let mut vs: Option<u8> = None;
                    while v < bytes.len() {
                        let ch = bytes[v];
                        if let Some(q) = vs {
                            if ch == b'\\' {
                                v += 2;
                                continue;
                            }
                            if ch == q {
                                vs = None;
                            }
                            v += 1;
                            continue;
                        }
                        if ch == b'"' || ch == b'\'' {
                            vs = Some(ch);
                        } else if ch == b'(' {
                            p += 1;
                        } else if ch == b')' {
                            if p == 0 {
                                break;
                            }
                            p -= 1;
                        } else if p == 0 && (ch == b';' || ch == b'}') {
                            break;
                        }
                        v += 1;
                    }
                    let raw_val = &css[val_start..v];
                    let (body, imp_tail) = if let Some(bang) = raw_val.rfind('!') {
                        (&raw_val[..bang], &raw_val[bang..])
                    } else {
                        (raw_val, "")
                    };
                    let trimmed_body = body.trim();
                    let mut replacement: Option<String> = None;
                    let mut invalidate = false;

                    if want_hex {
                        if prop_lower == "border-color" {
                            let parts = split_css_value_tokens(trimmed_body);
                            if !parts.is_empty() && parts.len() <= 4 {
                                let mut any = false;
                                let mut rebuilt = Vec::with_capacity(parts.len());
                                for t in parts {
                                    if let Some(hex) = parse_hashless_hex_token(t) {
                                        any = true;
                                        rebuilt.push(hex);
                                    } else {
                                        rebuilt.push(t.to_string());
                                    }
                                }
                                if any {
                                    replacement = Some(rebuilt.join(" "));
                                }
                            }
                        } else {
                            let parts = split_css_value_tokens(trimmed_body);
                            if parts.len() == 1 {
                                if let Some(hex) = parse_hashless_hex_token(parts[0]) {
                                    replacement = Some(hex);
                                }
                            }
                        }
                    } else if prop_lower == "clip" {
                        let low = trimmed_body.to_ascii_lowercase();
                        if low.starts_with("rect(") && trimmed_body.ends_with(')') {
                            let inner = &trimmed_body[5..trimmed_body.len() - 1];
                            let sep = if inner.contains(',') { ',' } else { ' ' };
                            let parts: Vec<&str> = if sep == ',' {
                                inner.split(',').map(str::trim).collect()
                            } else {
                                split_css_value_tokens(inner)
                            };
                            if parts.len() == 4 {
                                let has_bad = parts
                                    .iter()
                                    .any(|t| has_malformed_numeric_escape_or_comment(t));
                                let has_nz = parts
                                    .iter()
                                    .any(|t| parse_bare_number(t).is_some_and(|n| n != 0.0));
                                if has_bad {
                                    invalidate = true;
                                } else if has_nz {
                                    if quirks_mode {
                                        let fixed: Vec<String> = parts
                                            .iter()
                                            .map(|t| {
                                                if parse_bare_number(t).is_some_and(|n| n != 0.0) {
                                                    format!("{t}px")
                                                } else {
                                                    (*t).to_string()
                                                }
                                            })
                                            .collect();
                                        let glue = if sep == ',' { ", " } else { " " };
                                        replacement = Some(format!("rect({})", fixed.join(glue)));
                                    } else {
                                        invalidate = true;
                                    }
                                }
                            }
                        }
                    } else if is_quirky_len {
                        let parts = split_css_value_tokens(trimmed_body);
                        let has_bad = parts
                            .iter()
                            .any(|t| has_malformed_numeric_escape_or_comment(t));
                        let has_nz = parts.iter().any(|t| {
                            !t.contains('(') && parse_bare_number(t).is_some_and(|n| n != 0.0)
                        });
                        if has_bad {
                            invalidate = true;
                        } else if has_nz {
                            if quirks_mode {
                                let fixed: Vec<String> = parts
                                    .into_iter()
                                    .map(|t| {
                                        if !t.contains('(')
                                            && parse_bare_number(t).is_some_and(|n| n != 0.0)
                                        {
                                            format!("{t}px")
                                        } else {
                                            t.to_string()
                                        }
                                    })
                                    .collect();
                                replacement = Some(fixed.join(" "));
                            } else {
                                invalidate = true;
                            }
                        }
                    } else if is_strict_other {
                        // Excluded from Quirks §3.2 in BOTH quirks and non-quirks mode.
                        let bad = if prop_lower == "font" {
                            font_shorthand_has_unitless_size(trimmed_body)
                        } else {
                            has_nonzero_bare_number_outside_parens(trimmed_body)
                        };
                        if bad {
                            invalidate = true;
                        }
                    }

                    if invalidate {
                        changed = true;
                        out.push_str(&css[name_start..=colon]);
                        out.push_str(" __rix_invalid_unitless__");
                        i = v;
                        continue;
                    }
                    if let Some(new_val) = replacement {
                        changed = true;
                        out.push_str(&css[name_start..=colon]);
                        out.push(' ');
                        out.push_str(&new_val);
                        if !imp_tail.is_empty() {
                            out.push(' ');
                            out.push_str(imp_tail.trim());
                        }
                        i = v;
                        continue;
                    }
                }
            }
        }
        let ch = css[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    if changed {
        std::borrow::Cow::Owned(out)
    } else {
        std::borrow::Cow::Borrowed(css)
    }
}

/// Parses an HTML `style="..."` attribute in Quirks Mode (applying Quirks Mode Living Standard
/// §3.1 hashless hex colors and §3.2 unitless lengths before standard CSS parsing).
#[must_use]
pub fn parse_inline_style_quirks(
    style_attr: &str,
    missing: &mut Vec<MissingCssFeature>,
) -> Vec<SpecifiedDeclaration> {
    let normalized = preprocess_quirks_and_strict_lengths(style_attr, true);
    parse_inline_style_raw(&normalized, missing)
}

/// Parses an HTML `style="..."` attribute into typed [`SpecifiedDeclaration`]s using a
/// lock-free thread-local cache (zero-allocation lookup on repeated React/framework inline styles).
#[must_use]
pub fn parse_inline_style(
    style_attr: &str,
    missing: &mut Vec<MissingCssFeature>,
) -> Vec<SpecifiedDeclaration> {
    let normalized = preprocess_quirks_and_strict_lengths(style_attr, false);
    parse_inline_style_raw(&normalized, missing)
}

fn parse_inline_style_raw(
    style_attr: &str,
    missing: &mut Vec<MissingCssFeature>,
) -> Vec<SpecifiedDeclaration> {
    let trimmed = style_attr.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if trimmed.len() <= 512 {
        let hash = fnv1a_str_hash(0xcbf2_9ce4_8422_2325, trimmed);
        let cached = INLINE_STYLE_PARSE_CACHE.with(|cell| {
            let map = cell.borrow();
            map.get(&hash).and_then(|(k, decls, miss)| {
                if k.as_ref() == trimmed {
                    Some((decls.clone(), miss.clone()))
                } else {
                    None
                }
            })
        });
        if let Some((decls, miss)) = cached {
            missing.extend(miss);
            return decls;
        }
        let mut local_missing = Vec::new();
        let decls = parse_inline_style_uncached(trimmed, &mut local_missing);
        INLINE_STYLE_PARSE_CACHE.with(|cell| {
            let mut map = cell.borrow_mut();
            if map.len() >= 2_048 {
                map.clear();
            }
            map.insert(
                hash,
                (
                    trimmed.to_string().into_boxed_str(),
                    decls.clone(),
                    local_missing.clone(),
                ),
            );
        });
        missing.extend(local_missing);
        return decls;
    }
    parse_inline_style_uncached(trimmed, missing)
}

fn parse_inline_style_uncached(
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

#[inline]
fn fnv1a_str_hash(mut h: u64, s: &str) -> u64 {
    for &b in s.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

type PropParseCacheMap =
    crate::rule_index::FastU64Map<(PropertyIdKind, Box<str>, Vec<SpecifiedDeclaration>)>;
type InlineStyleCacheMap =
    crate::rule_index::FastU64Map<(Box<str>, Vec<SpecifiedDeclaration>, Vec<MissingCssFeature>)>;

thread_local! {
    static PROP_PARSE_CACHE: std::cell::RefCell<PropParseCacheMap> =
        std::cell::RefCell::new(crate::rule_index::FastU64Map::default());
    static INLINE_STYLE_PARSE_CACHE: std::cell::RefCell<InlineStyleCacheMap> =
        std::cell::RefCell::new(crate::rule_index::FastU64Map::default());
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
/// intermediate token round-trip (preserving wide-gamut colors, `image-set()`, and `/**/` boundaries),
/// backed by a lock-free thread-local parse cache for repeated `var()` expansions.
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
    if trimmed.len() <= 512 {
        let seed = (prop as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0xcbf2_9ce4_8422_2325;
        let hash = fnv1a_str_hash(seed, trimmed);
        let hit = PROP_PARSE_CACHE.with(|cell| {
            let map = cell.borrow();
            map.get(&hash).and_then(|(kp, ks, decls)| {
                if *kp == prop && ks.as_ref() == trimmed {
                    Some(decls.clone())
                } else {
                    None
                }
            })
        });
        if let Some(mut decls) = hit {
            if important {
                for d in &mut decls {
                    d.important = true;
                }
            }
            return decls;
        }
        let mut decls = parse_property_from_str_uncached(prop, trimmed, false);
        PROP_PARSE_CACHE.with(|cell| {
            let mut map = cell.borrow_mut();
            if map.len() >= 4_096 {
                map.clear();
            }
            map.insert(
                hash,
                (prop, trimmed.to_string().into_boxed_str(), decls.clone()),
            );
        });
        if important {
            for d in &mut decls {
                d.important = true;
            }
        }
        return decls;
    }
    parse_property_from_str_uncached(prop, trimmed, important)
}

fn parse_property_from_str_uncached(
    prop: PropertyIdKind,
    trimmed: &str,
    important: bool,
) -> Vec<SpecifiedDeclaration> {
    if prop == PropertyIdKind::Appearance {
        if let Some(mode) = AppearanceMode::parse_css(trimmed) {
            return vec![SpecifiedDeclaration {
                property: prop,
                custom_name: None,
                value: PropertyValue::Appearance(mode),
                important,
                order: 0,
            }];
        }
        return Vec::new();
    }
    if prop == PropertyIdKind::AccentColor {
        if trimmed.eq_ignore_ascii_case("auto") {
            return vec![SpecifiedDeclaration {
                property: prop,
                custom_name: None,
                value: PropertyValue::AccentColor(None),
                important,
                order: 0,
            }];
        }
        if let Some(expr) = crate::color::parse_css_color(trimmed) {
            let col = if matches!(expr, crate::color::CssColorExpr::CurrentColor) {
                CssColor::CurrentColor
            } else {
                let resolved = expr.resolve(
                    crate::color::ResolvedColor::from_legacy_f64(0.0, 0.0, 0.0, 1.0),
                    false,
                    &crate::values::EvalContext::default(),
                );
                CssColor::Rgba(resolved.to_srgb_u8())
            };
            return vec![SpecifiedDeclaration {
                property: prop,
                custom_name: None,
                value: PropertyValue::AccentColor(Some(col)),
                important,
                order: 0,
            }];
        }
        return Vec::new();
    }
    if matches!(prop, PropertyIdKind::SvgFill | PropertyIdKind::SvgStroke) {
        if trimmed.eq_ignore_ascii_case("none") {
            return vec![SpecifiedDeclaration {
                property: prop,
                custom_name: None,
                value: PropertyValue::SvgPaint(None),
                important,
                order: 0,
            }];
        }
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
    if prop == PropertyIdKind::SvgFillRule {
        let even_odd = if trimmed.eq_ignore_ascii_case("evenodd") {
            Some(true)
        } else if trimmed.eq_ignore_ascii_case("nonzero") {
            Some(false)
        } else {
            None
        };
        if let Some(v) = even_odd {
            return vec![SpecifiedDeclaration {
                property: prop,
                custom_name: None,
                value: PropertyValue::SvgFillRuleEvenOdd(v),
                important,
                order: 0,
            }];
        }
        return Vec::new();
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
        let fn_prefix = [
            "image-set(",
            "-webkit-image-set(",
            "src(",
            "image(",
            "linear-gradient(",
            "radial-gradient(",
            "conic-gradient(",
            "repeating-linear-gradient(",
            "repeating-radial-gradient(",
            "repeating-conic-gradient(",
        ]
        .into_iter()
        .find(|p| lower.starts_with(p));
        let is_gradient_pfx = fn_prefix.is_some_and(|p| p.contains("gradient("));
        let fn_prefix = if is_gradient_pfx && !crate::values_math::has_values4_syntax(trimmed) {
            None
        } else {
            fn_prefix
        };
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
        let mut out = Vec::new();
        let mut order = 0u32;
        if parse_values4_property_decls(prop, trimmed, important, &mut order, &mut out) {
            return out;
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
        Property::BorderSpacing(bs) => {
            matches!(&bs.0, Length::Value(lv) if convert_length_value(lv).is_negative_literal())
                || matches!(&bs.1, Length::Value(lv) if convert_length_value(lv).is_negative_literal())
        }
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

/// Converts a `lightningcss` color into a [`PropertyValue`], preserving CSS Color 4
/// wide-gamut/perceptual spaces (`oklch()`, `oklab()`, `lab()`, `lch()`, `color(...)`)
/// as [`PropertyValue::Color4`] per CSS Color 4 §14.1 instead of eagerly collapsing them
/// to legacy sRGB `rgb(...)` or dropping colors whose `.to_rgb()` fails.
fn convert_color_prop(c: &lightningcss::values::color::CssColor) -> Option<PropertyValue> {
    use lightningcss::values::color::CssColor as LcColor;
    match c {
        LcColor::CurrentColor | LcColor::RGBA(_) | LcColor::System(_) => {
            convert_color(c).map(PropertyValue::Color)
        }
        other => {
            if let Ok(css_str) = lightningcss::traits::ToCss::to_css_string(
                other,
                lightningcss::printer::PrinterOptions::default(),
            ) {
                if let Some(expr) = crate::color::parse_css_color(&css_str) {
                    return Some(PropertyValue::Color4(Box::new(expr)));
                }
            }
            convert_color(other).map(PropertyValue::Color)
        }
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
            let val = if c.images.is_empty() {
                PropertyValue::Cursor(convert_cursor(c))
            } else {
                let imgs = c
                    .images
                    .iter()
                    .map(|img| (intern_str(img.url.url.as_ref()), img.hotspot))
                    .collect();
                PropertyValue::CursorWithUrls(imgs, convert_cursor(c))
            };
            push_decl(out, ord, PropertyIdKind::Cursor, val, imp);
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
            if let Some(val) = convert_color_prop(c) {
                push_decl(out, ord, PropertyIdKind::Color, val, imp);
            }
        }
        Property::BackgroundColor(c) => {
            if let Some(val) = convert_color_prop(c) {
                push_decl(out, ord, PropertyIdKind::BackgroundColor, val, imp);
            }
        }
        Property::Background(bgs) => {
            if let Some(last) = bgs.last() {
                if let Some(val) = convert_color_prop(&last.color) {
                    push_decl(out, ord, PropertyIdKind::BackgroundColor, val, imp);
                }
            }
            let layers: Vec<BackgroundLayerValue> =
                bgs.iter().map(convert_background_layer).collect();
            let sizes: Vec<BackgroundSizeValue> = layers.iter().map(|l| l.size.clone()).collect();
            let positions: Vec<(LengthPercentage, LengthPercentage)> = layers
                .iter()
                .map(|l| (l.pos_x.clone(), l.pos_y.clone()))
                .collect();
            let repeats: Vec<BackgroundRepeatPair> = bgs
                .iter()
                .map(|b| convert_background_repeat(&b.repeat))
                .collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::BackgroundLayers,
                PropertyValue::BackgroundLayers(layers),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BackgroundSize,
                PropertyValue::BackgroundSizes(sizes),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BackgroundPosition,
                PropertyValue::BgPositions(positions),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::BackgroundRepeat,
                PropertyValue::BackgroundRepeats(repeats),
                imp,
            );
            let first_clip = bgs
                .first()
                .map(|b| convert_background_clip(&b.clip))
                .unwrap_or(BackgroundClipMode::BorderBox);
            push_decl(
                out,
                ord,
                PropertyIdKind::BackgroundClip,
                PropertyValue::BackgroundClip(first_clip),
                imp,
            );
        }
        Property::BackgroundRepeat(reps) => {
            let list: Vec<BackgroundRepeatPair> =
                reps.iter().map(convert_background_repeat).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::BackgroundRepeat,
                PropertyValue::BackgroundRepeats(if list.is_empty() {
                    vec![BackgroundRepeatPair::default()]
                } else {
                    list
                }),
                imp,
            );
        }
        Property::BackgroundSize(sizes) => {
            let converted: Vec<BackgroundSizeValue> =
                sizes.iter().map(convert_background_size).collect();
            push_decl(
                out,
                ord,
                PropertyIdKind::BackgroundSize,
                PropertyValue::BackgroundSizes(if converted.is_empty() {
                    vec![BackgroundSizeValue::Auto]
                } else {
                    converted
                }),
                imp,
            );
        }
        Property::BackgroundClip(clips, _) => {
            let clip = clips
                .first()
                .map(convert_background_clip)
                .unwrap_or(BackgroundClipMode::BorderBox);
            push_decl(
                out,
                ord,
                PropertyIdKind::BackgroundClip,
                PropertyValue::BackgroundClip(clip),
                imp,
            );
        }
        Property::BackgroundOrigin(origs) => {
            use lightningcss::properties::background::BackgroundOrigin as Bo;
            let orig = match origs.first() {
                Some(Bo::BorderBox) => BackgroundClipMode::BorderBox,
                Some(Bo::ContentBox) => BackgroundClipMode::ContentBox,
                _ => BackgroundClipMode::PaddingBox,
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::BackgroundOrigin,
                PropertyValue::BackgroundClip(orig),
                imp,
            );
        }
        Property::BackgroundAttachment(atts) => {
            use lightningcss::properties::background::BackgroundAttachment as Ba;
            let att = match atts.first() {
                Some(Ba::Fixed) => "fixed",
                Some(Ba::Local) => "local",
                _ => "scroll",
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::BackgroundAttachment,
                PropertyValue::BackgroundAttachment(intern_str(att)),
                imp,
            );
        }
        Property::BorderImageSource(img) => {
            let raw = match img {
                lightningcss::values::image::Image::None => "none".to_string(),
                other => lightningcss::traits::ToCss::to_css_string(
                    other,
                    lightningcss::printer::PrinterOptions::default(),
                )
                .unwrap_or_else(|_| "none".to_string()),
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::BorderImageSource,
                PropertyValue::BackgroundImageRaw(raw),
                imp,
            );
        }
        Property::ListStyleImage(img) => {
            let raw = match img {
                lightningcss::values::image::Image::None => "none".to_string(),
                other => lightningcss::traits::ToCss::to_css_string(
                    other,
                    lightningcss::printer::PrinterOptions::default(),
                )
                .unwrap_or_else(|_| "none".to_string()),
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::ListStyleImage,
                PropertyValue::BackgroundImageRaw(raw),
                imp,
            );
        }
        Property::CaretColor(ca) => match ca {
            lightningcss::properties::ui::ColorOrAuto::Auto => {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::CaretColor,
                    PropertyValue::Color(CssColor::CurrentColor),
                    imp,
                );
            }
            lightningcss::properties::ui::ColorOrAuto::Color(c) => {
                if let Some(v) = convert_color_prop(c) {
                    push_decl(out, ord, PropertyIdKind::CaretColor, v, imp);
                }
            }
        },
        Property::PerspectiveOrigin(pos, _) => {
            let x = convert_bg_horiz_pos(&pos.x);
            let y = convert_bg_vert_pos(&pos.y);
            push_decl(
                out,
                ord,
                PropertyIdKind::PerspectiveOrigin,
                PropertyValue::BgPosition(x, y),
                imp,
            );
        }
        Property::ClipRule(rule) => {
            let even_odd = matches!(rule, lightningcss::values::shape::FillRule::Evenodd);
            push_decl(
                out,
                ord,
                PropertyIdKind::SvgClipRule,
                PropertyValue::SvgFillRuleEvenOdd(even_odd),
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
        Property::BackgroundPosition(positions) => {
            let list: Vec<(LengthPercentage, LengthPercentage)> = positions
                .iter()
                .map(|pos| (convert_bg_horiz_pos(&pos.x), convert_bg_vert_pos(&pos.y)))
                .collect();
            if !list.is_empty() {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BackgroundPosition,
                    PropertyValue::BgPositions(list),
                    imp,
                );
            }
        }
        Property::BackgroundPositionX(xs) => {
            if let Some(x) = xs.first() {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BackgroundPositionX,
                    PropertyValue::Length(convert_bg_horiz_pos(x)),
                    imp,
                );
            }
        }
        Property::BackgroundPositionY(ys) => {
            if let Some(y) = ys.first() {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::BackgroundPositionY,
                    PropertyValue::Length(convert_bg_vert_pos(y)),
                    imp,
                );
            }
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
            if let Some(v) = convert_color_prop(c) {
                push_decl(out, ord, PropertyIdKind::BorderTopColor, v, imp);
            }
        }
        Property::BorderRightColor(c) => {
            if let Some(v) = convert_color_prop(c) {
                push_decl(out, ord, PropertyIdKind::BorderRightColor, v, imp);
            }
        }
        Property::BorderBottomColor(c) => {
            if let Some(v) = convert_color_prop(c) {
                push_decl(out, ord, PropertyIdKind::BorderBottomColor, v, imp);
            }
        }
        Property::BorderLeftColor(c) => {
            if let Some(v) = convert_color_prop(c) {
                push_decl(out, ord, PropertyIdKind::BorderLeftColor, v, imp);
            }
        }
        Property::BorderColor(bc) => {
            if let Some(v) = convert_color_prop(&bc.top) {
                push_decl(out, ord, PropertyIdKind::BorderTopColor, v, imp);
            }
            if let Some(v) = convert_color_prop(&bc.right) {
                push_decl(out, ord, PropertyIdKind::BorderRightColor, v, imp);
            }
            if let Some(v) = convert_color_prop(&bc.bottom) {
                push_decl(out, ord, PropertyIdKind::BorderBottomColor, v, imp);
            }
            if let Some(v) = convert_color_prop(&bc.left) {
                push_decl(out, ord, PropertyIdKind::BorderLeftColor, v, imp);
            }
        }
        Property::Border(b) => {
            let w = convert_border_width_lp(&b.width);
            let s = convert_line_style(&b.style);
            let c = convert_color_prop(&b.color)
                .unwrap_or(PropertyValue::Color(CssColor::CurrentColor));
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
                push_decl(out, ord, ck, c.clone(), imp);
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
            if let Some(c) = convert_color_prop(&b.color) {
                push_decl(out, ord, PropertyIdKind::BorderTopColor, c, imp);
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
            if let Some(c) = convert_color_prop(&b.color) {
                push_decl(out, ord, PropertyIdKind::BorderRightColor, c, imp);
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
            if let Some(c) = convert_color_prop(&b.color) {
                push_decl(out, ord, PropertyIdKind::BorderBottomColor, c, imp);
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
            if let Some(c) = convert_color_prop(&b.color) {
                push_decl(out, ord, PropertyIdKind::BorderLeftColor, c, imp);
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
            if let Some(v) = convert_color_prop(c) {
                push_decl(out, ord, PropertyIdKind::BorderBlockStartColor, v, imp);
            }
        }
        Property::BorderBlockEndColor(c) => {
            if let Some(v) = convert_color_prop(c) {
                push_decl(out, ord, PropertyIdKind::BorderBlockEndColor, v, imp);
            }
        }
        Property::BorderInlineStartColor(c) => {
            if let Some(v) = convert_color_prop(c) {
                push_decl(out, ord, PropertyIdKind::BorderInlineStartColor, v, imp);
            }
        }
        Property::BorderInlineEndColor(c) => {
            if let Some(v) = convert_color_prop(c) {
                push_decl(out, ord, PropertyIdKind::BorderInlineEndColor, v, imp);
            }
        }
        Property::BorderBlockColor(bc) => {
            if let Some(v) = convert_color_prop(&bc.start) {
                push_decl(out, ord, PropertyIdKind::BorderBlockStartColor, v, imp);
            }
            if let Some(v) = convert_color_prop(&bc.end) {
                push_decl(out, ord, PropertyIdKind::BorderBlockEndColor, v, imp);
            }
        }
        Property::BorderInlineColor(bc) => {
            if let Some(v) = convert_color_prop(&bc.start) {
                push_decl(out, ord, PropertyIdKind::BorderInlineStartColor, v, imp);
            }
            if let Some(v) = convert_color_prop(&bc.end) {
                push_decl(out, ord, PropertyIdKind::BorderInlineEndColor, v, imp);
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
            if let Some(c) = convert_color_prop(&b.color) {
                push_decl(out, ord, PropertyIdKind::BorderBlockStartColor, c, imp);
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
            if let Some(c) = convert_color_prop(&b.color) {
                push_decl(out, ord, PropertyIdKind::BorderBlockEndColor, c, imp);
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
            if let Some(c) = convert_color_prop(&b.color) {
                push_decl(out, ord, PropertyIdKind::BorderInlineStartColor, c, imp);
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
            if let Some(c) = convert_color_prop(&b.color) {
                push_decl(out, ord, PropertyIdKind::BorderInlineEndColor, c, imp);
            }
        }
        Property::BorderBlock(b) => {
            let w = convert_border_width_lp(&b.width);
            let s = convert_line_style(&b.style);
            let c = convert_color_prop(&b.color);
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
                if let Some(ref col) = c {
                    push_decl(out, ord, ck, col.clone(), imp);
                }
            }
        }
        Property::BorderInline(b) => {
            let w = convert_border_width_lp(&b.width);
            let s = convert_line_style(&b.style);
            let c = convert_color_prop(&b.color);
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
                if let Some(ref col) = c {
                    push_decl(out, ord, ck, col.clone(), imp);
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
        Property::Appearance(app, _) => {
            use lightningcss::properties::ui::Appearance as LA;
            let mode = match app {
                LA::None => Some(AppearanceMode::None),
                LA::Auto
                | LA::Button
                | LA::Checkbox
                | LA::Listbox
                | LA::Menulist
                | LA::Meter
                | LA::ProgressBar
                | LA::PushButton
                | LA::Radio
                | LA::Searchfield
                | LA::SliderHorizontal
                | LA::SquareButton
                | LA::Textarea => Some(AppearanceMode::Auto),
                LA::Textfield => Some(AppearanceMode::Textfield),
                LA::MenulistButton => Some(AppearanceMode::MenulistButton),
                LA::NonStandard(s) => AppearanceMode::parse_css(s.as_ref()),
            };
            if let Some(m) = mode {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::Appearance,
                    PropertyValue::Appearance(m),
                    imp,
                );
            }
        }
        Property::AccentColor(col) => {
            use lightningcss::properties::ui::ColorOrAuto;
            let val = match col {
                ColorOrAuto::Auto => Some(None),
                ColorOrAuto::Color(c) => convert_color(c).map(Some),
            };
            if let Some(v) = val {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::AccentColor,
                    PropertyValue::AccentColor(v),
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
            PropertyValue::Length(convert_border_width_lp(w)),
            imp,
        ),
        Property::OutlineColor(c) => {
            if let Some(v) = convert_color_prop(c) {
                push_decl(out, ord, PropertyIdKind::OutlineColor, v, imp);
            }
        }
        Property::Outline(o) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::OutlineWidth,
                PropertyValue::Length(convert_border_width_lp(&o.width)),
                imp,
            );
            if let Some(c) = convert_color_prop(&o.color) {
                push_decl(out, ord, PropertyIdKind::OutlineColor, c, imp);
            }
            let st = match &o.style {
                lightningcss::properties::outline::OutlineStyle::Auto => LineStyle::Auto,
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
                lightningcss::properties::outline::OutlineStyle::Auto => LineStyle::Auto,
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
        Property::FontWeight(fw) => {
            use lightningcss::properties::font::FontWeight as LcFw;
            let val = match fw {
                LcFw::Bolder => PropertyValue::FontWeightRelative(1),
                LcFw::Lighter => PropertyValue::FontWeightRelative(-1),
                other => PropertyValue::Number(convert_font_weight(other)),
            };
            push_decl(out, ord, PropertyIdKind::FontWeight, val, imp);
        }
        Property::FontStyle(fs) => push_decl(
            out,
            ord,
            PropertyIdKind::FontStyle,
            PropertyValue::FontStyle(convert_font_style(fs)),
            imp,
        ),
        Property::FontFamily(fams) => {
            use lightningcss::properties::font::{FontFamily as LcFontFamily, GenericFontFamily};
            if fams.len() == 1 {
                if let LcFontFamily::Generic(g) = &fams[0] {
                    let global_kw = match g {
                        GenericFontFamily::Initial => Some(GlobalKeyword::Initial),
                        GenericFontFamily::Inherit => Some(GlobalKeyword::Inherit),
                        GenericFontFamily::Unset => Some(GlobalKeyword::Unset),
                        GenericFontFamily::Revert => Some(GlobalKeyword::Revert),
                        GenericFontFamily::RevertLayer => Some(GlobalKeyword::RevertLayer),
                        _ => None,
                    };
                    if let Some(kw) = global_kw {
                        push_decl(
                            out,
                            ord,
                            PropertyIdKind::FontFamily,
                            PropertyValue::Global(kw),
                            imp,
                        );
                        return;
                    }
                }
            }
            // CSS Fonts L3 §3.1 & CSS Values L4 §3.1.2: CSS-wide keywords and `default` are
            // invalid as unquoted `<family-name>` inside a `font-family` list.
            let has_reserved_kw = fams.iter().any(|f| {
                matches!(
                    f,
                    LcFontFamily::Generic(
                        GenericFontFamily::Initial
                            | GenericFontFamily::Inherit
                            | GenericFontFamily::Unset
                            | GenericFontFamily::Default
                            | GenericFontFamily::Revert
                            | GenericFontFamily::RevertLayer
                    )
                )
            });
            if !has_reserved_kw {
                let names = fams.iter().map(convert_font_family_name).collect();
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::FontFamily,
                    PropertyValue::FontFamily(names),
                    imp,
                );
            }
        }
        Property::LineHeight(lh) => push_decl(
            out,
            ord,
            PropertyIdKind::LineHeight,
            PropertyValue::LineHeight(convert_line_height(lh)),
            imp,
        ),
        Property::Font(f) => {
            use lightningcss::properties::font::{FontFamily as LcFontFamily, GenericFontFamily};
            let has_reserved_kw = f.family.iter().any(|fam| {
                matches!(
                    fam,
                    LcFontFamily::Generic(
                        GenericFontFamily::Initial
                            | GenericFontFamily::Inherit
                            | GenericFontFamily::Unset
                            | GenericFontFamily::Default
                            | GenericFontFamily::Revert
                            | GenericFontFamily::RevertLayer
                    )
                )
            });
            if !has_reserved_kw {
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
        Property::TextDecorationStyle(st, _) => {
            use lightningcss::properties::text::TextDecorationStyle as LTds;
            let mode = match st {
                LTds::Solid => crate::values::TextDecorationStyleMode::Solid,
                LTds::Double => crate::values::TextDecorationStyleMode::Double,
                LTds::Dotted => crate::values::TextDecorationStyleMode::Dotted,
                LTds::Dashed => crate::values::TextDecorationStyleMode::Dashed,
                LTds::Wavy => crate::values::TextDecorationStyleMode::Wavy,
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::TextDecorationStyle,
                PropertyValue::TextDecorationStyle(mode),
                imp,
            );
        }
        Property::TextDecorationColor(c, _) => {
            if let Some(v) = convert_color_prop(c) {
                push_decl(out, ord, PropertyIdKind::TextDecorationColor, v, imp);
            }
        }
        Property::TextDecoration(td, _) => {
            use lightningcss::properties::text::TextDecorationStyle as LTds;
            push_decl(
                out,
                ord,
                PropertyIdKind::TextDecorationLine,
                PropertyValue::TextDecorationLine(convert_text_decoration_line(&td.line)),
                imp,
            );
            let st_mode = match td.style {
                LTds::Solid => crate::values::TextDecorationStyleMode::Solid,
                LTds::Double => crate::values::TextDecorationStyleMode::Double,
                LTds::Dotted => crate::values::TextDecorationStyleMode::Dotted,
                LTds::Dashed => crate::values::TextDecorationStyleMode::Dashed,
                LTds::Wavy => crate::values::TextDecorationStyleMode::Wavy,
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::TextDecorationStyle,
                PropertyValue::TextDecorationStyle(st_mode),
                imp,
            );
            if let Some(c) = convert_color_prop(&td.color) {
                push_decl(out, ord, PropertyIdKind::TextDecorationColor, c, imp);
            }
        }
        Property::FontStretch(fs) => {
            use lightningcss::properties::font::{FontStretch as LFs, FontStretchKeyword as LKw};
            let pct = match fs {
                LFs::Keyword(LKw::UltraCondensed) => 50.0,
                LFs::Keyword(LKw::ExtraCondensed) => 62.5,
                LFs::Keyword(LKw::Condensed) => 75.0,
                LFs::Keyword(LKw::SemiCondensed) => 87.5,
                LFs::Keyword(LKw::Normal) => 100.0,
                LFs::Keyword(LKw::SemiExpanded) => 112.5,
                LFs::Keyword(LKw::Expanded) => 125.0,
                LFs::Keyword(LKw::ExtraExpanded) => 150.0,
                LFs::Keyword(LKw::UltraExpanded) => 200.0,
                LFs::Percentage(p) => (p.0 * 100.0).max(0.0),
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::FontStretch,
                PropertyValue::Number(pct),
                imp,
            );
        }
        Property::FillOpacity(a) => push_decl(
            out,
            ord,
            PropertyIdKind::SvgFillOpacity,
            PropertyValue::Number(a.0.clamp(0.0, 1.0)),
            imp,
        ),
        Property::StrokeOpacity(a) => push_decl(
            out,
            ord,
            PropertyIdKind::SvgStrokeOpacity,
            PropertyValue::Number(a.0.clamp(0.0, 1.0)),
            imp,
        ),
        Property::StrokeLinecap(lc) => {
            use lightningcss::properties::svg::StrokeLinecap as LSlc;
            let v = match lc {
                LSlc::Butt => crate::values::SvgStrokeLinecap::Butt,
                LSlc::Round => crate::values::SvgStrokeLinecap::Round,
                LSlc::Square => crate::values::SvgStrokeLinecap::Square,
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::SvgStrokeLinecap,
                PropertyValue::SvgStrokeLinecap(v),
                imp,
            );
        }
        Property::StrokeLinejoin(lj) => {
            use lightningcss::properties::svg::StrokeLinejoin as LSlj;
            let v = match lj {
                LSlj::Miter => crate::values::SvgStrokeLinejoin::Miter,
                LSlj::MiterClip => crate::values::SvgStrokeLinejoin::MiterClip,
                LSlj::Round => crate::values::SvgStrokeLinejoin::Round,
                LSlj::Bevel => crate::values::SvgStrokeLinejoin::Bevel,
                LSlj::Arcs => crate::values::SvgStrokeLinejoin::Arcs,
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::SvgStrokeLinejoin,
                PropertyValue::SvgStrokeLinejoin(v),
                imp,
            );
        }
        Property::StrokeMiterlimit(ml) => {
            if *ml >= 1.0 && ml.is_finite() {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::SvgStrokeMiterlimit,
                    PropertyValue::Number(*ml),
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
        Property::WhiteSpace(ws) => {
            let mode = convert_white_space(ws);
            let (collapse, wrap) = crate::text_css::decompose_legacy_white_space(mode);
            push_decl(
                out,
                ord,
                PropertyIdKind::WhiteSpaceCollapse,
                PropertyValue::WhiteSpaceCollapse(collapse),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::TextWrapMode,
                PropertyValue::TextWrapMode(wrap),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::WhiteSpaceTrim,
                PropertyValue::WhiteSpaceTrim(crate::text_css::WhiteSpaceTrim::NONE),
                imp,
            );
        }
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
        Property::RowGap(g) => {
            push_decl(out, ord, PropertyIdKind::RowGap, convert_gap_prop(g), imp)
        }
        Property::ColumnGap(g) => push_decl(
            out,
            ord,
            PropertyIdKind::ColumnGap,
            convert_gap_prop(g),
            imp,
        ),
        Property::Gap(g) => {
            push_decl(
                out,
                ord,
                PropertyIdKind::RowGap,
                convert_gap_prop(&g.row),
                imp,
            );
            push_decl(
                out,
                ord,
                PropertyIdKind::ColumnGap,
                convert_gap_prop(&g.column),
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
        Property::MaskImage(imgs, _) => {
            let img = imgs
                .first()
                .map(convert_image)
                .unwrap_or(BackgroundImageValue::None);
            push_decl(
                out,
                ord,
                PropertyIdKind::MaskImage,
                PropertyValue::MaskImage(img),
                imp,
            );
        }
        Property::Mask(masks, _) => {
            let img = masks
                .first()
                .map(|m| convert_image(&m.image))
                .unwrap_or(BackgroundImageValue::None);
            push_decl(
                out,
                ord,
                PropertyIdKind::MaskImage,
                PropertyValue::MaskImage(img),
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
            if props.len() == 1 {
                if let Some(kw) = GlobalKeyword::from_ident(props[0].name()) {
                    push_decl(
                        out,
                        ord,
                        PropertyIdKind::TransitionProperty,
                        PropertyValue::Global(kw),
                        imp,
                    );
                    return;
                }
            }
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
            if matches!(&bs.0, lightningcss::values::length::Length::Value(lv) if convert_length_value(lv).is_negative_literal())
                || matches!(&bs.1, lightningcss::values::length::Length::Value(lv) if convert_length_value(lv).is_negative_literal())
            {
                return;
            }
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
        Property::ListStyleType(lst) => {
            if let Ok(s) = lightningcss::traits::ToCss::to_css_string(
                lst,
                lightningcss::printer::PrinterOptions::default(),
            ) {
                if let Some(mode) = ListStyleTypeMode::from_css_keyword(&s) {
                    push_decl(
                        out,
                        ord,
                        PropertyIdKind::ListStyleType,
                        PropertyValue::ListStyleType(mode),
                        imp,
                    );
                }
            }
        }
        Property::ListStylePosition(lsp) => {
            if let Ok(s) = lightningcss::traits::ToCss::to_css_string(
                lsp,
                lightningcss::printer::PrinterOptions::default(),
            ) {
                if let Some(mode) = ListStylePositionMode::from_css_keyword(&s) {
                    push_decl(
                        out,
                        ord,
                        PropertyIdKind::ListStylePosition,
                        PropertyValue::ListStylePosition(mode),
                        imp,
                    );
                }
            }
        }
        Property::ListStyle(ls) => {
            if let Ok(s) = lightningcss::traits::ToCss::to_css_string(
                &ls.list_style_type,
                lightningcss::printer::PrinterOptions::default(),
            ) {
                if let Some(mode) = ListStyleTypeMode::from_css_keyword(&s) {
                    push_decl(
                        out,
                        ord,
                        PropertyIdKind::ListStyleType,
                        PropertyValue::ListStyleType(mode),
                        imp,
                    );
                }
            }
            if let Ok(s) = lightningcss::traits::ToCss::to_css_string(
                &ls.position,
                lightningcss::printer::PrinterOptions::default(),
            ) {
                if let Some(mode) = ListStylePositionMode::from_css_keyword(&s) {
                    push_decl(
                        out,
                        ord,
                        PropertyIdKind::ListStylePosition,
                        PropertyValue::ListStylePosition(mode),
                        imp,
                    );
                }
            }
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
        Property::Fill(paint) => match paint {
            lightningcss::properties::svg::SVGPaint::None => {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::SvgFill,
                    PropertyValue::SvgPaint(None),
                    imp,
                );
            }
            lightningcss::properties::svg::SVGPaint::Color(c) => {
                if let Some(col) = convert_color(c) {
                    push_decl(
                        out,
                        ord,
                        PropertyIdKind::SvgFill,
                        PropertyValue::SvgPaint(Some(col)),
                        imp,
                    );
                }
            }
            lightningcss::properties::svg::SVGPaint::Url {
                fallback: Some(lightningcss::properties::svg::SVGPaintFallback::Color(c)),
                ..
            } => {
                if let Some(col) = convert_color(c) {
                    push_decl(
                        out,
                        ord,
                        PropertyIdKind::SvgFill,
                        PropertyValue::SvgPaint(Some(col)),
                        imp,
                    );
                }
            }
            _ => {}
        },
        Property::Stroke(paint) => match paint {
            lightningcss::properties::svg::SVGPaint::None => {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::SvgStroke,
                    PropertyValue::SvgPaint(None),
                    imp,
                );
            }
            lightningcss::properties::svg::SVGPaint::Color(c) => {
                if let Some(col) = convert_color(c) {
                    push_decl(
                        out,
                        ord,
                        PropertyIdKind::SvgStroke,
                        PropertyValue::SvgPaint(Some(col)),
                        imp,
                    );
                }
            }
            lightningcss::properties::svg::SVGPaint::Url {
                fallback: Some(lightningcss::properties::svg::SVGPaintFallback::Color(c)),
                ..
            } => {
                if let Some(col) = convert_color(c) {
                    push_decl(
                        out,
                        ord,
                        PropertyIdKind::SvgStroke,
                        PropertyValue::SvgPaint(Some(col)),
                        imp,
                    );
                }
            }
            _ => {}
        },
        Property::FillRule(rule) => {
            let even_odd = matches!(rule, lightningcss::values::shape::FillRule::Evenodd);
            push_decl(
                out,
                ord,
                PropertyIdKind::SvgFillRule,
                PropertyValue::SvgFillRuleEvenOdd(even_odd),
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
                } else {
                    let _ = parse_values4_property_decls(kind, raw_val, imp, ord, out);
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
                    let sentinel_unwrapped: Option<(&str, &str)> = {
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
                                    "--__rix_subst__" | "--__rix_val4__" | "--__rix_color4__"
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
                    if let Some((sent_name, raw_val)) = sentinel_unwrapped {
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
                                push_decl(
                                    out,
                                    ord,
                                    kind,
                                    PropertyValue::Color4(Box::new(expr)),
                                    imp,
                                );
                            }
                        } else {
                            let _ = parse_values4_property_decls(kind, raw_val, imp, ord, out);
                        }
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
                                            for target in kind.shorthand_longhands() {
                                                push_decl(
                                                    out,
                                                    ord,
                                                    target,
                                                    PropertyValue::Global(kw),
                                                    imp,
                                                );
                                            }
                                            handled = true;
                                        }
                                    }
                                    CssToken::Var {
                                        name,
                                        fallback: None,
                                    } if name.as_str() == "--__rix_revert_rule__" => {
                                        for target in kind.shorthand_longhands() {
                                            push_decl(
                                                out,
                                                ord,
                                                target,
                                                PropertyValue::Global(GlobalKeyword::RevertRule),
                                                imp,
                                            );
                                        }
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
                                            let _ = parse_values4_property_decls(
                                                kind, raw_val, imp, ord, out,
                                            );
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
                        let _ = parse_values4_property_decls(kind, buf.trim(), imp, ord, out);
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
            let pname = prop_id.name();
            if let Some(
                kind @ (PropertyIdKind::ContainerType
                | PropertyIdKind::ContainerName
                | PropertyIdKind::Container),
            ) = PropertyIdKind::from_name(pname)
            {
                if let Ok(val_str) =
                    other.value_to_css_string(lightningcss::stylesheet::PrinterOptions::default())
                {
                    if kind == PropertyIdKind::Container {
                        let trimmed = val_str.trim();
                        let (name_part, type_part) = if let Some((l, r)) = trimmed.split_once('/') {
                            (l.trim(), Some(r.trim()))
                        } else {
                            (trimmed, None)
                        };
                        if let Some((nv, _)) =
                            parse_values4_property(PropertyIdKind::ContainerName, name_part)
                        {
                            let tv = match type_part {
                                Some(tp) => {
                                    parse_values4_property(PropertyIdKind::ContainerType, tp)
                                        .map(|(v, _)| v)
                                }
                                None => Some(PropertyValue::ContainerType(0)),
                            };
                            if let Some(tv) = tv {
                                push_decl(out, ord, PropertyIdKind::ContainerName, nv, imp);
                                push_decl(out, ord, PropertyIdKind::ContainerType, tv, imp);
                                return;
                            }
                        }
                    } else if let Some((v, _)) = parse_values4_property(kind, val_str.trim()) {
                        push_decl(out, ord, kind, v, imp);
                        return;
                    }
                }
            }
            missing.push(MissingCssFeature::new(MissingCssKind::Property, pname));
        }
    }
}

fn convert_gap_prop(gap: &lightningcss::properties::align::GapValue) -> PropertyValue {
    match gap {
        lightningcss::properties::align::GapValue::Normal => PropertyValue::GapNormal,
        lightningcss::properties::align::GapValue::LengthPercentage(lp) => {
            PropertyValue::Length(convert_length_percentage(lp))
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

fn parse_values4_property_decls(
    prop: PropertyIdKind,
    raw: &str,
    imp: bool,
    ord: &mut u32,
    out: &mut Vec<SpecifiedDeclaration>,
) -> bool {
    match prop {
        PropertyIdKind::Container => {
            let trimmed = raw.trim();
            let (name_part, type_part) = if let Some((l, r)) = trimmed.split_once('/') {
                (l.trim(), Some(r.trim()))
            } else {
                (trimmed, None)
            };
            if let Some((nv, _)) = parse_values4_property(PropertyIdKind::ContainerName, name_part)
            {
                let tv = match type_part {
                    Some(tp) => {
                        parse_values4_property(PropertyIdKind::ContainerType, tp).map(|(v, _)| v)
                    }
                    None => Some(PropertyValue::ContainerType(0)),
                };
                if let Some(tv) = tv {
                    push_decl(out, ord, PropertyIdKind::ContainerName, nv, imp);
                    push_decl(out, ord, PropertyIdKind::ContainerType, tv, imp);
                    return true;
                }
            }
            false
        }
        PropertyIdKind::GridRow => {
            if let Some((s, e)) = crate::grid_css::parse_grid_row_or_col_shorthand(raw) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridRowStart,
                    PropertyValue::GridPlacement(s),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridRowEnd,
                    PropertyValue::GridPlacement(e),
                    imp,
                );
                return true;
            }
            false
        }
        PropertyIdKind::GridColumn => {
            if let Some((s, e)) = crate::grid_css::parse_grid_row_or_col_shorthand(raw) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridColumnStart,
                    PropertyValue::GridPlacement(s),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridColumnEnd,
                    PropertyValue::GridPlacement(e),
                    imp,
                );
                return true;
            }
            false
        }
        PropertyIdKind::GridArea => {
            if let Some((rs, cs, re, ce)) = crate::grid_css::parse_grid_area_shorthand(raw) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridRowStart,
                    PropertyValue::GridPlacement(rs),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridColumnStart,
                    PropertyValue::GridPlacement(cs),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridRowEnd,
                    PropertyValue::GridPlacement(re),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridColumnEnd,
                    PropertyValue::GridPlacement(ce),
                    imp,
                );
                return true;
            }
            false
        }
        PropertyIdKind::GridTemplate => {
            if let Some((r, c, a)) = crate::grid_css::parse_grid_template_shorthand(raw) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridTemplateRows,
                    PropertyValue::GridTemplateTracks(r),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridTemplateColumns,
                    PropertyValue::GridTemplateTracks(c),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridTemplateAreas,
                    PropertyValue::GridTemplateAreas(a),
                    imp,
                );
                return true;
            }
            false
        }
        PropertyIdKind::Grid => {
            if let Some((r, c, a, ar, ac, af)) = crate::grid_css::parse_grid_shorthand(raw) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridTemplateRows,
                    PropertyValue::GridTemplateTracks(r),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridTemplateColumns,
                    PropertyValue::GridTemplateTracks(c),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridTemplateAreas,
                    PropertyValue::GridTemplateAreas(a),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridAutoRows,
                    PropertyValue::GridTracks(ar),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridAutoColumns,
                    PropertyValue::GridTracks(ac),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridAutoFlow,
                    PropertyValue::GridAutoFlow(af),
                    imp,
                );
                return true;
            }
            false
        }
        PropertyIdKind::GridLanes => {
            if let Some((c, r, a, d)) = crate::grid_css::parse_grid_lanes_shorthand(raw) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridTemplateColumns,
                    PropertyValue::GridTemplateTracks(c),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridTemplateRows,
                    PropertyValue::GridTemplateTracks(r),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridTemplateAreas,
                    PropertyValue::GridTemplateAreas(a),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::GridLanesDirection,
                    PropertyValue::GridLanesDirection(d),
                    imp,
                );
                return true;
            }
            false
        }
        PropertyIdKind::AnimationRange => {
            if let Some((starts, ends, _)) = crate::anim_css::parse_animation_range_shorthand(raw) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::AnimationRangeStart,
                    PropertyValue::AnimationRangeList(starts),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::AnimationRangeEnd,
                    PropertyValue::AnimationRangeList(ends),
                    imp,
                );
                return true;
            }
            false
        }
        PropertyIdKind::WhiteSpace => {
            if let Some((c, w, t)) = crate::text_css::parse_white_space_shorthand(raw) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::WhiteSpaceCollapse,
                    PropertyValue::WhiteSpaceCollapse(c),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::TextWrapMode,
                    PropertyValue::TextWrapMode(w),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::WhiteSpaceTrim,
                    PropertyValue::WhiteSpaceTrim(t),
                    imp,
                );
                return true;
            }
            false
        }
        PropertyIdKind::TextWrap => {
            if let Some((m, s)) = crate::text_css::parse_text_wrap_shorthand(raw) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::TextWrapMode,
                    PropertyValue::TextWrapMode(m),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::TextWrapStyle,
                    PropertyValue::TextWrapStyle(s),
                    imp,
                );
                return true;
            }
            false
        }
        PropertyIdKind::TextSpacing => {
            if let Some((t, a)) = crate::text_css::parse_text_spacing_shorthand(raw) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::TextSpacingTrim,
                    PropertyValue::TextSpacingTrim(t),
                    imp,
                );
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::TextAutospace,
                    PropertyValue::TextAutospace(a),
                    imp,
                );
                return true;
            }
            false
        }
        PropertyIdKind::Background => {
            let decls = parse_property_from_str(PropertyIdKind::Background, raw, imp);
            if !decls.is_empty() {
                for mut d in decls {
                    let idx = *ord;
                    *ord = ord.saturating_add(1);
                    d.order = idx;
                    out.push(d);
                }
                true
            } else {
                false
            }
        }
        _ => {
            if let Some((val, _)) = parse_values4_property(prop, raw) {
                push_decl(out, ord, prop, val, imp);
                true
            } else {
                false
            }
        }
    }
}

fn convert_grid_line(line: &lightningcss::properties::grid::GridLine<'_>) -> GridPlacementValue {
    match line {
        lightningcss::properties::grid::GridLine::Auto => GridPlacementValue::Auto,
        lightningcss::properties::grid::GridLine::Area { name } => {
            GridPlacementValue::Named(intern_str(name.0.as_ref()))
        }
        lightningcss::properties::grid::GridLine::Line { index, name } => {
            GridPlacementValue::Line {
                index: *index,
                index_math: None,
                name: name.as_ref().map(|n| intern_str(n.0.as_ref())),
            }
        }
        lightningcss::properties::grid::GridLine::Span { index, name } => {
            GridPlacementValue::Span {
                count: (*index).max(1) as u32,
                count_math: None,
                name: name.as_ref().map(|n| intern_str(n.0.as_ref())),
            }
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
        "clip" => {
            if single_ident.is_some_and(|s| s.eq_ignore_ascii_case("auto")) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::Clip,
                    PropertyValue::ClipRect(None),
                    imp,
                );
                return;
            }
            if let Some(CssToken::Function { name: fname, args }) = non_ws.first() {
                if fname.as_str().eq_ignore_ascii_case("rect") && non_ws.len() == 1 {
                    let parts: Vec<&CssToken> = args
                        .iter()
                        .filter(|t| !matches!(t, CssToken::Space | CssToken::Comma))
                        .collect();
                    if parts.len() == 4 {
                        let parse_edge = |tok: &CssToken| -> Option<Option<LengthPercentage>> {
                            match tok {
                                CssToken::Ident(id) if id.as_str().eq_ignore_ascii_case("auto") => {
                                    Some(None)
                                }
                                CssToken::Number(n) if *n == 0.0 => {
                                    Some(Some(LengthPercentage::Zero))
                                }
                                CssToken::Dimension(v, u) => LengthUnit::from_unit_str(u.as_str())
                                    .map(|lu| Some(LengthPercentage::Length(*v, lu))),
                                _ => None,
                            }
                        };
                        if let (Some(top), Some(right), Some(bottom), Some(left)) = (
                            parse_edge(parts[0]),
                            parse_edge(parts[1]),
                            parse_edge(parts[2]),
                            parse_edge(parts[3]),
                        ) {
                            push_decl(
                                out,
                                ord,
                                PropertyIdKind::Clip,
                                PropertyValue::ClipRect(Some(crate::values::CssClipRect {
                                    top,
                                    right,
                                    bottom,
                                    left,
                                })),
                                imp,
                            );
                        }
                    }
                }
            }
        }
        "grid-gap" | "grid-row-gap" | "grid-column-gap" => {
            let canonical = match lower.as_str() {
                "grid-gap" => PropertyIdKind::Gap,
                "grid-row-gap" => PropertyIdKind::RowGap,
                _ => PropertyIdKind::ColumnGap,
            };
            let mut buf = String::new();
            write_tokens_for_reparse(&owned, &mut buf);
            for mut d in parse_property_from_str(canonical, buf.trim(), imp) {
                let idx = *ord;
                *ord = ord.saturating_add(1);
                d.order = idx;
                out.push(d);
            }
        }
        "text-orientation" => {
            let Some(mode) = single_ident.and_then(|s| match s.to_ascii_lowercase().as_str() {
                "mixed" => Some(TextOrientationMode::Mixed),
                "upright" => Some(TextOrientationMode::Upright),
                "sideways" | "sideways-right" => Some(TextOrientationMode::Sideways),
                _ => None,
            }) else {
                return;
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::TextOrientation,
                PropertyValue::TextOrientation(mode),
                imp,
            );
        }
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
        "scroll-behavior" => {
            let mode = match single_ident.map(str::to_ascii_lowercase).as_deref() {
                Some("auto") => ScrollBehaviorMode::Auto,
                Some("smooth") => ScrollBehaviorMode::Smooth,
                _ => return,
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::ScrollBehavior,
                PropertyValue::ScrollBehavior(mode),
                imp,
            );
        }
        "content-visibility" => {
            let mode = match single_ident.map(str::to_ascii_lowercase).as_deref() {
                Some("visible") => ContentVisibilityMode::Visible,
                Some("auto") => ContentVisibilityMode::Auto,
                Some("hidden") => ContentVisibilityMode::Hidden,
                _ => return,
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::ContentVisibility,
                PropertyValue::ContentVisibility(mode),
                imp,
            );
        }
        "zoom" => {
            if non_ws.len() != 1 {
                return;
            }
            let z = match &non_ws[0] {
                CssToken::Ident(s) if s.as_str().eq_ignore_ascii_case("normal") => 1.0,
                CssToken::Number(n) if *n >= 0.0 && n.is_finite() => {
                    if *n == 0.0 {
                        1.0
                    } else {
                        *n
                    }
                }
                CssToken::Percentage(p) if *p >= 0.0 && p.is_finite() => {
                    if *p == 0.0 {
                        1.0
                    } else {
                        *p
                    }
                }
                _ => return,
            };
            push_decl(out, ord, PropertyIdKind::Zoom, PropertyValue::Zoom(z), imp);
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
                "auto" => Some(PointerEventsMode::Auto),
                "none" => Some(PointerEventsMode::None),
                "visiblepainted" => Some(PointerEventsMode::VisiblePainted),
                "visiblefill" => Some(PointerEventsMode::VisibleFill),
                "visiblestroke" => Some(PointerEventsMode::VisibleStroke),
                "visible" => Some(PointerEventsMode::Visible),
                "painted" => Some(PointerEventsMode::Painted),
                "fill" => Some(PointerEventsMode::Fill),
                "stroke" => Some(PointerEventsMode::Stroke),
                "all" => Some(PointerEventsMode::All),
                "bounding-box" => Some(PointerEventsMode::BoundingBox),
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
        "empty-cells" => {
            let Some(mode) = single_ident.and_then(|s| match s.to_ascii_lowercase().as_str() {
                "show" => Some(EmptyCellsMode::Show),
                "hide" => Some(EmptyCellsMode::Hide),
                _ => None,
            }) else {
                return;
            };
            push_decl(
                out,
                ord,
                PropertyIdKind::EmptyCells,
                PropertyValue::EmptyCells(mode),
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
            let unwrapped_val4: Option<String> = {
                let non_ws_c: SmallVec<[&CssToken; 2]> = owned
                    .iter()
                    .filter(|t| !matches!(t, CssToken::Space))
                    .collect();
                if non_ws_c.len() == 1 {
                    if let CssToken::Var {
                        name,
                        fallback: Some(fb),
                    } = non_ws_c[0]
                    {
                        if name.as_str() == "--__rix_val4__" || name.as_str() == "--__rix_color4__"
                        {
                            fb.iter().find_map(|t| match t {
                                CssToken::String(s) => Some(s.as_str().to_string()),
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
            let raw_str = unwrapped_val4.unwrap_or_else(|| serialize_css_tokens(&owned));
            let trimmed = raw_str.trim();
            if trimmed.to_ascii_lowercase().starts_with("light-dark(")
                && crate::substitution::resolve_light_dark_image_fn(
                    trimmed,
                    false,
                    &crate::values::EvalContext::default(),
                    false,
                )
                .is_some()
            {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::Content,
                    PropertyValue::BackgroundImageRaw(trimmed.to_string()),
                    imp,
                );
                return;
            }
            if let Some((cname, cstyle)) = owned.iter().find_map(|t| match t {
                CssToken::Function { name, args } if name.eq_ignore_ascii_case("counter") => {
                    let mut idents = args.iter().filter_map(|a| match a {
                        CssToken::Ident(s) => Some(s.clone()),
                        _ => None,
                    });
                    let cname = idents.next()?;
                    let cstyle = idents.next().unwrap_or_else(|| intern_str("decimal"));
                    Some((cname, cstyle))
                }
                _ => None,
            }) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::Content,
                    PropertyValue::ContentCounter(cname, cstyle),
                    imp,
                );
                return;
            }
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
        "column-rule-color" => {
            let raw_str = serialize_css_tokens(&owned);
            if let Some(mut expr) = crate::color::parse_css_color(raw_str.trim()) {
                expr.bind_random_property("column-rule-color");
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::ColumnRuleColor,
                    PropertyValue::Color4(Box::new(expr)),
                    imp,
                );
            }
        }
        "will-change" => {
            let creates_cb = owned.iter().any(|t| {
                matches!(
                    t,
                    CssToken::Ident(id) if matches!(
                        id.as_str().to_ascii_lowercase().as_str(),
                        "transform"
                            | "scale"
                            | "rotate"
                            | "translate"
                            | "perspective"
                            | "filter"
                            | "backdrop-filter"
                            | "contain"
                            | "clip-path"
                            | "mask"
                            | "mask-image"
                            | "view-transition-name"
                    )
                )
            });
            push_decl(
                out,
                ord,
                PropertyIdKind::WillChange,
                PropertyValue::WillChangeCreatesCb(creates_cb),
                imp,
            );
        }
        "flood-opacity" | "stop-opacity" => {
            let kind = if name == "flood-opacity" {
                PropertyIdKind::SvgFloodOpacity
            } else {
                PropertyIdKind::SvgStopOpacity
            };
            if non_ws.len() == 1 {
                let v = match &non_ws[0] {
                    CssToken::Number(n) if n.is_finite() => Some(n.clamp(0.0, 1.0)),
                    CssToken::Percentage(p) if p.is_finite() => Some((p / 100.0).clamp(0.0, 1.0)),
                    _ => None,
                };
                if let Some(val) = v {
                    push_decl(out, ord, kind, PropertyValue::Number(val), imp);
                }
            }
        }
        "font-feature-settings" => {
            // CSS Syntax L3 §4.3.3: bare `<integer>` tokens must have type flag `integer`
            // (rejecting scientific notation like `1e1` / `1.1e1` which parse as `int_value: None`).
            let has_non_int_bare_num = tokens.0.iter().any(|t| {
                matches!(
                    t,
                    TokenOrValue::Token(LightningToken::Number {
                        int_value: None,
                        ..
                    })
                )
            });
            if !has_non_int_bare_num {
                let raw_str = serialize_css_tokens(&owned);
                if let Some(parsed) = parse_font_feature_settings_str(&raw_str) {
                    push_decl(
                        out,
                        ord,
                        PropertyIdKind::FontFeatureSettings,
                        PropertyValue::FontFeatureSettings(parsed),
                        imp,
                    );
                }
            }
        }
        "font-variation-settings" => {
            let raw_str = serialize_css_tokens(&owned);
            if let Some(parsed) = parse_font_variation_settings_str(&raw_str) {
                push_decl(
                    out,
                    ord,
                    PropertyIdKind::FontVariationSettings,
                    PropertyValue::FontVariationSettings(parsed),
                    imp,
                );
            }
        }
        "font-size-adjust" => {
            if non_ws.len() == 1 {
                match &non_ws[0] {
                    CssToken::Ident(id) if id.as_str().eq_ignore_ascii_case("none") => {
                        push_decl(
                            out,
                            ord,
                            PropertyIdKind::FontSizeAdjust,
                            PropertyValue::OptNumber(None),
                            imp,
                        );
                    }
                    CssToken::Number(n) if n.is_finite() && *n >= 0.0 => {
                        push_decl(
                            out,
                            ord,
                            PropertyIdKind::FontSizeAdjust,
                            PropertyValue::OptNumber(Some(*n)),
                            imp,
                        );
                    }
                    _ => {}
                }
            }
        }
        "text-anchor" => {
            if let [CssToken::Ident(id)] = non_ws.as_slice() {
                let v = match id.as_str().to_ascii_lowercase().as_str() {
                    "start" => Some(crate::values::SvgTextAnchor::Start),
                    "middle" => Some(crate::values::SvgTextAnchor::Middle),
                    "end" => Some(crate::values::SvgTextAnchor::End),
                    _ => None,
                };
                if let Some(ta) = v {
                    push_decl(
                        out,
                        ord,
                        PropertyIdKind::SvgTextAnchor,
                        PropertyValue::SvgTextAnchor(ta),
                        imp,
                    );
                }
            }
        }
        "dominant-baseline" => {
            if let [CssToken::Ident(id)] = non_ws.as_slice() {
                let v = match id.as_str().to_ascii_lowercase().as_str() {
                    "auto" => Some(crate::values::SvgDominantBaseline::Auto),
                    "ideographic" => Some(crate::values::SvgDominantBaseline::Ideographic),
                    "alphabetic" => Some(crate::values::SvgDominantBaseline::Alphabetic),
                    "hanging" => Some(crate::values::SvgDominantBaseline::Hanging),
                    "mathematical" => Some(crate::values::SvgDominantBaseline::Mathematical),
                    "central" => Some(crate::values::SvgDominantBaseline::Central),
                    "middle" => Some(crate::values::SvgDominantBaseline::Middle),
                    "text-bottom" => Some(crate::values::SvgDominantBaseline::TextBottom),
                    "text-top" => Some(crate::values::SvgDominantBaseline::TextTop),
                    _ => None,
                };
                if let Some(db) = v {
                    push_decl(
                        out,
                        ord,
                        PropertyIdKind::SvgDominantBaseline,
                        PropertyValue::SvgDominantBaseline(db),
                        imp,
                    );
                }
            }
        }
        "list-style-image" | "border-image-source" | "background-image" => {
            let raw_str = serialize_css_tokens(&owned);
            let trimmed = raw_str.trim();
            let kind = match name {
                "list-style-image" => PropertyIdKind::ListStyleImage,
                "border-image-source" => PropertyIdKind::BorderImageSource,
                _ => PropertyIdKind::BackgroundLayers,
            };
            if let Some((val, _)) = parse_values4_property(kind, trimmed) {
                push_decl(out, ord, kind, val, imp);
            }
        }
        _ => missing.push(MissingCssFeature::new(MissingCssKind::Property, name)),
    }
}

/// Parses CSS Fonts L3/L4 `font-feature-settings`: `normal | <feature-tag-value>#`
/// where `<feature-tag-value> = <opentype-tag> [ <integer [0,∞]> | on | off ]?`.
pub(crate) fn parse_font_feature_settings_str(
    raw: &str,
) -> Option<Option<Vec<(InternedStr, crate::values_math::MathNode)>>> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("normal") {
        return Some(None);
    }
    let parts = crate::substitution::split_top_level(trimmed, b',');
    if parts.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(parts.len());
    for part in parts {
        let p = part.trim();
        if p.is_empty() {
            return None;
        }
        let mut pi = cssparser::ParserInput::new(p);
        let mut parser = cssparser::Parser::new(&mut pi);
        let tag_str = match parser.next() {
            Ok(cssparser::Token::QuotedString(s)) => s.as_ref().to_string(),
            _ => return None,
        };
        if tag_str.len() != 4 || !tag_str.bytes().all(|b| (0x20..=0x7E).contains(&b)) {
            return None;
        }
        let rest = parser.current_line()[parser.position().byte_index()..].trim();
        let val_node = if rest.is_empty() || rest.eq_ignore_ascii_case("on") {
            crate::values_math::MathNode::Number(1.0)
        } else if rest.eq_ignore_ascii_case("off") {
            crate::values_math::MathNode::Number(0.0)
        } else if crate::values_math::has_values4_syntax(rest) {
            // CSS Values L4 §10.9: math functions in `<integer>` context accept `<number>`
            // and round to the nearest integer (half toward +∞) at computed-value time.
            let pm =
                crate::values_math::parse_math_value(rest, crate::values_math::ValueKind::Number)?;
            pm.node
        } else {
            // Bare `<integer [0,∞]>`: CSS Syntax L3 §4.3.3 requires an integer token
            // (rejecting scientific notation like `1e1` and decimals like `10.1`).
            let mut npi = cssparser::ParserInput::new(rest);
            let mut np = cssparser::Parser::new(&mut npi);
            let iv = match np.next() {
                Ok(cssparser::Token::Number {
                    int_value: Some(v), ..
                }) if *v >= 0 => *v,
                _ => return None,
            };
            if !np.is_exhausted() {
                return None;
            }
            crate::values_math::MathNode::Number(f64::from(iv))
        };
        out.push((intern_str(&tag_str), val_node));
    }
    Some(Some(out))
}

/// Parses CSS Fonts L4 `font-variation-settings`: `normal | [ <string> <number> ]#`.
pub(crate) fn parse_font_variation_settings_str(
    raw: &str,
) -> Option<Option<Vec<(InternedStr, crate::values_math::MathNode)>>> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("normal") {
        return Some(None);
    }
    let parts = crate::substitution::split_top_level(trimmed, b',');
    if parts.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(parts.len());
    for part in parts {
        let p = part.trim();
        if p.is_empty() {
            return None;
        }
        let mut pi = cssparser::ParserInput::new(p);
        let mut parser = cssparser::Parser::new(&mut pi);
        let tag_str = match parser.next() {
            Ok(cssparser::Token::QuotedString(s)) => s.as_ref().to_string(),
            _ => return None,
        };
        if tag_str.len() != 4 || !tag_str.bytes().all(|b| (0x20..=0x7E).contains(&b)) {
            return None;
        }
        let rest = parser.current_line()[parser.position().byte_index()..].trim();
        if rest.is_empty() {
            return None;
        }
        let pm = crate::values_math::parse_math_value(rest, crate::values_math::ValueKind::Number)?;
        out.push((intern_str(&tag_str), pm.node));
    }
    Some(Some(out))
}
