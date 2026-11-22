//! Decoded raster/SVG image assets, parallel multi-core decoding (`rayon`),
//! `srcset` selection, and inline/external SVG path extraction for layout and paint.

use std::collections::HashMap;
use std::sync::Arc;

use rayon::prelude::*;
use rix_core::Size;
use rix_css::ColorRgba;
use rix_dom::{svg_element_to_path_d, Document, NodeData, NodeId, SvgViewBox};
use rix_ipc::ImageId;
use thiserror::Error;

/// Errors returned when decoding raster image assets.
#[derive(Debug, Error)]
pub enum AssetError {
    #[error("failed to decode image: {0}")]
    ImageDecode(#[from] image::ImageError),
    #[error("invalid RGBA buffer length {actual} for {width}x{height}")]
    InvalidRgbaSize {
        width: u32,
        height: u32,
        actual: usize,
    },
}

/// Extracted vector primitive inside an inline or `<img src="*.svg">` SVG element.
#[derive(Debug, Clone, PartialEq)]
pub struct SvgPathEntry {
    pub path_d: String,
    pub fill: Option<ColorRgba>,
    pub stroke: Option<(ColorRgba, f32)>,
}

/// Decoded external SVG vector asset ready for `<img src="*.svg">` rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedSvg {
    pub view_box: Option<SvgViewBox>,
    pub intrinsic_size: Size,
    pub paths: Vec<SvgPathEntry>,
}

/// Decoded RGBA8 image ready for intrinsic sizing and GPU upload.
#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub image_id: ImageId,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
}

impl DecodedImage {
    #[must_use]
    pub fn intrinsic_size(&self) -> Size {
        Size::new(self.width as f32, self.height as f32)
    }
}

/// Decoded raw RGBA8 payload produced by parallel off-thread decoding before registry binding.
#[derive(Debug, Clone)]
pub struct DecodedRasterBytes {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
}

/// Decodes a batch of `(key, encoded_bytes)` raster images (`png`, `jpeg`, `webp`, `gif`)
/// concurrently across CPU cores using `rayon`.
#[must_use]
pub fn decode_raster_batch_parallel(
    items: &[(String, Vec<u8>)],
) -> Vec<(String, Result<DecodedRasterBytes, AssetError>)> {
    if items.len() <= 1 {
        return items
            .iter()
            .map(|(k, bytes)| (k.clone(), decode_single_raster(bytes)))
            .collect();
    }
    items
        .par_iter()
        .map(|(k, bytes)| (k.clone(), decode_single_raster(bytes)))
        .collect()
}

/// Decodes a single encoded raster image into an [`Arc`]-wrapped RGBA8 buffer.
pub fn decode_single_raster(bytes: &[u8]) -> Result<DecodedRasterBytes, AssetError> {
    let decoded = image::load_from_memory(bytes)?.into_rgba8();
    let (width, height) = decoded.dimensions();
    if width == 0 || height == 0 {
        return Err(AssetError::InvalidRgbaSize {
            width,
            height,
            actual: 0,
        });
    }
    Ok(DecodedRasterBytes {
        width,
        height,
        rgba: Arc::new(decoded.into_raw()),
    })
}

/// Per-document registry of decoded raster (`png`, `jpeg`, `webp`, `gif`) and SVG images.
#[derive(Debug, Default)]
pub struct AssetRegistry {
    by_src: HashMap<String, DecodedImage>,
    by_id: HashMap<ImageId, DecodedImage>,
    svgs_by_src: HashMap<String, DecodedSvg>,
    next_image_id: u64,
}

