//! Web Platform Tests (WPT) subset runner & baseline scorecard guard
//! (`cargo xtask wpt --subset <name>`).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rix_core::{Point, Rect, Size};
use rix_css::{CascadeOrigin, MediaContext, StyleContext, Stylesheet};
use rix_dom::Document;
use rix_html::parse_html;
use rix_layout::{Fragment, LayoutEngine};
use serde::{Deserialize, Serialize};

use crate::reftest::{compare_rgba, extract_style_blocks, render_html_rgba, shared_font_db};

/// Committed baseline pass counts per WPT subset.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WptBaseline {
    pub subsets: BTreeMap<String, usize>,
}

/// Declarative layout/style assertion case inside a WPT subset TOML manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WptAssertionManifest {
    pub cases: Vec<WptAssertionCase>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WptAssertionCase {
    pub name: String,
    pub html: String,
    #[serde(default)]
    pub css: String,
    #[serde(default = "default_vp_w")]
    pub viewport_width: f32,
    #[serde(default = "default_vp_h")]
    pub viewport_height: f32,
    pub checks: Vec<WptElementCheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WptElementCheck {
    pub id: String,
    pub expected_x: Option<f32>,
    pub expected_y: Option<f32>,
    pub expected_width: Option<f32>,
    pub expected_height: Option<f32>,
    pub expected_color_rgb: Option<[u8; 3]>,
    #[serde(default = "default_tol")]
    pub tolerance: f32,
}

fn default_vp_w() -> f32 {
    800.0
}
fn default_vp_h() -> f32 {
    600.0
}
fn default_tol() -> f32 {
    1.5
}

#[derive(Debug, Clone)]
pub struct WptSubsetResult {
    pub subset: String,
    pub passed: usize,
    pub total: usize,
    pub baseline: usize,
    pub failed_names: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct WptReport {
    pub subsets: Vec<WptSubsetResult>,
    pub regressions: Vec<String>,
}

/// Runs one WPT subset (or `"all"`) and enforces no regressions against `baseline_path`.
pub fn run_wpt_suite(
    wpt_dir: &Path,
    subset: &str,
    baseline_path: &Path,
    update_baseline: bool,
) -> Result<WptReport> {
    let mut baseline: WptBaseline = if baseline_path.exists() {
        let raw = fs::read_to_string(baseline_path)?;
        serde_json::from_str(&raw).with_context(|| format!("parse {}", baseline_path.display()))?
    } else {
        WptBaseline::default()
    };

    let target_subsets: Vec<String> = if subset == "all" {
        let mut dirs = Vec::new();
        if wpt_dir.exists() {
            for entry in fs::read_dir(wpt_dir)? {
                let p = entry?.path();
                if p.is_dir() {
                    if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                        dirs.push(name.to_string());
                    }
                }
            }
        }
        dirs.sort();
        dirs
    } else {
        vec![subset.to_string()]
    };

    let mut report = WptReport::default();
    for sub in target_subsets {
        let sub_dir = wpt_dir.join(&sub);
        let mut passed = 0usize;
        let mut total = 0usize;
        let mut failed_names = Vec::new();

        if sub_dir.exists() {
            let mut entries: Vec<_> = fs::read_dir(&sub_dir)?
                .filter_map(Result::ok)
                .map(|e| e.path())
                .collect();
            entries.sort();

            for path in entries {
                let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if fname.ends_with(".toml") {
                    let raw = fs::read_to_string(&path)?;
                    let manifest: WptAssertionManifest = toml::from_str(&raw)
                        .with_context(|| format!("parse WPT manifest {}", path.display()))?;
                    for case in &manifest.cases {
                        total += 1;
                        if run_assertion_case(case) {
                            passed += 1;
                        } else {
                            failed_names.push(case.name.clone());
                        }
                    }
                } else if fname.ends_with(".html") && !fname.ends_with("-ref.html") {
                    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                    let ref_path = sub_dir.join(format!("{stem}-ref.html"));
                    if ref_path.exists() {
                        total += 1;
                        let test_html = fs::read_to_string(&path)?;
                        let ref_html = fs::read_to_string(&ref_path)?;
                        let actual = render_html_rgba(&test_html, 320, 200)?;
                        let expected = render_html_rgba(&ref_html, 320, 200)?;
                        let diff = compare_rgba(&actual, &expected, 4);
                        if diff.diff_pixels == 0 {
                            passed += 1;
                        } else {
                            failed_names.push(stem.to_string());
                        }
                    }
                }
            }
        }

        let base_count = baseline.subsets.get(&sub).copied().unwrap_or(0);
        if update_baseline {
            baseline.subsets.insert(sub.clone(), passed);
        } else if passed < base_count {
            report.regressions.push(format!(
                "subset `{sub}` passed {passed}/{total} < baseline {base_count}"
            ));
        }

        report.subsets.push(WptSubsetResult {
            subset: sub,
            passed,
            total,
            baseline: base_count,
            failed_names,
        });
    }

    if update_baseline {
        if let Some(parent) = baseline_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            baseline_path,
            serde_json::to_string_pretty(&baseline)? + "\n",
        )?;
    }

    Ok(report)
}

