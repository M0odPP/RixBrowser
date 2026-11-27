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
    /// Accumulated 2D affine transform `[a, b, c, d, e, f]` (Kurbo / SVG matrix convention).
    pub transform: [f64; 6],
    /// SVG 2 `fill-rule`: `true` for `evenodd`, `false` for `nonzero` (default).
    pub fill_rule_even_odd: bool,
}

/// Decoded external SVG vector asset ready for `<img src="*.svg">` rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedSvg {
    pub view_box: Option<SvgViewBox>,
    pub intrinsic_size: Size,
    pub paths: Vec<SvgPathEntry>,
}

/// Maximum decoded raster texture dimension on either axis (caps 4K/8K photos to ~10 MiB RGBA8
/// while preserving exact original CSS `intrinsic_width`/`intrinsic_height` for layout & DOM).
pub const MAX_RASTER_TEX_DIM: u32 = 1600;

/// Decoded RGBA8 image ready for intrinsic sizing and GPU upload.
#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub image_id: ImageId,
    pub width: u32,
    pub height: u32,
    pub intrinsic_width: u32,
    pub intrinsic_height: u32,
    pub rgba: Arc<Vec<u8>>,
}

impl DecodedImage {
    #[must_use]
    pub fn intrinsic_size(&self) -> Size {
        Size::new(self.intrinsic_width as f32, self.intrinsic_height as f32)
    }
}

/// Decoded raw RGBA8 payload produced by parallel off-thread decoding before registry binding.
#[derive(Debug, Clone)]
pub struct DecodedRasterBytes {
    pub width: u32,
    pub height: u32,
    pub intrinsic_width: u32,
    pub intrinsic_height: u32,
    pub rgba: Arc<Vec<u8>>,
}

impl DecodedRasterBytes {
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.rgba.len()
    }
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

