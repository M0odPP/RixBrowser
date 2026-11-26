//! Differential analyzer comparing RixBrowser against Mozilla Firefox across:
//! - Perceptual visual similarity (8x8 block SSIM, mean RGB delta, red-highlight diff heatmap PNG)
//! - Structural DOM, `getComputedStyle` (34 properties), and `getBoundingClientRect` (`<= 2px`)
//! - Broken/missing image subresources & JS console error parity
//! - Cross-cohort architectural subsystem ranking (NEVER grouped by domain!)

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use image::{Rgb, RgbImage};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct RectSnapshot {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAuditSummary {
    pub total: usize,
    pub loaded: usize,
    pub broken: usize,
    pub visible_broken: usize,
}

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
    #[must_use]
    pub fn from_samples_ms(samples: &[f64]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let mut sorted = samples.to_vec();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let count = sorted.len();
        let avg = sorted.iter().sum::<f64>() / count as f64;
        let avg_fps = if avg > 0.001 {
            (1000.0 / avg).min(240.0)
        } else {
            240.0
        };
        let pct = |p: f64| -> f64 {
            let idx = ((count - 1) as f64 * p.clamp(0.0, 1.0)).round() as usize;
            sorted[idx.min(count - 1)]
        };
        Self {
            frame_count: count,
            avg_frame_ms: avg,
            avg_fps,
            p50_ms: pct(0.50),
            p95_ms: pct(0.95),
            p99_ms: pct(0.99),
            max_ms: *sorted.last().unwrap_or(&0.0),
            jank_frames_over_16ms: sorted.iter().filter(|&&ms| ms > 16.67).count(),
        }
    }
}

/// Visual comparison output including optional normalized score vs Firefox self-consistency.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct VisualDiffMetrics {
    pub ssim_pct: f64,
    pub mean_rgb_delta: f64,
    pub hot_blocks_pct: f64,
    pub firefox_self_ssim_pct: Option<f64>,
    pub normalized_ssim_pct: Option<f64>,
}

/// Single CSS property divergence observed across matched elements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropertyMismatchSample {
    pub property: String,
    pub count: usize,
    pub sample_path: String,
    pub firefox_val: String,
    pub rix_val: String,
}

/// Single geometric bounding-box divergence (> 2px) between Firefox and RixBrowser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutMismatchSample {
    pub path: String,
    pub tag: String,
    pub max_delta_px: f32,
    pub firefox_rect: RectSnapshot,
    pub rix_rect: RectSnapshot,
    pub styles_matched: bool,
}

/// Full structural differential analysis for one site.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SiteDiffAnalysis {
    pub firefox_elements: usize,
    pub rix_elements: usize,
    pub matched_elements: usize,
    pub dom_match_pct: f64,
    pub style_checks: usize,
    pub style_matches: usize,
    pub style_match_pct: f64,
    pub layout_within_2px: usize,
    pub layout_match_pct: f64,
    pub scroll_height_ratio: f64,
    pub rix_visible_broken_imgs: usize,
    pub firefox_visible_broken_imgs: usize,
    pub primary_subsystem: String,
    pub subsystem_signals: BTreeMap<String, usize>,
    pub top_style_mismatches: Vec<PropertyMismatchSample>,
    pub top_layout_mismatches: Vec<LayoutMismatchSample>,
}

/// Aggregated cross-cohort architectural diagnosis (strictly by engine subsystem / CSS property / JS error class).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubsystemRanking {
    pub subsystem_totals: Vec<(String, usize)>,
    pub top_css_property_mismatches: Vec<(String, usize, String, String)>,
    pub top_js_error_classes: Vec<(String, usize)>,
    pub sites_with_visible_broken_images: usize,
    pub sites_with_extra_js_errors: usize,
}