impl AssetRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_src: HashMap::new(),
            by_id: HashMap::new(),
            svgs_by_src: HashMap::new(),
            next_image_id: 1,
        }
    }

    /// Decodes an encoded image (`png`, `jpeg`, `webp`, `gif`) and binds it to `src`.
    pub fn register_encoded(
        &mut self,
        src: impl Into<String>,
        bytes: &[u8],
    ) -> Result<ImageId, AssetError> {
        let decoded = decode_single_raster(bytes)?;
        self.register_rgba8_shared(src, decoded.width, decoded.height, decoded.rgba)
    }

    /// Registers a pre-decoded RGBA8 image buffer under `src`.
    pub fn register_rgba8(
        &mut self,
        src: impl Into<String>,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    ) -> Result<ImageId, AssetError> {
        self.register_rgba8_shared(src, width, height, Arc::new(rgba))
    }

    /// Registers a shared `Arc<Vec<u8>>` RGBA8 image buffer (zero-copy cross-tab cache reuse).
    pub fn register_rgba8_shared(
        &mut self,
        src: impl Into<String>,
        width: u32,
        height: u32,
        rgba: Arc<Vec<u8>>,
    ) -> Result<ImageId, AssetError> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(4))
            .ok_or(AssetError::InvalidRgbaSize {
                width,
                height,
                actual: rgba.len(),
            })?;
        if width == 0 || height == 0 || rgba.len() != expected {
            return Err(AssetError::InvalidRgbaSize {
                width,
                height,
                actual: rgba.len(),
            });
        }
        let image_id = ImageId(self.next_image_id);
        self.next_image_id = self.next_image_id.wrapping_add(1).max(1);
        let entry = DecodedImage {
            image_id,
            width,
            height,
            rgba,
        };
        let src_key = src.into();
        self.by_id.insert(image_id, entry.clone());
        self.by_src.insert(src_key, entry);
        Ok(image_id)
    }

    /// Binds an additional source key (e.g. raw relative `src` attribute) to an already-registered
    /// canonical raster or SVG asset without duplicating pixel memory or GPU uploads.
    pub fn register_alias(&mut self, alias_src: impl Into<String>, canonical_src: &str) -> bool {
        let alias = alias_src.into();
        if alias == canonical_src {
            return self.by_src.contains_key(canonical_src)
                || self.svgs_by_src.contains_key(canonical_src);
        }
        if let Some(img) = self.by_src.get(canonical_src).cloned() {
            self.by_src.insert(alias, img);
            return true;
        }
        if let Some(svg) = self.svgs_by_src.get(canonical_src).cloned() {
            self.svgs_by_src.insert(alias, svg);
            return true;
        }
        false
    }

    /// Registers a parsed SVG vector asset under `src` for `<img src="*.svg">` rendering.
    pub fn register_svg(&mut self, src: impl Into<String>, svg: DecodedSvg) {
        self.svgs_by_src.insert(src.into(), svg);
    }

    #[must_use]
    pub fn get_by_src(&self, src: &str) -> Option<&DecodedImage> {
        self.by_src.get(src)
    }

    #[must_use]
    pub fn get_svg_by_src(&self, src: &str) -> Option<&DecodedSvg> {
        self.svgs_by_src.get(src)
    }

    #[must_use]
    pub fn get_by_id(&self, id: ImageId) -> Option<&DecodedImage> {
        self.by_id.get(&id)
    }

    pub fn iter_images(&self) -> impl Iterator<Item = &DecodedImage> {
        self.by_id.values()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len() + self.svgs_by_src.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty() && self.svgs_by_src.is_empty()
    }
}

/// Extracts vector paths and intrinsic dimensions from a parsed SVG DOM document
/// (used for external `<img src="*.svg">` and `data:image/svg+xml` assets).
#[must_use]
pub fn extract_svg_from_document(doc: &Document, current_color: ColorRgba) -> Option<DecodedSvg> {
    let svg_node = doc.descendants(doc.root()).find(|&nid| {
        matches!(
            doc.get(nid),
            Some(NodeData::Element(e)) if e.name.local.as_str().eq_ignore_ascii_case("svg")
        )
    })?;
    let Some(NodeData::Element(elem)) = doc.get(svg_node) else {
        return None;
    };
    let view_box = elem.get_attribute("viewBox").and_then(SvgViewBox::parse);
    let attr_w = elem
        .get_attribute("width")
        .and_then(parse_html_dimension_attr);
    let attr_h = elem
        .get_attribute("height")
        .and_then(parse_html_dimension_attr);
    let iw = attr_w
        .or_else(|| view_box.map(|vb| vb.width))
        .unwrap_or(32.0)
        .max(1.0);
    let ih = attr_h
        .or_else(|| view_box.map(|vb| vb.height))
        .unwrap_or(32.0)
        .max(1.0);
    let inherited_fill = elem
        .get_attribute("fill")
        .map(|raw| parse_svg_color(raw, current_color));
    let inherited_stroke = elem
        .get_attribute("stroke")
        .and_then(|raw| parse_svg_color(raw, current_color));
    let inherited_stroke_w = elem
        .get_attribute("stroke-width")
        .and_then(parse_html_dimension_attr)
        .unwrap_or(1.5);
    let mut paths = Vec::new();
    collect_svg_paths_from_dom(
        doc,
        svg_node,
        current_color,
        inherited_fill,
        inherited_stroke,
        inherited_stroke_w,
        &mut paths,
    );
    if paths.is_empty() {
        return None;
    }
    Some(DecodedSvg {
        view_box,
        intrinsic_size: Size::new(iw, ih),
        paths,
    })
}

