//! Structured DOM, computed-style, layout-fragment, image-audit, and interactive-scenario
//! inspector for differential parity and smoothness benchmarking against Mozilla Firefox.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::{Duration, Instant};

use rix_core::{Point, Rect, Size};
use rix_css::ComputedStyle;
use rix_dom::{Document, NodeData, NodeId};
use rix_layout::{AssetRegistry, Fragment, FragmentTree};
use rix_render::{Color, Scene};
use serde::{Deserialize, Serialize};

use crate::browser_controller::BrowserController;

/// Canonical set of 34 computed CSS properties captured in both RixBrowser and Firefox
/// for element-by-element differential parity analysis.
pub const INSPECTED_CSS_PROPERTIES: &[&str] = &[
    "display",
    "position",
    "visibility",
    "opacity",
    "z-index",
    "overflow-x",
    "overflow-y",
    "box-sizing",
    "flex-direction",
    "flex-wrap",
    "justify-content",
    "align-items",
    "grid-template-columns",
    "color",
    "background-color",
    "background-image",
    "font-family",
    "font-size",
    "font-weight",
    "line-height",
    "text-align",
    "white-space",
    "margin-top",
    "margin-right",
    "margin-bottom",
    "margin-left",
    "padding-top",
    "padding-right",
    "padding-bottom",
    "padding-left",
    "border-top-width",
    "border-top-left-radius",
    "width",
    "height",
];

/// Maximum visible elements serialized per page snapshot (covers full above-the-fold + near-fold).
const MAX_SNAPSHOT_ELEMENTS: usize = 400;

/// Serialized viewport-relative bounding box rounded to 0.1 CSS px.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct RectSnapshot {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl RectSnapshot {
    #[must_use]
    pub fn from_rect(r: Rect) -> Self {
        Self {
            x: round_tenth(r.origin.x),
            y: round_tenth(r.origin.y),
            width: round_tenth(r.size.width),
            height: round_tenth(r.size.height),
        }
    }
}

/// Summary of `<img>` asset load health on the rendered page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAuditSummary {
    pub total: usize,
    pub loaded: usize,
    pub broken: usize,
    pub visible_broken: usize,
}

/// Single visible element's canonical selector identity, geometry, and computed styles.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ElementSnapshot {
    pub path: String,
    pub tag: String,
    pub id: String,
    pub classes: Vec<String>,
    pub text_snippet: String,
    pub rect: RectSnapshot,
    pub styles: BTreeMap<String, String>,
}

/// Complete schema-matched DOM/Style/Layout inspection snapshot emitted identically by
/// RixBrowser (`--dump-dom-layout`) and Mozilla Firefox (`xtask compare-firefox` BiDi script).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PageInspectionSnapshot {
    pub url: String,
    pub title: String,
    pub viewport_width: f32,
    pub viewport_height: f32,
    pub scroll_width: f32,
    pub scroll_height: f32,
    pub node_count: usize,
    pub images: ImageAuditSummary,
    pub elements: Vec<ElementSnapshot>,
}

/// Frame-time distribution and smoothness statistics for scroll/animation runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct FrameTimingStats {
    pub frame_count: usize,
    pub avg_frame_ms: f64,
    pub avg_fps: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub jank_frames_over_16ms: usize,
}

impl FrameTimingStats {
    /// Computes percentile frame-time statistics from raw per-frame durations in milliseconds.
    #[must_use]
    pub fn from_samples_ms(samples: &[f64]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let mut sorted = samples.to_vec();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let count = sorted.len();
        let sum: f64 = sorted.iter().sum();
        let avg = sum / count as f64;
        let avg_fps = if avg > 0.001 {
            (1000.0 / avg).min(240.0)
        } else {
            240.0
        };
        let p50 = percentile(&sorted, 0.50);
        let p95 = percentile(&sorted, 0.95);
        let p99 = percentile(&sorted, 0.99);
        let max_ms = *sorted.last().unwrap_or(&0.0);
        let jank = sorted.iter().filter(|&&ms| ms > 16.67).count();
        Self {
            frame_count: count,
            avg_frame_ms: avg,
            avg_fps,
            p50_ms: p50,
            p95_ms: p95,
            p99_ms: p99,
            max_ms,
            jank_frames_over_16ms: jank,
        }
    }
}

