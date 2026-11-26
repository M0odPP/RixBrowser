//! Unlimited lazy system font discovery, Unicode block coverage index, and
//! on-demand font byte loading.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::UNIX_EPOCH;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

const COVERAGE_BLOCK_COUNT: usize = 32;
const FONT_INDEX_CACHE_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FontId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum FontStyle {
    #[default]
    Normal,
    Italic,
    Oblique,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FontWeight(pub u16);

impl FontWeight {
    pub const THIN: Self = Self(100);
    pub const EXTRA_LIGHT: Self = Self(200);
    pub const LIGHT: Self = Self(300);
    pub const NORMAL: Self = Self(400);
    pub const MEDIUM: Self = Self(500);
    pub const SEMI_BOLD: Self = Self(600);
    pub const BOLD: Self = Self(700);
    pub const EXTRA_BOLD: Self = Self(800);
    pub const BLACK: Self = Self(900);
}

impl Default for FontWeight {
    fn default() -> Self {
        Self::NORMAL
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FontDescriptor {
    pub family: String,
    pub weight: FontWeight,
    pub style: FontStyle,
}

impl FontDescriptor {
    pub fn new(family: impl Into<String>) -> Self {
        Self {
            family: family.into(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        }
    }

    pub fn with_weight(mut self, weight: FontWeight) -> Self {
        self.weight = weight;
        self
    }

    pub fn with_style(mut self, style: FontStyle) -> Self {
        self.style = style;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FontMetrics {
    pub units_per_em: u16,
    pub ascender: i16,
    pub descender: i16,
    pub line_gap: i16,
    pub cap_height: Option<i16>,
    pub x_height: Option<i16>,
}

impl FontMetrics {
    pub fn scale(&self, font_size: f32) -> f32 {
        font_size / (self.units_per_em as f32).max(1.0)
    }

    pub fn ascender_px(&self, font_size: f32) -> f32 {
        self.ascender as f32 * self.scale(font_size)
    }

    pub fn descender_px(&self, font_size: f32) -> f32 {
        self.descender as f32 * self.scale(font_size)
    }

    pub fn line_height_px(&self, font_size: f32) -> f32 {
        (self.ascender - self.descender + self.line_gap) as f32 * self.scale(font_size)
    }

    pub fn x_height_px(&self, font_size: f32) -> Option<f32> {
        self.x_height
            .map(|height| height as f32 * self.scale(font_size))
            .filter(|height| *height > 0.0)
    }
}

#[derive(Clone)]
enum FontSource {
    Memory(Arc<Vec<u8>>),
    LazyFile {
        path: PathBuf,
        loaded: Arc<OnceLock<Arc<Vec<u8>>>>,
    },
}

#[derive(Clone)]
pub struct FontData {
    pub id: FontId,
    pub descriptor: FontDescriptor,
    source: FontSource,
    cache_identity: u64,
    pub face_index: u32,
    pub metrics: FontMetrics,
    pub coverage_mask: u64,
    blob: Arc<OnceLock<linebender_resource_handle::Blob<u8>>>,
    pub variations: Vec<([u8; 4], f32)>,
    pub normalized_coords: Vec<i16>,
}

impl FontData {
    /// Returns whether raw font bytes are already resident in memory.
    #[must_use]
    pub fn is_loaded(&self) -> bool {
        match &self.source {
            FontSource::Memory(_) => true,
            FontSource::LazyFile { loaded, .. } => loaded.get().is_some(),
        }
    }

    /// Returns the source file path if this font is backed by a lazy system font file.
    #[must_use]
    pub fn source_path(&self) -> Option<&Path> {
        match &self.source {
            FontSource::Memory(_) => None,
            FontSource::LazyFile { path, .. } => Some(path.as_path()),
        }
    }

    /// Stable hash identity for text shaping cache keys without triggering lazy file reads.
    #[must_use]
    pub fn cache_identity(&self) -> u64 {
        self.cache_identity
    }

    /// Loads font bytes lazily on first use and returns a shared `Arc<Vec<u8>>`.
    #[must_use]
    pub fn bytes(&self) -> Arc<Vec<u8>> {
        match &self.source {
            FontSource::Memory(b) => Arc::clone(b),
            FontSource::LazyFile { path, loaded } => {
                Arc::clone(loaded.get_or_init(|| Arc::new(std::fs::read(path).unwrap_or_default())))
            }
        }
    }

    fn bytes_slice(&self) -> &[u8] {
        match &self.source {
            FontSource::Memory(b) => b.as_slice(),
            FontSource::LazyFile { path, loaded } => loaded
                .get_or_init(|| Arc::new(std::fs::read(path).unwrap_or_default()))
                .as_slice(),
        }
    }

    pub fn blob(&self) -> &linebender_resource_handle::Blob<u8> {
        self.blob
            .get_or_init(|| linebender_resource_handle::Blob::new(self.bytes()))
    }

    pub fn as_rustybuzz_face(&self) -> Option<rustybuzz::Face<'_>> {
        let mut face = rustybuzz::Face::from_slice(self.bytes_slice(), self.face_index)?;
        if !self.variations.is_empty() {
            let variations: Vec<rustybuzz::Variation> = self
                .variations
                .iter()
                .map(|(tag, value)| rustybuzz::Variation {
                    tag: rustybuzz::ttf_parser::Tag::from_bytes(tag),
                    value: *value,
                })
                .collect();
            face.set_variations(&variations);
        }
        Some(face)
    }

    pub fn as_swash_font(&self) -> Option<swash::FontRef<'_>> {
        swash::FontRef::from_index(self.bytes_slice(), self.face_index as usize)
    }

    pub fn as_read_font(&self) -> Result<read_fonts::FontRef<'_>, read_fonts::ReadError> {
        read_fonts::FontRef::from_index(self.bytes_slice(), self.face_index)
    }

    pub fn supports_char(&self, ch: char) -> bool {
        if let Some(font) = self.as_swash_font() {
            let gid = font.charmap().map(ch);
            if gid != 0 {
                return true;
            }
            if ch == '\0' {
                return true;
            }
        }
        if let Ok(rf) = self.as_read_font() {
            use read_fonts::TableProvider;
            if let Ok(cmap) = rf.cmap() {
                if cmap.map_codepoint(ch as u32).is_some() {
                    return true;
                }
            }
        }
        false
    }
}

/// Maps any Unicode scalar value to one of 32 script/symbol/emoji coverage buckets.
#[must_use]
pub fn unicode_coverage_block(ch: char) -> usize {
    let cp = ch as u32;
    match cp {
        0x0000..=0x007F => 0,    // Basic Latin
        0x0080..=0x024F => 1,    // Latin-1 & Latin Extended-A/B
        0x0250..=0x036F => 2,    // IPA, Spacing Modifiers, Combining Diacritics
        0x0370..=0x03FF => 3,    // Greek & Coptic
        0x0400..=0x052F => 4,    // Cyrillic
        0x0530..=0x058F => 5,    // Armenian
        0x0590..=0x05FF => 6,    // Hebrew
        0x0600..=0x077F => 7,    // Arabic & Syriac
        0x0900..=0x097F => 8,    // Devanagari
        0x0980..=0x0DFF => 9, // Bengali, Gurmukhi, Gujarati, Tamil, Telugu, Kannada, Malayalam, Sinhala
        0x0E00..=0x0FFF => 10, // Thai, Lao, Tibetan
        0x1000..=0x137F => 11, // Myanmar, Georgian, Hangul Jamo, Ethiopic
        0x1E00..=0x1FFF => 12, // Latin Extended Additional & Greek Extended
        0x2000..=0x20CF => 13, // General Punctuation, Superscripts, Currency
        0x2100..=0x21FF => 14, // Letterlike, Number Forms, Arrows
        0x2200..=0x23FF => 15, // Mathematical Operators & Misc Technical
        0x2400..=0x25FF => 16, // Enclosed Alphanumerics, Box Drawing, Geometric Shapes
        0x2600..=0x27BF => 17, // Misc Symbols & Dingbats
        0x2800..=0x2BFF => 18, // Braille, Supplemental Arrows/Math
        0x3000..=0x303F => 19, // CJK Symbols & Punctuation
        0x3040..=0x30FF => 20, // Hiragana & Katakana
        0x3100..=0x33FF => 21, // Bopomofo, Hangul Compat, Enclosed CJK
        0x3400..=0x4DBF => 22, // CJK Unified Ideographs Ext A
        0x4E00..=0x9FFF => 23, // CJK Unified Ideographs
        0xAC00..=0xD7AF => 24, // Hangul Syllables
        0xE000..=0xF8FF => 25, // Private Use Area (Icon Fonts)
        0xFB00..=0xFEFF => 26, // Alphabetic/Arabic Presentation Forms, Half/Fullwidth
        0x1F000..=0x1F2FF => 27, // Mahjong, Playing Cards, Enclosed Ideographic
        0x1F300..=0x1F5FF => 28, // Misc Symbols & Pictographs (Emoji)
        0x1F600..=0x1F6FF => 29, // Emoticons & Transport Symbols (Emoji)
        0x1F700..=0x1FAFF => 30, // Alchemical, Supplemental Symbols & Extended Emoji
        _ => 31,              // Supplemental planes & rare blocks
    }
}

/// Representative probe codepoints per coverage block for fast metadata indexing.
const BLOCK_PROBES: &[(usize, &[u32])] = &[
    (0, &[0x0041, 0x0061, 0x0030]),
    (1, &[0x00E9, 0x00FC, 0x0105, 0x0159]),
    (2, &[0x0259, 0x0301]),
    (3, &[0x0391, 0x03B1, 0x03C9]),
    (4, &[0x0410, 0x0430, 0x044F, 0x0457]),
    (5, &[0x0531, 0x0561]),
    (6, &[0x05D0, 0x05E9]),
    (7, &[0x0627, 0x0644, 0x064A]),
    (8, &[0x0905, 0x0915]),
    (9, &[0x0985, 0x0B85, 0x0C05]),
    (10, &[0x0E01, 0x0E17, 0x0F40]),
    (11, &[0x1000, 0x10D0, 0x1200]),
    (12, &[0x1EA1, 0x1F00]),
    (13, &[0x2014, 0x2026, 0x20AC]),
    (14, &[0x2190, 0x2192, 0x2122]),
    (15, &[0x2200, 0x221E, 0x2260, 0x2318]),
    (16, &[0x2500, 0x25A0, 0x25CF]),
    (17, &[0x2600, 0x2605, 0x2713, 0x2764]),
    (18, &[0x2801, 0x2B50]),
    (19, &[0x3001, 0x3002, 0x300C]),
    (20, &[0x3042, 0x30A2]),
    (21, &[0x3105, 0x3131]),
    (22, &[0x3400, 0x3405]),
    (23, &[0x4E00, 0x4E2D, 0x65E5, 0x672C]),
    (24, &[0xAC00, 0xD55C, 0xAE00]),
    (25, &[0xE000, 0xE0A0, 0xF000]),
    (26, &[0xFB01, 0xFF01]),
    (27, &[0x1F004, 0x1F197]),
    (28, &[0x1F3AE, 0x1F4A1, 0x1F525]),
    (29, &[0x1F600, 0x1F680]),
    (30, &[0x1F914, 0x1F9E0, 0x1FA84]),
    (31, &[0x20000, 0x1D400]),
];

fn compute_coverage_mask(
    swash_font: &swash::FontRef<'_>,
    read_font: &read_fonts::FontRef<'_>,
) -> u64 {
    use read_fonts::TableProvider;
    let charmap = swash_font.charmap();
    let rf_cmap = read_font.cmap().ok();
    let mut mask = 0u64;
    for &(block, probes) in BLOCK_PROBES {
        let covered = probes.iter().any(|&cp| {
            let ch = char::from_u32(cp).unwrap_or('\0');
            if charmap.map(ch) != 0 {
                return true;
            }
            rf_cmap
                .as_ref()
                .is_some_and(|c| c.map_codepoint(cp).is_some())
        });
        if covered {
            mask |= 1u64 << block;
        }
    }
    mask
}

#[derive(Clone, Serialize, Deserialize)]
struct CachedFontFace {
    face_index: u32,
    descriptor: FontDescriptor,
    metrics: FontMetrics,
    coverage_mask: u64,
    variations: Vec<([u8; 4], f32)>,
    normalized_coords: Vec<i16>,
}

#[derive(Clone, Serialize, Deserialize)]
struct CachedFontFile {
    path: PathBuf,
    file_len: u64,
    mtime_secs: u64,
    faces: Vec<CachedFontFace>,
}

#[derive(Serialize, Deserialize)]
struct DiskFontIndex {
    version: u32,
    files: Vec<CachedFontFile>,
}

#[derive(Clone)]
pub struct FontDatabase {
    pub(crate) fonts: Vec<FontData>,
    pub(crate) family_index: HashMap<String, Vec<FontId>>,
    coverage_buckets: [Vec<FontId>; COVERAGE_BLOCK_COUNT],
    pub(crate) aliases: Arc<crate::fontconfig::FamilyAliases>,
    pub default_sans_serif: Option<FontId>,
    pub default_serif: Option<FontId>,
    pub default_monospace: Option<FontId>,
    pub default_emoji: Option<FontId>,
}

impl Default for FontDatabase {
    fn default() -> Self {
        Self {
            fonts: Vec::new(),
            family_index: HashMap::new(),
            coverage_buckets: std::array::from_fn(|_| Vec::new()),
            aliases: Arc::new(crate::fontconfig::FamilyAliases::default()),
            default_sans_serif: None,
            default_serif: None,
            default_monospace: None,
            default_emoji: None,
        }
    }
}

static SYSTEM_FONTS_CACHE: OnceLock<FontDatabase> = OnceLock::new();

fn get_font_cache_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("RIX_FONT_CACHE_PATH") {
        return Some(PathBuf::from(path));
    }
    if let Ok(home) = std::env::var("HOME") {
        let cache_dir = PathBuf::from(home).join(".cache").join("rixbrowser");
        let _ = std::fs::create_dir_all(&cache_dir);
        return Some(cache_dir.join("font_index_v2.bin"));
    }
    None
}

fn fnv1a_u64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn compute_identity_for_path(path: &Path, face_index: u32, variations: &[([u8; 4], f32)]) -> u64 {
    let mut h = fnv1a_u64(path.to_string_lossy().as_bytes());
    h = h.wrapping_mul(0x0100_0000_01b3) ^ u64::from(face_index);
    for (tag, val) in variations {
        h = h.wrapping_mul(0x0100_0000_01b3) ^ u64::from(u32::from_le_bytes(*tag));
        h = h.wrapping_mul(0x0100_0000_01b3) ^ u64::from(val.to_bits());
    }
    h
}

fn compute_identity_for_memory(
    bytes: &Arc<Vec<u8>>,
    face_index: u32,
    variations: &[([u8; 4], f32)],
) -> u64 {
    let mut h = (Arc::as_ptr(bytes) as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    h = h.wrapping_mul(0x0100_0000_01b3) ^ u64::from(face_index);
    for (tag, val) in variations {
        h = h.wrapping_mul(0x0100_0000_01b3) ^ u64::from(u32::from_le_bytes(*tag));
        h = h.wrapping_mul(0x0100_0000_01b3) ^ u64::from(val.to_bits());
    }
    h
}

impl FontDatabase {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.fonts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fonts.is_empty()
    }

    /// Returns how many indexed fonts currently have their raw bytes resident in RAM.
    #[must_use]
    pub fn loaded_font_count(&self) -> usize {
        self.fonts.iter().filter(|f| f.is_loaded()).count()
    }

    pub fn get_font(&self, id: FontId) -> Option<&FontData> {
        self.fonts.get(id.0)
    }

    /// Finds a font covering `ch` using the indexed Unicode coverage table rather
    /// than linearly scanning every installed system font.
    pub fn find_covering_font(&self, ch: char, preferred: Option<FontId>) -> Option<FontId> {
        if let Some(id) = preferred {
            if let Some(f) = self.get_font(id) {
                if f.supports_char(ch) {
                    return Some(id);
                }
            }
        }
        if is_emoji_char(ch) {
            if let Some(id) = self.default_emoji {
                if let Some(f) = self.get_font(id) {
                    if f.supports_char(ch) {
                        return Some(id);
                    }
                }
            }
        }

        let block = unicode_coverage_block(ch);
        let bucket = &self.coverage_buckets[block];

        // Pass 1: check already-resident fonts in this block first (zero disk I/O).
        for &id in bucket {
            if let Some(f) = self.get_font(id) {
                if f.is_loaded() && f.supports_char(ch) {
                    return Some(id);
                }
            }
        }
        // Pass 2: lazily check remaining fonts indexed for this Unicode block.
        for &id in bucket {
            if let Some(f) = self.get_font(id) {
                if !f.is_loaded() && f.supports_char(ch) {
                    return Some(id);
                }
            }
        }

        // Pass 3: fallback for uncommon characters outside block probes (loaded fonts first).
        for f in &self.fonts {
            if f.is_loaded() && f.supports_char(ch) {
                return Some(f.id);
            }
        }
        for f in &self.fonts {
            if !f.is_loaded() && f.supports_char(ch) {
                return Some(f.id);
            }
        }
        None
    }

    fn index_coverage(&mut self, id: FontId, mask: u64) {
        for (bit_idx, bucket) in self.coverage_buckets.iter_mut().enumerate() {
            if (mask & (1u64 << bit_idx)) != 0 {
                bucket.push(id);
            }
        }
    }

    pub fn add_font_bytes(&mut self, bytes: Vec<u8>, face_index: u32) -> Result<FontId, String> {
        self.add_memory_font_internal(bytes, face_index, None, None, None)
    }

    /// Registers decoded `@font-face` SFNT bytes with explicit CSS family, weight, and style.
    pub fn add_web_font_bytes(
        &mut self,
        bytes: Vec<u8>,
        face_index: u32,
        family_override: &str,
        weight_override: FontWeight,
        style_override: FontStyle,
    ) -> Result<FontId, String> {
        self.add_memory_font_internal(
            bytes,
            face_index,
            Some(family_override),
            Some(weight_override),
            Some(style_override),
        )
    }

    /// Registers decoded `@font-face` SFNT bytes honoring variable `wght` axis instances across
    /// `weight_range` (CSS Fonts L4 §4.3) so variable web fonts render at the requested weight
    /// instead of falling back to the font's raw default axis weight (often `100` Thin).
    pub fn add_web_font_bytes_range(
        &mut self,
        bytes: Vec<u8>,
        face_index: u32,
        family_override: &str,
        weight_range: (u16, u16),
        style_override: FontStyle,
    ) -> Result<Vec<(u16, FontId)>, String> {
        use read_fonts::TableProvider;
        let read_font = read_fonts::FontRef::from_index(&bytes, face_index)
            .map_err(|e| format!("Failed to parse font data (read-fonts): {e:?}"))?;
        let _ = read_font
            .head()
            .map_err(|e| format!("Missing head table: {e:?}"))?;
        let swash_font = swash::FontRef::from_index(&bytes, face_index as usize)
            .ok_or_else(|| "Failed to parse font data (swash)".to_string())?;

        let variations = swash_font.variations();
        let wght = swash::tag_from_bytes(b"wght");
        let Some(axis) = variations.find_by_tag(wght) else {
            let id = self.add_web_font_bytes(
                bytes,
                face_index,
                family_override,
                FontWeight(weight_range.0),
                style_override,
            )?;
            return Ok(vec![(weight_range.0, id)]);
        };
        let (axis_min, axis_max) = (axis.min_value(), axis.max_value());
        if axis_min.partial_cmp(&axis_max) != Some(std::cmp::Ordering::Less) {
            let id = self.add_web_font_bytes(
                bytes,
                face_index,
                family_override,
                FontWeight(weight_range.0),
                style_override,
            )?;
            return Ok(vec![(weight_range.0, id)]);
        }

        let r_lo = weight_range.0.min(weight_range.1);
        let r_hi = weight_range.0.max(weight_range.1);
        let mut target_weights: Vec<u16> = (1..=9u16)
            .map(|step| step * 100)
            .filter(|&w| w >= r_lo && w <= r_hi)
            .collect();
        if target_weights.is_empty() {
            target_weights.push(r_lo);
        }

        let family = family_override.to_owned();
        let family_lower = family.to_lowercase();
        let metrics = extract_metrics(&swash_font, &read_font);
        let coverage_mask = compute_coverage_mask(&swash_font, &read_font);
        let weight_coords: Vec<(u16, f32, Vec<i16>)> = target_weights
            .into_iter()
            .map(|w| {
                let clamped = f32::from(w).clamp(axis_min, axis_max);
                let coords: Vec<i16> = variations.normalized_coords([(wght, clamped)]).collect();
                (w, clamped, coords)
            })
            .collect();

        let bytes = Arc::new(bytes);
        let blob_cell = Arc::new(OnceLock::new());
        let _ = blob_cell.set(linebender_resource_handle::Blob::new(bytes.clone()));

        let mut registered = Vec::with_capacity(weight_coords.len());
        for (w, clamped, coords) in weight_coords {
            let var_tags = vec![(*b"wght", clamped)];
            let ident = compute_identity_for_memory(&bytes, face_index, &var_tags);
            let id = FontId(self.fonts.len());
            self.fonts.push(FontData {
                id,
                descriptor: FontDescriptor {
                    family: family.clone(),
                    weight: FontWeight(w),
                    style: style_override,
                },
                source: FontSource::Memory(Arc::clone(&bytes)),
                cache_identity: ident,
                face_index,
                metrics,
                coverage_mask,
                blob: Arc::clone(&blob_cell),
                variations: var_tags,
                normalized_coords: coords,
            });
            self.family_index
                .entry(family_lower.clone())
                .or_default()
                .push(id);
            registered.push((w, id));
        }
        if let Some(&(_, first_id)) = registered.first() {
            self.index_coverage(first_id, coverage_mask);
            self.auto_assign_fallback(first_id, &family);
        }
        Ok(registered)
    }

    fn add_memory_font_internal(
        &mut self,
        bytes: Vec<u8>,
        face_index: u32,
        family_override: Option<&str>,
        weight_override: Option<FontWeight>,
        style_override: Option<FontStyle>,
    ) -> Result<FontId, String> {
        use read_fonts::TableProvider;
        let read_font = read_fonts::FontRef::from_index(&bytes, face_index)
            .map_err(|e| format!("Failed to parse font data (read-fonts): {e:?}"))?;
        let _ = read_font
            .head()
            .map_err(|e| format!("Missing head table: {e:?}"))?;

        let swash_font = swash::FontRef::from_index(&bytes, face_index as usize)
            .ok_or_else(|| "Failed to parse font data (swash)".to_string())?;

        let family = family_override
            .map(str::to_owned)
            .or_else(|| extract_family_name(&swash_font, &read_font))
            .unwrap_or_else(|| "Unknown".to_string());

        let attrs = swash_font.attributes();
        let style = style_override.unwrap_or(match attrs.style() {
            swash::Style::Normal => FontStyle::Normal,
            swash::Style::Italic => FontStyle::Italic,
            swash::Style::Oblique(_) => FontStyle::Oblique,
        });
        let raw_weight = attrs.weight().0;
        let weight = weight_override.unwrap_or(FontWeight(raw_weight.clamp(100, 900)));

        let metrics = extract_metrics(&swash_font, &read_font);
        let coverage_mask = compute_coverage_mask(&swash_font, &read_font);
        let weight_instances = if weight_override.is_some() {
            Vec::new()
        } else {
            variable_weight_instances(&swash_font, weight)
        };

        let id = FontId(self.fonts.len());
        let descriptor = FontDescriptor {
            family: family.clone(),
            weight,
            style,
        };

        let bytes = Arc::new(bytes);
        let blob_cell = Arc::new(OnceLock::new());
        let _ = blob_cell.set(linebender_resource_handle::Blob::new(bytes.clone()));
        let cache_identity = compute_identity_for_memory(&bytes, face_index, &[]);

        self.fonts.push(FontData {
            id,
            descriptor: descriptor.clone(),
            source: FontSource::Memory(Arc::clone(&bytes)),
            cache_identity,
            face_index,
            metrics,
            coverage_mask,
            blob: Arc::clone(&blob_cell),
            variations: Vec::new(),
            normalized_coords: Vec::new(),
        });
        self.family_index
            .entry(family.to_lowercase())
            .or_default()
            .push(id);
        self.index_coverage(id, coverage_mask);

        for (instance_weight, normalized_coords) in weight_instances {
            let instance_id = FontId(self.fonts.len());
            let variations = vec![(*b"wght", instance_weight as f32)];
            let inst_ident = compute_identity_for_memory(&bytes, face_index, &variations);
            self.fonts.push(FontData {
                id: instance_id,
                descriptor: FontDescriptor {
                    weight: FontWeight(instance_weight),
                    ..descriptor.clone()
                },
                source: FontSource::Memory(Arc::clone(&bytes)),
                cache_identity: inst_ident,
                face_index,
                metrics,
                coverage_mask,
                blob: Arc::clone(&blob_cell),
                variations,
                normalized_coords,
            });
            self.family_index
                .entry(family.to_lowercase())
                .or_default()
                .push(instance_id);
        }

        self.auto_assign_fallback(id, &family);
        Ok(id)
    }

    pub fn load_font_file<P: AsRef<Path>>(&mut self, path: P) -> Result<Vec<FontId>, String> {
        let path_ref = path.as_ref();
        let cached = extract_file_metadata(path_ref)
            .ok_or_else(|| format!("No valid font faces found in {}", path_ref.display()))?;
        let ids = self.register_cached_file(&cached);
        if ids.is_empty() {
            Err(format!(
                "No valid font faces found in {}",
                path_ref.display()
            ))
        } else {
            Ok(ids)
        }
    }

    fn register_cached_file(&mut self, file: &CachedFontFile) -> Vec<FontId> {
        let shared_loaded = Arc::new(OnceLock::new());
        let shared_blob = Arc::new(OnceLock::new());
        let mut ids = Vec::with_capacity(file.faces.len());
        let mut primary_faces = HashSet::new();

        for face in &file.faces {
            let id = FontId(self.fonts.len());
            let cache_identity =
                compute_identity_for_path(&file.path, face.face_index, &face.variations);
            self.fonts.push(FontData {
                id,
                descriptor: face.descriptor.clone(),
                source: FontSource::LazyFile {
                    path: file.path.clone(),
                    loaded: Arc::clone(&shared_loaded),
                },
                cache_identity,
                face_index: face.face_index,
                metrics: face.metrics,
                coverage_mask: face.coverage_mask,
                blob: Arc::clone(&shared_blob),
                variations: face.variations.clone(),
                normalized_coords: face.normalized_coords.clone(),
            });
            self.family_index
                .entry(face.descriptor.family.to_lowercase())
                .or_default()
                .push(id);
            // Index coverage once per physical face (skip duplicate synthetic weight instances).
            if face.variations.is_empty() && primary_faces.insert(face.face_index) {
                self.index_coverage(id, face.coverage_mask);
            }
            self.auto_assign_fallback(id, &face.descriptor.family);
            ids.push(id);
        }
        ids
    }

    /// Discovers and lazily indexes all installed system fonts (no 32-file cap).
    pub fn scan_system_fonts(&mut self) -> usize {
        if self.fonts.is_empty() && std::env::var_os("RIX_FONT_CACHE_PATH").is_none() {
            let cached = SYSTEM_FONTS_CACHE.get_or_init(|| {
                let mut db = FontDatabase::new();
                db.scan_system_fonts_with_limit(usize::MAX);
                db
            });
            *self = cached.clone();
            return self.fonts.len();
        }
        self.scan_system_fonts_with_limit(usize::MAX)
    }

    /// Lazily indexes up to `limit` font faces from system directories, using the
    /// v2 `postcard` metadata cache on warm runs and parallel `rayon` extraction on misses.
    pub fn scan_system_fonts_with_limit(&mut self, limit: usize) -> usize {
        let cache_path = get_font_cache_path();
        let disk_map: HashMap<PathBuf, CachedFontFile> = cache_path
            .as_deref()
            .and_then(load_disk_index)
            .map(|idx| idx.files.into_iter().map(|f| (f.path.clone(), f)).collect())
            .unwrap_or_default();

        let discovered = discover_system_font_files();
        let mut hits: Vec<CachedFontFile> = Vec::with_capacity(discovered.len());
        let mut misses: Vec<PathBuf> = Vec::new();

        for (path, file_len, mtime_secs) in discovered {
            if let Some(entry) = disk_map.get(&path) {
                if entry.file_len == file_len && entry.mtime_secs == mtime_secs {
                    hits.push(entry.clone());
                    continue;
                }
            }
            misses.push(path);
        }

        let cache_dirty = !misses.is_empty() || hits.len() != disk_map.len();
        if !misses.is_empty() {
            let parsed: Vec<CachedFontFile> = misses
                .par_iter()
                .filter_map(|p| extract_file_metadata(p))
                .collect();
            hits.extend(parsed);
        }

        if cache_dirty {
            if let Some(ref cp) = cache_path {
                save_disk_index(cp, &hits);
            }
        }

        let mut count = 0usize;
        for file in &hits {
            if count >= limit {
                break;
            }
            let ids = self.register_cached_file(file);
            count += ids.len();
        }

        self.finalize_default_fallbacks(count);
        tracing::info!(
            "Lazily indexed {count} system font faces ({} resident)",
            self.loaded_font_count()
        );
        count
    }

    fn finalize_default_fallbacks(&mut self, count: usize) {
        const BASIC_LATIN_BIT: u64 = 1u64 << 0;
        let sans_id = {
            let pick = |preferred: &[&str]| -> Option<FontId> {
                for &target in preferred {
                    if let Some(id) = self.best_fallback_matching(|font| {
                        (font.coverage_mask & BASIC_LATIN_BIT) != 0
                            && font.descriptor.family.eq_ignore_ascii_case(target)
                    }) {
                        return Some(id);
                    }
                }
                None
            };
            pick(&[
                "noto sans",
                "liberation sans",
                "dejavu sans",
                "adwaita sans",
                "arial",
                "helvetica",
                "cantarell",
            ])
            .or_else(|| {
                self.best_fallback_matching(|font| {
                    let family = font.descriptor.family.to_lowercase();
                    (font.coverage_mask & BASIC_LATIN_BIT) != 0
                        && (family.contains("sans")
                            || family.contains("adwaita")
                            || family.contains("dejavu"))
                        && !family.contains("mono")
                        && !family.contains("serif")
                        && !family.contains("cjk")
                })
            })
            .or_else(|| {
                self.best_fallback_matching(|font| {
                    let family = font.descriptor.family.to_lowercase();
                    (family.contains("sans")
                        || family.contains("adwaita")
                        || family.contains("dejavu"))
                        && !family.contains("mono")
                        && !family.contains("serif")
                        && !family.contains("cjk")
                })
            })
        };
        if let Some(id) = sans_id {
            self.default_sans_serif = Some(id);
        }
        let serif_id = {
            let pick = |preferred: &[&str]| -> Option<FontId> {
                for &target in preferred {
                    if let Some(id) = self.best_fallback_matching(|font| {
                        (font.coverage_mask & BASIC_LATIN_BIT) != 0
                            && font.descriptor.family.eq_ignore_ascii_case(target)
                    }) {
                        return Some(id);
                    }
                }
                None
            };
            pick(&[
                "noto serif",
                "liberation serif",
                "dejavu serif",
                "times new roman",
                "georgia",
                "nimbus roman",
                "free serif",
                "freeserif",
            ])
            .or_else(|| {
                self.best_fallback_matching(|font| {
                    let family = font.descriptor.family.to_lowercase();
                    (font.coverage_mask & BASIC_LATIN_BIT) != 0
                        && family.contains("serif")
                        && !family.contains("sans")
                        && !family.contains("cjk")
                })
            })
            .or_else(|| {
                self.best_fallback_matching(|font| {
                    let family = font.descriptor.family.to_lowercase();
                    family.contains("serif") && !family.contains("sans") && !family.contains("cjk")
                })
            })
        };
        if let Some(id) = serif_id {
            self.default_serif = Some(id);
        }
        let mono_id = {
            let pick = |preferred: &[&str]| -> Option<FontId> {
                for &target in preferred {
                    if let Some(id) = self.best_fallback_matching(|font| {
                        (font.coverage_mask & BASIC_LATIN_BIT) != 0
                            && font.descriptor.family.eq_ignore_ascii_case(target)
                    }) {
                        return Some(id);
                    }
                }
                None
            };
            pick(&[
                "noto sans mono",
                "liberation mono",
                "dejavu sans mono",
                "adwaita mono",
                "courier new",
                "ubuntu mono",
            ])
            .or_else(|| {
                self.best_fallback_matching(|font| {
                    let family = font.descriptor.family.to_lowercase();
                    (font.coverage_mask & BASIC_LATIN_BIT) != 0
                        && (family.contains("mono")
                            || family.contains("console")
                            || family.contains("typewriter"))
                })
            })
            .or_else(|| {
                self.best_fallback_matching(|font| {
                    let family = font.descriptor.family.to_lowercase();
                    family.contains("mono")
                        || family.contains("console")
                        || family.contains("typewriter")
                })
            })
        };
        if let Some(id) = mono_id {
            self.default_monospace = Some(id);
        }
        if let Some(id) = self.best_fallback_matching(|font| {
            let family = font.descriptor.family.to_lowercase();
            family.contains("emoji") || family.contains("color")
        }) {
            self.default_emoji = Some(id);
        }

        if self.default_emoji.is_none() {
            let emoji_candidates = [
                "/usr/share/fonts/noto/NotoColorEmoji.ttf",
                "/usr/share/fonts/google-noto-color-emoji/NotoColorEmoji.ttf",
                "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
                "/usr/share/fonts/noto-color-emoji/NotoColorEmoji.ttf",
                "/usr/share/fonts/TTF/NotoColorEmoji.ttf",
                "/System/Library/Fonts/Apple Color Emoji.ttc",
                "/System/Library/Fonts/Apple Color Emoji.ttf",
                "C:\\Windows\\Fonts\\seguiemj.ttf",
            ];
            for path_str in emoji_candidates {
                let path = Path::new(path_str);
                if path.exists() {
                    if let Ok(ids) = self.load_font_file(path) {
                        if let Some(&id) = ids.first() {
                            self.default_emoji = Some(id);
                            break;
                        }
                    }
                }
            }
        }

        self.set_family_aliases(crate::fontconfig::FamilyAliases::from_system());

        if count > 0 {
            let first = self.fonts.first().map(|f| f.id);
            if self.default_sans_serif.is_none() {
                self.default_sans_serif = first;
            }
            if self.default_serif.is_none() {
                self.default_serif = first;
            }
            if self.default_monospace.is_none() {
                self.default_monospace = first;
            }
        }
    }

    pub fn set_family_aliases(&mut self, aliases: crate::fontconfig::FamilyAliases) {
        self.aliases = Arc::new(aliases);
        let normal = FontDescriptor::new("");
        if let Some(id) = self.match_family("sans-serif", &normal) {
            self.default_sans_serif = Some(id);
        }
        if let Some(id) = self.match_family("serif", &normal) {
            self.default_serif = Some(id);
        }
        if let Some(id) = self.match_family("monospace", &normal) {
            self.default_monospace = Some(id);
        }
        if let Some(id) = self.match_family("emoji", &normal) {
            self.default_emoji = Some(id);
        }
    }

    fn best_fallback_matching(
        &self,
        mut matches_family: impl FnMut(&FontData) -> bool,
    ) -> Option<FontId> {
        self.fonts
            .iter()
            .filter(|font| matches_family(font))
            .min_by_key(|font| {
                let style_penalty = match font.descriptor.style {
                    FontStyle::Normal => 0,
                    FontStyle::Oblique => 1,
                    FontStyle::Italic => 2,
                };
                let weight_distance =
                    (font.descriptor.weight.0 as i32 - FontWeight::NORMAL.0 as i32).unsigned_abs();
                (style_penalty, weight_distance, font.id)
            })
            .map(|font| font.id)
    }

    fn auto_assign_fallback(&mut self, id: FontId, family: &str) {
        let lower = family.to_lowercase();

        if self.default_emoji.is_none() && (lower.contains("emoji") || lower.contains("color")) {
            self.default_emoji = Some(id);
        } else if self.default_monospace.is_none()
            && (lower.contains("mono") || lower.contains("console") || lower.contains("typewriter"))
        {
            self.default_monospace = Some(id);
        } else if self.default_serif.is_none()
            && (lower.contains("serif") && !lower.contains("sans"))
        {
            self.default_serif = Some(id);
        } else if self.default_sans_serif.is_none()
            && (lower.contains("sans") || lower.contains("adwaita") || lower.contains("dejavu"))
        {
            self.default_sans_serif = Some(id);
        }
    }
}

fn discover_system_font_files() -> Vec<(PathBuf, u64, u64)> {
    let mut roots = vec![
        PathBuf::from("/usr/share/fonts/Adwaita"),
        PathBuf::from("/usr/share/fonts/cantarell"),
        PathBuf::from("/usr/share/fonts/liberation"),
        PathBuf::from("/usr/share/fonts/noto"),
        PathBuf::from("/usr/share/fonts/TTF"),
        PathBuf::from("/usr/share/fonts/truetype"),
        PathBuf::from("/usr/share/fonts"),
        PathBuf::from("/usr/local/share/fonts"),
    ];
    if let Ok(home) = std::env::var("HOME") {
        let home_path = PathBuf::from(home);
        roots.push(home_path.join(".local/share/fonts"));
        roots.push(home_path.join(".fonts"));
    }

    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for root in roots {
        if root.exists() {
            collect_font_files_recursive(&root, &mut seen, &mut files);
        }
    }
    files
}

fn collect_font_files_recursive(
    dir: &Path,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<(PathBuf, u64, u64)>,
) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_font_files_recursive(&path, seen, out);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if ext.eq_ignore_ascii_case("ttf")
                || ext.eq_ignore_ascii_case("otf")
                || ext.eq_ignore_ascii_case("ttc")
                || ext.eq_ignore_ascii_case("otc")
            {
                let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                if !seen.insert(key) {
                    continue;
                }
                if let Ok(meta) = entry.metadata() {
                    let mtime_secs = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_secs());
                    out.push((path, meta.len(), mtime_secs));
                }
            }
        }
    }
}

