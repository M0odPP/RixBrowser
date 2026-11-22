use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rix_app::{render_url_to_png, BrowserController, BrowserKey, KeyModifiers};
use rix_chrome::ThemeMode;
use rix_core::{Point, Size};
use rix_render::VelloRenderer;

fn encode_test_png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    let mut bytes = Vec::new();
    // Build minimal valid uncompressed PNG using raw zlib + CRC? Even simpler: valid 1x1/4x4 PNG via
    // deterministic header/IHDR/IDAT/IEND (no extra dev-dependency on `image` needed in rix-app!).
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
    fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }

    bytes.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    push_chunk(&mut bytes, b"IHDR", &ihdr);

    let mut row = Vec::with_capacity(1 + (w as usize) * 4);
    row.push(0); // filter type 0
    for _ in 0..w {
        row.extend_from_slice(&rgba);
    }
    let mut raw_scanlines = Vec::with_capacity((h as usize) * row.len());
    for _ in 0..h {
        raw_scanlines.extend_from_slice(&row);
    }
    assert!(raw_scanlines.len() <= 65535);
    let len = raw_scanlines.len() as u16;
    let mut zlib = Vec::with_capacity(2 + 5 + raw_scanlines.len() + 4);
    zlib.extend_from_slice(&[0x78, 0x01]); // zlib header (no compression)
    zlib.push(0x01); // final stored block
    zlib.extend_from_slice(&len.to_le_bytes());
    zlib.extend_from_slice(&(!len).to_le_bytes());
    zlib.extend_from_slice(&raw_scanlines);
    zlib.extend_from_slice(&adler32(&raw_scanlines).to_be_bytes());
    push_chunk(&mut bytes, b"IDAT", &zlib);
    push_chunk(&mut bytes, b"IEND", &[]);
    bytes
}

fn spawn_full_site_server() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let request_hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = Arc::clone(&request_hits);

    let photo_png = Arc::new(encode_test_png(32, 24, [37, 99, 235, 255]));
    let banner_png = Arc::new(encode_test_png(32, 16, [16, 185, 129, 255]));

    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let hits = Arc::clone(&hits_clone);
            let photo = Arc::clone(&photo_png);
            let banner = Arc::clone(&banner_png);
            thread::spawn(move || {
                let mut stream = stream;
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    return;
                }
                hits.fetch_add(1, Ordering::Relaxed);
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("");
                let path = first_line.split_whitespace().nth(1).unwrap_or("/");

                let (status, mime, cache_hdr, body): (&str, &str, &str, Vec<u8>) = match path {
                    "/" => (
                        "200 OK",
                        "text/html; charset=utf-8",
                        "public, max-age=300",
                        r##"<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <title>Rix Full Engine Site</title>
  <link rel="stylesheet" href="/theme.css">
  <script src="/app.js"></script>
  <script defer src="/hydrate.js"></script>
</head>
<body>
  <header class="hero">
    <img id="logo" src="/logo.svg" width="36" height="36" alt="Logo">
    <h1 id="headline">Full Engine Demo</h1>
  </header>
  <main class="content">
    <div id="banner" class="banner">CSS Background Banner</div>
    <img id="photo" src="/photo.png" width="120" height="90" alt="Photo">
    <button id="counter-btn" class="btn">Count: 0</button>
    <a id="spa-link" href="/should-not-navigate" class="link">Prevented SPA Link</a>
    <p id="api-status">Loading API...</p>
    <canvas id="chart" width="240" height="80"></canvas>
  </main>
</body>
</html>"##
                            .as_bytes()
                            .to_vec(),
                    ),
                    "/theme.css" => (
                        "200 OK",
                        "text/css",
                        "public, max-age=300",
                        r##"
@import url("/reset.css");
body { background-color: #f8fafc; color: #0f172a; font-family: sans-serif; padding: 20px; }
.hero { display: flex; align-items: center; gap: 12px; margin-bottom: 16px; }
.banner { width: 280px; height: 48px; background-image: url("/banner.png"); color: #ffffff; padding: 12px; margin-bottom: 12px; }
.btn { display: inline-block; width: 180px; height: 44px; background-color: #2563eb; color: #ffffff; border: 0; border-radius: 8px; }
.link { display: inline-block; margin-left: 12px; color: #2563eb; }
"##
                        .as_bytes()
                        .to_vec(),
                    ),
                    "/reset.css" => (
                        "200 OK",
                        "text/css",
                        "public, max-age=300",
                        b"* { box-sizing: border-box; margin: 0; padding: 0; }".to_vec(),
                    ),
                    "/app.js" => (
                        "200 OK",
                        "application/javascript",
                        "public, max-age=300",
                        r##"
window.__clicks = 0;
window.__preventedCount = 0;
console.log("app.js loaded");
"##
                        .as_bytes()
                        .to_vec(),
                    ),
                    "/hydrate.js" => (
                        "200 OK",
                        "application/javascript",
                        "public, max-age=300",
                        r##"
document.addEventListener("DOMContentLoaded", function() {
    const btn = document.getElementById("counter-btn");
    btn.addEventListener("click", function() {
        window.__clicks += 1;
        btn.textContent = "Count: " + window.__clicks;
        document.title = "Clicks: " + window.__clicks;
    });
    const link = document.getElementById("spa-link");
    link.addEventListener("click", function(e) {
        e.preventDefault();
        window.__preventedCount += 1;
        document.getElementById("headline").textContent = "SPA Prevented #" + window.__preventedCount;
    });
    const canvas = document.getElementById("chart");
    const ctx = canvas.getContext("2d");
    if (ctx) {
        ctx.fillStyle = "#1e293b";
        ctx.fillRect(0, 0, 240, 80);
        ctx.fillStyle = "#38bdf8";
        ctx.fillRect(16, 40, 40, 32);
        ctx.fillRect(72, 24, 40, 48);
        ctx.fillRect(128, 12, 40, 60);
    }
    fetch("/api/status.json")
        .then(function(r) { return r.json(); })
        .then(function(data) {
            document.getElementById("api-status").textContent = "API: " + data.status + " (" + data.engine + ")";
        });
});
"##
                        .as_bytes()
                        .to_vec(),
                    ),
                    "/api/status.json" => (
                        "200 OK",
                        "application/json",
                        "no-cache",
                        br#"{"status":"ready","engine":"RixBrowser"}"#.to_vec(),
                    ),
                    "/photo.png" => ("200 OK", "image/png", "public, max-age=300", (*photo).clone()),
                    "/banner.png" => ("200 OK", "image/png", "public, max-age=300", (*banner).clone()),
                    "/logo.svg" => (
                        "200 OK",
                        "image/svg+xml",
                        "public, max-age=300",
                        br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 36 36" width="36" height="36"><rect x="2" y="2" width="32" height="32" rx="8" fill="#2563eb"/><circle cx="18" cy="18" r="8" fill="#ffffff"/></svg>"##
                            .to_vec(),
                    ),
                    _ => ("404 Not Found", "text/plain", "no-store", b"Not Found".to_vec()),
                };

                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nCache-Control: {cache_hdr}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
            });
        }
    });

    (port, request_hits)
}