/// Computes 8x8 block SSIM, mean RGB delta, % of diverged blocks (delta > 24), and writes a
/// side-by-side / heatmap diff PNG highlighting mismatched regions in crimson.
pub fn compare_pngs_with_heatmap(
    rix_png: &Path,
    ref_png: &Path,
    crop_x: u32,
    crop_y: u32,
    crop_w: u32,
    crop_h: u32,
    heatmap_out: Option<&Path>,
) -> Option<VisualDiffMetrics> {
    let rix_img = image::open(rix_png).ok()?.to_rgb8();
    let ref_img = image::open(ref_png).ok()?.to_rgb8();

    let rx = crop_x.min(rix_img.width().saturating_sub(1));
    let ry = crop_y.min(rix_img.height().saturating_sub(1));
    let avail_rw = rix_img.width().saturating_sub(rx);
    let avail_rh = rix_img.height().saturating_sub(ry);
    let rw = if crop_w > 0 {
        crop_w.min(avail_rw)
    } else {
        avail_rw
    };
    let rh = if crop_h > 0 {
        crop_h.min(avail_rh)
    } else {
        avail_rh
    };

    let w = rw.min(ref_img.width());
    let h = rh.min(ref_img.height());
    if w < 16 || h < 16 {
        return None;
    }

    let mut heatmap = heatmap_out.map(|_| RgbImage::new(w, h));
    let mut abs_delta_sum = 0.0f64;
    let mut px_count = 0u64;
    let luma = |r: u8, g: u8, b: u8| -> f64 {
        0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)
    };

    const C1: f64 = (0.01 * 255.0) * (0.01 * 255.0);
    const C2: f64 = (0.03 * 255.0) * (0.03 * 255.0);
    let mut ssim_sum = 0.0f64;
    let mut blocks = 0u64;
    let mut hot_blocks = 0u64;

    let bx_count = w / 8;
    let by_count = h / 8;
    for by in 0..by_count {
        for bx in 0..bx_count {
            let mut sum_x = 0.0;
            let mut sum_y = 0.0;
            let mut sum_xx = 0.0;
            let mut sum_yy = 0.0;
            let mut sum_xy = 0.0;
            let mut block_delta = 0.0;

            for dy in 0..8 {
                for dx in 0..8 {
                    let x = bx * 8 + dx;
                    let y = by * 8 + dy;
                    let rp = rix_img.get_pixel(rx + x, ry + y).0;
                    let fp = ref_img.get_pixel(x, y).0;
                    let d = (f64::from(rp[0]) - f64::from(fp[0])).abs()
                        + (f64::from(rp[1]) - f64::from(fp[1])).abs()
                        + (f64::from(rp[2]) - f64::from(fp[2])).abs();
                    abs_delta_sum += d;
                    block_delta += d / 3.0;
                    px_count += 3;

                    if let Some(ref mut hm) = heatmap {
                        let px_d = d / 3.0;
                        if px_d > 24.0 {
                            let blend = ((px_d - 24.0) / 140.0).clamp(0.35, 1.0);
                            let r = (f64::from(rp[0]) * (1.0 - blend) + 235.0 * blend) as u8;
                            let g = (f64::from(rp[1]) * (1.0 - blend) + 32.0 * blend) as u8;
                            let b = (f64::from(rp[2]) * (1.0 - blend) + 48.0 * blend) as u8;
                            hm.put_pixel(x, y, Rgb([r, g, b]));
                        } else {
                            // Dim matching areas slightly so red diffs pop visually.
                            let gray = (luma(fp[0], fp[1], fp[2]) * 0.55) as u8;
                            hm.put_pixel(x, y, Rgb([gray, gray, gray]));
                        }
                    }

                    let lx = luma(rp[0], rp[1], rp[2]);
                    let ly = luma(fp[0], fp[1], fp[2]);
                    sum_x += lx;
                    sum_y += ly;
                    sum_xx += lx * lx;
                    sum_yy += ly * ly;
                    sum_xy += lx * ly;
                }
            }

            if (block_delta / 64.0) > 24.0 {
                hot_blocks += 1;
            }
            let n = 64.0;
            let mu_x = sum_x / n;
            let mu_y = sum_y / n;
            let var_x = (sum_xx / n - mu_x * mu_x).max(0.0);
            let var_y = (sum_yy / n - mu_y * mu_y).max(0.0);
            let cov_xy = sum_xy / n - mu_x * mu_y;
            let num = (2.0 * mu_x * mu_y + C1) * (2.0 * cov_xy + C2);
            let den = (mu_x * mu_x + mu_y * mu_y + C1) * (var_x + var_y + C2);
            let ssim = (num / den).clamp(-1.0, 1.0);
            ssim_sum += ssim.max(0.0);
            blocks += 1;
        }
    }

    if let (Some(out_path), Some(hm)) = (heatmap_out, heatmap) {
        let _ = hm.save(out_path);
    }

    let ssim_pct = if blocks == 0 {
        0.0
    } else {
        (ssim_sum / blocks as f64) * 100.0
    };
    let mean_rgb_delta = if px_count == 0 {
        255.0
    } else {
        abs_delta_sum / px_count as f64
    };
    let hot_blocks_pct = if blocks == 0 {
        0.0
    } else {
        (hot_blocks as f64 / blocks as f64) * 100.0
    };

    Some(VisualDiffMetrics {
        ssim_pct,
        mean_rgb_delta,
        hot_blocks_pct,
        firefox_self_ssim_pct: None,
        normalized_ssim_pct: None,
    })
}