fn extract_file_metadata(path: &Path) -> Option<CachedFontFile> {
    use read_fonts::TableProvider;
    let meta = std::fs::metadata(path).ok()?;
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let bytes = std::fs::read(path).ok()?;
    let num_faces = count_faces(&bytes);
    let mut faces = Vec::new();

    for face_index in 0..num_faces {
        let Ok(read_font) = read_fonts::FontRef::from_index(&bytes, face_index) else {
            continue;
        };
        if read_font.head().is_err() {
            continue;
        }
        let Some(swash_font) = swash::FontRef::from_index(&bytes, face_index as usize) else {
            continue;
        };
        let family =
            extract_family_name(&swash_font, &read_font).unwrap_or_else(|| "Unknown".to_string());
        let attrs = swash_font.attributes();
        let style = match attrs.style() {
            swash::Style::Normal => FontStyle::Normal,
            swash::Style::Italic => FontStyle::Italic,
            swash::Style::Oblique(_) => FontStyle::Oblique,
        };
        let weight = FontWeight(attrs.weight().0.clamp(100, 900));
        let metrics = extract_metrics(&swash_font, &read_font);
        let coverage_mask = compute_coverage_mask(&swash_font, &read_font);
        let weight_instances = variable_weight_instances(&swash_font, weight);
        let descriptor = FontDescriptor {
            family,
            weight,
            style,
        };

        faces.push(CachedFontFace {
            face_index,
            descriptor: descriptor.clone(),
            metrics,
            coverage_mask,
            variations: Vec::new(),
            normalized_coords: Vec::new(),
        });

        for (instance_weight, normalized_coords) in weight_instances {
            faces.push(CachedFontFace {
                face_index,
                descriptor: FontDescriptor {
                    weight: FontWeight(instance_weight),
                    ..descriptor.clone()
                },
                metrics,
                coverage_mask,
                variations: vec![(*b"wght", instance_weight as f32)],
                normalized_coords,
            });
        }
    }

    if faces.is_empty() {
        return None;
    }
    Some(CachedFontFile {
        path: path.to_path_buf(),
        file_len: meta.len(),
        mtime_secs,
        faces,
    })
}

