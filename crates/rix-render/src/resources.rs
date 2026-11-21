use rix_ipc::{FontResourceId, ImageId, PathResourceId, PipelineId, SurfaceId};
use std::collections::HashMap;
use std::sync::Arc;
use svgtypes::{SimplePathSegment, SimplifyingPathParser};
use thiserror::Error;
use vello::kurbo::BezPath;
use vello::peniko::{
    Blob, FontData as PenikoFontData, ImageAlphaType, ImageData as PenikoImageData, ImageFormat,
};

pub const DEFAULT_IMAGE_CACHE_BUDGET_BYTES: usize = 256 * 1024 * 1024;

#[derive(Error, Debug)]
pub enum ResourceError {
    #[error("Image decode error: {0}")]
    ImageDecode(#[from] image::ImageError),
    #[error("Invalid RGBA buffer length {actual} for {width}x{height}")]
    InvalidRgbaSize {
        width: u32,
        height: u32,
        actual: usize,
    },
    #[error("Invalid SVG path data: {0}")]
    InvalidSvgPath(String),
}

#[derive(Clone)]
pub struct RegisteredFont {
    pub font: PenikoFontData,
    pub normalized_coords: Vec<i16>,
    pub byte_len: usize,
}

#[derive(Clone)]
struct CachedImageEntry {
    image: PenikoImageData,
    bytes: usize,
    last_used: u64,
}

pub struct ResourceRegistry {
    fonts: HashMap<FontResourceId, RegisteredFont>,
    images: HashMap<(PipelineId, ImageId), CachedImageEntry>,
    image_used_bytes: usize,
    image_budget_bytes: usize,
    use_tick: u64,
    paths: HashMap<PathResourceId, BezPath>,
    external_surfaces: HashMap<SurfaceId, PenikoImageData>,
}

impl Default for ResourceRegistry {
    fn default() -> Self {
        Self::new(DEFAULT_IMAGE_CACHE_BUDGET_BYTES)
    }
}

impl ResourceRegistry {
    pub fn new(image_budget_bytes: usize) -> Self {
        Self {
            fonts: HashMap::new(),
            images: HashMap::new(),
            image_used_bytes: 0,
            image_budget_bytes,
            use_tick: 0,
            paths: HashMap::new(),
            external_surfaces: HashMap::new(),
        }
    }

    pub fn font_resource_id(bytes: &[u8], face_index: u32) -> FontResourceId {
        // 64-bit FNV-1a over face index and font bytes for cross-pipeline deduplication.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in face_index.to_le_bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        FontResourceId(hash)
    }

    pub fn register_font_bytes(
        &mut self,
        bytes: Vec<u8>,
        face_index: u32,
        normalized_coords: Vec<i16>,
    ) -> FontResourceId {
        let id = Self::font_resource_id(&bytes, face_index);
        self.fonts.entry(id).or_insert_with(|| {
            let byte_len = bytes.len();
            let blob = Blob::new(Arc::new(bytes));
            RegisteredFont {
                font: PenikoFontData::new(blob, face_index),
                normalized_coords,
                byte_len,
            }
        });
        id
    }

    pub fn register_rix_font(&mut self, font: &rix_text::FontData) -> FontResourceId {
        let id = Self::font_resource_id(&font.bytes, font.face_index);
        self.fonts.entry(id).or_insert_with(|| RegisteredFont {
            font: PenikoFontData::new(font.blob().clone(), font.face_index),
            normalized_coords: font.normalized_coords.clone(),
            byte_len: font.bytes.len(),
        });
        id
    }

    pub fn get_font(&self, id: FontResourceId) -> Option<&RegisteredFont> {
        self.fonts.get(&id)
    }

    pub fn font_count(&self) -> usize {
        self.fonts.len()
    }

