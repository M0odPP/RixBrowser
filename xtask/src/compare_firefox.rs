//! Head-to-head real-web differential benchmark & parity harness (`cargo xtask compare-firefox`)
//! comparing RixBrowser against Mozilla Firefox across HTML/DOM, Computed CSS, Layout Geometry,
//! Visual SSIM + diff heatmaps, JS console errors, Interactivity, Scroll FPS/p95/p99, Load speed,
//! Peak/Steady RSS, CPU time, and IDLE 0 Hz.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::diff_analyzer::{
    aggregate_subsystem_ranking, analyze_page_diff, compare_pngs_with_heatmap,
    format_subsystem_ranking_markdown, FrameTimingStats, PageInspectionSnapshot, SiteDiffAnalysis,
    SubsystemRanking, VisualDiffMetrics,
};
use crate::firefox_bidi::{
    default_firefox_bin, run_firefox_site, sample_process_tree_stats, FirefoxInteractiveReport,
    FirefoxRunConfig,
};

#[derive(Debug, Clone)]
pub struct CompareFirefoxOptions {
    pub cohort_path: PathBuf,
    pub out_dir: PathBuf,
    pub rix_bin: PathBuf,
    pub firefox_bin: PathBuf,
    pub width: u32,
    pub height: u32,
    pub dpr: f64,
    pub dark_mode: bool,
    pub interactive: bool,
    pub warm: bool,
    pub self_consistency: bool,
    pub limit: Option<usize>,
    pub report_json: Option<PathBuf>,
}

