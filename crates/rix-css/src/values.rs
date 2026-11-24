//! Typed CSS value representations and direct `lightningcss` AST converters.
//!
//! All converters consume `lightningcss` parsed values directly without
//! serializing intermediate CSS strings.

use lightningcss::printer::PrinterOptions;
use lightningcss::properties::{
    align::{
        AlignContent, AlignItems, AlignSelf, ContentDistribution, ContentPosition, JustifyContent,
        JustifyItems, JustifySelf, SelfPosition,
    },
    animation::{
        Animation as LightningAnimation, AnimationDirection as LightningAnimDirection,
        AnimationFillMode as LightningAnimFill, AnimationIterationCount as LightningAnimIter,
        AnimationName as LightningAnimName, AnimationPlayState as LightningAnimPlay,
    },
    background::{Background, BackgroundRepeatKeyword, BackgroundSize},
    border::{BorderSideWidth, LineStyle as LightningLineStyle},
    box_shadow::BoxShadow,
    display::{Display, DisplayInside, DisplayKeyword, DisplayOutside, Visibility},
    effects::{BlendMode as LightningBlendMode, Filter},
    flex::{FlexDirection, FlexWrap},
    font::{
        AbsoluteFontSize, AbsoluteFontWeight, FontFamily, FontSize, FontStyle, FontWeight,
        GenericFontFamily, LineHeight, RelativeFontSize,
    },
    grid::{GridAutoFlow, RepeatCount, TrackBreadth, TrackListItem, TrackSize, TrackSizing},
    masking::ClipPath as LightningClipPath,
    overflow::OverflowKeyword,
    position::{Position, ZIndex},
    size::{MaxSize, Size},
    text::{
        Direction, OverflowWrap, Spacing, TextAlign, TextDecorationLine as LightningTextDeco,
        TextTransform, TextTransformCase, WhiteSpace, WordBreak,
    },
    transform::Transform,
    transition::Transition as LightningTransition,
    ui::{Cursor, CursorKeyword, UserSelect},
};
use lightningcss::traits::ToCss;
use lightningcss::values::{
    alpha::AlphaValue,
    angle::Angle,
    calc::{Calc, MathFunction},
    color::CssColor as LightningCssColor,
    easing::{EasingFunction, StepPosition},
    gradient::{EndingShape, Gradient, GradientItem, LineDirection},
    image::Image,
    length::{Length, LengthPercentage as LightningLp, LengthPercentageOrAuto, LengthValue},
    percentage::{DimensionPercentage, NumberOrPercentage},
    position::{
        HorizontalPosition, HorizontalPositionKeyword, Position as LightningPosition,
        VerticalPosition, VerticalPositionKeyword,
    },
    shape::{BasicShape, FillRule, ShapeRadius},
    size::Size2D,
    time::Time,
};
use rix_core::{intern_str, InternedStr};
use serde::{Deserialize, Serialize};

/// Compact 4-byte sRGBA color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct ColorRgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl ColorRgba {
    pub const TRANSPARENT: Self = Self::rgba(0, 0, 0, 0);
    pub const BLACK: Self = Self::rgba(0, 0, 0, 255);
    pub const WHITE: Self = Self::rgba(255, 255, 255, 255);

    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    #[must_use]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    #[must_use]
    pub fn to_f32_rgba(self) -> [f32; 4] {
        [
            f32::from(self.r) / 255.0,
            f32::from(self.g) / 255.0,
            f32::from(self.b) / 255.0,
            f32::from(self.a) / 255.0,
        ]
    }
}

/// Specified or computed CSS color value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CssColor {
    CurrentColor,
    Rgba(ColorRgba),
    LightDark(ColorRgba, ColorRgba),
    SystemAccent,
}

impl Default for CssColor {
    fn default() -> Self {
        Self::Rgba(ColorRgba::TRANSPARENT)
    }
}

impl CssColor {
    #[must_use]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self::Rgba(ColorRgba::rgba(r, g, b, a))
    }

    /// Resolves `currentColor` and `light-dark()` into a concrete sRGBA color.
    #[must_use]
    pub const fn resolve(self, current_color: ColorRgba, dark_mode: bool) -> ColorRgba {
        match self {
            Self::CurrentColor => current_color,
            Self::Rgba(c) => c,
            Self::LightDark(light, dark) => {
                if dark_mode {
                    dark
                } else {
                    light
                }
            }
            Self::SystemAccent => ColorRgba::rgb(0, 96, 223),
        }
    }
}

/// Converts a `lightningcss` CSS Color 4/5 AST node into a compact [`CssColor`].
#[must_use]
pub fn convert_color(color: &LightningCssColor) -> Option<CssColor> {
    match color {
        LightningCssColor::CurrentColor => Some(CssColor::CurrentColor),
        LightningCssColor::RGBA(rgba) => {
            Some(CssColor::rgba(rgba.red, rgba.green, rgba.blue, rgba.alpha))
        }
        LightningCssColor::LightDark(light, dark) => {
            let l = convert_color(light)?.resolve(ColorRgba::BLACK, false);
            let d = convert_color(dark)?.resolve(ColorRgba::WHITE, true);
            Some(CssColor::LightDark(l, d))
        }
        LightningCssColor::System(_) => Some(CssColor::SystemAccent),
        other => match other.to_rgb().ok()? {
            LightningCssColor::RGBA(rgba) => {
                Some(CssColor::rgba(rgba.red, rgba.green, rgba.blue, rgba.alpha))
            }
            _ => None,
        },
    }
}

/// CSS relative or absolute length unit (W3C CSS Values & Units Level 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum LengthUnit {
    Px,
    Rem,
    Em,
    Ex,
    Rex,
    Ch,
    Rch,
    Cap,
    Rcap,
    Ic,
    Ric,
    Lh,
    Rlh,
    Vw,
    Vh,
    Vi,
    Vb,
    Vmin,
    Vmax,
    Svw,
    Svh,
    Svi,
    Svb,
    Svmin,
    Svmax,
    Lvw,
    Lvh,
    Lvi,
    Lvb,
    Lvmin,
    Lvmax,
    Dvw,
    Dvh,
    Dvi,
    Dvb,
    Dvmin,
    Dvmax,
    Cqw,
    Cqh,
    Cqi,
    Cqb,
    Cqmin,
    Cqmax,
}

impl LengthUnit {
    #[must_use]
    #[rustfmt::skip]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Px => "px", Self::Rem => "rem", Self::Em => "em", Self::Ex => "ex", Self::Rex => "rex",
            Self::Ch => "ch", Self::Rch => "rch", Self::Cap => "cap", Self::Rcap => "rcap",
            Self::Ic => "ic", Self::Ric => "ric", Self::Lh => "lh", Self::Rlh => "rlh",
            Self::Vw => "vw", Self::Vh => "vh", Self::Vi => "vi", Self::Vb => "vb",
            Self::Vmin => "vmin", Self::Vmax => "vmax", Self::Svw => "svw", Self::Svh => "svh",
            Self::Svi => "svi", Self::Svb => "svb", Self::Svmin => "svmin", Self::Svmax => "svmax",
            Self::Lvw => "lvw", Self::Lvh => "lvh", Self::Lvi => "lvi", Self::Lvb => "lvb",
            Self::Lvmin => "lvmin", Self::Lvmax => "lvmax", Self::Dvw => "dvw", Self::Dvh => "dvh",
            Self::Dvi => "dvi", Self::Dvb => "dvb", Self::Dvmin => "dvmin", Self::Dvmax => "dvmax",
            Self::Cqw => "cqw", Self::Cqh => "cqh", Self::Cqi => "cqi", Self::Cqb => "cqb",
            Self::Cqmin => "cqmin", Self::Cqmax => "cqmax",
        }
    }

    #[must_use]
    #[rustfmt::skip]
    pub fn from_unit_str(unit: &str) -> Option<Self> {
        match unit.to_ascii_lowercase().as_str() {
            "px" => Some(Self::Px), "rem" => Some(Self::Rem), "em" => Some(Self::Em),
            "ex" => Some(Self::Ex), "rex" => Some(Self::Rex), "ch" => Some(Self::Ch),
            "rch" => Some(Self::Rch), "cap" => Some(Self::Cap), "rcap" => Some(Self::Rcap),
            "ic" => Some(Self::Ic), "ric" => Some(Self::Ric), "lh" => Some(Self::Lh),
            "rlh" => Some(Self::Rlh), "vw" => Some(Self::Vw), "vh" => Some(Self::Vh),
            "vi" => Some(Self::Vi), "vb" => Some(Self::Vb), "vmin" => Some(Self::Vmin),
            "vmax" => Some(Self::Vmax), "svw" => Some(Self::Svw), "svh" => Some(Self::Svh),
            "svi" => Some(Self::Svi), "svb" => Some(Self::Svb), "svmin" => Some(Self::Svmin),
            "svmax" => Some(Self::Svmax), "lvw" => Some(Self::Lvw), "lvh" => Some(Self::Lvh),
            "lvi" => Some(Self::Lvi), "lvb" => Some(Self::Lvb), "lvmin" => Some(Self::Lvmin),
            "lvmax" => Some(Self::Lvmax), "dvw" => Some(Self::Dvw), "dvh" => Some(Self::Dvh),
            "dvi" => Some(Self::Dvi), "dvb" => Some(Self::Dvb), "dvmin" => Some(Self::Dvmin),
            "dvmax" => Some(Self::Dvmax), "cqw" => Some(Self::Cqw), "cqh" => Some(Self::Cqh),
            "cqi" => Some(Self::Cqi), "cqb" => Some(Self::Cqb), "cqmin" => Some(Self::Cqmin),
            "cqmax" => Some(Self::Cqmax),
            _ => None,
        }
    }

    #[must_use]
    pub fn resolve_px(self, value: f32, ctx: &EvalContext) -> f32 {
        match self {
            Self::Px => value,
            Self::Rem => value * ctx.root_font_size_px,
            Self::Em => value * ctx.font_size_px,
            Self::Ex => value * ctx.font_size_px * 0.5,
            Self::Ch => value * ctx.ch_px.unwrap_or(ctx.font_size_px * 0.5),
            Self::Rex | Self::Rch => value * ctx.root_font_size_px * 0.5,
            Self::Cap => value * ctx.font_size_px * 0.7,
            Self::Rcap => value * ctx.root_font_size_px * 0.7,
            Self::Ic => value * ctx.font_size_px,
            Self::Ric => value * ctx.root_font_size_px,
            Self::Lh => value * ctx.line_height_px,
            Self::Rlh => value * ctx.root_line_height_px,
            Self::Vw
            | Self::Vi
            | Self::Svw
            | Self::Svi
            | Self::Lvw
            | Self::Lvi
            | Self::Dvw
            | Self::Dvi => value * ctx.viewport_width_px * 0.01,
            Self::Vh
            | Self::Vb
            | Self::Svh
            | Self::Svb
            | Self::Lvh
            | Self::Lvb
            | Self::Dvh
            | Self::Dvb => value * ctx.viewport_height_px * 0.01,
            Self::Vmin | Self::Svmin | Self::Lvmin | Self::Dvmin => {
                value * ctx.viewport_width_px.min(ctx.viewport_height_px) * 0.01
            }
            Self::Vmax | Self::Svmax | Self::Lvmax | Self::Dvmax => {
                value * ctx.viewport_width_px.max(ctx.viewport_height_px) * 0.01
            }
            Self::Cqw | Self::Cqi => value * ctx.container_width_px * 0.01,
            Self::Cqh | Self::Cqb => value * ctx.container_height_px * 0.01,
            Self::Cqmin => value * ctx.container_width_px.min(ctx.container_height_px) * 0.01,
            Self::Cqmax => value * ctx.container_width_px.max(ctx.container_height_px) * 0.01,
        }
    }
}

/// Evaluation environment for resolving relative CSS units and `calc()` expressions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct EvalContext {
    pub font_size_px: f32,
    pub root_font_size_px: f32,
    pub line_height_px: f32,
    pub root_line_height_px: f32,
    pub viewport_width_px: f32,
    pub viewport_height_px: f32,
    pub container_width_px: f32,
    pub container_height_px: f32,
    pub ch_px: Option<f32>,
    pub sibling_index: u32,
    pub sibling_count: u32,
    pub random_node_id: u64,
    pub random_doc_seed: u64,
}