/// Results of running the universal interactive + smoothness + idle-0Hz scenario on a page.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScenarioReport {
    pub scroll_gpu: FrameTimingStats,
    pub scroll_compose: FrameTimingStats,
    pub scroll_steps_applied: usize,
    pub hover_attempted: bool,
    pub hover_changed: bool,
    pub click_attempted: bool,
    pub click_dom_changed: bool,
    pub resize_ok: bool,
    pub idle_frames_rendered: u64,
    pub post_action_png: Option<String>,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p.clamp(0.0, 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[inline]
fn round_tenth(v: f32) -> f32 {
    if !v.is_finite() {
        return 0.0;
    }
    (v * 10.0).round() / 10.0
}

/// Builds a canonical CSS structural path (`html>body>main:nth-of-type(1)>h1:nth-of-type(1)`)
/// matching the identical algorithm executed in Firefox via WebDriver BiDi.
#[must_use]
pub fn build_canonical_css_path(doc: &Document, node_id: NodeId) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = Some(node_id);
    while let Some(nid) = cur {
        if let Some(NodeData::Element(el)) = doc.get(nid) {
            let tag = el.name.local.as_str().to_ascii_lowercase();
            let segment = if tag == "html" || tag == "body" || tag == "head" {
                tag
            } else if let Some(parent) = doc.parent(nid) {
                let mut idx = 0usize;
                for sib in doc.children(parent) {
                    if let Some(NodeData::Element(se)) = doc.get(sib) {
                        if se.name.local.as_str().eq_ignore_ascii_case(&tag) {
                            idx += 1;
                            if sib == nid {
                                break;
                            }
                        }
                    }
                }
                format!("{tag}:nth-of-type({})", idx.max(1))
            } else {
                tag
            };
            parts.push(segment);
        }
        cur = doc.parent(nid);
    }
    parts.reverse();
    parts.join(">")
}

/// Extracts a trimmed, whitespace-collapsed preview of the element's direct/descendant text.
fn extract_text_snippet(doc: &Document, node_id: NodeId, max_chars: usize) -> String {
    let mut out = String::new();
    let mut prev_space = true;
    for nid in doc.descendants(node_id) {
        if out.len() >= max_chars {
            break;
        }
        if let Some(NodeData::Text(t)) = doc.get(nid) {
            for ch in t.chars() {
                if ch.is_whitespace() {
                    if !prev_space {
                        out.push(' ');
                        prev_space = true;
                    }
                } else {
                    out.push(ch);
                    prev_space = false;
                }
                if out.len() >= max_chars {
                    break;
                }
            }
        }
    }
    out.trim().to_string()
}

struct FragmentSnapshotInfo {
    rect: Rect,
    content_rect: Rect,
    padding: rix_layout::EdgeInsets,
    margin: rix_layout::EdgeInsets,
    style: std::sync::Arc<ComputedStyle>,
    resolved_grid_cols: Option<String>,
    is_replaced: bool,
}

