//! W3C CSS Color Level 4 & Level 5 (plus Color 6 `color-layers()`) parser, evaluator,
//! gamut mapper, and CSSOM serializer.

use cssparser::{Parser, ParserInput, Token};
use serde::{Deserialize, Serialize};

use crate::values::{ColorRgba, EvalContext, LengthUnit};

/// CSS Color 4/5 color spaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ColorSpace {
    SrgbLegacy,
    Srgb,
    SrgbLinear,
    DisplayP3,
    DisplayP3Linear,
    A98Rgb,
    ProphotoRgb,
    Rec2020,
    XyzD50,
    XyzD65,
    Lab,
    Lch,
    Oklab,
    Oklch,
    Hsl,
    Hwb,
}

#[rustfmt::skip]
const COLOR_SPACE_IDENTS: &[(&str, ColorSpace, bool)] = &[
    ("srgb", ColorSpace::Srgb, true), ("srgb-linear", ColorSpace::SrgbLinear, true),
    ("display-p3", ColorSpace::DisplayP3, true), ("display-p3-linear", ColorSpace::DisplayP3Linear, true),
    ("a98-rgb", ColorSpace::A98Rgb, true), ("prophoto-rgb", ColorSpace::ProphotoRgb, true),
    ("rec2020", ColorSpace::Rec2020, true), ("xyz", ColorSpace::XyzD65, true),
    ("xyz-d65", ColorSpace::XyzD65, true), ("xyz-d50", ColorSpace::XyzD50, true),
    ("lab", ColorSpace::Lab, false), ("lch", ColorSpace::Lch, false),
    ("oklab", ColorSpace::Oklab, false), ("oklch", ColorSpace::Oklch, false),
    ("hsl", ColorSpace::Hsl, false), ("hwb", ColorSpace::Hwb, false),
];

impl ColorSpace {
    #[must_use]
    pub fn from_color_fn_ident(ident: &str) -> Option<Self> {
        COLOR_SPACE_IDENTS
            .iter()
            .find(|&&(k, _, predef)| predef && k.eq_ignore_ascii_case(ident))
            .map(|&(_, sp, _)| sp)
    }

    #[must_use]
    pub fn from_interp_ident(ident: &str) -> Option<Self> {
        COLOR_SPACE_IDENTS
            .iter()
            .find(|&&(k, _, _)| k.eq_ignore_ascii_case(ident))
            .map(|&(_, sp, _)| sp)
    }

    #[must_use]
    pub const fn is_polar(self) -> bool {
        matches!(self, Self::Hsl | Self::Hwb | Self::Lch | Self::Oklch)
    }

    #[must_use]
    pub const fn is_legacy(self) -> bool {
        matches!(self, Self::SrgbLegacy | Self::Hsl | Self::Hwb)
    }

    #[must_use]
    pub const fn hue_index(self) -> Option<usize> {
        match self {
            Self::Hsl | Self::Hwb => Some(0),
            Self::Lch | Self::Oklch => Some(2),
            _ => None,
        }
    }

    #[must_use]
    #[rustfmt::skip]
    pub const fn css_name(self) -> &'static str {
        match self {
            Self::SrgbLegacy | Self::Srgb => "srgb", Self::SrgbLinear => "srgb-linear",
            Self::DisplayP3 => "display-p3", Self::DisplayP3Linear => "display-p3-linear",
            Self::A98Rgb => "a98-rgb", Self::ProphotoRgb => "prophoto-rgb", Self::Rec2020 => "rec2020",
            Self::XyzD50 => "xyz-d50", Self::XyzD65 => "xyz-d65", Self::Lab => "lab", Self::Lch => "lch",
            Self::Oklab => "oklab", Self::Oklch => "oklch", Self::Hsl => "hsl", Self::Hwb => "hwb",
        }
    }
}

/// Hue interpolation method for polar `color-mix()` (CSS Color 4 §12.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum HueMethod {
    #[default]
    Shorter,
    Longer,
    Increasing,
    Decreasing,
}

impl HueMethod {
    #[must_use]
    pub const fn css_suffix(self) -> &'static str {
        match self {
            Self::Shorter => "",
            Self::Longer => " longer hue",
            Self::Increasing => " increasing hue",
            Self::Decreasing => " decreasing hue",
        }
    }
}

/// Concrete computed color value preserving wide-gamut space, unclamped floats, and `none`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedColor {
    pub space: ColorSpace,
    pub c: [Option<f64>; 3],
    pub alpha: Option<f64>,
}

impl ResolvedColor {
    pub const BLACK_LEGACY: Self = Self::from_rgba(ColorRgba::BLACK);
    pub const TRANSPARENT_LEGACY: Self = Self::from_rgba(ColorRgba::TRANSPARENT);

    #[must_use]
    pub const fn from_rgba(rgba: ColorRgba) -> Self {
        Self {
            space: ColorSpace::SrgbLegacy,
            c: [
                Some(rgba.r as f64 / 255.0),
                Some(rgba.g as f64 / 255.0),
                Some(rgba.b as f64 / 255.0),
            ],
            alpha: Some(rgba.a as f64 / 255.0),
        }
    }

    #[must_use]
    pub fn from_legacy_f64(r255: f64, g255: f64, b255: f64, alpha: f64) -> Self {
        Self::from_rgba(ColorRgba::rgba(
            clamp_round_u8(r255),
            clamp_round_u8(g255),
            clamp_round_u8(b255),
            clamp_round_u8(alpha * 255.0),
        ))
    }

    /// Converts to GPU/layout-ready 8-bit sRGB with CSS Color 4 §13.2 OKLCH gamut mapping.
    #[must_use]
    pub fn to_srgb_u8(self) -> ColorRgba {
        let a = clamp_round_u8(self.alpha.unwrap_or(0.0).clamp(0.0, 1.0) * 255.0);
        if self.space == ColorSpace::SrgbLegacy {
            return ColorRgba::rgba(
                clamp_round_u8(self.c[0].unwrap_or(0.0) * 255.0),
                clamp_round_u8(self.c[1].unwrap_or(0.0) * 255.0),
                clamp_round_u8(self.c[2].unwrap_or(0.0) * 255.0),
                a,
            );
        }
        let rgb = self
            .convert_to(ColorSpace::Srgb)
            .c
            .map(|v| v.unwrap_or(0.0));
        let mapped = if rgb.iter().all(|v| (0.0..=1.0).contains(v)) {
            rgb
        } else {
            gamut_map_to_srgb(self)
        };
        ColorRgba::rgba(
            clamp_round_u8(mapped[0] * 255.0),
            clamp_round_u8(mapped[1] * 255.0),
            clamp_round_u8(mapped[2] * 255.0),
            a,
        )
    }

    /// Converts to `target` color space following CSS Color 4 §4.4 (powerless) and §12.2 (missing).
    #[must_use]
    pub fn convert_to(self, target: ColorSpace) -> Self {
        let src_norm = if self.space == ColorSpace::SrgbLegacy {
            ColorSpace::Srgb
        } else {
            self.space
        };
        let dst_norm = if target == ColorSpace::SrgbLegacy {
            ColorSpace::Srgb
        } else {
            target
        };
        if src_norm == dst_norm {
            return Self {
                space: target,
                c: self.c,
                alpha: self.alpha,
            };
        }
        if self.c.iter().all(Option::is_none) {
            return Self {
                space: target,
                c: [None; 3],
                alpha: self.alpha,
            };
        }

        let (src_c, mut v) = normalize_source_powerless(src_norm, self.c);
        let miss = MissingAnalogues::from_color(src_norm, src_c);
        for i in 0..3 {
            if src_c[i].is_none() {
                v[i] = 0.0;
            }
        }

        let mut out_opt: [Option<f64>; 3] = match (src_norm, dst_norm) {
            (ColorSpace::Srgb, ColorSpace::Hsl) => srgb_to_hsl_opt(v),
            (ColorSpace::Srgb, ColorSpace::Hwb) => srgb_to_hwb_opt(v),
            (ColorSpace::Hsl, ColorSpace::Srgb) => hsl_to_srgb(v).map(Some),
            (ColorSpace::Hwb, ColorSpace::Srgb) => hwb_to_srgb(v).map(Some),
            (ColorSpace::Hsl, ColorSpace::Hwb) => srgb_to_hwb_opt(hsl_to_srgb(v)),
            (ColorSpace::Hwb, ColorSpace::Hsl) => srgb_to_hsl_opt(hwb_to_srgb(v)),
            (ColorSpace::Lab, ColorSpace::Lch) => lab_to_lch_opt(v, LCH_CHROMA_EPS),
            (ColorSpace::Lch, ColorSpace::Lab) => lch_to_lab(v).map(Some),
            (ColorSpace::Oklab, ColorSpace::Oklch) => lab_to_lch_opt(v, OKLCH_CHROMA_EPS),
            (ColorSpace::Oklch, ColorSpace::Oklab) => lch_to_lab(v).map(Some),
            _ => from_xyz_d65_opt(dst_norm, to_xyz_d65(src_norm, v)),
        };

        miss.apply_to(dst_norm, &mut out_opt);
        if matches!(
            (src_norm, dst_norm),
            (ColorSpace::Hwb, ColorSpace::Hsl) | (ColorSpace::Hsl, ColorSpace::Hwb)
        ) && self.c[1].is_none()
            && self.c[2].is_none()
        {
            out_opt[1] = None;
            out_opt[2] = None;
        }
        if matches!(src_norm, ColorSpace::Lab | ColorSpace::Oklab)
            && self.c[1].is_none()
            && self.c[2].is_none()
            && matches!(
                dst_norm,
                ColorSpace::Hsl | ColorSpace::Lch | ColorSpace::Oklch
            )
        {
            out_opt[1] = None;
        }
        Self {
            space: target,
            c: out_opt,
            alpha: self.alpha,
        }
    }

    /// Serializes this computed color according to W3C CSS Color 4 §15.
    #[must_use]
    pub fn to_computed_css_string(self) -> String {
        if self.space == ColorSpace::SrgbLegacy {
            return format_legacy_rgba(self.to_srgb_u8());
        }
        let (a, b, c) = if matches!(self.space, ColorSpace::Hsl | ColorSpace::Hwb) {
            (
                format_opt_num(self.c[0]),
                format_opt_pct(self.c[1]),
                format_opt_pct(self.c[2]),
            )
        } else {
            (
                format_opt_num(self.c[0]),
                format_opt_num(self.c[1]),
                format_opt_num(self.c[2]),
            )
        };
        let prefix = if matches!(
            self.space,
            ColorSpace::Hsl
                | ColorSpace::Hwb
                | ColorSpace::Lab
                | ColorSpace::Lch
                | ColorSpace::Oklab
                | ColorSpace::Oklch
        ) {
            format!("{}({a} {b} {c}", self.space.css_name())
        } else {
            format!("color({} {a} {b} {c}", self.space.css_name())
        };
        format_with_alpha(&prefix, self.alpha)
    }
}

/// CSS Color 4 §15.5 shortest-roundtrip `rgb()` / `rgba()` serialization for 8-bit sRGB.
#[must_use]
pub fn format_legacy_rgba(c: ColorRgba) -> String {
    if c.a == 255 {
        return format!("rgb({}, {}, {})", c.r, c.g, c.b);
    }
    if c.a == 0 {
        return format!("rgba({}, {}, {}, 0)", c.r, c.g, c.b);
    }
    let a2 = (f64::from(c.a) / 255.0 * 100.0).round() / 100.0;
    let alpha_str = if (a2 * 255.0).round() as u8 == c.a {
        format_css_f64(a2)
    } else {
        format_css_f64((f64::from(c.a) / 255.0 * 1000.0).round() / 1000.0)
    };
    format!("rgba({}, {}, {}, {alpha_str})", c.r, c.g, c.b)
}

fn format_with_alpha(prefix: &str, alpha: Option<f64>) -> String {
    match alpha {
        None => format!("{prefix} / none)"),
        Some(a) => {
            let clamped = a.clamp(0.0, 1.0);
            if (clamped - 1.0).abs() < 1e-6 {
                format!("{prefix})")
            } else {
                format!("{prefix} / {})", format_css_f64(clamped))
            }
        }
    }
}

fn format_opt_num(v: Option<f64>) -> String {
    match v {
        None => "none".to_string(),
        Some(x) if !x.is_finite() => format!("calc({})", format_non_finite(x)),
        Some(x) => format_css_f64(x),
    }
}

fn format_opt_pct(v: Option<f64>) -> String {
    match v {
        None => "none".to_string(),
        Some(x) if !x.is_finite() => format!("calc({}%)", format_non_finite(x)),
        Some(x) => format!("{}%", format_css_f64(x)),
    }
}

fn format_non_finite(v: f64) -> &'static str {
    if v.is_nan() {
        "NaN"
    } else if v.is_sign_positive() {
        "infinity"
    } else {
        "-infinity"
    }
}