/// Normalizes CSS computed values across Gecko and RixBrowser syntax variations
/// (e.g. `rgba(0, 0, 0, 0)` vs `transparent`, `16.0px` vs `16px`, whitespace/quotes).
#[must_use]
pub fn css_values_equivalent(prop: &str, firefox: &str, rix: &str) -> bool {
    let ff = normalize_css_val(prop, firefox);
    let rx = normalize_css_val(prop, rix);
    if ff == rx {
        return true;
    }
    // Numeric px tolerance (<= 0.6px accounts for subpixel layout/font rounding).
    if let (Some(a), Some(b)) = (parse_single_px(&ff), parse_single_px(&rx)) {
        return (a - b).abs() <= 0.6;
    }
    // Numeric unitless tolerance (opacity, font-weight, line-height).
    if let (Ok(a), Ok(b)) = (ff.parse::<f32>(), rx.parse::<f32>()) {
        return (a - b).abs() <= 0.05;
    }
    // Compare primary font-family name when system fallback stacks differ slightly.
    if prop == "font-family" {
        let f1 = primary_font_name(&ff);
        let r1 = primary_font_name(&rx);
        if !f1.is_empty() && f1 == r1 {
            return true;
        }
    }
    // `background-image`: treat `url("...")` and `url(...)` with identical URL as matching.
    if prop == "background-image" {
        let f_clean = ff.replace(['"', '\''], "");
        let r_clean = rx.replace(['"', '\''], "");
        if f_clean == r_clean {
            return true;
        }
    }
    // CSS Color 4 (`oklch`/`oklab`/`lab`/`lch`) floating-point serialization tolerance
    // (e.g. Gecko 6-sig-fig `0.00166669` vs Rix `0.001667` represent identical colors).
    (prop == "color" || prop == "background-color") && color4_values_close(&ff, &rx)
}

fn color4_values_close(a: &str, b: &str) -> bool {
    let Some((fa, ia)) = a
        .split_once('(')
        .and_then(|(f, r)| Some((f, r.strip_suffix(')')?)))
    else {
        return false;
    };
    let Some((fb, ib)) = b
        .split_once('(')
        .and_then(|(f, r)| Some((f, r.strip_suffix(')')?)))
    else {
        return false;
    };
    if fa != fb || !matches!(fa, "oklch" | "oklab" | "lab" | "lch" | "color") {
        return false;
    }
    let ta: Vec<&str> = ia.split_whitespace().collect();
    let tb: Vec<&str> = ib.split_whitespace().collect();
    if ta.len() != tb.len() {
        return false;
    }
    ta.iter().zip(tb.iter()).all(|(&xa, &xb)| {
        if xa == xb {
            return true;
        }
        match (xa.parse::<f64>(), xb.parse::<f64>()) {
            (Ok(na), Ok(nb)) => {
                let tol = if na.abs().max(nb.abs()) > 10.0 {
                    0.5
                } else {
                    0.003
                };
                (na - nb).abs() <= tol
            }
            _ => false,
        }
    })
}