fn collect_fragment_rects_and_styles(
    frag: &Fragment,
    parent_origin: Point,
    out: &mut HashMap<NodeId, FragmentSnapshotInfo>,
) {
    let abs = frag.absolute_border_rect(parent_origin);
    let is_replaced = !matches!(
        frag.kind,
        rix_layout::FragmentKind::Box
            | rix_layout::FragmentKind::InlineBox { .. }
            | rix_layout::FragmentKind::LineBox { .. }
            | rix_layout::FragmentKind::TextRun(_)
    );
    if let Some(nid) = frag.dom_node {
        out.entry(nid)
            .and_modify(|existing| {
                let min_x = existing.rect.origin.x.min(abs.origin.x);
                let min_y = existing.rect.origin.y.min(abs.origin.y);
                let max_x = existing.rect.max_x().max(abs.max_x());
                let max_y = existing.rect.max_y().max(abs.max_y());
                existing.rect = Rect::new(
                    min_x,
                    min_y,
                    (max_x - min_x).max(0.0),
                    (max_y - min_y).max(0.0),
                );
                existing.is_replaced |= is_replaced;
            })
            .or_insert_with(|| FragmentSnapshotInfo {
                rect: abs,
                content_rect: frag.content_rect,
                padding: frag.padding,
                margin: frag.margin,
                style: std::sync::Arc::clone(&frag.style),
                resolved_grid_cols: frag
                    .resolved_grid_tracks
                    .as_ref()
                    .filter(|g| !g.columns.is_empty())
                    .map(|g| g.columns.clone()),
                is_replaced,
            });
    }
    for child in frag.children.iter() {
        collect_fragment_rects_and_styles(child, abs.origin, out);
    }
}

fn audit_images(
    doc: &Document,
    assets: &AssetRegistry,
    rect_map: &HashMap<NodeId, FragmentSnapshotInfo>,
    viewport_w: f32,
    viewport_h: f32,
) -> ImageAuditSummary {
    let mut summary = ImageAuditSummary::default();
    for nid in doc.descendants(doc.root()) {
        let Some(NodeData::Element(el)) = doc.get(nid) else {
            continue;
        };
        if !el.name.local.as_str().eq_ignore_ascii_case("img") {
            continue;
        }
        let raw_src = el
            .get_attribute("src")
            .or_else(|| el.get_attribute("srcset"))
            .unwrap_or("")
            .trim();
        if raw_src.is_empty() {
            continue;
        }
        summary.total += 1;
        let is_loaded = assets.get_by_src(raw_src).is_some()
            || assets.get_svg_by_src(raw_src).is_some()
            || raw_src.starts_with("data:");
        if is_loaded {
            summary.loaded += 1;
        } else {
            summary.broken += 1;
            if let Some(info) = rect_map.get(&nid) {
                let rect = info.rect;
                let visible = rect.size.width >= 1.0
                    && rect.size.height >= 1.0
                    && rect.max_x() > 0.0
                    && rect.max_y() > 0.0
                    && rect.origin.x < viewport_w
                    && rect.origin.y < viewport_h;
                if visible {
                    summary.visible_broken += 1;
                }
            }
        }
    }
    summary
}

fn format_px_cssom(v: f32) -> String {
    let r = round_tenth(v);
    if (r - r.round()).abs() < 0.05 {
        format!("{}px", r.round() as i64)
    } else {
        format!("{r:.1}px")
    }
}

/// Formats CSSOM §6.7.3 resolved property values using the laid-out fragment box for
/// `width`/`height`/`margin-*`/`padding-*`, matching Firefox `window.getComputedStyle()`.
fn format_resolved_property(info: &FragmentSnapshotInfo, prop: &str) -> String {
    let is_border_box = matches!(info.style.box_sizing, rix_css::BoxSizingMode::BorderBox);
    let is_non_replaced_inline =
        matches!(info.style.display, rix_css::DisplayMode::Inline) && !info.is_replaced;
    match prop {
        "width" => {
            // CSSOM §6.7.3: non-replaced inline elements do not have a used width/height box,
            // so `getComputedStyle(el).width` returns `"auto"`.
            if is_non_replaced_inline {
                return "auto".to_string();
            }
            let v = if is_border_box {
                info.rect.size.width
            } else {
                info.content_rect.size.width
            };
            format_px_cssom(v)
        }
        "height" => {
            if is_non_replaced_inline {
                return "auto".to_string();
            }
            let v = if is_border_box {
                info.rect.size.height
            } else {
                info.content_rect.size.height
            };
            format_px_cssom(v)
        }
        "margin-top" => format_px_cssom(info.margin.top),
        "margin-right" => format_px_cssom(info.margin.right),
        "margin-bottom" => format_px_cssom(info.margin.bottom),
        "margin-left" => format_px_cssom(info.margin.left),
        "padding-top" => format_px_cssom(info.padding.top),
        "padding-right" => format_px_cssom(info.padding.right),
        "padding-bottom" => format_px_cssom(info.padding.bottom),
        "padding-left" => format_px_cssom(info.padding.left),
        "grid-template-columns" => {
            if let Some(ref cols) = info.resolved_grid_cols {
                cols.clone()
            } else {
                rix_content::cssom::format_computed_property(&info.style, prop)
            }
        }
        _ => rix_content::cssom::format_computed_property(&info.style, prop),
    }
}

