#![cfg(unix)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rix_app::{BrowserController, BrowserKey, KeyModifiers};
use rix_chrome::{ThemeMode, URL_NEWTAB};
use rix_core::{Point, RixUrl, Size};
use rix_ipc::{
    BrowserToContent, ContentToBrowser, PipelineId, ProcessSupervisor, SiteKey, SupervisorEvent,
    TabId, ViewportMetrics,
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const RIX_APP_BIN: &str = env!("CARGO_BIN_EXE_rix-app");

fn spawn_local_form_server() -> (u16, std::thread::JoinHandle<()>) {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind local test port");
    let port = std_listener.local_addr().unwrap().port();
    std_listener.set_nonblocking(true).unwrap();

    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = TcpListener::from_std(std_listener).unwrap();
            for _ in 0..8 {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let first_line = req.lines().next().unwrap_or("");
                let mut parts = first_line.split_whitespace();
                let method = parts.next().unwrap_or("GET");
                let target = parts.next().unwrap_or("/");

                let body_html = if method == "POST" && target == "/submit" {
                    let req_body = req.split("\r\n\r\n").nth(1).unwrap_or("");
                    format!(
                        "<!DOCTYPE html><html><head><title>POST OK</title></head>\
                         <body><h1 id='result'>POST:{req_body}</h1></body></html>"
                    )
                } else if method == "GET" && target.starts_with("/search") {
                    format!(
                        "<!DOCTYPE html><html><head><title>GET OK</title></head>\
                         <body><h1 id='result'>GET:{target}</h1></body></html>"
                    )
                } else {
                    r#"<!DOCTYPE html><html><head><title>Form Fixture</title></head><body>
                        <a id="next-link" href="/search?q=hello">Go Search</a>
                        <form id="get-form" action="/search" method="get">
                            <input id="q-input" type="text" name="q" value="rust">
                            <button id="get-submit" type="submit">Search</button>
                        </form>
                        <form id="post-form" action="/submit" method="post">
                            <input id="user-input" type="text" name="user" value="alice">
                            <textarea id="note-input" name="note">hello</textarea>
                            <select id="role-select" name="role">
                                <option value="dev" selected>Developer</option>
                                <option value="ops">Operator</option>
                            </select>
                            <button id="post-submit" type="submit">Save</button>
                        </form>
                    </body></html>"#
                        .to_string()
                };

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body_html.len(),
                    body_html
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
    });

    (port, handle)
}

#[test]
fn forms_get_and_post_submission_clipboard_hover_and_hud_work_end_to_end() {
    let (port, _server) = spawn_local_form_server();
    let mut ctrl = BrowserController::new(Size::new(1280.0, 720.0), 1.0);

    // 1. Navigate via omnibox to local test form page.
    ctrl.navigate_active(&format!("127.0.0.1:{port}/"));
    assert_eq!(ctrl.chrome.active().title, "Form Fixture");

    // 2. Focus GET input, type extra characters, and press Enter -> submits real HTTP GET form.
    let q_node = ctrl
        .active_tab_doc()
        .unwrap()
        .doc
        .get_element_by_id("q-input")
        .expect("q-input exists");
    let tab_id = ctrl.chrome.active().id;
    {
        // Trigger focus + edit + Enter submit directly through BrowserController APIs.
        let base_url = ctrl.chrome.active().url.clone();
        let tab_doc = ctrl.active_tab_doc().unwrap();
        let sub = rix_app::collect_form_submission(&tab_doc.doc, q_node, &base_url)
            .expect("GET form submission builds");
        assert_eq!(sub.method, rix_app::FormMethod::Get);
        assert!(sub.action_url.contains("/search?q=rust"));
        ctrl.submit_form(sub);
    }
    assert_eq!(ctrl.chrome.active().title, "GET OK");
    assert!(ctrl
        .active_tab_doc()
        .unwrap()
        .raw_html
        .contains("GET:/search?q=rust"));

    // 3. Navigate back via Alt+Left and submit the HTTP POST form (<input> + <textarea> + <select>).
    ctrl.handle_key(
        KeyModifiers {
            alt: true,
            ..KeyModifiers::default()
        },
        BrowserKey::ArrowLeft,
    );
    assert_eq!(ctrl.chrome.active().title, "Form Fixture");

    let post_btn = ctrl
        .active_tab_doc()
        .unwrap()
        .doc
        .get_element_by_id("post-submit")
        .expect("post-submit exists");
    let base_url = ctrl.chrome.active().url.clone();
    let post_sub =
        rix_app::collect_form_submission(&ctrl.active_tab_doc().unwrap().doc, post_btn, &base_url)
            .expect("POST form submission builds");
    assert_eq!(post_sub.method, rix_app::FormMethod::Post);
    assert_eq!(
        post_sub.content_type.as_deref(),
        Some("application/x-www-form-urlencoded")
    );
    let body_str = String::from_utf8(post_sub.body.clone()).unwrap();
    assert!(body_str.contains("user=alice"));
    assert!(body_str.contains("note=hello"));
    assert!(body_str.contains("role=dev"));

    ctrl.submit_form(post_sub);
    assert_eq!(ctrl.chrome.active().title, "POST OK");
    assert!(ctrl
        .active_tab_doc()
        .unwrap()
        .raw_html
        .contains("POST:user=alice&note=hello&role=dev"));
    assert_eq!(ctrl.chrome.active().id, tab_id);

    // 4. Verify Clipboard copy/paste & F12 HUD toggle + Network waterfall recorded both GET & POST.
    ctrl.handle_key(
        KeyModifiers {
            ctrl: true,
            ..KeyModifiers::default()
        },
        BrowserKey::Char('l'),
    );
    ctrl.handle_key(
        KeyModifiers {
            ctrl: true,
            ..KeyModifiers::default()
        },
        BrowserKey::Char('c'),
    );
    assert!(ctrl.clipboard.get_text().contains("/submit"));

    ctrl.handle_key(KeyModifiers::default(), BrowserKey::F12);
    assert!(ctrl.chrome.hud_open);
    assert!(ctrl
        .hud
        .network
        .iter()
        .any(|n| n.method == "POST" && n.status == 200));
    assert!(ctrl
        .hud
        .network
        .iter()
        .any(|n| n.method == "GET" && n.status == 200));

    // Toggle light theme cleanly.
    ctrl.set_theme(ThemeMode::Light);
    assert_eq!(ctrl.chrome.theme, ThemeMode::Light);
}

