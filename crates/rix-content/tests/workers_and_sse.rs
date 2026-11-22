//! Integration tests for Dedicated `Worker`, `SharedWorker`, `BroadcastChannel`,
//! `URL.createObjectURL`, and streaming `EventSource` (SSE) in `rix-content`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use rix_content::ContentRuntime;
use rix_core::Size;
use rix_dom::Document;
use rix_storage::StorageEngine;
use rix_text::FontDatabase;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn runtime_with_origin(origin_url: &str) -> ContentRuntime {
    let doc = Rc::new(RefCell::new(Document::new()));
    let font_db = Arc::new(FontDatabase::new());
    let storage = StorageEngine::in_memory().expect("in-memory storage");
    ContentRuntime::with_origin_and_storage(
        doc,
        Size::new(800.0, 600.0),
        font_db,
        origin_url,
        storage,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedicated_worker_blob_structured_clone_import_scripts_and_terminate() {
    let mut rt = runtime_with_origin("https://workers.example/index.html");
    rt.eval_script(
        r#"
        globalThis.mainSecret = "parent-only";
        globalThis.replies = [];
        globalThis.cloneRejected = false;
        const helperUrl = "data:application/javascript,self.helperFactor%20%3D%203%3B";
        const workerCode = `
            importScripts(${JSON.stringify(helperUrl)});
            const isolated = (typeof mainSecret === "undefined");
            self.onmessage = function(ev) {
                const payload = ev.data;
                if (payload.cmd === "compute") {
                    const out = new Uint8Array(payload.bytes.length);
                    for (let i = 0; i < payload.bytes.length; i++) {
                        out[i] = payload.bytes[i] * self.helperFactor;
                    }
                    self.postMessage({
                        isolated,
                        workerName: self.name,
                        sum: payload.nums.reduce((a, b) => a + b, 0),
                        scaled: out,
                    });
                } else if (payload.cmd === "delayed") {
                    setTimeout(() => self.postMessage({ delayed: true }), 15);
                }
            };
        `;
        const blob = new Blob([workerCode], { type: "application/javascript" });
        const url = URL.createObjectURL(blob);
        globalThis.w = new Worker(url, { name: "math-worker" });
        URL.revokeObjectURL(url);
        w.onmessage = (ev) => {
            globalThis.replies.push(ev.data);
        };
        try {
            w.postMessage(() => 1);
        } catch (e) {
            globalThis.cloneRejected = (e && e.name === "DataCloneError");
        }
        w.postMessage({
            cmd: "compute",
            nums: [10, 20, 12],
            bytes: new Uint8Array([2, 4, 7]),
        });
        w.postMessage({ cmd: "delayed" });
        "#,
    )
    .expect("eval worker script");

    let ok = rt
        .drive_async_until(Duration::from_secs(5), |rt| {
            rt.eval_script("globalThis.replies.length >= 2")
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .await;
    assert!(ok, "timed out waiting for dedicated worker replies");

    let summary = rt
        .eval_script(
            r#"
            const r0 = globalThis.replies[0];
            const r1 = globalThis.replies[1];
            w.terminate();
            JSON.stringify({
                cloneRejected: globalThis.cloneRejected,
                isolated: r0.isolated,
                workerName: r0.workerName,
                sum: r0.sum,
                scaled: [r0.scaled[0], r0.scaled[1], r0.scaled[2]],
                delayed: r1.delayed,
            })
            "#,
        )
        .expect("summary");
    let json_str = rt.vm.value_to_rust_string(summary);
    let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("valid json");
    assert_eq!(parsed["cloneRejected"], true);
    assert_eq!(parsed["isolated"], true);
    assert_eq!(parsed["workerName"], "math-worker");
    assert_eq!(parsed["sum"], 42);
    assert_eq!(parsed["scaled"], serde_json::json!([6, 12, 21]));
    assert_eq!(parsed["delayed"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedicated_worker_http_fetch_and_error_event() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = [0u8; 2048];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let body = if req.contains("GET /ok.js") {
                "self.onmessage = (e) => self.postMessage('echo:' + e.data);"
            } else {
                "throw new Error('boom-from-worker');"
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let origin = format!("http://{addr}/index.html");
    let mut rt = runtime_with_origin(&origin);
    rt.eval_script(
        r#"
        globalThis.gotEcho = "";
        globalThis.gotErr = "";
        const w1 = new Worker("/ok.js");
        w1.onmessage = (e) => { globalThis.gotEcho = e.data; };
        w1.postMessage("hello-http");

        const w2 = new Worker("/bad.js");
        w2.onerror = (e) => { globalThis.gotErr = e.message; };
        "#,
    )
    .expect("spawn http workers");

    let ok = rt
        .drive_async_until(Duration::from_secs(5), |rt| {
            rt.eval_script("globalThis.gotEcho.length > 0 && globalThis.gotErr.length > 0")
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .await;
    assert!(ok, "timed out waiting for HTTP worker reply & error");

    let echo = rt.eval_script("globalThis.gotEcho").expect("echo");
    let err = rt.eval_script("globalThis.gotErr").expect("err");
    assert_eq!(rt.vm.value_to_rust_string(echo), "echo:hello-http");
    assert!(
        rt.vm.value_to_rust_string(err).contains("boom-from-worker"),
        "expected worker error message"
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_worker_connects_multiple_runtimes_over_shared_state() {
    let origin = "https://shared-worker-test.example/app.html";
    let script_url = "data:application/javascript,let%20count%3D0%3Bconst%20ports%3D%5B%5D%3Bself.onconnect%3D(e)%3D%3E%7Bconst%20p%3De.ports%5B0%5D%3Bports.push(p)%3Bp.onmessage%3D(m)%3D%3E%7Bcount%2B%3D1%3Bfor(const%20peer%20of%20ports)%7Bpeer.postMessage(%7Bcount%2Cfrom%3Am.data%7D)%3B%7D%7D%3B%7D%3B";

    let mut rt_a = runtime_with_origin(origin);
    let mut rt_b = runtime_with_origin(origin);

    let init_a = format!(
        r#"
        globalThis.msgs = [];
        const sw = new SharedWorker({script_url:?}, "counter-hub");
        sw.port.onmessage = (e) => globalThis.msgs.push(e.data);
        sw.port.postMessage("client-A");
        "#
    );
    let init_b = format!(
        r#"
        globalThis.msgs = [];
        const sw = new SharedWorker({script_url:?}, "counter-hub");
        sw.port.onmessage = (e) => globalThis.msgs.push(e.data);
        sw.port.postMessage("client-B");
        "#
    );

    rt_a.eval_script(&init_a).expect("init_a");
    let ok_a1 = rt_a
        .drive_async_until(Duration::from_secs(5), |rt| {
            rt.eval_script("globalThis.msgs.length >= 1")
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .await;
    assert!(ok_a1, "rt_a should receive first shared worker reply");

    rt_b.eval_script(&init_b).expect("init_b");
    let ok_b = rt_b
        .drive_async_until(Duration::from_secs(5), |rt| {
            rt.eval_script("globalThis.msgs.length >= 1")
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .await;
    assert!(ok_b, "rt_b should receive shared worker broadcast");

    let ok_a2 = rt_a
        .drive_async_until(Duration::from_secs(5), |rt| {
            rt.eval_script("globalThis.msgs.length >= 2")
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .await;
    assert!(ok_a2, "rt_a should receive broadcast triggered by rt_b");

    let a_val = rt_a.eval_script("JSON.stringify(globalThis.msgs)").unwrap();
    let a_json = rt_a.vm.value_to_rust_string(a_val);
    let b_val = rt_b.eval_script("JSON.stringify(globalThis.msgs)").unwrap();
    let b_json = rt_b.vm.value_to_rust_string(b_val);
    assert_eq!(
        a_json,
        r#"[{"count":1,"from":"client-A"},{"count":2,"from":"client-B"}]"#
    );
    assert_eq!(b_json, r#"[{"count":2,"from":"client-B"}]"#);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broadcast_channel_delivers_per_origin_without_self_echo() {
    let mut rt_a = runtime_with_origin("https://bc-origin.example/page1");
    let mut rt_b = runtime_with_origin("https://bc-origin.example/page2");
    let mut rt_other = runtime_with_origin("https://other-origin.example/page");

    rt_a.eval_script(
        r#"
        globalThis.got = [];
        globalThis.ch = new BroadcastChannel("rix-sync");
        ch.onmessage = (e) => globalThis.got.push({ data: e.data, origin: e.origin });
        "#,
    )
    .expect("bc a");
    rt_b.eval_script(
        r#"
        globalThis.got = [];
        globalThis.ch = new BroadcastChannel("rix-sync");
        ch.onmessage = (e) => globalThis.got.push({ data: e.data, origin: e.origin });
        "#,
    )
    .expect("bc b");
    rt_other
        .eval_script(
            r#"
        globalThis.got = [];
        globalThis.ch = new BroadcastChannel("rix-sync");
        ch.onmessage = (e) => globalThis.got.push(e.data);
        "#,
        )
        .expect("bc other");

    rt_a.eval_script(r#"ch.postMessage({ step: 1, tag: "alpha" });"#)
        .expect("post from a");

    let ok_b = rt_b
        .drive_async_until(Duration::from_secs(2), |rt| {
            rt.eval_script("globalThis.got.length === 1")
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .await;
    assert!(ok_b, "rt_b should receive broadcast from rt_a");

    // Pump rt_a and rt_other briefly: neither should receive the message!
    let _ = rt_a
        .drive_async_until(Duration::from_millis(40), |_| false)
        .await;
    let _ = rt_other
        .drive_async_until(Duration::from_millis(40), |_| false)
        .await;

    assert_eq!(
        rt_a.eval_script("globalThis.got.length")
            .unwrap()
            .as_number(),
        Some(0.0),
        "BroadcastChannel must never echo to sender"
    );
    assert_eq!(
        rt_other
            .eval_script("globalThis.got.length")
            .unwrap()
            .as_number(),
        Some(0.0),
        "BroadcastChannel must isolate different origins"
    );

    let b_val = rt_b
        .eval_script("JSON.stringify(globalThis.got[0])")
        .unwrap();
    let b_payload = rt_b.vm.value_to_rust_string(b_val);
    assert_eq!(
        b_payload,
        r#"{"data":{"step":1,"tag":"alpha"},"origin":"https://bc-origin.example"}"#
    );

    // Closed channel throws InvalidStateError on postMessage
    let closed_err = rt_a
        .eval_script(
            r#"
            ch.close();
            let errName = "";
            try { ch.postMessage("fail"); } catch (e) { errName = e.name; }
            errName
            "#,
        )
        .expect("close test");
    assert_eq!(
        rt_a.vm.value_to_rust_string(closed_err),
        "InvalidStateError"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_source_streams_live_chunked_sse_events() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind sse");
    let addr = listener.local_addr().expect("addr");

    let server = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let mut req_buf = [0u8; 1024];
        let _ = sock.read(&mut req_buf).await;

        let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nTransfer-Encoding: chunked\r\nCache-Control: no-cache\r\n\r\n";
        let _ = sock.write_all(header.as_bytes()).await;

        let write_chunk = |data: &str| format!("{:x}\r\n{}\r\n", data.len(), data);

        let chunk1 = ": keepalive comment\nid: 101\ndata: first line\ndata: second line\n\n";
        let _ = sock.write_all(write_chunk(chunk1).as_bytes()).await;
        let _ = sock.flush().await;

        tokio::time::sleep(Duration::from_millis(20)).await;

        let chunk2 = "event: metrics\nid: 102\ndata: {\"fps\":120}\n\n";
        let _ = sock.write_all(write_chunk(chunk2).as_bytes()).await;
        let _ = sock.flush().await;

        // Keep connection open briefly so client sees OPEN state and closes cleanly
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = sock.write_all(b"0\r\n\r\n").await;
    });

    let origin = format!("http://{addr}/index.html");
    let mut rt = runtime_with_origin(&origin);
    rt.eval_script(
        r#"
        globalThis.opened = false;
        globalThis.openState = -1;
        globalThis.messages = [];
        globalThis.metrics = [];
        globalThis.es = new EventSource("/events");
        es.onopen = () => {
            globalThis.opened = true;
            globalThis.openState = es.readyState;
        };
        es.onmessage = (e) => {
            globalThis.messages.push({ data: e.data, id: e.lastEventId, origin: e.origin });
        };
        es.addEventListener("metrics", (e) => {
            globalThis.metrics.push({ data: e.data, id: e.lastEventId });
        });
        "#,
    )
    .expect("create EventSource");

    let ok = rt
        .drive_async_until(Duration::from_secs(5), |rt| {
            rt.eval_script(
                "globalThis.opened && globalThis.messages.length === 1 && globalThis.metrics.length === 1",
            )
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        })
        .await;
    assert!(ok, "timed out waiting for live EventSource SSE events");

    let summary = rt
        .eval_script(
            r#"
            es.close();
            JSON.stringify({
                openState: globalThis.openState,
                closedState: es.readyState,
                constOpen: EventSource.OPEN,
                constClosed: EventSource.CLOSED,
                msg: globalThis.messages[0],
                metric: globalThis.metrics[0],
            })
            "#,
        )
        .expect("sse summary");
    let parsed: serde_json::Value =
        serde_json::from_str(&rt.vm.value_to_rust_string(summary)).expect("json");
    assert_eq!(parsed["openState"], 1);
    assert_eq!(parsed["closedState"], 2);
    assert_eq!(parsed["constOpen"], 1);
    assert_eq!(parsed["constClosed"], 2);
    assert_eq!(parsed["msg"]["data"], "first line\nsecond line");
    assert_eq!(parsed["msg"]["id"], "101");
    assert_eq!(parsed["msg"]["origin"], format!("http://{addr}"));
    assert_eq!(parsed["metric"]["data"], r#"{"fps":120}"#);
    assert_eq!(parsed["metric"]["id"], "102");

    server.abort();
}