/// Decodes a single encoded raster image into an [`Arc`]-wrapped RGBA8 buffer, downscaling
/// oversized (> [`MAX_RASTER_TEX_DIM`]) textures to bound RAM/VRAM while preserving true
/// original `intrinsic_width` / `intrinsic_height` for CSS layout and `HTMLImageElement.naturalWidth`.
pub fn decode_single_raster(bytes: &[u8]) -> Result<DecodedRasterBytes, AssetError> {
    let dyn_img = image::load_from_memory(bytes)?;
    let (orig_w, orig_h) = (dyn_img.width(), dyn_img.height());
    if orig_w == 0 || orig_h == 0 {
        return Err(AssetError::InvalidRgbaSize {
            width: orig_w,
            height: orig_h,
            actual: 0,
        });
    }
    let rgba_img = if orig_w > MAX_RASTER_TEX_DIM || orig_h > MAX_RASTER_TEX_DIM {
        let scale = (MAX_RASTER_TEX_DIM as f64 / f64::from(orig_w.max(orig_h))).clamp(0.01, 1.0);
        let target_w = ((f64::from(orig_w) * scale).round() as u32).max(1);
        let target_h = ((f64::from(orig_h) * scale).round() as u32).max(1);
        dyn_img
            .resize_exact(target_w, target_h, image::imageops::FilterType::Triangle)
            .into_rgba8()
    } else {
        dyn_img.into_rgba8()
    };
    let (width, height) = rgba_img.dimensions();
    Ok(DecodedRasterBytes {
        width,
        height,
        intrinsic_width: orig_w,
        intrinsic_height: orig_h,
        rgba: Arc::new(rgba_img.into_raw()),
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
        self.register_rgba8_shared_with_intrinsic(
            src,
            decoded.width,
            decoded.height,
            decoded.intrinsic_width,
            decoded.intrinsic_height,
            decoded.rgba,
        )
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
        self.register_rgba8_shared_with_intrinsic(src, width, height, width, height, rgba)
    }

    /// Registers a shared `Arc<Vec<u8>>` RGBA8 image buffer with distinct original CSS intrinsic
    /// dimensions (when oversized textures were downscaled to cap RSS/VRAM).
    pub fn register_rgba8_shared_with_intrinsic(
        &mut self,
        src: impl Into<String>,
        width: u32,
        height: u32,
        intrinsic_width: u32,
        intrinsic_height: u32,
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
            intrinsic_width: intrinsic_width.max(1),
            intrinsic_height: intrinsic_height.max(1),
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
    extract_svg_from_node(doc, svg_node, current_color)
}

/// Extracts vector paths and intrinsic dimensions from a specific `<svg>` element node in `doc`.
#[must_use]
pub fn extract_svg_from_node(
    doc: &Document,
    svg_node: NodeId,
    current_color: ColorRgba,
) -> Option<DecodedSvg> {
    extract_svg_from_node_with_css(doc, svg_node, current_color, None, None, false)
}

/// Extracts vector paths from `<svg>` applying inherited CSS presentation properties (`fill`,
/// `stroke`, `fill-rule`) from the host stylesheet per SVG 2 §13.
#[must_use]
pub fn extract_svg_from_node_with_css(
    doc: &Document,
    svg_node: NodeId,
    current_color: ColorRgba,
    css_fill: Option<Option<ColorRgba>>,
    css_stroke: Option<Option<ColorRgba>>,
    css_fill_rule_even_odd: bool,
) -> Option<DecodedSvg> {
    extract_svg_from_node_with_css_and_viewport(
        doc,
        svg_node,
        current_color,
        css_fill,
        css_stroke,
        css_fill_rule_even_odd,
        None,
    )
}

/// Extracts vector paths from `<svg>` with an explicit resolved host viewport `(iw, ih)` so
/// `<use href="#sym">` referencing `<symbol viewBox="...">` scales accurately per SVG 2 §5.7.1.
#[must_use]
pub fn extract_svg_from_node_with_css_and_viewport(
    doc: &Document,
    svg_node: NodeId,
    current_color: ColorRgba,
    css_fill: Option<Option<ColorRgba>>,
    css_stroke: Option<Option<ColorRgba>>,
    css_fill_rule_even_odd: bool,
    resolved_viewport: Option<Size>,
) -> Option<DecodedSvg> {
    let Some(NodeData::Element(elem)) = doc.get(svg_node) else {
        return None;
    };
    let view_box = elem
        .get_attribute("viewBox")
        .or_else(|| elem.get_attribute("viewbox"))
        .and_then(SvgViewBox::parse);
    let attr_w = elem
        .get_attribute("width")
        .and_then(parse_html_dimension_attr);
    let attr_h = elem
        .get_attribute("height")
        .and_then(parse_html_dimension_attr);
    let iw = resolved_viewport
        .map(|s| s.width)
        .or(attr_w)
        .or_else(|| view_box.map(|vb| vb.width))
        .unwrap_or(32.0)
        .max(1.0);
    let ih = resolved_viewport
        .map(|s| s.height)
        .or(attr_h)
        .or_else(|| view_box.map(|vb| vb.height))
        .unwrap_or(32.0)
        .max(1.0);
    let mut paths = Vec::new();
    let coord_w = view_box.map_or(iw, |vb| vb.width.max(1.0));
    let coord_h = view_box.map_or(ih, |vb| vb.height.max(1.0));
    let ctx = SvgExtractContext::build(doc, svg_node, current_color, coord_w, coord_h);
    let root_style = parse_inline_svg_style(elem.get_attribute("style").unwrap_or(""));
    let inherited_fill = root_style
        .get("fill")
        .copied()
        .map(|raw| ctx.resolve_paint(raw, current_color))
        .or(css_fill)
        .or_else(|| {
            elem.get_attribute("fill")
                .map(|raw| ctx.resolve_paint(raw, current_color))
        });
    let inherited_stroke = root_style
        .get("stroke")
        .copied()
        .map(|raw| ctx.resolve_paint(raw, current_color))
        .or(css_stroke)
        .or_else(|| {
            elem.get_attribute("stroke")
                .map(|raw| ctx.resolve_paint(raw, current_color))
        })
        .flatten();
    let inherited_stroke_w = root_style
        .get("stroke-width")
        .copied()
        .or_else(|| elem.get_attribute("stroke-width"))
        .and_then(parse_html_dimension_attr)
        .unwrap_or(1.5);
    let inherited_fill_rule_even_odd = root_style
        .get("fill-rule")
        .copied()
        .map(|v| v.trim().eq_ignore_ascii_case("evenodd"))
        .or(if css_fill_rule_even_odd {
            Some(true)
        } else {
            None
        })
        .or_else(|| {
            elem.get_attribute("fill-rule")
                .map(|v| v.trim().eq_ignore_ascii_case("evenodd"))
        })
        .unwrap_or(false);
    let root_opacity = parse_opacity_attr(
        root_style
            .get("opacity")
            .copied()
            .or_else(|| elem.get_attribute("opacity")),
    );
    let root_xf = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    collect_svg_paths_inner(
        doc,
        svg_node,
        &ctx,
        SvgInheritState {
            current_color,
            fill: inherited_fill,
            stroke: inherited_stroke,
            stroke_w: inherited_stroke_w,
            opacity: root_opacity,
            transform: root_xf,
            fill_rule_even_odd: inherited_fill_rule_even_odd,
        },
        0,
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
    let ctx = SvgExtractContext::build(doc, parent, current_color, 32.0, 32.0);
    collect_svg_paths_inner(
        doc,
        parent,
        &ctx,
        SvgInheritState {
            current_color,
            fill: inherited_fill,
            stroke: inherited_stroke,
            stroke_w: inherited_stroke_w,
            opacity: 1.0,
            transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            fill_rule_even_odd: false,
        },
        0,
        out,
    );
}

#[derive(Clone, Copy)]
struct SvgInheritState {
    current_color: ColorRgba,
    fill: Option<Option<ColorRgba>>,
    stroke: Option<ColorRgba>,
    stroke_w: f32,
    opacity: f32,
    transform: [f64; 6],
    fill_rule_even_odd: bool,
}

#[derive(Default)]
struct SvgStyleRule {
    fill: Option<String>,
    stroke: Option<String>,
    stroke_width: Option<String>,
    opacity: Option<String>,
    fill_opacity: Option<String>,
    display_none: bool,
}

struct SvgExtractContext {
    by_id: HashMap<String, NodeId>,
    gradient_colors: HashMap<String, ColorRgba>,
    class_rules: HashMap<String, SvgStyleRule>,
    tag_rules: HashMap<String, SvgStyleRule>,
    id_rules: HashMap<String, SvgStyleRule>,
    viewport_w: f32,
    viewport_h: f32,
}

impl SvgExtractContext {
    fn build(
        doc: &Document,
        root: NodeId,
        current_color: ColorRgba,
        viewport_w: f32,
        viewport_h: f32,
    ) -> Self {
        let mut ctx = Self {
            by_id: HashMap::new(),
            gradient_colors: HashMap::new(),
            class_rules: HashMap::new(),
            tag_rules: HashMap::new(),
            id_rules: HashMap::new(),
            viewport_w: viewport_w.max(1.0),
            viewport_h: viewport_h.max(1.0),
        };
        let mut grad_nodes: Vec<(String, NodeId)> = Vec::new();
        for nid in doc.descendants(root) {
            let Some(NodeData::Element(el)) = doc.get(nid) else {
                continue;
            };
            let tag = el.name.local.as_str();
            if let Some(id) = el
                .get_attribute("id")
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                ctx.by_id.insert(id.to_string(), nid);
                if tag.eq_ignore_ascii_case("lineargradient")
                    || tag.eq_ignore_ascii_case("radialgradient")
                {
                    grad_nodes.push((id.to_string(), nid));
                }
            }
            if tag.eq_ignore_ascii_case("style") {
                let mut css = String::new();
                for ch in doc.children(nid) {
                    if let Some(NodeData::Text(t)) = doc.get(ch) {
                        css.push_str(t);
                    }
                }
                parse_svg_style_sheet(&css, &mut ctx);
            }
        }
        // SVG 2 §14.2: Resolve `<linearGradient>`/`<radialGradient>` stops including `href` /
        // `xlink:href` template inheritance chains (used extensively by Inkscape/Illustrator/Figma).
        for (id, nid) in grad_nodes {
            if let Some(color) =
                extract_gradient_stop_with_href(doc, nid, &ctx.by_id, current_color, 0)
            {
                ctx.gradient_colors.insert(id, color);
            }
        }
        ctx
    }

    fn resolve_paint(&self, raw: &str, current_color: ColorRgba) -> Option<ColorRgba> {
        let trimmed = raw.trim();
        if let Some(rest) = trimmed
            .strip_prefix("url(")
            .or_else(|| trimmed.strip_prefix("URL("))
        {
            if let Some(end) = rest.find(')') {
                let target = rest[..end].trim().trim_matches('"').trim_matches('\'');
                if let Some(id) = target.strip_prefix('#') {
                    return self.gradient_colors.get(id).copied();
                }
            }
            return None;
        }
        parse_svg_color(trimmed, current_color)
    }
}

fn extract_gradient_stop_with_href(
    doc: &Document,
    grad_node: NodeId,
    by_id: &HashMap<String, NodeId>,
    current_color: ColorRgba,
    depth: u8,
) -> Option<ColorRgba> {
    if let Some(c) = extract_gradient_first_stop(doc, grad_node, current_color) {
        return Some(c);
    }
    if depth >= 4 {
        return None;
    }
    let Some(NodeData::Element(el)) = doc.get(grad_node) else {
        return None;
    };
    let href = el
        .get_attribute("href")
        .or_else(|| el.get_attribute("xlink:href"))
        .map(str::trim)?;
    let target_id = href.strip_prefix('#')?;
    let &target_node = by_id.get(target_id)?;
    if target_node == grad_node {
        return None;
    }
    extract_gradient_stop_with_href(doc, target_node, by_id, current_color, depth + 1)
}

fn extract_gradient_first_stop(
    doc: &Document,
    grad_node: NodeId,
    current_color: ColorRgba,
) -> Option<ColorRgba> {
    for ch in doc.children(grad_node) {
        let Some(NodeData::Element(el)) = doc.get(ch) else {
            continue;
        };
        if !el.name.local.as_str().eq_ignore_ascii_case("stop") {
            continue;
        }
        let inline = parse_inline_svg_style(el.get_attribute("style").unwrap_or(""));
        let raw_color = inline
            .get("stop-color")
            .copied()
            .or_else(|| el.get_attribute("stop-color"));
        if let Some(raw) = raw_color {
            if let Some(mut c) = parse_svg_color(raw, current_color) {
                let op = parse_opacity_attr(
                    inline
                        .get("stop-opacity")
                        .copied()
                        .or_else(|| el.get_attribute("stop-opacity")),
                );
                c.a = ((f32::from(c.a) * op).round().clamp(0.0, 255.0)) as u8;
                return Some(c);
            }
        }
    }
    None
}

fn parse_svg_style_sheet(css: &str, ctx: &mut SvgExtractContext) {
    // Strip /* ... */ comments cleanly before splitting rule blocks.
    let mut cleaned = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        cleaned.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        if let Some(end) = after.find("*/") {
            rest = &after[end + 2..];
        } else {
            rest = "";
            break;
        }
    }
    cleaned.push_str(rest);

    for block in cleaned.split('}') {
        let Some((sel_part, decl_part)) = block.split_once('{') else {
            continue;
        };
        let props = parse_inline_svg_style(decl_part);
        if props.is_empty() {
            continue;
        }
        for sel in sel_part.split(',') {
            let s = sel.trim();
            if s.is_empty() || s.starts_with('@') {
                continue;
            }
            let target = if let Some(cls) = s.strip_prefix('.') {
                if cls.contains(|c: char| c.is_ascii_whitespace() || c == ':' || c == '[') {
                    continue;
                }
                ctx.class_rules.entry(cls.to_string()).or_default()
            } else if let Some(id) = s.strip_prefix('#') {
                if id.contains(|c: char| c.is_ascii_whitespace() || c == ':' || c == '[') {
                    continue;
                }
                ctx.id_rules.entry(id.to_string()).or_default()
            } else if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                ctx.tag_rules.entry(s.to_ascii_lowercase()).or_default()
            } else {
                continue;
            };
            if let Some(v) = props.get("fill") {
                target.fill = Some((*v).to_string());
            }
            if let Some(v) = props.get("stroke") {
                target.stroke = Some((*v).to_string());
            }
            if let Some(v) = props.get("stroke-width") {
                target.stroke_width = Some((*v).to_string());
            }
            if let Some(v) = props.get("opacity") {
                target.opacity = Some((*v).to_string());
            }
            if let Some(v) = props.get("fill-opacity") {
                target.fill_opacity = Some((*v).to_string());
            }
            if props
                .get("display")
                .is_some_and(|v| v.eq_ignore_ascii_case("none"))
            {
                target.display_none = true;
            }
        }
    }
}

