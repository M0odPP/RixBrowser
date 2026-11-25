//! Integration tests for WHATWG Fetch CORS enforcement, OPTIONS preflight
//! caching (`Access-Control-Max-Age`), opaque `no-cors`, `AbortHandle`
//! instant cancellation, and RFC 6455 WebSocket client over local TCP servers.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rix_net::{
    compute_ws_accept_key, encode_server_frame, AbortHandle, FetchCredentials, FetchEngine,
    FetchMode, FetchRedirect, FetchRequest, FetchResponseType, HttpClient, NetError,
    WebSocketConnection, WsMessage,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn spawn_cors_test_server() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let preflight_hits = Arc::new(AtomicUsize::new(0));
    let hits = Arc::clone(&preflight_hits);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let hits = Arc::clone(&hits);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let Ok(n) = sock.read(&mut buf).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("");
                let mut parts = first_line.split_whitespace();
                let method = parts.next().unwrap_or("GET");
                let path = parts.next().unwrap_or("/");
                let origin = req
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("origin")
                            .then(|| v.trim().to_string())
                    })
                    .unwrap_or_default();

                let resp = match (method, path) {
                    ("OPTIONS", "/api/preflight") => {
                        hits.fetch_add(1, Ordering::SeqCst);
                        format!(
                            "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: {origin}\r\nAccess-Control-Allow-Methods: PUT, DELETE\r\nAccess-Control-Allow-Headers: x-custom-token, content-type\r\nAccess-Control-Max-Age: 60\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                    }
                    ("PUT", "/api/preflight") => {
                        let body = r#"{"ok":true}"#;
                        format!(
                            "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: {origin}\r\nAccess-Control-Expose-Headers: X-Exposed\r\nX-Exposed: visible\r\nX-Secret: hidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    ("GET", "/api/wildcard") => {
                        let body = "wild";
                        format!(
                            "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    ("GET", "/api/denied") => {
                        let body = "secret";
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    ("GET", "/api/slow") => {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_string()
                    }
                    _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string(),
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });

    (format!("http://127.0.0.1:{}", addr.port()), preflight_hits)
}

#[tokio::test]
async fn cors_preflight_cache_expose_headers_credentials_no_cors_and_abort() {
    let (base, preflight_hits) = spawn_cors_test_server().await;
    let engine = FetchEngine::new(HttpClient::new(None).expect("http client"));
    let other_origin = "http://app.local:9999".to_string();

    // 1. Cross-origin PUT with custom header triggers automatic OPTIONS preflight + filters headers!
    let mut hdrs = HashMap::new();
    hdrs.insert("X-Custom-Token".into(), "abc".into());
    hdrs.insert("Content-Type".into(), "application/json".into());
    let resp1 = engine
        .fetch(FetchRequest {
            url: format!("{base}/api/preflight"),
            method: "PUT".into(),
            headers: hdrs.clone(),
            body: b"{}".to_vec(),
            mode: FetchMode::Cors,
            credentials: FetchCredentials::Omit,
            cache: rix_net::FetchCache::Default,
            redirect: FetchRedirect::Follow,
            origin: other_origin.clone(),
            abort_signal: None,
        })
        .await
        .expect("preflighted PUT succeeds");
    assert_eq!(resp1.status, 200);
    assert_eq!(resp1.response_type, FetchResponseType::Cors);
    assert_eq!(preflight_hits.load(Ordering::SeqCst), 1);
    assert!(
        resp1
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-exposed") && v == "visible"),
        "exposed header must be visible"
    );
    assert!(
        !resp1
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("x-secret")),
        "non-exposed header must be hidden"
    );

    // 2. Second identical PUT hits Access-Control-Max-Age preflight cache -> ZERO extra OPTIONS!
    let _resp2 = engine
        .fetch(FetchRequest {
            url: format!("{base}/api/preflight"),
            method: "PUT".into(),
            headers: hdrs,
            body: b"{}".to_vec(),
            mode: FetchMode::Cors,
            credentials: FetchCredentials::Omit,
            cache: rix_net::FetchCache::Default,
            redirect: FetchRedirect::Follow,
            origin: other_origin.clone(),
            abort_signal: None,
        })
        .await
        .expect("cached preflight PUT succeeds");
    assert_eq!(
        preflight_hits.load(Ordering::SeqCst),
        1,
        "preflight cache must prevent duplicate OPTIONS request"
    );

    // 3. Cross-origin credentials: include MUST reject wildcard Access-Control-Allow-Origin: *!
    let cred_err = engine
        .fetch(FetchRequest {
            url: format!("{base}/api/wildcard"),
            method: "GET".into(),
            headers: HashMap::new(),
            body: Vec::new(),
            mode: FetchMode::Cors,
            credentials: FetchCredentials::Include,
            cache: rix_net::FetchCache::Default,
            redirect: FetchRedirect::Follow,
            origin: other_origin.clone(),
            abort_signal: None,
        })
        .await
        .expect_err("wildcard ACAO with credentials: include must fail");
    assert!(matches!(cred_err, NetError::Cors(_)));

    // 4. Missing ACAO blocked in cors mode, but returns empty Opaque response in no-cors mode!
    let denied = engine
        .fetch(FetchRequest {
            url: format!("{base}/api/denied"),
            method: "GET".into(),
            headers: HashMap::new(),
            body: Vec::new(),
            mode: FetchMode::Cors,
            credentials: FetchCredentials::Omit,
            cache: rix_net::FetchCache::Default,
            redirect: FetchRedirect::Follow,
            origin: other_origin.clone(),
            abort_signal: None,
        })
        .await
        .expect_err("missing ACAO must fail in cors mode");
    assert!(matches!(denied, NetError::Cors(_)));

    let opaque = engine
        .fetch(FetchRequest {
            url: format!("{base}/api/denied"),
            method: "GET".into(),
            headers: HashMap::new(),
            body: Vec::new(),
            mode: FetchMode::NoCors,
            credentials: FetchCredentials::Omit,
            cache: rix_net::FetchCache::Default,
            redirect: FetchRedirect::Follow,
            origin: other_origin.clone(),
            abort_signal: None,
        })
        .await
        .expect("no-cors returns opaque");
    assert_eq!(opaque.response_type, FetchResponseType::Opaque);
    assert_eq!(opaque.status, 0);
    assert!(opaque.body.is_empty());

    // 5. AbortHandle cancels slow request immediately via tokio::select! (zero polling!).
    let abort = AbortHandle::new();
    let abort_trigger = abort.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        abort_trigger.abort();
    });
    let t0 = Instant::now();
    let aborted = engine
        .fetch(FetchRequest {
            url: format!("{base}/api/slow"),
            method: "GET".into(),
            headers: HashMap::new(),
            body: Vec::new(),
            mode: FetchMode::Cors,
            credentials: FetchCredentials::Omit,
            cache: rix_net::FetchCache::Default,
            redirect: FetchRedirect::Follow,
            origin: other_origin,
            abort_signal: Some(abort),
        })
        .await
        .expect_err("slow fetch must abort");
    assert!(matches!(aborted, NetError::Aborted));
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "abort should wake immediately, took {:?}",
        t0.elapsed()
    );
}

