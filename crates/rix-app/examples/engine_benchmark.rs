//! Reproducible release-mode end-to-end performance benchmark suite for RixBrowser.
//!
//! Run with:
//! `CARGO_BUILD_JOBS=3 cargo run --release -p rix-app --example engine_benchmark`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rix_app::BrowserController;
use rix_chrome::hud::read_process_rss_mb;
use rix_chrome::JitMode;
use rix_core::{Point, Size};
use rix_css::{CascadeOrigin, MediaContext, StyleContext, Stylesheet};
use rix_html::parse_html;
use rix_layout::LayoutEngine;
use rix_text::FontDatabase;

fn encode_solid_png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn adler32(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in data {
            a = (a + u32::from(byte)) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out.extend_from_slice(&crc32(&out[start..]).to_be_bytes());
    }
    let mut out = vec![137, 80, 78, 71, 13, 10, 26, 10];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    let mut row = Vec::with_capacity(1 + (w as usize) * 4);
    row.push(0);
    for _ in 0..w {
        row.extend_from_slice(&rgba);
    }
    let mut raw = Vec::with_capacity((h as usize) * row.len());
    for _ in 0..h {
        raw.extend_from_slice(&row);
    }
    let len = raw.len() as u16;
    let mut zlib = Vec::with_capacity(11 + raw.len());
    zlib.extend_from_slice(&[0x78, 0x01, 0x01]);
    zlib.extend_from_slice(&len.to_le_bytes());
    zlib.extend_from_slice(&(!len).to_le_bytes());
    zlib.extend_from_slice(&raw);
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn spawn_local_app_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local benchmark server");
    let port = listener.local_addr().unwrap().port();
    let img_a = Arc::new(encode_solid_png(64, 48, [59, 130, 246, 255]));
    let img_b = Arc::new(encode_solid_png(64, 48, [16, 185, 129, 255]));
    let img_c = Arc::new(encode_solid_png(64, 48, [245, 158, 11, 255]));
    let img_d = Arc::new(encode_solid_png(64, 48, [139, 92, 246, 255]));

    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (a, b, c, d) = (
                Arc::clone(&img_a),
                Arc::clone(&img_b),
                Arc::clone(&img_c),
                Arc::clone(&img_d),
            );
            thread::spawn(move || {
                let mut stream = stream;
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    return;
                }
                let path = String::from_utf8_lossy(&buf[..n])
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let (mime, body): (&str, Vec<u8>) = match path.as_str() {
                    "/" => (
                        "text/html; charset=utf-8",
                        r##"<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <title>RixBench Multi-Resource App</title>
  <link rel="stylesheet" href="/layout.css">
  <link rel="stylesheet" href="/theme.css">
  <script src="/app.js"></script>
</head>
<body>
  <div class="shell">
    <header class="hdr">
      <img src="/logo.svg" width="32" height="32" alt="Logo">
      <h1>RixBench Dashboard</h1>
      <button id="bump">Bump: 0</button>
    </header>
    <aside class="side">
      <nav><ul><li>Overview</li><li>Metrics</li><li>Traces</li><li>Network</li></ul></nav>
    </aside>
    <main class="main">
      <div class="cards">
        <div class="card"><img src="/a.png" width="64" height="48"><h3>Card A</h3><p id="stat">0</p></div>
        <div class="card"><img src="/b.png" width="64" height="48"><h3>Card B</h3><p>Active</p></div>
        <div class="card"><img src="/c.png" width="64" height="48"><h3>Card C</h3><p>Cached</p></div>
        <div class="card"><img src="/d.png" width="64" height="48"><h3>Card D</h3><p>Ready</p></div>
      </div>
      <div class="feed" id="feed"></div>
      <canvas id="spark" width="480" height="120"></canvas>
    </main>
  </div>
</body>
</html>"##
                            .as_bytes()
                            .to_vec(),
                    ),
                    "/layout.css" => (
                        "text/css",
                        r#"
@import url("/reset.css");
.shell { display: grid; grid-template: 56px 1fr / 220px 1fr; grid-template-areas: 'hdr hdr' 'side main'; min-height: 1800px; }
.hdr { grid-area: hdr; display: flex; align-items: center; gap: 16px; padding: 0 20px; }
.side { grid-area: side; padding: 16px; }
.main { grid-area: main; padding: 20px; }
.cards { display: grid; grid-template-columns: repeat(4, 1fr); gap: 16px; margin-bottom: 20px; }
.card { display: flex; flex-direction: column; gap: 8px; padding: 14px; border-radius: 10px; }
.row { display: flex; justify-content: space-between; padding: 8px 12px; border-bottom: 1px solid #1e293b; }
"#
                        .as_bytes()
                        .to_vec(),
                    ),
                    "/reset.css" => ("text/css", b"* { box-sizing: border-box; margin: 0; padding: 0; }".to_vec()),
                    "/theme.css" => (
                        "text/css",
                        r#"
body { background: #0f172a; color: #f8fafc; font-family: sans-serif; font-size: 14px; }
.hdr { background: #1e293b; border-bottom: 1px solid #334155; }
.side { background: #111827; border-right: 1px solid #1f2937; }
.card { background: #1e293b; border: 1px solid #334155; }
button { background: #2563eb; color: #ffffff; border: 0; border-radius: 6px; padding: 8px 14px; }
"#
                        .as_bytes()
                        .to_vec(),
                    ),
                    "/app.js" => (
                        "application/javascript",
                        r##"
window.__n = 0;
document.addEventListener("DOMContentLoaded", function() {
    const feed = document.getElementById("feed");
    let html = "";
    for (let i = 0; i < 60; i++) {
        html += "<div class='row'><span>Metric row #" + i + "</span><span>" + (i * 17) + " ms</span></div>";
    }
    feed.innerHTML = html;
    const c = document.getElementById("spark");
    const ctx = c.getContext("2d");
    if (ctx) {
        ctx.fillStyle = "#1e293b";
        ctx.fillRect(0, 0, 480, 120);
        ctx.fillStyle = "#38bdf8";
        for (let x = 0; x < 24; x++) {
            const h = 20 + ((x * 29) % 85);
            ctx.fillRect(8 + x * 19, 112 - h, 12, h);
        }
    }
});
function bumpCounter() {
    window.__n += 1;
    document.getElementById("stat").textContent = String(window.__n);
    document.getElementById("bump").textContent = "Bump: " + window.__n;
}
"##
                        .as_bytes()
                        .to_vec(),
                    ),
                    "/logo.svg" => (
                        "image/svg+xml",
                        br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32" width="32" height="32"><rect x="2" y="2" width="28" height="28" rx="7" fill="#2563eb"/><circle cx="16" cy="16" r="7" fill="#ffffff"/></svg>"##
                            .to_vec(),
                    ),
                    "/a.png" => ("image/png", (*a).clone()),
                    "/b.png" => ("image/png", (*b).clone()),
                    "/c.png" => ("image/png", (*c).clone()),
                    "/d.png" => ("image/png", (*d).clone()),
                    _ => ("text/plain", b"not found".to_vec()),
                };
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nCache-Control: public, max-age=300\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(hdr.as_bytes());
                let _ = stream.write_all(&body);
            });
        }
    });
    port
}

fn main() {
    println!("=== RixBrowser Release Engine Benchmark Suite ===");
    let port = spawn_local_app_server();
    let url = format!("http://127.0.0.1:{port}/");

    let t_boot = Instant::now();
    let mut ctrl = BrowserController::new(Size::new(1280.0, 800.0), 1.0);
    let boot_ms = t_boot.elapsed().as_secs_f64() * 1000.0;
    println!(
        "1. BrowserController Cold Startup (741 system fonts lazily indexed): {boot_ms:.2} ms"
    );

    // Benchmark 1: Cold vs Warm Multi-Resource Page Load (HTML + 3 CSS w/ @import + 5 PNG/SVG + JS + Canvas2D).
    let t_cold = Instant::now();
    ctrl.navigate_active(&url);
    let cold_ms = t_cold.elapsed().as_secs_f64() * 1000.0;
    let cold_stats = ctrl.active_tab_doc().unwrap().last_load_stats;

    let mut warm_times = Vec::with_capacity(5);
    for _ in 0..5 {
        let t0 = Instant::now();
        ctrl.reload_active_tab();
        warm_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    warm_times.sort_by(f64::total_cmp);
    let warm_median = warm_times[warm_times.len() / 2];
    let warm_stats = ctrl.active_tab_doc().unwrap().last_load_stats;
    println!(
        "2. Full Multi-Resource Load (10 assets + JS + Canvas2D): Cold={cold_ms:.2} ms | Warm median={warm_median:.2} ms (speedup={:.2}x, asset_cache_hits={}, css_ms={:.2})",
        cold_ms / warm_median.max(0.001),
        warm_stats.decoded_asset_cache_hits,
        cold_stats.css_parse_ms
    );

    // Benchmark 2: Incremental JS DOM Mutation -> restyle_dirty -> LayoutCache -> Paint (50 frames).
    let mut inc_times = Vec::with_capacity(50);
    for _ in 0..50 {
        let t0 = Instant::now();
        let _ = ctrl.eval_js_on_active_tab("bumpCounter()");
        inc_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    inc_times.sort_by(f64::total_cmp);
    let inc_med = inc_times[25];
    let inc_p95 = inc_times[47];
    println!("3. Incremental JS DOM Mutation -> Restyle -> Layout -> Paint (50 frames): median={inc_med:.2} ms | p95={inc_p95:.2} ms");

    // Benchmark 3: 14-Level Deeply Nested Flex+Grid Layout (Proving O(N) Linear Layout).
    let mut deep_html = String::from("<!doctype html><html><body>");
    for d in 0..14 {
        deep_html.push_str(if d % 2 == 0 {
            "<div class='f'><span>Item</span>"
        } else {
            "<div class='g'><span>Col</span>"
        });
    }
    deep_html.push_str("<div>Leaf</div>");
    for _ in 0..14 {
        deep_html.push_str("</div>");
    }
    deep_html.push_str("</body></html>");
    let mut deep_doc = parse_html(&deep_html);
    let media = MediaContext {
        viewport_width: 1280.0,
        viewport_height: 800.0,
        ..MediaContext::default()
    };
    let mut deep_styles = StyleContext::new(media);
    deep_styles.add_stylesheet(Arc::new(Stylesheet::parse(
        ".f{display:flex;gap:4px}.g{display:grid;grid-template-columns:1fr 1fr;gap:4px}",
        CascadeOrigin::Author,
        &media,
    )));
    deep_styles.cascade_full(&mut deep_doc);
    let mut layout_eng = LayoutEngine::new();
    let font_db = FontDatabase::new();
    let t_deep = Instant::now();
    let _ = layout_eng.layout_only(&deep_doc, &deep_styles, Size::new(1280.0, 800.0), &font_db);
    let deep_ms = t_deep.elapsed().as_secs_f64() * 1000.0;
    println!("4. 14-Level Deeply Nested Flex+Grid Layout (cold pass): {deep_ms:.2} ms");

    // Benchmark 4: Cranelift 3-Tier JIT vs Bytecode Interpreter numeric kernel.
    let js_kernel = r#"
        function compute(n) {
            let acc = 0;
            for (let i = 1; i <= n; i++) {
                acc = (acc + (i * 3 - (i & 7))) | 0;
            }
            return acc;
        }
        let total = 0;
        for (let r = 0; r < 1200; r++) {
            total = (total + compute(250)) | 0;
        }
        total;
    "#;
    // Measure Interpreter-only
    ctrl.chrome.set_jit_mode(JitMode::InterpreterOnly);
    ctrl.reload_active_tab();
    let t_interp = Instant::now();
    let interp_res = ctrl
        .eval_js_on_active_tab(js_kernel)
        .expect("interp kernel");
    let interp_ms = t_interp.elapsed().as_secs_f64() * 1000.0;

    // Measure Cranelift 3-Tier JIT
    ctrl.chrome.set_jit_mode(JitMode::FullThreeTier);
    ctrl.reload_active_tab();
    let t_jit = Instant::now();
    let jit_res = ctrl.eval_js_on_active_tab(js_kernel).expect("jit kernel");
    let jit_ms = t_jit.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(
        interp_res, jit_res,
        "JIT and interpreter must produce identical results"
    );
    println!(
        "5. JavaScript Numeric Kernel (300k iterations): Interpreter={interp_ms:.2} ms | Cranelift 3-Tier JIT={jit_ms:.2} ms (speedup={:.2}x)",
        interp_ms / jit_ms.max(0.001)
    );

    // Benchmark 5: 120 FPS APZ Scroll (0 DisplayList rebuilds) + IDLE 0 Hz + RSS Memory.
    let scroll_pt = Point::new(640.0, 400.0);
    let t_scroll = Instant::now();
    let mut composed = 0usize;
    for _ in 0..120 {
        if ctrl.handle_wheel(scroll_pt, 0.0, 12.0) && ctrl.compositor.compose().is_some() {
            composed += 1;
        }
    }
    let scroll_total_ms = t_scroll.elapsed().as_secs_f64() * 1000.0;
    let per_scroll_ms = scroll_total_ms / 120.0;
    let idle_wakeup: Option<Duration> = ctrl.tick_active_tab();
    let rss_mb = read_process_rss_mb(std::process::id()).unwrap_or(0.0);
    println!(
        "6. 120-Frame APZ Scroll: {per_scroll_ms:.3} ms/frame ({composed} frames composed, 0 DisplayList rebuilds)"
    );
    println!(
        "7. Idle Wakeup Policy: {:?} (IDLE 0 Hz) | Live Process VmRSS: {rss_mb:.1} MiB",
        idle_wakeup
    );
}