fn parse_inline_svg_style(raw: &str) -> HashMap<&str, &str> {
    let mut map = HashMap::new();
    for part in raw.split(';') {
        if let Some((k, v)) = part.split_once(':') {
            let k = k.trim();
            let v = v.trim().trim_end_matches("!important").trim();
            if !k.is_empty() && !v.is_empty() {
                map.insert(k, v);
            }
        }
    }
    map
}

fn parse_opacity_attr(raw: Option<&str>) -> f32 {
    raw.and_then(|s| s.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .map_or(1.0, |v| v.clamp(0.0, 1.0))
}

fn scale_alpha(color: ColorRgba, alpha: f32) -> ColorRgba {
    if alpha >= 0.999 {
        return color;
    }
    ColorRgba::rgba(
        color.r,
        color.g,
        color.b,
        ((f32::from(color.a) * alpha.clamp(0.0, 1.0)).round()) as u8,
    )
}

fn is_non_rendering_svg_container(tag: &str) -> bool {
    matches!(
        tag.to_ascii_lowercase().as_str(),
        "defs"
            | "clippath"
            | "mask"
            | "symbol"
            | "pattern"
            | "lineargradient"
            | "radialgradient"
            | "filter"
            | "marker"
            | "metadata"
            | "title"
            | "desc"
            | "style"
            | "script"
    )
}

#[inline]
fn affine_mul(lhs: [f64; 6], rhs: [f64; 6]) -> [f64; 6] {
    [
        lhs[0] * rhs[0] + lhs[2] * rhs[1],
        lhs[1] * rhs[0] + lhs[3] * rhs[1],
        lhs[0] * rhs[2] + lhs[2] * rhs[3],
        lhs[1] * rhs[2] + lhs[3] * rhs[3],
        lhs[0] * rhs[4] + lhs[2] * rhs[5] + lhs[4],
        lhs[1] * rhs[4] + lhs[3] * rhs[5] + lhs[5],
    ]
}

fn parse_svg_transform(raw: &str) -> [f64; 6] {
    let mut acc = [1.0f64, 0.0, 0.0, 1.0, 0.0, 0.0];
    let mut rest = raw.trim();
    while !rest.is_empty() {
        let Some(open) = rest.find('(') else {
            break;
        };
        let fn_name = rest[..open].trim().trim_start_matches(',').trim();
        let after = &rest[open + 1..];
        let Some(close) = after.find(')') else {
            break;
        };
        let args_str = &after[..close];
        rest = after[close + 1..].trim();

        let nums: Vec<f64> = args_str
            .split(|c: char| c.is_ascii_whitespace() || c == ',')
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .collect();

        let step = match fn_name.to_ascii_lowercase().as_str() {
            "translate" if !nums.is_empty() => {
                let tx = nums[0];
                let ty = nums.get(1).copied().unwrap_or(0.0);
                [1.0, 0.0, 0.0, 1.0, tx, ty]
            }
            "scale" if !nums.is_empty() => {
                let sx = nums[0];
                let sy = nums.get(1).copied().unwrap_or(sx);
                [sx, 0.0, 0.0, sy, 0.0, 0.0]
            }
            "rotate" if !nums.is_empty() => {
                let rad = nums[0].to_radians();
                let (s, c) = rad.sin_cos();
                let rot = [c, s, -s, c, 0.0, 0.0];
                if nums.len() >= 3 {
                    let cx = nums[1];
                    let cy = nums[2];
                    affine_mul(
                        affine_mul([1.0, 0.0, 0.0, 1.0, cx, cy], rot),
                        [1.0, 0.0, 0.0, 1.0, -cx, -cy],
                    )
                } else {
                    rot
                }
            }
            "skewx" if !nums.is_empty() => {
                let t = nums[0].to_radians().tan();
                [1.0, 0.0, t, 1.0, 0.0, 0.0]
            }
            "skewy" if !nums.is_empty() => {
                let t = nums[0].to_radians().tan();
                [1.0, t, 0.0, 1.0, 0.0, 0.0]
            }
            "matrix" if nums.len() >= 6 => [nums[0], nums[1], nums[2], nums[3], nums[4], nums[5]],
            _ => continue,
        };
        acc = affine_mul(acc, step);
    }
    acc
}

fn collect_svg_paths_inner(
    doc: &Document,
    parent: NodeId,
    ctx: &SvgExtractContext,
    inherited: SvgInheritState,
    use_depth: u8,
    out: &mut Vec<SvgPathEntry>,
) {
    for child in doc.children(parent) {
        collect_svg_element(doc, child, ctx, inherited, use_depth, out);
    }
}

fn collect_svg_element(
    doc: &Document,
    node: NodeId,
    ctx: &SvgExtractContext,
    inherited: SvgInheritState,
    use_depth: u8,
    out: &mut Vec<SvgPathEntry>,
) {
    let Some(NodeData::Element(elem)) = doc.get(node) else {
        return;
    };
    let tag = elem.name.local.as_str();
    if is_non_rendering_svg_container(tag) {
        return;
    }

    let inline = parse_inline_svg_style(elem.get_attribute("style").unwrap_or(""));
    if elem
        .get_attribute("display")
        .or_else(|| inline.get("display").copied())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("none"))
        || elem
            .get_attribute("visibility")
            .or_else(|| inline.get("visibility").copied())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("hidden"))
    {
        return;
    }

    // Gather matched `<style>` rules (tag -> class -> id order).
    let mut matched_rules: Vec<&SvgStyleRule> = Vec::new();
    if let Some(r) = ctx.tag_rules.get(&tag.to_ascii_lowercase()) {
        matched_rules.push(r);
    }
    if let Some(classes) = elem.get_attribute("class") {
        for cls in classes.split_ascii_whitespace() {
            if let Some(r) = ctx.class_rules.get(cls) {
                matched_rules.push(r);
            }
        }
    }
    if let Some(id) = elem.get_attribute("id") {
        if let Some(r) = ctx.id_rules.get(id.trim()) {
            matched_rules.push(r);
        }
    }
    let mut css_fill: Option<&str> = None;
    let mut css_stroke: Option<&str> = None;
    let mut css_stroke_w: Option<&str> = None;
    let mut css_opacity: Option<&str> = None;
    let mut css_fill_opacity: Option<&str> = None;
    let mut css_display_none = false;
    for rule in matched_rules {
        if let Some(ref f) = rule.fill {
            css_fill = Some(f.as_str());
        }
        if let Some(ref s) = rule.stroke {
            css_stroke = Some(s.as_str());
        }
        if let Some(ref sw) = rule.stroke_width {
            css_stroke_w = Some(sw.as_str());
        }
        if let Some(ref op) = rule.opacity {
            css_opacity = Some(op.as_str());
        }
        if let Some(ref fop) = rule.fill_opacity {
            css_fill_opacity = Some(fop.as_str());
        }
        if rule.display_none {
            css_display_none = true;
        }
    }
    if css_display_none {
        return;
    }

    let raw_fill = inline
        .get("fill")
        .copied()
        .or(css_fill)
        .or_else(|| elem.get_attribute("fill"));
    let fill = match raw_fill {
        Some(raw) => Some(ctx.resolve_paint(raw, inherited.current_color)),
        None => inherited.fill,
    };
    let raw_stroke = inline
        .get("stroke")
        .copied()
        .or(css_stroke)
        .or_else(|| elem.get_attribute("stroke"));
    let stroke = match raw_stroke {
        Some(raw) => ctx.resolve_paint(raw, inherited.current_color),
        None => inherited.stroke,
    };
    let stroke_w = inline
        .get("stroke-width")
        .copied()
        .or(css_stroke_w)
        .or_else(|| elem.get_attribute("stroke-width"))
        .and_then(parse_html_dimension_attr)
        .unwrap_or(inherited.stroke_w);

    let elem_opacity = inherited.opacity
        * parse_opacity_attr(
            inline
                .get("opacity")
                .copied()
                .or(css_opacity)
                .or_else(|| elem.get_attribute("opacity")),
        );
    let fill_opacity = elem_opacity
        * parse_opacity_attr(
            inline
                .get("fill-opacity")
                .copied()
                .or(css_fill_opacity)
                .or_else(|| elem.get_attribute("fill-opacity")),
        );
    let stroke_opacity = elem_opacity
        * parse_opacity_attr(
            inline
                .get("stroke-opacity")
                .copied()
                .or_else(|| elem.get_attribute("stroke-opacity")),
        );

    let mut transform = inherited.transform;
    if let Some(tf_raw) = elem.get_attribute("transform") {
        transform = affine_mul(transform, parse_svg_transform(tf_raw));
    }

    let fill_rule_even_odd = inline
        .get("fill-rule")
        .copied()
        .or_else(|| elem.get_attribute("fill-rule"))
        .map_or(inherited.fill_rule_even_odd, |v| {
            v.trim().eq_ignore_ascii_case("evenodd")
        });

    let child_state = SvgInheritState {
        current_color: inherited.current_color,
        fill,
        stroke,
        stroke_w,
        opacity: elem_opacity,
        transform,
        fill_rule_even_odd,
    };

    if tag.eq_ignore_ascii_case("use") {
        if use_depth < 4 {
            let href = elem
                .get_attribute("href")
                .or_else(|| elem.get_attribute("xlink:href"))
                .map(str::trim);
            if let Some(id) = href.and_then(|h| h.strip_prefix('#')) {
                let target_opt = ctx.by_id.get(id).copied().or_else(|| {
                    doc.descendants(doc.root()).find(|&nid| {
                        matches!(
                            doc.get(nid),
                            Some(NodeData::Element(e))
                                if e.get_attribute("id").map(str::trim) == Some(id)
                        )
                    })
                });
                if let Some(target_id) = target_opt {
                    let ux = elem
                        .get_attribute("x")
                        .and_then(parse_html_dimension_attr)
                        .unwrap_or(0.0);
                    let uy = elem
                        .get_attribute("y")
                        .and_then(parse_html_dimension_attr)
                        .unwrap_or(0.0);
                    let mut use_state = child_state;
                    if ux != 0.0 || uy != 0.0 {
                        use_state.transform = affine_mul(
                            use_state.transform,
                            [1.0, 0.0, 0.0, 1.0, f64::from(ux), f64::from(uy)],
                        );
                    }
                    if let Some(NodeData::Element(target_el)) = doc.get(target_id) {
                        if target_el.name.local.as_str().eq_ignore_ascii_case("symbol") {
                            // SVG 2 §5.7.1: `<use>` instantiating `<symbol viewBox="...">`
                            // maps the symbol's `viewBox` into the `<use>` viewport (defaulting
                            // to 100% of the host `<svg>` coordinate space).
                            if let Some(sym_vb) = target_el
                                .get_attribute("viewBox")
                                .or_else(|| target_el.get_attribute("viewbox"))
                                .and_then(SvgViewBox::parse)
                            {
                                if sym_vb.width > 0.0 && sym_vb.height > 0.0 {
                                    let uw = elem
                                        .get_attribute("width")
                                        .and_then(parse_html_dimension_attr)
                                        .unwrap_or(ctx.viewport_w);
                                    let uh = elem
                                        .get_attribute("height")
                                        .and_then(parse_html_dimension_attr)
                                        .unwrap_or(ctx.viewport_h);
                                    let sx = f64::from(uw / sym_vb.width);
                                    let sy = f64::from(uh / sym_vb.height);
                                    let scale = sx.min(sy);
                                    let tx = -f64::from(sym_vb.min_x) * scale
                                        + (f64::from(uw) - f64::from(sym_vb.width) * scale) * 0.5;
                                    let ty = -f64::from(sym_vb.min_y) * scale
                                        + (f64::from(uh) - f64::from(sym_vb.height) * scale) * 0.5;
                                    use_state.transform = affine_mul(
                                        use_state.transform,
                                        [scale, 0.0, 0.0, scale, tx, ty],
                                    );
                                }
                            }
                            collect_svg_paths_inner(
                                doc,
                                target_id,
                                ctx,
                                use_state,
                                use_depth + 1,
                                out,
                            );
                        } else {
                            collect_svg_element(doc, target_id, ctx, use_state, use_depth + 1, out);
                        }
                    }
                }
            }
        }
        return;
    }

    if let Some(d) = svg_element_to_path_d(elem) {
        let resolved_fill = match fill {
            Some(explicit) => explicit,
            None if stroke.is_some() => None,
            None => Some(inherited.current_color),
        }
        .map(|c| scale_alpha(c, fill_opacity))
        .filter(|c| c.a > 0);
        let resolved_stroke = stroke
            .map(|c| (scale_alpha(c, stroke_opacity), stroke_w))
            .filter(|(c, w)| c.a > 0 && *w > 0.0);
        if resolved_fill.is_some() || resolved_stroke.is_some() {
            out.push(SvgPathEntry {
                path_d: d,
                fill: resolved_fill,
                stroke: resolved_stroke,
                transform,
                fill_rule_even_odd,
            });
        }
    }
    collect_svg_paths_inner(doc, node, ctx, child_state, use_depth, out);
}

