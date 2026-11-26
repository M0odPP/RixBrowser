//! Head-to-head real-web comparison harness (`cargo xtask compare-chrome`)
//! benchmarking RixBrowser against Google Chrome Stable on rotating cohorts of URLs.
//!
//! Measures wall-clock load time, full process-tree Peak RSS (via `/proc`), JS error count,
//! and perceptual visual similarity (block SSIM + mean absolute pixel delta) over the matched
//! web viewport area.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct CompareChromeOptions {
    pub cohort_path: PathBuf,
    pub out_dir: PathBuf,
    pub rix_bin: PathBuf,
    pub chrome_bin: PathBuf,
    pub limit: Option<usize>,
    pub report_json: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RixRenderMetrics {
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
    css_parse_ms: f32,
    #[serde(default)]
    js_ms: f32,
    #[serde(default)]
    subresources_fetched: usize,
    #[serde(default)]
    js_error_count: usize,
    #[serde(default)]
    peak_rss_kib: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SiteComparison {
    pub url: String,
    pub rix_ok: bool,
    pub chrome_ok: bool,
    pub rix_wall_ms: f64,
    pub chrome_wall_ms: f64,
    pub rix_peak_rss_mib: f64,
    pub chrome_peak_rss_mib: f64,
    pub rix_js_errors: usize,
    pub rix_subresources: usize,
    pub rix_css_ms: f32,
    pub rix_js_ms: f32,
    pub visual_ssim_pct: Option<f64>,
    pub mean_pixel_delta: Option<f64>,
    pub rix_png: String,
    pub chrome_png: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CompareChromeReport {
    pub cohort: String,
    pub sites_total: usize,
    pub rix_succeeded: usize,
    pub chrome_succeeded: usize,
    pub avg_rix_wall_ms: f64,
    pub avg_chrome_wall_ms: f64,
    pub avg_rix_rss_mib: f64,
    pub avg_chrome_rss_mib: f64,
    pub avg_visual_ssim_pct: f64,
    pub sites: Vec<SiteComparison>,
}

/// Samples total `VmRSS` (in KiB) of `root_pid` and all its descendant processes from `/proc`.
fn sample_process_tree_rss_kib(root_pid: u32) -> u64 {
    let Ok(entries) = fs::read_dir("/proc") else {
        return 0;
    };
    let mut parent_map: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut rss_map: HashMap<u32, u64> = HashMap::new();

    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let status_path = entry.path().join("status");
        let Ok(status) = fs::read_to_string(status_path) else {
            continue;
        };
        let mut ppid = 0u32;
        let mut rss = 0u64;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("PPid:") {
                if let Some(tok) = rest.split_whitespace().next() {
                    ppid = tok.parse().unwrap_or(0);
                }
            } else if let Some(rest) = line.strip_prefix("VmRSS:") {
                if let Some(tok) = rest.split_whitespace().next() {
                    rss = tok.parse().unwrap_or(0);
                }
            }
        }
        parent_map.entry(ppid).or_default().push(pid);
        rss_map.insert(pid, rss);
    }

    let mut visited = HashSet::new();
    let mut stack = vec![root_pid];
    let mut total_kib = 0u64;
    while let Some(pid) = stack.pop() {
        if !visited.insert(pid) {
            continue;
        }
        total_kib += rss_map.get(&pid).copied().unwrap_or(0);
        if let Some(children) = parent_map.get(&pid) {
            stack.extend(children.iter().copied());
        }
    }
    total_kib
}

fn run_monitored_command(mut cmd: Command, timeout: Duration) -> (bool, f64, u64) {
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let start = Instant::now();
    let Ok(mut child) = cmd.spawn() else {
        return (false, 0.0, 0);
    };
    let pid = child.id();
    let mut peak_rss_kib = 0u64;

    loop {
        let rss = sample_process_tree_rss_kib(pid);
        if rss > peak_rss_kib {
            peak_rss_kib = rss;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                return (status.success(), elapsed_ms, peak_rss_kib);
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (false, start.elapsed().as_secs_f64() * 1000.0, peak_rss_kib);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                let _ = child.kill();
                return (false, start.elapsed().as_secs_f64() * 1000.0, peak_rss_kib);
            }
        }
    }
}

