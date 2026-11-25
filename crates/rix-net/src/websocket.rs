//! Pure-Rust RFC 6455 WebSocket client (`ws://` and `wss://`) over `tokio`
//! and `tokio-rustls` with zero `unsafe_code` and zero polling.
//!
//! Reference: Spec 06 §2 & Issue #17.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-host opening-handshake mutexes implementing RFC 6455 §4.1 Step 1
/// (serialize concurrent opening handshakes to the same host).
fn ws_host_handshake_lock(host_key: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let map = LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
    Arc::clone(
        guard
            .entry(host_key.to_ascii_lowercase())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}

use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsConnector;

use crate::error::{NetError, Result};

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_WS_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

/// Decoded RFC 6455 WebSocket message delivered to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsMessage {
    Text(String),
    Binary(Vec<u8>),
    /// Sent bytes were flushed to the underlying TCP/TLS socket (`bufferedAmount` drain).
    Sent {
        bytes: usize,
    },
    Close {
        code: u16,
        reason: String,
        was_clean: bool,
    },
}

/// Outbound command sent to the background WebSocket writer task.
#[derive(Debug, Clone)]
pub enum WsCommand {
    SendText(String),
    SendBinary(Vec<u8>),
    Close { code: Option<u16>, reason: String },
}

/// Connected RFC 6455 WebSocket session communicating purely via reactive async channels.
pub struct WebSocketConnection {
    pub negotiated_protocol: String,
    pub negotiated_extensions: String,
    pub response_set_cookies: Vec<String>,
    pub tx: mpsc::UnboundedSender<WsCommand>,
    pub rx: mpsc::UnboundedReceiver<Result<WsMessage>>,
}

/// WHATWG Fetch §2.9 bad port list (blocked for WebSocket & HTTP connections).
fn is_blocked_ws_port(port: u16) -> bool {
    matches!(
        port,
        0 | 1
            | 7
            | 9
            | 11
            | 13
            | 15
            | 17
            | 19
            | 20
            | 21
            | 22
            | 23
            | 25
            | 37
            | 42
            | 43
            | 53
            | 69
            | 77
            | 79
            | 87
            | 95
            | 101
            | 102
            | 103
            | 104
            | 109
            | 110
            | 111
            | 113
            | 115
            | 117
            | 119
            | 123
            | 135
            | 137
            | 139
            | 143
            | 161
            | 179
            | 389
            | 427
            | 465
            | 512
            | 513
            | 514
            | 515
            | 526
            | 530
            | 531
            | 532
            | 540
            | 548
            | 554
            | 556
            | 563
            | 587
            | 601
            | 636
            | 989
            | 990
            | 993
            | 995
            | 1719
            | 1720
            | 1723
            | 2049
            | 3659
            | 4045
            | 4190
            | 5060
            | 5061
            | 6000
            | 6566
            | 6665..=6669 | 6697 | 10080
    )
}

impl WebSocketConnection {
    /// Connects to a `ws://` or `wss://` endpoint, validates the RFC 6455 handshake,
    /// and spawns a zero-polling `tokio::select!` I/O pump.
    pub async fn connect(url: &str, origin: Option<&str>, protocols: &[String]) -> Result<Self> {
        Self::connect_with_headers(url, origin, protocols, &[]).await
    }

