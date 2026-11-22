//! Web Platform Tests (WPT) subset runner & baseline scorecard guard
//! (`cargo xtask wpt --subset <name>`).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rix_content::ContentRuntime;
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

/// Declarative layout/style, JS/DOM/Event/Timer, and live-server Fetch/CORS/WS cases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WptAssertionManifest {
    #[serde(default)]
    pub cases: Vec<WptAssertionCase>,
    #[serde(default)]
    pub script_cases: Vec<WptScriptCase>,
    #[serde(default)]
    pub network_cases: Vec<WptNetworkCase>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WptNetworkCase {
    pub name: String,
    #[serde(default = "default_net_origin")]
    pub origin_mode: String,
    pub script: String,
    #[serde(default = "default_assert_expr")]
    pub assert_expr: String,
    #[serde(default = "default_expected_str")]
    pub expected: String,
}

fn default_net_origin() -> String {
    "same".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WptScriptCase {
    pub name: String,
    #[serde(default = "default_script_html")]
    pub html: String,
    pub script: String,
    #[serde(default)]
    pub advance_ms: u64,
    #[serde(default)]
    pub vsync_frames: u32,
    #[serde(default = "default_assert_expr")]
    pub assert_expr: String,
    #[serde(default = "default_expected_str")]
    pub expected: String,
}

fn default_script_html() -> String {
    "<!doctype html><html><body></body></html>".to_string()
}
fn default_assert_expr() -> String {
    "__pass".to_string()
}
fn default_expected_str() -> String {
    "true".to_string()
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
                    for case in &manifest.script_cases {
                        total += 1;
                        if run_script_case(case) {
                            passed += 1;
                        } else {
                            failed_names.push(case.name.clone());
                        }
                    }
                    for case in &manifest.network_cases {
                        total += 1;
                        if run_network_case(case) {
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

fn run_script_case(case: &WptScriptCase) -> bool {
    static FONT_DB: std::sync::OnceLock<Arc<rix_text::FontDatabase>> = std::sync::OnceLock::new();
    let font_db = Arc::clone(FONT_DB.get_or_init(|| Arc::new(rix_text::FontDatabase::new())));
    let doc = Rc::new(RefCell::new(Document::new()));
    let mut rt = ContentRuntime::new(doc, Size::new(800.0, 600.0), font_db);
    rt.event_loop.borrow_mut().enable_virtual_clock();
    if rt.load_html_with_scripts(&case.html).is_err() {
        return false;
    }
    if rt.eval_script(&case.script).is_err() {
        return false;
    }
    if case.advance_ms > 0 {
        // Step in small virtual ticks so nested/interval timers fire in exact chronological order.
        let mut remaining = case.advance_ms;
        while remaining > 0 {
            let step = remaining.min(5);
            rt.advance_and_drain(Duration::from_millis(step));
            remaining -= step;
        }
    } else {
        rt.advance_and_drain(Duration::ZERO);
    }
    for i in 1..=case.vsync_frames {
        rt.fire_vsync(f64::from(i) * 16.6);
    }
    match rt.eval_script(&case.assert_expr) {
        Ok(val) => rt.vm.value_to_rust_string(val) == case.expected,
        Err(_) => false,
    }
}

fn run_network_case(case: &WptNetworkCase) -> bool {
    let Ok(rt_tokio) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    rt_tokio.block_on(async move { run_network_case_async(case).await })
}

async fn run_network_case_async(case: &WptNetworkCase) -> bool {
    use rix_net::{compute_ws_accept_key, encode_server_frame};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let Ok(listener) = TcpListener::bind("127.0.0.1:0").await else {
        return false;
    };
    let Ok(addr) = listener.local_addr() else {
        return false;
    };
    let port = addr.port();

    let server_task = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let Ok(n) = sock.read(&mut buf).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let first = req.lines().next().unwrap_or("");
                let mut parts = first.split_whitespace();
                let method = parts.next().unwrap_or("GET");
                let path = parts.next().unwrap_or("/");
                let header_val = |name: &str| -> Option<String> {
                    req.lines().find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
                    })
                };

                // RFC 6455 WebSocket upgrade endpoint (`/ws`)
                if path == "/ws" {
                    if let Some(ws_key) = header_val("sec-websocket-key") {
                        let accept = compute_ws_accept_key(&ws_key);
                        let handshake = format!(
                            "HTTP/1.1 101 Switching Protocols\r\n\
                             Upgrade: websocket\r\n\
                             Connection: Upgrade\r\n\
                             Sec-WebSocket-Accept: {accept}\r\n\r\n"
                        );
                        let _ = sock.write_all(handshake.as_bytes()).await;
                        let mut acc = Vec::new();
                        let mut tmp = [0u8; 2048];
                        while let Ok(m) = sock.read(&mut tmp).await {
                            if m == 0 {
                                break;
                            }
                            acc.extend_from_slice(&tmp[..m]);
                            while acc.len() >= 6 {
                                let op = acc[0] & 0x0F;
                                let len = (acc[1] & 0x7F) as usize;
                                let frame_len = 6 + len;
                                if acc.len() < frame_len {
                                    break;
                                }
                                let mask = [acc[2], acc[3], acc[4], acc[5]];
                                let mut payload = acc[6..frame_len].to_vec();
                                for (i, b) in payload.iter_mut().enumerate() {
                                    *b ^= mask[i & 3];
                                }
                                acc.drain(..frame_len);
                                match op {
                                    0x1 | 0x2 => {
                                        let _ = sock
                                            .write_all(&encode_server_frame(op, &payload))
                                            .await;
                                    }
                                    0x8 => {
                                        let _ = sock
                                            .write_all(&encode_server_frame(0x8, &payload))
                                            .await;
                                        return;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    return;
                }

                let origin = header_val("origin").unwrap_or_else(|| "*".to_string());
                let resp = match (method, path) {
                    ("OPTIONS", "/cors-preflight") => format!(
                        "HTTP/1.1 204 No Content\r\n\
                         Access-Control-Allow-Origin: {origin}\r\n\
                         Access-Control-Allow-Methods: GET, POST, PUT, DELETE\r\n\
                         Access-Control-Allow-Headers: X-Wpt-Header, Content-Type\r\n\
                         Access-Control-Max-Age: 60\r\n\
                         Content-Length: 0\r\n\
                         Connection: close\r\n\r\n"
                    ),
                    (_, "/cors-preflight") => {
                        let body = r#"{"cors":"ok"}"#;
                        format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Access-Control-Allow-Origin: {origin}\r\n\
                             Access-Control-Expose-Headers: X-Exposed\r\n\
                             X-Exposed: yes\r\n\
                             X-Internal: no\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    (_, "/cors-open") => {
                        let body = "open-ok";
                        format!(
                            "HTTP/1.1 200 OK\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    (_, "/cors-denied") => {
                        let body = "denied";
                        format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    ("POST", "/echo") => {
                        let body = req.split_once("\r\n\r\n").map_or("", |(_, b)| b);
                        format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: text/plain\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    (_, "/json") => {
                        let body = r#"{"status":"ok","value":42}"#;
                        format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    _ => "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                        .to_string(),
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });

    let base_url = format!("http://127.0.0.1:{port}");
    let ws_url = format!("ws://127.0.0.1:{port}/ws");
    let doc_url = if case.origin_mode == "cross" {
        "http://wpt-origin.local:9999/".to_string()
    } else {
        format!("{base_url}/")
    };

    let mut rt = ContentRuntime::with_empty_document();
    rt.set_document_url(&doc_url);
    let prelude = format!(
        r#"var __WPT_BASE__ = "{base_url}"; var __WPT_WS__ = "{ws_url}"; var __done = false; var __pass = false;"#
    );
    if rt.eval_script(&prelude).is_err() || rt.eval_script(&case.script).is_err() {
        server_task.abort();
        return false;
    }
    let completed = rt
        .drive_async_until(Duration::from_secs(3), |rt| {
            rt.eval_script("__done")
                .is_ok_and(|v| v.as_bool() == Some(true))
        })
        .await;
    server_task.abort();
    if !completed {
        return false;
    }
    match rt.eval_script(&case.assert_expr) {
        Ok(val) => rt.vm.value_to_rust_string(val) == case.expected,
        Err(_) => false,
    }
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
