use crate::font_loader::{FontDatabase, FontId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use unicode_bidi::BidiInfo;
use unicode_segmentation::UnicodeSegmentation;

const DEFAULT_SHAPING_CACHE_MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ShapeCacheKey {
    font_key: u64,
    face_index: u32,
    font_size_bits: u32,
    text: String,
}

struct ShapeCacheEntry {
    runs: Arc<[ShapedRun]>,
    approx_bytes: usize,
    last_used: AtomicU64,
}

#[derive(Default)]
struct ShapingCacheInner {
    entries: HashMap<ShapeCacheKey, ShapeCacheEntry>,
    used_bytes: usize,
}

/// Thread-safe shared text shaping cache with LRU memory budget eviction.
pub struct TextShapingCache {
    inner: RwLock<ShapingCacheInner>,
    max_bytes: usize,
    clock: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl TextShapingCache {
    #[must_use]
    pub fn new(max_bytes: usize) -> Self {
        Self {
            inner: RwLock::new(ShapingCacheInner {
                entries: HashMap::with_capacity(2048),
                used_bytes: 0,
            }),
            max_bytes: max_bytes.max(1024),
            clock: AtomicU64::new(1),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.read().map_or(0, |g| g.entries.len())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn used_bytes(&self) -> usize {
        self.inner.read().map_or(0, |g| g.used_bytes)
    }

    pub fn clear(&self) {
        if let Ok(mut guard) = self.inner.write() {
            guard.entries.clear();
            guard.used_bytes = 0;
        }
    }

    fn get(&self, key: &ShapeCacheKey) -> Option<Arc<[ShapedRun]>> {
        let guard = self.inner.read().ok()?;
        let entry = guard.entries.get(key)?;
        let tick = self.clock.fetch_add(1, Ordering::Relaxed);
        entry.last_used.store(tick, Ordering::Relaxed);
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(Arc::clone(&entry.runs))
    }

    fn insert(&self, key: ShapeCacheKey, runs: Vec<ShapedRun>) {
        self.misses.fetch_add(1, Ordering::Relaxed);
        let approx_bytes = estimate_entry_bytes(&key.text, &runs);
        if approx_bytes > self.max_bytes {
            return;
        }
        let Ok(mut guard) = self.inner.write() else {
            return;
        };
        if let Some(prev) = guard.entries.remove(&key) {
            guard.used_bytes = guard.used_bytes.saturating_sub(prev.approx_bytes);
        }
        if guard.used_bytes + approx_bytes > self.max_bytes {
            let target = (self.max_bytes * 3) / 4;
            let mut victims: Vec<(u64, ShapeCacheKey)> = guard
                .entries
                .iter()
                .map(|(k, v)| (v.last_used.load(Ordering::Relaxed), k.clone()))
                .collect();
            victims.sort_unstable_by_key(|(ts, _)| *ts);
            for (_, victim_key) in victims {
                if guard.used_bytes + approx_bytes <= target {
                    break;
                }
                if let Some(removed) = guard.entries.remove(&victim_key) {
                    guard.used_bytes = guard.used_bytes.saturating_sub(removed.approx_bytes);
                }
            }
        }
        let tick = self.clock.fetch_add(1, Ordering::Relaxed);
        guard.used_bytes += approx_bytes;
        guard.entries.insert(
            key,
            ShapeCacheEntry {
                runs: Arc::from(runs),
                approx_bytes,
                last_used: AtomicU64::new(tick),
            },
        );
    }
}

fn estimate_entry_bytes(text: &str, runs: &[ShapedRun]) -> usize {
    let glyph_count: usize = runs.iter().map(|r| r.glyphs.len()).sum();
    64 + text.len()
        + std::mem::size_of_val(runs)
        + glyph_count * std::mem::size_of::<GlyphPosition>()
}

/// Returns the process-wide shared thread-safe [`TextShapingCache`].
#[must_use]
pub fn shared_shaping_cache() -> Arc<TextShapingCache> {
    static CACHE: OnceLock<Arc<TextShapingCache>> = OnceLock::new();
    Arc::clone(
        CACHE.get_or_init(|| Arc::new(TextShapingCache::new(DEFAULT_SHAPING_CACHE_MAX_BYTES))),
    )
}

pub fn contains_rtl_runs(text: &str) -> bool {
    BidiInfo::new(text, None)
        .levels
        .iter()
        .any(|level| level.is_rtl())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    Ltr,
    Rtl,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlyphPosition {
    pub glyph_id: u32,
    pub cluster: u32,
    pub x_advance: f32,
    pub y_advance: f32,
    pub x_offset: f32,
    pub y_offset: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShapedRun {
    pub font_id: FontId,
    pub text_range: std::ops::Range<usize>,
    pub direction: Direction,
    pub glyphs: Vec<GlyphPosition>,
    pub width: f32,
    pub font_size: f32,
}

pub struct TextShaper<'a> {
    db: &'a FontDatabase,
    cache: Arc<TextShapingCache>,
}

impl<'a> TextShaper<'a> {
    #[must_use]
    pub fn new(db: &'a FontDatabase) -> Self {
        Self {
            db,
            cache: shared_shaping_cache(),
        }
    }

    #[must_use]
    pub fn with_cache(db: &'a FontDatabase, cache: Arc<TextShapingCache>) -> Self {
        Self { db, cache }
    }

    #[must_use]
    pub fn cache(&self) -> &Arc<TextShapingCache> {
        &self.cache
    }

    pub fn shape_text(&self, text: &str, font_id: FontId, font_size: f32) -> Vec<ShapedRun> {
        if text.is_empty() {
            return Vec::new();
        }
        let Some(font) = self.db.get_font(font_id) else {
            return Vec::new();
        };

        let can_cache = text.len() <= 512;
        let cache_key = if can_cache {
            let key = ShapeCacheKey {
                font_key: font.cache_identity(),
                face_index: font.face_index,
                font_size_bits: font_size.to_bits(),
                text: text.to_string(),
            };
            if let Some(cached) = self.cache.get(&key) {
                return cached.to_vec();
            }
            Some(key)
        } else {
            None
        };

        let features = typographic_features();
        let bidi_info = BidiInfo::new(text, None);
        let mut visual_runs = Vec::new();

        if bidi_info.paragraphs.is_empty() {
            if let Some(run) = self.shape_single_run(
                text,
                0..text.len(),
                Direction::Ltr,
                font_id,
                font_size,
                &features,
            ) {
                visual_runs.push(run);
            }
        } else {
            for para in &bidi_info.paragraphs {
                let (_, runs) = bidi_info.visual_runs(para, para.range.clone());

                for run_range in runs {
                    let is_rtl = bidi_info.levels[run_range.start].is_rtl();
                    let direction = if is_rtl {
                        Direction::Rtl
                    } else {
                        Direction::Ltr
                    };
                    let mut font_runs =
                        self.split_by_font_coverage(text, run_range.clone(), font_id);
                    if is_rtl {
                        font_runs.reverse();
                    }
                    for (sub_range, sub_font) in font_runs {
                        if let Some(shaped) = self.shape_single_run(
                            text, sub_range, direction, sub_font, font_size, &features,
                        ) {
                            visual_runs.push(shaped);
                        }
                    }
                }
            }
        }

        if let Some(key) = cache_key {
            self.cache.insert(key, visual_runs.clone());
        }

        visual_runs
    }

    fn shape_single_run(
        &self,
        text: &str,
        byte_range: std::ops::Range<usize>,
        direction: Direction,
        font_id: FontId,
        font_size: f32,
        features: &[rustybuzz::Feature],
    ) -> Option<ShapedRun> {
        let font = self.db.get_font(font_id)?;
        let rb_face = font.as_rustybuzz_face()?;
        let scale = font.metrics.scale(font_size);
        let run_text = text.get(byte_range.clone())?;
        if run_text.is_empty() {
            return None;
        }
        let is_rtl = direction == Direction::Rtl;

        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(run_text);
        buffer.set_direction(if is_rtl {
            rustybuzz::Direction::RightToLeft
        } else {
            rustybuzz::Direction::LeftToRight
        });

        let glyph_buffer = rustybuzz::shape(&rb_face, features, buffer);
        let infos = glyph_buffer.glyph_infos();
        let positions = glyph_buffer.glyph_positions();

        let mut glyphs = Vec::with_capacity(infos.len());
        let mut run_width = 0.0;

        for (info, pos) in infos.iter().zip(positions.iter()) {
            let x_adv = pos.x_advance as f32 * scale;
            let y_adv = pos.y_advance as f32 * scale;
            let x_off = pos.x_offset as f32 * scale;
            let y_off = pos.y_offset as f32 * scale;

            run_width += x_adv;
            glyphs.push(GlyphPosition {
                glyph_id: info.glyph_id,
                cluster: info.cluster + byte_range.start as u32,
                x_advance: x_adv,
                y_advance: y_adv,
                x_offset: x_off,
                y_offset: y_off,
            });
        }

        Some(ShapedRun {
            font_id,
            text_range: byte_range,
            direction,
            glyphs,
            width: run_width,
            font_size,
        })
    }

    fn split_by_font_coverage(
        &self,
        text: &str,
        range: std::ops::Range<usize>,
        primary: FontId,
    ) -> Vec<(std::ops::Range<usize>, FontId)> {
        let Some(slice) = text.get(range.clone()) else {
            return Vec::new();
        };
        let base = range.start;
        let mut out: Vec<(std::ops::Range<usize>, FontId)> = Vec::new();

        let mut seg_start = base;
        let mut seg_font = primary;
        let mut seg_end = base;
        let mut prev_font = primary;

        for (rel_offset, grapheme) in slice.grapheme_indices(true) {
            let abs_start = base + rel_offset;
            let abs_end = abs_start + grapheme.len();
            let font_for_cluster = self.font_for_grapheme(grapheme, primary, prev_font);

            if out.is_empty() && seg_end == seg_start {
                seg_font = font_for_cluster;
                seg_start = abs_start;
                seg_end = abs_end;
            } else if font_for_cluster == seg_font {
                seg_end = abs_end;
            } else {
                out.push((seg_start..seg_end, seg_font));
                seg_start = abs_start;
                seg_end = abs_end;
                seg_font = font_for_cluster;
            }
            prev_font = font_for_cluster;
        }
        if seg_end > seg_start {
            out.push((seg_start..seg_end, seg_font));
        }
        out
    }

    fn font_for_grapheme(&self, grapheme: &str, primary: FontId, prev: FontId) -> FontId {
        let mut has_base = false;
        let mut all_primary = true;
        for ch in grapheme.chars() {
            if is_inheriting_mark(ch) {
                continue;
            }
            has_base = true;
            if !covers(self.db, primary, ch) {
                all_primary = false;
                break;
            }
        }
        if !has_base {
            return prev;
        }
        if all_primary {
            return primary;
        }
        for ch in grapheme.chars() {
            if is_inheriting_mark(ch) {
                continue;
            }
            if !covers(self.db, primary, ch) {
                if let Some(id) = self.db.find_covering_font(ch, Some(primary)) {
                    return id;
                }
                break;
            }
        }
        primary
    }
}

fn typographic_features() -> Vec<rustybuzz::Feature> {
    use std::str::FromStr;
    const ENABLED: &[&str] = &[
        "+liga", "+clig", "+calt", "+kern", "+mark", "+mkmk", "+dist", "+abvm", "+blwm", "+curs",
        "+locl",
    ];
    ENABLED
        .iter()
        .filter_map(|s| rustybuzz::Feature::from_str(s).ok())
        .collect()
}

fn covers(db: &FontDatabase, id: FontId, ch: char) -> bool {
    db.get_font(id)
        .map(|f| f.supports_char(ch))
        .unwrap_or(false)
}

fn is_inheriting_mark(ch: char) -> bool {
    let v = ch as u32;
    matches!(
        v,
        0x0300..=0x036F
        | 0x1AB0..=0x1AFF
        | 0x1DC0..=0x1DFF
        | 0x20D0..=0x20FF
        | 0xFE00..=0xFE0F
        | 0xFE20..=0xFE2F
        | 0x200C..=0x200D
        | 0x1F3FB..=0x1F3FF
        | 0xE0020..=0xE007F
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shape_text_caching_dedup() {
        let mut db = FontDatabase::new();
        db.scan_system_fonts();
        let Some(font_id) = db.default_sans_serif else {
            return;
        };
        let cache = Arc::new(TextShapingCache::new(4 * 1024 * 1024));
        let shaper = TextShaper::with_cache(&db, Arc::clone(&cache));
        let runs1 = shaper.shape_text("Hello world shaping test!", font_id, 16.0);
        assert!(!runs1.is_empty(), "First shaping run must return glyphs");
        assert_eq!(cache.misses(), 1);
        let runs2 = shaper.shape_text("Hello world shaping test!", font_id, 16.0);
        assert_eq!(
            runs1, runs2,
            "Second shaping run must match cached run exactly"
        );
        assert_eq!(cache.hits(), 1);
    }
}