#[must_use]
pub fn format_css_f64(v: f64) -> String {
    if !v.is_finite() {
        return if v.is_nan() {
            "0".to_string()
        } else {
            format_non_finite(v).to_string()
        };
    }
    let v = if v.abs() < 1e-9 { 0.0 } else { v };
    let dec: usize = if v.abs() >= 1.0 {
        (5 - v.abs().log10().floor() as i32).clamp(0, 6) as usize
    } else {
        6
    };
    let factor = 10f64.powi(dec as i32);
    let rounded = (v * factor).round() / factor;
    let rounded = if rounded == 0.0 { 0.0 } else { rounded };
    if (rounded - rounded.round()).abs() < 1e-6 {
        format!("{}", rounded.round() as i64)
    } else {
        format!("{rounded:.dec$}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

#[inline]
fn clamp_round_u8(v: f64) -> u8 {
    if !v.is_finite() {
        return if v.is_sign_positive() { 255 } else { 0 };
    }
    (v.clamp(0.0, 255.0) + 0.5 + 1e-9).floor() as u8
}

// ---------------------------------------------------------------------------
// CSS Color 4 §4.4 Powerless & §12.2 Analogous Missing Components + Matrices
// ---------------------------------------------------------------------------

const HSL_SAT_EPS: f64 = 0.001;
const HWB_SUM_EPS: f64 = 0.001;
const LCH_CHROMA_EPS: f64 = 0.0015;
const OKLCH_CHROMA_EPS: f64 = 0.000004;

fn normalize_source_powerless(
    space: ColorSpace,
    mut c: [Option<f64>; 3],
) -> ([Option<f64>; 3], [f64; 3]) {
    let mut v = c.map(|x| x.unwrap_or(0.0));
    match space {
        ColorSpace::Hsl => {
            if v[1].abs() <= HSL_SAT_EPS || v[2].abs() <= 1e-6 || (v[2] - 100.0).abs() <= 1e-6 {
                if v[1].abs() <= HSL_SAT_EPS {
                    v[1] = 0.0;
                }
                c[0] = None;
            }
        }
        ColorSpace::Hwb if v[1] + v[2] >= 100.0 - HWB_SUM_EPS => {
            c[0] = None;
            let s = v[1] + v[2];
            if s > 0.0 && s < 100.0 {
                v[1] = v[1] / s * 100.0;
                v[2] = 100.0 - v[1];
            }
        }
        ColorSpace::Lch if v[1].abs() <= LCH_CHROMA_EPS => {
            v[1] = 0.0;
            c[2] = None;
        }
        ColorSpace::Oklch if v[1].abs() <= OKLCH_CHROMA_EPS => {
            v[1] = 0.0;
            c[2] = None;
        }
        _ => {}
    }
    (c, v)
}

#[derive(Default, Clone, Copy)]
struct MissingAnalogues(u8);

impl MissingAnalogues {
    const R: u8 = 1 << 0;
    const G: u8 = 1 << 1;
    const B: u8 = 1 << 2;
    const L: u8 = 1 << 3;
    const C: u8 = 1 << 4;
    const H: u8 = 1 << 5;
    const A: u8 = 1 << 6;
    const BB: u8 = 1 << 7;

    fn from_color(space: ColorSpace, c: [Option<f64>; 3]) -> Self {
        let tags = Self::tags(space);
        let mut bits = 0u8;
        for i in 0..3 {
            if c[i].is_none() {
                bits |= tags[i];
            }
        }
        Self(bits)
    }

    fn apply_to(self, space: ColorSpace, c: &mut [Option<f64>; 3]) {
        let tags = Self::tags(space);
        for i in 0..3 {
            if tags[i] != 0 && (self.0 & tags[i]) != 0 {
                c[i] = None;
            }
        }
    }

    #[rustfmt::skip]
    const fn tags(space: ColorSpace) -> [u8; 3] {
        match space {
            ColorSpace::Srgb | ColorSpace::SrgbLegacy | ColorSpace::SrgbLinear | ColorSpace::DisplayP3
            | ColorSpace::DisplayP3Linear | ColorSpace::A98Rgb | ColorSpace::ProphotoRgb
            | ColorSpace::Rec2020 => [Self::R, Self::G, Self::B],
            ColorSpace::Lab | ColorSpace::Oklab => [Self::L, Self::A, Self::BB],
            ColorSpace::Lch | ColorSpace::Oklch => [Self::L, Self::C, Self::H],
            ColorSpace::Hsl => [Self::H, Self::C, Self::L],
            ColorSpace::Hwb => [Self::H, 0, 0],
            ColorSpace::XyzD50 | ColorSpace::XyzD65 => [0, 0, 0],
        }
    }
}

#[inline]
fn mul3(m: &[[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

#[rustfmt::skip]
mod transfer {
    pub fn srgb_to_lin(v: f64) -> f64 { let a = v.abs(); if a <= 0.04045 { v / 12.92 } else { v.signum() * ((a + 0.055) / 1.055).powf(2.4) } }
    pub fn lin_to_srgb(v: f64) -> f64 { let a = v.abs(); if a > 0.0031308 { v.signum() * (1.055 * a.powf(1.0 / 2.4) - 0.055) } else { 12.92 * v } }
    pub fn a98_to_lin(v: f64) -> f64 { v.signum() * v.abs().powf(563.0 / 256.0) }
    pub fn lin_to_a98(v: f64) -> f64 { v.signum() * v.abs().powf(256.0 / 563.0) }
    pub fn prophoto_to_lin(v: f64) -> f64 { let a = v.abs(); if a <= 16.0 / 512.0 { v / 16.0 } else { v.signum() * a.powf(1.8) } }
    pub fn lin_to_prophoto(v: f64) -> f64 { let a = v.abs(); if a >= 1.0 / 512.0 { v.signum() * a.powf(1.0 / 1.8) } else { 16.0 * v } }
    pub fn rec2020_to_lin(v: f64) -> f64 { const A: f64 = 1.09929682680944; const B: f64 = 0.018053968510807; let a = v.abs(); if a < B * 4.5 { v / 4.5 } else { v.signum() * ((a + A - 1.0) / A).powf(1.0 / 0.45) } }
    pub fn lin_to_rec2020(v: f64) -> f64 { const A: f64 = 1.09929682680944; const B: f64 = 0.018053968510807; let a = v.abs(); if a > B { v.signum() * (A * a.powf(0.45) - (A - 1.0)) } else { 4.5 * v } }
}

// Exact W3C CSS Color 4 §17 transformation matrices.
#[rustfmt::skip]
const LIN_SRGB_TO_XYZ_D65: [[f64; 3]; 3] = [
    [506752.0 / 1228815.0, 87881.0 / 245763.0, 12673.0 / 70218.0],
    [87098.0 / 409605.0, 175762.0 / 245763.0, 12673.0 / 175545.0],
    [7918.0 / 409605.0, 87881.0 / 737289.0, 1001167.0 / 1053270.0],
];
#[rustfmt::skip]
const XYZ_D65_TO_LIN_SRGB: [[f64; 3]; 3] = [
    [12831.0 / 3959.0, -329.0 / 214.0, -1974.0 / 3959.0],
    [-851781.0 / 878810.0, 1648619.0 / 878810.0, 36519.0 / 878810.0],
    [705.0 / 12673.0, -2585.0 / 12673.0, 705.0 / 667.0],
];
#[rustfmt::skip]
const LIN_P3_TO_XYZ_D65: [[f64; 3]; 3] = [
    [608311.0 / 1250200.0, 189793.0 / 714400.0, 198249.0 / 1000160.0],
    [35783.0 / 156275.0, 247089.0 / 357200.0, 198249.0 / 2500400.0],
    [0.0, 32229.0 / 714400.0, 5220557.0 / 5000800.0],
];
#[rustfmt::skip]
const XYZ_D65_TO_LIN_P3: [[f64; 3]; 3] = [
    [446124.0 / 178915.0, -333277.0 / 357830.0, -72051.0 / 178915.0],
    [-14852.0 / 17905.0, 63121.0 / 35810.0, 423.0 / 17905.0],
    [11844.0 / 330415.0, -50337.0 / 660830.0, 316169.0 / 330415.0],
];
#[rustfmt::skip]
const LIN_A98_TO_XYZ_D65: [[f64; 3]; 3] = [
    [573536.0 / 994567.0, 263643.0 / 1420810.0, 187206.0 / 994567.0],
    [591459.0 / 1989134.0, 6239551.0 / 9945670.0, 374412.0 / 4972835.0],
    [53769.0 / 1989134.0, 351524.0 / 4972835.0, 4929758.0 / 4972835.0],
];
#[rustfmt::skip]
const XYZ_D65_TO_LIN_A98: [[f64; 3]; 3] = [
    [2.041587903810746, -0.5650069742788597, -0.3447313507783296],
    [-0.9692436362808798, 1.8759675015077213, 0.0415550574071756],
    [0.013444280632031, -0.1183623922310183, 1.0151749943912056],
];
#[rustfmt::skip]
const LIN_PROPHOTO_TO_XYZ_D50: [[f64; 3]; 3] = [
    [0.7977604896723027, 0.13518583717574031, 0.0313493495815248],
    [0.2880711282292934, 0.7118432178101014, 0.00008565396060525902],
    [0.0, 0.0, 0.8251046025104601],
];
#[rustfmt::skip]
const XYZ_D50_TO_LIN_PROPHOTO: [[f64; 3]; 3] = [
    [1.3457989731028281, -0.25558010007997534, -0.05110628506753401],
    [-0.5446224939028347, 1.5082327413132781, 0.02053603239147973],
    [0.0, 0.0, 1.2119675456389454],
];
#[rustfmt::skip]
const LIN_REC2020_TO_XYZ_D65: [[f64; 3]; 3] = [
    [63426534.0 / 99577255.0, 20160776.0 / 139408157.0, 47086771.0 / 278816314.0],
    [26158966.0 / 99577255.0, 472592308.0 / 697040785.0, 8267143.0 / 139408157.0],
    [0.0, 19567812.0 / 697040785.0, 295819943.0 / 278816314.0],
];
#[rustfmt::skip]
const XYZ_D65_TO_LIN_REC2020: [[f64; 3]; 3] = [
    [30757411.0 / 17917100.0, -6372589.0 / 17917100.0, -4539589.0 / 17917100.0],
    [-19765991.0 / 29648200.0, 47925759.0 / 29648200.0, 467509.0 / 29648200.0],
    [792561.0 / 44930125.0, -1921689.0 / 44930125.0, 42328811.0 / 44930125.0],
];
// Exact W3C CSS Color 4 §17 Bradford D65 <-> D50 chromatic adaptation matrices.
#[rustfmt::skip]
const D65_TO_D50: [[f64; 3]; 3] = [
    [1.0479297925449966, 0.022946870601609527, -0.050192266289205194],
    [0.029627808770055674, 0.99043442675388, -0.01707379906341879],
    [-0.009243040646204521, 0.015055191490298164, 0.751874281428137],
];
#[rustfmt::skip]
const D50_TO_D65: [[f64; 3]; 3] = [
    [0.9554734214880752, -0.023098454948764523, 0.06325924320057066],
    [-0.028369709333863583, 1.0099953980813041, 0.021041441191917306],
    [0.012314014864481996, -0.02050764929889898, 1.330365926242124],
];
#[rustfmt::skip]
const XYZ_D65_TO_LMS: [[f64; 3]; 3] = [
    [0.819022437996703, 0.3619062600528904, -0.1288737815209879],
    [0.0329836539323885, 0.9292868615863434, 0.0361446663506424],
    [0.0481771893596242, 0.2642395317527308, 0.6335478284694309],
];
#[rustfmt::skip]
const LMS_TO_OKLAB: [[f64; 3]; 3] = [
    [0.210454268309314, 0.7936177747023054, -0.0040720430116193],
    [1.9779985324311684, -2.42859224204858, 0.450593709617411],
    [0.0259040424655478, 0.7827717124575296, -0.8086757549230774],
];
#[rustfmt::skip]
const OKLAB_TO_LMS_CBRT: [[f64; 3]; 3] = [
    [1.0, 0.3963377773761749, 0.2158037573099136],
    [1.0, -0.1055613458156586, -0.0638541728258133],
    [1.0, -0.0894841775298119, -1.2914855480194092],
];
#[rustfmt::skip]
const LMS_TO_XYZ_D65: [[f64; 3]; 3] = [
    [1.2268798758459243, -0.5578149944602171, 0.2813910456659647],
    [-0.0405757452148008, 1.112286803280317, -0.0717110580655164],
    [-0.0763729366746601, -0.4214933324022432, 1.5869240198367816],
];

const D50_WHITE: [f64; 3] = [0.3457 / 0.3585, 1.0, (1.0 - 0.3457 - 0.3585) / 0.3585];
const LAB_KAPPA: f64 = 24389.0 / 27.0;
const LAB_EPSILON: f64 = 216.0 / 24389.0;

fn hsl_to_srgb(v: [f64; 3]) -> [f64; 3] {
    let (h, s, l) = (v[0].rem_euclid(360.0), v[1] / 100.0, v[2] / 100.0);
    let a = s * l.min(1.0 - l);
    let f = |n: f64| {
        let k = (n + h / 30.0) % 12.0;
        l - a * (k - 3.0).min(9.0 - k).clamp(-1.0, 1.0)
    };
    [f(0.0), f(8.0), f(4.0)]
}

fn srgb_hue(r: f64, g: f64, b: f64, max: f64, d: f64) -> f64 {
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0).rem_euclid(360.0)
}

fn srgb_to_hsl_opt([r, g, b]: [f64; 3]) -> [Option<f64>; 3] {
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let (d, l) = (max - min, (max + min) * 0.5);
    if d <= 1e-6 || l.abs() <= 1e-6 || (1.0 - l).abs() <= 1e-6 {
        return [None, Some(0.0), Some(l.clamp(0.0, 1.0) * 100.0)];
    }
    let s = if l == 0.0 || l == 1.0 {
        0.0
    } else {
        (max - l) / l.min(1.0 - l)
    };
    let mut h = srgb_hue(r, g, b, max, d);
    let s_pct = if s < 0.0 {
        h = (h + 180.0).rem_euclid(360.0);
        -s * 100.0
    } else {
        s * 100.0
    };
    if s_pct <= HSL_SAT_EPS {
        [None, Some(0.0), Some(l * 100.0)]
    } else {
        [Some(h), Some(s_pct), Some(l * 100.0)]
    }
}

fn hwb_to_srgb(v: [f64; 3]) -> [f64; 3] {
    let (h, w, b) = (v[0].rem_euclid(360.0), v[1] / 100.0, v[2] / 100.0);
    if w + b >= 1.0 {
        return [w / (w + b); 3];
    }
    let factor = 1.0 - w - b;
    hsl_to_srgb([h, 100.0, 50.0]).map(|c| c * factor + w)
}

fn srgb_to_hwb_opt([r, g, b]: [f64; 3]) -> [Option<f64>; 3] {
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let (d, w, bl) = (max - min, min * 100.0, (1.0 - max) * 100.0);
    let h = (d > 1e-5 && w + bl < 100.0 - HWB_SUM_EPS).then(|| srgb_hue(r, g, b, max, d));
    [h, Some(w), Some(bl)]
}

fn lch_to_lab(v: [f64; 3]) -> [f64; 3] {
    let rad = v[2].to_radians();
    [v[0], v[1] * rad.cos(), v[1] * rad.sin()]
}

fn lab_to_lch_opt(v: [f64; 3], chroma_eps: f64) -> [Option<f64>; 3] {
    let c = v[1].hypot(v[2]);
    if c <= chroma_eps {
        [Some(v[0]), Some(0.0), None]
    } else {
        [
            Some(v[0]),
            Some(c),
            Some(v[2].atan2(v[1]).to_degrees().rem_euclid(360.0)),
        ]
    }
}

#[rustfmt::skip]
fn lab_to_xyz_d50(v: [f64; 3]) -> [f64; 3] {
    let f1 = (v[0] + 16.0) / 116.0;
    let inv = |f: f64| if f.powi(3) > LAB_EPSILON { f.powi(3) } else { (116.0 * f - 16.0) / LAB_KAPPA };
    let y = if v[0] > LAB_KAPPA * LAB_EPSILON { f1.powi(3) } else { v[0] / LAB_KAPPA };
    [inv(v[1] / 500.0 + f1) * D50_WHITE[0], y * D50_WHITE[1], inv(f1 - v[2] / 200.0) * D50_WHITE[2]]
}

#[rustfmt::skip]
fn xyz_d50_to_lab(xyz: [f64; 3]) -> [f64; 3] {
    let f = |t: f64| if t > LAB_EPSILON { t.cbrt() } else { (LAB_KAPPA * t + 16.0) / 116.0 };
    let [f0, f1, f2] = [f(xyz[0] / D50_WHITE[0]), f(xyz[1] / D50_WHITE[1]), f(xyz[2] / D50_WHITE[2])];
    [116.0 * f1 - 16.0, 500.0 * (f0 - f1), 200.0 * (f1 - f2)]
}

fn oklab_to_xyz_d65(v: [f64; 3]) -> [f64; 3] {
    mul3(
        &LMS_TO_XYZ_D65,
        mul3(&OKLAB_TO_LMS_CBRT, v).map(|x| x.powi(3)),
    )
}

fn xyz_d65_to_oklab(xyz: [f64; 3]) -> [f64; 3] {
    mul3(&LMS_TO_OKLAB, mul3(&XYZ_D65_TO_LMS, xyz).map(f64::cbrt))
}

#[rustfmt::skip]
fn to_xyz_d65(space: ColorSpace, v: [f64; 3]) -> [f64; 3] {
    match space {
        ColorSpace::Srgb | ColorSpace::SrgbLegacy => mul3(&LIN_SRGB_TO_XYZ_D65, v.map(transfer::srgb_to_lin)),
        ColorSpace::SrgbLinear => mul3(&LIN_SRGB_TO_XYZ_D65, v),
        ColorSpace::Hsl => to_xyz_d65(ColorSpace::Srgb, hsl_to_srgb(v)),
        ColorSpace::Hwb => to_xyz_d65(ColorSpace::Srgb, hwb_to_srgb(v)),
        ColorSpace::DisplayP3 => mul3(&LIN_P3_TO_XYZ_D65, v.map(transfer::srgb_to_lin)),
        ColorSpace::DisplayP3Linear => mul3(&LIN_P3_TO_XYZ_D65, v),
        ColorSpace::A98Rgb => mul3(&LIN_A98_TO_XYZ_D65, v.map(transfer::a98_to_lin)),
        ColorSpace::ProphotoRgb => mul3(&D50_TO_D65, mul3(&LIN_PROPHOTO_TO_XYZ_D50, v.map(transfer::prophoto_to_lin))),
        ColorSpace::Rec2020 => mul3(&LIN_REC2020_TO_XYZ_D65, v.map(transfer::rec2020_to_lin)),
        ColorSpace::XyzD65 => v,
        ColorSpace::XyzD50 => mul3(&D50_TO_D65, v),
        ColorSpace::Lab => mul3(&D50_TO_D65, lab_to_xyz_d50(v)),
        ColorSpace::Lch => mul3(&D50_TO_D65, lab_to_xyz_d50(lch_to_lab(v))),
        ColorSpace::Oklab => oklab_to_xyz_d65(v),
        ColorSpace::Oklch => oklab_to_xyz_d65(lch_to_lab(v)),
    }
}

#[rustfmt::skip]
fn from_xyz_d65_opt(space: ColorSpace, xyz65: [f64; 3]) -> [Option<f64>; 3] {
    match space {
        ColorSpace::Srgb | ColorSpace::SrgbLegacy => mul3(&XYZ_D65_TO_LIN_SRGB, xyz65).map(|x| Some(transfer::lin_to_srgb(x))),
        ColorSpace::SrgbLinear => mul3(&XYZ_D65_TO_LIN_SRGB, xyz65).map(Some),
        ColorSpace::Hsl => srgb_to_hsl_opt(mul3(&XYZ_D65_TO_LIN_SRGB, xyz65).map(transfer::lin_to_srgb)),
        ColorSpace::Hwb => srgb_to_hwb_opt(mul3(&XYZ_D65_TO_LIN_SRGB, xyz65).map(transfer::lin_to_srgb)),
        ColorSpace::DisplayP3 => mul3(&XYZ_D65_TO_LIN_P3, xyz65).map(|x| Some(transfer::lin_to_srgb(x))),
        ColorSpace::DisplayP3Linear => mul3(&XYZ_D65_TO_LIN_P3, xyz65).map(Some),
        ColorSpace::A98Rgb => mul3(&XYZ_D65_TO_LIN_A98, xyz65).map(|x| Some(transfer::lin_to_a98(x))),
        ColorSpace::ProphotoRgb => mul3(&XYZ_D50_TO_LIN_PROPHOTO, mul3(&D65_TO_D50, xyz65)).map(|x| Some(transfer::lin_to_prophoto(x))),
        ColorSpace::Rec2020 => mul3(&XYZ_D65_TO_LIN_REC2020, xyz65).map(|x| Some(transfer::lin_to_rec2020(x))),
        ColorSpace::XyzD65 => xyz65.map(Some),
        ColorSpace::XyzD50 => mul3(&D65_TO_D50, xyz65).map(Some),
        ColorSpace::Lab => xyz_d50_to_lab(mul3(&D65_TO_D50, xyz65)).map(Some),
        ColorSpace::Lch => lab_to_lch_opt(xyz_d50_to_lab(mul3(&D65_TO_D50, xyz65)), LCH_CHROMA_EPS),
        ColorSpace::Oklab => xyz_d65_to_oklab(xyz65).map(Some),
        ColorSpace::Oklch => lab_to_lch_opt(xyz_d65_to_oklab(xyz65), OKLCH_CHROMA_EPS),
    }
}

/// CSS Color 4 §13.2 binary-search OKLCH chroma reduction gamut mapping into sRGB `[0, 1]^3`.
#[rustfmt::skip]
fn gamut_map_to_srgb(color: ResolvedColor) -> [f64; 3] {
    let oklch = color.convert_to(ColorSpace::Oklch);
    let l = oklch.c[0].unwrap_or(0.0);
    if l >= 1.0 { return [1.0; 3]; }
    if l <= 0.0 { return [0.0; 3]; }
    let (c_orig, h) = (oklch.c[1].unwrap_or(0.0).max(0.0), oklch.c[2].unwrap_or(0.0));
    let to_srgb = |c: f64| mul3(&XYZ_D65_TO_LIN_SRGB, oklab_to_xyz_d65(lch_to_lab([l, c, h]))).map(transfer::lin_to_srgb);
    let direct = to_srgb(c_orig);
    if direct.iter().all(|v| (0.0..=1.0).contains(v)) { return direct; }
    let (mut lo, mut hi, mut best) = (0.0, c_orig, direct.map(|v| v.clamp(0.0, 1.0)));
    for _ in 0..16 {
        let mid = 0.5 * (lo + hi);
        let cand = to_srgb(mid);
        if cand.iter().all(|v| (0.0..=1.0).contains(v)) { lo = mid; best = cand; } else {
            let clipped = cand.map(|v| v.clamp(0.0, 1.0));
            let (a, b) = (lch_to_lab([l, mid, h]), xyz_d65_to_oklab(to_xyz_d65(ColorSpace::Srgb, clipped)));
            if ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt() < 0.02 { return clipped; }
            hi = mid;
        }
    }
    best
}

/// Evaluates CSS Color 4 §12 `color-mix()` on two resolved colors.
#[must_use]
pub fn mix_resolved_colors(
    space: ColorSpace,
    hue_method: HueMethod,
    c1: ResolvedColor,
    p1: Option<f64>,
    c2: ResolvedColor,
    p2: Option<f64>,
) -> ResolvedColor {
    let (mut w1, mut w2) = match (p1, p2) {
        (None, None) => (0.5, 0.5),
        (Some(a), None) => (a / 100.0, 1.0 - a / 100.0),
        (None, Some(b)) => (1.0 - b / 100.0, b / 100.0),
        (Some(a), Some(b)) => (a / 100.0, b / 100.0),
    };
    let sum = w1 + w2;
    let alpha_mult = sum.clamp(0.0, 1.0);
    if sum <= 0.0 {
        w1 = 0.5;
        w2 = 0.5;
    } else {
        w1 /= sum;
        w2 /= sum;
    }

    let (conv1, conv2) = (c1.convert_to(space), c2.convert_to(space));
    let (a1_eff, a2_eff, a_premult, out_alpha) = match (conv1.alpha, conv2.alpha) {
        (None, None) => (1.0, 1.0, 1.0, None),
        (Some(a), None) => (a, a, a, Some(a * alpha_mult)),
        (None, Some(b)) => (b, b, b, Some(b * alpha_mult)),
        (Some(a), Some(b)) => (a, b, a * w1 + b * w2, Some((a * w1 + b * w2) * alpha_mult)),
    };
    let hue_idx = space.hue_index();
    let mut out_c = [None; 3];
    for i in 0..3 {
        out_c[i] = match (conv1.c[i], conv2.c[i]) {
            (None, None) => None,
            (Some(v), None) | (None, Some(v)) => Some(if hue_idx == Some(i) {
                interpolate_hue(v, v, w1, w2, hue_method)
            } else {
                v
            }),
            (Some(v1), Some(v2)) => Some(if hue_idx == Some(i) {
                interpolate_hue(v1, v2, w1, w2, hue_method)
            } else if a_premult.abs() < 1e-12 {
                0.0
            } else {
                (v1 * a1_eff * w1 + v2 * a2_eff * w2) / a_premult
            }),
        };
    }
    finalize_mix_output(ResolvedColor {
        space,
        c: out_c,
        alpha: out_alpha,
    })
}

fn finalize_mix_output(mixed: ResolvedColor) -> ResolvedColor {
    if matches!(mixed.space, ColorSpace::Hsl | ColorSpace::Hwb)
        && mixed.c.iter().all(Option::is_some)
        && mixed.alpha.is_some()
    {
        mixed.convert_to(ColorSpace::Srgb)
    } else {
        mixed
    }
}

#[rustfmt::skip]
fn interpolate_hue(h1: f64, h2: f64, w1: f64, w2: f64, method: HueMethod) -> f64 {
    let (mut a, mut b) = (h1.rem_euclid(360.0), h2.rem_euclid(360.0));
    match method {
        HueMethod::Shorter => { if b - a > 180.0 { a += 360.0; } else if b - a < -180.0 { b += 360.0; } }
        HueMethod::Longer => { if 0.0 < b - a && b - a < 180.0 { a += 360.0; } else if -180.0 < b - a && b - a <= 0.0 { b += 360.0; } }
        HueMethod::Increasing => { if b < a { b += 360.0; } }
        HueMethod::Decreasing => { if a < b { a += 360.0; } }
    }
    (a * w1 + b * w2).rem_euclid(360.0)
}

// ---------------------------------------------------------------------------
// Channel Math Expression AST (CSS Values L4 subset for Color 4/5 channels)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MathDim {
    Number,
    Percent,
    Angle,
    Length,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChannelRandom {
    pub dashed_ident: Option<String>,
    pub element_scoped: bool,
    pub prop_scoped: bool,
    pub prop_index_scoped: bool,
    pub ua_ident: Option<String>,
    pub fixed: Option<Box<ChannelMath>>,
    pub min: Box<ChannelMath>,
    pub max: Box<ChannelMath>,
    pub step: Option<Box<ChannelMath>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChannelMath {
    Num(f64),
    Pct(f64),
    AngleDeg(f64),
    Length(f64, LengthUnit),
    Ident(String),
    Add(Box<ChannelMath>, Box<ChannelMath>),
    Sub(Box<ChannelMath>, Box<ChannelMath>),
    Mul(Box<ChannelMath>, Box<ChannelMath>),
    Div(Box<ChannelMath>, Box<ChannelMath>),
    Func(String, Vec<ChannelMath>),
    Random(Box<ChannelRandom>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChannelSpec {
    None,
    Keyword(String),
    Expr {
        expr: ChannelMath,
        specified_text: String,
        is_pct: bool,
        is_calc: bool,
    },
}

impl ChannelMath {
    fn dim(&self) -> Option<MathDim> {
        match self {
            Self::Num(_) | Self::Ident(_) => Some(MathDim::Number),
            Self::Pct(_) => Some(MathDim::Percent),
            Self::AngleDeg(_) => Some(MathDim::Angle),
            Self::Length(_, _) => Some(MathDim::Length),
            Self::Add(a, b) | Self::Sub(a, b) => {
                let da = a.dim()?;
                (da == b.dim()?).then_some(da)
            }
            Self::Mul(a, b) => match (a.dim()?, b.dim()?) {
                (MathDim::Number, x) | (x, MathDim::Number) => Some(x),
                _ => None,
            },
            Self::Div(a, b) => match (a.dim()?, b.dim()?) {
                (x, MathDim::Number) => Some(x),
                (x, y) if x == y => Some(MathDim::Number),
                _ => None,
            },
            Self::Func(name, args) => match name.as_str() {
                "sibling-index" | "sibling-count" => args.is_empty().then_some(MathDim::Number),
                "calc" | "abs" => (args.len() == 1).then(|| args[0].dim()).flatten(),
                "sign" => (args.len() == 1 && args[0].dim().is_some()).then_some(MathDim::Number),
                "sin" | "cos" | "tan" => (args.len() == 1
                    && matches!(args[0].dim()?, MathDim::Number | MathDim::Angle))
                .then_some(MathDim::Number),
                "exp" | "sqrt" | "pow" | "log" => args
                    .iter()
                    .all(|a| a.dim() == Some(MathDim::Number))
                    .then_some(MathDim::Number),
                "asin" | "acos" | "atan" => (args.len() == 1
                    && args[0].dim() == Some(MathDim::Number))
                .then_some(MathDim::Angle),
                "atan2" => {
                    (args.len() == 2 && args[0].dim()? == args[1].dim()?).then_some(MathDim::Angle)
                }
                "min" | "max" | "clamp" | "hypot" | "round" | "mod" | "rem" => {
                    let first = args.first()?.dim()?;
                    args.iter().all(|a| a.dim() == Some(first)).then_some(first)
                }
                _ => None,
            },
            Self::Random(r) => {
                if let Some(f) = &r.fixed {
                    if f.dim() != Some(MathDim::Number) {
                        return None;
                    }
                }
                let d = r.min.dim()?;
                (r.max.dim()? == d && r.step.as_ref().is_none_or(|s| s.dim() == Some(d)))
                    .then_some(d)
            }
        }
    }

    fn is_constant(&self) -> bool {
        !self.uses_dynamic_units()
    }

    fn serialize_const(&self) -> String {
        let v = self.eval(&[], &EvalContext::default(), 100.0);
        let num = if v.is_nan() {
            "NaN".to_string()
        } else {
            format_css_f64(v)
        };
        match self.dim() {
            Some(MathDim::Percent) => format!("{num}%"),
            Some(MathDim::Angle) => format!("{num}deg"),
            Some(MathDim::Length) => format!("{num}px"),
            _ => num,
        }
    }

    /// Serializes a math expression per CSS Values L4 §10.9.3.
    fn serialize_specified(&self) -> String {
        if self.is_constant() {
            return format!("calc({})", self.serialize_const());
        }
        match self {
            Self::Func(name, args) if name == "calc" && args.len() == 1 => {
                format!("calc({})", args[0].serialize_inner(false))
            }
            other => other.serialize_inner(false),
        }
    }

    fn sort_key(&self) -> u8 {
        match self {
            Self::Num(_) | Self::Pct(_) | Self::AngleDeg(_) | Self::Length(_, _) => 0,
            Self::Ident(_) => 1,
            _ => 2,
        }
    }

    fn serialize_inner(&self, wrap_parens: bool) -> String {
        if self.is_constant() && !matches!(self, Self::Ident(_)) {
            return self.serialize_const();
        }
        let wrap = |s: String| if wrap_parens { format!("({s})") } else { s };
        match self {
            Self::Num(n) => format_css_f64(*n),
            Self::Pct(p) => format!("{}%", format_css_f64(*p)),
            Self::AngleDeg(d) => format!("{}deg", format_css_f64(*d)),
            Self::Length(v, u) => format!("{}{}", format_css_f64(*v), length_unit_str(*u)),
            Self::Ident(s) => s.clone(),
            Self::Add(a, b) => {
                let mut terms = Vec::new();
                collect_add_terms(a, &mut terms);
                collect_add_terms(b, &mut terms);
                terms.sort_by_key(|t| t.sort_key());
                let s = terms
                    .iter()
                    .map(|t| t.serialize_inner(true))
                    .collect::<Vec<_>>()
                    .join(" + ");
                wrap(s)
            }
            Self::Sub(a, b) => {
                // Canonical CSS Values L4 §10.9.3: `calc(l - 20)` -> `calc(-20 + l)` when RHS is numeric literal.
                if let Some(neg) = match &**b {
                    Self::Num(n) => Some(Self::Num(-*n)),
                    Self::Pct(p) => Some(Self::Pct(-*p)),
                    Self::AngleDeg(d) => Some(Self::AngleDeg(-*d)),
                    _ => None,
                } {
                    return Self::Add(a.clone(), Box::new(neg)).serialize_inner(wrap_parens);
                }
                let lhs_wrap = matches!(**a, Self::Mul(_, _) | Self::Div(_, _));
                wrap(format!(
                    "{} - {}",
                    a.serialize_inner(lhs_wrap),
                    b.serialize_inner(true)
                ))
            }
            Self::Mul(a, b) => {
                let mut factors = Vec::new();
                collect_mul_factors(a, &mut factors);
                collect_mul_factors(b, &mut factors);
                factors.sort_by_key(|f| f.sort_key());
                let s = factors
                    .iter()
                    .map(|f| f.serialize_inner(true))
                    .collect::<Vec<_>>()
                    .join(" * ");
                wrap(s)
            }
            Self::Div(a, b) => {
                // CSS Values L4 §10.9.3: canonicalize `expr / const_num` into `(1 / const_num) * expr`.
                if b.is_constant() && b.dim() == Some(MathDim::Number) {
                    let denom = b.eval(&[], &EvalContext::default(), 100.0);
                    if denom.is_finite() && denom != 0.0 {
                        return Self::Mul(Box::new(Self::Num(1.0 / denom)), a.clone())
                            .serialize_inner(wrap_parens);
                    }
                }
                wrap(format!(
                    "{} / {}",
                    a.serialize_inner(true),
                    b.serialize_inner(true)
                ))
            }
            Self::Func(name, args) => {
                let inner = args
                    .iter()
                    .map(|a| a.serialize_inner(false))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{name}({inner})")
            }
            Self::Random(r) => {
                let mut parts = Vec::with_capacity(4);
                if let Some(f) = &r.fixed {
                    parts.push(format!("fixed {}", f.serialize_inner(false)));
                } else {
                    let mut opts = Vec::new();
                    if let Some(d) = &r.dashed_ident {
                        opts.push(d.clone());
                    }
                    if r.element_scoped {
                        opts.push("element-scoped".to_string());
                    }
                    if let Some(u) = &r.ua_ident {
                        opts.push(u.clone());
                    }
                    if !opts.is_empty() {
                        parts.push(opts.join(" "));
                    }
                }
                parts.push(r.min.serialize_inner(false));
                parts.push(r.max.serialize_inner(false));
                if let Some(st) = &r.step {
                    parts.push(st.serialize_inner(false));
                }
                format!("random({})", parts.join(", "))
            }
        }
    }

    fn eval(&self, vars: &[(&str, f64)], ctx: &EvalContext, pct_scale: f64) -> f64 {
        match self {
            Self::Num(n) => *n,
            Self::Pct(p) => *p * pct_scale / 100.0,
            Self::AngleDeg(d) => *d,
            Self::Length(v, u) => f64::from(u.resolve_px(*v as f32, ctx)),
            Self::Ident(name) => {
                if let Some(&(_, v)) = vars.iter().find(|&&(k, _)| k.eq_ignore_ascii_case(name)) {
                    return v;
                }
                match name.to_ascii_lowercase().as_str() {
                    "pi" => std::f64::consts::PI,
                    "e" => std::f64::consts::E,
                    "infinity" => f64::INFINITY,
                    "-infinity" => f64::NEG_INFINITY,
                    _ => f64::NAN,
                }
            }
            Self::Add(a, b) => a.eval(vars, ctx, pct_scale) + b.eval(vars, ctx, pct_scale),
            Self::Sub(a, b) => a.eval(vars, ctx, pct_scale) - b.eval(vars, ctx, pct_scale),
            Self::Mul(a, b) => {
                let ps = |e: &Self| {
                    if e.dim() == Some(MathDim::Percent) {
                        pct_scale
                    } else {
                        100.0
                    }
                };
                a.eval(vars, ctx, ps(a)) * b.eval(vars, ctx, ps(b))
            }
            Self::Div(a, b) => {
                a.eval(vars, ctx, pct_scale)
                    / b.eval(
                        vars,
                        ctx,
                        if a.dim() == b.dim() { pct_scale } else { 100.0 },
                    )
            }
            Self::Func(name, args) => eval_math_fn(name, args, vars, ctx, pct_scale),
            Self::Random(r) => {
                let rv = if let Some(f) = &r.fixed {
                    let v = f.eval(vars, ctx, 100.0);
                    if v.is_nan() {
                        f64::NAN
                    } else if v < 0.0 {
                        0.0
                    } else if v >= 1.0 {
                        f64::from_bits(1.0_f64.to_bits() - 1)
                    } else {
                        v
                    }
                } else {
                    let nid = if r.element_scoped {
                        ctx.random_node_id
                    } else {
                        0
                    };
                    crate::values_math::hash_random_f64(
                        ctx.random_doc_seed,
                        nid,
                        r.dashed_ident.as_deref(),
                        r.ua_ident.as_deref(),
                    )
                };
                let lo = r.min.eval(vars, ctx, pct_scale);
                let hi = r.max.eval(vars, ctx, pct_scale);
                let st = r.step.as_deref().map(|s| s.eval(vars, ctx, pct_scale));
                if rv.is_nan() || lo.is_nan() || hi.is_nan() || st.is_some_and(f64::is_nan) {
                    f64::NAN
                } else if hi <= lo {
                    lo
                } else if !lo.is_finite() || !hi.is_finite() {
                    f64::NAN
                } else if let Some(sv) = st.filter(|&s| s > 0.0) {
                    if sv.is_infinite() || sv > (hi - lo) {
                        lo
                    } else {
                        let n = ((hi - lo) / sv).floor() + 1.0;
                        let k = (rv * n).floor().min(n - 1.0);
                        lo + k * sv
                    }
                } else {
                    lo + rv * (hi - lo)
                }
            }
        }
    }

    fn uses_dynamic_units(&self) -> bool {
        match self {
            Self::Length(_, u) => *u != LengthUnit::Px,
            Self::Ident(s) => !matches!(
                s.to_ascii_lowercase().as_str(),
                "pi" | "e" | "infinity" | "-infinity" | "nan"
            ),
            Self::Add(a, b) | Self::Sub(a, b) | Self::Mul(a, b) | Self::Div(a, b) => {
                a.uses_dynamic_units() || b.uses_dynamic_units()
            }
            Self::Func(name, args) => {
                matches!(name.as_str(), "sibling-index" | "sibling-count")
                    || args.iter().any(Self::uses_dynamic_units)
            }
            Self::Random(_) => true,
            _ => false,
        }
    }

    fn contains_percent(&self) -> bool {
        match self {
            Self::Pct(_) => true,
            Self::Add(a, b) | Self::Sub(a, b) | Self::Mul(a, b) | Self::Div(a, b) => {
                a.contains_percent() || b.contains_percent()
            }
            Self::Func(_, args) => args.iter().any(Self::contains_percent),
            Self::Random(r) => {
                r.min.contains_percent()
                    || r.max.contains_percent()
                    || r.step.as_deref().is_some_and(Self::contains_percent)
            }
            _ => false,
        }
    }

    fn contains_channel_ident(&self) -> bool {
        match self {
            Self::Ident(s) => !matches!(
                s.to_ascii_lowercase().as_str(),
                "pi" | "e" | "infinity" | "-infinity" | "nan"
            ),
            Self::Add(a, b) | Self::Sub(a, b) | Self::Mul(a, b) | Self::Div(a, b) => {
                a.contains_channel_ident() || b.contains_channel_ident()
            }
            Self::Func(_, args) => args.iter().any(Self::contains_channel_ident),
            Self::Random(r) => {
                r.fixed.as_deref().is_some_and(Self::contains_channel_ident)
                    || r.min.contains_channel_ident()
                    || r.max.contains_channel_ident()
                    || r.step.as_deref().is_some_and(Self::contains_channel_ident)
            }
            _ => false,
        }
    }

    fn simplify_for_computed(&self, eval: &EvalContext) -> Self {
        if !self.contains_channel_ident() {
            let v = self.eval(&[], eval, 100.0);
            return match self.dim() {
                Some(MathDim::Percent) => Self::Pct(v),
                Some(MathDim::Angle) => Self::AngleDeg(v),
                Some(MathDim::Length) => Self::Length(v, LengthUnit::Px),
                _ => Self::Num(v),
            };
        }
        match self {
            Self::Add(a, b) => Self::Add(
                Box::new(a.simplify_for_computed(eval)),
                Box::new(b.simplify_for_computed(eval)),
            ),
            Self::Sub(a, b) => Self::Sub(
                Box::new(a.simplify_for_computed(eval)),
                Box::new(b.simplify_for_computed(eval)),
            ),
            Self::Mul(a, b) => Self::Mul(
                Box::new(a.simplify_for_computed(eval)),
                Box::new(b.simplify_for_computed(eval)),
            ),
            Self::Div(a, b) => Self::Div(
                Box::new(a.simplify_for_computed(eval)),
                Box::new(b.simplify_for_computed(eval)),
            ),
            Self::Func(name, args) => Self::Func(
                name.clone(),
                args.iter().map(|a| a.simplify_for_computed(eval)).collect(),
            ),
            other => other.clone(),
        }
    }

    fn bind_random_property(&mut self, prop_name: &str, counter: &mut u32) {
        match self {
            Self::Add(a, b) | Self::Sub(a, b) | Self::Mul(a, b) | Self::Div(a, b) => {
                a.bind_random_property(prop_name, counter);
                b.bind_random_property(prop_name, counter);
            }
            Self::Func(_, args) => {
                for a in args {
                    a.bind_random_property(prop_name, counter);
                }
            }
            Self::Random(r) => {
                if let Some(f) = &mut r.fixed {
                    f.bind_random_property(prop_name, counter);
                }
                r.min.bind_random_property(prop_name, counter);
                r.max.bind_random_property(prop_name, counter);
                if let Some(st) = &mut r.step {
                    st.bind_random_property(prop_name, counter);
                }
                if r.fixed.is_none() && r.ua_ident.is_none() {
                    if r.prop_index_scoped {
                        *counter += 1;
                        r.ua_ident = Some(format!("ua-{prop_name}-{counter}"));
                    } else if r.prop_scoped {
                        r.ua_ident = Some(format!("ua-{prop_name}"));
                    }
                }
            }
            _ => {}
        }
    }
}

#[rustfmt::skip]
fn eval_math_fn(name: &str, args: &[ChannelMath], vars: &[(&str, f64)], ctx: &EvalContext, ps: f64) -> f64 {
    let ea = |i: usize| args.get(i).map_or(f64::NAN, |e| e.eval(vars, ctx, ps));
    let trig = || args.first().map_or(f64::NAN, |e| { let v = e.eval(vars, ctx, 100.0); if e.dim() == Some(MathDim::Angle) { v.to_radians() } else { v } });
    match name {
        "calc" => ea(0), "abs" => ea(0).abs(), "sqrt" => ea(0).sqrt(), "exp" => ea(0).exp(),
        "sin" => trig().sin(), "cos" => trig().cos(), "tan" => trig().tan(),
        "asin" => ea(0).asin().to_degrees(), "acos" => ea(0).acos().to_degrees(), "atan" => ea(0).atan().to_degrees(),
        "atan2" => ea(0).atan2(ea(1)).to_degrees(), "pow" => ea(0).powf(ea(1)),
        "log" => if args.len() == 2 { ea(0).log(ea(1)) } else { ea(0).ln() },
        "hypot" => args.iter().map(|e| e.eval(vars, ctx, ps).powi(2)).sum::<f64>().sqrt(),
        "sign" => { let x = args.first().map_or(f64::NAN, |e| e.eval(vars, ctx, 1.0)); if x.is_nan() { f64::NAN } else if x > 0.0 { 1.0 } else if x < 0.0 { -1.0 } else { x } }
        "min" => args.iter().map(|e| e.eval(vars, ctx, ps)).fold(f64::INFINITY, |a, b| if a.is_nan() || b.is_nan() { f64::NAN } else { a.min(b) }),
        "max" => args.iter().map(|e| e.eval(vars, ctx, ps)).fold(f64::NEG_INFINITY, |a, b| if a.is_nan() || b.is_nan() { f64::NAN } else { a.max(b) }),
        "clamp" => { let (lo, v, hi) = (ea(0), ea(1), ea(2)); if lo.is_nan() || v.is_nan() || hi.is_nan() { f64::NAN } else { v.min(hi).max(lo) } }
        "mod" => { let (a, b) = (ea(0), ea(1)); if b == 0.0 || !a.is_finite() || b.is_nan() { f64::NAN } else if b.is_infinite() { if a.signum() == b.signum() { a } else { f64::NAN } } else { a - b * (a / b).floor() } }
        "rem" => { let (a, b) = (ea(0), ea(1)); if b == 0.0 || !a.is_finite() || b.is_nan() { f64::NAN } else { a % b } }
        "round" => { let (a, b) = (ea(0), ea(1)); if b == 0.0 { f64::NAN } else { (a / b).round() * b } }
        "sibling-index" => f64::from(ctx.sibling_index),
        "sibling-count" => f64::from(ctx.sibling_count),
        _ => f64::NAN,
    }
}

fn collect_add_terms<'a>(expr: &'a ChannelMath, out: &mut Vec<&'a ChannelMath>) {
    if let ChannelMath::Add(a, b) = expr {
        collect_add_terms(a, out);
        collect_add_terms(b, out);
    } else {
        out.push(expr);
    }
}

fn collect_mul_factors<'a>(expr: &'a ChannelMath, out: &mut Vec<&'a ChannelMath>) {
    if let ChannelMath::Mul(a, b) = expr {
        collect_mul_factors(a, out);
        collect_mul_factors(b, out);
    } else {
        out.push(expr);
    }
}

const fn length_unit_str(u: LengthUnit) -> &'static str {
    u.as_str()
}

// ---------------------------------------------------------------------------
// System Colors & CSS Color 4/5/6 Expression AST
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SystemColor {
    AccentColor,
    AccentColorText,
    ActiveText,
    ButtonBorder,
    ButtonFace,
    ButtonText,
    Canvas,
    CanvasText,
    Field,
    FieldText,
    GrayText,
    Highlight,
    HighlightText,
    LinkText,
    Mark,
    MarkText,
    SelectedItem,
    SelectedItemText,
    VisitedText,
}

#[rustfmt::skip]
const SYSTEM_COLORS: &[(&str, SystemColor)] = &[
    ("accentcolor", SystemColor::AccentColor), ("accentcolortext", SystemColor::AccentColorText),
    ("activeborder", SystemColor::ButtonBorder), ("activecaption", SystemColor::Canvas),
    ("activetext", SystemColor::ActiveText), ("appworkspace", SystemColor::Canvas),
    ("background", SystemColor::Canvas), ("buttonborder", SystemColor::ButtonBorder),
    ("buttonface", SystemColor::ButtonFace), ("buttonhighlight", SystemColor::ButtonFace),
    ("buttonshadow", SystemColor::ButtonFace), ("buttontext", SystemColor::ButtonText),
    ("canvas", SystemColor::Canvas), ("canvastext", SystemColor::CanvasText),
    ("captiontext", SystemColor::CanvasText), ("field", SystemColor::Field),
    ("fieldtext", SystemColor::FieldText), ("graytext", SystemColor::GrayText),
    ("highlight", SystemColor::Highlight), ("highlighttext", SystemColor::HighlightText),
    ("inactiveborder", SystemColor::ButtonBorder), ("inactivecaption", SystemColor::Canvas),
    ("inactivecaptiontext", SystemColor::GrayText), ("infobackground", SystemColor::Canvas),
    ("infotext", SystemColor::CanvasText), ("linktext", SystemColor::LinkText),
    ("mark", SystemColor::Mark), ("marktext", SystemColor::MarkText),
    ("menu", SystemColor::Canvas), ("menutext", SystemColor::CanvasText),
    ("scrollbar", SystemColor::Canvas), ("selecteditem", SystemColor::SelectedItem),
    ("selecteditemtext", SystemColor::SelectedItemText), ("threeddarkshadow", SystemColor::ButtonBorder),
    ("threedface", SystemColor::ButtonFace), ("threedhighlight", SystemColor::ButtonBorder),
    ("threedlightshadow", SystemColor::ButtonBorder), ("threedshadow", SystemColor::ButtonBorder),
    ("visitedtext", SystemColor::VisitedText), ("window", SystemColor::Canvas),
    ("windowframe", SystemColor::ButtonBorder), ("windowtext", SystemColor::CanvasText),
];

impl SystemColor {
    #[must_use]
    pub fn from_ident(ident: &str) -> Option<(Self, &'static str)> {
        let lower = ident.to_ascii_lowercase();
        SYSTEM_COLORS
            .binary_search_by_key(&lower.as_str(), |&(k, _)| k)
            .ok()
            .map(|i| {
                let (canon, sys) = SYSTEM_COLORS[i];
                (sys, canon)
            })
    }

    #[must_use]
    #[rustfmt::skip]
    pub const fn resolve(self, dark_mode: bool) -> ColorRgba {
        match (self, dark_mode) {
            (Self::AccentColor, false) => ColorRgba::rgb(0, 96, 223),
            (Self::AccentColor, true) => ColorRgba::rgb(96, 165, 250),
            (Self::AccentColorText | Self::HighlightText | Self::SelectedItemText, _) => ColorRgba::WHITE,
            (Self::ActiveText, false) => ColorRgba::rgb(255, 0, 0),
            (Self::ActiveText, true) => ColorRgba::rgb(255, 102, 102),
            (Self::ButtonBorder, false) => ColorRgba::rgb(129, 127, 132),
            (Self::ButtonBorder, true) => ColorRgba::rgb(133, 133, 133),
            (Self::ButtonFace, false) => ColorRgba::rgb(233, 233, 237),
            (Self::ButtonFace, true) => ColorRgba::rgb(107, 107, 107),
            (Self::ButtonText | Self::CanvasText | Self::FieldText, false) => ColorRgba::BLACK,
            (Self::ButtonText | Self::CanvasText | Self::FieldText, true) => ColorRgba::WHITE,
            (Self::Canvas, false) => ColorRgba::WHITE,
            (Self::Canvas, true) => ColorRgba::rgb(18, 18, 18),
            (Self::Field, false) => ColorRgba::WHITE,
            (Self::Field, true) => ColorRgba::rgb(59, 59, 59),
            (Self::GrayText, false) => ColorRgba::rgb(128, 128, 128),
            (Self::GrayText, true) => ColorRgba::rgb(166, 166, 166),
            (Self::Highlight | Self::SelectedItem, false) => ColorRgba::rgb(0, 120, 215),
            (Self::Highlight | Self::SelectedItem, true) => ColorRgba::rgb(51, 153, 255),
            (Self::LinkText, false) => ColorRgba::rgb(0, 0, 238),
            (Self::LinkText, true) => ColorRgba::rgb(158, 158, 255),
            (Self::Mark, false) => ColorRgba::rgb(255, 255, 0),
            (Self::Mark, true) => ColorRgba::rgb(255, 230, 102),
            (Self::MarkText, _) => ColorRgba::BLACK,
            (Self::VisitedText, false) => ColorRgba::rgb(85, 26, 139),
            (Self::VisitedText, true) => ColorRgba::rgb(208, 173, 240),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MixPct {
    pub expr: ChannelMath,
    pub specified_text: String,
    pub literal_pct: Option<f64>,
}

/// Full CSS Color 4/5/6 AST preserved across specified, computed, and inherited stages.
#[derive(Debug, Clone, PartialEq)]
pub enum CssColorExpr {
    CurrentColor,
    Named(&'static str, ColorRgba),
    System(SystemColor, &'static str),
    Resolved(ResolvedColor),
    Functional {
        fn_name: &'static str,
        space: ColorSpace,
        c: [ChannelSpec; 3],
        alpha: Option<ChannelSpec>,
    },
    Relative {
        fn_name: &'static str,
        space: ColorSpace,
        origin: Box<CssColorExpr>,
        c: [ChannelSpec; 3],
        alpha: Option<ChannelSpec>,
    },
    Alpha {
        origin: Box<CssColorExpr>,
        alpha: ChannelSpec,
    },
    ColorMix {
        space: ColorSpace,
        hue_method: HueMethod,
        items: Vec<(CssColorExpr, Option<MixPct>)>,
    },
    ColorLayers {
        blend_mode: &'static str,
        layers: Vec<CssColorExpr>,
    },
    LightDark(Box<CssColorExpr>, Box<CssColorExpr>),
    ContrastColor(Box<CssColorExpr>),
}

impl CssColorExpr {
    /// Binds CSS Values L5 `property-scoped` / `property-index-scoped` `random()` keys inside this color.
    pub fn bind_random_property(&mut self, prop_name: &str) {
        let mut counter = 0u32;
        self.bind_random_inner(prop_name, &mut counter);
    }

    fn bind_random_inner(&mut self, prop_name: &str, counter: &mut u32) {
        let bind_ch = |ch: &mut ChannelSpec, c: &mut u32| {
            if let ChannelSpec::Expr { expr, .. } = ch {
                expr.bind_random_property(prop_name, c);
            }
        };
        match self {
            Self::Functional { c, alpha, .. } => {
                for ch in c {
                    bind_ch(ch, counter);
                }
                if let Some(a) = alpha {
                    bind_ch(a, counter);
                }
            }
            Self::Relative {
                origin, c, alpha, ..
            } => {
                origin.bind_random_inner(prop_name, counter);
                for ch in c {
                    bind_ch(ch, counter);
                }
                if let Some(a) = alpha {
                    bind_ch(a, counter);
                }
            }
            Self::Alpha { origin, alpha } => {
                origin.bind_random_inner(prop_name, counter);
                bind_ch(alpha, counter);
            }
            Self::ColorMix { items, .. } => {
                for (col, pct) in items {
                    col.bind_random_inner(prop_name, counter);
                    if let Some(m) = pct {
                        m.expr.bind_random_property(prop_name, counter);
                    }
                }
            }
            Self::ColorLayers { layers, .. } => {
                for l in layers {
                    l.bind_random_inner(prop_name, counter);
                }
            }
            Self::LightDark(a, b) => {
                a.bind_random_inner(prop_name, counter);
                b.bind_random_inner(prop_name, counter);
            }
            Self::ContrastColor(inner) => inner.bind_random_inner(prop_name, counter),
            _ => {}
        }
    }

    /// Simplifies a color expression to its CSS Color 4/5 computed-value representation:
    /// resolves `light-dark()`, system colors, and tree-counting functions (`sibling-index()`)
    /// while preserving `currentcolor`-dependent relative/mixed expressions unresolved against `color`.
    #[must_use]
    pub fn simplify_for_computed(&self, dark_mode: bool, eval: &EvalContext) -> Self {
        match self {
            Self::LightDark(l, d) => {
                (if dark_mode { d } else { l }).simplify_for_computed(dark_mode, eval)
            }
            Self::System(sys, _) => {
                Self::Resolved(ResolvedColor::from_rgba(sys.resolve(dark_mode)))
            }
            Self::CurrentColor => Self::CurrentColor,
            Self::Named(_, rgba) => Self::Resolved(ResolvedColor::from_rgba(*rgba)),
            Self::Resolved(r) => Self::Resolved(*r),
            Self::Relative {
                fn_name,
                space,
                origin,
                c,
                alpha,
            } => {
                let simp_origin = origin.simplify_for_computed(dark_mode, eval);
                if !simp_origin.contains_current_color() {
                    return Self::Resolved(self.resolve(
                        ResolvedColor::BLACK_LEGACY,
                        dark_mode,
                        eval,
                    ));
                }
                let simp_ch = |ch: &ChannelSpec| match ch {
                    ChannelSpec::Expr {
                        expr,
                        is_pct,
                        is_calc,
                        ..
                    } => {
                        let se = expr.simplify_for_computed(eval);
                        let still_calc = *is_calc && se.contains_channel_ident();
                        let text = if still_calc {
                            se.serialize_specified()
                        } else {
                            se.serialize_const()
                        };
                        ChannelSpec::Expr {
                            expr: se,
                            specified_text: text,
                            is_pct: *is_pct,
                            is_calc: still_calc,
                        }
                    }
                    other => other.clone(),
                };
                Self::Relative {
                    fn_name,
                    space: *space,
                    origin: Box::new(simp_origin),
                    c: [simp_ch(&c[0]), simp_ch(&c[1]), simp_ch(&c[2])],
                    alpha: alpha.as_ref().map(simp_ch),
                }
            }
            Self::Alpha { origin, alpha } => {
                let simp_origin = origin.simplify_for_computed(dark_mode, eval);
                if !simp_origin.contains_current_color() {
                    return Self::Resolved(self.resolve(
                        ResolvedColor::BLACK_LEGACY,
                        dark_mode,
                        eval,
                    ));
                }
                Self::Alpha {
                    origin: Box::new(simp_origin),
                    alpha: alpha.clone(),
                }
            }
            Self::ColorMix {
                space,
                hue_method,
                items,
            } => {
                let simp_items: Vec<(Self, Option<MixPct>)> = items
                    .iter()
                    .map(|(c, p)| (c.simplify_for_computed(dark_mode, eval), p.clone()))
                    .collect();
                if !simp_items.iter().any(|(c, _)| c.contains_current_color()) {
                    return Self::Resolved(self.resolve(
                        ResolvedColor::BLACK_LEGACY,
                        dark_mode,
                        eval,
                    ));
                }
                Self::ColorMix {
                    space: *space,
                    hue_method: *hue_method,
                    items: simp_items,
                }
            }
            _ if !self.contains_current_color() => {
                Self::Resolved(self.resolve(ResolvedColor::BLACK_LEGACY, dark_mode, eval))
            }
            other => other.clone(),
        }
    }

    #[must_use]
    pub fn contains_current_color(&self) -> bool {
        match self {
            Self::CurrentColor => true,
            Self::Named(_, _)
            | Self::System(_, _)
            | Self::Resolved(_)
            | Self::Functional { .. } => false,
            Self::Relative { origin, .. }
            | Self::Alpha { origin, .. }
            | Self::ContrastColor(origin) => origin.contains_current_color(),
            Self::ColorMix { items, .. } => items.iter().any(|(c, _)| c.contains_current_color()),
            Self::ColorLayers { layers, .. } => layers.iter().any(Self::contains_current_color),
            Self::LightDark(a, b) => a.contains_current_color() || b.contains_current_color(),
        }
    }

    /// Serializes the specified CSSOM value (`el.style.color`, CSS Color 4/5 §15).
    #[must_use]
    pub fn to_specified_css_string(&self) -> String {
        match self {
            Self::CurrentColor => "currentcolor".to_string(),
            Self::Named(name, _) | Self::System(_, name) => (*name).to_string(),
            Self::Resolved(r) => r.to_computed_css_string(),
            Self::Functional {
                fn_name,
                space,
                c,
                alpha,
            } => fmt_functional_specified(fn_name, *space, c, alpha.as_ref()),
            Self::Relative {
                fn_name,
                space,
                origin,
                c,
                alpha,
            } => {
                let (os, s0, s1, s2) = (
                    origin.to_specified_css_string(),
                    serialize_channel_spec(&c[0]),
                    serialize_channel_spec(&c[1]),
                    serialize_channel_spec(&c[2]),
                );
                let head = if *fn_name == "color" {
                    format!("color(from {os} {} {s0} {s1} {s2}", space.css_name())
                } else {
                    format!("{fn_name}(from {os} {s0} {s1} {s2}")
                };
                alpha.as_ref().map_or_else(
                    || format!("{head})"),
                    |a| format!("{head} / {})", serialize_channel_spec(a)),
                )
            }
            Self::Alpha { origin, alpha } => format!(
                "alpha(from {} / {})",
                origin.to_specified_css_string(),
                serialize_channel_spec(alpha)
            ),
            Self::ColorMix {
                space,
                hue_method,
                items,
            } => fmt_color_mix_specified(*space, *hue_method, items),
            Self::ColorLayers { blend_mode, layers } => {
                let pfx = if *blend_mode == "normal" {
                    String::new()
                } else {
                    format!("{blend_mode}, ")
                };
                format!(
                    "color-layers({pfx}{})",
                    layers
                        .iter()
                        .map(Self::to_specified_css_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            Self::LightDark(l, d) => format!(
                "light-dark({}, {})",
                l.to_specified_css_string(),
                d.to_specified_css_string()
            ),
            Self::ContrastColor(inner) => {
                format!("contrast-color({})", inner.to_specified_css_string())
            }
        }
    }

    fn resolve_unquantized(
        &self,
        current_color: ResolvedColor,
        dark_mode: bool,
        eval: &EvalContext,
    ) -> ResolvedColor {
        match self {
            Self::Functional {
                space, c, alpha, ..
            } => eval_functional_channels(*space, c, alpha.as_ref(), &[], eval, false, true),
            Self::LightDark(l, d) => {
                (if dark_mode { d } else { l }).resolve_unquantized(current_color, dark_mode, eval)
            }
            _ => self.resolve(current_color, dark_mode, eval),
        }
    }

    /// Evaluates this expression into a concrete [`ResolvedColor`].
    #[must_use]
    pub fn resolve(
        &self,
        current_color: ResolvedColor,
        dark_mode: bool,
        eval: &EvalContext,
    ) -> ResolvedColor {
        match self {
            Self::CurrentColor => current_color,
            Self::Named(_, rgba) => ResolvedColor::from_rgba(*rgba),
            Self::System(sys, _) => ResolvedColor::from_rgba(sys.resolve(dark_mode)),
            Self::Resolved(r) => *r,
            Self::Functional {
                space, c, alpha, ..
            } => eval_functional_channels(*space, c, alpha.as_ref(), &[], eval, false, false),
            Self::Relative {
                space,
                origin,
                c,
                alpha,
                ..
            } => resolve_relative(
                *space,
                origin,
                c,
                alpha.as_ref(),
                current_color,
                dark_mode,
                eval,
            ),
            Self::Alpha { origin, alpha } => {
                let orig = origin.resolve_unquantized(current_color, dark_mode, eval);
                let new_a = eval_relative_alpha(
                    alpha,
                    &[("alpha", orig.alpha.unwrap_or(0.0))],
                    &[("alpha", orig.alpha)],
                    eval,
                );
                let space = if orig.space == ColorSpace::SrgbLegacy {
                    ColorSpace::Srgb
                } else {
                    orig.space
                };
                ResolvedColor {
                    space,
                    c: orig.c,
                    alpha: new_a,
                }
            }
            Self::ColorMix {
                space,
                hue_method,
                items,
                ..
            } => resolve_color_mix(*space, *hue_method, items, current_color, dark_mode, eval),
            Self::ColorLayers { layers, .. } => {
                resolve_color_layers(layers, current_color, dark_mode, eval)
            }
            Self::LightDark(l, d) => {
                (if dark_mode { d } else { l }).resolve(current_color, dark_mode, eval)
            }
            Self::ContrastColor(inner) => {
                let [r, g, b] = inner
                    .resolve_unquantized(current_color, dark_mode, eval)
                    .convert_to(ColorSpace::SrgbLinear)
                    .c
                    .map(|v| v.unwrap_or(0.0).clamp(0.0, 1.0));
                let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
                ResolvedColor::from_rgba(if 1.05 / (y + 0.05) >= (y + 0.05) / 0.05 {
                    ColorRgba::WHITE
                } else {
                    ColorRgba::BLACK
                })
            }
        }
    }
}

fn fmt_functional_specified(
    fn_name: &str,
    space: ColorSpace,
    c: &[ChannelSpec; 3],
    alpha: Option<&ChannelSpec>,
) -> String {
    let has_calc = c
        .iter()
        .chain(alpha)
        .any(|ch| matches!(ch, ChannelSpec::Expr { is_calc: true, .. }));
    let has_dyn = c
        .iter()
        .chain(alpha)
        .any(|ch| matches!(ch, ChannelSpec::Expr { expr, .. } if expr.uses_dynamic_units()));
    if !(if space.is_legacy() { has_dyn } else { has_calc }) {
        return eval_functional_channels(
            space,
            c,
            alpha,
            &[],
            &EvalContext::default(),
            false,
            false,
        )
        .to_computed_css_string();
    }
    let ctx = EvalContext::default();
    let fmt_ch = |i: usize, ch: &ChannelSpec| match ch {
        ChannelSpec::None => "none".to_string(),
        ChannelSpec::Keyword(k) => k.clone(),
        ChannelSpec::Expr {
            expr,
            is_calc: true,
            ..
        } => expr.serialize_specified(),
        ChannelSpec::Expr { expr, .. } => format_css_f64(sanitize_channel_value(
            space,
            i,
            expr.eval(&[], &ctx, channel_pct_scale(space, i)),
            false,
        )),
    };
    let head = if fn_name == "color" {
        format!(
            "color({} {} {} {}",
            space.css_name(),
            fmt_ch(0, &c[0]),
            fmt_ch(1, &c[1]),
            fmt_ch(2, &c[2])
        )
    } else {
        format!(
            "{fn_name}({} {} {}",
            fmt_ch(0, &c[0]),
            fmt_ch(1, &c[1]),
            fmt_ch(2, &c[2])
        )
    };
    match alpha {
        None => format!("{head})"),
        Some(ChannelSpec::None) => format!("{head} / none)"),
        Some(ChannelSpec::Keyword(k)) => format!("{head} / {k})"),
        Some(ChannelSpec::Expr {
            expr,
            is_calc: true,
            ..
        }) => format!("{head} / {})", expr.serialize_specified()),
        Some(ChannelSpec::Expr { expr, .. }) => {
            let a = expr.eval(&[], &ctx, 1.0).clamp(0.0, 1.0);
            if (a - 1.0).abs() < 1e-6 {
                format!("{head})")
            } else {
                format!("{head} / {})", format_css_f64(a))
            }
        }
    }
}

fn fmt_color_mix_specified(
    space: ColorSpace,
    hue_method: HueMethod,
    items: &[(CssColorExpr, Option<MixPct>)],
) -> String {
    let pfx = if space != ColorSpace::Oklab {
        format!("in {}{}, ", space.css_name(), hue_method.css_suffix())
    } else {
        String::new()
    };
    if items.len() == 2 {
        let (s1, s2) = (
            items[0].0.to_specified_css_string(),
            items[1].0.to_specified_css_string(),
        );
        let (ps1, ps2) =
            normalize_color_mix_specified_pcts(items[0].1.as_ref(), items[1].1.as_ref());
        let p1 = ps1.map_or(s1.clone(), |p| format!("{s1} {p}"));
        let p2 = ps2.map_or(s2.clone(), |p| format!("{s2} {p}"));
        return format!("color-mix({pfx}{p1}, {p2})");
    }
    let single = items.len() == 1;
    // CSS Color 5 §2.1: for 3+ color-mix items where some have literal percentages and some omit
    // percentages, normalize omitted entries to `max(0%, (100% - sum_specified) / count_omitted)`.
    let multi_omitted_fill: Option<f64> = if items.len() >= 3
        && items.iter().any(|(_, p)| p.is_none())
        && items.iter().any(|(_, p)| p.is_some())
        && items
            .iter()
            .filter_map(|(_, p)| p.as_ref())
            .all(|m| m.literal_pct.is_some())
    {
        let sum_spec: f64 = items
            .iter()
            .filter_map(|(_, p)| p.as_ref().and_then(|m| m.literal_pct))
            .sum();
        let unspec = items.iter().filter(|(_, p)| p.is_none()).count();
        Some((100.0 - sum_spec).max(0.0) / unspec as f64)
    } else {
        None
    };
    let joined = items
        .iter()
        .map(|(c, p)| {
            let cs = c.to_specified_css_string();
            match p {
                Some(m) if single && m.literal_pct.is_some_and(|v| (v - 100.0).abs() < 1e-6) => cs,
                Some(m) => {
                    let ps = m.literal_pct.map_or_else(
                        || m.specified_text.clone(),
                        |v| format!("{}%", format_css_f64(v)),
                    );
                    format!("{cs} {ps}")
                }
                None => match multi_omitted_fill {
                    Some(fill) => format!("{cs} {}%", format_css_f64(fill)),
                    None => cs,
                },
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("color-mix({pfx}{joined})")
}

fn resolve_relative(
    space: ColorSpace,
    origin: &CssColorExpr,
    c: &[ChannelSpec; 3],
    alpha: Option<&ChannelSpec>,
    current_color: ResolvedColor,
    dark_mode: bool,
    eval: &EvalContext,
) -> ResolvedColor {
    let target_conv = origin
        .resolve_unquantized(current_color, dark_mode, eval)
        .convert_to(space);
    let (k0, k1, k2) = space_channel_names(space);
    let scale = if space == ColorSpace::SrgbLegacy {
        255.0
    } else {
        1.0
    };
    let ch = target_conv.c.map(|o| o.map(|v| v * scale));
    let oa = target_conv.alpha;
    let vars = [
        (k0, ch[0].unwrap_or(0.0)),
        (k1, ch[1].unwrap_or(0.0)),
        (k2, ch[2].unwrap_or(0.0)),
        ("alpha", oa.unwrap_or(0.0)),
    ];
    let opt = [(k0, ch[0]), (k1, ch[1]), (k2, ch[2]), ("alpha", oa)];
    let res_c = [
        eval_relative_channel(space, 0, &c[0], &vars, &opt, eval),
        eval_relative_channel(space, 1, &c[1], &vars, &opt, eval),
        eval_relative_channel(space, 2, &c[2], &vars, &opt, eval),
    ];
    let res_a = alpha.map_or(oa, |a| eval_relative_alpha(a, &vars, &opt, eval));
    finalize_computed_color(space, res_c, res_a, true)
}

fn resolve_color_mix(
    space: ColorSpace,
    hue_method: HueMethod,
    items: &[(CssColorExpr, Option<MixPct>)],
    current_color: ResolvedColor,
    dark_mode: bool,
    eval: &EvalContext,
) -> ResolvedColor {
    if items.is_empty() {
        return ResolvedColor::TRANSPARENT_LEGACY;
    }
    if items.len() == 1 {
        let mut conv = items[0]
            .0
            .resolve_unquantized(current_color, dark_mode, eval)
            .convert_to(space);
        if let Some(m) = &items[0].1 {
            let w = m.expr.eval(&[], eval, 100.0).clamp(0.0, 100.0) / 100.0;
            if w <= 0.0 {
                return ResolvedColor::TRANSPARENT_LEGACY;
            }
            conv.alpha = conv.alpha.map(|a| a * w.min(1.0));
        }
        return finalize_mix_output(conv);
    }
    if items.len() == 2 {
        let r1 = items[0]
            .0
            .resolve_unquantized(current_color, dark_mode, eval);
        let r2 = items[1]
            .0
            .resolve_unquantized(current_color, dark_mode, eval);
        let e1 = items[0]
            .1
            .as_ref()
            .map(|m| m.expr.eval(&[], eval, 100.0).clamp(0.0, 100.0));
        let e2 = items[1]
            .1
            .as_ref()
            .map(|m| m.expr.eval(&[], eval, 100.0).clamp(0.0, 100.0));
        return mix_resolved_colors(space, hue_method, r1, e1, r2, e2);
    }
    let res: Vec<(ResolvedColor, Option<f64>)> = items
        .iter()
        .map(|(c, p)| {
            (
                c.resolve_unquantized(current_color, dark_mode, eval)
                    .convert_to(space),
                p.as_ref()
                    .map(|m| m.expr.eval(&[], eval, 100.0).clamp(0.0, 100.0)),
            )
        })
        .collect();
    let spec_sum: f64 = res.iter().filter_map(|(_, p)| *p).sum();
    let unspec = res.iter().filter(|(_, p)| p.is_none()).count();
    let fill = if unspec > 0 {
        (100.0 - spec_sum).max(0.0) / unspec as f64
    } else {
        0.0
    };
    let weights: Vec<f64> = res.iter().map(|(_, p)| p.unwrap_or(fill) / 100.0).collect();
    let sum: f64 = weights.iter().sum();
    if sum <= 0.0 {
        return ResolvedColor::TRANSPARENT_LEGACY;
    }
    let norm_w: Vec<f64> = weights.into_iter().map(|w| w / sum).collect();
    let a_vals: Vec<f64> = res.iter().map(|(c, _)| c.alpha.unwrap_or(1.0)).collect();
    let a_premult: f64 = a_vals.iter().zip(&norm_w).map(|(a, w)| a * w).sum();
    let out_alpha =
        (!res.iter().all(|(c, _)| c.alpha.is_none())).then_some(a_premult * sum.min(1.0));
    let mut out_c = [None; 3];
    for i in 0..3 {
        let pres: Vec<(f64, f64, f64)> = res
            .iter()
            .enumerate()
            .filter_map(|(k, (c, _))| c.c[i].map(|v| (v, a_vals[k], norm_w[k])))
            .collect();
        if !pres.is_empty() {
            let ws: f64 = pres.iter().map(|(_, _, w)| w).sum();
            let aws: f64 = pres.iter().map(|(_, a, w)| a * w).sum();
            out_c[i] = Some(if aws.abs() < 1e-12 {
                pres.iter().map(|(v, _, w)| v * w).sum::<f64>() / ws.max(1e-12)
            } else {
                pres.iter().map(|(v, a, w)| v * a * w).sum::<f64>() / aws
            });
        }
    }
    finalize_mix_output(ResolvedColor {
        space,
        c: out_c,
        alpha: out_alpha,
    })
}

fn resolve_color_layers(
    layers: &[CssColorExpr],
    current_color: ResolvedColor,
    dark_mode: bool,
    eval: &EvalContext,
) -> ResolvedColor {
    let mut iter = layers.iter().rev();
    let Some(bottom) = iter.next() else {
        return ResolvedColor::TRANSPARENT_LEGACY;
    };
    let mut acc = bottom.resolve(current_color, dark_mode, eval);
    for top in iter {
        let s = top
            .resolve_unquantized(current_color, dark_mode, eval)
            .convert_to(ColorSpace::Srgb);
        let d = acc.convert_to(ColorSpace::Srgb);
        let (sa, da) = (
            s.alpha.unwrap_or(1.0).clamp(0.0, 1.0),
            d.alpha.unwrap_or(1.0).clamp(0.0, 1.0),
        );
        let oa = sa + da * (1.0 - sa);
        let mut oc = [Some(0.0); 3];
        if oa > 1e-12 {
            for i in 0..3 {
                oc[i] = Some(
                    (s.c[i].unwrap_or(0.0) * sa + d.c[i].unwrap_or(0.0) * da * (1.0 - sa)) / oa,
                );
            }
        }
        acc = ResolvedColor {
            space: ColorSpace::Srgb,
            c: oc,
            alpha: Some(oa),
        };
    }
    acc
}

fn serialize_channel_spec(spec: &ChannelSpec) -> String {
    match spec {
        ChannelSpec::None => "none".to_string(),
        ChannelSpec::Keyword(k) => k.clone(),
        ChannelSpec::Expr { specified_text, .. } => specified_text.clone(),
    }
}

#[rustfmt::skip]
fn normalize_color_mix_specified_pcts(p1: Option<&MixPct>, p2: Option<&MixPct>) -> (Option<String>, Option<String>) {
    let is_50 = |m: &MixPct| m.literal_pct.is_some_and(|v| (v - 50.0).abs() < 1e-6);
    let fmt = |m: &MixPct| m.literal_pct.map_or_else(|| m.specified_text.clone(), |v| format!("{}%", format_css_f64(v)));
    match (p1, p2) {
        (None, None) => (None, None),
        (Some(a), None) => if is_50(a) { (None, None) } else if let Some(v) = a.literal_pct { (Some(format!("{}%", format_css_f64(v))), Some(format!("{}%", format_css_f64(100.0 - v)))) } else { (Some(a.specified_text.clone()), None) },
        (None, Some(b)) => if is_50(b) { (None, None) } else if let Some(v) = b.literal_pct { (Some(format!("{}%", format_css_f64(100.0 - v))), Some(format!("{}%", format_css_f64(v)))) } else { (None, Some(b.specified_text.clone())) },
        (Some(a), Some(b)) => if is_50(a) && is_50(b) { (None, None) } else { (Some(fmt(a)), Some(fmt(b))) },
    }
}

#[rustfmt::skip]
const fn space_channel_names(space: ColorSpace) -> (&'static str, &'static str, &'static str) {
    match space {
        ColorSpace::SrgbLegacy | ColorSpace::Srgb | ColorSpace::SrgbLinear | ColorSpace::DisplayP3
        | ColorSpace::DisplayP3Linear | ColorSpace::A98Rgb | ColorSpace::ProphotoRgb
        | ColorSpace::Rec2020 => ("r", "g", "b"),
        ColorSpace::XyzD50 | ColorSpace::XyzD65 => ("x", "y", "z"),
        ColorSpace::Lab | ColorSpace::Oklab => ("l", "a", "b"),
        ColorSpace::Lch | ColorSpace::Oklch => ("l", "c", "h"),
        ColorSpace::Hsl => ("h", "s", "l"),
        ColorSpace::Hwb => ("h", "w", "b"),
    }
}

const fn channel_pct_scale(space: ColorSpace, idx: usize) -> f64 {
    match (space, idx) {
        (ColorSpace::SrgbLegacy, _) => 255.0,
        (ColorSpace::Lab | ColorSpace::Lch, 0) | (ColorSpace::Hsl | ColorSpace::Hwb, 1 | 2) => {
            100.0
        }
        (ColorSpace::Lab, 1 | 2) => 125.0,
        (ColorSpace::Lch, 1) => 150.0,
        (ColorSpace::Oklab, 1 | 2) | (ColorSpace::Oklch, 1) => 0.4,
        _ => 1.0,
    }
}

#[rustfmt::skip]
fn sanitize_channel_value(space: ColorSpace, idx: usize, v: f64, is_relative: bool) -> f64 {
    if v.is_nan() { return 0.0; }
    match (space, idx) {
        (ColorSpace::SrgbLegacy, _) if is_relative => if v.is_infinite() { if v.is_sign_positive() { 255.0 } else { 0.0 } } else { v },
        (ColorSpace::SrgbLegacy, _) => v.clamp(0.0, 255.0),
        (ColorSpace::Lab | ColorSpace::Lch, 0) => v.clamp(0.0, 100.0),
        (ColorSpace::Oklab | ColorSpace::Oklch, 0) => v.clamp(0.0, 1.0),
        (ColorSpace::Lch | ColorSpace::Oklch | ColorSpace::Hsl, 1) => v.max(0.0),
        (_, i) if space.hue_index() == Some(i) => if v.is_infinite() { 0.0 } else { v.rem_euclid(360.0) },
        _ => v,
    }
}

fn eval_relative_channel(
    space: ColorSpace,
    idx: usize,
    spec: &ChannelSpec,
    vars: &[(&str, f64)],
    opt: &[(&str, Option<f64>)],
    ctx: &EvalContext,
) -> Option<f64> {
    match spec {
        ChannelSpec::None => None,
        ChannelSpec::Keyword(k) => opt
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map_or(Some(0.0), |(_, ov)| {
                ov.map(|v| sanitize_channel_value(space, idx, v, true))
            }),
        ChannelSpec::Expr { expr, .. } => Some(sanitize_channel_value(
            space,
            idx,
            expr.eval(vars, ctx, channel_pct_scale(space, idx)),
            true,
        )),
    }
}

fn eval_relative_alpha(
    spec: &ChannelSpec,
    vars: &[(&str, f64)],
    opt: &[(&str, Option<f64>)],
    ctx: &EvalContext,
) -> Option<f64> {
    match spec {
        ChannelSpec::None => None,
        ChannelSpec::Keyword(k) => opt
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map_or(Some(1.0), |(_, ov)| ov.map(|v| v.clamp(0.0, 1.0))),
        ChannelSpec::Expr { expr, .. } => {
            let v = expr.eval(vars, ctx, 1.0);
            Some(if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) })
        }
    }
}

#[rustfmt::skip]
fn eval_functional_channels(
    space: ColorSpace, c: &[ChannelSpec; 3], alpha: Option<&ChannelSpec>, vars: &[(&str, f64)], ctx: &EvalContext, is_relative: bool, keep_unquantized: bool,
) -> ResolvedColor {
    let mut out = [None; 3];
    for i in 0..3 {
        out[i] = match &c[i] {
            ChannelSpec::None => None,
            ChannelSpec::Keyword(_) => Some(0.0),
            ChannelSpec::Expr { expr, .. } => Some(sanitize_channel_value(space, i, expr.eval(vars, ctx, channel_pct_scale(space, i)), is_relative)),
        };
    }
    let out_a = match alpha {
        None | Some(ChannelSpec::Keyword(_)) => Some(1.0),
        Some(ChannelSpec::None) => None,
        Some(ChannelSpec::Expr { expr, .. }) => { let v = expr.eval(vars, ctx, 1.0); Some(if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }) }
    };
    if keep_unquantized {
        return match space {
            ColorSpace::SrgbLegacy => ResolvedColor { space: ColorSpace::Srgb, c: out.map(|v| v.map(|x| x / 255.0)), alpha: out_a },
            _ => ResolvedColor { space, c: out, alpha: out_a },
        };
    }
    finalize_computed_color(space, out, out_a, is_relative)
}

#[rustfmt::skip]
fn finalize_computed_color(space: ColorSpace, c: [Option<f64>; 3], alpha: Option<f64>, is_relative: bool) -> ResolvedColor {
    let has_none = c.iter().any(Option::is_none) || alpha.is_none();
    match space {
        ColorSpace::SrgbLegacy if has_none || is_relative => ResolvedColor { space: ColorSpace::Srgb, c: c.map(|o| o.map(|v| v / 255.0)), alpha },
        ColorSpace::SrgbLegacy => ResolvedColor::from_legacy_f64(c[0].unwrap_or(0.0), c[1].unwrap_or(0.0), c[2].unwrap_or(0.0), alpha.unwrap_or(1.0)),
        ColorSpace::Hsl | ColorSpace::Hwb if has_none => ResolvedColor { space, c, alpha },
        ColorSpace::Hsl | ColorSpace::Hwb => {
            let raw = c.map(|x| x.unwrap_or(0.0));
            let rgb = if space == ColorSpace::Hsl { hsl_to_srgb(raw) } else { hwb_to_srgb(raw) };
            if is_relative { ResolvedColor { space: ColorSpace::Srgb, c: rgb.map(Some), alpha } }
            else { ResolvedColor::from_legacy_f64(rgb[0] * 255.0, rgb[1] * 255.0, rgb[2] * 255.0, alpha.unwrap_or(1.0)) }
        }
        _ => ResolvedColor { space, c, alpha },
    }
}

// ---------------------------------------------------------------------------
// Public Parser Entry Points (cssparser-backed, CSS Color 4/5/6 compliant)
// ---------------------------------------------------------------------------

#[must_use]
pub fn parse_css_color(input: &str) -> Option<CssColorExpr> {
    let mut pi = ParserInput::new(input);
    let mut p = Parser::new(&mut pi);
    let expr = parse_color_from_parser(&mut p)?;
    p.is_exhausted().then_some(expr)
}

#[must_use]
pub fn parse_and_serialize_color_specified(input: &str) -> Option<String> {
    parse_css_color(input).map(|c| c.to_specified_css_string())
}

#[must_use]
pub fn parse_and_serialize_opacity_specified(input: &str) -> Option<String> {
    let mut pi = ParserInput::new(input);
    let mut p = Parser::new(&mut pi);
    let spec = parse_channel_component(&mut p, ChannelRule::NumOrPct, &[], false)?;
    if !p.is_exhausted() {
        return None;
    }
    match spec {
        ChannelSpec::Expr {
            expr,
            is_calc: true,
            ..
        } => Some(expr.serialize_specified()),
        ChannelSpec::Expr { expr, .. } => {
            Some(format_css_f64(expr.eval(&[], &EvalContext::default(), 1.0)))
        }
        _ => None,
    }
}

fn parse_color_from_parser<'i, 't>(p: &mut Parser<'i, 't>) -> Option<CssColorExpr> {
    match p.next().ok()?.clone() {
        Token::Ident(ident) => {
            let lower = ident.to_ascii_lowercase();
            if lower == "currentcolor" {
                return Some(CssColorExpr::CurrentColor);
            }
            if lower == "transparent" {
                return Some(CssColorExpr::Named("transparent", ColorRgba::TRANSPARENT));
            }
            if let Some((canon, rgb)) = lookup_named_color(&lower) {
                return Some(CssColorExpr::Named(
                    canon,
                    ColorRgba::rgb(rgb[0], rgb[1], rgb[2]),
                ));
            }
            SystemColor::from_ident(&lower).map(|(sys, canon)| CssColorExpr::System(sys, canon))
        }
        Token::IDHash(h) | Token::Hash(h) => parse_hex_color(h.as_ref())
            .map(|rgba| CssColorExpr::Resolved(ResolvedColor::from_rgba(rgba))),
        Token::Function(name) => {
            let lower = name.to_ascii_lowercase();
            p.parse_nested_block(|nested| {
                parse_color_function(&lower, nested)
                    .ok_or_else(|| nested.new_custom_error::<_, ()>(()))
            })
            .ok()
        }
        _ => None,
    }
}

#[rustfmt::skip]
fn parse_hex_color(h: &str) -> Option<ColorRgba> {
    let b = h.as_bytes();
    let nib = |i: usize| match b[i] { b'0'..=b'9' => Some(b[i] - b'0'), b'a'..=b'f' => Some(b[i] - b'a' + 10), b'A'..=b'F' => Some(b[i] - b'A' + 10), _ => None };
    match b.len() {
        3 => Some(ColorRgba::rgb(nib(0)? * 17, nib(1)? * 17, nib(2)? * 17)),
        4 => Some(ColorRgba::rgba(nib(0)? * 17, nib(1)? * 17, nib(2)? * 17, nib(3)? * 17)),
        6 => Some(ColorRgba::rgb((nib(0)? << 4) | nib(1)?, (nib(2)? << 4) | nib(3)?, (nib(4)? << 4) | nib(5)?)),
        8 => Some(ColorRgba::rgba((nib(0)? << 4) | nib(1)?, (nib(2)? << 4) | nib(3)?, (nib(4)? << 4) | nib(5)?, (nib(6)? << 4) | nib(7)?)),
        _ => None,
    }
}

#[rustfmt::skip]
fn parse_color_function<'i, 't>(name: &str, p: &mut Parser<'i, 't>) -> Option<CssColorExpr> {
    use ChannelRule::{NumOrAngle, NumOrPct};
    match name {
        "rgb" | "rgba" => parse_legacy_fn("rgb", ColorSpace::SrgbLegacy, [NumOrPct; 3], true, p),
        "hsl" | "hsla" => parse_legacy_fn("hsl", ColorSpace::Hsl, [NumOrAngle, NumOrPct, NumOrPct], false, p),
        "hwb" => parse_modern_fn("hwb", ColorSpace::Hwb, [NumOrAngle, NumOrPct, NumOrPct], p),
        "lab" => parse_modern_fn("lab", ColorSpace::Lab, [NumOrPct; 3], p),
        "lch" => parse_modern_fn("lch", ColorSpace::Lch, [NumOrPct, NumOrPct, NumOrAngle], p),
        "oklab" => parse_modern_fn("oklab", ColorSpace::Oklab, [NumOrPct; 3], p),
        "oklch" => parse_modern_fn("oklch", ColorSpace::Oklch, [NumOrPct, NumOrPct, NumOrAngle], p),
        "color" => parse_color_space_fn(p),
        "color-mix" => parse_color_mix_fn(p),
        "color-layers" => parse_color_layers_fn(p),
        "alpha" => parse_alpha_fn(p),
        "light-dark" => parse_light_dark_fn(p),
        "contrast-color" => { let inner = parse_color_from_parser(p)?; p.is_exhausted().then_some(CssColorExpr::ContrastColor(Box::new(inner))) }
        _ => None,
    }
}

fn try_consume_from<'i, 't>(p: &mut Parser<'i, 't>) -> Option<CssColorExpr> {
    let state = p.state();
    if p.try_parse(|p| p.expect_ident_matching("from")).is_ok() {
        return parse_color_from_parser(p);
    }
    p.reset(&state);
    None
}

fn parse_optional_slash_alpha<'i, 't>(
    p: &mut Parser<'i, 't>,
    kws: &[&str],
) -> Option<Option<ChannelSpec>> {
    if p.is_exhausted() {
        return Some(None);
    }
    p.expect_delim('/').ok()?;
    let a = parse_channel_component(p, ChannelRule::NumOrPct, kws, true)?;
    p.is_exhausted().then_some(Some(a))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChannelRule {
    NumOrPct,
    NumOrAngle,
    PctOnly,
}

fn parse_channels_and_alpha<'i, 't>(
    p: &mut Parser<'i, 't>,
    rules: [ChannelRule; 3],
    kws: &[&str],
) -> Option<([ChannelSpec; 3], Option<ChannelSpec>)> {
    let c = [
        parse_channel_component(p, rules[0], kws, true)?,
        parse_channel_component(p, rules[1], kws, true)?,
        parse_channel_component(p, rules[2], kws, true)?,
    ];
    Some((c, parse_optional_slash_alpha(p, kws)?))
}

fn parse_relative_tail<'i, 't>(
    fn_name: &'static str,
    space: ColorSpace,
    origin: CssColorExpr,
    rules: [ChannelRule; 3],
    p: &mut Parser<'i, 't>,
) -> Option<CssColorExpr> {
    let (k0, k1, k2) = space_channel_names(space);
    let (c, alpha) = parse_channels_and_alpha(p, rules, &[k0, k1, k2, "alpha"])?;
    Some(CssColorExpr::Relative {
        fn_name,
        space,
        origin: Box::new(origin),
        c,
        alpha,
    })
}

fn parse_legacy_fn<'i, 't>(
    fn_name: &'static str,
    space: ColorSpace,
    rules: [ChannelRule; 3],
    is_rgb: bool,
    p: &mut Parser<'i, 't>,
) -> Option<CssColorExpr> {
    if let Some(origin) = try_consume_from(p) {
        return parse_relative_tail(fn_name, space, origin, rules, p);
    }
    let c0 = parse_channel_component(p, rules[0], &[], true)?;
    if p.try_parse(Parser::expect_comma).is_ok() {
        if matches!(c0, ChannelSpec::None) {
            return None;
        }
        let r12 = if is_rgb {
            ChannelRule::NumOrPct
        } else {
            ChannelRule::PctOnly
        };
        let c1 = parse_channel_component(p, r12, &[], false)?;
        p.expect_comma().ok()?;
        let c2 = parse_channel_component(p, r12, &[], false)?;
        if is_rgb
            && (is_channel_pct(&c1) != is_channel_pct(&c0)
                || is_channel_pct(&c2) != is_channel_pct(&c0))
        {
            return None;
        }
        let alpha = if p.try_parse(Parser::expect_comma).is_ok() {
            Some(parse_channel_component(
                p,
                ChannelRule::NumOrPct,
                &[],
                false,
            )?)
        } else {
            None
        };
        return p.is_exhausted().then_some(CssColorExpr::Functional {
            fn_name,
            space,
            c: [c0, c1, c2],
            alpha,
        });
    }
    let c1 = parse_channel_component(p, rules[1], &[], true)?;
    let c2 = parse_channel_component(p, rules[2], &[], true)?;
    let alpha = parse_optional_slash_alpha(p, &[])?;
    Some(CssColorExpr::Functional {
        fn_name,
        space,
        c: [c0, c1, c2],
        alpha,
    })
}

fn parse_modern_fn<'i, 't>(
    fn_name: &'static str,
    space: ColorSpace,
    rules: [ChannelRule; 3],
    p: &mut Parser<'i, 't>,
) -> Option<CssColorExpr> {
    if let Some(origin) = try_consume_from(p) {
        return parse_relative_tail(fn_name, space, origin, rules, p);
    }
    let (c, alpha) = parse_channels_and_alpha(p, rules, &[])?;
    Some(CssColorExpr::Functional {
        fn_name,
        space,
        c,
        alpha,
    })
}

fn parse_color_space_fn<'i, 't>(p: &mut Parser<'i, 't>) -> Option<CssColorExpr> {
    let origin = try_consume_from(p);
    let space = ColorSpace::from_color_fn_ident(p.expect_ident().ok()?.as_ref())?;
    let rules = [ChannelRule::NumOrPct; 3];
    if let Some(orig) = origin {
        return parse_relative_tail("color", space, orig, rules, p);
    }
    let (c, alpha) = parse_channels_and_alpha(p, rules, &[])?;
    Some(CssColorExpr::Functional {
        fn_name: "color",
        space,
        c,
        alpha,
    })
}

fn parse_alpha_fn<'i, 't>(p: &mut Parser<'i, 't>) -> Option<CssColorExpr> {
    p.expect_ident_matching("from").ok()?;
    let origin = parse_color_from_parser(p)?;
    p.expect_delim('/').ok()?;
    let alpha = parse_channel_component(p, ChannelRule::NumOrPct, &["alpha"], true)?;
    p.is_exhausted().then_some(CssColorExpr::Alpha {
        origin: Box::new(origin),
        alpha,
    })
}

fn parse_light_dark_fn<'i, 't>(p: &mut Parser<'i, 't>) -> Option<CssColorExpr> {
    let light = parse_color_from_parser(p)?;
    p.expect_comma().ok()?;
    let dark = parse_color_from_parser(p)?;
    p.is_exhausted()
        .then_some(CssColorExpr::LightDark(Box::new(light), Box::new(dark)))
}

#[rustfmt::skip]
const BLEND_MODES: &[&str] = &[
    "normal", "multiply", "screen", "overlay", "darken", "lighten", "color-dodge", "color-burn",
    "hard-light", "soft-light", "difference", "exclusion", "hue", "saturation", "color", "luminosity",
];

fn parse_color_layers_fn<'i, 't>(p: &mut Parser<'i, 't>) -> Option<CssColorExpr> {
    let blend_mode = p
        .try_parse(|p| -> Result<&'static str, cssparser::ParseError<'i, ()>> {
            let id = p.expect_ident()?;
            let m = BLEND_MODES
                .iter()
                .copied()
                .find(|m| m.eq_ignore_ascii_case(id.as_ref()))
                .ok_or_else(|| p.new_custom_error(()))?;
            p.expect_comma()?;
            Ok(m)
        })
        .unwrap_or("normal");
    let mut layers = Vec::new();
    loop {
        layers.push(parse_color_from_parser(p)?);
        if p.try_parse(Parser::expect_comma).is_err() {
            break;
        }
    }
    (!layers.is_empty() && p.is_exhausted())
        .then_some(CssColorExpr::ColorLayers { blend_mode, layers })
}

fn parse_color_mix_fn<'i, 't>(p: &mut Parser<'i, 't>) -> Option<CssColorExpr> {
    let (space, hue_method) = if p.try_parse(|p| p.expect_ident_matching("in")).is_ok() {
        let sp = ColorSpace::from_interp_ident(p.expect_ident().ok()?.as_ref())?;
        let hm = if sp.is_polar() {
            if let Ok(id) = p.try_parse(|p| p.expect_ident().map(|s| s.to_ascii_lowercase())) {
                let m = match id.as_str() {
                    "shorter" => HueMethod::Shorter,
                    "longer" => HueMethod::Longer,
                    "increasing" => HueMethod::Increasing,
                    "decreasing" => HueMethod::Decreasing,
                    _ => return None,
                };
                p.expect_ident_matching("hue").ok()?;
                m
            } else {
                HueMethod::Shorter
            }
        } else {
            HueMethod::Shorter
        };
        p.expect_comma().ok()?;
        (sp, hm)
    } else {
        (ColorSpace::Oklab, HueMethod::Shorter)
    };
    let mut items = Vec::new();
    loop {
        items.push(parse_mix_item(p)?);
        if p.try_parse(Parser::expect_comma).is_err() {
            break;
        }
    }
    (!items.is_empty() && p.is_exhausted()).then_some(CssColorExpr::ColorMix {
        space,
        hue_method,
        items,
    })
}

fn parse_mix_item<'i, 't>(p: &mut Parser<'i, 't>) -> Option<(CssColorExpr, Option<MixPct>)> {
    if let Some(pct) = try_parse_mix_pct(p) {
        return Some((parse_color_from_parser(p)?, Some(pct)));
    }
    let color = parse_color_from_parser(p)?;
    Some((color, try_parse_mix_pct(p)))
}

fn exact_f64_from_slice(slice: &str, suffix_len: usize, fallback: f64) -> f64 {
    let t = slice.trim();
    t.get(..t.len().saturating_sub(suffix_len))
        .and_then(|n| n.parse::<f64>().ok())
        .unwrap_or(fallback)
}

fn parse_nested_math<'i, 't>(
    p: &mut Parser<'i, 't>,
    fname: &str,
    kws: &[&str],
) -> Option<ChannelMath> {
    let lower = fname.to_ascii_lowercase();
    p.parse_nested_block(|nested| {
        parse_math_fn_args(&lower, nested, kws).ok_or_else(|| nested.new_custom_error::<_, ()>(()))
    })
    .ok()
}

fn try_parse_mix_pct<'i, 't>(p: &mut Parser<'i, 't>) -> Option<MixPct> {
    let state = p.state();
    let start = p.position();
    match p.next().ok()?.clone() {
        Token::Percentage { unit_value, .. } => {
            let pct = exact_f64_from_slice(p.slice_from(start), 1, f64::from(unit_value) * 100.0);
            if !(0.0..=100.0).contains(&pct) {
                p.reset(&state);
                return None;
            }
            Some(MixPct {
                expr: ChannelMath::Pct(pct),
                specified_text: format!("{}%", format_css_f64(pct)),
                literal_pct: Some(pct),
            })
        }
        Token::Function(ref fname) if is_math_fn_name(fname.as_ref()) => {
            let Some(expr) = parse_nested_math(p, fname.as_ref(), &[])
                .filter(|e| e.dim() == Some(MathDim::Percent))
            else {
                p.reset(&state);
                return None;
            };
            Some(MixPct {
                specified_text: expr.serialize_specified(),
                expr,
                literal_pct: None,
            })
        }
        _ => {
            p.reset(&state);
            None
        }
    }
}

fn is_channel_pct(spec: &ChannelSpec) -> bool {
    matches!(spec, ChannelSpec::Expr { is_pct: true, .. })
}

fn ch_expr(expr: ChannelMath, specified_text: String, is_pct: bool, is_calc: bool) -> ChannelSpec {
    ChannelSpec::Expr {
        expr,
        specified_text,
        is_pct,
        is_calc,
    }
}

fn parse_channel_component<'i, 't>(
    p: &mut Parser<'i, 't>,
    rule: ChannelRule,
    kws: &[&str],
    allow_none: bool,
) -> Option<ChannelSpec> {
    let start = p.position();
    match p.next().ok()?.clone() {
        Token::Ident(ident) => {
            let lower = ident.to_ascii_lowercase();
            if allow_none && lower == "none" {
                return Some(ChannelSpec::None);
            }
            kws.contains(&lower.as_str())
                .then_some(ChannelSpec::Keyword(lower))
        }
        Token::Number { value, .. } => {
            if rule == ChannelRule::PctOnly {
                return None;
            }
            let v = exact_f64_from_slice(p.slice_from(start), 0, f64::from(value));
            Some(ch_expr(
                ChannelMath::Num(v),
                format_css_f64(v),
                false,
                false,
            ))
        }
        Token::Percentage { unit_value, .. } => {
            if rule == ChannelRule::NumOrAngle {
                return None;
            }
            let pct = exact_f64_from_slice(p.slice_from(start), 1, f64::from(unit_value) * 100.0);
            Some(ch_expr(
                ChannelMath::Pct(pct),
                format!("{}%", format_css_f64(pct)),
                true,
                false,
            ))
        }
        Token::Dimension { value, unit, .. } => {
            if rule != ChannelRule::NumOrAngle {
                return None;
            }
            let v = exact_f64_from_slice(p.slice_from(start), unit.len(), f64::from(value));
            let deg = angle_unit_to_deg(v, unit.as_ref())?;
            Some(ch_expr(
                ChannelMath::AngleDeg(deg),
                format!("{}{}", format_css_f64(v), unit.to_ascii_lowercase()),
                false,
                false,
            ))
        }
        Token::Function(ref fname) if is_math_fn_name(fname.as_ref()) => {
            let expr = parse_nested_math(p, fname.as_ref(), kws)?;
            if rule == ChannelRule::NumOrAngle && expr.contains_percent() {
                return None;
            }
            let dim = expr.dim()?;
            let ok = matches!(
                (rule, dim),
                (ChannelRule::NumOrPct, MathDim::Number | MathDim::Percent)
                    | (ChannelRule::NumOrAngle, MathDim::Number | MathDim::Angle)
                    | (ChannelRule::PctOnly, MathDim::Percent)
            );
            ok.then(|| {
                ch_expr(
                    expr.clone(),
                    expr.serialize_specified(),
                    dim == MathDim::Percent,
                    true,
                )
            })
        }
        _ => None,
    }
}

#[rustfmt::skip]
fn is_math_fn_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "calc" | "min" | "max" | "clamp" | "round" | "mod" | "rem" | "sin" | "cos" | "tan"
            | "asin" | "acos" | "atan" | "atan2" | "pow" | "sqrt" | "hypot" | "log" | "exp" | "abs" | "sign"
            | "sibling-index" | "sibling-count" | "random"
    )
}

fn angle_unit_to_deg(v: f64, unit: &str) -> Option<f64> {
    match unit.to_ascii_lowercase().as_str() {
        "deg" => Some(v),
        "rad" => Some(v.to_degrees()),
        "grad" => Some(v * 0.9),
        "turn" => Some(v * 360.0),
        _ => None,
    }
}

#[rustfmt::skip]
fn parse_random_channel<'i, 't>(p: &mut Parser<'i, 't>, kws: &[&str]) -> Option<ChannelMath> {
    let st0 = p.state();
    let (mut dashed_ident, mut element_scoped, mut prop_scoped, mut prop_index_scoped, mut ua_ident, mut fixed) =
        (None::<String>, false, false, false, None::<String>, None::<Box<ChannelMath>>);
    let mut saw_options = false;
    if p.try_parse(|p| -> Result<(), cssparser::ParseError<'i, ()>> {
        p.expect_ident_matching("fixed")?;
        let f = parse_math_atom(p, kws).ok_or_else(|| p.new_custom_error(()))?;
        if let ChannelMath::Num(v) = f {
            if !v.is_finite() || v < 0.0 || v >= 1.0 { return Err(p.new_custom_error(())); }
        }
        p.expect_comma()?;
        fixed = Some(Box::new(f));
        Ok(())
    }).is_ok() {
        saw_options = true;
    } else if p.try_parse(|p| -> Result<(), cssparser::ParseError<'i, ()>> {
        p.expect_ident_matching("auto")?;
        p.expect_comma()?;
        Ok(())
    }).is_ok() {
        element_scoped = true; prop_index_scoped = true; saw_options = true;
    } else {
        type RandOpts = (Option<String>, bool, bool, bool, Option<String>);
        let opt_res = p.try_parse(|p| -> Result<RandOpts, cssparser::ParseError<'i, ()>> {
            let (mut d, mut es, mut ps, mut pis, mut ua, mut count) = (None, false, false, false, None, 0usize);
            while count < 3 {
                let Ok(id) = p.try_parse(|p| p.expect_ident().map(|s| s.as_ref().to_string())) else { break; };
                let lower = id.to_ascii_lowercase();
                if id.starts_with("--") {
                    if d.is_some() { return Err(p.new_custom_error(())); }
                    d = Some(id);
                } else if lower == "element-scoped" {
                    if es { return Err(p.new_custom_error(())); }
                    es = true;
                } else if lower == "property-scoped" {
                    if ps || pis || ua.is_some() { return Err(p.new_custom_error(())); }
                    ps = true;
                } else if lower == "property-index-scoped" {
                    if ps || pis || ua.is_some() { return Err(p.new_custom_error(())); }
                    pis = true;
                } else if id.starts_with("ua-") && id.len() > 3 {
                    if ps || pis || ua.is_some() { return Err(p.new_custom_error(())); }
                    ua = Some(id);
                } else {
                    return Err(p.new_custom_error(()));
                }
                count += 1;
            }
            if count == 0 { return Err(p.new_custom_error(())); }
            p.expect_comma()?;
            Ok((d, es, ps, pis, ua))
        });
        if let Ok((d, es, ps, pis, ua)) = opt_res {
            dashed_ident = d; element_scoped = es; prop_scoped = ps; prop_index_scoped = pis; ua_ident = ua;
            saw_options = true;
        }
    }
    if !saw_options {
        p.reset(&st0);
        element_scoped = true;
        prop_index_scoped = true;
    }
    let min = Box::new(parse_math_sum(p, kws)?);
    p.expect_comma().ok()?;
    let max = Box::new(parse_math_sum(p, kws)?);
    let step = if p.try_parse(Parser::expect_comma).is_ok() {
        Some(Box::new(parse_math_sum(p, kws)?))
    } else {
        None
    };
    p.is_exhausted().then_some(ChannelMath::Random(Box::new(ChannelRandom {
        dashed_ident, element_scoped, prop_scoped, prop_index_scoped, ua_ident, fixed, min, max, step,
    })))
}

fn parse_math_fn_args<'i, 't>(
    fn_name: &str,
    p: &mut Parser<'i, 't>,
    kws: &[&str],
) -> Option<ChannelMath> {
    if fn_name == "random" {
        return parse_random_channel(p, kws);
    }
    if fn_name == "sibling-index" || fn_name == "sibling-count" {
        return p
            .is_exhausted()
            .then_some(ChannelMath::Func(fn_name.to_string(), Vec::new()));
    }
    if fn_name == "calc" {
        let e = parse_math_sum(p, kws)?;
        return p
            .is_exhausted()
            .then_some(ChannelMath::Func("calc".to_string(), vec![e]));
    }
    if fn_name == "clamp" {
        let lo = if p.try_parse(|p| p.expect_ident_matching("none")).is_ok() {
            ChannelMath::Num(f64::NEG_INFINITY)
        } else {
            parse_math_sum(p, kws)?
        };
        p.expect_comma().ok()?;
        let mid = parse_math_sum(p, kws)?;
        p.expect_comma().ok()?;
        let hi = if p.try_parse(|p| p.expect_ident_matching("none")).is_ok() {
            ChannelMath::Num(f64::INFINITY)
        } else {
            parse_math_sum(p, kws)?
        };
        return p
            .is_exhausted()
            .then_some(ChannelMath::Func("clamp".to_string(), vec![lo, mid, hi]));
    }
    if fn_name == "round" {
        let _ = p.try_parse(|p| -> Result<(), cssparser::ParseError<'i, ()>> {
            if matches!(
                p.expect_ident()?.to_ascii_lowercase().as_str(),
                "nearest" | "up" | "down" | "to-zero"
            ) {
                p.expect_comma()?;
                Ok(())
            } else {
                Err(p.new_custom_error(()))
            }
        });
    }
    let mut args = Vec::new();
    loop {
        args.push(parse_math_sum(p, kws)?);
        if p.try_parse(Parser::expect_comma).is_err() {
            break;
        }
    }
    (!args.is_empty() && p.is_exhausted()).then_some(ChannelMath::Func(fn_name.to_string(), args))
}

#[rustfmt::skip]
fn parse_math_sum<'i, 't>(p: &mut Parser<'i, 't>, kws: &[&str]) -> Option<ChannelMath> {
    let mut lhs = parse_math_product(p, kws)?;
    loop {
        let st = p.state();
        match p.next() {
            Ok(Token::Delim('+')) => lhs = ChannelMath::Add(Box::new(lhs), Box::new(parse_math_product(p, kws)?)),
            Ok(Token::Delim('-')) => lhs = ChannelMath::Sub(Box::new(lhs), Box::new(parse_math_product(p, kws)?)),
            _ => { p.reset(&st); return Some(lhs); }
        }
    }
}

#[rustfmt::skip]
fn parse_math_product<'i, 't>(p: &mut Parser<'i, 't>, kws: &[&str]) -> Option<ChannelMath> {
    let mut lhs = parse_math_atom(p, kws)?;
    loop {
        let st = p.state();
        match p.next() {
            Ok(Token::Delim('*')) => lhs = ChannelMath::Mul(Box::new(lhs), Box::new(parse_math_atom(p, kws)?)),
            Ok(Token::Delim('/')) => lhs = ChannelMath::Div(Box::new(lhs), Box::new(parse_math_atom(p, kws)?)),
            _ => { p.reset(&st); return Some(lhs); }
        }
    }
}

fn parse_math_atom<'i, 't>(p: &mut Parser<'i, 't>, kws: &[&str]) -> Option<ChannelMath> {
    let start = p.position();
    match p.next().ok()?.clone() {
        Token::Number { value, .. } => Some(ChannelMath::Num(exact_f64_from_slice(
            p.slice_from(start),
            0,
            f64::from(value),
        ))),
        Token::Percentage { unit_value, .. } => Some(ChannelMath::Pct(exact_f64_from_slice(
            p.slice_from(start),
            1,
            f64::from(unit_value) * 100.0,
        ))),
        Token::Dimension { value, unit, .. } => {
            let v = exact_f64_from_slice(p.slice_from(start), unit.len(), f64::from(value));
            if let Some(deg) = angle_unit_to_deg(v, unit.as_ref()) {
                return Some(ChannelMath::AngleDeg(deg));
            }
            let (px, lu) = match unit.to_ascii_lowercase().as_str() {
                "cm" => (v * 96.0 / 2.54, LengthUnit::Px),
                "mm" => (v * 96.0 / 25.4, LengthUnit::Px),
                "q" => (v * 96.0 / 101.6, LengthUnit::Px),
                "in" => (v * 96.0, LengthUnit::Px),
                "pc" => (v * 16.0, LengthUnit::Px),
                "pt" => (v * 96.0 / 72.0, LengthUnit::Px),
                other => (v, LengthUnit::from_unit_str(other)?),
            };
            Some(ChannelMath::Length(px, lu))
        }
        Token::Ident(id) => {
            let lower = id.to_ascii_lowercase();
            (matches!(
                lower.as_str(),
                "pi" | "e" | "infinity" | "-infinity" | "nan"
            ) || kws.contains(&lower.as_str()))
            .then_some(ChannelMath::Ident(lower))
        }
        Token::ParenthesisBlock => p
            .parse_nested_block(|nested| {
                let inner = parse_math_sum(nested, kws)
                    .ok_or_else(|| nested.new_custom_error::<_, ()>(()))?;
                if !nested.is_exhausted() {
                    return Err(nested.new_custom_error(()));
                }
                Ok(inner)
            })
            .ok(),
        Token::Function(ref fname) if is_math_fn_name(fname.as_ref()) => {
            let lower = fname.to_ascii_lowercase();
            p.parse_nested_block(|nested| {
                parse_math_fn_args(&lower, nested, kws)
                    .ok_or_else(|| nested.new_custom_error::<_, ()>(()))
            })
            .ok()
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// All 148 W3C CSS Color Level 4 Named Colors (sorted for O(log N) lookup)
// ---------------------------------------------------------------------------

#[rustfmt::skip]
const NAMED_COLORS: &[(&str, [u8; 3])] = &[
    ("aliceblue", [240, 248, 255]), ("antiquewhite", [250, 235, 215]), ("aqua", [0, 255, 255]),
    ("aquamarine", [127, 255, 212]), ("azure", [240, 255, 255]), ("beige", [245, 245, 220]),
    ("bisque", [255, 228, 196]), ("black", [0, 0, 0]), ("blanchedalmond", [255, 235, 205]),
    ("blue", [0, 0, 255]), ("blueviolet", [138, 43, 226]), ("brown", [165, 42, 42]),
    ("burlywood", [222, 184, 135]), ("cadetblue", [95, 158, 160]), ("chartreuse", [127, 255, 0]),
    ("chocolate", [210, 105, 30]), ("coral", [255, 127, 80]), ("cornflowerblue", [100, 149, 237]),
    ("cornsilk", [255, 248, 220]), ("crimson", [220, 20, 60]), ("cyan", [0, 255, 255]),
    ("darkblue", [0, 0, 139]), ("darkcyan", [0, 139, 139]), ("darkgoldenrod", [184, 134, 11]),
    ("darkgray", [169, 169, 169]), ("darkgreen", [0, 100, 0]), ("darkgrey", [169, 169, 169]),
    ("darkkhaki", [189, 183, 107]), ("darkmagenta", [139, 0, 139]), ("darkolivegreen", [85, 107, 47]),
    ("darkorange", [255, 140, 0]), ("darkorchid", [153, 50, 204]), ("darkred", [139, 0, 0]),
    ("darksalmon", [233, 150, 122]), ("darkseagreen", [143, 188, 143]), ("darkslateblue", [72, 61, 139]),
    ("darkslategray", [47, 79, 79]), ("darkslategrey", [47, 79, 79]), ("darkturquoise", [0, 206, 209]),
    ("darkviolet", [148, 0, 211]), ("deeppink", [255, 20, 147]), ("deepskyblue", [0, 191, 255]),
    ("dimgray", [105, 105, 105]), ("dimgrey", [105, 105, 105]), ("dodgerblue", [30, 144, 255]),
    ("firebrick", [178, 34, 34]), ("floralwhite", [255, 250, 240]), ("forestgreen", [34, 139, 34]),
    ("fuchsia", [255, 0, 255]), ("gainsboro", [220, 220, 220]), ("ghostwhite", [248, 248, 255]),
    ("gold", [255, 215, 0]), ("goldenrod", [218, 165, 32]), ("gray", [128, 128, 128]),
    ("green", [0, 128, 0]), ("greenyellow", [173, 255, 47]), ("grey", [128, 128, 128]),
    ("honeydew", [240, 255, 240]), ("hotpink", [255, 105, 180]), ("indianred", [205, 92, 92]),
    ("indigo", [75, 0, 130]), ("ivory", [255, 255, 240]), ("khaki", [240, 230, 140]),
    ("lavender", [230, 230, 250]), ("lavenderblush", [255, 240, 245]), ("lawngreen", [124, 252, 0]),
    ("lemonchiffon", [255, 250, 205]), ("lightblue", [173, 216, 230]), ("lightcoral", [240, 128, 128]),
    ("lightcyan", [224, 255, 255]), ("lightgoldenrodyellow", [250, 250, 210]), ("lightgray", [211, 211, 211]),
    ("lightgreen", [144, 238, 144]), ("lightgrey", [211, 211, 211]), ("lightpink", [255, 182, 193]),
    ("lightsalmon", [255, 160, 122]), ("lightseagreen", [32, 178, 170]), ("lightskyblue", [135, 206, 250]),
    ("lightslategray", [119, 136, 153]), ("lightslategrey", [119, 136, 153]), ("lightsteelblue", [176, 196, 222]),
    ("lightyellow", [255, 255, 224]), ("lime", [0, 255, 0]), ("limegreen", [50, 205, 50]),
    ("linen", [250, 240, 230]), ("magenta", [255, 0, 255]), ("maroon", [128, 0, 0]),
    ("mediumaquamarine", [102, 205, 170]), ("mediumblue", [0, 0, 205]), ("mediumorchid", [186, 85, 211]),
    ("mediumpurple", [147, 112, 219]), ("mediumseagreen", [60, 179, 113]), ("mediumslateblue", [123, 104, 238]),
    ("mediumspringgreen", [0, 250, 154]), ("mediumturquoise", [72, 209, 204]), ("mediumvioletred", [199, 21, 133]),
    ("midnightblue", [25, 25, 112]), ("mintcream", [245, 255, 250]), ("mistyrose", [255, 228, 225]),
    ("moccasin", [255, 228, 181]), ("navajowhite", [255, 222, 173]), ("navy", [0, 0, 128]),
    ("oldlace", [253, 245, 230]), ("olive", [128, 128, 0]), ("olivedrab", [107, 142, 35]),
    ("orange", [255, 165, 0]), ("orangered", [255, 69, 0]), ("orchid", [218, 112, 214]),
    ("palegoldenrod", [238, 232, 170]), ("palegreen", [152, 251, 152]), ("paleturquoise", [175, 238, 238]),
    ("palevioletred", [219, 112, 147]), ("papayawhip", [255, 239, 213]), ("peachpuff", [255, 218, 185]),
    ("peru", [205, 133, 63]), ("pink", [255, 192, 203]), ("plum", [221, 160, 221]),
    ("powderblue", [176, 224, 230]), ("purple", [128, 0, 128]), ("rebeccapurple", [102, 51, 153]),
    ("red", [255, 0, 0]), ("rosybrown", [188, 143, 143]), ("royalblue", [65, 105, 225]),
    ("saddlebrown", [139, 69, 19]), ("salmon", [250, 128, 114]), ("sandybrown", [244, 164, 96]),
    ("seagreen", [46, 139, 87]), ("seashell", [255, 245, 238]), ("sienna", [160, 82, 45]),
    ("silver", [192, 192, 192]), ("skyblue", [135, 206, 235]), ("slateblue", [106, 90, 205]),
    ("slategray", [112, 128, 144]), ("slategrey", [112, 128, 144]), ("snow", [255, 250, 250]),
    ("springgreen", [0, 255, 127]), ("steelblue", [70, 130, 180]), ("tan", [210, 180, 140]),
    ("teal", [0, 128, 128]), ("thistle", [216, 191, 216]), ("tomato", [255, 99, 71]),
    ("turquoise", [64, 224, 208]), ("violet", [238, 130, 238]), ("wheat", [245, 222, 179]),
    ("white", [255, 255, 255]), ("whitesmoke", [245, 245, 245]), ("yellow", [255, 255, 0]),
    ("yellowgreen", [154, 205, 50]),
];

fn lookup_named_color(lower: &str) -> Option<(&'static str, [u8; 3])> {
    NAMED_COLORS
        .binary_search_by_key(&lower, |&(k, _)| k)
        .ok()
        .map(|i| NAMED_COLORS[i])
}