    /// Connects to a `ws://` or `wss://` endpoint with additional handshake headers (e.g. `Cookie`),
    /// serializing concurrent handshakes to the same host per RFC 6455 §4.1.
    pub async fn connect_with_headers(
        url: &str,
        origin: Option<&str>,
        protocols: &[String],
        extra_headers: &[(String, String)],
    ) -> Result<Self> {
        let parsed = parse_ws_url(url)?;
        if is_blocked_ws_port(parsed.port) {
            return Err(NetError::WebSocket(format!(
                "WebSocket port {} is blocked by Fetch specification",
                parsed.port
            )));
        }
        let host_lock = ws_host_handshake_lock(&parsed.host);
        let _handshake_guard = host_lock.lock().await;

        let addr = format!("{}:{}", parsed.host, parsed.port);
        let tcp = TcpStream::connect(&addr).await?;
        let _ = tcp.set_nodelay(true);

        if parsed.secure {
            let mut root_store = rustls::RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|e| NetError::Tls(e.to_string()))?
            .with_root_certificates(root_store)
            .with_no_client_auth();
            let connector = TlsConnector::from(Arc::new(config));
            let server_name = ServerName::try_from(parsed.host.clone())
                .map_err(|e| NetError::Tls(e.to_string()))?;
            let tls_stream = connector
                .connect(server_name, tcp)
                .await
                .map_err(|e| NetError::Tls(e.to_string()))?;
            finish_handshake_and_spawn(tls_stream, &parsed, origin, protocols, extra_headers).await
        } else {
            finish_handshake_and_spawn(tcp, &parsed, origin, protocols, extra_headers).await
        }
    }

    pub fn send_text(&self, text: impl Into<String>) -> Result<()> {
        self.tx
            .send(WsCommand::SendText(text.into()))
            .map_err(|_| NetError::WebSocket("WebSocket connection is closed".into()))
    }

    pub fn send_binary(&self, data: Vec<u8>) -> Result<()> {
        self.tx
            .send(WsCommand::SendBinary(data))
            .map_err(|_| NetError::WebSocket("WebSocket connection is closed".into()))
    }

    pub fn close(&self, code: Option<u16>, reason: impl Into<String>) -> Result<()> {
        self.tx
            .send(WsCommand::Close {
                code,
                reason: reason.into(),
            })
            .map_err(|_| NetError::WebSocket("WebSocket connection is closed".into()))
    }

    pub async fn recv(&mut self) -> Option<Result<WsMessage>> {
        self.rx.recv().await
    }
}

struct ParsedWsUrl {
    secure: bool,
    host: String,
    port: u16,
    path_and_query: String,
    host_header: String,
    basic_auth: Option<String>,
}