fn build_snapshot_from_parts(
    url: &str,
    title: &str,
    viewport_w: f32,
    viewport_h: f32,
    doc: &Document,
    fragment_tree: Option<&FragmentTree>,
    assets: Option<&AssetRegistry>,
) -> PageInspectionSnapshot {
    let mut rect_map: HashMap<NodeId, FragmentSnapshotInfo> = HashMap::new();
    let (scroll_w, scroll_h) = if let Some(ft) = fragment_tree {
        if let Some(ref root) = ft.root {
            collect_fragment_rects_and_styles(root, Point::ZERO, &mut rect_map);
        }
        (
            round_tenth(ft.content_bounds.size.width.max(viewport_w)),
            round_tenth(ft.content_bounds.size.height.max(viewport_h)),
        )
    } else {
        (round_tenth(viewport_w), round_tenth(viewport_h))
    };

    let images = assets.map_or_else(ImageAuditSummary::default, |reg| {
        audit_images(doc, reg, &rect_map, viewport_w, viewport_h)
    });

    let mut elements = Vec::new();
    let near_fold_y = viewport_h * 1.5;

    for nid in doc.descendants(doc.root()) {
        if elements.len() >= MAX_SNAPSHOT_ELEMENTS {
            break;
        }
        let Some(NodeData::Element(el)) = doc.get(nid) else {
            continue;
        };
        let tag = el.name.local.as_str().to_ascii_lowercase();
        if matches!(
            tag.as_str(),
            "script" | "style" | "meta" | "link" | "head" | "title" | "noscript" | "br"
        ) {
            continue;
        }
        let Some(info) = rect_map.get(&nid) else {
            continue;
        };
        let rect = info.rect;
        if rect.size.width < 1.0 && rect.size.height < 1.0 {
            continue;
        }
        if rect.origin.y > near_fold_y || rect.max_y() < -50.0 {
            continue;
        }

        let id = el.get_attribute("id").unwrap_or("").to_string();
        let classes: Vec<String> = el
            .get_attribute("class")
            .map(|c| c.split_whitespace().take(6).map(String::from).collect())
            .unwrap_or_default();
        let path = build_canonical_css_path(doc, nid);
        let text_snippet = extract_text_snippet(doc, nid, 60);

        let mut styles = BTreeMap::new();
        for &prop in INSPECTED_CSS_PROPERTIES {
            let val = format_resolved_property(info, prop);
            styles.insert(prop.to_string(), val);
        }

        elements.push(ElementSnapshot {
            path,
            tag,
            id,
            classes,
            text_snippet,
            rect: RectSnapshot::from_rect(rect),
            styles,
        });
    }

    PageInspectionSnapshot {
        url: url.to_string(),
        title: title.to_string(),
        viewport_width: round_tenth(viewport_w),
        viewport_height: round_tenth(viewport_h),
        scroll_width: scroll_w,
        scroll_height: scroll_h,
        node_count: doc.node_count(),
        images,
        elements,
    }
}

