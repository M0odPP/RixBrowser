//! Pure-Rust `@font-face` decoder (WOFF2, WOFF, TTF, OTF), `unicode-range`
//! parser, and per-document [`FontFaceSet`].

use thiserror::Error;

use crate::font_loader::{FontDatabase, FontId, FontStyle, FontWeight};

/// Detected web font container format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WebFontFormat {
    Woff2,
    Woff,
    TrueType,
    OpenType,
    Collection,
}

/// Errors encountered while decoding or registering an `@font-face` resource.
#[derive(Debug, Error)]
pub enum WebFontError {
    #[error("web font payload is empty")]
    EmptyData,
    #[error("unrecognized web font magic bytes")]
    UnsupportedFormat,
    #[error("failed to decode WOFF/WOFF2 container: {0}")]
    ContainerDecode(String),
    #[error("failed to parse decoded SFNT font: {0}")]
    FontParse(String),
}

/// Inspects the first 4 bytes to identify a WOFF2, WOFF, TTF, OTF, or TTC payload.
#[must_use]
pub fn detect_web_font_format(bytes: &[u8]) -> Option<WebFontFormat> {
    if bytes.len() < 4 {
        return None;
    }
    match &bytes[..4] {
        b"wOF2" => Some(WebFontFormat::Woff2),
        b"wOFF" => Some(WebFontFormat::Woff),
        b"\x00\x01\x00\x00" | b"true" => Some(WebFontFormat::TrueType),
        b"OTTO" => Some(WebFontFormat::OpenType),
        b"ttcf" => Some(WebFontFormat::Collection),
        _ => None,
    }
}

/// Decodes WOFF2 (Brotli) / WOFF (zlib) / TTF / OTF bytes into raw SFNT bytes.
pub fn decode_web_font(bytes: &[u8]) -> Result<(WebFontFormat, Vec<u8>), WebFontError> {
    if bytes.is_empty() {
        return Err(WebFontError::EmptyData);
    }
    let format = detect_web_font_format(bytes).ok_or(WebFontError::UnsupportedFormat)?;
    let sfnt = match format {
        WebFontFormat::Woff2 => wuff::decompress_woff2(bytes)
            .map_err(|e| WebFontError::ContainerDecode(format!("{e:?}")))?,
        WebFontFormat::Woff => wuff::decompress_woff1(bytes)
            .map_err(|e| WebFontError::ContainerDecode(format!("{e:?}")))?,
        WebFontFormat::TrueType | WebFontFormat::OpenType | WebFontFormat::Collection => {
            bytes.to_vec()
        }
    };
    read_fonts::FileRef::new(&sfnt).map_err(|e| WebFontError::FontParse(format!("{e:?}")))?;
    Ok((format, sfnt))
}

/// Inclusive Unicode scalar range for CSS `@font-face` `unicode-range` filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UnicodeRange {
    pub start: u32,
    pub end: u32,
}

impl UnicodeRange {
    #[must_use]
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    #[must_use]
    pub const fn contains(self, ch: char) -> bool {
        let cp = ch as u32;
        cp >= self.start && cp <= self.end
    }
}

/// Parses a CSS `unicode-range` descriptor (e.g. `U+0025, U+0400-04FF, U+4??`).
#[must_use]
pub fn parse_unicode_range(spec: &str) -> Vec<UnicodeRange> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        let trimmed = part.trim();
        let Some(hex) = trimmed
            .strip_prefix("U+")
            .or_else(|| trimmed.strip_prefix("u+"))
        else {
            continue;
        };
        if let Some((a, b)) = hex.split_once('-') {
            if let (Ok(start), Ok(end)) = (u32::from_str_radix(a, 16), u32::from_str_radix(b, 16)) {
                if start <= end && end <= 0x10_FFFF {
                    out.push(UnicodeRange::new(start, end));
                }
            }
        } else if hex.contains('?') {
            let start_hex: String = hex
                .chars()
                .map(|c| if c == '?' { '0' } else { c })
                .collect();
            let end_hex: String = hex
                .chars()
                .map(|c| if c == '?' { 'F' } else { c })
                .collect();
            if let (Ok(start), Ok(end)) = (
                u32::from_str_radix(&start_hex, 16),
                u32::from_str_radix(&end_hex, 16),
            ) {
                if start <= end && end <= 0x10_FFFF {
                    out.push(UnicodeRange::new(start, end));
                }
            }
        } else if let Ok(cp) = u32::from_str_radix(hex, 16) {
            if cp <= 0x10_FFFF {
                out.push(UnicodeRange::new(cp, cp));
            }
        }
    }
    out
}