fn load_disk_index(path: &Path) -> Option<DiskFontIndex> {
    let raw = std::fs::read(path).ok()?;
    let index: DiskFontIndex = postcard::from_bytes(&raw).ok()?;
    if index.version != FONT_INDEX_CACHE_VERSION {
        return None;
    }
    Some(index)
}

fn save_disk_index(path: &Path, files: &[CachedFontFile]) {
    let idx = DiskFontIndex {
        version: FONT_INDEX_CACHE_VERSION,
        files: files.to_vec(),
    };
    if let Ok(encoded) = postcard::to_allocvec(&idx) {
        let _ = std::fs::write(path, encoded);
    }
}

fn count_faces(bytes: &[u8]) -> u32 {
    match read_fonts::FileRef::new(bytes) {
        Ok(read_fonts::FileRef::Font(_)) => 1,
        Ok(read_fonts::FileRef::Collection(col)) => col.len().max(1),
        Err(_) => 1,
    }
}

fn extract_family_name(
    swash_font: &swash::FontRef<'_>,
    read_font: &read_fonts::FontRef<'_>,
) -> Option<String> {
    use read_fonts::TableProvider;
    use swash::StringId;

    let mut full_fallback: Option<String> = None;
    let mut family_fallback: Option<String> = None;

    for s in swash_font.localized_strings() {
        if !s.is_decodable() {
            continue;
        }
        let text: String = s.chars().collect();
        let text = text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        match s.id() {
            StringId::TypographicFamily | StringId::WwsFamily => {
                return Some(text);
            }
            StringId::Family => {
                if family_fallback.is_none() {
                    family_fallback = Some(text);
                }
            }
            StringId::Full if full_fallback.is_none() => {
                full_fallback = Some(text);
            }
            _ => {}
        }
    }
    if let Some(f) = family_fallback {
        return Some(f);
    }

    if let Ok(name) = read_font.name() {
        use read_fonts::types::NameId;
        let data = name.string_data();
        let mut id1: Option<String> = None;
        let mut id4: Option<String> = None;
        for rec in name.name_record() {
            let nid = rec.name_id();
            if nid == NameId::TYPOGRAPHIC_FAMILY_NAME
                || nid == NameId::FAMILY_NAME
                || nid == NameId::FULL_NAME
            {
                if let Ok(ns) = rec.string(data) {
                    let text: String = ns.chars().collect();
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        continue;
                    }
                    let unicode = rec.is_unicode();
                    if nid == NameId::TYPOGRAPHIC_FAMILY_NAME {
                        if unicode {
                            return Some(text);
                        }
                        if id1.is_none() {
                            id4 = Some(text);
                        }
                    } else if nid == NameId::FAMILY_NAME {
                        if unicode {
                            if id1.is_none() {
                                id1 = Some(text);
                            }
                        } else if id1.is_none() {
                            id1 = Some(text);
                        }
                    } else if id4.is_none() {
                        id4 = Some(text);
                    }
                }
            }
        }
        if let Some(f) = id1 {
            return Some(f);
        }
        if let Some(f) = id4 {
            return Some(f);
        }
    }

    full_fallback
}