impl PartialEq for EvalContext {
    fn eq(&self, other: &Self) -> bool {
        self.font_size_px == other.font_size_px
            && self.root_font_size_px == other.root_font_size_px
            && self.line_height_px == other.line_height_px
            && self.root_line_height_px == other.root_line_height_px
            && self.viewport_width_px == other.viewport_width_px
            && self.viewport_height_px == other.viewport_height_px
            && self.container_width_px == other.container_width_px
            && self.container_height_px == other.container_height_px
            && self.ch_px == other.ch_px
            && self.sibling_index == other.sibling_index
            && self.sibling_count == other.sibling_count
    }
}

impl Default for EvalContext {
    fn default() -> Self {
        Self {
            font_size_px: 16.0,
            root_font_size_px: 16.0,
            line_height_px: 19.2,
            root_line_height_px: 19.2,
            viewport_width_px: 1280.0,
            viewport_height_px: 720.0,
            container_width_px: 1280.0,
            container_height_px: 720.0,
            ch_px: None,
            sibling_index: 1,
            sibling_count: 1,
            random_node_id: 0,
            random_doc_seed: 0x517c_c1b7_2722_0a95,
        }
    }
}

/// Typed CSS math expression supporting full W3C CSS Values L4 calculations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CalcExpr {
    Length(f32, LengthUnit),
    Percent(f32),
    Number(f32),
    Sum(Box<CalcExpr>, Box<CalcExpr>),
    Product(f32, Box<CalcExpr>),
    Min(Vec<CalcExpr>),
    Max(Vec<CalcExpr>),
    Clamp(Box<CalcExpr>, Box<CalcExpr>, Box<CalcExpr>),
    Abs(Box<CalcExpr>),
    Sign(Box<CalcExpr>),
    Rem(Box<CalcExpr>, Box<CalcExpr>),
    Mod(Box<CalcExpr>, Box<CalcExpr>),
    Math(Box<crate::values_math::MathNode>),
}

impl CalcExpr {
    #[must_use]
    pub fn has_percentage(&self) -> bool {
        match self {
            Self::Percent(_) => true,
            Self::Length(_, _) | Self::Number(_) => false,
            Self::Sum(a, b) | Self::Rem(a, b) | Self::Mod(a, b) => {
                a.has_percentage() || b.has_percentage()
            }
            Self::Product(_, a) | Self::Abs(a) | Self::Sign(a) => a.has_percentage(),
            Self::Min(args) | Self::Max(args) => args.iter().any(Self::has_percentage),
            Self::Clamp(a, b, c) => a.has_percentage() || b.has_percentage() || c.has_percentage(),
            Self::Math(node) => node.has_percentage(),
        }
    }

    /// Evaluates the expression to pixels given a percentage basis and unit context.
    #[must_use]
    pub fn resolve_px(&self, percent_basis: f32, ctx: &EvalContext) -> f32 {
        match self {
            Self::Length(v, unit) => unit.resolve_px(*v, ctx),
            Self::Percent(p) => *p * percent_basis,
            Self::Number(n) => *n,
            Self::Sum(a, b) => a.resolve_px(percent_basis, ctx) + b.resolve_px(percent_basis, ctx),
            Self::Product(k, a) => *k * a.resolve_px(percent_basis, ctx),
            Self::Min(args) => args
                .iter()
                .map(|a| a.resolve_px(percent_basis, ctx))
                .fold(f32::INFINITY, f32::min),
            Self::Max(args) => args
                .iter()
                .map(|a| a.resolve_px(percent_basis, ctx))
                .fold(f32::NEG_INFINITY, f32::max),
            Self::Clamp(min_e, val_e, max_e) => {
                let lo = min_e.resolve_px(percent_basis, ctx);
                let val = val_e.resolve_px(percent_basis, ctx);
                let hi = max_e.resolve_px(percent_basis, ctx);
                val.min(hi).max(lo)
            }
            Self::Abs(v) => v.resolve_px(percent_basis, ctx).abs(),
            Self::Sign(v) => {
                let x = v.resolve_px(percent_basis, ctx);
                if x > 0.0 {
                    1.0
                } else if x < 0.0 {
                    -1.0
                } else {
                    0.0
                }
            }
            Self::Rem(a, b) => {
                let bv = b.resolve_px(percent_basis, ctx);
                if bv == 0.0 {
                    0.0
                } else {
                    a.resolve_px(percent_basis, ctx) % bv
                }
            }
            Self::Mod(a, b) => {
                let bv = b.resolve_px(percent_basis, ctx);
                if bv == 0.0 {
                    0.0
                } else {
                    a.resolve_px(percent_basis, ctx).rem_euclid(bv.abs()) * bv.signum()
                }
            }
            Self::Math(node) => node.resolve_px(percent_basis, ctx),
        }
    }
}

/// Compact `<length-percentage>` value.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum LengthPercentage {
    #[default]
    Zero,
    Length(f32, LengthUnit),
    Percent(f32),
    Calc(Box<CalcExpr>),
}

impl LengthPercentage {
    #[must_use]
    pub const fn px(value: f32) -> Self {
        if value == 0.0 {
            Self::Zero
        } else {
            Self::Length(value, LengthUnit::Px)
        }
    }

    #[must_use]
    pub fn is_negative_literal(&self) -> bool {
        match self {
            Self::Length(v, _) | Self::Percent(v) => *v < 0.0,
            Self::Zero | Self::Calc(_) => false,
        }
    }

    #[must_use]
    pub fn has_percentage(&self) -> bool {
        match self {
            Self::Zero | Self::Length(_, _) => false,
            Self::Percent(_) => true,
            Self::Calc(expr) => expr.has_percentage(),
        }
    }

    #[must_use]
    pub fn resolve_px(&self, percent_basis: f32, ctx: &EvalContext) -> f32 {
        match self {
            Self::Zero => 0.0,
            Self::Length(v, unit) => unit.resolve_px(*v, ctx),
            Self::Percent(p) => *p * percent_basis,
            Self::Calc(expr) => expr.resolve_px(percent_basis, ctx),
        }
    }
}

/// `<length-percentage> | auto` value.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum LengthPercentageAuto {
    #[default]
    Auto,
    Value(LengthPercentage),
}

/// Preferred, min, or max sizing value.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum SizeValue {
    #[default]
    Auto,
    None,
    Length(LengthPercentage),
    MinContent,
    MaxContent,
    FitContent(Option<LengthPercentage>),
    Stretch,
    Content,
    CalcSize(Box<crate::values_math::CalcSizeExpr>),
}

/// CSS Values L5 `interpolate-size` inherited keyword property.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum InterpolateSizeMode {
    #[default]
    NumericOnly,
    AllowKeywords,
}

/// CSS Transitions L2 `transition-behavior` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum TransitionBehaviorMode {
    #[default]
    Normal,
    AllowDiscrete,
}

/// CSS Animations L2 `animation-composition` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum AnimationCompositionMode {
    #[default]
    Replace,
    Add,
    Accumulate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum DisplayMode {
    Block,
    #[default]
    Inline,
    InlineBlock,
    FlowRoot,
    Flex,
    InlineFlex,
    Grid,
    InlineGrid,
    GridLanes,
    InlineGridLanes,
    InlineGridLanesMulti,
    Table,
    InlineTable,
    TableRowGroup,
    TableHeaderGroup,
    TableFooterGroup,
    TableRow,
    TableColumnGroup,
    TableColumn,
    TableCell,
    TableCaption,
    ListItem,
    InlineListItem,
    FlowRootListItem,
    InlineFlowRootListItem,
    Ruby,
    BlockRuby,
    RubyBase,
    RubyText,
    RubyBaseContainer,
    RubyTextContainer,
    RunIn,
    RunInFlowRoot,
    RunInFlex,
    RunInGrid,
    RunInTable,
    RunInRuby,
    RunInListItem,
    RunInFlowRootListItem,
    Math,
    BlockMath,
    Contents,
    None,
}

impl DisplayMode {
    /// Parses a CSS Display Level 3/4 `<display>` value per W3C grammar.
    #[must_use]
    #[rustfmt::skip]
    pub fn parse_css(raw: &str) -> Option<Self> {
        let toks: Vec<String> = raw.split_ascii_whitespace().map(str::to_ascii_lowercase).collect();
        if toks.is_empty() || toks.len() > 3 { return None; }
        if toks.len() == 1 {
            return match toks[0].as_str() {
                "block" | "flow" => Some(Self::Block),
                "inline" => Some(Self::Inline),
                "inline-block" => Some(Self::InlineBlock),
                "flow-root" => Some(Self::FlowRoot),
                "flex" => Some(Self::Flex),
                "inline-flex" => Some(Self::InlineFlex),
                "grid" => Some(Self::Grid),
                "inline-grid" => Some(Self::InlineGrid),
                "grid-lanes" => Some(Self::GridLanes),
                "inline-grid-lanes" => Some(Self::InlineGridLanes),
                "table" => Some(Self::Table),
                "inline-table" => Some(Self::InlineTable),
                "table-row-group" => Some(Self::TableRowGroup),
                "table-header-group" => Some(Self::TableHeaderGroup),
                "table-footer-group" => Some(Self::TableFooterGroup),
                "table-row" => Some(Self::TableRow),
                "table-column-group" => Some(Self::TableColumnGroup),
                "table-column" => Some(Self::TableColumn),
                "table-cell" => Some(Self::TableCell),
                "table-caption" => Some(Self::TableCaption),
                "list-item" => Some(Self::ListItem),
                "ruby" => Some(Self::Ruby),
                "ruby-base" => Some(Self::RubyBase),
                "ruby-text" => Some(Self::RubyText),
                "ruby-base-container" => Some(Self::RubyBaseContainer),
                "ruby-text-container" => Some(Self::RubyTextContainer),
                "run-in" => Some(Self::RunIn),
                "math" => Some(Self::Math),
                "contents" => Some(Self::Contents),
                "none" => Some(Self::None),
                _ => None,
            };
        }
        // Multi-keyword grammar: `[ <display-outside> || <display-inside> ]` or
        // `<display-outside>? && [ flow | flow-root ]? && list-item`.
        let (mut outside, mut inside, mut list_item) = (None::<&str>, None::<&str>, false);
        for t in &toks {
            match t.as_str() {
                "block" | "inline" | "run-in" => {
                    if outside.is_some() { return None; }
                    outside = Some(t.as_str());
                }
                "flow" | "flow-root" | "table" | "flex" | "grid" | "grid-lanes" | "ruby" | "math" => {
                    if inside.is_some() { return None; }
                    inside = Some(t.as_str());
                }
                "list-item" => {
                    if list_item { return None; }
                    list_item = true;
                }
                _ => return None,
            }
        }
        if list_item {
            if !matches!(inside, None | Some("flow" | "flow-root")) { return None; }
            return Some(match (outside.unwrap_or("block"), inside.unwrap_or("flow")) {
                ("block", "flow") => Self::ListItem,
                ("block", "flow-root") => Self::FlowRootListItem,
                ("inline", "flow") => Self::InlineListItem,
                ("inline", "flow-root") => Self::InlineFlowRootListItem,
                ("run-in", "flow") => Self::RunInListItem,
                ("run-in", "flow-root") => Self::RunInFlowRootListItem,
                _ => return None,
            });
        }
        let (out_k, in_k) = (outside?, inside?);
        Some(match (out_k, in_k) {
            ("block", "flow") => Self::Block,
            ("block", "flow-root") => Self::FlowRoot,
            ("block", "flex") => Self::Flex,
            ("block", "grid") => Self::Grid,
            ("block", "grid-lanes") => Self::GridLanes,
            ("block", "table") => Self::Table,
            ("block", "ruby") => Self::BlockRuby,
            ("block", "math") => Self::BlockMath,
            ("inline", "flow") => Self::Inline,
            ("inline", "flow-root") => Self::InlineBlock,
            ("inline", "flex") => Self::InlineFlex,
            ("inline", "grid") => Self::InlineGrid,
            ("inline", "grid-lanes") => Self::InlineGridLanesMulti,
            ("inline", "table") => Self::InlineTable,
            ("inline", "ruby") => Self::Ruby,
            ("inline", "math") => Self::Math,
            ("run-in", "flow") => Self::RunIn,
            ("run-in", "flow-root") => Self::RunInFlowRoot,
            ("run-in", "flex") => Self::RunInFlex,
            ("run-in", "grid") => Self::RunInGrid,
            ("run-in", "table") => Self::RunInTable,
            ("run-in", "ruby") => Self::RunInRuby,
            _ => return None,
        })
    }

