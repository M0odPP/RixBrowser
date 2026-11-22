//! Comprehensive integration tests for `HTMLIFrameElement` (external `src` fetch,
//! `srcdoc`, same-origin vs cross-origin isolation, `sandbox` enforcement),
//! `window.postMessage`, `MessageEvent`, `MessageChannel`, `MessagePort`, and `window.open`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use rix_content::{drain_ready_tasks, ContentRuntime};
use rix_core::Size;
use rix_dom::Document;
use rix_ipc::{ContentToBrowser, PipelineId};
use rix_net::{FetchEngine, HttpClient};
use rix_storage::StorageEngine;
use rix_text::FontDatabase;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn spawn_static_http_server(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_origin_external_src_iframe_fetch_and_cross_origin_sandbox_isolation() {
    let base_url = spawn_static_http_server(
        r#"<!doctype html><html><head><title>ChildPage</title></head><body><div id="msg">from-http</div><script>window.childRan = true; parent.__fromChild = document.getElementById("msg").textContent;</script></body></html>"#,
    )
    .await;

    let doc = Rc::new(RefCell::new(Document::new()));
    let fetch_engine = Arc::new(FetchEngine::new(
        HttpClient::new(None).expect("http client"),
    ));
    let mut rt = ContentRuntime::with_full_config(
        doc,
        Size::new(800.0, 600.0),
        Arc::new(FontDatabase::new()),
        &format!("{base_url}/index.html"),
        StorageEngine::in_memory().expect("storage"),
        Some(fetch_engine),
    );
    rt.set_pipeline_id(PipelineId(1));

    let html = format!(
        r#"<!doctype html><html><body>
            <iframe id="same" src="{base_url}/child.html" width="320" height="180"></iframe>
            <iframe id="xorigin" src="https://remote.example.com/embed" width="200" height="120"></iframe>
            <iframe id="sb_blocked" sandbox="" srcdoc="<script>parent.__sbBlockedRan = true;</script><p id='p'>locked</p>"></iframe>
            <iframe id="sb_dom_only" sandbox="allow-same-origin" srcdoc="<script>parent.__sbDomOnlyScript = true;</script><p id='ok'>dom-visible</p>"></iframe>
            <iframe id="sb_both" sandbox="allow-same-origin allow-scripts" srcdoc="<script>parent.__sbBothRan = true;</script>"></iframe>
        </body></html>"#
    );
    rt.load_html_with_scripts(&html).expect("load html");
    drain_ready_tasks(&rt.event_loop, &mut rt.vm);

    // Cross-origin & opaque-sandboxed iframes must have emitted CreateChildFrame IPC events!
    let outbound = rt.drain_outbound_ipc();
    let child_frames: Vec<_> = outbound
        .iter()
        .filter(|m| matches!(m, ContentToBrowser::CreateChildFrame { .. }))
        .collect();
    assert_eq!(
        child_frames.len(),
        2,
        "expected 2 OOPIF CreateChildFrame messages (xorigin + opaque sandbox), got {child_frames:?}"
    );

    let res = rt
        .eval_script(
            r#"
            const same = document.getElementById("same");
            const xo = document.getElementById("xorigin");
            const sbBlocked = document.getElementById("sb_blocked");
            const sbDomOnly = document.getElementById("sb_dom_only");

            let xoSecErr = "";
            try { void xo.contentWindow.document; } catch (e) { xoSecErr = e.name; }
            let xoLocErr = "";
            try { void xo.contentWindow.location.href; } catch (e) { xoLocErr = e.name; }
            let sbSecErr = "";
            try { void sbBlocked.contentWindow.document; } catch (e) { sbSecErr = e.name; }

            JSON.stringify({
                sameTitle: same.contentDocument.title,
                sameMsg: same.contentDocument.getElementById("msg").textContent,
                sameChildRan: window.__fromChild,
                sameFrameEl: same.contentWindow.frameElement === same,
                xoDocNull: xo.contentDocument === null,
                xoClosed: xo.contentWindow.closed === false,
                xoSecErr,
                xoLocErr,
                sbBlockedDocNull: sbBlocked.contentDocument === null,
                sbBlockedRan: !!window.__sbBlockedRan,
                sbSecErr,
                sbDomOnlyText: sbDomOnly.contentDocument.getElementById("ok").textContent,
                sbDomOnlyScript: !!window.__sbDomOnlyScript,
                sbBothRan: !!window.__sbBothRan,
                frameCount: window.length
            });
            "#,
        )
        .expect("eval assertions");
    let json = rt.vm.value_to_rust_string(res);
    for expected in [
        r#""sameTitle":"ChildPage""#,
        r#""sameMsg":"from-http""#,
        r#""sameChildRan":"from-http""#,
        r#""sameFrameEl":true"#,
        r#""xoDocNull":true"#,
        r#""xoClosed":true"#,
        r#""xoSecErr":"SecurityError""#,
        r#""xoLocErr":"SecurityError""#,
        r#""sbBlockedDocNull":true"#,
        r#""sbBlockedRan":false"#,
        r#""sbSecErr":"SecurityError""#,
        r#""sbDomOnlyText":"dom-visible""#,
        r#""sbDomOnlyScript":false"#,
        r#""sbBothRan":true"#,
        r#""frameCount":5"#,
    ] {
        assert!(json.contains(expected), "missing {expected} in {json}");
    }
}

#[test]
fn message_channel_port_transfer_post_message_and_window_open_semantics() {
    let doc = Rc::new(RefCell::new(Document::new()));
    let mut rt = ContentRuntime::with_origin_and_storage(
        doc,
        Size::new(800.0, 600.0),
        Arc::new(FontDatabase::new()),
        "https://app.example.org/page",
        StorageEngine::in_memory().expect("storage"),
    );

    rt.eval_script(
        r#"
        var log = [];
        // 1. MessageChannel queuing before start(), auto-start via onmessage, port transfer, ArrayBuffer transfer
        var ch1 = new MessageChannel();
        ch1.port2.addEventListener("message", function(e) {
            log.push("p2:" + e.data);
        });
        ch1.port1.postMessage("queued-first");
        ch1.port1.postMessage("queued-second");

        // 2. window.postMessage async ordering + origin check + options dictionary + transfer
        window.addEventListener("message", function(e) {
            if (e.data && e.data.tag === "xfer") {
                var u8 = new Uint8Array(e.data.buf);
                log.push("win:" + e.origin + ":" + u8.length + ":" + u8[0] + ":" + e.ports.length);
                e.ports[0].onmessage = function(pe) { log.push("xfer-port:" + pe.data); };
            } else {
                log.push("win-msg:" + e.data + ":" + (e.source === window));
            }
        });

        var ch2 = new MessageChannel();
        var ab = new ArrayBuffer(4);
        new Uint8Array(ab)[0] = 77;
        window.postMessage({ tag: "xfer", buf: ab }, { targetOrigin: "/", transfer: [ab, ch2.port2] });
        var detachedLen = ab.byteLength;
        ch2.port1.postMessage("over-transferred");

        // Mismatched targetOrigin must NOT deliver
        window.postMessage("dropped", "https://other.example.com");
        // Wildcard delivers asynchronously
        window.postMessage("sync-check", "*");
        log.push("after-post");

        // Invalid targetOrigin must throw SyntaxError
        var badOriginErr = "";
        try { window.postMessage("x", "not a valid url"); } catch (e) { badOriginErr = e.name; }

        // Duplicate transfer / non-cloneable must throw DataCloneError
        var dupErr = "";
        try {
            var chDup = new MessageChannel();
            window.postMessage("x", "*", [chDup.port1, chDup.port1]);
        } catch (e) { dupErr = e.name; }

        // 3. window.open() semantics (same-origin, named reuse, noopener, noreferrer, cross-origin, close)
        var w1 = window.open("https://app.example.org/sub", "panel");
        var w1Again = window.open("", "panel");
        var wNoOpener = window.open("https://app.example.org/isolated", "_blank", "noopener");
        var wNoRef = window.open("https://app.example.org/isolated2", "_blank", "noreferrer");
        var wCross = window.open("https://cross.example.net/home", "_blank");
        var crossErr = "";
        try { void wCross.document; } catch (e) { crossErr = e.name; }
        var sameOpener = w1.opener === window && w1 === w1Again && w1.name === "panel";
        w1.close();
        var closedAfter = w1.closed === true;
        "#,
    )
    .expect("setup script");

    // Drain tasks BEFORE ch1.port2.start(): queued-first/second must NOT have delivered yet!
    rt.advance_and_drain(Duration::from_millis(1));
    let before_start = rt
        .eval_script("log.join('|')")
        .expect("read log before start");
    let before_s = rt.vm.value_to_rust_string(before_start);
    assert!(
        !before_s.contains("p2:queued-first"),
        "MessagePort addEventListener must queue until start(): {before_s}"
    );
    assert!(
        before_s.starts_with("after-post|"),
        "window.postMessage must be strictly async: {before_s}"
    );
    assert!(
        !before_s.contains("dropped"),
        "mismatched targetOrigin must not deliver: {before_s}"
    );

    // Now call ch1.port2.start() and drain!
    rt.eval_script("ch1.port2.start();").expect("start port2");
    rt.advance_and_drain(Duration::from_millis(1));

    let summary = rt
        .eval_script(
            r#"
            JSON.stringify({
                log: log.join("|"),
                detachedLen,
                badOriginErr,
                dupErr,
                noOpenerNull: wNoOpener === null,
                noRefNull: wNoRef === null,
                crossErr,
                sameOpener,
                closedAfter
            })
            "#,
        )
        .expect("summary");
    let s = rt.vm.value_to_rust_string(summary);
    for expected in [
        "win:https://app.example.org:4:77:1",
        "win-msg:sync-check:true",
        "xfer-port:over-transferred",
        "p2:queued-first|p2:queued-second",
        r#""detachedLen":0"#,
        r#""badOriginErr":"SyntaxError""#,
        r#""dupErr":"DataCloneError""#,
        r#""noOpenerNull":true"#,
        r#""noRefNull":true"#,
        r#""crossErr":"SecurityError""#,
        r#""sameOpener":true"#,
        r#""closedAfter":true"#,
    ] {
        assert!(s.contains(expected), "missing {expected} in {s}");
    }
}