fn variable_weight_instances(
    font: &swash::FontRef<'_>,
    default_weight: FontWeight,
) -> Vec<(u16, Vec<i16>)> {
    let variations = font.variations();
    let wght = swash::tag_from_bytes(b"wght");
    let Some(axis) = variations.find_by_tag(wght) else {
        return Vec::new();
    };
    let (min, max) = (axis.min_value(), axis.max_value());
    if min.partial_cmp(&max) != Some(std::cmp::Ordering::Less) {
        return Vec::new();
    }
    (1..=9u16)
        .map(|step| step * 100)
        .filter(|weight| *weight != default_weight.0)
        .filter(|weight| (min..=max).contains(&(*weight as f32)))
        .map(|weight| {
            let coords = variations
                .normalized_coords([(wght, weight as f32)])
                .collect();
            (weight, coords)
        })
        .collect()
}

fn extract_metrics(
    swash_font: &swash::FontRef<'_>,
    read_font: &read_fonts::FontRef<'_>,
) -> FontMetrics {
    use read_fonts::TableProvider;

    let m = swash_font.metrics(&[]);
    let mut upem = m.units_per_em;
    let mut asc = m.ascent as i16;
    let mut desc = if m.descent > 0.0 {
        -(m.descent as i16)
    } else {
        m.descent as i16
    };
    if m.ascent < 0.0 {
        asc = m.ascent.abs() as i16;
    }
    let gap = m.leading as i16;
    let mut cap = if m.cap_height != 0.0 {
        Some(m.cap_height as i16)
    } else {
        None
    };
    let mut xh = if m.x_height != 0.0 {
        Some(m.x_height as i16)
    } else {
        None
    };

    if upem == 0 {
        if let Ok(head) = read_font.head() {
            upem = head.units_per_em();
        }
    }
    if upem == 0 {
        upem = 1000;
    }
    if asc == 0 {
        if let Ok(hhea) = read_font.hhea() {
            asc = hhea.ascender().to_i16();
        }
    }
    if desc == 0 {
        if let Ok(hhea) = read_font.hhea() {
            desc = hhea.descender().to_i16();
        }
    }
    if cap.is_none() || xh.is_none() {
        if let Ok(os2) = read_font.os2() {
            if cap.is_none() {
                if let Some(v) = os2.s_cap_height() {
                    if v != 0 {
                        cap = Some(v);
                    }
                }
            }
            if xh.is_none() {
                if let Some(v) = os2.sx_height() {
                    if v != 0 {
                        xh = Some(v);
                    }
                }
            }
        }
    }
    if asc < 0 {
        asc = -asc;
    }
    if desc > 0 {
        desc = -desc;
    }
    if asc == 0 {
        asc = (upem as i16 * 4) / 5;
    }
    if desc == 0 {
        desc = -(upem as i16 / 5);
    }

    FontMetrics {
        units_per_em: upem,
        ascender: asc,
        descender: desc,
        line_gap: gap,
        cap_height: cap,
        x_height: xh,
    }
}