    pub fn register_rgba8(
        &mut self,
        pipeline: PipelineId,
        image_id: ImageId,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    ) -> Result<(), ResourceError> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(4))
            .ok_or(ResourceError::InvalidRgbaSize {
                width,
                height,
                actual: rgba.len(),
            })?;
        if rgba.len() != expected || width == 0 || height == 0 {
            return Err(ResourceError::InvalidRgbaSize {
                width,
                height,
                actual: rgba.len(),
            });
        }
        let image = PenikoImageData {
            data: Blob::new(Arc::new(rgba)),
            format: ImageFormat::Rgba8,
            alpha_type: ImageAlphaType::Alpha,
            width,
            height,
        };
        self.insert_image_entry((pipeline, image_id), image, expected);
        Ok(())
    }

    pub fn register_encoded(
        &mut self,
        pipeline: PipelineId,
        image_id: ImageId,
        encoded: &[u8],
    ) -> Result<(), ResourceError> {
        let decoded = image::load_from_memory(encoded)?.into_rgba8();
        let (width, height) = decoded.dimensions();
        self.register_rgba8(pipeline, image_id, width, height, decoded.into_raw())
    }

    pub fn release_image(&mut self, pipeline: PipelineId, image_id: ImageId) -> bool {
        if let Some(removed) = self.images.remove(&(pipeline, image_id)) {
            self.image_used_bytes = self.image_used_bytes.saturating_sub(removed.bytes);
            true
        } else {
            false
        }
    }

    pub fn release_pipeline_images(&mut self, pipeline: PipelineId) {
        let keys: Vec<(PipelineId, ImageId)> = self
            .images
            .keys()
            .copied()
            .filter(|(p, _)| *p == pipeline)
            .collect();
        for key in keys {
            self.release_image(key.0, key.1);
        }
    }

    pub fn get_image(
        &mut self,
        pipeline: PipelineId,
        image_id: ImageId,
    ) -> Option<PenikoImageData> {
        let entry = self.images.get_mut(&(pipeline, image_id))?;
        self.use_tick = self.use_tick.wrapping_add(1);
        entry.last_used = self.use_tick;
        Some(entry.image.clone())
    }

    pub fn image_used_bytes(&self) -> usize {
        self.image_used_bytes
    }

    pub fn image_count(&self) -> usize {
        self.images.len()
    }

    pub fn register_path(&mut self, id: PathResourceId, path: BezPath) {
        self.paths.insert(id, path);
    }

    pub fn register_svg_path(&mut self, id: PathResourceId, d: &str) -> Result<(), ResourceError> {
        let path = parse_svg_path(d)?;
        self.paths.insert(id, path);
        Ok(())
    }

    pub fn get_path(&self, id: PathResourceId) -> Option<&BezPath> {
        self.paths.get(&id)
    }

    pub fn register_external_surface_rgba8(
        &mut self,
        surface_id: SurfaceId,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    ) -> Result<(), ResourceError> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(4))
            .ok_or(ResourceError::InvalidRgbaSize {
                width,
                height,
                actual: rgba.len(),
            })?;
        if rgba.len() != expected || width == 0 || height == 0 {
            return Err(ResourceError::InvalidRgbaSize {
                width,
                height,
                actual: rgba.len(),
            });
        }
        let image = PenikoImageData {
            data: Blob::new(Arc::new(rgba)),
            format: ImageFormat::Rgba8,
            alpha_type: ImageAlphaType::Alpha,
            width,
            height,
        };
        self.external_surfaces.insert(surface_id, image);
        Ok(())
    }

    pub fn remove_external_surface(&mut self, surface_id: SurfaceId) -> bool {
        self.external_surfaces.remove(&surface_id).is_some()
    }

    pub fn get_external_surface(&self, surface_id: SurfaceId) -> Option<PenikoImageData> {
        self.external_surfaces.get(&surface_id).cloned()
    }

    fn insert_image_entry(
        &mut self,
        key: (PipelineId, ImageId),
        image: PenikoImageData,
        bytes: usize,
    ) {
        if let Some(prev) = self.images.remove(&key) {
            self.image_used_bytes = self.image_used_bytes.saturating_sub(prev.bytes);
        }
        if bytes > self.image_budget_bytes {
            return;
        }
        while self.image_used_bytes + bytes > self.image_budget_bytes {
            let Some(oldest_key) = self
                .images
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(k, _)| *k)
            else {
                break;
            };
            if let Some(evicted) = self.images.remove(&oldest_key) {
                self.image_used_bytes = self.image_used_bytes.saturating_sub(evicted.bytes);
            }
        }
        self.use_tick = self.use_tick.wrapping_add(1);
        self.image_used_bytes += bytes;
        self.images.insert(
            key,
            CachedImageEntry {
                image,
                bytes,
                last_used: self.use_tick,
            },
        );
    }
}

pub fn parse_svg_path(d: &str) -> Result<BezPath, ResourceError> {
    let mut path = BezPath::new();
    let mut has_segment = false;
    for seg in SimplifyingPathParser::from(d) {
        let seg = seg.map_err(|e| ResourceError::InvalidSvgPath(e.to_string()))?;
        has_segment = true;
        match seg {
            SimplePathSegment::MoveTo { x, y } => path.move_to((x, y)),
            SimplePathSegment::LineTo { x, y } => path.line_to((x, y)),
            SimplePathSegment::CurveTo {
                x1,
                y1,
                x2,
                y2,
                x,
                y,
            } => path.curve_to((x1, y1), (x2, y2), (x, y)),
            SimplePathSegment::Quadratic { x1, y1, x, y } => path.quad_to((x1, y1), (x, y)),
            SimplePathSegment::ClosePath => path.close_path(),
        }
    }
    if !has_segment {
        return Err(ResourceError::InvalidSvgPath(
            "empty SVG path string".to_string(),
        ));
    }
    Ok(path)
}