fn run_assertion_case(case: &WptAssertionCase) -> bool {
    let viewport = Size::new(case.viewport_width, case.viewport_height);
    let media = MediaContext {
        viewport_width: viewport.width,
        viewport_height: viewport.height,
        ..MediaContext::default()
    };
    let mut doc = parse_html(&case.html);
    let mut styles = StyleContext::new(media);
    for css in extract_style_blocks(&doc) {
        styles.add_stylesheet(Arc::new(Stylesheet::parse(
            &css,
            CascadeOrigin::Author,
            &media,
        )));
    }
    if !case.css.trim().is_empty() {
        styles.add_stylesheet(Arc::new(Stylesheet::parse(
            &case.css,
            CascadeOrigin::Author,
            &media,
        )));
    }
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    let (ftree, _) = engine.layout_only(&doc, &styles, viewport, shared_font_db());
    let Some(root) = ftree.root.as_ref() else {
        return false;
    };

    for chk in &case.checks {
        let Some(node) = doc.get_element_by_id(&chk.id) else {
            return false;
        };
        if let Some(rgb) = chk.expected_color_rgb {
            let Some(style) = styles.style_for(node) else {
                return false;
            };
            let c = style.text.color;
            if [c.r, c.g, c.b] != rgb {
                return false;
            }
        }
        if chk.expected_x.is_some()
            || chk.expected_y.is_some()
            || chk.expected_width.is_some()
            || chk.expected_height.is_some()
        {
            let Some(rect) = find_abs_rect(&doc, root, &chk.id) else {
                return false;
            };
            if let Some(ex) = chk.expected_x {
                if (rect.min_x() - ex).abs() > chk.tolerance {
                    return false;
                }
            }
            if let Some(ey) = chk.expected_y {
                if (rect.min_y() - ey).abs() > chk.tolerance {
                    return false;
                }
            }
            if let Some(ew) = chk.expected_width {
                if (rect.size.width - ew).abs() > chk.tolerance {
                    return false;
                }
            }
            if let Some(eh) = chk.expected_height {
                if (rect.size.height - eh).abs() > chk.tolerance {
                    return false;
                }
            }
        }
    }
    true
}

fn find_abs_rect(doc: &Document, root: &Fragment, id: &str) -> Option<Rect> {
    let target = doc.get_element_by_id(id)?;
    fn walk(frag: &Fragment, target: rix_dom::NodeId, origin: Point) -> Option<Rect> {
        let abs = frag.absolute_border_rect(origin);
        if frag.dom_node == Some(target) {
            return Some(abs);
        }
        for ch in frag.children.iter() {
            if let Some(found) = walk(ch, target, abs.origin) {
                return Some(found);
            }
        }
        None
    }
    walk(root, target, Point::ZERO)
}

pub fn print_report(report: &WptReport) {
    println!("=== WPT Conformance Scorecard ===");
    println!(
        "  {:<14} {:>8} {:>8} {:>10}  Status",
        "Subset", "Passed", "Total", "Baseline"
    );
    for s in &report.subsets {
        let status = if s.passed < s.baseline {
            "REGRESSION"
        } else if s.passed == s.total {
            "PASS"
        } else {
            "OK (baseline)"
        };
        println!(
            "  {:<14} {:>8} {:>8} {:>10}  {}",
            s.subset, s.passed, s.total, s.baseline, status
        );
        for fail in &s.failed_names {
            println!("    - failed: {fail}");
        }
    }
}
