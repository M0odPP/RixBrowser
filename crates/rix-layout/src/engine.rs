//! High-level [`LayoutEngine`] orchestrating `BoxTree -> FragmentTree -> DisplayList`
//! with incremental subtree caching, frame-time metrics, and hit-testing.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use rix_core::{Point, Size};
use rix_css::{PointerEventsMode, StyleContext, VisibilityMode};
use rix_dom::{Document, NodeId};
use rix_ipc::PipelineId;
use rix_render::display_list::DisplayList;
use rix_render::ResourceRegistry;
use rix_text::{shared_shaping_cache, FontDatabase, FontFaceSet, TextShapingCache};

use crate::assets::AssetRegistry;
use crate::box_tree::BoxTree;
use crate::fragment::{layout_tree, layout_tree_with_scroll, Fragment, FragmentTree, LayoutCache};
use crate::paint::paint_fragment_tree_with_keyframes;

/// Timing and cache telemetry emitted by [`LayoutEngine::layout_and_paint`].
#[derive(Debug, Clone, Copy, Default)]
pub struct LayoutMetrics {
    pub box_tree_ms: f32,
    pub layout_ms: f32,
    pub paint_ms: f32,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub display_items: usize,
}

/// Complete layout + paint frame output for a document pass.
pub struct LayoutFrame {
    pub fragment_tree: FragmentTree,
    pub display_list: DisplayList,
    pub metrics: LayoutMetrics,
}

/// Per-document retained layout engine holding [`LayoutCache`], decoded [`AssetRegistry`],
/// web [`FontFaceSet`], and shared [`TextShapingCache`].
pub struct LayoutEngine {
    pub cache: LayoutCache,
    pub assets: AssetRegistry,
    pub web_fonts: FontFaceSet,
    pub shaping_cache: Arc<TextShapingCache>,
    pub dark_mode: bool,
}

impl Default for LayoutEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutEngine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cache: LayoutCache::new(),
            assets: AssetRegistry::new(),
            web_fonts: FontFaceSet::new(),
            shaping_cache: shared_shaping_cache(),
            dark_mode: false,
        }
    }

    pub fn clear_caches(&mut self) {
        self.cache.clear();
    }

    /// Resolves the CSS Values L3 §5.1.1 `1ch` advance measure (`"0"` glyph width) for `style`.
    pub fn ch_advance_px(&mut self, style: &rix_css::ComputedStyle, font_db: &FontDatabase) -> f32 {
        self.cache
            .ch_advance_px(font_db, Some(&self.web_fonts), &self.shaping_cache, style)
    }

    /// Runs `BoxTree -> FragmentTree` layout pass (without painting) and returns timing metrics.
    pub fn layout_only(
        &mut self,
        doc: &Document,
        styles: &StyleContext,
        viewport: Size,
        font_db: &FontDatabase,
    ) -> (FragmentTree, LayoutMetrics) {
        let t0 = Instant::now();
        let box_tree = BoxTree::build(doc, styles, &self.assets);
        let t1 = Instant::now();
        let fragment_tree = layout_tree(
            &box_tree,
            viewport,
            font_db,
            Some(&self.web_fonts),
            Arc::clone(&self.shaping_cache),
            &mut self.cache,
        );
        let t2 = Instant::now();
        let metrics = LayoutMetrics {
            box_tree_ms: (t1 - t0).as_secs_f32() * 1000.0,
            layout_ms: (t2 - t1).as_secs_f32() * 1000.0,
            paint_ms: 0.0,
            cache_hits: self.cache.cache_hits,
            cache_misses: self.cache.cache_misses,
            display_items: 0,
        };
        (fragment_tree, metrics)
    }

    /// Runs `BoxTree -> FragmentTree` layout pass with active element/root scroll offsets
    /// (used for live `position: sticky` offsets and CSSOM View queries).
    pub fn layout_only_with_scroll(
        &mut self,
        doc: &Document,
        styles: &StyleContext,
        viewport: Size,
        font_db: &FontDatabase,
        scroll_offsets: &HashMap<NodeId, Point>,
        root_scroll: Point,
    ) -> (FragmentTree, LayoutMetrics) {
        let t0 = Instant::now();
        let box_tree = BoxTree::build(doc, styles, &self.assets);
        let t1 = Instant::now();
        let fragment_tree = layout_tree_with_scroll(
            &box_tree,
            viewport,
            font_db,
            Some(&self.web_fonts),
            Arc::clone(&self.shaping_cache),
            &mut self.cache,
            scroll_offsets,
            root_scroll,
        );
        let t2 = Instant::now();
        let metrics = LayoutMetrics {
            box_tree_ms: (t1 - t0).as_secs_f32() * 1000.0,
            layout_ms: (t2 - t1).as_secs_f32() * 1000.0,
            paint_ms: 0.0,
            cache_hits: self.cache.cache_hits,
            cache_misses: self.cache.cache_misses,
            display_items: 0,
        };
        (fragment_tree, metrics)
    }

    /// Runs full `BoxTree -> FragmentTree -> DisplayList` pipeline and registers GPU resources.
    pub fn layout_and_paint(
        &mut self,
        doc: &Document,
        styles: &StyleContext,
        viewport: Size,
        font_db: &FontDatabase,
        pipeline: PipelineId,
        resources: &mut ResourceRegistry,
    ) -> LayoutFrame {
        let (fragment_tree, mut metrics) = self.layout_only(doc, styles, viewport, font_db);
        let t_paint = Instant::now();
        let keyframes = styles.all_keyframes();
        let display_list = paint_fragment_tree_with_keyframes(
            &fragment_tree,
            font_db,
            &self.assets,
            &keyframes,
            viewport,
            pipeline,
            resources,
            self.dark_mode,
        );
        metrics.paint_ms = t_paint.elapsed().as_secs_f32() * 1000.0;
        metrics.display_items = display_list.items.len();
        LayoutFrame {
            fragment_tree,
            display_list,
            metrics,
        }
    }
}

/// Hit-tests `point` (in document coordinates) against `tree` in reverse visual order,
/// honoring `pointer-events: none` and `visibility: hidden`.
#[must_use]
pub fn hit_test(tree: &FragmentTree, point: Point) -> Option<NodeId> {
    let root = tree.root.as_ref()?;
    hit_test_fragment(root, Point::ZERO, point)
}

fn hit_test_fragment(frag: &Fragment, parent_origin: Point, point: Point) -> Option<NodeId> {
    let abs = frag.absolute_border_rect(parent_origin);

    // Search children in reverse visual order (top-most painted child first).
    for ch in frag.children.iter().rev() {
        if let Some(hit) = hit_test_fragment(ch, abs.origin, point) {
            return Some(hit);
        }
    }

    if frag.style.visibility != VisibilityMode::Visible
        || frag.style.pointer_events == PointerEventsMode::None
    {
        return None;
    }

    if abs.contains(point) {
        return frag.dom_node;
    }
    None
}
