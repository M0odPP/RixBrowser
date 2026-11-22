//! Integration tests for live WHATWG Fetch (CORS, OPTIONS preflight cache,
//! Expose-Headers, no-cors opaque, AbortController instant wakeup),
//! RFC 6455 WebSocket (text + binary echo + clean close), and WHATWG Streams
//! (`ReadableStream`, `WritableStream`, `TransformStream`, `tee`, `Blob.stream`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rix_content::ContentRuntime;
use rix_net::{compute_ws_accept_key, encode_server_frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn spawn_http_cors_server() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let preflight_count = Arc::new(AtomicUsize::new(0));
    let hits = Arc::clone(&preflight_count);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let hits = Arc::clone(&hits);
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
                let origin = req
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("origin")
                            .then(|| v.trim().to_string())
                    })
                    .unwrap_or_else(|| "*".to_string());

                let resp = match (method, path) {
                    ("OPTIONS", "/api/cors") => {
                        hits.fetch_add(1, Ordering::SeqCst);
                        format!(
                            "HTTP/1.1 204 No Content\r\n\
                             Access-Control-Allow-Origin: {origin}\r\n\
                             Access-Control-Allow-Methods: GET, POST, PUT\r\n\
                             Access-Control-Allow-Headers: X-Custom, Content-Type\r\n\
                             Access-Control-Max-Age: 300\r\n\
                             Content-Length: 0\r\n\
                             Connection: close\r\n\r\n"
                        )
                    }
                    (_, "/api/cors") => {
                        let body = r#"{"ok":true,"source":"cors"}"#;
                        format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Access-Control-Allow-Origin: {origin}\r\n\
                             Access-Control-Expose-Headers: X-Public-Id\r\n\
                             X-Public-Id: pub-42\r\n\
                             X-Hidden-Secret: secret\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    (_, "/api/blocked") => {
                        let body = "forbidden-cors";
                        format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    (_, "/api/slow") => {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                            .to_string()
                    }
                    _ => {
                        let body = "hello-same-origin";
                        format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: text/plain\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });

    (format!("http://127.0.0.1:{port}"), preflight_count)
}

async fn spawn_ws_echo_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ws");
    let port = listener.local_addr().expect("addr").port();

    tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let mut req_buf = vec![0u8; 4096];
        let Ok(n) = sock.read(&mut req_buf).await else {
            return;
        };
        let req = String::from_utf8_lossy(&req_buf[..n]);
        let key = req
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("sec-websocket-key")
                    .then(|| v.trim().to_string())
            })
            .expect("ws key");
        let accept = compute_ws_accept_key(&key);
        let handshake = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\
             Sec-WebSocket-Protocol: chat\r\n\r\n"
        );
        let _ = sock.write_all(handshake.as_bytes()).await;

        let mut buf = Vec::new();
        let mut tmp = [0u8; 2048];
        while let Ok(m) = sock.read(&mut tmp).await {
            if m == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..m]);
            while buf.len() >= 6 {
                let op = buf[0] & 0x0F;
                let len = (buf[1] & 0x7F) as usize;
                let frame_len = 2 + 4 + len;
                if buf.len() < frame_len {
                    break;
                }
                let mask = [buf[2], buf[3], buf[4], buf[5]];
                let mut payload = buf[6..frame_len].to_vec();
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i & 3];
                }
                buf.drain(..frame_len);
                match op {
                    0x1 | 0x2 => {
                        let _ = sock.write_all(&encode_server_frame(op, &payload)).await;
                    }
                    0x8 => {
                        let _ = sock.write_all(&encode_server_frame(0x8, &payload)).await;
                        return;
                    }
                    _ => {}
                }
            }
        }
    });

    format!("ws://127.0.0.1:{port}/echo")
}

