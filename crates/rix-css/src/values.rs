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

/// CSS relative or absolute length unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum LengthUnit {
    Px,
    Rem,
    Em,
    Ex,
    Ch,
    Cap,
    Ic,
    Lh,
    Rlh,
    Vw,
    Vh,
    Vmin,
    Vmax,
    Svw,
    Svh,
    Lvw,
    Lvh,
    Dvw,
    Dvh,
    Cqw,
    Cqh,
    Cqi,
    Cqb,
}

impl LengthUnit {
    #[must_use]
    pub fn from_unit_str(unit: &str) -> Option<Self> {
        match unit.to_ascii_lowercase().as_str() {
            "px" => Some(Self::Px),
            "rem" => Some(Self::Rem),
            "em" => Some(Self::Em),
            "ex" => Some(Self::Ex),
            "ch" => Some(Self::Ch),
            "cap" => Some(Self::Cap),
            "ic" => Some(Self::Ic),
            "lh" => Some(Self::Lh),
            "rlh" => Some(Self::Rlh),
            "vw" => Some(Self::Vw),
            "vh" => Some(Self::Vh),
            "vmin" => Some(Self::Vmin),
            "vmax" => Some(Self::Vmax),
            "svw" => Some(Self::Svw),
            "svh" => Some(Self::Svh),
            "lvw" => Some(Self::Lvw),
            "lvh" => Some(Self::Lvh),
            "dvw" => Some(Self::Dvw),
            "dvh" => Some(Self::Dvh),
            "cqw" => Some(Self::Cqw),
            "cqh" => Some(Self::Cqh),
            "cqi" => Some(Self::Cqi),
            "cqb" => Some(Self::Cqb),
            _ => None,
        }
    }

    #[must_use]
    pub fn resolve_px(self, value: f32, ctx: &EvalContext) -> f32 {
        match self {
            Self::Px => value,
            Self::Rem => value * ctx.root_font_size_px,
            Self::Em => value * ctx.font_size_px,
            Self::Ex | Self::Ch => value * ctx.font_size_px * 0.5,
            Self::Cap => value * ctx.font_size_px * 0.7,
            Self::Ic => value * ctx.font_size_px,
            Self::Lh => value * ctx.font_size_px * 1.2,
            Self::Rlh => value * ctx.root_font_size_px * 1.2,
            Self::Vw | Self::Svw | Self::Lvw | Self::Dvw => value * ctx.viewport_width_px * 0.01,
            Self::Vh | Self::Svh | Self::Lvh | Self::Dvh => value * ctx.viewport_height_px * 0.01,
            Self::Vmin => value * ctx.viewport_width_px.min(ctx.viewport_height_px) * 0.01,
            Self::Vmax => value * ctx.viewport_width_px.max(ctx.viewport_height_px) * 0.01,
            Self::Cqw | Self::Cqi => value * ctx.container_width_px * 0.01,
            Self::Cqh | Self::Cqb => value * ctx.container_height_px * 0.01,
        }
    }
}

/// Evaluation environment for resolving relative CSS units and `calc()` expressions.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EvalContext {
    pub font_size_px: f32,
    pub root_font_size_px: f32,
    pub viewport_width_px: f32,
    pub viewport_height_px: f32,
    pub container_width_px: f32,
    pub container_height_px: f32,
}

impl Default for EvalContext {
    fn default() -> Self {
        Self {
            font_size_px: 16.0,
            root_font_size_px: 16.0,
            viewport_width_px: 1280.0,
            viewport_height_px: 720.0,
            container_width_px: 1280.0,
            container_height_px: 720.0,
        }
    }
}

/// Typed CSS math expression supporting `calc()`, `min()`, `max()`, `clamp()`, `abs()`, `sign()`.
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
}

impl CalcExpr {
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
    Table,
    InlineTable,
    TableRow,
    TableCell,
    ListItem,
    Contents,
    None,
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
    Center,
    Baseline,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum GridPlacementValue {
    #[default]
    Auto,
    Line(i16),
    Span(u16),
    Area(InternedStr),
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
    CubicBezier(f32, f32, f32, f32),
    Steps(u32, bool),
}

impl TimingFunction {
    /// Evaluates the CSS easing curve at normalized progress `t` in `[0.0, 1.0]`.
    #[must_use]
    pub fn evaluate(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            Self::Ease => eval_cubic_bezier(0.25, 0.1, 0.25, 1.0, t),
            Self::EaseIn => eval_cubic_bezier(0.42, 0.0, 1.0, 1.0, t),
            Self::EaseOut => eval_cubic_bezier(0.0, 0.0, 0.58, 1.0, t),
            Self::EaseInOut => eval_cubic_bezier(0.42, 0.0, 0.58, 1.0, t),
            Self::CubicBezier(x1, y1, x2, y2) => eval_cubic_bezier(x1, y1, x2, y2, t),
            Self::Steps(count, jump_start) => {
                let n = count.max(1) as f32;
                if jump_start {
                    ((t * n).floor() + 1.0).min(n) / n
                } else {
                    (t * n).floor() / n
                }
            }
        }
    }
}