/// A loaded `@font-face` registration bound to a document's [`FontFaceSet`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebFontFace {
    pub family: String,
    pub family_lower: String,
    pub weight_range: (u16, u16),
    pub style: FontStyle,
    pub unicode_ranges: Vec<UnicodeRange>,
    pub font_id: FontId,
    pub format: WebFontFormat,
}

impl WebFontFace {
    #[must_use]
    pub fn covers_codepoint(&self, ch: char) -> bool {
        if self.unicode_ranges.is_empty() {
            return true;
        }
        self.unicode_ranges.iter().any(|r| r.contains(ch))
    }

    #[must_use]
    pub fn weight_distance(&self, desired: FontWeight) -> u16 {
        let w = desired.0;
        if w < self.weight_range.0 {
            self.weight_range.0 - w
        } else {
            w.saturating_sub(self.weight_range.1)
        }
    }
}

/// Per-document `@font-face` registry supporting `unicode-range` filtering.
#[derive(Debug, Default, Clone)]
pub struct FontFaceSet {
    faces: Vec<WebFontFace>,
}

impl FontFaceSet {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes `raw_bytes` (WOFF2/WOFF/TTF/OTF), registers the font in `db` under `family`,
    /// and records the `@font-face` metadata and `unicode-range` constraints.
    pub fn register_font_bytes(
        &mut self,
        db: &mut FontDatabase,
        family: impl Into<String>,
        weight_range: (u16, u16),
        style: FontStyle,
        unicode_ranges: Vec<UnicodeRange>,
        raw_bytes: &[u8],
    ) -> Result<FontId, WebFontError> {
        let (format, sfnt) = decode_web_font(raw_bytes)?;
        let family_str = family.into();
        let font_id = db
            .add_web_font_bytes(sfnt, 0, &family_str, FontWeight(weight_range.0), style)
            .map_err(WebFontError::FontParse)?;
        self.faces.push(WebFontFace {
            family_lower: family_str.to_ascii_lowercase(),
            family: family_str,
            weight_range,
            style,
            unicode_ranges,
            font_id,
            format,
        });
        Ok(font_id)
    }

    /// Resolves the best registered `@font-face` matching `(family, weight, style)` whose
    /// `unicode-range` includes `ch` and whose glyph table supports `ch`.
    #[must_use]
    pub fn match_face_for_char(
        &self,
        db: &FontDatabase,
        family: &str,
        weight: FontWeight,
        style: FontStyle,
        ch: char,
    ) -> Option<FontId> {
        let target = family
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_ascii_lowercase();
        let mut best: Option<((u8, u16, usize), FontId)> = None;
        for (idx, face) in self.faces.iter().enumerate() {
            if face.family_lower != target || !face.covers_codepoint(ch) {
                continue;
            }
            if let Some(font) = db.get_font(face.font_id) {
                if !font.supports_char(ch) {
                    continue;
                }
            } else {
                continue;
            }
            let style_penalty = if face.style == style { 0 } else { 1 };
            let key = (style_penalty, face.weight_distance(weight), idx);
            if best.is_none_or(|(b, _)| key < b) {
                best = Some((key, face.font_id));
            }
        }
        best.map(|(_, id)| id)
    }

    /// Returns all registered `@font-face` entries for this document.
    #[must_use]
    pub fn faces(&self) -> &[WebFontFace] {
        &self.faces
    }
}