#[tokio::test]
async fn fetch_cors_preflight_cache_expose_headers_no_cors_and_abort() {
    let (base, preflight_hits) = spawn_http_cors_server().await;
    let mut rt = ContentRuntime::with_empty_document();
    rt.set_document_url("http://app.local:9000/page");

    let script = format!(
        r#"
        var results = {{}};
        var done = false;
        (async function() {{
            var r1 = await fetch("{base}/api/cors", {{
                method: "PUT",
                headers: {{ "X-Custom": "1", "Content-Type": "application/json" }},
                body: JSON.stringify({{ hello: 1 }})
            }});
            results.status1 = r1.status;
            results.type1 = r1.type;
            results.exposed = r1.headers.get("X-Public-Id");
            results.hidden = r1.headers.get("X-Hidden-Secret");
            var j1 = await r1.json();
            results.jsonOk = j1.ok === true && j1.source === "cors";

            var r2 = await fetch("{base}/api/cors", {{
                method: "PUT",
                headers: {{ "X-Custom": "1", "Content-Type": "application/json" }},
                body: "{{}}"
            }});
            var reader = r2.body.getReader();
            var chunk = await reader.read();
            results.streamLen = chunk.value.length;

            try {{
                await fetch("{base}/api/blocked");
                results.blockedRejected = false;
            }} catch (e) {{
                results.blockedRejected = true;
            }}

            var op = await fetch("{base}/api/blocked", {{ mode: "no-cors" }});
            results.opaqueOk = op.type === "opaque" && op.status === 0;

            var ac = new AbortController();
            var abortFired = false;
            ac.signal.addEventListener("abort", function() {{ abortFired = true; }});
            var slowP = fetch("{base}/api/slow", {{ signal: ac.signal }});
            ac.abort();
            try {{
                await slowP;
                results.abortedOk = false;
            }} catch (e) {{
                results.abortedOk = abortFired && ac.signal.aborted && e.name === "AbortError";
            }}
            done = true;
        }})();
        "#
    );

    rt.eval_script(&script).expect("eval fetch script");
    let finished = rt
        .drive_async_until(Duration::from_secs(3), |rt| {
            rt.eval_script("done")
                .is_ok_and(|v| v.as_bool() == Some(true))
        })
        .await;
    assert!(finished, "fetch workflow must finish without polling");

    let summary = rt.eval_script("JSON.stringify(results)").expect("summary");
    let s = rt.vm.value_to_rust_string(summary);
    for expected in [
        r#""status1":200"#,
        r#""type1":"cors""#,
        r#""exposed":"pub-42""#,
        r#""hidden":null"#,
        r#""jsonOk":true"#,
        r#""blockedRejected":true"#,
        r#""opaqueOk":true"#,
        r#""abortedOk":true"#,
    ] {
        assert!(s.contains(expected), "missing {expected} in {s}");
    }
    assert_eq!(preflight_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn websocket_text_binary_echo_and_clean_close() {
    let ws_url = spawn_ws_echo_server().await;
    let mut rt = ContentRuntime::with_empty_document();
    rt.set_document_url("http://app.local:8080/");

    let script = format!(
        r#"
        var wsLog = [];
        var wsDone = false;
        var ws = new WebSocket("{ws_url}", ["chat"]);
        ws.binaryType = "arraybuffer";
        ws.onopen = function() {{
            wsLog.push("open:" + ws.protocol + ":" + ws.readyState);
            ws.send("ping-from-js");
            ws.send(new Uint8Array([9, 8, 7, 6]));
        }};
        ws.addEventListener("message", function(ev) {{
            if (typeof ev.data === "string") {{
                wsLog.push("text:" + ev.data);
            }} else {{
                var bytes = new Uint8Array(ev.data);
                wsLog.push("bin:" + bytes[0] + "," + bytes[3]);
                ws.close(1000, "done");
            }}
        }});
        ws.onclose = function(ev) {{
            wsLog.push("close:" + ev.code + ":" + ev.wasClean + ":" + ws.readyState);
            wsDone = true;
        }};
        "#
    );

    rt.eval_script(&script).expect("eval ws script");
    let finished = rt
        .drive_async_until(Duration::from_secs(3), |rt| {
            rt.eval_script("wsDone")
                .is_ok_and(|v| v.as_bool() == Some(true))
        })
        .await;
    assert!(finished);

    let log_val = rt.eval_script("wsLog.join('|')").expect("wsLog");
    assert_eq!(
        rt.vm.value_to_rust_string(log_val),
        "open:chat:1|text:ping-from-js|bin:9,6|close:1000:true:3"
    );
}

#[test]
fn streams_pipe_through_pipe_to_tee_and_blob_stream() {
    let mut rt = ContentRuntime::with_empty_document();
    rt.eval_script(
        r#"
        var outChunks = [];
        var pipedDone = false;
        var src = new ReadableStream({
            start: function(c) { c.enqueue("a"); c.enqueue("b"); c.close(); }
        });
        var upper = new TransformStream({
            transform: function(chunk, c) { c.enqueue(chunk.toUpperCase()); }
        });
        var dst = new WritableStream({
            write: function(chunk) { outChunks.push(chunk); }
        });
        src.pipeThrough(upper).pipeTo(dst).then(function() { pipedDone = true; });

        var blob = new Blob(["stream-ok"]);
        var bs = blob.stream();
        var pair = bs.tee();
        var b1 = "";
        var b2 = "";
        pair[0].getReader().read().then(function(r) {
            b1 = new TextDecoder().decode(r.value);
        });
        pair[1].getReader().read().then(function(r) {
            b2 = new TextDecoder().decode(r.value);
        });
        "#,
    )
    .expect("eval streams");
    rt.advance_and_drain(Duration::ZERO);
    let res = rt
        .eval_script(r#"outChunks.join("") + "|" + pipedDone + "|" + b1 + "|" + b2"#)
        .expect("check streams");
    assert_eq!(
        rt.vm.value_to_rust_string(res),
        "AB|true|stream-ok|stream-ok"
    );
}