    /// Canonical shortest W3C CSS Display Level 3 serialization.
    #[must_use]
    #[rustfmt::skip]
    pub const fn to_css_string(self) -> &'static str {
        match self {
            Self::Block => "block", Self::Inline => "inline", Self::InlineBlock => "inline-block",
            Self::FlowRoot => "flow-root", Self::Flex => "flex", Self::InlineFlex => "inline-flex",
            Self::Grid => "grid", Self::InlineGrid => "inline-grid",
            Self::GridLanes => "grid-lanes", Self::InlineGridLanes => "inline-grid-lanes",
            Self::InlineGridLanesMulti => "inline grid-lanes",
            Self::Table => "table",
            Self::InlineTable => "inline-table", Self::TableRowGroup => "table-row-group",
            Self::TableHeaderGroup => "table-header-group", Self::TableFooterGroup => "table-footer-group",
            Self::TableRow => "table-row", Self::TableColumnGroup => "table-column-group",
            Self::TableColumn => "table-column", Self::TableCell => "table-cell",
            Self::TableCaption => "table-caption", Self::ListItem => "list-item",
            Self::InlineListItem => "inline list-item", Self::FlowRootListItem => "flow-root list-item",
            Self::InlineFlowRootListItem => "inline flow-root list-item",
            Self::Ruby => "ruby", Self::BlockRuby => "block ruby",
            Self::RubyBase => "ruby-base", Self::RubyText => "ruby-text",
            Self::RubyBaseContainer => "ruby-base-container", Self::RubyTextContainer => "ruby-text-container",
            Self::RunIn => "run-in", Self::RunInFlowRoot => "run-in flow-root",
            Self::RunInFlex => "run-in flex", Self::RunInGrid => "run-in grid",
            Self::RunInTable => "run-in table", Self::RunInRuby => "run-in ruby",
            Self::RunInListItem => "run-in list-item", Self::RunInFlowRootListItem => "run-in flow-root list-item",
            Self::Math => "math", Self::BlockMath => "block math",
            Self::Contents => "contents", Self::None => "none",
        }
    }

    /// CSS Display L3 §2.7 / CSS 2.1 §9.7 blockification (`float`, `position: absolute|fixed`,
    /// root element, flex/grid items).
    #[must_use]
    #[rustfmt::skip]
    pub const fn blockify(self) -> Self {
        match self {
            Self::Inline | Self::InlineBlock | Self::TableRowGroup | Self::TableHeaderGroup
            | Self::TableFooterGroup | Self::TableRow | Self::TableColumnGroup | Self::TableColumn
            | Self::TableCell | Self::TableCaption | Self::Ruby | Self::RubyBase | Self::RubyText
            | Self::RubyBaseContainer | Self::RubyTextContainer | Self::RunIn | Self::BlockRuby => Self::Block,
            Self::InlineTable | Self::RunInTable => Self::Table,
            Self::InlineFlex | Self::RunInFlex => Self::Flex,
            Self::InlineGrid | Self::RunInGrid => Self::Grid,
            Self::InlineGridLanes | Self::InlineGridLanesMulti => Self::GridLanes,
            Self::RunInFlowRoot => Self::FlowRoot,
            Self::RunInRuby => Self::BlockRuby,
            Self::InlineListItem | Self::RunInListItem => Self::ListItem,
            Self::InlineFlowRootListItem | Self::RunInFlowRootListItem => Self::FlowRootListItem,
            Self::Math => Self::BlockMath,
            other => other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum OverlayMode {
    #[default]
    None,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum ReadingFlowMode {
    #[default]
    Normal,
    FlexVisual,
    FlexFlow,
    GridRows,
    GridColumns,
    GridOrder,
    GridLanesFlow,
    GridLanesTrack,
    GridLanesOrder,
    SourceOrder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum TextJustifyMode {
    #[default]
    Auto,
    None,
    InterWord,
    InterCharacter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum TextAlignLastMode {
    #[default]
    Auto,
    Start,
    End,
    Left,
    Right,
    Center,
    Justify,
    MatchParent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum PositionMode {
    #[default]
    Static,
    Relative,
    Absolute,
    Fixed,
    Sticky,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum FloatMode {
    #[default]
    None,
    Left,
    Right,
    InlineStart,
    InlineEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum ClearMode {
    #[default]
    None,
    Left,
    Right,
    Both,
    InlineStart,
    InlineEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum BoxSizingMode {
    #[default]
    ContentBox,
    BorderBox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum OverflowMode {
    #[default]
    Visible,
    Hidden,
    Clip,
    Scroll,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct OverflowPair {
    pub x: OverflowMode,
    pub y: OverflowMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum VisibilityMode {
    #[default]
    Visible,
    Hidden,
    Collapse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum PointerEventsMode {
    #[default]
    Auto,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum CursorKind {
    #[default]
    Auto,
    Default,
    Pointer,
    Text,
    Move,
    NotAllowed,
    Grab,
    Grabbing,
    EwResize,
    NsResize,
    NeswResize,
    NwseResize,
    ColResize,
    RowResize,
    Wait,
    Progress,
    Crosshair,
    Help,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ZIndexValue {
    #[default]
    Auto,
    Integer(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum LineStyle {
    #[default]
    None,
    Hidden,
    Dotted,
    Dashed,
    Solid,
    Double,
    Groove,
    Ridge,
    Inset,
    Outset,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum FontStyleMode {
    #[default]
    Normal,
    Italic,
    Oblique(f32),
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum LineHeightValue {
    #[default]
    Normal,
    Number(f32),
    Length(LengthPercentage),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum TextAlignMode {
    #[default]
    Start,
    End,
    Left,
    Right,
    Center,
    Justify,
    MatchParent,
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct TextDecorationLine: u8 {
        const NONE         = 0;
        const UNDERLINE    = 1 << 0;
        const OVERLINE     = 1 << 1;
        const LINE_THROUGH = 1 << 2;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum WhiteSpaceMode {
    #[default]
    Normal,
    Nowrap,
    Pre,
    PreWrap,
    PreLine,
    BreakSpaces,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum WordBreakMode {
    #[default]
    Normal,
    BreakAll,
    KeepAll,
    BreakWord,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum OverflowWrapMode {
    #[default]
    Normal,
    BreakWord,
    Anywhere,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum TextTransformMode {
    #[default]
    None,
    Capitalize,
    Uppercase,
    Lowercase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum DirectionMode {
    #[default]
    Ltr,
    Rtl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum WritingMode {
    #[default]
    HorizontalTb,
    VerticalRl,
    VerticalLr,
    SidewaysRl,
    SidewaysLr,
}

impl WritingMode {
    #[must_use]
    pub const fn is_vertical(self) -> bool {
        !matches!(self, Self::HorizontalTb)
    }

    #[must_use]
    pub const fn as_css_str(self) -> &'static str {
        match self {
            Self::HorizontalTb => "horizontal-tb",
            Self::VerticalRl => "vertical-rl",
            Self::VerticalLr => "vertical-lr",
            Self::SidewaysRl => "sideways-rl",
            Self::SidewaysLr => "sideways-lr",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum TextOrientationMode {
    #[default]
    Mixed,
    Upright,
    Sideways,
}

impl TextOrientationMode {
    #[must_use]
    pub const fn as_css_str(self) -> &'static str {
        match self {
            Self::Mixed => "mixed",
            Self::Upright => "upright",
            Self::Sideways => "sideways",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum ResizeMode {
    #[default]
    None,
    Both,
    Horizontal,
    Vertical,
    Block,
    Inline,
}

impl ResizeMode {
    #[must_use]
    pub const fn as_css_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Both => "both",
            Self::Horizontal => "horizontal",
            Self::Vertical => "vertical",
            Self::Block => "block",
            Self::Inline => "inline",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum CaptionSideMode {
    #[default]
    Top,
    Bottom,
    BlockStart,
    BlockEnd,
    InlineStart,
    InlineEnd,
}

impl CaptionSideMode {
    #[must_use]
    pub const fn as_css_str(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::BlockStart => "block-start",
            Self::BlockEnd => "block-end",
            Self::InlineStart => "inline-start",
            Self::InlineEnd => "inline-end",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum OverscrollBehaviorMode {
    #[default]
    Auto,
    Contain,
    None,
}

impl OverscrollBehaviorMode {
    #[must_use]
    pub const fn as_css_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Contain => "contain",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum UserSelectMode {
    #[default]
    Auto,
    Text,
    None,
    Contain,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum FlexDirectionMode {
    #[default]
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum FlexWrapMode {
    #[default]
    NoWrap,
    Wrap,
    WrapReverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum AlignValue {
    #[default]
    Normal,
    Auto,
    Stretch,
    Start,
    End,
    FlexStart,
    FlexEnd,
    SelfStart,
    SelfEnd,
    Left,
    Right,
    Center,
    Baseline,
    LastBaseline,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
    SafeStart,
    SafeEnd,
    SafeFlexStart,
    SafeFlexEnd,
    SafeSelfStart,
    SafeSelfEnd,
    SafeLeft,
    SafeRight,
    SafeCenter,
    UnsafeStart,
    UnsafeEnd,
    UnsafeFlexStart,
    UnsafeFlexEnd,
    UnsafeSelfStart,
    UnsafeSelfEnd,
    UnsafeLeft,
    UnsafeRight,
    UnsafeCenter,
}

impl AlignValue {
    #[must_use]
    #[rustfmt::skip]
    pub const fn with_overflow(self, safe: bool, is_unsafe: bool) -> Self {
        if safe {
            match self {
                Self::Start => Self::SafeStart, Self::End => Self::SafeEnd,
                Self::FlexStart => Self::SafeFlexStart, Self::FlexEnd => Self::SafeFlexEnd,
                Self::SelfStart => Self::SafeSelfStart, Self::SelfEnd => Self::SafeSelfEnd,
                Self::Left => Self::SafeLeft, Self::Right => Self::SafeRight,
                Self::Center => Self::SafeCenter, other => other,
            }
        } else if is_unsafe {
            match self {
                Self::Start => Self::UnsafeStart, Self::End => Self::UnsafeEnd,
                Self::FlexStart => Self::UnsafeFlexStart, Self::FlexEnd => Self::UnsafeFlexEnd,
                Self::SelfStart => Self::UnsafeSelfStart, Self::SelfEnd => Self::UnsafeSelfEnd,
                Self::Left => Self::UnsafeLeft, Self::Right => Self::UnsafeRight,
                Self::Center => Self::UnsafeCenter, other => other,
            }
        } else {
            self
        }
    }

    #[must_use]
    #[rustfmt::skip]
    pub const fn base(self) -> Self {
        match self {
            Self::SafeStart | Self::UnsafeStart => Self::Start,
            Self::SafeEnd | Self::UnsafeEnd => Self::End,
            Self::SafeFlexStart | Self::UnsafeFlexStart => Self::FlexStart,
            Self::SafeFlexEnd | Self::UnsafeFlexEnd => Self::FlexEnd,
            Self::SafeSelfStart | Self::UnsafeSelfStart => Self::SelfStart,
            Self::SafeSelfEnd | Self::UnsafeSelfEnd => Self::SelfEnd,
            Self::SafeLeft | Self::UnsafeLeft => Self::Left,
            Self::SafeRight | Self::UnsafeRight => Self::Right,
            Self::SafeCenter | Self::UnsafeCenter => Self::Center,
            other => other,
        }
    }

    #[must_use]
    pub const fn is_safe(self) -> bool {
        matches!(
            self,
            Self::SafeStart
                | Self::SafeEnd
                | Self::SafeFlexStart
                | Self::SafeFlexEnd
                | Self::SafeSelfStart
                | Self::SafeSelfEnd
                | Self::SafeLeft
                | Self::SafeRight
                | Self::SafeCenter
        )
    }

    #[must_use]
    pub const fn is_unsafe(self) -> bool {
        matches!(
            self,
            Self::UnsafeStart
                | Self::UnsafeEnd
                | Self::UnsafeFlexStart
                | Self::UnsafeFlexEnd
                | Self::UnsafeSelfStart
                | Self::UnsafeSelfEnd
                | Self::UnsafeLeft
                | Self::UnsafeRight
                | Self::UnsafeCenter
        )
    }

    #[must_use]
    #[rustfmt::skip]
    pub const fn as_css_str(self) -> &'static str {
        match self {
            Self::Auto => "auto", Self::Normal => "normal", Self::Stretch => "stretch",
            Self::Start => "start", Self::End => "end",
            Self::FlexStart => "flex-start", Self::FlexEnd => "flex-end",
            Self::SelfStart => "self-start", Self::SelfEnd => "self-end",
            Self::Left => "left", Self::Right => "right", Self::Center => "center",
            Self::Baseline => "baseline", Self::LastBaseline => "last baseline",
            Self::SpaceBetween => "space-between", Self::SpaceAround => "space-around",
            Self::SpaceEvenly => "space-evenly",
            Self::SafeStart => "safe start", Self::SafeEnd => "safe end",
            Self::SafeFlexStart => "safe flex-start", Self::SafeFlexEnd => "safe flex-end",
            Self::SafeSelfStart => "safe self-start", Self::SafeSelfEnd => "safe self-end",
            Self::SafeLeft => "safe left", Self::SafeRight => "safe right",
            Self::SafeCenter => "safe center",
            Self::UnsafeStart => "unsafe start", Self::UnsafeEnd => "unsafe end",
            Self::UnsafeFlexStart => "unsafe flex-start", Self::UnsafeFlexEnd => "unsafe flex-end",
            Self::UnsafeSelfStart => "unsafe self-start", Self::UnsafeSelfEnd => "unsafe self-end",
            Self::UnsafeLeft => "unsafe left", Self::UnsafeRight => "unsafe right",
            Self::UnsafeCenter => "unsafe center",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum GridAutoFlowMode {
    #[default]
    Row,
    Column,
    RowDense,
    ColumnDense,
}

/// Typed CSS Grid line / span / named area placement (`grid-row-*`, `grid-column-*`, `grid-area`).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum GridPlacementValue {
    #[default]
    Auto,
    Named(InternedStr),
    Line {
        index: i32,
        index_math: Option<Box<crate::values_math::MathNode>>,
        name: Option<InternedStr>,
    },
    Span {
        count: u32,
        count_math: Option<Box<crate::values_math::MathNode>>,
        name: Option<InternedStr>,
    },
}

/// Typed CSS `grid-template-areas` matrix (`columns` wide, `None` for `.` unnamed cells).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GridTemplateAreasValue {
    pub columns: u32,
    pub areas: Vec<Option<InternedStr>>,
}

impl GridTemplateAreasValue {
    /// Looks up the bounding `(row_start, row_end, col_start, col_end)` in 1-based CSS grid lines
    /// for named area `name`.
    #[must_use]
    pub fn find_area_lines(&self, name: &str) -> Option<(i16, i16, i16, i16)> {
        let cols = self.columns as usize;
        if cols == 0 || self.areas.is_empty() {
            return None;
        }
        let mut min_r = usize::MAX;
        let mut max_r = 0usize;
        let mut min_c = usize::MAX;
        let mut max_c = 0usize;
        let mut found = false;
        for (idx, cell) in self.areas.iter().enumerate() {
            if let Some(a) = cell {
                if a.as_str() == name {
                    let r = idx / cols;
                    let c = idx % cols;
                    min_r = min_r.min(r);
                    max_r = max_r.max(r);
                    min_c = min_c.min(c);
                    max_c = max_c.max(c);
                    found = true;
                }
            }
        }
        if !found {
            return None;
        }
        Some((
            (min_r + 1) as i16,
            (max_r + 2) as i16,
            (min_c + 1) as i16,
            (max_c + 2) as i16,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum TextOverflowMode {
    #[default]
    Clip,
    Ellipsis,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum VerticalAlignMode {
    #[default]
    Baseline,
    Top,
    Middle,
    Bottom,
    TextTop,
    TextBottom,
    Sub,
    Super,
    LengthPx(f32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum BorderCollapseMode {
    #[default]
    Separate,
    Collapse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum TableLayoutMode {
    #[default]
    Auto,
    Fixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum EmptyCellsMode {
    #[default]
    Show,
    Hide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct EdgeValues<T> {
    pub top: T,
    pub right: T,
    pub bottom: T,
    pub left: T,
}

impl<T: Clone> EdgeValues<T> {
    #[must_use]
    pub fn all(value: T) -> Self {
        Self {
            top: value.clone(),
            right: value.clone(),
            bottom: value.clone(),
            left: value,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct BorderEdge {
    pub width: f32,
    pub style: LineStyle,
    pub color: CssColor,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CornerRadius {
    pub x: LengthPercentage,
    pub y: LengthPercentage,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CornerRadii {
    pub top_left: CornerRadius,
    pub top_right: CornerRadius,
    pub bottom_right: CornerRadius,
    pub bottom_left: CornerRadius,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoxShadowValue {
    pub offset_x: LengthPercentage,
    pub offset_y: LengthPercentage,
    pub blur: LengthPercentage,
    pub spread: LengthPercentage,
    pub color: CssColor,
    pub inset: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CssGradientStop {
    pub color: CssColor,
    pub position: Option<LengthPercentage>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum BackgroundImageValue {
    #[default]
    None,
    Url(InternedStr),
    LinearGradient {
        angle_deg: f32,
        stops: Vec<CssGradientStop>,
    },
    RadialGradient {
        shape_circle: bool,
        cx: LengthPercentage,
        cy: LengthPercentage,
        stops: Vec<CssGradientStop>,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct BackgroundLayerValue {
    pub image_url: Option<InternedStr>,
    pub image: BackgroundImageValue,
    pub repeat_x: bool,
    pub repeat_y: bool,
    pub pos_x: LengthPercentage,
    pub pos_y: LengthPercentage,
    pub size_cover: bool,
    pub size_contain: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransformOrigin {
    pub x: LengthPercentage,
    pub y: LengthPercentage,
    pub z: f32,
}

impl Default for TransformOrigin {
    fn default() -> Self {
        Self {
            x: LengthPercentage::Percent(0.5),
            y: LengthPercentage::Percent(0.5),
            z: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TransformOp {
    Translate(LengthPercentage, LengthPercentage),
    Translate3d(LengthPercentage, LengthPercentage, f32),
    TranslateZ(f32),
    Scale(f32, f32),
    Scale3d(f32, f32, f32),
    ScaleZ(f32),
    RotateDeg(f32),
    RotateXDeg(f32),
    RotateYDeg(f32),
    Rotate3d(f32, f32, f32, f32),
    SkewDeg(f32, f32),
    Perspective(f32),
    Matrix([f32; 6]),
    Matrix3d([f32; 16]),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FilterOp {
    Blur(LengthPercentage),
    Brightness(f32),
    Contrast(f32),
    Grayscale(f32),
    Opacity(f32),
    Saturate(f32),
    Invert(f32),
    Sepia(f32),
    HueRotateDeg(f32),
    DropShadow {
        offset_x: LengthPercentage,
        offset_y: LengthPercentage,
        blur: LengthPercentage,
        color: CssColor,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum MixBlendMode {
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Hue,
    Saturation,
    Color,
    Luminosity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClipRadius {
    Length(LengthPercentage),
    ClosestSide,
    FarthestSide,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum ClipPathValue {
    #[default]
    None,
    Inset {
        top: LengthPercentage,
        right: LengthPercentage,
        bottom: LengthPercentage,
        left: LengthPercentage,
        radii: [LengthPercentage; 4],
    },
    Circle {
        radius: ClipRadius,
        cx: LengthPercentage,
        cy: LengthPercentage,
    },
    Ellipse {
        rx: ClipRadius,
        ry: ClipRadius,
        cx: LengthPercentage,
        cy: LengthPercentage,
    },
    Polygon {
        fill_odd: bool,
        points: Vec<(LengthPercentage, LengthPercentage)>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum TimingFunction {
    Linear,
    #[default]
    Ease,
    EaseIn,
    EaseOut,
    EaseInOut,
    CubicBezier(f64, f64, f64, f64),
    Steps(u32, bool),
}

impl TimingFunction {
    /// Evaluates the CSS easing curve at normalized progress `t` in `[0.0, 1.0]` in `f64` precision.
    #[must_use]
    pub fn evaluate_f64(self, t: f64) -> f64 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            Self::Ease => eval_cubic_bezier(0.25, 0.1, 0.25, 1.0, t),
            Self::EaseIn => eval_cubic_bezier(0.42, 0.0, 1.0, 1.0, t),
            Self::EaseOut => eval_cubic_bezier(0.0, 0.0, 0.58, 1.0, t),
            Self::EaseInOut => eval_cubic_bezier(0.42, 0.0, 0.58, 1.0, t),
            Self::CubicBezier(x1, y1, x2, y2) => eval_cubic_bezier(x1, y1, x2, y2, t),
            Self::Steps(count, jump_start) => {
                let n = f64::from(count.max(1));
                if jump_start {
                    ((t * n).floor() + 1.0).min(n) / n
                } else {
                    (t * n).floor() / n
                }
            }
        }
    }

    /// Evaluates the CSS easing curve at normalized progress `t` in `[0.0, 1.0]`.
    #[must_use]
    pub fn evaluate(self, t: f32) -> f32 {
        self.evaluate_f64(f64::from(t)) as f32
    }
}

fn eval_cubic_bezier(x1: f64, y1: f64, x2: f64, y2: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let sample_x = |u: f64| {
        let inv = 1.0 - u;
        3.0 * inv * inv * u * x1 + 3.0 * inv * u * u * x2 + u * u * u
    };
    let sample_dx = |u: f64| {
        let inv = 1.0 - u;
        3.0 * inv * inv * x1 + 6.0 * inv * u * (x2 - x1) + 3.0 * u * u * (1.0 - x2)
    };
    let sample_y = |u: f64| {
        let inv = 1.0 - u;
        3.0 * inv * inv * u * y1 + 3.0 * inv * u * u * y2 + u * u * u
    };
    let mut u = x;
    let mut converged = false;
    for _ in 0..8 {
        let err = sample_x(u) - x;
        if err.abs() < 1e-11 {
            converged = true;
            break;
        }
        let d = sample_dx(u);
        if d.abs() < 1e-12 {
            break;
        }
        u = (u - err / d).clamp(0.0, 1.0);
    }
    if !converged {
        let (mut lo, mut hi) = (0.0_f64, 1.0_f64);
        u = x;
        for _ in 0..42 {
            let sx = sample_x(u);
            if (sx - x).abs() < 1e-11 {
                break;
            }
            if sx < x {
                lo = u;
            } else {
                hi = u;
            }
            u = 0.5 * (lo + hi);
        }
    }
    sample_y(u)
}

#[derive(Debug, Clone, PartialEq)]
pub struct TransitionSpec {
    pub property: InternedStr,
    pub duration_ms: f32,
    pub delay_ms: f32,
    pub timing: TimingFunction,
    pub behavior: TransitionBehaviorMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum AnimationDirectionMode {
    #[default]
    Normal,
    Reverse,
    Alternate,
    AlternateReverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum AnimationFillModeKind {
    #[default]
    None,
    Forwards,
    Backwards,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum AnimationPlayStateMode {
    #[default]
    Running,
    Paused,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnimationSpec {
    pub name: InternedStr,
    pub duration_ms: f32,
    pub delay_ms: f32,
    pub timing: TimingFunction,
    /// `f32::INFINITY` represents CSS `infinite`.
    pub iteration_count: f32,
    pub direction: AnimationDirectionMode,
    pub fill_mode: AnimationFillModeKind,
    pub play_state: AnimationPlayStateMode,
    pub composition: AnimationCompositionMode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GridTrackSize {
    Auto,
    MinContent,
    MaxContent,
    Length(LengthPercentage),
    Fr(f32),
    FrMath(Box<crate::values_math::MathNode>),
    MinMax(Box<GridTrackSize>, Box<GridTrackSize>),
    FitContent(LengthPercentage),
}

// ---------------------------------------------------------------------------
// Direct converters from lightningcss AST nodes
// ---------------------------------------------------------------------------

#[must_use]
pub fn convert_length_value(lv: &LengthValue) -> LengthPercentage {
    if let Some(px) = lv.to_px() {
        return LengthPercentage::px(px);
    }
    let (value, unit) = lv.to_unit_value();
    match LengthUnit::from_unit_str(unit) {
        Some(u) => LengthPercentage::Length(value, u),
        None => LengthPercentage::px(value),
    }
}

#[must_use]
pub fn convert_calc_length(calc: &Calc<Length>) -> CalcExpr {
    match calc {
        Calc::Value(len) => match convert_length(len) {
            LengthPercentage::Zero => CalcExpr::Length(0.0, LengthUnit::Px),
            LengthPercentage::Length(v, u) => CalcExpr::Length(v, u),
            LengthPercentage::Percent(p) => CalcExpr::Percent(p),
            LengthPercentage::Calc(inner) => *inner,
        },
        Calc::Number(n) => CalcExpr::Number(*n),
        Calc::Sum(a, b) => CalcExpr::Sum(
            Box::new(convert_calc_length(a)),
            Box::new(convert_calc_length(b)),
        ),
        Calc::Product(k, a) => CalcExpr::Product(*k, Box::new(convert_calc_length(a))),
        Calc::Function(func) => convert_math_fn_length(func),
    }
}

fn convert_math_fn_length(func: &MathFunction<Length>) -> CalcExpr {
    match func {
        MathFunction::Calc(c) => convert_calc_length(c),
        MathFunction::Min(args) => CalcExpr::Min(args.iter().map(convert_calc_length).collect()),
        MathFunction::Max(args) => CalcExpr::Max(args.iter().map(convert_calc_length).collect()),
        MathFunction::Clamp(lo, val, hi) => CalcExpr::Clamp(
            Box::new(convert_calc_length(lo)),
            Box::new(convert_calc_length(val)),
            Box::new(convert_calc_length(hi)),
        ),
        MathFunction::Abs(v) => CalcExpr::Abs(Box::new(convert_calc_length(v))),
        MathFunction::Sign(v) => CalcExpr::Sign(Box::new(convert_calc_length(v))),
        MathFunction::Rem(a, b) => CalcExpr::Rem(
            Box::new(convert_calc_length(a)),
            Box::new(convert_calc_length(b)),
        ),
        MathFunction::Mod(a, b) => CalcExpr::Mod(
            Box::new(convert_calc_length(a)),
            Box::new(convert_calc_length(b)),
        ),
        MathFunction::Round(_, a, _) => convert_calc_length(a),
        MathFunction::Hypot(args) => args
            .first()
            .map(convert_calc_length)
            .unwrap_or(CalcExpr::Number(0.0)),
    }
}

#[must_use]
pub fn convert_length(len: &Length) -> LengthPercentage {
    match len {
        Length::Value(lv) => convert_length_value(lv),
        Length::Calc(calc) => LengthPercentage::Calc(Box::new(convert_calc_length(calc))),
    }
}

#[must_use]
pub fn convert_calc_lp(calc: &Calc<LightningLp>) -> CalcExpr {
    match calc {
        Calc::Value(lp) => match convert_length_percentage(lp) {
            LengthPercentage::Zero => CalcExpr::Length(0.0, LengthUnit::Px),
            LengthPercentage::Length(v, u) => CalcExpr::Length(v, u),
            LengthPercentage::Percent(p) => CalcExpr::Percent(p),
            LengthPercentage::Calc(inner) => *inner,
        },
        Calc::Number(n) => CalcExpr::Number(*n),
        Calc::Sum(a, b) => {
            CalcExpr::Sum(Box::new(convert_calc_lp(a)), Box::new(convert_calc_lp(b)))
        }
        Calc::Product(k, a) => CalcExpr::Product(*k, Box::new(convert_calc_lp(a))),
        Calc::Function(func) => match func.as_ref() {
            MathFunction::Calc(c) => convert_calc_lp(c),
            MathFunction::Min(args) => CalcExpr::Min(args.iter().map(convert_calc_lp).collect()),
            MathFunction::Max(args) => CalcExpr::Max(args.iter().map(convert_calc_lp).collect()),
            MathFunction::Clamp(lo, val, hi) => CalcExpr::Clamp(
                Box::new(convert_calc_lp(lo)),
                Box::new(convert_calc_lp(val)),
                Box::new(convert_calc_lp(hi)),
            ),
            MathFunction::Abs(v) => CalcExpr::Abs(Box::new(convert_calc_lp(v))),
            MathFunction::Sign(v) => CalcExpr::Sign(Box::new(convert_calc_lp(v))),
            MathFunction::Rem(a, b) => {
                CalcExpr::Rem(Box::new(convert_calc_lp(a)), Box::new(convert_calc_lp(b)))
            }
            MathFunction::Mod(a, b) => {
                CalcExpr::Mod(Box::new(convert_calc_lp(a)), Box::new(convert_calc_lp(b)))
            }
            MathFunction::Round(_, a, _) => convert_calc_lp(a),
            MathFunction::Hypot(args) => args
                .first()
                .map(convert_calc_lp)
                .unwrap_or(CalcExpr::Number(0.0)),
        },
    }
}

#[must_use]
pub fn convert_length_percentage(lp: &LightningLp) -> LengthPercentage {
    match lp {
        DimensionPercentage::Dimension(lv) => convert_length_value(lv),
        DimensionPercentage::Percentage(p) => LengthPercentage::Percent(p.0),
        DimensionPercentage::Calc(calc) => LengthPercentage::Calc(Box::new(convert_calc_lp(calc))),
    }
}

#[must_use]
pub fn convert_length_percentage_or_auto(val: &LengthPercentageOrAuto) -> LengthPercentageAuto {
    match val {
        LengthPercentageOrAuto::Auto => LengthPercentageAuto::Auto,
        LengthPercentageOrAuto::LengthPercentage(lp) => {
            LengthPercentageAuto::Value(convert_length_percentage(lp))
        }
    }
}

#[must_use]
pub fn convert_size(size: &Size) -> SizeValue {
    match size {
        Size::Auto => SizeValue::Auto,
        Size::LengthPercentage(lp) => SizeValue::Length(convert_length_percentage(lp)),
        Size::MinContent(_) => SizeValue::MinContent,
        Size::MaxContent(_) => SizeValue::MaxContent,
        Size::FitContent(_) => SizeValue::FitContent(None),
        Size::FitContentFunction(lp) => SizeValue::FitContent(Some(convert_length_percentage(lp))),
        Size::Stretch(_) | Size::Contain => SizeValue::Stretch,
    }
}

#[must_use]
pub fn convert_max_size(size: &MaxSize) -> SizeValue {
    match size {
        MaxSize::None => SizeValue::None,
        MaxSize::LengthPercentage(lp) => SizeValue::Length(convert_length_percentage(lp)),
        MaxSize::MinContent(_) => SizeValue::MinContent,
        MaxSize::MaxContent(_) => SizeValue::MaxContent,
        MaxSize::FitContent(_) => SizeValue::FitContent(None),
        MaxSize::FitContentFunction(lp) => {
            SizeValue::FitContent(Some(convert_length_percentage(lp)))
        }
        MaxSize::Stretch(_) | MaxSize::Contain => SizeValue::Stretch,
    }
}

#[must_use]
pub fn convert_display(display: &Display) -> DisplayMode {
    match display {
        Display::Keyword(kw) => match kw {
            DisplayKeyword::None => DisplayMode::None,
            DisplayKeyword::Contents => DisplayMode::Contents,
            DisplayKeyword::TableRowGroup => DisplayMode::TableRowGroup,
            DisplayKeyword::TableHeaderGroup => DisplayMode::TableHeaderGroup,
            DisplayKeyword::TableFooterGroup => DisplayMode::TableFooterGroup,
            DisplayKeyword::TableRow => DisplayMode::TableRow,
            DisplayKeyword::TableColumnGroup => DisplayMode::TableColumnGroup,
            DisplayKeyword::TableColumn => DisplayMode::TableColumn,
            DisplayKeyword::TableCell => DisplayMode::TableCell,
            DisplayKeyword::TableCaption => DisplayMode::TableCaption,
            DisplayKeyword::RubyBase => DisplayMode::RubyBase,
            DisplayKeyword::RubyText => DisplayMode::RubyText,
            DisplayKeyword::RubyBaseContainer => DisplayMode::RubyBaseContainer,
            DisplayKeyword::RubyTextContainer => DisplayMode::RubyTextContainer,
        },
        Display::Pair(pair) => {
            if pair.is_list_item {
                return match (pair.outside, &pair.inside) {
                    (DisplayOutside::Inline, DisplayInside::FlowRoot) => {
                        DisplayMode::InlineFlowRootListItem
                    }
                    (DisplayOutside::Inline, _) => DisplayMode::InlineListItem,
                    (DisplayOutside::Block, DisplayInside::FlowRoot) => {
                        DisplayMode::FlowRootListItem
                    }
                    _ => DisplayMode::ListItem,
                };
            }
            match (pair.outside, &pair.inside) {
                (DisplayOutside::Inline, DisplayInside::Flow) => DisplayMode::Inline,
                (DisplayOutside::Inline, DisplayInside::FlowRoot) => DisplayMode::InlineBlock,
                (DisplayOutside::Inline, DisplayInside::Flex(_) | DisplayInside::Box(_)) => {
                    DisplayMode::InlineFlex
                }
                (DisplayOutside::Inline, DisplayInside::Grid) => DisplayMode::InlineGrid,
                (DisplayOutside::Inline, DisplayInside::Table) => DisplayMode::InlineTable,
                (DisplayOutside::Inline, DisplayInside::Ruby) => DisplayMode::Ruby,
                (DisplayOutside::Block, DisplayInside::Ruby) => DisplayMode::BlockRuby,
                (_, DisplayInside::Flex(_) | DisplayInside::Box(_)) => DisplayMode::Flex,
                (_, DisplayInside::Grid) => DisplayMode::Grid,
                (_, DisplayInside::Table) => DisplayMode::Table,
                (_, DisplayInside::FlowRoot) => DisplayMode::FlowRoot,
                _ => DisplayMode::Block,
            }
        }
    }
}

#[must_use]
pub fn convert_position(pos: &Position) -> PositionMode {
    match pos {
        Position::Static => PositionMode::Static,
        Position::Relative => PositionMode::Relative,
        Position::Absolute => PositionMode::Absolute,
        Position::Fixed => PositionMode::Fixed,
        Position::Sticky(_) => PositionMode::Sticky,
    }
}

#[must_use]
pub fn convert_z_index(z: &ZIndex) -> ZIndexValue {
    match z {
        ZIndex::Auto => ZIndexValue::Auto,
        ZIndex::Integer(i) => ZIndexValue::Integer(*i),
    }
}

#[must_use]
pub fn convert_visibility(vis: &Visibility) -> VisibilityMode {
    match vis {
        Visibility::Visible => VisibilityMode::Visible,
        Visibility::Hidden => VisibilityMode::Hidden,
        Visibility::Collapse => VisibilityMode::Collapse,
    }
}

#[must_use]
pub fn convert_overflow(ov: &OverflowKeyword) -> OverflowMode {
    match ov {
        OverflowKeyword::Visible => OverflowMode::Visible,
        OverflowKeyword::Hidden => OverflowMode::Hidden,
        OverflowKeyword::Clip => OverflowMode::Clip,
        OverflowKeyword::Scroll => OverflowMode::Scroll,
        OverflowKeyword::Auto => OverflowMode::Auto,
    }
}

#[must_use]
pub fn convert_alpha(alpha: &AlphaValue) -> f32 {
    alpha.0.clamp(0.0, 1.0)
}

#[must_use]
pub fn convert_border_width_lp(width: &BorderSideWidth) -> LengthPercentage {
    match width {
        BorderSideWidth::Thin => LengthPercentage::px(1.0),
        BorderSideWidth::Medium => LengthPercentage::px(3.0),
        BorderSideWidth::Thick => LengthPercentage::px(5.0),
        BorderSideWidth::Length(len) => convert_length(len),
    }
}

#[must_use]
pub fn convert_border_width(width: &BorderSideWidth) -> f32 {
    convert_border_width_lp(width)
        .resolve_px(0.0, &EvalContext::default())
        .max(0.0)
}

#[must_use]
pub fn convert_line_style(style: &LightningLineStyle) -> LineStyle {
    match style {
        LightningLineStyle::None => LineStyle::None,
        LightningLineStyle::Hidden => LineStyle::Hidden,
        LightningLineStyle::Dotted => LineStyle::Dotted,
        LightningLineStyle::Dashed => LineStyle::Dashed,
        LightningLineStyle::Solid => LineStyle::Solid,
        LightningLineStyle::Double => LineStyle::Double,
        LightningLineStyle::Groove => LineStyle::Groove,
        LightningLineStyle::Ridge => LineStyle::Ridge,
        LightningLineStyle::Inset => LineStyle::Inset,
        LightningLineStyle::Outset => LineStyle::Outset,
    }
}

#[must_use]
pub fn convert_corner_radius(size: &Size2D<LightningLp>) -> CornerRadius {
    CornerRadius {
        x: convert_length_percentage(&size.0),
        y: convert_length_percentage(&size.1),
    }
}

#[must_use]
pub fn convert_font_weight(weight: &FontWeight) -> f32 {
    match weight {
        FontWeight::Absolute(AbsoluteFontWeight::Normal) => 400.0,
        FontWeight::Absolute(AbsoluteFontWeight::Bold) => 700.0,
        FontWeight::Absolute(AbsoluteFontWeight::Weight(w)) => w.clamp(1.0, 1000.0),
        FontWeight::Bolder => 700.0,
        FontWeight::Lighter => 300.0,
    }
}

#[must_use]
pub fn convert_font_size(size: &FontSize) -> LengthPercentage {
    match size {
        FontSize::Length(lp) => convert_length_percentage(lp),
        FontSize::Absolute(abs) => LengthPercentage::px(match abs {
            AbsoluteFontSize::XXSmall => 9.0,
            AbsoluteFontSize::XSmall => 10.0,
            AbsoluteFontSize::Small => 13.0,
            AbsoluteFontSize::Medium => 16.0,
            AbsoluteFontSize::Large => 18.0,
            AbsoluteFontSize::XLarge => 24.0,
            AbsoluteFontSize::XXLarge => 32.0,
            AbsoluteFontSize::XXXLarge => 48.0,
        }),
        FontSize::Relative(RelativeFontSize::Smaller) => LengthPercentage::Percent(0.833),
        FontSize::Relative(RelativeFontSize::Larger) => LengthPercentage::Percent(1.2),
    }
}

#[must_use]
pub fn convert_font_style(style: &FontStyle) -> FontStyleMode {
    match style {
        FontStyle::Normal => FontStyleMode::Normal,
        FontStyle::Italic => FontStyleMode::Italic,
        FontStyle::Oblique(angle) => FontStyleMode::Oblique(angle_to_deg(angle)),
    }
}

#[must_use]
pub fn convert_font_family_name(family: &FontFamily<'_>) -> InternedStr {
    match family {
        FontFamily::Generic(g) => intern_str(match g {
            GenericFontFamily::Serif => "serif",
            GenericFontFamily::SansSerif => "sans-serif",
            GenericFontFamily::Cursive => "cursive",
            GenericFontFamily::Fantasy => "fantasy",
            GenericFontFamily::Monospace => "monospace",
            GenericFontFamily::SystemUI => "system-ui",
            GenericFontFamily::Emoji => "emoji",
            GenericFontFamily::Math => "math",
            GenericFontFamily::FangSong => "fangsong",
            GenericFontFamily::UISerif => "ui-serif",
            GenericFontFamily::UISansSerif => "ui-sans-serif",
            GenericFontFamily::UIMonospace => "ui-monospace",
            GenericFontFamily::UIRounded => "ui-rounded",
            _ => "sans-serif",
        }),
        FontFamily::FamilyName(name) => {
            let mut s = String::new();
            let mut printer = lightningcss::printer::Printer::new(
                &mut s,
                lightningcss::stylesheet::PrinterOptions::default(),
            );
            let _ = lightningcss::traits::ToCss::to_css(name, &mut printer);
            intern_str(s.trim_matches('"').trim_matches('\''))
        }
    }
}

#[must_use]
pub fn convert_line_height(lh: &LineHeight) -> LineHeightValue {
    match lh {
        LineHeight::Normal => LineHeightValue::Normal,
        LineHeight::Number(n) => LineHeightValue::Number(*n),
        LineHeight::Length(lp) => LineHeightValue::Length(convert_length_percentage(lp)),
    }
}

#[must_use]
pub fn convert_spacing(sp: &Spacing) -> LengthPercentage {
    match sp {
        Spacing::Normal => LengthPercentage::Zero,
        Spacing::Length(len) => convert_length(len),
    }
}

#[must_use]
pub fn convert_text_align(ta: &TextAlign) -> TextAlignMode {
    match ta {
        TextAlign::Start => TextAlignMode::Start,
        TextAlign::End => TextAlignMode::End,
        TextAlign::Left => TextAlignMode::Left,
        TextAlign::Right => TextAlignMode::Right,
        TextAlign::Center => TextAlignMode::Center,
        TextAlign::Justify | TextAlign::JustifyAll => TextAlignMode::Justify,
        TextAlign::MatchParent => TextAlignMode::MatchParent,
    }
}

#[must_use]
pub fn convert_text_decoration_line(line: &LightningTextDeco) -> TextDecorationLine {
    let mut out = TextDecorationLine::NONE;
    if line.contains(LightningTextDeco::Underline) {
        out |= TextDecorationLine::UNDERLINE;
    }
    if line.contains(LightningTextDeco::Overline) {
        out |= TextDecorationLine::OVERLINE;
    }
    if line.contains(LightningTextDeco::LineThrough) {
        out |= TextDecorationLine::LINE_THROUGH;
    }
    out
}

#[must_use]
pub fn convert_text_transform(tt: &TextTransform) -> TextTransformMode {
    match tt.case {
        TextTransformCase::None => TextTransformMode::None,
        TextTransformCase::Capitalize => TextTransformMode::Capitalize,
        TextTransformCase::Uppercase => TextTransformMode::Uppercase,
        TextTransformCase::Lowercase => TextTransformMode::Lowercase,
    }
}

#[must_use]
pub fn convert_white_space(ws: &WhiteSpace) -> WhiteSpaceMode {
    match ws {
        WhiteSpace::Normal => WhiteSpaceMode::Normal,
        WhiteSpace::NoWrap => WhiteSpaceMode::Nowrap,
        WhiteSpace::Pre => WhiteSpaceMode::Pre,
        WhiteSpace::PreWrap => WhiteSpaceMode::PreWrap,
        WhiteSpace::PreLine => WhiteSpaceMode::PreLine,
        WhiteSpace::BreakSpaces => WhiteSpaceMode::BreakSpaces,
    }
}

#[must_use]
pub fn convert_word_break(wb: &WordBreak) -> WordBreakMode {
    match wb {
        WordBreak::Normal => WordBreakMode::Normal,
        WordBreak::BreakAll => WordBreakMode::BreakAll,
        WordBreak::KeepAll => WordBreakMode::KeepAll,
        WordBreak::BreakWord => WordBreakMode::BreakWord,
    }
}

#[must_use]
pub fn convert_overflow_wrap(ow: &OverflowWrap) -> OverflowWrapMode {
    match ow {
        OverflowWrap::Normal => OverflowWrapMode::Normal,
        OverflowWrap::BreakWord => OverflowWrapMode::BreakWord,
        OverflowWrap::Anywhere => OverflowWrapMode::Anywhere,
    }
}

#[must_use]
pub fn convert_direction(dir: &Direction) -> DirectionMode {
    match dir {
        Direction::Ltr => DirectionMode::Ltr,
        Direction::Rtl => DirectionMode::Rtl,
    }
}

#[must_use]
pub fn convert_user_select(us: &UserSelect) -> UserSelectMode {
    match us {
        UserSelect::Auto => UserSelectMode::Auto,
        UserSelect::Text => UserSelectMode::Text,
        UserSelect::None => UserSelectMode::None,
        UserSelect::Contain => UserSelectMode::Contain,
        UserSelect::All => UserSelectMode::All,
    }
}

#[must_use]
pub fn convert_cursor(cursor: &Cursor<'_>) -> CursorKind {
    match cursor.keyword {
        CursorKeyword::Auto => CursorKind::Auto,
        CursorKeyword::Default => CursorKind::Default,
        CursorKeyword::Pointer => CursorKind::Pointer,
        CursorKeyword::Text | CursorKeyword::VerticalText => CursorKind::Text,
        CursorKeyword::Move | CursorKeyword::AllScroll => CursorKind::Move,
        CursorKeyword::NotAllowed | CursorKeyword::NoDrop => CursorKind::NotAllowed,
        CursorKeyword::Grab => CursorKind::Grab,
        CursorKeyword::Grabbing => CursorKind::Grabbing,
        CursorKeyword::EwResize | CursorKeyword::EResize | CursorKeyword::WResize => {
            CursorKind::EwResize
        }
        CursorKeyword::NsResize | CursorKeyword::NResize | CursorKeyword::SResize => {
            CursorKind::NsResize
        }
        CursorKeyword::NeswResize | CursorKeyword::NeResize | CursorKeyword::SwResize => {
            CursorKind::NeswResize
        }
        CursorKeyword::NwseResize | CursorKeyword::NwResize | CursorKeyword::SeResize => {
            CursorKind::NwseResize
        }
        CursorKeyword::ColResize => CursorKind::ColResize,
        CursorKeyword::RowResize => CursorKind::RowResize,
        CursorKeyword::Wait => CursorKind::Wait,
        CursorKeyword::Progress => CursorKind::Progress,
        CursorKeyword::Crosshair => CursorKind::Crosshair,
        CursorKeyword::Help => CursorKind::Help,
        CursorKeyword::None => CursorKind::None,
        _ => CursorKind::Auto,
    }
}

#[must_use]
pub fn convert_flex_direction(fd: &FlexDirection) -> FlexDirectionMode {
    match fd {
        FlexDirection::Row => FlexDirectionMode::Row,
        FlexDirection::RowReverse => FlexDirectionMode::RowReverse,
        FlexDirection::Column => FlexDirectionMode::Column,
        FlexDirection::ColumnReverse => FlexDirectionMode::ColumnReverse,
    }
}

#[must_use]
pub fn convert_flex_wrap(fw: &FlexWrap) -> FlexWrapMode {
    match fw {
        FlexWrap::NoWrap => FlexWrapMode::NoWrap,
        FlexWrap::Wrap => FlexWrapMode::Wrap,
        FlexWrap::WrapReverse => FlexWrapMode::WrapReverse,
    }
}

fn apply_overflow_pos(
    val: AlignValue,
    overflow: Option<&lightningcss::properties::align::OverflowPosition>,
) -> AlignValue {
    match overflow {
        Some(lightningcss::properties::align::OverflowPosition::Safe) => {
            val.with_overflow(true, false)
        }
        Some(lightningcss::properties::align::OverflowPosition::Unsafe) => {
            val.with_overflow(false, true)
        }
        None => val,
    }
}

fn convert_baseline_pos(bp: &lightningcss::properties::align::BaselinePosition) -> AlignValue {
    match bp {
        lightningcss::properties::align::BaselinePosition::Last => AlignValue::LastBaseline,
        lightningcss::properties::align::BaselinePosition::First => AlignValue::Baseline,
    }
}

fn convert_content_position(cp: &ContentPosition) -> AlignValue {
    match cp {
        ContentPosition::Center => AlignValue::Center,
        ContentPosition::Start => AlignValue::Start,
        ContentPosition::End => AlignValue::End,
        ContentPosition::FlexStart => AlignValue::FlexStart,
        ContentPosition::FlexEnd => AlignValue::FlexEnd,
    }
}

fn convert_content_dist(cd: &ContentDistribution) -> AlignValue {
    match cd {
        ContentDistribution::SpaceBetween => AlignValue::SpaceBetween,
        ContentDistribution::SpaceAround => AlignValue::SpaceAround,
        ContentDistribution::SpaceEvenly => AlignValue::SpaceEvenly,
        ContentDistribution::Stretch => AlignValue::Stretch,
    }
}

fn convert_self_position(sp: &SelfPosition) -> AlignValue {
    match sp {
        SelfPosition::Center => AlignValue::Center,
        SelfPosition::Start => AlignValue::Start,
        SelfPosition::SelfStart => AlignValue::SelfStart,
        SelfPosition::End => AlignValue::End,
        SelfPosition::SelfEnd => AlignValue::SelfEnd,
        SelfPosition::FlexStart => AlignValue::FlexStart,
        SelfPosition::FlexEnd => AlignValue::FlexEnd,
    }
}

#[must_use]
pub fn convert_align_content(ac: &AlignContent) -> AlignValue {
    match ac {
        AlignContent::Normal => AlignValue::Normal,
        AlignContent::BaselinePosition(bp) => convert_baseline_pos(bp),
        AlignContent::ContentDistribution(d) => convert_content_dist(d),
        AlignContent::ContentPosition { value, overflow } => {
            apply_overflow_pos(convert_content_position(value), overflow.as_ref())
        }
    }
}

#[must_use]
pub fn convert_justify_content(jc: &JustifyContent) -> AlignValue {
    match jc {
        JustifyContent::Normal => AlignValue::Normal,
        JustifyContent::ContentDistribution(d) => convert_content_dist(d),
        JustifyContent::ContentPosition { value, overflow } => {
            apply_overflow_pos(convert_content_position(value), overflow.as_ref())
        }
        JustifyContent::Left { overflow } => {
            apply_overflow_pos(AlignValue::Left, overflow.as_ref())
        }
        JustifyContent::Right { overflow } => {
            apply_overflow_pos(AlignValue::Right, overflow.as_ref())
        }
    }
}

#[must_use]
pub fn convert_align_items(ai: &AlignItems) -> AlignValue {
    match ai {
        AlignItems::Normal => AlignValue::Normal,
        AlignItems::Stretch => AlignValue::Stretch,
        AlignItems::BaselinePosition(bp) => convert_baseline_pos(bp),
        AlignItems::SelfPosition { value, overflow } => {
            apply_overflow_pos(convert_self_position(value), overflow.as_ref())
        }
    }
}

#[must_use]
pub fn convert_align_self(as_: &AlignSelf) -> AlignValue {
    match as_ {
        AlignSelf::Auto => AlignValue::Auto,
        AlignSelf::Normal => AlignValue::Normal,
        AlignSelf::Stretch => AlignValue::Stretch,
        AlignSelf::BaselinePosition(bp) => convert_baseline_pos(bp),
        AlignSelf::SelfPosition { value, overflow } => {
            apply_overflow_pos(convert_self_position(value), overflow.as_ref())
        }
    }
}

#[must_use]
pub fn convert_justify_items(ji: &JustifyItems) -> AlignValue {
    match ji {
        JustifyItems::Normal => AlignValue::Normal,
        JustifyItems::Stretch => AlignValue::Stretch,
        JustifyItems::BaselinePosition(bp) => convert_baseline_pos(bp),
        JustifyItems::SelfPosition { value, overflow } => {
            apply_overflow_pos(convert_self_position(value), overflow.as_ref())
        }
        JustifyItems::Left { overflow } => apply_overflow_pos(AlignValue::Left, overflow.as_ref()),
        JustifyItems::Right { overflow } => {
            apply_overflow_pos(AlignValue::Right, overflow.as_ref())
        }
        JustifyItems::Legacy(_) => AlignValue::Normal,
    }
}

#[must_use]
pub fn convert_justify_self(js: &JustifySelf) -> AlignValue {
    match js {
        JustifySelf::Auto => AlignValue::Auto,
        JustifySelf::Normal => AlignValue::Normal,
        JustifySelf::Stretch => AlignValue::Stretch,
        JustifySelf::BaselinePosition(bp) => convert_baseline_pos(bp),
        JustifySelf::SelfPosition { value, overflow } => {
            apply_overflow_pos(convert_self_position(value), overflow.as_ref())
        }
        JustifySelf::Left { overflow } => apply_overflow_pos(AlignValue::Left, overflow.as_ref()),
        JustifySelf::Right { overflow } => apply_overflow_pos(AlignValue::Right, overflow.as_ref()),
    }
}

#[must_use]
pub fn convert_box_shadow(shadow: &BoxShadow) -> BoxShadowValue {
    BoxShadowValue {
        offset_x: convert_length(&shadow.x_offset),
        offset_y: convert_length(&shadow.y_offset),
        blur: convert_length(&shadow.blur),
        spread: convert_length(&shadow.spread),
        color: convert_color(&shadow.color).unwrap_or(CssColor::CurrentColor),
        inset: shadow.inset,
    }
}

fn convert_hv_axes(
    hx: &HorizontalPosition,
    vy: &VerticalPosition,
) -> (LengthPercentage, LengthPercentage) {
    let x = match hx {
        HorizontalPosition::Length(lp) => convert_length_percentage(lp),
        HorizontalPosition::Side { side, .. } => match side {
            HorizontalPositionKeyword::Left => LengthPercentage::Percent(0.0),
            HorizontalPositionKeyword::Right => LengthPercentage::Percent(1.0),
        },
        HorizontalPosition::Center => LengthPercentage::Percent(0.5),
    };
    let y = match vy {
        VerticalPosition::Length(lp) => convert_length_percentage(lp),
        VerticalPosition::Side { side, .. } => match side {
            VerticalPositionKeyword::Top => LengthPercentage::Percent(0.0),
            VerticalPositionKeyword::Bottom => LengthPercentage::Percent(1.0),
        },
        VerticalPosition::Center => LengthPercentage::Percent(0.5),
    };
    (x, y)
}

fn convert_position_axes(pos: &LightningPosition) -> (LengthPercentage, LengthPercentage) {
    convert_hv_axes(&pos.x, &pos.y)
}

fn convert_gradient_stops(items: &[GradientItem<LightningLp>]) -> Vec<CssGradientStop> {
    let mut stops = Vec::new();
    for item in items {
        if let GradientItem::ColorStop(cs) = item {
            if let Some(color) = convert_color(&cs.color) {
                stops.push(CssGradientStop {
                    color,
                    position: cs.position.as_ref().map(convert_length_percentage),
                });
            }
        }
    }
    stops
}

#[must_use]
pub fn convert_image(img: &Image<'_>) -> BackgroundImageValue {
    match img {
        Image::None => BackgroundImageValue::None,
        Image::Url(u) => BackgroundImageValue::Url(intern_str(u.url.as_ref())),
        Image::Gradient(grad) => match grad.as_ref() {
            Gradient::Linear(lg) | Gradient::RepeatingLinear(lg) => {
                let angle_deg = match &lg.direction {
                    LineDirection::Angle(a) => angle_to_deg(a),
                    LineDirection::Horizontal(HorizontalPositionKeyword::Right) => 90.0,
                    LineDirection::Horizontal(HorizontalPositionKeyword::Left) => 270.0,
                    LineDirection::Vertical(VerticalPositionKeyword::Top) => 0.0,
                    LineDirection::Vertical(VerticalPositionKeyword::Bottom) => 180.0,
                    LineDirection::Corner {
                        horizontal,
                        vertical,
                    } => match (horizontal, vertical) {
                        (HorizontalPositionKeyword::Right, VerticalPositionKeyword::Top) => 45.0,
                        (HorizontalPositionKeyword::Right, VerticalPositionKeyword::Bottom) => {
                            135.0
                        }
                        (HorizontalPositionKeyword::Left, VerticalPositionKeyword::Bottom) => 225.0,
                        (HorizontalPositionKeyword::Left, VerticalPositionKeyword::Top) => 315.0,
                    },
                };
                BackgroundImageValue::LinearGradient {
                    angle_deg,
                    stops: convert_gradient_stops(&lg.items),
                }
            }
            Gradient::Radial(rg) | Gradient::RepeatingRadial(rg) => {
                let shape_circle = matches!(rg.shape, EndingShape::Circle(_));
                let (cx, cy) = convert_position_axes(&rg.position);
                BackgroundImageValue::RadialGradient {
                    shape_circle,
                    cx,
                    cy,
                    stops: convert_gradient_stops(&rg.items),
                }
            }
            _ => BackgroundImageValue::None,
        },
        _ => BackgroundImageValue::None,
    }
}

#[must_use]
pub fn convert_background_layer(bg: &Background<'_>) -> BackgroundLayerValue {
    let image = convert_image(&bg.image);
    let image_url = match &image {
        BackgroundImageValue::Url(u) => Some(u.clone()),
        _ => None,
    };
    let (pos_x, pos_y) = convert_hv_axes(&bg.position.x, &bg.position.y);
    BackgroundLayerValue {
        image_url,
        image,
        repeat_x: !matches!(bg.repeat.x, BackgroundRepeatKeyword::NoRepeat),
        repeat_y: !matches!(bg.repeat.y, BackgroundRepeatKeyword::NoRepeat),
        pos_x,
        pos_y,
        size_cover: matches!(bg.size, BackgroundSize::Cover),
        size_contain: matches!(bg.size, BackgroundSize::Contain),
    }
}

fn num_or_pct(val: &NumberOrPercentage) -> f32 {
    match val {
        NumberOrPercentage::Number(n) => *n,
        NumberOrPercentage::Percentage(p) => p.0,
    }
}

#[must_use]
pub fn angle_to_deg(angle: &Angle) -> f32 {
    match angle {
        Angle::Deg(d) => *d,
        Angle::Rad(r) => r.to_degrees(),
        Angle::Grad(g) => *g * 0.9,
        Angle::Turn(t) => *t * 360.0,
    }
}

#[must_use]
pub fn convert_transform_origin(pos: &LightningPosition) -> TransformOrigin {
    let (x, y) = convert_position_axes(pos);
    TransformOrigin { x, y, z: 0.0 }
}

#[must_use]
pub fn convert_transform(tf: &Transform) -> Option<TransformOp> {
    let eval = EvalContext::default();
    match tf {
        Transform::Translate(x, y) => Some(TransformOp::Translate(
            convert_length_percentage(x),
            convert_length_percentage(y),
        )),
        Transform::TranslateX(x) => Some(TransformOp::Translate(
            convert_length_percentage(x),
            LengthPercentage::Zero,
        )),
        Transform::TranslateY(y) => Some(TransformOp::Translate(
            LengthPercentage::Zero,
            convert_length_percentage(y),
        )),
        Transform::TranslateZ(z) => Some(TransformOp::TranslateZ(
            convert_length(z).resolve_px(0.0, &eval),
        )),
        Transform::Translate3d(x, y, z) => Some(TransformOp::Translate3d(
            convert_length_percentage(x),
            convert_length_percentage(y),
            convert_length(z).resolve_px(0.0, &eval),
        )),
        Transform::Scale(x, y) => Some(TransformOp::Scale(num_or_pct(x), num_or_pct(y))),
        Transform::ScaleX(x) => Some(TransformOp::Scale(num_or_pct(x), 1.0)),
        Transform::ScaleY(y) => Some(TransformOp::Scale(1.0, num_or_pct(y))),
        Transform::ScaleZ(z) => Some(TransformOp::ScaleZ(num_or_pct(z))),
        Transform::Scale3d(x, y, z) => Some(TransformOp::Scale3d(
            num_or_pct(x),
            num_or_pct(y),
            num_or_pct(z),
        )),
        Transform::Rotate(a) | Transform::RotateZ(a) => {
            Some(TransformOp::RotateDeg(angle_to_deg(a)))
        }
        Transform::RotateX(a) => Some(TransformOp::RotateXDeg(angle_to_deg(a))),
        Transform::RotateY(a) => Some(TransformOp::RotateYDeg(angle_to_deg(a))),
        Transform::Rotate3d(x, y, z, a) => Some(TransformOp::Rotate3d(*x, *y, *z, angle_to_deg(a))),
        Transform::Skew(ax, ay) => Some(TransformOp::SkewDeg(angle_to_deg(ax), angle_to_deg(ay))),
        Transform::SkewX(ax) => Some(TransformOp::SkewDeg(angle_to_deg(ax), 0.0)),
        Transform::SkewY(ay) => Some(TransformOp::SkewDeg(0.0, angle_to_deg(ay))),
        Transform::Perspective(len) => Some(TransformOp::Perspective(
            convert_length(len).resolve_px(0.0, &eval),
        )),
        Transform::Matrix(m) => Some(TransformOp::Matrix([m.a, m.b, m.c, m.d, m.e, m.f])),
        Transform::Matrix3d(m) => Some(TransformOp::Matrix3d([
            m.m11, m.m12, m.m13, m.m14, m.m21, m.m22, m.m23, m.m24, m.m31, m.m32, m.m33, m.m34,
            m.m41, m.m42, m.m43, m.m44,
        ])),
    }
}

#[must_use]
pub fn convert_filter(filter: &Filter<'_>) -> Option<FilterOp> {
    match filter {
        Filter::Blur(len) => Some(FilterOp::Blur(convert_length(len))),
        Filter::Brightness(v) => Some(FilterOp::Brightness(num_or_pct(v))),
        Filter::Contrast(v) => Some(FilterOp::Contrast(num_or_pct(v))),
        Filter::Grayscale(v) => Some(FilterOp::Grayscale(num_or_pct(v))),
        Filter::Opacity(v) => Some(FilterOp::Opacity(num_or_pct(v))),
        Filter::Saturate(v) => Some(FilterOp::Saturate(num_or_pct(v))),
        Filter::Invert(v) => Some(FilterOp::Invert(num_or_pct(v))),
        Filter::Sepia(v) => Some(FilterOp::Sepia(num_or_pct(v))),
        Filter::HueRotate(a) => Some(FilterOp::HueRotateDeg(angle_to_deg(a))),
        Filter::DropShadow(ds) => Some(FilterOp::DropShadow {
            offset_x: convert_length(&ds.x_offset),
            offset_y: convert_length(&ds.y_offset),
            blur: convert_length(&ds.blur),
            color: convert_color(&ds.color).unwrap_or(CssColor::CurrentColor),
        }),
        Filter::Url(_) => None,
    }
}

#[must_use]
pub fn convert_blend_mode(bm: &LightningBlendMode) -> MixBlendMode {
    match bm {
        LightningBlendMode::Normal => MixBlendMode::Normal,
        LightningBlendMode::Multiply => MixBlendMode::Multiply,
        LightningBlendMode::Screen => MixBlendMode::Screen,
        LightningBlendMode::Overlay => MixBlendMode::Overlay,
        LightningBlendMode::Darken => MixBlendMode::Darken,
        LightningBlendMode::Lighten => MixBlendMode::Lighten,
        LightningBlendMode::ColorDodge => MixBlendMode::ColorDodge,
        LightningBlendMode::ColorBurn => MixBlendMode::ColorBurn,
        LightningBlendMode::HardLight => MixBlendMode::HardLight,
        LightningBlendMode::SoftLight => MixBlendMode::SoftLight,
        LightningBlendMode::Difference => MixBlendMode::Difference,
        LightningBlendMode::Exclusion => MixBlendMode::Exclusion,
        LightningBlendMode::Hue => MixBlendMode::Hue,
        LightningBlendMode::Saturation => MixBlendMode::Saturation,
        LightningBlendMode::Color => MixBlendMode::Color,
        LightningBlendMode::Luminosity => MixBlendMode::Luminosity,
        LightningBlendMode::PlusDarker => MixBlendMode::Darken,
        LightningBlendMode::PlusLighter => MixBlendMode::Lighten,
    }
}

fn convert_shape_radius(r: &ShapeRadius) -> ClipRadius {
    match r {
        ShapeRadius::LengthPercentage(lp) => ClipRadius::Length(convert_length_percentage(lp)),
        ShapeRadius::ClosestSide => ClipRadius::ClosestSide,
        ShapeRadius::FarthestSide => ClipRadius::FarthestSide,
    }
}

fn parse_simple_lp(token: &str) -> LengthPercentage {
    let t = token.trim();
    if let Some(p) = t.strip_suffix('%') {
        if let Ok(v) = p.parse::<f32>() {
            return LengthPercentage::Percent(v / 100.0);
        }
    }
    if let Some(p) = t.strip_suffix("px") {
        if let Ok(v) = p.parse::<f32>() {
            return LengthPercentage::px(v);
        }
    }
    if let Ok(v) = t.parse::<f32>() {
        return LengthPercentage::px(v);
    }
    LengthPercentage::Zero
}

#[must_use]
pub fn convert_clip_path(cp: &LightningClipPath<'_>) -> ClipPathValue {
    match cp {
        LightningClipPath::None | LightningClipPath::Url(_) | LightningClipPath::Box(_) => {
            ClipPathValue::None
        }
        LightningClipPath::Shape { shape, .. } => match shape.as_ref() {
            BasicShape::Inset(ins) => ClipPathValue::Inset {
                top: convert_length_percentage(&ins.rect.0),
                right: convert_length_percentage(&ins.rect.1),
                bottom: convert_length_percentage(&ins.rect.2),
                left: convert_length_percentage(&ins.rect.3),
                radii: [
                    convert_length_percentage(&ins.radius.top_left.0),
                    convert_length_percentage(&ins.radius.top_right.0),
                    convert_length_percentage(&ins.radius.bottom_right.0),
                    convert_length_percentage(&ins.radius.bottom_left.0),
                ],
            },
            BasicShape::Circle(c) => {
                let (cx, cy) = convert_position_axes(&c.position);
                ClipPathValue::Circle {
                    radius: convert_shape_radius(&c.radius),
                    cx,
                    cy,
                }
            }
            BasicShape::Ellipse(e) => {
                let (cx, cy) = convert_position_axes(&e.position);
                ClipPathValue::Ellipse {
                    rx: convert_shape_radius(&e.radius_x),
                    ry: convert_shape_radius(&e.radius_y),
                    cx,
                    cy,
                }
            }
            BasicShape::Polygon(poly) => {
                let fill_odd = matches!(poly.fill_rule, FillRule::Evenodd);
                let mut points = Vec::with_capacity(poly.points.len());
                for pt in &poly.points {
                    if let Ok(s) = pt.to_css_string(PrinterOptions::default()) {
                        let mut parts = s.split_ascii_whitespace();
                        if let (Some(xs), Some(ys)) = (parts.next(), parts.next()) {
                            points.push((parse_simple_lp(xs), parse_simple_lp(ys)));
                        }
                    }
                }
                ClipPathValue::Polygon { fill_odd, points }
            }
        },
    }
}

#[must_use]
pub fn convert_easing(ef: &EasingFunction) -> TimingFunction {
    match ef {
        EasingFunction::Linear => TimingFunction::Linear,
        EasingFunction::Ease => TimingFunction::Ease,
        EasingFunction::EaseIn => TimingFunction::EaseIn,
        EasingFunction::EaseOut => TimingFunction::EaseOut,
        EasingFunction::EaseInOut => TimingFunction::EaseInOut,
        EasingFunction::CubicBezier { x1, y1, x2, y2 } => TimingFunction::CubicBezier(
            f64::from(*x1),
            f64::from(*y1),
            f64::from(*x2),
            f64::from(*y2),
        ),
        EasingFunction::Steps { count, position } => TimingFunction::Steps(
            (*count).max(1) as u32,
            matches!(position, StepPosition::Start | StepPosition::JumpBoth),
        ),
    }
}

#[must_use]
pub fn time_to_ms(t: &Time) -> f32 {
    t.to_ms()
}

#[must_use]
pub fn convert_transition(tr: &LightningTransition<'_>) -> TransitionSpec {
    TransitionSpec {
        property: intern_str(tr.property.name()),
        duration_ms: tr.duration.to_ms().max(0.0),
        delay_ms: tr.delay.to_ms(),
        timing: convert_easing(&tr.timing_function),
        behavior: TransitionBehaviorMode::Normal,
    }
}

#[must_use]
pub fn convert_animation_direction(d: &LightningAnimDirection) -> AnimationDirectionMode {
    match d {
        LightningAnimDirection::Normal => AnimationDirectionMode::Normal,
        LightningAnimDirection::Reverse => AnimationDirectionMode::Reverse,
        LightningAnimDirection::Alternate => AnimationDirectionMode::Alternate,
        LightningAnimDirection::AlternateReverse => AnimationDirectionMode::AlternateReverse,
    }
}

#[must_use]
pub fn convert_animation_fill(f: &LightningAnimFill) -> AnimationFillModeKind {
    match f {
        LightningAnimFill::None => AnimationFillModeKind::None,
        LightningAnimFill::Forwards => AnimationFillModeKind::Forwards,
        LightningAnimFill::Backwards => AnimationFillModeKind::Backwards,
        LightningAnimFill::Both => AnimationFillModeKind::Both,
    }
}

#[must_use]
pub fn convert_animation_play_state(p: &LightningAnimPlay) -> AnimationPlayStateMode {
    match p {
        LightningAnimPlay::Running => AnimationPlayStateMode::Running,
        LightningAnimPlay::Paused => AnimationPlayStateMode::Paused,
    }
}

#[must_use]
pub fn convert_animation_iteration(c: &LightningAnimIter) -> f32 {
    match c {
        LightningAnimIter::Number(n) => n.max(0.0),
        LightningAnimIter::Infinite => f32::INFINITY,
    }
}

#[must_use]
pub fn convert_animation_name(n: &LightningAnimName<'_>) -> InternedStr {
    match n {
        LightningAnimName::None => intern_str("none"),
        LightningAnimName::Ident(id) => intern_str(id.as_ref()),
        LightningAnimName::String(s) => intern_str(s.as_ref()),
    }
}

#[must_use]
pub fn convert_animation(anim: &LightningAnimation<'_>) -> AnimationSpec {
    AnimationSpec {
        name: convert_animation_name(&anim.name),
        duration_ms: anim.duration.to_ms().max(0.0),
        delay_ms: anim.delay.to_ms(),
        timing: convert_easing(&anim.timing_function),
        iteration_count: convert_animation_iteration(&anim.iteration_count),
        direction: convert_animation_direction(&anim.direction),
        fill_mode: convert_animation_fill(&anim.fill_mode),
        play_state: convert_animation_play_state(&anim.play_state),
        composition: AnimationCompositionMode::Replace,
    }
}

fn convert_track_breadth(tb: &TrackBreadth) -> GridTrackSize {
    match tb {
        TrackBreadth::Length(lp) => GridTrackSize::Length(convert_length_percentage(lp)),
        TrackBreadth::Flex(fr) => GridTrackSize::Fr(*fr),
        TrackBreadth::MinContent => GridTrackSize::MinContent,
        TrackBreadth::MaxContent => GridTrackSize::MaxContent,
        TrackBreadth::Auto => GridTrackSize::Auto,
    }
}

#[must_use]
pub fn convert_track_size(ts: &TrackSize) -> GridTrackSize {
    match ts {
        TrackSize::TrackBreadth(tb) => convert_track_breadth(tb),
        TrackSize::MinMax { min, max } => GridTrackSize::MinMax(
            Box::new(convert_track_breadth(min)),
            Box::new(convert_track_breadth(max)),
        ),
        TrackSize::FitContent(lp) => GridTrackSize::FitContent(convert_length_percentage(lp)),
    }
}

#[must_use]
pub fn convert_track_sizing(ts: &TrackSizing<'_>) -> Vec<GridTrackSize> {
    let mut out = Vec::new();
    let TrackSizing::TrackList(list) = ts else {
        return out;
    };
    for item in &list.items {
        match item {
            TrackListItem::TrackSize(size) => out.push(convert_track_size(size)),
            TrackListItem::TrackRepeat(rep) => {
                let count = match rep.count {
                    RepeatCount::Number(n) => (n.max(0) as usize).min(128),
                    RepeatCount::AutoFill | RepeatCount::AutoFit => 1,
                };
                for _ in 0..count {
                    for s in &rep.track_sizes {
                        out.push(convert_track_size(s));
                    }
                }
            }
        }
    }
    out
}

#[must_use]
pub fn convert_grid_auto_flow(flow: &GridAutoFlow) -> GridAutoFlowMode {
    let dense = flow.contains(GridAutoFlow::Dense);
    let col = flow.contains(GridAutoFlow::Column);
    match (col, dense) {
        (false, false) => GridAutoFlowMode::Row,
        (true, false) => GridAutoFlowMode::Column,
        (false, true) => GridAutoFlowMode::RowDense,
        (true, true) => GridAutoFlowMode::ColumnDense,
    }
}