fn parse_ws_url(raw: &str) -> Result<ParsedWsUrl> {
    let (secure, rest, default_port) = if let Some(r) = raw.strip_prefix("wss://") {
        (true, r, 443u16)
    } else if let Some(r) = raw.strip_prefix("ws://") {
        (false, r, 80u16)
    } else {
        return Err(NetError::WebSocket(format!(
            "Unsupported WebSocket URL scheme: {raw}"
        )));
    };
    let (raw_authority, path_and_query) = match rest.find(['/', '?']) {
        Some(idx) if rest.as_bytes()[idx] == b'/' => (&rest[..idx], rest[idx..].to_string()),
        Some(idx) => (&rest[..idx], format!("/{}", &rest[idx..])),
        None => (rest, "/".to_string()),
    };
    let (basic_auth, authority) = match raw_authority.rfind('@') {
        Some(at) => {
            let userinfo = &raw_authority[..at];
            let encoded = base64_encode(userinfo.as_bytes());
            (Some(format!("Basic {encoded}")), &raw_authority[at + 1..])
        }
        None => (None, raw_authority),
    };
    let (host, port) = if authority.starts_with('[') {
        if let Some(close_br) = authority.find(']') {
            let h = authority[..=close_br].to_string();
            let after = &authority[close_br + 1..];
            if let Some(p_str) = after.strip_prefix(':') {
                let p = p_str
                    .parse::<u16>()
                    .map_err(|_| NetError::WebSocket(format!("Invalid WebSocket port in {raw}")))?;
                (h, p)
            } else {
                (h, default_port)
            }
        } else {
            return Err(NetError::WebSocket(format!("Invalid IPv6 host in {raw}")));
        }
    } else if let Some(colon) = authority.rfind(':') {
        let p = authority[colon + 1..]
            .parse::<u16>()
            .map_err(|_| NetError::WebSocket(format!("Invalid WebSocket port in {raw}")))?;
        (authority[..colon].to_string(), p)
    } else {
        (authority.to_string(), default_port)
    };
    let host_header = if port == default_port {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    Ok(ParsedWsUrl {
        secure,
        host,
        port,
        path_and_query,
        host_header,
        basic_auth,
    })
}

async fn finish_handshake_and_spawn<S>(
    mut stream: S,
    parsed: &ParsedWsUrl,
    origin: Option<&str>,
    protocols: &[String],
    extra_headers: &[(String, String)],
) -> Result<WebSocketConnection>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let nonce = generate_random_16();
    let sec_key = base64_encode(&nonce);
    let expected_accept = compute_ws_accept_key(&sec_key);

    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n",
        parsed.path_and_query, parsed.host_header, sec_key
    );
    if let Some(orig) = origin {
        req.push_str(&format!("Origin: {orig}\r\n"));
    }
    if let Some(ref auth) = parsed.basic_auth {
        req.push_str(&format!("Authorization: {auth}\r\n"));
    }
    if !protocols.is_empty() {
        req.push_str(&format!(
            "Sec-WebSocket-Protocol: {}\r\n",
            protocols.join(", ")
        ));
    }
    for (k, v) in extra_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");

    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;

    // Read HTTP response headers cleanly until `\r\n\r\n` (capturing any trailing WS frame bytes).
    let mut header_buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(NetError::WebSocket(
                "Connection closed during WebSocket handshake".into(),
            ));
        }
        header_buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_crlfcrlf(&header_buf) {
            break pos;
        }
        if header_buf.len() > 16 * 1024 {
            return Err(NetError::WebSocket(
                "Handshake response headers too large".into(),
            ));
        }
    };

    let header_str = std::str::from_utf8(&header_buf[..header_end])
        .map_err(|_| NetError::WebSocket("Invalid UTF-8 in handshake response".into()))?;
    let mut lines = header_str.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    if !status_line.contains(" 101") {
        return Err(NetError::WebSocket(format!(
            "WebSocket upgrade rejected: {status_line}"
        )));
    }

    let mut accept_header = None;
    let mut upgrade_ok = false;
    let mut connection_ok = false;
    let mut negotiated_protocol: Option<String> = None;
    let mut negotiated_extensions = String::new();
    let mut response_set_cookies: Vec<String> = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k_lower = k.trim().to_ascii_lowercase();
            let v_trim = v.trim();
            match k_lower.as_str() {
                "sec-websocket-accept" => {
                    if accept_header.is_some() {
                        return Err(NetError::WebSocket(
                            "Duplicate Sec-WebSocket-Accept header".into(),
                        ));
                    }
                    accept_header = Some(v_trim.to_string());
                }
                "upgrade" => {
                    if v_trim.eq_ignore_ascii_case("websocket") {
                        upgrade_ok = true;
                    }
                }
                "connection" => {
                    if v_trim
                        .split(',')
                        .any(|tok| tok.trim().eq_ignore_ascii_case("upgrade"))
                    {
                        connection_ok = true;
                    }
                }
                "sec-websocket-protocol" => {
                    if negotiated_protocol.is_some() {
                        return Err(NetError::WebSocket(
                            "Duplicate Sec-WebSocket-Protocol header".into(),
                        ));
                    }
                    negotiated_protocol = Some(v_trim.to_string());
                }
                "sec-websocket-extensions" => {
                    if !negotiated_extensions.is_empty() {
                        negotiated_extensions.push_str(", ");
                    }
                    negotiated_extensions.push_str(v_trim);
                }
                "set-cookie" => {
                    response_set_cookies.push(v_trim.to_string());
                }
                _ => {}
            }
        }
    }

    if !upgrade_ok || !connection_ok {
        return Err(NetError::WebSocket(
            "Missing Upgrade: websocket or Connection: Upgrade header".into(),
        ));
    }
    if accept_header.as_deref() != Some(expected_accept.as_str()) {
        return Err(NetError::WebSocket(
            "Invalid Sec-WebSocket-Accept header in server handshake".into(),
        ));
    }
    // RFC 6455 §4.1 subprotocol validation.
    let final_protocol = match negotiated_protocol {
        Some(p) => {
            if protocols.is_empty() || !protocols.iter().any(|req_p| req_p == &p) {
                return Err(NetError::WebSocket(format!(
                    "Server selected unrequested WebSocket subprotocol: '{p}'"
                )));
            }
            p
        }
        None => {
            if !protocols.is_empty() {
                return Err(NetError::WebSocket(
                    "Server did not select any requested WebSocket subprotocol".into(),
                ));
            }
            String::new()
        }
    };
    if !negotiated_extensions.is_empty() {
        // We did not offer any extensions in the client handshake; per RFC 6455 §4.1 reject unknown extensions.
        return Err(NetError::WebSocket(format!(
            "Server selected unrequested WebSocket extension: '{negotiated_extensions}'"
        )));
    }

    let leftover = header_buf[header_end + 4..].to_vec();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (msg_tx, msg_rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        run_ws_pump(stream, leftover, cmd_rx, msg_tx).await;
    });

    Ok(WebSocketConnection {
        negotiated_protocol: final_protocol,
        negotiated_extensions,
        response_set_cookies,
        tx: cmd_tx,
        rx: msg_rx,
    })
}