impl CompareFirefoxOptions {
    #[must_use]
    pub fn default_firefox() -> PathBuf {
        default_firefox_bin()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RixScenarioJson {
    #[serde(default)]
    scroll_gpu: FrameTimingStats,
    #[serde(default)]
    scroll_steps_applied: usize,
    #[serde(default)]
    hover_attempted: bool,
    #[serde(default)]
    resize_ok: bool,
    #[serde(default)]
    idle_frames_rendered: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RixMetricsJson {
    #[serde(default)]
    web_viewport_x: u32,
    #[serde(default)]
    web_viewport_y: u32,
    #[serde(default)]
    web_viewport_width: u32,
    #[serde(default)]
    web_viewport_height: u32,
    #[serde(default)]
    total_ms: f32,
    #[serde(default)]
    warm_total_ms: Option<f32>,
    #[serde(default)]
    css_parse_ms: f32,
    #[serde(default)]
    js_ms: f32,
    #[serde(default)]
    wave1_ms: f32,
    #[serde(default)]
    subresources_fetched: usize,
    #[serde(default)]
    js_error_count: usize,
    #[serde(default)]
    js_errors: Vec<String>,
    #[serde(default)]
    peak_rss_kib: u64,
    #[serde(default)]
    current_rss_kib: u64,
    #[serde(default)]
    cpu_time_ms: f64,
    #[serde(default)]
    idle_frames: u64,
    #[serde(default)]
    scenario: Option<RixScenarioJson>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirefoxSiteComparison {
    pub url: String,
    pub rix_ok: bool,
    pub firefox_ok: bool,
    pub firefox_error: Option<String>,
    pub rix_total_ms: f64,
    pub firefox_total_ms: f64,
    pub rix_warm_ms: Option<f64>,
    pub firefox_warm_ms: Option<f64>,
    pub rix_peak_rss_mib: f64,
    pub rix_steady_rss_mib: f64,
    pub firefox_peak_rss_mib: f64,
    pub firefox_steady_rss_mib: f64,
    pub rix_cpu_ms: f64,
    pub firefox_cpu_ms: f64,
    pub rix_idle_frames: u64,
    pub rix_js_errors: usize,
    pub firefox_js_errors: usize,
    pub rix_js_error_samples: Vec<String>,
    pub rix_subresources: usize,
    pub rix_wave1_ms: f32,
    pub rix_css_ms: f32,
    pub rix_js_ms: f32,
    pub rix_scroll_fps: Option<f64>,
    pub rix_scroll_p95_ms: Option<f64>,
    pub rix_scroll_jank: Option<usize>,
    pub firefox_scroll_fps: Option<f64>,
    pub firefox_scroll_p95_ms: Option<f64>,
    pub firefox_scroll_jank: Option<usize>,
    pub rix_interactive_ok: Option<bool>,
    pub firefox_interactive: Option<FirefoxInteractiveReport>,
    pub visual: Option<VisualDiffMetrics>,
    pub diff: Option<SiteDiffAnalysis>,
    pub rix_png: String,
    pub firefox_png: String,
    pub heatmap_png: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompareFirefoxReport {
    pub cohort: String,
    pub viewport: String,
    pub sites_total: usize,
    pub rix_succeeded: usize,
    pub firefox_succeeded: usize,
    pub median_rix_ms: f64,
    pub median_firefox_ms: f64,
    pub p90_rix_ms: f64,
    pub p90_firefox_ms: f64,
    pub median_rix_rss_mib: f64,
    pub median_firefox_rss_mib: f64,
    pub median_rix_cpu_ms: f64,
    pub median_firefox_cpu_ms: f64,
    pub avg_visual_ssim_pct: f64,
    pub avg_style_match_pct: f64,
    pub avg_layout_match_pct: f64,
    pub total_extra_js_errors: usize,
    pub idle_zero_hz_sites: usize,
    pub avg_rix_scroll_fps: f64,
    pub avg_firefox_scroll_fps: f64,
    pub subsystem_ranking: SubsystemRanking,
    pub sites: Vec<FirefoxSiteComparison>,
}

fn sanitize_slug(url: &str, idx: usize) -> String {
    let host_part: String = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .chars()
        .take(36)
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("{idx:02}_{host_part}")
}

fn run_monitored_rix(mut cmd: Command, timeout: Duration) -> (bool, f64, u64, f64) {
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let start = Instant::now();
    let Ok(mut child) = cmd.spawn() else {
        return (false, 0.0, 0, 0.0);
    };
    let pid = child.id();
    let mut peak_rss_kib = 0u64;
    let mut max_cpu_ms = 0.0f64;

    loop {
        let (rss, cpu) = sample_process_tree_stats(pid);
        peak_rss_kib = peak_rss_kib.max(rss);
        max_cpu_ms = max_cpu_ms.max(cpu);
        match child.try_wait() {
            Ok(Some(status)) => {
                return (
                    status.success(),
                    start.elapsed().as_secs_f64() * 1000.0,
                    peak_rss_kib,
                    max_cpu_ms,
                );
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (
                        false,
                        start.elapsed().as_secs_f64() * 1000.0,
                        peak_rss_kib,
                        max_cpu_ms,
                    );
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                let _ = child.kill();
                return (
                    false,
                    start.elapsed().as_secs_f64() * 1000.0,
                    peak_rss_kib,
                    max_cpu_ms,
                );
            }
        }
    }
}

fn quantile(mut vals: Vec<f64>, q: f64) -> f64 {
    if vals.is_empty() {
        return 0.0;
    }
    vals.sort_by(|a, b| a.total_cmp(b));
    let idx = ((vals.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize;
    vals[idx.min(vals.len() - 1)]
}

/// Runs RixBrowser vs Mozilla Firefox across every URL in `opts.cohort_path`.
pub fn run_compare_firefox(opts: &CompareFirefoxOptions) -> Result<CompareFirefoxReport> {
    if !opts.rix_bin.exists() {
        bail!(
            "RixBrowser binary not found at {} (build with `CARGO_BUILD_JOBS=3 cargo build --release -p rix-app`)",
            opts.rix_bin.display()
        );
    }
    if !opts.firefox_bin.exists() {
        bail!(
            "Firefox binary not found at {} (pass --firefox-bin or install to ~/.local/opt/firefox/firefox)",
            opts.firefox_bin.display()
        );
    }

    let raw = fs::read_to_string(&opts.cohort_path)
        .with_context(|| format!("read cohort {}", opts.cohort_path.display()))?;
    let mut urls: Vec<String> = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect();
    if let Some(lim) = opts.limit {
        urls.truncate(lim);
    }

    fs::create_dir_all(&opts.out_dir)?;
    let chrome_height_logical = 117u32;
    let rix_phys_w = ((f64::from(opts.width) * opts.dpr).round() as u32).max(64);
    let rix_phys_h =
        ((f64::from(opts.height + chrome_height_logical) * opts.dpr).round() as u32).max(64);

    let mut report = CompareFirefoxReport {
        cohort: opts.cohort_path.display().to_string(),
        viewport: format!("{}x{}@{:.1}x", opts.width, opts.height, opts.dpr),
        sites_total: urls.len(),
        ..CompareFirefoxReport::default()
    };

    for (idx, url) in urls.iter().enumerate() {
        let slug = sanitize_slug(url, idx + 1);
        let rix_png = opts.out_dir.join(format!("{slug}_rix.png"));
        let rix_json = opts.out_dir.join(format!("{slug}_rix.json"));
        let rix_dom_json = opts.out_dir.join(format!("{slug}_rix_dom.json"));
        let rix_post_png = opts.out_dir.join(format!("{slug}_rix_post.png"));
        let ff_png = opts.out_dir.join(format!("{slug}_firefox.png"));
        let ff_self_png = opts.out_dir.join(format!("{slug}_firefox_self.png"));
        let ff_post_png = opts.out_dir.join(format!("{slug}_firefox_post.png"));
        let ff_dom_json = opts.out_dir.join(format!("{slug}_firefox_dom.json"));
        let heatmap_png = opts.out_dir.join(format!("{slug}_diff.png"));

        // 1. Run RixBrowser headless.
        let mut rix_cmd = Command::new(&opts.rix_bin);
        rix_cmd
            .arg(format!("--render-url={url}"))
            .arg(format!("--out={}", rix_png.display()))
            .arg(format!("--metrics-json={}", rix_json.display()))
            .arg(format!("--dump-dom-layout={}", rix_dom_json.display()))
            .arg(format!("--width={rix_phys_w}"))
            .arg(format!("--height={rix_phys_h}"))
            .arg(format!("--dpr={:.2}", opts.dpr))
            .arg(if opts.dark_mode { "--dark" } else { "--light" });
        if opts.interactive {
            rix_cmd
                .arg("--interactive")
                .arg(format!("--post-out={}", rix_post_png.display()));
        }
        if opts.warm {
            rix_cmd.arg("--warm");
        }
        let (rix_ok, rix_wall_ms, rix_tree_rss_kib, rix_tree_cpu_ms) =
            run_monitored_rix(rix_cmd, Duration::from_secs(30));

        let rix_metrics: RixMetricsJson = fs::read_to_string(&rix_json)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let rix_snap: Option<PageInspectionSnapshot> = fs::read_to_string(&rix_dom_json)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());

        // 2. Run Mozilla Firefox via W3C WebDriver BiDi.
        let ff_cfg = FirefoxRunConfig {
            firefox_bin: opts.firefox_bin.clone(),
            url: url.clone(),
            out_png: ff_png.clone(),
            self_check_png: opts.self_consistency.then(|| ff_self_png.clone()),
            post_action_png: opts.interactive.then(|| ff_post_png.clone()),
            width: opts.width,
            height: opts.height,
            dpr: opts.dpr,
            dark_mode: opts.dark_mode,
            run_interactive: opts.interactive,
            measure_warm: opts.warm,
            timeout: Duration::from_secs(25),
        };
        let ff_out = run_firefox_site(&ff_cfg);
        if let Some(ref snap) = ff_out.snapshot {
            if let Ok(s) = serde_json::to_string_pretty(snap) {
                let _ = fs::write(&ff_dom_json, s);
            }
        }

        // 3. Visual SSIM + heatmap + Firefox self-consistency normalization.
        let mut visual = if rix_ok && ff_out.ok && rix_png.exists() && ff_png.exists() {
            compare_pngs_with_heatmap(
                &rix_png,
                &ff_png,
                rix_metrics.web_viewport_x,
                rix_metrics.web_viewport_y,
                rix_metrics.web_viewport_width,
                rix_metrics.web_viewport_height,
                Some(&heatmap_png),
            )
        } else {
            None
        };
        if let (Some(ref mut vis), true) =
            (&mut visual, opts.self_consistency && ff_self_png.exists())
        {
            if let Some(self_vis) =
                compare_pngs_with_heatmap(&ff_self_png, &ff_png, 0, 0, 0, 0, None)
            {
                vis.firefox_self_ssim_pct = Some(self_vis.ssim_pct);
                if self_vis.ssim_pct > 1.0 {
                    vis.normalized_ssim_pct =
                        Some(((vis.ssim_pct / self_vis.ssim_pct) * 100.0).min(100.0));
                }
            }
        }

        // 4. Element-by-element DOM / ComputedStyle / Layout-Box differential analysis.
        let diff = match (ff_out.snapshot.as_ref(), rix_snap.as_ref()) {
            (Some(ff_s), Some(rx_s)) => Some(analyze_page_diff(
                ff_s,
                rx_s,
                rix_metrics.js_error_count,
                ff_out.js_errors.len(),
                visual.as_ref(),
            )),
            _ => None,
        };

        if rix_ok {
            report.rix_succeeded += 1;
        }
        if ff_out.ok {
            report.firefox_succeeded += 1;
        }

        let rix_total_ms = if rix_metrics.total_ms > 0.0 {
            f64::from(rix_metrics.total_ms)
        } else {
            rix_wall_ms
        };
        let rix_peak_kib = rix_tree_rss_kib.max(rix_metrics.peak_rss_kib);
        let rix_steady_kib = if rix_metrics.current_rss_kib > 0 {
            rix_metrics.current_rss_kib
        } else {
            rix_peak_kib
        };
        let rix_cpu_ms = if rix_metrics.cpu_time_ms > 0.0 {
            rix_metrics.cpu_time_ms
        } else {
            rix_tree_cpu_ms
        };
        let rix_idle_frames = rix_metrics
            .scenario
            .as_ref()
            .map_or(rix_metrics.idle_frames, |s| s.idle_frames_rendered);

        let rix_scroll_fps = rix_metrics.scenario.as_ref().map(|s| s.scroll_gpu.avg_fps);
        let rix_scroll_p95_ms = rix_metrics.scenario.as_ref().map(|s| s.scroll_gpu.p95_ms);
        let rix_scroll_jank = rix_metrics
            .scenario
            .as_ref()
            .map(|s| s.scroll_gpu.jank_frames_over_16ms);
        let rix_interactive_ok = rix_metrics
            .scenario
            .as_ref()
            .map(|s| s.resize_ok && (s.scroll_steps_applied > 0 || s.hover_attempted));

        let ff_scroll_fps = ff_out.interactive.as_ref().map(|i| i.scroll_raf.avg_fps);
        let ff_scroll_p95_ms = ff_out.interactive.as_ref().map(|i| i.scroll_raf.p95_ms);
        let ff_scroll_jank = ff_out
            .interactive
            .as_ref()
            .map(|i| i.scroll_raf.jank_frames_over_16ms);

        let ssim_str = visual
            .map(|v| format!("{:.1}%", v.ssim_pct))
            .unwrap_or_else(|| "n/a".into());
        let style_str = diff
            .as_ref()
            .map(|d| format!("{:.1}%", d.style_match_pct))
            .unwrap_or_else(|| "n/a".into());
        let lay_str = diff
            .as_ref()
            .map(|d| format!("{:.1}%", d.layout_match_pct))
            .unwrap_or_else(|| "n/a".into());
        let sub_str = diff
            .as_ref()
            .map_or("n/a", |d| d.primary_subsystem.as_str());

        eprintln!(
            "[{:02}/{:02}] {:<40} | SSIM {:>6} | CSS {:>6} | Lay<=2px {:>6} | Rix {:>6.0}ms/{:>5.0}MiB vs FF {:>6.0}ms/{:>5.0}MiB | JSerr {}/{} | {}",
            idx + 1,
            urls.len(),
            url.chars().take(40).collect::<String>(),
            ssim_str,
            style_str,
            lay_str,
            rix_total_ms,
            rix_peak_kib as f64 / 1024.0,
            ff_out.total_ms,
            ff_out.peak_rss_kib as f64 / 1024.0,
            rix_metrics.js_error_count,
            ff_out.js_errors.len(),
            sub_str,
        );

        report.sites.push(FirefoxSiteComparison {
            url: url.clone(),
            rix_ok,
            firefox_ok: ff_out.ok,
            firefox_error: ff_out.error,
            rix_total_ms,
            firefox_total_ms: ff_out.total_ms,
            rix_warm_ms: rix_metrics.warm_total_ms.map(f64::from),
            firefox_warm_ms: ff_out.warm_total_ms,
            rix_peak_rss_mib: rix_peak_kib as f64 / 1024.0,
            rix_steady_rss_mib: rix_steady_kib as f64 / 1024.0,
            firefox_peak_rss_mib: ff_out.peak_rss_kib as f64 / 1024.0,
            firefox_steady_rss_mib: ff_out.steady_rss_kib as f64 / 1024.0,
            rix_cpu_ms,
            firefox_cpu_ms: ff_out.cpu_time_ms,
            rix_idle_frames,
            rix_js_errors: rix_metrics.js_error_count,
            firefox_js_errors: ff_out.js_errors.len(),
            rix_js_error_samples: rix_metrics.js_errors.into_iter().take(12).collect(),
            rix_subresources: rix_metrics.subresources_fetched,
            rix_wave1_ms: rix_metrics.wave1_ms,
            rix_css_ms: rix_metrics.css_parse_ms,
            rix_js_ms: rix_metrics.js_ms,
            rix_scroll_fps,
            rix_scroll_p95_ms,
            rix_scroll_jank,
            firefox_scroll_fps: ff_scroll_fps,
            firefox_scroll_p95_ms: ff_scroll_p95_ms,
            firefox_scroll_jank: ff_scroll_jank,
            rix_interactive_ok,
            firefox_interactive: ff_out.interactive,
            visual,
            diff,
            rix_png: rix_png.display().to_string(),
            firefox_png: ff_png.display().to_string(),
            heatmap_png: heatmap_png.display().to_string(),
        });
    }

    // Aggregate summary statistics across cohort.
    let ok_pairs: Vec<&FirefoxSiteComparison> = report
        .sites
        .iter()
        .filter(|s| s.rix_ok && s.firefox_ok)
        .collect();
    report.median_rix_ms = quantile(ok_pairs.iter().map(|s| s.rix_total_ms).collect(), 0.50);
    report.median_firefox_ms =
        quantile(ok_pairs.iter().map(|s| s.firefox_total_ms).collect(), 0.50);
    report.p90_rix_ms = quantile(ok_pairs.iter().map(|s| s.rix_total_ms).collect(), 0.90);
    report.p90_firefox_ms = quantile(ok_pairs.iter().map(|s| s.firefox_total_ms).collect(), 0.90);
    report.median_rix_rss_mib =
        quantile(ok_pairs.iter().map(|s| s.rix_peak_rss_mib).collect(), 0.50);
    report.median_firefox_rss_mib = quantile(
        ok_pairs.iter().map(|s| s.firefox_peak_rss_mib).collect(),
        0.50,
    );
    report.median_rix_cpu_ms = quantile(ok_pairs.iter().map(|s| s.rix_cpu_ms).collect(), 0.50);
    report.median_firefox_cpu_ms =
        quantile(ok_pairs.iter().map(|s| s.firefox_cpu_ms).collect(), 0.50);

    let ssims: Vec<f64> = ok_pairs
        .iter()
        .filter_map(|s| s.visual.map(|v| v.ssim_pct))
        .collect();
    if !ssims.is_empty() {
        report.avg_visual_ssim_pct = ssims.iter().sum::<f64>() / ssims.len() as f64;
    }
    let styles: Vec<f64> = ok_pairs
        .iter()
        .filter_map(|s| s.diff.as_ref().map(|d| d.style_match_pct))
        .collect();
    if !styles.is_empty() {
        report.avg_style_match_pct = styles.iter().sum::<f64>() / styles.len() as f64;
    }
    let layouts: Vec<f64> = ok_pairs
        .iter()
        .filter_map(|s| s.diff.as_ref().map(|d| d.layout_match_pct))
        .collect();
    if !layouts.is_empty() {
        report.avg_layout_match_pct = layouts.iter().sum::<f64>() / layouts.len() as f64;
    }
    report.total_extra_js_errors = ok_pairs
        .iter()
        .map(|s| s.rix_js_errors.saturating_sub(s.firefox_js_errors))
        .sum();
    report.idle_zero_hz_sites = ok_pairs.iter().filter(|s| s.rix_idle_frames == 0).count();

    let rx_fps: Vec<f64> = ok_pairs.iter().filter_map(|s| s.rix_scroll_fps).collect();
    if !rx_fps.is_empty() {
        report.avg_rix_scroll_fps = rx_fps.iter().sum::<f64>() / rx_fps.len() as f64;
    }
    let ff_fps: Vec<f64> = ok_pairs
        .iter()
        .filter_map(|s| s.firefox_scroll_fps)
        .collect();
    if !ff_fps.is_empty() {
        report.avg_firefox_scroll_fps = ff_fps.iter().sum::<f64>() / ff_fps.len() as f64;
    }

    let ranking_inputs: Vec<(&SiteDiffAnalysis, &[String], usize)> = ok_pairs
        .iter()
        .filter_map(|s| {
            s.diff
                .as_ref()
                .map(|d| (d, s.rix_js_error_samples.as_slice(), s.firefox_js_errors))
        })
        .collect();
    report.subsystem_ranking = aggregate_subsystem_ranking(&ranking_inputs);

    let triage_md = format_subsystem_ranking_markdown(&report.subsystem_ranking);
    let _ = fs::write(opts.out_dir.join("subsystem_ranking.md"), &triage_md);

    let report_path = opts
        .report_json
        .clone()
        .unwrap_or_else(|| opts.out_dir.join("report.json"));
    if let Some(parent) = report_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&report_path, serde_json::to_string_pretty(&report)?)?;

    Ok(report)
}

/// Prints a concise head-to-head scorecard comparing RixBrowser vs Mozilla Firefox.
pub fn print_report(report: &CompareFirefoxReport) {
    println!("\n=== RixBrowser vs Mozilla Firefox Differential Scorecard ===");
    println!("Cohort:   {} ({})", report.cohort, report.viewport);
    println!(
        "Loaded:   Rix {}/{} | Firefox {}/{}",
        report.rix_succeeded, report.sites_total, report.firefox_succeeded, report.sites_total
    );
    println!(
        "Visual:   avg SSIM {:.2}% | avg CSS match {:.2}% | avg Layout <=2px {:.2}%",
        report.avg_visual_ssim_pct, report.avg_style_match_pct, report.avg_layout_match_pct
    );
    println!(
        "Speed:    median Rix {:.0} ms vs Firefox {:.0} ms | p90 Rix {:.0} ms vs Firefox {:.0} ms",
        report.median_rix_ms, report.median_firefox_ms, report.p90_rix_ms, report.p90_firefox_ms
    );
    println!(
        "Memory:   median Peak RSS Rix {:.1} MiB vs Firefox {:.1} MiB",
        report.median_rix_rss_mib, report.median_firefox_rss_mib
    );
    println!(
        "CPU/Idle: median CPU Rix {:.0} ms vs Firefox {:.0} ms | IDLE 0 Hz: {}/{}",
        report.median_rix_cpu_ms,
        report.median_firefox_cpu_ms,
        report.idle_zero_hz_sites,
        report.rix_succeeded
    );
    if report.avg_rix_scroll_fps > 0.0 || report.avg_firefox_scroll_fps > 0.0 {
        println!(
            "Smooth:   avg Scroll FPS Rix {:.1} vs Firefox {:.1}",
            report.avg_rix_scroll_fps, report.avg_firefox_scroll_fps
        );
    }
    println!(
        "JS/Imgs:  extra JS errors vs Firefox = {} | sites w/ extra broken imgs = {}",
        report.total_extra_js_errors, report.subsystem_ranking.sites_with_visible_broken_images
    );
    println!("Top diverging subsystems:");
    for (sub, w) in report.subsystem_ranking.subsystem_totals.iter().take(6) {
        println!("  - {sub:<30} signal={w}");
    }
}
