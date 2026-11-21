//! Headless GPU visual reftest runner (`cargo xtask reftest`).
//!
//! Renders HTML/CSS test documents and their `-ref.html` (or baseline `.png`)
//! counterparts via `rix-html -> rix-css -> rix-layout -> rix-render::VelloRenderer`
//! and compares RGBA pixel buffers against per-channel & mismatched-pixel tolerances.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use rix_core::Size;
use rix_css::{CascadeOrigin, MediaContext, StyleContext, Stylesheet};
use rix_dom::{Document, NodeData};
use rix_html::parse_html;
use rix_ipc::PipelineId;
use rix_layout::LayoutEngine;
use rix_render::{encode_display_list, EncodeOptions, ResourceRegistry, VelloRenderer};
use rix_text::FontDatabase;
use serde::{Deserialize, Serialize};
use vello::peniko::Color;
use vello::Scene;

const DEFAULT_WIDTH: u32 = 360;
const DEFAULT_HEIGHT: u32 = 240;
const DEFAULT_CHANNEL_TOLERANCE: u8 = 4;
const DEFAULT_MAX_DIFF_PIXELS: usize = 0;

/// Optional `<fixture>.toml` configuration overriding viewport or pixel tolerance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReftestConfig {
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_channel_tol")]
    pub channel_tolerance: u8,
    #[serde(default)]
    pub max_diff_pixels: usize,
}

impl Default for ReftestConfig {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            channel_tolerance: DEFAULT_CHANNEL_TOLERANCE,
            max_diff_pixels: DEFAULT_MAX_DIFF_PIXELS,
        }
    }
}

fn default_width() -> u32 {
    DEFAULT_WIDTH
}
fn default_height() -> u32 {
    DEFAULT_HEIGHT
}
fn default_channel_tol() -> u8 {
    DEFAULT_CHANNEL_TOLERANCE
}

/// Pixel-level diff result comparing a rendered test image against its reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelDiffSummary {
    pub total_pixels: usize,
    pub diff_pixels: usize,
    pub max_channel_delta: u8,
}

/// Result of a single visual reftest fixture.
#[derive(Debug, Clone)]
pub struct ReftestCaseResult {
    pub name: String,
    pub passed: bool,
    pub diff: PixelDiffSummary,
    pub config: ReftestConfig,
}

/// Aggregate visual reftest suite report.
#[derive(Debug, Clone, Default)]
pub struct ReftestReport {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub cases: Vec<ReftestCaseResult>,
}

/// Shared lazy system font database reused across reftests and WPT cases.
pub fn shared_font_db() -> &'static FontDatabase {
    static DB: OnceLock<FontDatabase> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = FontDatabase::new();
        db.scan_system_fonts();
        db
    })
}

/// Renders an HTML string (with embedded `<style>` tags) headlessly into an RGBA8 buffer.
pub fn render_html_rgba(html: &str, width: u32, height: u32) -> Result<Vec<u8>> {
    let viewport = Size::new(width as f32, height as f32);
    let media = MediaContext {
        viewport_width: viewport.width,
        viewport_height: viewport.height,
        ..MediaContext::default()
    };
    let mut doc = parse_html(html);
    let mut styles = StyleContext::new(media);
    for css in extract_style_blocks(&doc) {
        styles.add_stylesheet(Arc::new(Stylesheet::parse(
            &css,
            CascadeOrigin::Author,
            &media,
        )));
    }
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    let pipeline = PipelineId(1);
    let mut resources = ResourceRegistry::default();
    let frame = engine.layout_and_paint(
        &doc,
        &styles,
        viewport,
        shared_font_db(),
        pipeline,
        &mut resources,
    );

    let mut scene = Scene::new();
    encode_display_list(
        pipeline,
        &frame.display_list,
        &mut resources,
        &EncodeOptions::default(),
        &mut scene,
    );

    static GPU_LOCK: Mutex<()> = Mutex::new(());
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut renderer = VelloRenderer::new();
    renderer
        .render_to_rgba_blocking(&scene, width, height, Color::WHITE)
        .map_err(|e| anyhow::anyhow!("headless Vello render failed: {e}"))
}