/// Returns `true` if `mime` is an image format supported by our raster/SVG decoder pipeline.
#[must_use]
pub fn is_supported_image_mime(mime: &str) -> bool {
    let lower = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "image/png"
            | "image/jpeg"
            | "image/jpg"
            | "image/webp"
            | "image/gif"
            | "image/svg+xml"
            | "image/apng"
    )
}

/// Resolved `<picture><source>` match per HTML Living Standard §4.8.4.2.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectedPictureSource {
    pub url: String,
    pub attr_width: Option<f32>,
    pub attr_height: Option<f32>,
}

/// HTML Living Standard §4.8.4.2 ("update the source set"): if `img_node`'s parent is a
/// `<picture>` element, inspects preceding `<source>` siblings in DOM order and returns the
/// first candidate whose `media` query matches `media`, whose `type` (if present) is a
/// supported image MIME type, and whose `srcset` yields a valid URL.
#[must_use]
pub fn select_picture_source_for_img(
    doc: &Document,
    img_node: NodeId,
    media: &rix_css::MediaContext,
) -> Option<SelectedPictureSource> {
    let parent = doc.parent(img_node)?;
    let is_picture = matches!(
        doc.get(parent),
        Some(NodeData::Element(pe)) if pe.name.local.as_str().eq_ignore_ascii_case("picture")
    );
    if !is_picture {
        return None;
    }
    for sib in doc.children(parent) {
        if sib == img_node {
            break;
        }
        let Some(NodeData::Element(se)) = doc.get(sib) else {
            continue;
        };
        if !se.name.local.as_str().eq_ignore_ascii_case("source") {
            continue;
        }
        let media_ok = se
            .get_attribute("media")
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .is_none_or(|m| media.evaluate_media_query_str(m));
        if !media_ok {
            continue;
        }
        let type_ok = se
            .get_attribute("type")
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .is_none_or(is_supported_image_mime);
        if !type_ok {
            continue;
        }
        let Some(srcset) = se.get_attribute("srcset") else {
            continue;
        };
        if let Some(url) = parse_best_srcset_url(srcset) {
            let attr_width = se
                .get_attribute("width")
                .and_then(parse_html_dimension_attr);
            let attr_height = se
                .get_attribute("height")
                .and_then(parse_html_dimension_attr);
            return Some(SelectedPictureSource {
                url,
                attr_width,
                attr_height,
            });
        }
    }
    None
}