/// Computes block-wise perceptual SSIM (0..100%) and mean absolute RGB delta over the matched web viewport.
fn compare_rendered_pngs(
    rix_png: &Path,
    chrome_png: &Path,
    crop_x: u32,
    crop_y: u32,
    crop_w: u32,
    crop_h: u32,
) -> Option<(f64, f64)> {
    let rix_img = image::open(rix_png).ok()?.to_rgb8();
    let chrome_img = image::open(chrome_png).ok()?.to_rgb8();

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

    let w = rw.min(chrome_img.width());
    let h = rh.min(chrome_img.height());
    if w < 16 || h < 16 {
        return None;
    }

    let mut abs_delta_sum = 0.0f64;
    let mut px_count = 0u64;
    let luma = |r: u8, g: u8, b: u8| -> f64 {
        0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)
    };

    // 8x8 block SSIM on luminance (standard structural similarity constants C1, C2).
    const C1: f64 = (0.01 * 255.0) * (0.01 * 255.0);
    const C2: f64 = (0.03 * 255.0) * (0.03 * 255.0);
    let mut ssim_sum = 0.0f64;
    let mut blocks = 0u64;

    let bx_count = w / 8;
    let by_count = h / 8;
    for by in 0..by_count {
        for bx in 0..bx_count {
            let mut sum_x = 0.0;
            let mut sum_y = 0.0;
            let mut sum_xx = 0.0;
            let mut sum_yy = 0.0;
            let mut sum_xy = 0.0;

            for dy in 0..8 {
                for dx in 0..8 {
                    let x = bx * 8 + dx;
                    let y = by * 8 + dy;
                    let rp = rix_img.get_pixel(rx + x, ry + y).0;
                    let cp = chrome_img.get_pixel(x, y).0;
                    abs_delta_sum += (f64::from(rp[0]) - f64::from(cp[0])).abs()
                        + (f64::from(rp[1]) - f64::from(cp[1])).abs()
                        + (f64::from(rp[2]) - f64::from(cp[2])).abs();
                    px_count += 3;

                    let lx = luma(rp[0], rp[1], rp[2]);
                    let ly = luma(cp[0], cp[1], cp[2]);
                    sum_x += lx;
                    sum_y += ly;
                    sum_xx += lx * lx;
                    sum_yy += ly * ly;
                    sum_xy += lx * ly;
                }
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

    let ssim_pct = if blocks == 0 {
        0.0
    } else {
        (ssim_sum / blocks as f64) * 100.0
    };
    let mean_delta = if px_count == 0 {
        255.0
    } else {
        abs_delta_sum / px_count as f64
    };
    Some((ssim_pct, mean_delta))
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

/// Runs RixBrowser vs Google Chrome head-to-head on every URL in `opts.cohort_path`.
pub fn run_compare_chrome(opts: &CompareChromeOptions) -> Result<CompareChromeReport> {
    if !opts.rix_bin.exists() {
        bail!(
            "RixBrowser binary not found at {} (build with `CARGO_BUILD_JOBS=3 cargo build --release -p rix-app`)",
            opts.rix_bin.display()
        );
    }
    if !opts.chrome_bin.exists() {
        bail!(
            "Chrome binary not found at {} (pass --chrome-bin)",
            opts.chrome_bin.display()
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

    let mut report = CompareChromeReport {
        cohort: opts.cohort_path.display().to_string(),
        sites_total: urls.len(),
        ..CompareChromeReport::default()
    };

    for (idx, url) in urls.iter().enumerate() {
        let slug = sanitize_slug(url, idx + 1);
        let rix_png = opts.out_dir.join(format!("{slug}_rix.png"));
        let rix_json = opts.out_dir.join(format!("{slug}_rix.json"));
        let chrome_png = opts.out_dir.join(format!("{slug}_chrome.png"));
        let chrome_profile = opts.out_dir.join(format!("{slug}_chrome_profile"));
        let _ = fs::remove_dir_all(&chrome_profile);

        // 1. Run RixBrowser headless in Light mode (matching Chromium default) with an exact
        // 1280x800 web viewport (`800 + 117px` top chrome = `917px` total window height).
        let mut rix_cmd = Command::new(&opts.rix_bin);
        rix_cmd
            .arg(format!("--render-url={url}"))
            .arg(format!("--out={}", rix_png.display()))
            .arg(format!("--metrics-json={}", rix_json.display()))
            .arg("--width=1280")
            .arg("--height=917")
            .arg("--light");
        let (rix_ok, rix_wall_ms, rix_tree_rss_kib) =
            run_monitored_command(rix_cmd, Duration::from_secs(25));

        let rix_metrics: RixRenderMetrics = fs::read_to_string(&rix_json)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let rix_peak_kib = rix_tree_rss_kib.max(rix_metrics.peak_rss_kib);

        // 2. Run Google Chrome headless with isolated temporary profile.
        let mut chrome_cmd = Command::new(&opts.chrome_bin);
        chrome_cmd
            .arg("--headless=new")
            .arg("--no-sandbox")
            .arg("--disable-gpu")
            .arg("--disable-dev-shm-usage")
            .arg("--hide-scrollbars")
            .arg(format!("--user-data-dir={}", chrome_profile.display()))
            .arg("--window-size=1280,800")
            .arg("--virtual-time-budget=5000")
            .arg(format!("--screenshot={}", chrome_png.display()))
            .arg(url);
        let (chrome_ok, chrome_wall_ms, chrome_peak_kib) =
            run_monitored_command(chrome_cmd, Duration::from_secs(25));
        let _ = fs::remove_dir_all(&chrome_profile);

        let visual = if rix_ok && chrome_ok && rix_png.exists() && chrome_png.exists() {
            compare_rendered_pngs(
                &rix_png,
                &chrome_png,
                rix_metrics.web_viewport_x,
                rix_metrics.web_viewport_y,
                rix_metrics.web_viewport_width,
                rix_metrics.web_viewport_height,
            )
        } else {
            None
        };

        if rix_ok {
            report.rix_succeeded += 1;
        }
        if chrome_ok {
            report.chrome_succeeded += 1;
        }

        let effective_rix_ms = if rix_metrics.total_ms > 0.0 {
            f64::from(rix_metrics.total_ms)
        } else {
            rix_wall_ms
        };

        report.sites.push(SiteComparison {
            url: url.clone(),
            rix_ok,
            chrome_ok,
            rix_wall_ms: effective_rix_ms,
            chrome_wall_ms,
            rix_peak_rss_mib: rix_peak_kib as f64 / 1024.0,
            chrome_peak_rss_mib: chrome_peak_kib as f64 / 1024.0,
            rix_js_errors: rix_metrics.js_error_count,
            rix_subresources: rix_metrics.subresources_fetched,
            rix_css_ms: rix_metrics.css_parse_ms,
            rix_js_ms: rix_metrics.js_ms,
            visual_ssim_pct: visual.map(|v| v.0),
            mean_pixel_delta: visual.map(|v| v.1),
            rix_png: rix_png.display().to_string(),
            chrome_png: chrome_png.display().to_string(),
        });
    }

    let ok_rix: Vec<&SiteComparison> = report.sites.iter().filter(|s| s.rix_ok).collect();
    if !ok_rix.is_empty() {
        report.avg_rix_wall_ms =
            ok_rix.iter().map(|s| s.rix_wall_ms).sum::<f64>() / ok_rix.len() as f64;
        report.avg_rix_rss_mib =
            ok_rix.iter().map(|s| s.rix_peak_rss_mib).sum::<f64>() / ok_rix.len() as f64;
    }
    let ok_chr: Vec<&SiteComparison> = report.sites.iter().filter(|s| s.chrome_ok).collect();
    if !ok_chr.is_empty() {
        report.avg_chrome_wall_ms =
            ok_chr.iter().map(|s| s.chrome_wall_ms).sum::<f64>() / ok_chr.len() as f64;
        report.avg_chrome_rss_mib =
            ok_chr.iter().map(|s| s.chrome_peak_rss_mib).sum::<f64>() / ok_chr.len() as f64;
    }
    let ssims: Vec<f64> = report
        .sites
        .iter()
        .filter_map(|s| s.visual_ssim_pct)
        .collect();
    if !ssims.is_empty() {
        report.avg_visual_ssim_pct = ssims.iter().sum::<f64>() / ssims.len() as f64;
    }

    if let Some(ref json_path) = opts.report_json {
        if let Some(parent) = json_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(json_path, serde_json::to_string_pretty(&report)? + "\n")?;
    }

    Ok(report)
}

pub fn print_report(report: &CompareChromeReport) {
    println!("=== RixBrowser vs Google Chrome Head-to-Head ===");
    println!(
        "  Cohort: {} ({} sites | Rix OK: {} | Chrome OK: {})",
        report.cohort, report.sites_total, report.rix_succeeded, report.chrome_succeeded
    );
    println!(
        "  Averages: Rix {:.0} ms vs Chrome {:.0} ms | Rix {:.1} MiB vs Chrome {:.1} MiB | Visual SSIM {:.1}%",
        report.avg_rix_wall_ms,
        report.avg_chrome_wall_ms,
        report.avg_rix_rss_mib,
        report.avg_chrome_rss_mib,
        report.avg_visual_ssim_pct,
    );
    println!(
        "  {:<38} {:>9} {:>9} {:>9} {:>9} {:>7} {:>6}",
        "URL", "Rix ms", "Chr ms", "Rix MiB", "Chr MiB", "SSIM%", "JSErr"
    );
    for s in &report.sites {
        let short_url: String = s.url.chars().take(38).collect();
        let ssim_str = s
            .visual_ssim_pct
            .map_or_else(|| "N/A".to_string(), |v| format!("{v:.1}%"));
        println!(
            "  {:<38} {:>9.0} {:>9.0} {:>9.1} {:>9.1} {:>7} {:>6}",
            short_url,
            s.rix_wall_ms,
            s.chrome_wall_ms,
            s.rix_peak_rss_mib,
            s.chrome_peak_rss_mib,
            ssim_str,
            s.rix_js_errors
        );
    }
}