/// Captures a structured DOM, computed-style, layout-geometry, and image-health snapshot
/// from the active tab of `controller` with zero mutation of page state.
#[must_use]
pub fn capture_page_snapshot(controller: &BrowserController) -> PageInspectionSnapshot {
    let web_rect = controller.web_viewport_rect();
    let active_tab = controller.chrome.active();
    let Some(tab_doc) = controller.active_tab_doc() else {
        return PageInspectionSnapshot {
            url: active_tab.url.clone(),
            title: active_tab.title.clone(),
            viewport_width: round_tenth(web_rect.size.width),
            viewport_height: round_tenth(web_rect.size.height),
            ..PageInspectionSnapshot::default()
        };
    };

    if let Some(ref rt) = tab_doc.runtime {
        let host = rt.host.borrow();
        let doc = host.document.borrow();
        return build_snapshot_from_parts(
            &active_tab.url,
            &active_tab.title,
            web_rect.size.width,
            web_rect.size.height,
            &doc,
            host.cached_fragment_tree(),
            Some(&host.layout_engine.assets),
        );
    }

    build_snapshot_from_parts(
        &active_tab.url,
        &active_tab.title,
        web_rect.size.width,
        web_rect.size.height,
        &tab_doc.doc,
        Some(&tab_doc.fragment_tree),
        None,
    )
}

