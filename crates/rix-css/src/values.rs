//! Typed CSS value representations and direct `lightningcss` AST converters.
//!
//! All converters consume `lightningcss` parsed values directly without
//! serializing intermediate CSS strings.

use lightningcss::properties::{
    align::{
        AlignContent, AlignItems, AlignSelf, ContentDistribution, ContentPosition, JustifyContent,
        JustifyItems, JustifySelf, SelfPosition,
    },
    background::{Background, BackgroundRepeatKeyword, BackgroundSize},
    border::{BorderSideWidth, LineStyle as LightningLineStyle},
    box_shadow::BoxShadow,
    display::{Display, DisplayInside, DisplayKeyword, DisplayOutside, Visibility},
    effects::Filter,
    flex::{FlexDirection, FlexWrap},
    font::{
        AbsoluteFontSize, AbsoluteFontWeight, FontFamily, FontSize, FontStyle, FontWeight,
        GenericFontFamily, LineHeight, RelativeFontSize,
    },
    grid::{GridAutoFlow, RepeatCount, TrackBreadth, TrackListItem, TrackSize, TrackSizing},
    overflow::OverflowKeyword,
    position::{Position, ZIndex},
    size::{MaxSize, Size},
    text::{
        Direction, OverflowWrap, Spacing, TextAlign, TextDecorationLine as LightningTextDeco,
        TextTransform, TextTransformCase, WhiteSpace, WordBreak,
    },
    transform::Transform,
    ui::{Cursor, CursorKeyword, UserSelect},
};
use lightningcss::values::{
    alpha::AlphaValue,
    angle::Angle,
    calc::{Calc, MathFunction},
    color::CssColor as LightningCssColor,
    image::Image,
    length::{Length, LengthPercentage as LightningLp, LengthPercentageOrAuto, LengthValue},
    percentage::{DimensionPercentage, NumberOrPercentage},
    position::{HorizontalPosition, VerticalPosition},
    size::Size2D,
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

#[derive(Debug, Clone, PartialEq, Default)]
pub struct BackgroundLayerValue {
    pub image_url: Option<InternedStr>,
    pub repeat_x: bool,
    pub repeat_y: bool,
    pub pos_x: LengthPercentage,
    pub pos_y: LengthPercentage,
    pub size_cover: bool,
    pub size_contain: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TransformOp {
    Translate(LengthPercentage, LengthPercentage),
    Scale(f32, f32),
    RotateDeg(f32),
    SkewDeg(f32, f32),
    Matrix([f32; 6]),
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
pub fn convert_border_width(width: &BorderSideWidth) -> f32 {
    match width {
        BorderSideWidth::Thin => 1.0,
        BorderSideWidth::Medium => 3.0,
        BorderSideWidth::Thick => 5.0,
        BorderSideWidth::Length(len) => convert_length(len)
            .resolve_px(0.0, &EvalContext::default())
            .max(0.0),
    }
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

#[must_use]
pub fn convert_background_layer(bg: &Background<'_>) -> BackgroundLayerValue {
    let image_url = match &bg.image {
        Image::Url(u) => Some(intern_str(u.url.as_ref())),
        _ => None,
    };
    let pos_x = match &bg.position.x {
        HorizontalPosition::Length(lp) => convert_length_percentage(lp),
        HorizontalPosition::Side { side, .. } => {
            use lightningcss::values::position::HorizontalPositionKeyword;
            match side {
                HorizontalPositionKeyword::Left => LengthPercentage::Percent(0.0),
                HorizontalPositionKeyword::Right => LengthPercentage::Percent(1.0),
            }
        }
        HorizontalPosition::Center => LengthPercentage::Percent(0.5),
    };
    let pos_y = match &bg.position.y {
        VerticalPosition::Length(lp) => convert_length_percentage(lp),
        VerticalPosition::Side { side, .. } => {
            use lightningcss::values::position::VerticalPositionKeyword;
            match side {
                VerticalPositionKeyword::Top => LengthPercentage::Percent(0.0),
                VerticalPositionKeyword::Bottom => LengthPercentage::Percent(1.0),
            }
        }
        VerticalPosition::Center => LengthPercentage::Percent(0.5),
    };
    BackgroundLayerValue {
        image_url,
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
pub fn convert_transform(tf: &Transform) -> Option<TransformOp> {
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
        Transform::Translate3d(x, y, _) => Some(TransformOp::Translate(
            convert_length_percentage(x),
            convert_length_percentage(y),
        )),
        Transform::Scale(x, y) => Some(TransformOp::Scale(num_or_pct(x), num_or_pct(y))),
        Transform::ScaleX(x) => Some(TransformOp::Scale(num_or_pct(x), 1.0)),
        Transform::ScaleY(y) => Some(TransformOp::Scale(1.0, num_or_pct(y))),
        Transform::Scale3d(x, y, _) => Some(TransformOp::Scale(num_or_pct(x), num_or_pct(y))),
        Transform::Rotate(a) | Transform::RotateZ(a) => {
            Some(TransformOp::RotateDeg(angle_to_deg(a)))
        }
        Transform::Skew(ax, ay) => Some(TransformOp::SkewDeg(angle_to_deg(ax), angle_to_deg(ay))),
        Transform::SkewX(ax) => Some(TransformOp::SkewDeg(angle_to_deg(ax), 0.0)),
        Transform::SkewY(ay) => Some(TransformOp::SkewDeg(0.0, angle_to_deg(ay))),
        Transform::Matrix(m) => Some(TransformOp::Matrix([m.a, m.b, m.c, m.d, m.e, m.f])),
        _ => None,
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
        Filter::DropShadow(_) | Filter::Url(_) => None,
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