fn normalize_css_val(prop: &str, raw: &str) -> String {
    let s = raw.trim().to_ascii_lowercase();
    if prop == "text-align" {
        match s.as_str() {
            "-moz-center" => return "center".to_string(),
            "-moz-left" => return "left".to_string(),
            "-moz-right" => return "right".to_string(),
            _ => {}
        }
    }
    if s == "transparent" || s == "rgba(0, 0, 0, 0)" || s == "rgb(0, 0, 0, 0)" {
        return "rgba(0, 0, 0, 0)".to_string();
    }
    if s == "0px" || s == "0" || s == "0.0px" {
        return if prop.contains("width")
            || prop.contains("height")
            || prop.contains("margin")
            || prop.contains("padding")
            || prop.contains("radius")
        {
            "0px".to_string()
        } else {
            "0".to_string()
        };
    }
    if prop == "font-weight" {
        if s == "normal" {
            return "400".to_string();
        }
        if s == "bold" {
            return "700".to_string();
        }
    }
    if prop == "z-index" && s == "auto" {
        return "auto".to_string();
    }
    // Normalize `rgb(r, g, b)` vs `rgba(r, g, b, 1)`.
    if let Some(inner) = s.strip_prefix("rgba(").and_then(|x| x.strip_suffix(')')) {
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() == 4 && (parts[3] == "1" || parts[3] == "1.0") {
            return format!("rgb({}, {}, {})", parts[0], parts[1], parts[2]);
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_single_px(s: &str) -> Option<f32> {
    s.strip_suffix("px")?.trim().parse::<f32>().ok()
}

fn primary_font_name(family: &str) -> String {
    family
        .split(',')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches(['"', '\''])
        .to_ascii_lowercase()
}

/// Runs element-by-element DOM, ComputedStyle, Layout-Box, and Image-Health diffing between
/// Firefox and RixBrowser, pinning every divergence to the responsible browser subsystem.
#[must_use]
pub fn analyze_page_diff(
    firefox: &PageInspectionSnapshot,
    rix: &PageInspectionSnapshot,
    rix_js_errors: usize,
    firefox_js_errors: usize,
    visual: Option<&VisualDiffMetrics>,
) -> SiteDiffAnalysis {
    let mut analysis = SiteDiffAnalysis {
        firefox_elements: firefox.elements.len(),
        rix_elements: rix.elements.len(),
        rix_visible_broken_imgs: rix.images.visible_broken,
        firefox_visible_broken_imgs: firefox.images.visible_broken,
        scroll_height_ratio: if firefox.scroll_height > 1.0 {
            f64::from(rix.scroll_height) / f64::from(firefox.scroll_height)
        } else {
            1.0
        },
        ..SiteDiffAnalysis::default()
    };

    let mut rix_by_path: HashMap<&str, &ElementSnapshot> = HashMap::new();
    let mut rix_by_id: HashMap<&str, &ElementSnapshot> = HashMap::new();
    for el in &rix.elements {
        rix_by_path.entry(el.path.as_str()).or_insert(el);
        if !el.id.is_empty() {
            rix_by_id.entry(el.id.as_str()).or_insert(el);
        }
    }

    let mut prop_mismatches: HashMap<String, (usize, String, String, String)> = HashMap::new();
    let mut layout_outliers: Vec<LayoutMismatchSample> = Vec::new();
    let mut signals: BTreeMap<String, usize> = BTreeMap::new();

    for ff_el in &firefox.elements {
        let matched = rix_by_path.get(ff_el.path.as_str()).copied().or_else(|| {
            if !ff_el.id.is_empty() {
                rix_by_id.get(ff_el.id.as_str()).copied()
            } else {
                None
            }
        });
        let Some(rx_el) = matched else {
            continue;
        };
        analysis.matched_elements += 1;

        let mut el_styles_ok = true;
        for (prop, ff_val) in &ff_el.styles {
            let rx_val = rx_el.styles.get(prop).map_or("", String::as_str);
            analysis.style_checks += 1;
            if css_values_equivalent(prop, ff_val, rx_val) {
                analysis.style_matches += 1;
            } else {
                el_styles_ok = false;
                let entry = prop_mismatches
                    .entry(prop.clone())
                    .or_insert_with(|| (0, ff_el.path.clone(), ff_val.clone(), rx_val.to_string()));
                entry.0 += 1;
                if prop == "font-family" || prop == "font-size" || prop == "line-height" {
                    *signals
                        .entry("rix-text/typography".to_string())
                        .or_default() += 1;
                } else {
                    *signals
                        .entry("rix-css/computed-style".to_string())
                        .or_default() += 1;
                }
            }
        }

        let dx = (ff_el.rect.x - rx_el.rect.x).abs();
        let dy = (ff_el.rect.y - rx_el.rect.y).abs();
        let dw = (ff_el.rect.width - rx_el.rect.width).abs();
        let dh = (ff_el.rect.height - rx_el.rect.height).abs();
        let max_d = dx.max(dy).max(dw).max(dh);
        if max_d <= 2.0 {
            analysis.layout_within_2px += 1;
        } else {
            if el_styles_ok {
                *signals
                    .entry("rix-layout/geometry".to_string())
                    .or_default() += 2;
            } else {
                *signals
                    .entry("rix-css/cascade-layout".to_string())
                    .or_default() += 1;
            }
            layout_outliers.push(LayoutMismatchSample {
                path: ff_el.path.clone(),
                tag: ff_el.tag.clone(),
                max_delta_px: max_d,
                firefox_rect: ff_el.rect,
                rix_rect: rx_el.rect,
                styles_matched: el_styles_ok,
            });
        }
    }

    if firefox.elements.is_empty() {
        analysis.dom_match_pct = 100.0;
    } else {
        analysis.dom_match_pct =
            (analysis.matched_elements as f64 / firefox.elements.len() as f64) * 100.0;
    }
    if analysis.dom_match_pct < 65.0 {
        *signals
            .entry("rix-js/dom-hydration".to_string())
            .or_default() += 40;
    }
    if rix.images.visible_broken > firefox.images.visible_broken {
        let extra = rix.images.visible_broken - firefox.images.visible_broken;
        *signals
            .entry("rix-net/subresources-images".to_string())
            .or_default() += extra * 15;
    }
    if rix_js_errors > firefox_js_errors {
        let extra = rix_js_errors - firefox_js_errors;
        *signals
            .entry("rix-js/runtime-errors".to_string())
            .or_default() += extra * 20;
    }

    analysis.style_match_pct = if analysis.style_checks == 0 {
        100.0
    } else {
        (analysis.style_matches as f64 / analysis.style_checks as f64) * 100.0
    };
    analysis.layout_match_pct = if analysis.matched_elements == 0 {
        0.0
    } else {
        (analysis.layout_within_2px as f64 / analysis.matched_elements as f64) * 100.0
    };

    if let Some(vis) = visual {
        if vis.ssim_pct < 85.0
            && analysis.layout_match_pct >= 80.0
            && analysis.style_match_pct >= 90.0
        {
            *signals
                .entry("rix-render/paint-svg-images".to_string())
                .or_default() += 50;
        }
    }

    let mut prop_vec: Vec<PropertyMismatchSample> = prop_mismatches
        .into_iter()
        .map(
            |(property, (count, sample_path, firefox_val, rix_val))| PropertyMismatchSample {
                property,
                count,
                sample_path,
                firefox_val,
                rix_val,
            },
        )
        .collect();
    prop_vec.sort_by_key(|a| std::cmp::Reverse(a.count));
    prop_vec.truncate(10);
    analysis.top_style_mismatches = prop_vec;

    layout_outliers.sort_by(|a, b| b.max_delta_px.total_cmp(&a.max_delta_px));
    layout_outliers.truncate(10);
    analysis.top_layout_mismatches = layout_outliers;

    analysis.primary_subsystem = signals
        .iter()
        .max_by_key(|(_, v)| *v)
        .map_or_else(|| "parity-ok".to_string(), |(k, _)| k.clone());
    analysis.subsystem_signals = signals;
    analysis
}

/// Classifies a raw JS error string into a canonical architectural error class (zero domain names).
#[must_use]
pub fn classify_js_error(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.contains("250 register") {
        return "rix-js/compiler: 250-register limit".to_string();
    }
    if trimmed.contains("Unexpected token Lt") || trimmed.contains("Unexpected token '<'") {
        return "rix-net/script-loader: HTML response executed as JS".to_string();
    }
    if trimmed.contains("Failed to load module") || trimmed.contains("Module error") {
        return "rix-content/esm: dynamic/transitive module load failure".to_string();
    }
    if trimmed.contains("private member") {
        return "rix-js/classes: private member brand check".to_string();
    }
    if trimmed.contains("before initialization") {
        return "rix-js/scope: lexical TDZ binding".to_string();
    }
    if let Some(pos) = trimmed.find(" is not defined") {
        let prefix = trimmed[..pos]
            .rsplit(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '$')
            .next()
            .unwrap_or("symbol");
        return format!("rix-content/webapi: missing global `{prefix}`");
    }
    if trimmed.contains("is not a function") {
        return "rix-js/webapi: method is not a function".to_string();
    }
    if trimmed.contains("Cannot read properties of") || trimmed.contains("Cannot set properties of")
    {
        return "rix-content/dom: null/undefined property access".to_string();
    }
    trimmed.chars().take(72).collect()
}

/// Aggregates differential diagnoses across all sites strictly by architectural subsystem.
#[must_use]
pub fn aggregate_subsystem_ranking(
    analyses: &[(&SiteDiffAnalysis, &[String], usize)],
) -> SubsystemRanking {
    let mut sub_totals: HashMap<String, usize> = HashMap::new();
    let mut prop_totals: HashMap<String, (usize, String, String)> = HashMap::new();
    let mut js_classes: HashMap<String, usize> = HashMap::new();
    let mut broken_img_sites = 0usize;
    let mut extra_js_sites = 0usize;

    for (diff, rix_errors, ff_err_count) in analyses {
        for (sub, &weight) in &diff.subsystem_signals {
            *sub_totals.entry(sub.clone()).or_default() += weight;
        }
        for pm in &diff.top_style_mismatches {
            let e = prop_totals
                .entry(pm.property.clone())
                .or_insert_with(|| (0, pm.firefox_val.clone(), pm.rix_val.clone()));
            e.0 += pm.count;
        }
        if diff.rix_visible_broken_imgs > diff.firefox_visible_broken_imgs {
            broken_img_sites += 1;
        }
        if rix_errors.len() > *ff_err_count {
            extra_js_sites += 1;
            for err in *rix_errors {
                let cls = classify_js_error(err);
                *js_classes.entry(cls).or_default() += 1;
            }
        }
    }

    let mut subsystem_totals: Vec<(String, usize)> = sub_totals.into_iter().collect();
    subsystem_totals.sort_by_key(|a| std::cmp::Reverse(a.1));

    let mut top_css_property_mismatches: Vec<(String, usize, String, String)> = prop_totals
        .into_iter()
        .map(|(p, (c, ff, rx))| (p, c, ff, rx))
        .collect();
    top_css_property_mismatches.sort_by_key(|a| std::cmp::Reverse(a.1));
    top_css_property_mismatches.truncate(20);

    let mut top_js_error_classes: Vec<(String, usize)> = js_classes.into_iter().collect();
    top_js_error_classes.sort_by_key(|a| std::cmp::Reverse(a.1));
    top_js_error_classes.truncate(20);

    SubsystemRanking {
        subsystem_totals,
        top_css_property_mismatches,
        top_js_error_classes,
        sites_with_visible_broken_images: broken_img_sites,
        sites_with_extra_js_errors: extra_js_sites,
    }
}

/// Formats a human-readable Markdown architectural triage report ranked by subsystem.
#[must_use]
pub fn format_subsystem_ranking_markdown(ranking: &SubsystemRanking) -> String {
    let mut md = String::new();
    md.push_str("# RixBrowser vs Firefox — Architectural Subsystem Triage\n\n");
    md.push_str("All divergences are aggregated strictly by engine subsystem, CSS property, and JS error class (zero domain-specific rules).\n\n");
    md.push_str(&format!(
        "- Sites with extra visible broken images vs Firefox: **{}**\n",
        ranking.sites_with_visible_broken_images
    ));
    md.push_str(&format!(
        "- Sites with extra JS console errors vs Firefox: **{}**\n\n",
        ranking.sites_with_extra_js_errors
    ));

    md.push_str(
        "## 1. Top Diverging Engine Subsystems\n\n| Subsystem | Weighted Signal |\n|---|---:|\n",
    );
    for (sub, w) in &ranking.subsystem_totals {
        md.push_str(&format!("| `{sub}` | {w} |\n"));
    }

    md.push_str("\n## 2. Top Computed CSS Property Mismatches\n\n| Property | Mismatched Elements | Sample Firefox | Sample Rix |\n|---|---:|---|---|\n");
    for (prop, count, ff, rx) in &ranking.top_css_property_mismatches {
        md.push_str(&format!(
            "| `{prop}` | {count} | `{}` | `{}` |\n",
            ff.replace('|', "\\|"),
            rx.replace('|', "\\|")
        ));
    }

    md.push_str("\n## 3. Top JS / WebAPI Error Classes\n\n| Architectural Error Class | Occurrences |\n|---|---:|\n");
    for (cls, count) in &ranking.top_js_error_classes {
        md.push_str(&format!("| `{}` | {count} |\n", cls.replace('|', "\\|")));
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_equivalence_normalizes_colors_px_and_font_weight() {
        assert!(css_values_equivalent(
            "background-color",
            "rgba(0, 0, 0, 0)",
            "transparent"
        ));
        assert!(css_values_equivalent(
            "color",
            "rgb(17, 24, 39)",
            "rgba(17, 24, 39, 1)"
        ));
        assert!(css_values_equivalent("font-weight", "400", "normal"));
        assert!(css_values_equivalent("font-size", "16px", "16.2px"));
        assert!(!css_values_equivalent("display", "grid", "block"));
    }

    #[test]
    fn analyzer_attributes_geometry_vs_style_divergences() {
        let mut styles = BTreeMap::new();
        styles.insert("display".into(), "flex".into());
        styles.insert("width".into(), "400px".into());
        let ff = PageInspectionSnapshot {
            elements: vec![ElementSnapshot {
                path: "html>body>main:nth-of-type(1)".into(),
                tag: "main".into(),
                rect: RectSnapshot {
                    x: 0.0,
                    y: 0.0,
                    width: 400.0,
                    height: 200.0,
                },
                styles: styles.clone(),
                ..ElementSnapshot::default()
            }],
            ..PageInspectionSnapshot::default()
        };
        let rx = PageInspectionSnapshot {
            elements: vec![ElementSnapshot {
                path: "html>body>main:nth-of-type(1)".into(),
                tag: "main".into(),
                rect: RectSnapshot {
                    x: 0.0,
                    y: 0.0,
                    width: 640.0,
                    height: 200.0,
                },
                styles,
                ..ElementSnapshot::default()
            }],
            ..PageInspectionSnapshot::default()
        };
        let diff = analyze_page_diff(&ff, &rx, 0, 0, None);
        assert_eq!(diff.style_match_pct, 100.0);
        assert_eq!(diff.layout_match_pct, 0.0);
        assert_eq!(diff.primary_subsystem, "rix-layout/geometry");
    }
}