async fn run_ws_pump<S>(
    mut stream: S,
    initial_buf: Vec<u8>,
    mut cmd_rx: mpsc::UnboundedReceiver<WsCommand>,
    msg_tx: mpsc::UnboundedSender<Result<WsMessage>>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut read_buf = initial_buf;
    let mut tmp = [0u8; 4096];
    let mut frag_opcode: Option<u8> = None;
    let mut frag_payload: Vec<u8> = Vec::new();
    let mut sent_close = false;

    loop {
        // First drain any complete frames already buffered in `read_buf`.
        while let Some(frame) = try_decode_frame(&mut read_buf) {
            match frame {
                Ok(f) => {
                    if handle_decoded_frame(
                        f,
                        &mut stream,
                        &mut frag_opcode,
                        &mut frag_payload,
                        sent_close,
                        &msg_tx,
                    )
                    .await
                    {
                        return;
                    }
                }
                Err(e) => {
                    let _ = msg_tx.send(Err(e));
                    return;
                }
            }
        }

        // Reactive `tokio::select!` with ZERO polling!
        tokio::select! {
            cmd = cmd_rx.recv(), if !sent_close => {
                match cmd {
                    Some(WsCommand::SendText(t)) => {
                        let byte_len = t.len();
                        let wire = encode_client_frame(0x1, t.as_bytes());
                        if stream.write_all(&wire).await.is_err() {
                            let _ = msg_tx.send(Ok(WsMessage::Close {
                                code: 1006,
                                reason: String::new(),
                                was_clean: false,
                            }));
                            return;
                        }
                        let _ = msg_tx.send(Ok(WsMessage::Sent { bytes: byte_len }));
                    }
                    Some(WsCommand::SendBinary(b)) => {
                        let byte_len = b.len();
                        let wire = encode_client_frame(0x2, &b);
                        if stream.write_all(&wire).await.is_err() {
                            let _ = msg_tx.send(Ok(WsMessage::Close {
                                code: 1006,
                                reason: String::new(),
                                was_clean: false,
                            }));
                            return;
                        }
                        let _ = msg_tx.send(Ok(WsMessage::Sent { bytes: byte_len }));
                    }
                    Some(WsCommand::Close { code, reason }) => {
                        let mut payload = Vec::new();
                        if let Some(c) = code {
                            payload.extend_from_slice(&c.to_be_bytes());
                            payload.extend_from_slice(reason.as_bytes());
                        }
                        let wire = encode_client_frame(0x8, &payload);
                        let _ = stream.write_all(&wire).await;
                        let _ = stream.flush().await;
                        sent_close = true;
                        // RFC 6455 §7.1.2: stay open reading until server replies with Close frame or EOF!
                    }
                    None => return,
                }
            }
            read_res = stream.read(&mut tmp) => {
                match read_res {
                    Ok(0) => {
                        let _ = msg_tx.send(Ok(WsMessage::Close {
                            code: 1006,
                            reason: String::new(),
                            was_clean: false,
                        }));
                        return;
                    }
                    Ok(n) => {
                        read_buf.extend_from_slice(&tmp[..n]);
                    }
                    Err(e) => {
                        if sent_close {
                            let _ = msg_tx.send(Ok(WsMessage::Close {
                                code: 1006,
                                reason: String::new(),
                                was_clean: false,
                            }));
                        } else {
                            let _ = msg_tx.send(Err(NetError::Io(e)));
                        }
                        return;
                    }
                }
            }
        }
    }
}

fn is_valid_wire_close_code(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1011 | 3000..=4999)
}