/// Recursively collects SVG vector primitives from `parent` inside `doc`.
pub fn collect_svg_paths_from_dom(
    doc: &Document,
    parent: NodeId,
    current_color: ColorRgba,
    inherited_fill: Option<Option<ColorRgba>>,
    inherited_stroke: Option<ColorRgba>,
    inherited_stroke_w: f32,
    out: &mut Vec<SvgPathEntry>,
) {
    for child in doc.children(parent) {
        let Some(NodeData::Element(elem)) = doc.get(child) else {
            continue;
        };
        let fill = match elem.get_attribute("fill") {
            Some(raw) => Some(parse_svg_color(raw, current_color)),
            None => inherited_fill,
        };
        let stroke = match elem.get_attribute("stroke") {
            Some(raw) => parse_svg_color(raw, current_color),
            None => inherited_stroke,
        };
        let stroke_w = elem
            .get_attribute("stroke-width")
            .and_then(parse_html_dimension_attr)
            .unwrap_or(inherited_stroke_w);

        if let Some(d) = svg_element_to_path_d(elem) {
            let resolved_fill = match fill {
                Some(explicit) => explicit,
                None if stroke.is_some() => None,
                None => Some(current_color),
            };
            out.push(SvgPathEntry {
                path_d: d,
                fill: resolved_fill,
                stroke: stroke.map(|c| (c, stroke_w)),
            });
        }
        collect_svg_paths_from_dom(doc, child, current_color, fill, stroke, stroke_w, out);
    }
}

/// Parses an HTML `srcset` attribute and returns the best candidate URL (preferring 1x/2x or
/// widest standard candidate without exceeding 2x density).
#[must_use]
pub fn parse_best_srcset_url(srcset: &str) -> Option<String> {
    let mut best: Option<(f32, String)> = None;
    for raw_candidate in srcset.split(',') {
        let trimmed = raw_candidate.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(url) = parts.next() else {
            continue;
        };
        let score = match parts.next() {
            Some(desc) if desc.ends_with('x') || desc.ends_with('X') => {
                desc[..desc.len() - 1].parse::<f32>().unwrap_or(1.0)
            }
            Some(desc) if desc.ends_with('w') || desc.ends_with('W') => {
                desc[..desc.len() - 1].parse::<f32>().unwrap_or(800.0) / 800.0
            }
            _ => 1.0,
        };
        let replace = match &best {
            None => true,
            Some((prev, _)) => {
                let d_cur = (score - 1.5).abs();
                let d_prev = (*prev - 1.5).abs();
                d_cur < d_prev || ((d_cur - d_prev).abs() < 1e-4 && score > *prev)
            }
        };
        if replace {
            best = Some((score, url.to_string()));
        }
    }
    best.map(|(_, url)| url)
}

/// Parses an HTML/SVG numeric attribute like `"120"` or `"120px"`.
#[must_use]
pub fn parse_html_dimension_attr(value: &str) -> Option<f32> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let stripped = trimmed
        .strip_suffix("px")
        .or_else(|| trimmed.strip_suffix("PX"))
        .unwrap_or(trimmed)
        .trim();
    stripped
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// Parses an inline SVG color attribute (`#rrggbb`, `#rgb`, `none`, `currentColor`, named basics).
#[must_use]
pub fn parse_svg_color(raw: &str, current_color: ColorRgba) -> Option<ColorRgba> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
        return None;
    }
    if trimmed.eq_ignore_ascii_case("currentColor") {
        return Some(current_color);
    }
    if let Some(hex) = trimmed.strip_prefix('#') {
        return match hex.len() {
            3 => {
                let r = u8::from_str_radix(&hex[0..1], 16).ok()?;
                let g = u8::from_str_radix(&hex[1..2], 16).ok()?;
                let b = u8::from_str_radix(&hex[2..3], 16).ok()?;
                Some(ColorRgba::rgb(r * 17, g * 17, b * 17))
            }
            6 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                Some(ColorRgba::rgb(r, g, b))
            }
            8 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
                Some(ColorRgba::rgba(r, g, b, a))
            }
            _ => None,
        };
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "black" => Some(ColorRgba::BLACK),
        "white" => Some(ColorRgba::WHITE),
        "red" => Some(ColorRgba::rgb(255, 0, 0)),
        "green" => Some(ColorRgba::rgb(0, 128, 0)),
        "blue" => Some(ColorRgba::rgb(0, 0, 255)),
        "gray" | "grey" => Some(ColorRgba::rgb(128, 128, 128)),
        "transparent" => Some(ColorRgba::TRANSPARENT),
        _ => None,
    }
}