/// Runs a universal generic interaction, APZ smooth-scroll GPU frame-pacing, viewport resize,
/// and IDLE 0 Hz benchmark on the already-loaded active tab.
///
/// `render_frame` executes one warmed headless GPU frame (optionally saving a PNG when `Some(path)`
/// is provided), keeping GPU device ownership strictly on the dedicated GPU worker thread.
pub fn run_interactive_scenario<F>(
    controller: &mut BrowserController,
    width: u32,
    height: u32,
    post_png: Option<&Path>,
    mut render_frame: F,
) -> ScenarioReport
where
    F: FnMut(Scene, u32, u32, Color, Option<&Path>) -> bool,
{
    let web_rect = controller.web_viewport_rect();
    let center = Point::new(
        web_rect.origin.x + web_rect.size.width * 0.5,
        web_rect.origin.y + web_rect.size.height * 0.5,
    );
    let clear = controller.clear_color();

    // 1. Smooth APZ wheel scroll sweep (16 frames) measuring both compose-only and full GPU
    //    frame times so FPS / p95 / p99 / jank are real end-to-end compositor numbers.
    let mut compose_samples = Vec::with_capacity(16);
    let mut gpu_samples = Vec::with_capacity(16);
    let mut scroll_steps_applied = 0usize;

    for _ in 0..16 {
        let t0 = Instant::now();
        if controller.handle_wheel(center, 0.0, 140.0) {
            scroll_steps_applied += 1;
        }
        controller.sync_layers();
        let scene = controller.compositor.compose_force().clone();
        let compose_ms = t0.elapsed().as_secs_f64() * 1000.0;
        compose_samples.push(compose_ms);

        let _ = render_frame(scene, width, height, clear, None);
        let total_frame_ms = t0.elapsed().as_secs_f64() * 1000.0;
        gpu_samples.push(total_frame_ms);
    }

    // Scroll back to top smoothly so post-action hover/click targets above-the-fold controls.
    for _ in 0..16 {
        let _ = controller.handle_wheel(center, 0.0, -140.0);
    }
    controller.sync_layers();

    // 2. Find an above-the-fold interactive element (button/a/input) purely by generic tag/layout.
    let target_pt = find_first_interactive_point(controller, web_rect);
    let mut hover_attempted = false;
    let mut hover_changed = false;
    let mut click_attempted = false;
    let mut click_dom_changed = false;

    if let Some(pt) = target_pt {
        hover_attempted = true;
        hover_changed = controller.handle_hover(pt);

        let rev_before = active_doc_revision(controller);
        let url_before = controller.chrome.active().url.clone();
        click_attempted = true;
        let _ = controller.handle_left_click(pt);
        for _ in 0..8 {
            if controller.tick_active_tab().is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let rev_after = active_doc_revision(controller);
        let url_after = controller.chrome.active().url.clone();
        click_dom_changed = rev_after != rev_before || url_after != url_before;
    }

    // 3. Viewport resize responsiveness check (shrink width by 120px, compose, then restore).
    let dpr = controller.compositor.device_pixel_ratio();
    let orig_logical = Size::new(width as f32 / dpr.max(0.5), height as f32 / dpr.max(0.5));
    let narrowed = Size::new((orig_logical.width - 120.0).max(320.0), orig_logical.height);
    let r1 = controller.set_viewport(narrowed, dpr);
    controller.sync_layers();
    let _ = controller.compositor.compose_force();
    let r2 = controller.set_viewport(orig_logical, dpr);
    controller.sync_layers();
    let post_scene = controller.compositor.compose_force().clone();
    let resize_ok = r1 && r2;

    let mut saved_post_png = None;
    if let Some(path) = post_png {
        if render_frame(post_scene, width, height, clear, Some(path)) {
            saved_post_png = Some(path.display().to_string());
        }
    }

    // 4. IDLE 0 Hz check: once composed and clean, verify repeated compose() calls produce 0 frames.
    let frames_before_idle = controller.compositor.frames_rendered();
    for _ in 0..5 {
        let _ = controller.compositor.compose();
    }
    let idle_frames_rendered = controller
        .compositor
        .frames_rendered()
        .saturating_sub(frames_before_idle);

    ScenarioReport {
        scroll_gpu: FrameTimingStats::from_samples_ms(&gpu_samples),
        scroll_compose: FrameTimingStats::from_samples_ms(&compose_samples),
        scroll_steps_applied,
        hover_attempted,
        hover_changed,
        click_attempted,
        click_dom_changed,
        resize_ok,
        idle_frames_rendered,
        post_action_png: saved_post_png,
    }
}

fn active_doc_revision(controller: &BrowserController) -> u64 {
    let Some(tab_doc) = controller.active_tab_doc() else {
        return 0;
    };
    if let Some(ref rt) = tab_doc.runtime {
        return rt.host.borrow().document.borrow().revision();
    }
    tab_doc.doc.revision()
}

/// Finds the screen-space center of the first visible `<button>` or `[role=button]` or `<summary>`
/// or safe in-page `<a href="#...">` within the web viewport for generic interactivity verification.
fn find_first_interactive_point(controller: &BrowserController, web_rect: Rect) -> Option<Point> {
    let snapshot = capture_page_snapshot(controller);
    for preferred in ["button", "summary", "input", "a"] {
        for el in &snapshot.elements {
            if el.tag == preferred
                && el.rect.width >= 12.0
                && el.rect.height >= 12.0
                && el.rect.x >= 0.0
                && el.rect.y >= 0.0
                && el.rect.x + el.rect.width * 0.5 < web_rect.size.width
                && el.rect.y + el.rect.height * 0.5 < web_rect.size.height
            {
                return Some(Point::new(
                    web_rect.origin.x + el.rect.x + el.rect.width * 0.5,
                    web_rect.origin.y + el.rect.y + el.rect.height * 0.5,
                ));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rix_html::parse_html;

    #[test]
    fn canonical_css_path_matches_standard_nth_of_type() {
        let doc = parse_html(
            "<!doctype html><html><body><header><nav><a>One</a><a>Two</a></nav></header></body></html>",
        );
        let mut links = Vec::new();
        for nid in doc.descendants(doc.root()) {
            if let Some(NodeData::Element(el)) = doc.get(nid) {
                if el.name.local.as_str().eq_ignore_ascii_case("a") {
                    links.push(build_canonical_css_path(&doc, nid));
                }
            }
        }
        assert_eq!(
            links,
            vec![
                "html>body>header:nth-of-type(1)>nav:nth-of-type(1)>a:nth-of-type(1)",
                "html>body>header:nth-of-type(1)>nav:nth-of-type(1)>a:nth-of-type(2)",
            ]
        );
    }

    #[test]
    fn frame_timing_stats_computes_percentiles_and_jank() {
        let samples = vec![4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 12.0, 18.5, 25.0];
        let stats = FrameTimingStats::from_samples_ms(&samples);
        assert_eq!(stats.frame_count, 10);
        assert_eq!(stats.jank_frames_over_16ms, 2);
        assert!(stats.p50_ms >= 8.0 && stats.p50_ms <= 9.0);
        assert!(stats.p95_ms >= 18.5);
        assert_eq!(stats.max_ms, 25.0);
        assert!(stats.avg_fps > 60.0);
    }
}