fn eval_cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let sample_x = |u: f32| {
        let inv = 1.0 - u;
        3.0 * inv * inv * u * x1 + 3.0 * inv * u * u * x2 + u * u * u
    };
    let sample_dx = |u: f32| {
        let inv = 1.0 - u;
        3.0 * inv * inv * x1 + 6.0 * inv * u * (x2 - x1) + 3.0 * u * u * (1.0 - x2)
    };
    let sample_y = |u: f32| {
        let inv = 1.0 - u;
        3.0 * inv * inv * u * y1 + 3.0 * inv * u * u * y2 + u * u * u
    };
    let mut u = x;
    for _ in 0..6 {
        let err = sample_x(u) - x;
        if err.abs() < 1e-5 {
            break;
        }
        let d = sample_dx(u);
        if d.abs() < 1e-6 {
            break;
        }
        u = (u - err / d).clamp(0.0, 1.0);
    }
    sample_y(u)
}

#[derive(Debug, Clone, PartialEq)]
pub struct TransitionSpec {
    pub property: InternedStr,
    pub duration_ms: f32,
    pub delay_ms: f32,
    pub timing: TimingFunction,
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GridTrackSize {
    Auto,
    MinContent,
    MaxContent,
    Length(LengthPercentage),
    Fr(f32),
    MinMax(Box<GridTrackSize>, Box<GridTrackSize>),
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
            DisplayKeyword::TableRow => DisplayMode::TableRow,
            DisplayKeyword::TableCell => DisplayMode::TableCell,
            _ => DisplayMode::Block,
        },
        Display::Pair(pair) => {
            if pair.is_list_item {
                return DisplayMode::ListItem;
            }
            match (pair.outside, &pair.inside) {
                (DisplayOutside::Inline, DisplayInside::Flow) => DisplayMode::Inline,
                (DisplayOutside::Inline, DisplayInside::FlowRoot) => DisplayMode::InlineBlock,
                (DisplayOutside::Inline, DisplayInside::Flex(_) | DisplayInside::Box(_)) => {
                    DisplayMode::InlineFlex
                }
                (DisplayOutside::Inline, DisplayInside::Grid) => DisplayMode::InlineGrid,
                (DisplayOutside::Inline, DisplayInside::Table) => DisplayMode::InlineTable,
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
        SelfPosition::Start | SelfPosition::SelfStart => AlignValue::Start,
        SelfPosition::End | SelfPosition::SelfEnd => AlignValue::End,
        SelfPosition::FlexStart => AlignValue::FlexStart,
        SelfPosition::FlexEnd => AlignValue::FlexEnd,
    }
}

#[must_use]
pub fn convert_align_content(ac: &AlignContent) -> AlignValue {
    match ac {
        AlignContent::Normal => AlignValue::Normal,
        AlignContent::BaselinePosition(_) => AlignValue::Baseline,
        AlignContent::ContentDistribution(d) => convert_content_dist(d),
        AlignContent::ContentPosition { value, .. } => convert_content_position(value),
    }
}

#[must_use]
pub fn convert_justify_content(jc: &JustifyContent) -> AlignValue {
    match jc {
        JustifyContent::Normal => AlignValue::Normal,
        JustifyContent::ContentDistribution(d) => convert_content_dist(d),
        JustifyContent::ContentPosition { value, .. } => convert_content_position(value),
        JustifyContent::Left { .. } => AlignValue::Start,
        JustifyContent::Right { .. } => AlignValue::End,
    }
}

#[must_use]
pub fn convert_align_items(ai: &AlignItems) -> AlignValue {
    match ai {
        AlignItems::Normal => AlignValue::Normal,
        AlignItems::Stretch => AlignValue::Stretch,
        AlignItems::BaselinePosition(_) => AlignValue::Baseline,
        AlignItems::SelfPosition { value, .. } => convert_self_position(value),
    }
}

#[must_use]
pub fn convert_align_self(as_: &AlignSelf) -> AlignValue {
    match as_ {
        AlignSelf::Auto => AlignValue::Auto,
        AlignSelf::Normal => AlignValue::Normal,
        AlignSelf::Stretch => AlignValue::Stretch,
        AlignSelf::BaselinePosition(_) => AlignValue::Baseline,
        AlignSelf::SelfPosition { value, .. } => convert_self_position(value),
    }
}

#[must_use]
pub fn convert_justify_items(ji: &JustifyItems) -> AlignValue {
    match ji {
        JustifyItems::Normal => AlignValue::Normal,
        JustifyItems::Stretch => AlignValue::Stretch,
        JustifyItems::BaselinePosition(_) => AlignValue::Baseline,
        JustifyItems::SelfPosition { value, .. } => convert_self_position(value),
        JustifyItems::Left { .. } => AlignValue::Start,
        JustifyItems::Right { .. } => AlignValue::End,
        JustifyItems::Legacy(_) => AlignValue::Normal,
    }
}

#[must_use]
pub fn convert_justify_self(js: &JustifySelf) -> AlignValue {
    match js {
        JustifySelf::Auto => AlignValue::Auto,
        JustifySelf::Normal => AlignValue::Normal,
        JustifySelf::Stretch => AlignValue::Stretch,
        JustifySelf::BaselinePosition(_) => AlignValue::Baseline,
        JustifySelf::SelfPosition { value, .. } => convert_self_position(value),
        JustifySelf::Left { .. } => AlignValue::Start,
        JustifySelf::Right { .. } => AlignValue::End,
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
        EasingFunction::CubicBezier { x1, y1, x2, y2 } => {
            TimingFunction::CubicBezier(*x1, *y1, *x2, *y2)
        }
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
        TrackSize::FitContent(lp) => GridTrackSize::Length(convert_length_percentage(lp)),
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