/// Returns `true` when the connection should terminate (e.g. Close frame received or protocol error).
async fn handle_decoded_frame<S: AsyncWrite + Unpin>(
    frame: DecodedFrame,
    stream: &mut S,
    frag_opcode: &mut Option<u8>,
    frag_payload: &mut Vec<u8>,
    sent_close: bool,
    msg_tx: &mpsc::UnboundedSender<Result<WsMessage>>,
) -> bool {
    // RFC 6455 §5.1/§5.2: server-to-client frames must not be masked, RSV must be 0,
    // control frames must have FIN=1 and payload length <= 125.
    if frame.masked || frame.rsv != 0 {
        let _ = msg_tx.send(Err(NetError::WebSocket(
            "Protocol error: invalid server frame mask or RSV bits".into(),
        )));
        return true;
    }
    let is_control = (frame.opcode & 0x08) != 0;
    if is_control && (!frame.fin || frame.payload.len() > 125) {
        let _ = msg_tx.send(Err(NetError::WebSocket(
            "Protocol error: fragmented or oversized control frame".into(),
        )));
        return true;
    }

    match frame.opcode {
        // Ping -> reply immediately with masked Pong carrying identical payload.
        0x9 => {
            if !sent_close {
                let pong = encode_client_frame(0xA, &frame.payload);
                let _ = stream.write_all(&pong).await;
            }
            false
        }
        // Pong -> ignore cleanly.
        0xA => false,
        // Close frame -> validate code/UTF-8, echo Close if we haven't already, notify receiver, and stop.
        0x8 => {
            if frame.payload.len() == 1 {
                let _ = msg_tx.send(Err(NetError::WebSocket(
                    "Protocol error: 1-byte close frame".into(),
                )));
                return true;
            }
            let (code, reason) = if frame.payload.len() >= 2 {
                let c = u16::from_be_bytes([frame.payload[0], frame.payload[1]]);
                if !is_valid_wire_close_code(c) {
                    let _ = msg_tx.send(Err(NetError::WebSocket(format!(
                        "Protocol error: invalid close status code {c}"
                    ))));
                    return true;
                }
                let Ok(r) = String::from_utf8(frame.payload[2..].to_vec()) else {
                    let _ = msg_tx.send(Err(NetError::WebSocket(
                        "Protocol error: invalid UTF-8 in close reason".into(),
                    )));
                    return true;
                };
                (c, r)
            } else {
                (1005, String::new())
            };
            if !sent_close {
                let reply_payload = if code == 1005 {
                    Vec::new()
                } else {
                    code.to_be_bytes().to_vec()
                };
                let reply = encode_client_frame(0x8, &reply_payload);
                let _ = stream.write_all(&reply).await;
                let _ = stream.flush().await;
            }
            let _ = msg_tx.send(Ok(WsMessage::Close {
                code,
                reason,
                was_clean: true,
            }));
            true
        }
        // Data frame (`Text = 0x1`, `Binary = 0x2`, or `Continuation = 0x0`).
        op @ 0x0..=0x2 => {
            if op == 0x0 {
                if frag_opcode.is_none() {
                    let _ = msg_tx.send(Err(NetError::WebSocket(
                        "Protocol error: unexpected continuation frame".into(),
                    )));
                    return true;
                }
            } else {
                if frag_opcode.is_some() {
                    let _ = msg_tx.send(Err(NetError::WebSocket(
                        "Protocol error: new data frame before previous fragmented message ended"
                            .into(),
                    )));
                    return true;
                }
                *frag_opcode = Some(op);
                frag_payload.clear();
            }
            if frag_payload.len() + frame.payload.len() > MAX_WS_MESSAGE_BYTES {
                let _ = msg_tx.send(Err(NetError::ResponseTooLarge(MAX_WS_MESSAGE_BYTES)));
                return true;
            }
            frag_payload.extend_from_slice(&frame.payload);
            if frame.fin {
                let final_op = frag_opcode.take().unwrap_or(0x2);
                let data = std::mem::take(frag_payload);
                if !sent_close {
                    if final_op == 0x1 {
                        match String::from_utf8(data) {
                            Ok(s) => {
                                let _ = msg_tx.send(Ok(WsMessage::Text(s)));
                            }
                            Err(_) => {
                                let _ = msg_tx.send(Err(NetError::WebSocket(
                                    "Invalid UTF-8 in WebSocket text frame".into(),
                                )));
                                return true;
                            }
                        }
                    } else {
                        let _ = msg_tx.send(Ok(WsMessage::Binary(data)));
                    }
                }
            }
            false
        }
        _ => {
            let _ = msg_tx.send(Err(NetError::WebSocket(format!(
                "Protocol error: reserved WebSocket opcode 0x{:X}",
                frame.opcode
            ))));
            true
        }
    }
}