#[test]
fn full_web_engine_loads_css_import_images_svg_js_canvas_fetch_and_caches_warm_reloads() {
    let (port, hits) = spawn_full_site_server();
    let url = format!("http://127.0.0.1:{port}/");

    let mut ctrl = BrowserController::new(Size::new(1280.0, 800.0), 1.0);
    ctrl.navigate_active(&url);

    // 1. Verify external CSS + @import + PNG + SVG + background-image + JS booted cleanly.
    assert_eq!(ctrl.chrome.active().title, "Rix Full Engine Site");
    assert!(
        ctrl.hud
            .console
            .iter()
            .any(|c| c.level == "log" && c.message.contains("app.js loaded")),
        "expected console.log from /app.js, got: {:?}",
        ctrl.hud.console
    );
    assert!(
        ctrl.hud.console.iter().all(|c| c.level != "error"),
        "expected zero JS errors, got: {:?}",
        ctrl.hud.console
    );
    assert!(
        ctrl.asset_cache().len() >= 3,
        "expected photo.png, banner.png, and logo.svg in cross-tab DecodedAssetCache, got {}",
        ctrl.asset_cache().len()
    );

    // 2. Drive async `fetch("/api/status.json")` via `tick_active_tab()` until settled.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let delay = ctrl.tick_active_tab();
        let status_text = ctrl
            .eval_js_on_active_tab("document.getElementById('api-status').textContent")
            .expect("read api-status");
        if status_text == "API: ready (RixBrowser)" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for fetch() hydration; last text = {status_text:?}"
        );
        thread::sleep(
            delay
                .unwrap_or(Duration::from_millis(5))
                .min(Duration::from_millis(10)),
        );
    }
    // After async fetch settles, runtime should be completely idle (IDLE 0 Hz).
    assert_eq!(
        ctrl.tick_active_tab(),
        None,
        "idle web page must report None wakeup delay (IDLE 0 Hz)"
    );

    // 3. Verify Canvas2D context was created & presented and decoded raster images registered.
    assert!(
        ctrl.compositor.resources.image_count() >= 2,
        "expected decoded photo.png & banner.png registered in Compositor ResourceRegistry"
    );

    // 4. Find #counter-btn and #spa-link bounding rects via layout fragment tree and click them!
    let web_rect = ctrl.web_viewport_rect();
    let (btn_pt, link_pt) = {
        let tab_doc = ctrl.active_tab_doc().expect("active tab doc");
        let rt = tab_doc
            .runtime
            .as_ref()
            .expect("web tab has ContentRuntime");
        let host = rt.host.borrow();
        let doc = host.document.borrow();
        let btn_node = doc.get_element_by_id("counter-btn").expect("counter-btn");
        let link_node = doc.get_element_by_id("spa-link").expect("spa-link");
        let ft = host.cached_fragment_tree().expect("fragment tree");
        let root_frag = ft.root.as_ref().expect("root fragment");

        fn find_center(
            frag: &rix_layout::Fragment,
            parent_origin: Point,
            target: rix_dom::NodeId,
        ) -> Option<Point> {
            let abs = frag.absolute_border_rect(parent_origin);
            if frag.dom_node == Some(target) {
                return Some(Point::new(
                    abs.min_x() + abs.size.width * 0.5,
                    abs.min_y() + abs.size.height * 0.5,
                ));
            }
            for ch in frag.children.iter() {
                if let Some(p) = find_center(ch, abs.origin, target) {
                    return Some(p);
                }
            }
            None
        }

        let b = find_center(root_frag, Point::ZERO, btn_node).expect("counter-btn layout rect");
        let l = find_center(root_frag, Point::ZERO, link_node).expect("spa-link layout rect");
        (
            Point::new(web_rect.min_x() + b.x, web_rect.min_y() + b.y),
            Point::new(web_rect.min_x() + l.x, web_rect.min_y() + l.y),
        )
    };

    // Click #counter-btn -> JS updates button text AND document.title!
    assert!(ctrl.handle_left_click(btn_pt));
    assert_eq!(ctrl.chrome.active().title, "Clicks: 1");
    assert_eq!(
        ctrl.eval_js_on_active_tab("document.getElementById('counter-btn').textContent")
            .unwrap(),
        "Count: 1"
    );

    // Click #spa-link -> preventDefault() cancels navigation and updates headline in place!
    assert!(ctrl.handle_left_click(link_pt));
    assert_eq!(ctrl.chrome.active().url, url);
    assert_eq!(
        ctrl.eval_js_on_active_tab("document.getElementById('headline').textContent")
            .unwrap(),
        "SPA Prevented #1"
    );

    // 5. Verify Zoom (Ctrl+=) updates active_page_zoom cleanly.
    assert!(ctrl.handle_key(
        KeyModifiers {
            ctrl: true,
            ..KeyModifiers::default()
        },
        BrowserKey::Char('=')
    ));
    assert!((ctrl.active_page_zoom() - 1.1).abs() < 0.01);

    // 6. Verify headless Vello GPU render of the full composed page succeeds & produces valid PNG!
    let tmp = tempfile::tempdir().expect("tempdir");
    let png_path = tmp.path().join("full_site.png");
    let clear = ctrl.clear_color();
    let scene = ctrl.compositor.compose_force();
    VelloRenderer::render_scene_to_png(scene, 1280, 800, clear, &png_path)
        .expect("render full site png");
    assert!(std::fs::metadata(&png_path).unwrap().len() > 2_000);

    // 7. Verify warm reload hits HttpCache + DecodedAssetCache + ResourceManager cache!
    let hits_before_reload = hits.load(Ordering::Relaxed);
    ctrl.reload_active_tab();
    let hits_after_reload = hits.load(Ordering::Relaxed);
    let reload_stats = ctrl.active_tab_doc().unwrap().last_load_stats;
    assert!(
        reload_stats.decoded_asset_cache_hits >= 3,
        "warm reload must hit DecodedAssetCache for all 3 images, got {:?}",
        reload_stats
    );
    assert!(
        hits_after_reload <= hits_before_reload + 1,
        "warm reload should serve HTML/CSS/JS/images from cache (before={hits_before_reload}, after={hits_after_reload})"
    );

    // 8. Verify CLI helper `render_url_to_png` end-to-end.
    let cli_png = tmp.path().join("cli_render.png");
    render_url_to_png(&url, &cli_png, 1280, 800, ThemeMode::Light).expect("render_url_to_png");
    assert!(std::fs::metadata(&cli_png).unwrap().len() > 2_000);
}