#[tokio::test]
async fn rfc6455_websocket_handshake_text_binary_ping_pong_and_close() {
    // Verify canonical RFC 6455 Section 1.3 test vector for Sec-WebSocket-Accept!
    assert_eq!(
        compute_ws_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ws");
    let port = listener.local_addr().expect("addr").port();

    tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let mut req_buf = vec![0u8; 2048];
        let Ok(n) = sock.read(&mut req_buf).await else {
            return;
        };
        let req_str = String::from_utf8_lossy(&req_buf[..n]);
        let key = req_str
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("sec-websocket-key")
                    .then(|| v.trim().to_string())
            })
            .unwrap_or_default();
        let accept = compute_ws_accept_key(&key);
        let hs = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: chat\r\n\r\n"
        );
        let _ = sock.write_all(hs.as_bytes()).await;

        // Send a server Ping first (client must auto-reply Pong!), then echo 2 frames (text + binary).
        let _ = sock.write_all(&encode_server_frame(0x9, b"hb")).await;

        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        let mut echoed = 0;
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
                    0xA => {
                        // Verified Pong from client!
                        assert_eq!(payload, b"hb");
                    }
                    0x1 | 0x2 => {
                        let _ = sock.write_all(&encode_server_frame(op, &payload)).await;
                        echoed += 1;
                    }
                    0x8 => {
                        let _ = sock.write_all(&encode_server_frame(0x8, &payload)).await;
                        return;
                    }
                    _ => {}
                }
            }
            if echoed >= 2 {
                // Keep reading for Close frame.
            }
        }
    });

    let mut ws = WebSocketConnection::connect(
        &format!("ws://127.0.0.1:{port}/socket"),
        Some("http://127.0.0.1"),
        &["chat".into()],
    )
    .await
    .expect("ws connect");
    assert_eq!(ws.negotiated_protocol, "chat");

    ws.send_text("hello rix").expect("send text");
    let mut m1 = ws.recv().await.expect("msg 1").expect("ok");
    if matches!(m1, WsMessage::Sent { .. }) {
        m1 = ws.recv().await.expect("msg 1 text").expect("ok");
    }
    assert_eq!(m1, WsMessage::Text("hello rix".into()));

    ws.send_binary(vec![1, 2, 3, 255]).expect("send bin");
    let mut m2 = ws.recv().await.expect("msg 2").expect("ok");
    if matches!(m2, WsMessage::Sent { .. }) {
        m2 = ws.recv().await.expect("msg 2 bin").expect("ok");
    }
    assert_eq!(m2, WsMessage::Binary(vec![1, 2, 3, 255]));

    ws.close(Some(1000), "bye").expect("close");
    let m3 = ws.recv().await.expect("close msg").expect("ok");
    assert_eq!(
        m3,
        WsMessage::Close {
            code: 1000,
            reason: "bye".into(),
            was_clean: true,
        }
    );
}
