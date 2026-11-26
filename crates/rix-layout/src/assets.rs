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
    extract_svg_from_node(doc, svg_node, current_color)
}

/// Extracts vector paths and intrinsic dimensions from a specific `<svg>` element node in `doc`.
#[must_use]
pub fn extract_svg_from_node(
    doc: &Document,
    svg_node: NodeId,
    current_color: ColorRgba,
) -> Option<DecodedSvg> {
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
    let mut paths = Vec::new();
    let ctx = SvgExtractContext::build(doc, svg_node, current_color);
    let root_style = parse_inline_svg_style(elem.get_attribute("style").unwrap_or(""));
    let inherited_fill = elem
        .get_attribute("fill")
        .or_else(|| root_style.get("fill").copied())
        .map(|raw| ctx.resolve_paint(raw, current_color));
    let inherited_stroke = elem
        .get_attribute("stroke")
        .or_else(|| root_style.get("stroke").copied())
        .and_then(|raw| ctx.resolve_paint(raw, current_color));
    let inherited_stroke_w = elem
        .get_attribute("stroke-width")
        .or_else(|| root_style.get("stroke-width").copied())
        .and_then(parse_html_dimension_attr)
        .unwrap_or(1.5);
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
    let ctx = SvgExtractContext::build(doc, parent, current_color);
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
}

impl SvgExtractContext {
    fn build(doc: &Document, root: NodeId, current_color: ColorRgba) -> Self {
        let mut ctx = Self {
            by_id: HashMap::new(),
            gradient_colors: HashMap::new(),
            class_rules: HashMap::new(),
            tag_rules: HashMap::new(),
            id_rules: HashMap::new(),
        };
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
                    if let Some(color) = extract_gradient_first_stop(doc, nid, current_color) {
                        ctx.gradient_colors.insert(id.to_string(), color);
                    }
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

    let child_state = SvgInheritState {
        current_color: inherited.current_color,
        fill,
        stroke,
        stroke_w,
        opacity: elem_opacity,
        transform,
    };

    if tag.eq_ignore_ascii_case("use") {
        if use_depth < 4 {
            let href = elem
                .get_attribute("href")
                .or_else(|| elem.get_attribute("xlink:href"))
                .map(str::trim);
            if let Some(id) = href.and_then(|h| h.strip_prefix('#')) {
                if let Some(&target_id) = ctx.by_id.get(id) {
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
            });
        }
    }
    collect_svg_paths_inner(doc, node, ctx, child_state, use_depth, out);
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