struct DecodedFrame {
    fin: bool,
    rsv: u8,
    opcode: u8,
    masked: bool,
    payload: Vec<u8>,
}

/// Decodes one complete RFC 6455 frame from the front of `buf` if available.
fn try_decode_frame(buf: &mut Vec<u8>) -> Option<Result<DecodedFrame>> {
    if buf.len() < 2 {
        return None;
    }
    let b0 = buf[0];
    let b1 = buf[1];
    let fin = (b0 & 0x80) != 0;
    let rsv = b0 & 0x70;
    let opcode = b0 & 0x0F;
    let masked = (b1 & 0x80) != 0;
    let len7 = (b1 & 0x7F) as usize;

    let mut pos = 2usize;
    let payload_len: usize = if len7 < 126 {
        len7
    } else if len7 == 126 {
        if buf.len() < pos + 2 {
            return None;
        }
        let n = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
        pos += 2;
        n
    } else {
        if buf.len() < pos + 8 {
            return None;
        }
        let n = u64::from_be_bytes([
            buf[pos],
            buf[pos + 1],
            buf[pos + 2],
            buf[pos + 3],
            buf[pos + 4],
            buf[pos + 5],
            buf[pos + 6],
            buf[pos + 7],
        ]);
        pos += 8;
        if n > MAX_WS_MESSAGE_BYTES as u64 {
            return Some(Err(NetError::ResponseTooLarge(MAX_WS_MESSAGE_BYTES)));
        }
        n as usize
    };

    let mask_key = if masked {
        if buf.len() < pos + 4 {
            return None;
        }
        let k = [buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]];
        pos += 4;
        Some(k)
    } else {
        None
    };

    if buf.len() < pos + payload_len {
        return None;
    }

    let mut payload = buf[pos..pos + payload_len].to_vec();
    buf.drain(..pos + payload_len);
    if let Some(k) = mask_key {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= k[i & 3];
        }
    }
    Some(Ok(DecodedFrame {
        fin,
        rsv,
        opcode,
        masked,
        payload,
    }))
}

/// Encodes a masked RFC 6455 client-to-server frame.
#[must_use]
pub fn encode_client_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mask = generate_mask_key();
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | (opcode & 0x0F));
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | (len as u8));
    } else if len <= u16::MAX as usize {
        out.push(0x80 | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    for (i, &b) in payload.iter().enumerate() {
        out.push(b ^ mask[i & 3]);
    }
    out
}

/// Encodes an unmasked RFC 6455 server-to-client frame (helper for local test servers).
#[must_use]
pub fn encode_server_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push(0x80 | (opcode & 0x0F));
    let len = payload.len();
    if len < 126 {
        out.push(len as u8);
    } else if len <= u16::MAX as usize {
        out.push(126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}

/// Computes RFC 6455 `Sec-WebSocket-Accept = Base64(SHA-1(key + WS_GUID))`.
#[must_use]
pub fn compute_ws_accept_key(sec_key: &str) -> String {
    let mut input = Vec::with_capacity(sec_key.len() + WS_GUID.len());
    input.extend_from_slice(sec_key.trim().as_bytes());
    input.extend_from_slice(WS_GUID.as_bytes());
    let digest = sha1_digest(&input);
    base64_encode(&digest)
}

fn find_crlfcrlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn next_entropy_u64() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0x517C_C1B7_2722_0A95);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let mut x = COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed) ^ t;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fn generate_random_16() -> [u8; 16] {
    let a = next_entropy_u64().to_be_bytes();
    let b = next_entropy_u64().to_be_bytes();
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&a);
    out[8..].copy_from_slice(&b);
    out
}

fn generate_mask_key() -> [u8; 4] {
    let v = next_entropy_u64();
    [
        (v & 0xFF) as u8,
        ((v >> 8) & 0xFF) as u8,
        ((v >> 16) & 0xFF) as u8,
        ((v >> 24) & 0xFF) as u8,
    ]
}

/// Pure-Rust FIPS 180-4 SHA-1 implementation exclusively for RFC 6455 WebSocket handshake.
fn sha1_digest(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xEFCD_AB89;
    let mut h2: u32 = 0x98BA_DCFE;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xC3D2_E1F0;

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}
