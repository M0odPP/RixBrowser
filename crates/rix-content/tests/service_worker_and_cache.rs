//! Integration tests for W3C `CacheStorage` / `Cache` (persisted in `redb`) and
//! `ServiceWorker` lifecycle (`register`, `install`, `activate`, `fetch` interception,
//! `unregister`) in `rix-content`.

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

fn runtime_with_storage(origin_url: &str, storage: StorageEngine) -> ContentRuntime {
    let doc = Rc::new(RefCell::new(Document::new()));
    let font_db = Arc::new(FontDatabase::new());
    ContentRuntime::with_origin_and_storage(
        doc,
        Size::new(800.0, 600.0),
        font_db,
        origin_url,
        storage,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_storage_full_api_and_redb_persistence_across_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache_storage.redb");
    let origin = "https://cache-persist.example/index.html";

    {
        let storage = StorageEngine::open(&db_path).expect("open redb");
        let mut rt = runtime_with_storage(origin, storage);
        rt.eval_script(
            r#"
            globalThis.step1Done = false;
            globalThis.step1Summary = "";
            (async () => {
                const c = await caches.open("static-v1");
                await c.put("/app.js", new Response("console.log('v1');", {
                    status: 200,
                    headers: { "content-type": "application/javascript", "x-ver": "1" },
                }));
                await c.put("/temp.txt", new Response("ephemeral"));
                const delOk = await c.delete("/temp.txt");
                const keys = await c.keys();
                const names = await caches.keys();
                const hasV1 = await caches.has("static-v1");
                const matched = await caches.match("/app.js");
                const text = await matched.text();
                globalThis.step1Summary = JSON.stringify({
                    delOk,
                    keyCount: keys.length,
                    firstKey: keys[0].url,
                    names,
                    hasV1,
                    text,
                    xVer: matched.headers.get("x-ver"),
                });
                globalThis.step1Done = true;
            })();
            "#,
        )
        .expect("eval cache phase 1");

        let ok = rt
            .drive_async_until(Duration::from_secs(3), |rt| {
                rt.eval_script("globalThis.step1Done")
                    .ok()
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            })
            .await;
        assert!(ok, "cache phase 1 timed out");

        let s_val = rt.eval_script("globalThis.step1Summary").unwrap();
        let s_json: serde_json::Value =
            serde_json::from_str(&rt.vm.value_to_rust_string(s_val)).unwrap();
        assert_eq!(s_json["delOk"], true);
        assert_eq!(s_json["keyCount"], 1);
        assert_eq!(s_json["firstKey"], "https://cache-persist.example/app.js");
        assert_eq!(s_json["names"], serde_json::json!(["static-v1"]));
        assert_eq!(s_json["hasV1"], true);
        assert_eq!(s_json["text"], "console.log('v1');");
        assert_eq!(s_json["xVer"], "1");
    }

    // Reopen fresh ContentRuntime from the SAME redb file!
    {
        let storage = StorageEngine::open(&db_path).expect("reopen redb");
        let mut rt = runtime_with_storage(origin, storage);
        rt.eval_script(
            r#"
            globalThis.step2Done = false;
            globalThis.step2Out = "";
            (async () => {
                const resp = await caches.match("/app.js");
                const body = resp ? await resp.text() : "MISSING";
                const hdr = resp ? resp.headers.get("x-ver") : "";
                const deletedBucket = await caches.delete("static-v1");
                const hasAfter = await caches.has("static-v1");
                globalThis.step2Out = JSON.stringify({ body, hdr, deletedBucket, hasAfter });
                globalThis.step2Done = true;
            })();
            "#,
        )
        .expect("eval cache phase 2");

        let ok = rt
            .drive_async_until(Duration::from_secs(3), |rt| {
                rt.eval_script("globalThis.step2Done")
                    .ok()
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            })
            .await;
        assert!(ok, "cache phase 2 timed out");

        let out_val = rt.eval_script("globalThis.step2Out").unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&rt.vm.value_to_rust_string(out_val)).unwrap();
        assert_eq!(parsed["body"], "console.log('v1');");
        assert_eq!(parsed["hdr"], "1");
        assert_eq!(parsed["deletedBucket"], true);
        assert_eq!(parsed["hasAfter"], false);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_worker_lifecycle_cache_first_fetch_interception_and_unregister() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    let sw_script = r#"
        self.addEventListener("install", (event) => {
            event.waitUntil(
                caches.open("sw-cache-v1").then((cache) =>
                    cache.put("/offline-doc", new Response("precached-offline-body", {
                        status: 200,
                        headers: { "x-source": "sw-cache" },
                    }))
                )
            );
            self.skipWaiting();
        });

        self.addEventListener("activate", (event) => {
            event.waitUntil(self.clients.claim());
        });

        self.addEventListener("fetch", (event) => {
            const url = event.request.url;
            if (url.endsWith("/offline-doc")) {
                event.respondWith(caches.match(event.request));
                return;
            }
            if (url.endsWith("/intercepted")) {
                event.respondWith(new Response("synthetic-from-sw", {
                    status: 201,
                    headers: { "x-sw-intercepted": "true" },
                }));
                return;
            }
            // `/passthrough` intentionally does NOT call respondWith -> goes to origin server!
        });
    "#;

    let server = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let sw_body = sw_script.to_string();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let (ct, body) = if req.contains("GET /sw.js") {
                    ("application/javascript", sw_body.as_str())
                } else if req.contains("GET /passthrough") {
                    ("text/plain", "from-origin-passthrough")
                } else if req.contains("GET /intercepted") {
                    ("text/plain", "from-origin-after-unregister")
                } else {
                    ("text/plain", "origin-fallback")
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });

    let origin = format!("http://{addr}/index.html");
    let storage = StorageEngine::in_memory().expect("storage");
    let mut rt = runtime_with_storage(&origin, storage);

    rt.eval_script(
        r#"
        globalThis.done = false;
        globalThis.stage = "start";
        globalThis.report = "";
        (async () => {
            let crossOriginRejected = false;
            try {
                await navigator.serviceWorker.register("https://evil.example/sw.js");
            } catch (e) {
                crossOriginRejected = (e && e.name === "SecurityError");
            }
            globalThis.stage = "registering";
            const reg = await navigator.serviceWorker.register("/sw.js");
            globalThis.stage = "ready";
            const readyReg = await navigator.serviceWorker.ready;
            const hasController = (navigator.serviceWorker.controller !== null);
            const activeState = reg.active ? reg.active.state : "none";

            globalThis.stage = "fetch-offline";
            const rOffline = await fetch("/offline-doc");
            const offlineText = await rOffline.text();
            const offlineHdr = rOffline.headers.get("x-source");

            globalThis.stage = "fetch-intercept";
            const rIntercept = await fetch("/intercepted");
            const interceptText = await rIntercept.text();
            const interceptStatus = rIntercept.status;
            const interceptHdr = rIntercept.headers.get("x-sw-intercepted");

            globalThis.stage = "fetch-pass";
            const rPass = await fetch("/passthrough");
            const passText = await rPass.text();

            globalThis.stage = "unreg";
            const unregOk = await reg.unregister();
            const afterUnregController = navigator.serviceWorker.controller;
            globalThis.stage = "fetch-after";
            const rAfter = await fetch("/intercepted");
            const afterText = await rAfter.text();

            globalThis.report = JSON.stringify({
                crossOriginRejected,
                scope: reg.scope,
                readyMatchesScope: readyReg.scope === reg.scope,
                hasController,
                activeState,
                offlineText,
                offlineHdr,
                interceptText,
                interceptStatus,
                interceptHdr,
                passText,
                unregOk,
                afterUnregControllerIsNull: afterUnregController === null,
                afterText,
            });
            globalThis.done = true;
        })().catch((e) => {
            globalThis.err = String(e && e.name ? (e.name + ": " + e.message) : e);
            globalThis.done = true;
        });
        "#,
    )
    .expect("eval sw test");

    let ok = rt
        .drive_async_until(Duration::from_secs(6), |rt| {
            rt.eval_script("globalThis.done")
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .await;
    let err_v = rt.eval_script("globalThis.err || ''").unwrap();
    let err_s = rt.vm.value_to_rust_string(err_v);
    let stage_v = rt.eval_script("globalThis.stage || ''").unwrap();
    let stage_s = rt.vm.value_to_rust_string(stage_v);
    assert!(
        err_s.is_empty(),
        "JS error in SW test (stage={stage_s}): {err_s}"
    );
    assert!(ok, "timed out at stage={stage_s}");

    let rep_val = rt.eval_script("globalThis.report").unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(&rt.vm.value_to_rust_string(rep_val)).unwrap();
    assert_eq!(parsed["crossOriginRejected"], true);
    assert_eq!(parsed["scope"], format!("http://{addr}/"));
    assert_eq!(parsed["readyMatchesScope"], true);
    assert_eq!(parsed["hasController"], true);
    assert_eq!(parsed["activeState"], "activated");
    assert_eq!(parsed["offlineText"], "precached-offline-body");
    assert_eq!(parsed["offlineHdr"], "sw-cache");
    assert_eq!(parsed["interceptText"], "synthetic-from-sw");
    assert_eq!(parsed["interceptStatus"], 201);
    assert_eq!(parsed["interceptHdr"], "true");
    assert_eq!(parsed["passText"], "from-origin-passthrough");
    assert_eq!(parsed["unregOk"], true);
    assert_eq!(parsed["afterUnregControllerIsNull"], true);
    assert_eq!(parsed["afterText"], "from-origin-after-unregister");

    server.abort();
}