pub(crate) fn is_emoji_char(ch: char) -> bool {
    let v = ch as u32;
    matches!(
        v,
        0x1F300..=0x1FAFF
            | 0x2600..=0x27BF
            | 0x2B00..=0x2BFF
            | 0xFE00..=0xFE0F
            | 0x1F000..=0x1F02F
            | 0x1F100..=0x1F1FF
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_fonts_cache_reuse() {
        let mut db1 = FontDatabase::new();
        let count1 = db1.scan_system_fonts();

        let mut db2 = FontDatabase::new();
        let count2 = db2.scan_system_fonts();

        assert_eq!(count1, count2);
        assert_eq!(db1.len(), db2.len());
        assert_eq!(db1.default_sans_serif, db2.default_sans_serif);
    }

    #[test]
    fn test_emoji_font_loaded_and_covers() {
        let mut db = FontDatabase::new();
        db.scan_system_fonts();
        if Path::new("/usr/share/fonts/noto/NotoColorEmoji.ttf").exists() {
            assert!(db.default_emoji.is_some());
            let emoji_font_id = db.default_emoji.unwrap();
            let font = db.get_font(emoji_font_id).unwrap();
            assert!(
                font.descriptor.family.contains("Emoji")
                    || font.descriptor.family.contains("Color")
            );
            let covering = db.find_covering_font('🎮', None);
            assert_eq!(covering, Some(emoji_font_id));
        }
    }

    #[test]
    fn test_font_paths_cache_roundtrip() {
        let temp_dir = std::env::temp_dir().join(format!("rix_font_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let cache_file = temp_dir.join("test_font_index_v2.bin");

        std::env::set_var("RIX_FONT_CACHE_PATH", cache_file.to_str().unwrap());

        let mut db1 = FontDatabase::new();
        let count1 = db1.scan_system_fonts_with_limit(8);
        assert!(count1 > 0);
        assert!(cache_file.exists());
        assert_eq!(db1.loaded_font_count(), 0);

        let mut db2 = FontDatabase::new();
        let count2 = db2.scan_system_fonts_with_limit(8);
        assert_eq!(count1, count2);
        assert_eq!(db2.loaded_font_count(), 0);

        std::env::remove_var("RIX_FONT_CACHE_PATH");
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