/// Compares two RGBA8 buffers of equal dimensions with a per-channel tolerance.
#[must_use]
pub fn compare_rgba(actual: &[u8], expected: &[u8], channel_tolerance: u8) -> PixelDiffSummary {
    let px_count = actual.len().min(expected.len()) / 4;
    let mut diff_pixels = 0usize;
    let mut max_delta = 0u8;

    for i in 0..px_count {
        let off = i * 4;
        let mut px_max = 0u8;
        for c in 0..4 {
            let d = actual[off + c].abs_diff(expected[off + c]);
            px_max = px_max.max(d);
        }
        max_delta = max_delta.max(px_max);
        if px_max > channel_tolerance {
            diff_pixels += 1;
        }
    }

    let extra = actual.len().abs_diff(expected.len()) / 4;
    PixelDiffSummary {
        total_pixels: px_count + extra,
        diff_pixels: diff_pixels + extra,
        max_channel_delta: max_delta,
    }
}

/// Runs all visual reftest pairs discovered in `dir`.
pub fn run_reftest_suite(dir: &Path, update_refs: bool) -> Result<ReftestReport> {
    let mut html_tests: Vec<PathBuf> = Vec::new();
    if dir.exists() {
        for entry in
            fs::read_dir(dir).with_context(|| format!("read reftest dir {}", dir.display()))?
        {
            let path = entry?.path();
            let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if fname.ends_with(".html") && !fname.ends_with("-ref.html") {
                html_tests.push(path);
            }
        }
    }
    html_tests.sort();

    let mut report = ReftestReport::default();
    for test_path in html_tests {
        let stem = test_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        let cfg_path = test_path.with_extension("toml");
        let config: ReftestConfig = if cfg_path.exists() {
            let raw = fs::read_to_string(&cfg_path)?;
            toml::from_str(&raw).with_context(|| format!("parse {}", cfg_path.display()))?
        } else {
            ReftestConfig::default()
        };

        let test_html = fs::read_to_string(&test_path)?;
        let actual_rgba = render_html_rgba(&test_html, config.width, config.height)?;

        let ref_html_path = dir.join(format!("{stem}-ref.html"));
        let ref_png_path = dir.join(format!("{stem}-ref.png"));

        let expected_rgba = if ref_html_path.exists() {
            let ref_html = fs::read_to_string(&ref_html_path)?;
            render_html_rgba(&ref_html, config.width, config.height)?
        } else if ref_png_path.exists() && !update_refs {
            image::open(&ref_png_path)?.into_rgba8().into_raw()
        } else if update_refs {
            let img = image::RgbaImage::from_raw(config.width, config.height, actual_rgba.clone())
                .ok_or_else(|| anyhow::anyhow!("invalid rgba image size"))?;
            img.save(&ref_png_path)?;
            actual_rgba.clone()
        } else {
            anyhow::bail!(
                "reftest `{stem}` has neither {}-ref.html nor {}-ref.png",
                stem,
                stem
            );
        };

        let diff = compare_rgba(&actual_rgba, &expected_rgba, config.channel_tolerance);
        let passed = diff.diff_pixels <= config.max_diff_pixels;
        report.total += 1;
        if passed {
            report.passed += 1;
        } else {
            report.failed += 1;
        }
        report.cases.push(ReftestCaseResult {
            name: stem,
            passed,
            diff,
            config,
        });
    }
    Ok(report)
}

pub fn print_report(report: &ReftestReport) {
    println!(
        "=== Visual Reftests ({}/{} passed) ===",
        report.passed, report.total
    );
    for case in &report.cases {
        let status = if case.passed { "PASS" } else { "FAIL" };
        println!(
            "  [{status}] {:<28} diff_px={:<5} max_delta={:<3} ({}x{})",
            case.name,
            case.diff.diff_pixels,
            case.diff.max_channel_delta,
            case.config.width,
            case.config.height
        );
    }
}

pub(crate) fn extract_style_blocks(doc: &Document) -> Vec<String> {
    let mut styles = Vec::new();
    for node in doc.descendants(doc.root()) {
        if let Some(NodeData::Element(e)) = doc.get(node) {
            if e.name.local.eq_str_ignore_ascii_case("style") {
                let mut css = String::new();
                for ch in doc.children(node) {
                    if let Some(NodeData::Text(t)) = doc.get(ch) {
                        css.push_str(t);
                    }
                }
                if !css.trim().is_empty() {
                    styles.push(css);
                }
            }
        }
    }
    styles
}