/// Parses an HTML `srcset` attribute per WHATWG HTML §4.8.4.3.7 (preserving embedded commas
/// inside URLs such as Cloudflare Image Resizing paths and `data:` URIs) and returns the best
/// candidate URL (preferring 1x–2x density or closest standard width).
#[must_use]
pub fn parse_best_srcset_url(srcset: &str) -> Option<String> {
    let candidates = rix_dom::parse_srcset(srcset);
    let mut best: Option<(f32, &str)> = None;
    for cand in candidates {
        let score = if let Some(d) = cand.density {
            d
        } else if let Some(w) = cand.width {
            (w as f32) / 800.0
        } else {
            1.0
        };
        let replace = match best {
            None => true,
            Some((prev, _)) => {
                let d_cur = (score - 1.5).abs();
                let d_prev = (prev - 1.5).abs();
                d_cur < d_prev || ((d_cur - d_prev).abs() < 1e-4 && score > prev)
            }
        };
        if replace {
            best = Some((score, cand.url));
        }
    }
    best.map(|(_, url)| url.to_string())
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

/// Parses an SVG 2 §4.2 presentation length attribute (`width`/`height`) supporting unitless
/// user units as well as CSS `<length>` units (`px`, `em`, `rem`, `ex`, `ch`, `pt`, `pc`,
/// `cm`, `mm`, `in`, `vw`, `vh`, `vmin`, `vmax`) against `eval`.
///
/// Percentage attributes (`"100%"`) return `None` so intrinsic sizing falls back to `viewBox`
/// aspect ratio while CSS layout resolves percentage sizing against the containing block.
#[must_use]
pub fn parse_svg_dimension_attr(value: &str, eval: &rix_css::EvalContext) -> Option<f32> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.ends_with('%') {
        return None;
    }
    if let Some(px) = parse_html_dimension_attr(trimmed) {
        return Some(px);
    }
    let split = trimmed
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '+' || c == '-'))
        .unwrap_or(trimmed.len());
    let num: f32 = trimmed[..split].trim().parse().ok()?;
    if !num.is_finite() || num < 0.0 {
        return None;
    }
    let unit = trimmed[split..].trim().to_ascii_lowercase();
    let px = match unit.as_str() {
        "" | "px" => num,
        "em" => num * eval.font_size_px,
        "rem" => num * eval.root_font_size_px,
        "ex" => num * eval.font_size_px * 0.5,
        "ch" => num * eval.font_size_px * 0.5,
        "pt" => num * (96.0 / 72.0),
        "pc" => num * 16.0,
        "in" => num * 96.0,
        "cm" => num * (96.0 / 2.54),
        "mm" => num * (96.0 / 25.4),
        "q" => num * (96.0 / 101.6),
        "vw" | "vi" | "svw" | "lvw" | "dvw" => num * eval.viewport_width_px * 0.01,
        "vh" | "vb" | "svh" | "lvh" | "dvh" => num * eval.viewport_height_px * 0.01,
        "vmin" => num * eval.viewport_width_px.min(eval.viewport_height_px) * 0.01,
        "vmax" => num * eval.viewport_width_px.max(eval.viewport_height_px) * 0.01,
        _ => return None,
    };
    px.is_finite().then_some(px.max(0.0))
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
    if let Some(rgb_inner) = trimmed
        .strip_prefix("rgb(")
        .or_else(|| trimmed.strip_prefix("rgba("))
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = rgb_inner
            .split(|c: char| c == ',' || c == '/' || c.is_ascii_whitespace())
            .filter(|s| !s.is_empty())
            .collect();
        if parts.len() >= 3 {
            let parse_ch = |s: &str| -> Option<u8> {
                if let Some(pct) = s.strip_suffix('%') {
                    let v = pct.parse::<f32>().ok()?;
                    Some(((v / 100.0) * 255.0).round().clamp(0.0, 255.0) as u8)
                } else {
                    Some(s.parse::<f32>().ok()?.round().clamp(0.0, 255.0) as u8)
                }
            };
            let (Some(r), Some(g), Some(b)) =
                (parse_ch(parts[0]), parse_ch(parts[1]), parse_ch(parts[2]))
            else {
                return None;
            };
            let a = if let Some(a_str) = parts.get(3) {
                if let Some(pct) = a_str.strip_suffix('%') {
                    ((pct.parse::<f32>().ok()? / 100.0) * 255.0)
                        .round()
                        .clamp(0.0, 255.0) as u8
                } else {
                    (a_str.parse::<f32>().ok()? * 255.0)
                        .round()
                        .clamp(0.0, 255.0) as u8
                }
            } else {
                255
            };
            return Some(ColorRgba::rgba(r, g, b, a));
        }
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "black" => Some(ColorRgba::BLACK),
        "white" => Some(ColorRgba::WHITE),
        "red" => Some(ColorRgba::rgb(255, 0, 0)),
        "green" => Some(ColorRgba::rgb(0, 128, 0)),
        "lime" => Some(ColorRgba::rgb(0, 255, 0)),
        "blue" => Some(ColorRgba::rgb(0, 0, 255)),
        "navy" => Some(ColorRgba::rgb(0, 0, 128)),
        "teal" => Some(ColorRgba::rgb(0, 128, 128)),
        "aqua" | "cyan" => Some(ColorRgba::rgb(0, 255, 255)),
        "yellow" => Some(ColorRgba::rgb(255, 255, 0)),
        "orange" => Some(ColorRgba::rgb(255, 165, 0)),
        "purple" => Some(ColorRgba::rgb(128, 0, 128)),
        "fuchsia" | "magenta" => Some(ColorRgba::rgb(255, 0, 255)),
        "maroon" => Some(ColorRgba::rgb(128, 0, 0)),
        "olive" => Some(ColorRgba::rgb(128, 128, 0)),
        "silver" => Some(ColorRgba::rgb(192, 192, 192)),
        "lightgray" | "lightgrey" => Some(ColorRgba::rgb(211, 211, 211)),
        "darkgray" | "darkgrey" => Some(ColorRgba::rgb(169, 169, 169)),
        "gray" | "grey" => Some(ColorRgba::rgb(128, 128, 128)),
        "transparent" => Some(ColorRgba::TRANSPARENT),
        _ => None,
    }
}

#[cfg(test)]
mod stage1_asset_tests {
    use super::*;

    #[test]
    fn oversized_raster_is_downscaled_while_preserving_intrinsic_dimensions() {
        let mut png_bytes = Vec::new();
        let img = image::RgbaImage::from_pixel(3200, 1600, image::Rgba([24, 128, 220, 255]));
        {
            use image::ImageEncoder;
            let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
            encoder
                .write_image(&img, 3200, 1600, image::ExtendedColorType::Rgba8)
                .unwrap();
        }
        let decoded = decode_single_raster(&png_bytes).unwrap();
        assert_eq!(decoded.intrinsic_width, 3200);
        assert_eq!(decoded.intrinsic_height, 1600);
        assert!(decoded.width <= MAX_RASTER_TEX_DIM);
        assert!(decoded.height <= MAX_RASTER_TEX_DIM);
        assert_eq!(decoded.width, 1600);
        assert_eq!(decoded.height, 800);

        let mut reg = AssetRegistry::new();
        reg.register_encoded("hero.png", &png_bytes).unwrap();
        let entry = reg.get_by_src("hero.png").unwrap();
        assert_eq!(entry.intrinsic_size(), Size::new(3200.0, 1600.0));
    }
}
