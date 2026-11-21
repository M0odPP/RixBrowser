//! Decoded image assets and inline SVG path extraction for layout and paint.

use std::collections::HashMap;
use std::sync::Arc;

use rix_core::Size;
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

/// Per-document registry of decoded raster images (`png`, `jpeg`, `webp`, `gif`).
#[derive(Debug, Default)]
pub struct AssetRegistry {
    by_src: HashMap<String, DecodedImage>,
    by_id: HashMap<ImageId, DecodedImage>,
    next_image_id: u64,
}

impl AssetRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_src: HashMap::new(),
            by_id: HashMap::new(),
            next_image_id: 1,
        }
    }

    /// Decodes an encoded image (`png`, `jpeg`, `webp`, `gif`) and binds it to `src`.
    pub fn register_encoded(
        &mut self,
        src: impl Into<String>,
        bytes: &[u8],
    ) -> Result<ImageId, AssetError> {
        let decoded = image::load_from_memory(bytes)?.into_rgba8();
        let (width, height) = decoded.dimensions();
        self.register_rgba8(src, width, height, decoded.into_raw())
    }

    /// Registers a pre-decoded RGBA8 image buffer under `src`.
    pub fn register_rgba8(
        &mut self,
        src: impl Into<String>,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
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
            rgba: Arc::new(rgba),
        };
        let src_key = src.into();
        self.by_id.insert(image_id, entry.clone());
        self.by_src.insert(src_key, entry);
        Ok(image_id)
    }

    #[must_use]
    pub fn get_by_src(&self, src: &str) -> Option<&DecodedImage> {
        self.by_src.get(src)
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
        self.by_id.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
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
pub fn parse_svg_color(raw: &str, current_color: rix_css::ColorRgba) -> Option<rix_css::ColorRgba> {
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
                Some(rix_css::ColorRgba::rgb(r * 17, g * 17, b * 17))
            }
            6 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                Some(rix_css::ColorRgba::rgb(r, g, b))
            }
            8 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
                Some(rix_css::ColorRgba::rgba(r, g, b, a))
            }
            _ => None,
        };
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "black" => Some(rix_css::ColorRgba::BLACK),
        "white" => Some(rix_css::ColorRgba::WHITE),
        "red" => Some(rix_css::ColorRgba::rgb(255, 0, 0)),
        "green" => Some(rix_css::ColorRgba::rgb(0, 128, 0)),
        "blue" => Some(rix_css::ColorRgba::rgb(0, 0, 255)),
        "gray" | "grey" => Some(rix_css::ColorRgba::rgb(128, 128, 128)),
        "transparent" => Some(rix_css::ColorRgba::TRANSPARENT),
        _ => None,
    }
}