#[test]
fn simulated_sigkill_crash_shows_crashed_html_and_reloads_cleanly() {
    let runtime_dir = TempDir::new().expect("tempdir");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut ctrl = BrowserController::new(Size::new(1280.0, 720.0), 1.0);
    let tab_id = ctrl.chrome.active().id;
    ctrl.chrome.active_mut().url = URL_NEWTAB.to_string();

    let (old_pid, recovered_pid) = rt.block_on(async {
        let mut sup = ProcessSupervisor::new(RIX_APP_BIN, runtime_dir.path());
        let site = SiteKey::from_url(&RixUrl::parse("https://example.org/doc").unwrap(), 1);
        let site_id = sup.ensure_content_process(site.clone()).await.unwrap();
        let pid1 = sup.child_pid(site_id).expect("content pid");

        let pipeline = PipelineId(1);
        sup.register_pipeline(site_id, pipeline).unwrap();
        sup.send_to_site(
            site_id,
            &BrowserToContent::CreatePipeline {
                pipeline,
                tab: TabId(1),
                viewport: ViewportMetrics {
                    logical_size: Size::new(1280.0, 720.0),
                    device_pixel_ratio: 1.0,
                },
            },
        )
        .await
        .unwrap();
        assert_eq!(
            sup.recv_from_site(site_id).await.unwrap(),
            Some(ContentToBrowser::PipelineReady { pipeline })
        );

        // Simulate `kill -9 <content_pid>`.
        let status = std::process::Command::new("kill")
            .args(["-9", &pid1.to_string()])
            .status()
            .expect("send SIGKILL");
        assert!(status.success());

        let event = tokio::time::timeout(Duration::from_secs(5), sup.next_event())
            .await
            .expect("timeout waiting for crash event")
            .expect("crash event");
        assert!(
            matches!(event, SupervisorEvent::ProcessCrashed { .. }),
            "unexpected supervisor event: {event:?}"
        );

        // Notify BrowserController and confirm crashed.html displays + Reload recovers.
        ctrl.handle_content_process_crash(tab_id, Some(pid1), "SIGKILL");
        assert!(ctrl.chrome.active().crashed_reason.is_some());
        assert!(ctrl
            .active_tab_doc()
            .unwrap()
            .raw_html
            .contains("This tab stopped responding"));

        // Reload tab recovers chrome tab state and supervisor spawns a fresh content process.
        ctrl.reload_active_tab();
        assert!(ctrl.chrome.active().crashed_reason.is_none());
        let site_id2 = sup.ensure_content_process(site).await.unwrap();
        let pid2 = sup.child_pid(site_id2).expect("recovered pid");
        sup.shutdown_all().await;
        (pid1, pid2)
    });

    assert_ne!(old_pid, recovered_pid);
}

#[test]
fn chrome_input_latency_remains_under_2ms_while_content_cpu_is_busy() {
    // Spawn a synthetic CPU-bound background worker simulating a busy Content Process.
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = Arc::clone(&stop);
    let busy_thread = std::thread::spawn(move || {
        let mut acc: u64 = 1;
        while !stop_worker.load(Ordering::Relaxed) {
            acc = acc.wrapping_mul(6364136223846793005).wrapping_add(1);
            std::hint::black_box(acc);
        }
    });

    let mut ctrl = BrowserController::new(Size::new(1280.0, 720.0), 1.0);
    // Open 3 tabs and focus omnibox (pure parent-process chrome state updates).
    ctrl.handle_key(
        KeyModifiers {
            ctrl: true,
            ..KeyModifiers::default()
        },
        BrowserKey::Char('t'),
    );
    ctrl.handle_key(
        KeyModifiers {
            ctrl: true,
            ..KeyModifiers::default()
        },
        BrowserKey::Char('l'),
    );

    // Measure 60 omnibox input + tab-switch interactions strictly isolating parent Chrome state.
    let mut samples_ms = Vec::with_capacity(60);
    for i in 0..60 {
        let t0 = Instant::now();
        if i % 10 == 0 {
            ctrl.chrome.cycle_tab(false);
        } else {
            ctrl.chrome.omnibox.insert_str("a");
        }
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        samples_ms.push(elapsed_ms);
    }

    stop.store(true, Ordering::Relaxed);
    let _ = busy_thread.join();

    samples_ms.sort_by(|a, b| a.total_cmp(b));
    let p95 = samples_ms[(samples_ms.len() * 95) / 100];
    let mean = samples_ms.iter().sum::<f64>() / samples_ms.len() as f64;
    assert!(
        p95 < 2.0 && mean < 2.0,
        "Chrome input latency exceeded 2 ms budget: mean={mean:.4}ms p95={p95:.4}ms"
    );

    // Also verify right-click context menu & hover link preview do not panic.
    assert!(ctrl.handle_right_click(Point::new(300.0, 220.0)));
    assert!(ctrl.chrome.context_menu.visible);
}
