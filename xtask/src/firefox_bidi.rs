//! Pure-Rust W3C WebDriver BiDi client for driving headless Mozilla Firefox
//! (`firefox --headless --remote-debugging-port=0`) without Node.js, Playwright, or geckodriver.
//!
//! Captures screenshots, full DOM/computed-style/layout-box snapshots, JS console errors,
//! interactive smooth-scroll `requestAnimationFrame` FPS/p95/p99 metrics, warm-reload timing,
//! self-consistency SSIM, and full process-tree Peak/Steady RSS & CPU time from `/proc`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use rix_net::websocket::{WebSocketConnection, WsCommand, WsMessage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::diff_analyzer::{FrameTimingStats, PageInspectionSnapshot};

/// Configuration for a single headless Firefox BiDi audit run.
#[derive(Debug, Clone)]
pub struct FirefoxRunConfig {
    pub firefox_bin: PathBuf,
    pub url: String,
    pub out_png: PathBuf,
    pub self_check_png: Option<PathBuf>,
    pub post_action_png: Option<PathBuf>,
    pub width: u32,
    pub height: u32,
    pub dpr: f64,
    pub dark_mode: bool,
    pub run_interactive: bool,
    pub measure_warm: bool,
    pub timeout: Duration,
}

/// Interactive scenario metrics measured inside Firefox via WebDriver BiDi + rAF.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FirefoxInteractiveReport {
    pub scroll_raf: FrameTimingStats,
    pub scroll_steps_applied: usize,
    pub hover_attempted: bool,
    pub click_attempted: bool,
    pub click_dom_changed: bool,
    pub resize_ok: bool,
    pub idle_raf_callbacks: u64,
}

/// Complete telemetry & inspection output from a single Firefox headless audit.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FirefoxSiteOutput {
    pub ok: bool,
    pub error: Option<String>,
    pub final_url: String,
    pub startup_ms: f64,
    pub navigate_ms: f64,
    pub total_ms: f64,
    pub warm_total_ms: Option<f64>,
    pub peak_rss_kib: u64,
    pub steady_rss_kib: u64,
    pub cpu_time_ms: f64,
    pub js_errors: Vec<String>,
    pub snapshot: Option<PageInspectionSnapshot>,
    pub interactive: Option<FirefoxInteractiveReport>,
}

/// Resolves the Firefox binary path (`~/.local/opt/firefox/firefox` or system `firefox`).
#[must_use]
pub fn default_firefox_bin() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        let local = PathBuf::from(home).join(".local/opt/firefox/firefox");
        if local.exists() {
            return local;
        }
    }
    let sys = PathBuf::from("/usr/bin/firefox");
    if sys.exists() {
        return sys;
    }
    PathBuf::from("firefox")
}

/// Samples total `VmRSS` (KiB) and total CPU time (`utime + stime` ms) across `root_pid` and
/// every descendant process (content, GPU, socket, utility processes) from `/proc`.
pub fn sample_process_tree_stats(root_pid: u32) -> (u64, f64) {
    let Ok(entries) = fs::read_dir("/proc") else {
        return (0, 0.0);
    };
    let mut parent_map: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut rss_map: HashMap<u32, u64> = HashMap::new();
    let mut cpu_map: HashMap<u32, f64> = HashMap::new();

    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let status_path = entry.path().join("status");
        let Ok(status) = fs::read_to_string(status_path) else {
            continue;
        };
        let mut ppid = 0u32;
        let mut rss = 0u64;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("PPid:") {
                if let Some(tok) = rest.split_whitespace().next() {
                    ppid = tok.parse().unwrap_or(0);
                }
            } else if let Some(rest) = line.strip_prefix("VmRSS:") {
                if let Some(tok) = rest.split_whitespace().next() {
                    rss = tok.parse().unwrap_or(0);
                }
            }
        }
        let cpu_ms = fs::read_to_string(entry.path().join("stat"))
            .ok()
            .and_then(|s| parse_proc_stat_cpu_ms(&s))
            .unwrap_or(0.0);
        parent_map.entry(ppid).or_default().push(pid);
        rss_map.insert(pid, rss);
        cpu_map.insert(pid, cpu_ms);
    }

    let mut visited = HashSet::new();
    let mut stack = vec![root_pid];
    let mut total_rss_kib = 0u64;
    let mut total_cpu_ms = 0.0f64;
    while let Some(pid) = stack.pop() {
        if !visited.insert(pid) {
            continue;
        }
        total_rss_kib += rss_map.get(&pid).copied().unwrap_or(0);
        total_cpu_ms += cpu_map.get(&pid).copied().unwrap_or(0.0);
        if let Some(children) = parent_map.get(&pid) {
            stack.extend(children.iter().copied());
        }
    }
    (total_rss_kib, total_cpu_ms)
}

fn parse_proc_stat_cpu_ms(stat: &str) -> Option<f64> {
    let rparen = stat.rfind(')')?;
    let fields: Vec<&str> = stat[rparen + 1..].split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) as f64 * 10.0)
}

/// Decodes a standard RFC 4648 base64 string (ignoring ASCII whitespace) into raw bytes.
pub fn decode_base64(input: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u8;
    for b in input.bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            break;
        }
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => bail!("invalid base64 byte: {b}"),
        };
        buf = (buf << 6) | u32::from(val);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Ok(out)
}

/// Minimal clean `user.js` disabling telemetry/onboarding/update noise while keeping standard
/// Gecko/SpiderMonkey/Stylo/WebRender web behavior untouched.
fn write_clean_firefox_profile(profile_dir: &Path, dark_mode: bool) -> Result<()> {
    fs::create_dir_all(profile_dir)?;
    let color_scheme = if dark_mode { 0 } else { 1 };
    let prefs = format!(
        r#"user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("startup.homepage_welcome_url", "");
user_pref("startup.homepage_welcome_url.additional", "");
user_pref("browser.tabs.warnOnClose", false);
user_pref("datareporting.healthreport.uploadEnabled", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.enabled", false);
user_pref("toolkit.telemetry.unified", false);
user_pref("app.update.enabled", false);
user_pref("extensions.update.enabled", false);
user_pref("layout.css.prefers-color-scheme.content-override", {color_scheme});
user_pref("ui.systemUsesDarkTheme", {dark_int});
user_pref("browser.cache.disk.enable", false);
user_pref("browser.cache.memory.enable", true);
"#,
        dark_int = if dark_mode { 1 } else { 0 }
    );
    fs::write(profile_dir.join("user.js"), prefs)?;
    Ok(())
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct BidiClient {
    ws: WebSocketConnection,
    next_id: u64,
    js_errors: Vec<String>,
}

impl BidiClient {
    async fn connect(ws_url: &str) -> Result<Self> {
        let ws = WebSocketConnection::connect(ws_url, None, &[])
            .await
            .map_err(|e| anyhow!("BiDi WebSocket connect failed ({ws_url}): {e}"))?;
        Ok(Self {
            ws,
            next_id: 1,
            js_errors: Vec::new(),
        })
    }

    async fn call(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string();
        self.ws
            .tx
            .send(WsCommand::SendText(payload))
            .map_err(|_| anyhow!("BiDi WebSocket send channel closed"))?;

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                bail!(
                    "BiDi command `{method}` timed out after {}ms",
                    timeout.as_millis()
                );
            }
            let msg = tokio::time::timeout(remaining, self.ws.rx.recv())
                .await
                .map_err(|_| anyhow!("BiDi command `{method}` timed out"))?;
            let Some(item) = msg else {
                bail!("BiDi WebSocket closed while waiting for `{method}`");
            };
            match item {
                Ok(WsMessage::Text(text)) => {
                    let Ok(val) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    if val.get("type").and_then(Value::as_str) == Some("event") {
                        self.record_event(&val);
                        continue;
                    }
                    if val.get("id").and_then(Value::as_u64) == Some(id) {
                        if val.get("type").and_then(Value::as_str) == Some("error") {
                            let err = val
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown");
                            let msg = val.get("message").and_then(Value::as_str).unwrap_or("");
                            bail!("BiDi `{method}` error ({err}): {msg}");
                        }
                        return Ok(val.get("result").cloned().unwrap_or(Value::Null));
                    }
                }
                Ok(WsMessage::Sent { .. }) | Ok(WsMessage::Binary(_)) => continue,
                Ok(WsMessage::Close { code, reason, .. }) => {
                    bail!("BiDi WebSocket closed ({code}: {reason}) during `{method}`");
                }
                Err(e) => bail!("BiDi WebSocket read error during `{method}`: {e}"),
            }
        }
    }

    fn record_event(&mut self, event: &Value) {
        if event.get("method").and_then(Value::as_str) != Some("log.entryAdded") {
            return;
        }
        let Some(params) = event.get("params") else {
            return;
        };
        let level = params.get("level").and_then(Value::as_str).unwrap_or("");
        let kind = params.get("type").and_then(Value::as_str).unwrap_or("");
        if level == "error" || kind == "javascript" {
            let text = params
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("uncaught error")
                .trim();
            if !text.is_empty() && self.js_errors.len() < 64 {
                self.js_errors.push(text.chars().take(240).collect());
            }
        }
    }
}

/// JavaScript inspector executed inside Firefox via `script.evaluate` producing the exact
/// `PageInspectionSnapshot` JSON schema as `rix-app --dump-dom-layout`.
const FIREFOX_INSPECTOR_JS: &str = r#"(() => {
  const PROPS = [
    "display","position","visibility","opacity","z-index","overflow-x","overflow-y",
    "box-sizing","top","right","bottom","left","float","clear",
    "width","height","min-width","max-width","min-height","max-height","aspect-ratio",
    "flex-direction","flex-wrap","flex-grow","flex-shrink","justify-content","align-items","align-self",
    "row-gap","column-gap","grid-template-columns","grid-template-rows",
    "color","background-color","background-image","background-size","background-position","background-repeat",
    "font-family","font-size","font-weight","font-style","line-height","letter-spacing","word-spacing",
    "text-align","text-transform","text-decoration-line","white-space","vertical-align","direction","writing-mode",
    "margin-top","margin-right","margin-bottom","margin-left",
    "padding-top","padding-right","padding-bottom","padding-left",
    "border-top-width","border-right-width","border-bottom-width","border-left-width",
    "border-top-style","border-top-color","border-top-left-radius",
    "transform","filter","box-shadow","pointer-events","cursor"
  ];
  const SKIP = new Set(["script","style","meta","link","head","title","noscript","br"]);
  const round10 = (v) => (Number.isFinite(v) ? Math.round(v * 10) / 10 : 0);
  const cssPath = (el) => {
    const parts = [];
    let cur = el;
    while (cur && cur.nodeType === 1) {
      const tag = cur.localName.toLowerCase();
      if (tag === "html" || tag === "body" || tag === "head") {
        parts.push(tag);
      } else {
        let idx = 1;
        let sib = cur.previousElementSibling;
        while (sib) {
          if (sib.localName.toLowerCase() === tag) idx++;
          sib = sib.previousElementSibling;
        }
        parts.push(tag + ":nth-of-type(" + idx + ")");
      }
      cur = cur.parentElement;
    }
    parts.reverse();
    return parts.join(">");
  };
  const vw = window.innerWidth || document.documentElement.clientWidth || 1280;
  const vh = window.innerHeight || document.documentElement.clientHeight || 800;
  const imgs = Array.from(document.querySelectorAll("img"));
  let imgTotal = 0, imgLoaded = 0, imgBroken = 0, imgVisBroken = 0;
  for (const img of imgs) {
    const src = (img.getAttribute("src") || img.getAttribute("srcset") || "").trim();
    if (!src) continue;
    imgTotal++;
    if (img.complete && img.naturalWidth > 0) {
      imgLoaded++;
    } else {
      imgBroken++;
      const r = img.getBoundingClientRect();
      if (r.width >= 1 && r.height >= 1 && r.right > 0 && r.bottom > 0 && r.left < vw && r.top < vh) {
        imgVisBroken++;
      }
    }
  }
  const elements = [];
  const all = document.querySelectorAll("*");
  const nearFold = vh * 1.5;
  for (let i = 0; i < all.length && elements.length < 400; i++) {
    const el = all[i];
    const tag = el.localName.toLowerCase();
    if (SKIP.has(tag)) continue;
    const r = el.getBoundingClientRect();
    if (r.width < 1 && r.height < 1) continue;
    if (r.top > nearFold || r.bottom < -50) continue;
    const cs = window.getComputedStyle(el);
    if (!cs || cs.display === "none") continue;
    const styles = {};
    for (const p of PROPS) {
      styles[p] = cs.getPropertyValue(p).trim();
    }
    const text = (el.textContent || "").replace(/\s+/g, " ").trim().slice(0, 60);
    const cls = typeof el.className === "string"
      ? el.className.trim().split(/\s+/).filter(Boolean).slice(0, 6)
      : [];
    elements.push({
      path: cssPath(el),
      tag,
      id: el.id || "",
      classes: cls,
      text_snippet: text,
      rect: {
        x: round10(r.left + window.scrollX),
        y: round10(r.top + window.scrollY),
        width: round10(r.width),
        height: round10(r.height),
      },
      styles,
    });
  }
  return JSON.stringify({
    url: location.href,
    title: document.title || "",
    viewport_width: round10(vw),
    viewport_height: round10(vh),
    scroll_width: round10(Math.max(document.documentElement.scrollWidth || 0, vw)),
    scroll_height: round10(Math.max(document.documentElement.scrollHeight || 0, vh)),
    node_count: all.length,
    images: {
      total: imgTotal,
      loaded: imgLoaded,
      broken: imgBroken,
      visible_broken: imgVisBroken,
    },
    elements,
  });
})()"#;

/// Async JS smooth-scroll frame-timing benchmark executed in Firefox over 16 frames.
const FIREFOX_SCROLL_BENCH_JS: &str = r#"new Promise((resolve) => {
  const samples = [];
  let steps = 0;
  let last = performance.now();
  let count = 0;
  function tick(now) {
    if (count > 0) {
      samples.push(Math.max(0.1, now - last));
    }
    last = now;
    if (count < 16) {
      const beforeY = window.scrollY;
      window.scrollBy(0, 140);
      if (window.scrollY !== beforeY) steps++;
      count++;
      requestAnimationFrame(tick);
    } else {
      window.scrollTo(0, 0);
      resolve(JSON.stringify({ samples, steps }));
    }
  }
  requestAnimationFrame(tick);
})"#;

/// Finds first visible interactive element center in Firefox matching `rix-app/src/inspector.rs`.
const FIREFOX_FIND_INTERACTIVE_JS: &str = r#"(() => {
  const vw = window.innerWidth || 1280;
  const vh = window.innerHeight || 800;
  for (const tag of ["button", "summary", "input", "a"]) {
    const els = document.querySelectorAll(tag);
    for (const el of els) {
      const r = el.getBoundingClientRect();
      if (r.width >= 12 && r.height >= 12 && r.left >= 0 && r.top >= 0 &&
          r.left + r.width * 0.5 < vw && r.top + r.height * 0.5 < vh) {
        return JSON.stringify({
          x: Math.round(r.left + r.width * 0.5),
          y: Math.round(r.top + r.height * 0.5),
          nodes: document.querySelectorAll("*").length,
          url: location.href,
        });
      }
    }
  }
  return "";
})()"#;

fn extract_eval_string(result: &Value) -> Option<String> {
    result
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(Value::as_str)
        .map(String::from)
}

/// Runs a complete headless Mozilla Firefox audit for `cfg.url` via W3C WebDriver BiDi.
pub fn run_firefox_site(cfg: &FirefoxRunConfig) -> FirefoxSiteOutput {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            return FirefoxSiteOutput {
                ok: false,
                error: Some(format!("failed to create tokio runtime: {e}")),
                ..FirefoxSiteOutput::default()
            }
        }
    };
    rt.block_on(run_firefox_site_async(cfg))
}

async fn run_firefox_site_async(cfg: &FirefoxRunConfig) -> FirefoxSiteOutput {
    let mut out = FirefoxSiteOutput::default();
    let profile_dir = std::env::temp_dir().join(format!(
        "rix_ff_bidi_{}_{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let _ = fs::remove_dir_all(&profile_dir);
    if let Err(e) = write_clean_firefox_profile(&profile_dir, cfg.dark_mode) {
        out.error = Some(format!("profile setup failed: {e}"));
        return out;
    }

    let t_total = Instant::now();
    let t_startup = Instant::now();
    let mut cmd = Command::new(&cfg.firefox_bin);
    cmd.arg("--headless")
        .arg("--no-remote")
        .arg("--profile")
        .arg(&profile_dir)
        .arg("--remote-debugging-port=0")
        .env("MOZ_HEADLESS_WIDTH", cfg.width.to_string())
        .env("MOZ_HEADLESS_HEIGHT", cfg.height.to_string())
        .env("MOZ_REMOTE_ALLOW_SYSTEM_ACCESS", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = fs::remove_dir_all(&profile_dir);
            out.error = Some(format!(
                "failed to launch Firefox at {}: {e}",
                cfg.firefox_bin.display()
            ));
            return out;
        }
    };
    let root_pid = child.id();
    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(&profile_dir);
        out.error = Some("failed to capture Firefox stderr".into());
        return out;
    };
    let _child_guard = ChildGuard(child);

    // Wait for `WebDriver BiDi listening on ws://127.0.0.1:<port>` on a dedicated reader thread.
    let (ws_tx, ws_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(pos) = line.find("WebDriver BiDi listening on ") {
                let url = line[pos + "WebDriver BiDi listening on ".len()..]
                    .trim()
                    .to_string();
                let _ = ws_tx.send(url);
            }
        }
    });

    let ws_base = match ws_rx.recv_timeout(Duration::from_secs(12)) {
        Ok(u) => u,
        Err(_) => {
            let _ = fs::remove_dir_all(&profile_dir);
            out.error = Some("timed out waiting for Firefox WebDriver BiDi endpoint".into());
            return out;
        }
    };
    let ws_url = format!("{}/session", ws_base.trim_end_matches('/'));

    let mut bidi = match BidiClient::connect(&ws_url).await {
        Ok(c) => c,
        Err(e) => {
            let _ = fs::remove_dir_all(&profile_dir);
            out.error = Some(e.to_string());
            return out;
        }
    };

    let run_res: Result<()> = async {
        let _ = bidi
            .call(
                "session.new",
                json!({ "capabilities": { "alwaysMatch": { "acceptInsecureCerts": true } } }),
                Duration::from_secs(10),
            )
            .await?;
        let _ = bidi
            .call(
                "session.subscribe",
                json!({ "events": ["log.entryAdded"] }),
                Duration::from_secs(5),
            )
            .await?;
        let created = bidi
            .call(
                "browsingContext.create",
                json!({ "type": "tab" }),
                Duration::from_secs(5),
            )
            .await;
        let context = match created
            .ok()
            .and_then(|v| v.get("context").and_then(Value::as_str).map(String::from))
        {
            Some(ctx) => ctx,
            None => {
                let tree = bidi
                    .call("browsingContext.getTree", json!({}), Duration::from_secs(5))
                    .await?;
                tree.get("contexts")
                    .and_then(Value::as_array)
                    .and_then(|arr| arr.first())
                    .and_then(|c| c.get("context"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("Firefox BiDi returned no browsing context"))?
                    .to_string()
            }
        };

        let _ = bidi
            .call(
                "browsingContext.setViewport",
                json!({
                    "context": context,
                    "viewport": { "width": cfg.width, "height": cfg.height },
                    "devicePixelRatio": cfg.dpr,
                }),
                Duration::from_secs(5),
            )
            .await?;

        out.startup_ms = t_startup.elapsed().as_secs_f64() * 1000.0;

        // Track Peak RSS while navigating.
        let mut peak_rss = 0u64;
        let t_nav = Instant::now();
        let nav_res = bidi
            .call(
                "browsingContext.navigate",
                json!({
                    "context": context,
                    "url": cfg.url,
                    "wait": "complete",
                }),
                cfg.timeout,
            )
            .await?;
        out.navigate_ms = t_nav.elapsed().as_secs_f64() * 1000.0;
        if let Some(u) = nav_res.get("url").and_then(Value::as_str) {
            out.final_url = u.to_string();
        }
        let (rss_after_nav, _) = sample_process_tree_stats(root_pid);
        peak_rss = peak_rss.max(rss_after_nav);

        // Brief settle matching RixBrowser so async hydration/fonts commit cleanly.
        tokio::time::sleep(Duration::from_millis(350)).await;
        let (rss_after_settle, cpu_after_settle) = sample_process_tree_stats(root_pid);
        peak_rss = peak_rss.max(rss_after_settle);

        // Primary screenshot.
        let shot = bidi
            .call(
                "browsingContext.captureScreenshot",
                json!({ "context": context, "origin": "viewport" }),
                Duration::from_secs(10),
            )
            .await?;
        let png_b64 = shot
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Firefox captureScreenshot missing data"))?;
        let png_bytes = decode_base64(png_b64)?;
        if let Some(parent) = cfg.out_png.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&cfg.out_png, &png_bytes)?;

        out.total_ms = t_total.elapsed().as_secs_f64() * 1000.0;
        let (steady_rss, cpu_ms) = sample_process_tree_stats(root_pid);
        out.peak_rss_kib = peak_rss.max(steady_rss);
        out.steady_rss_kib = steady_rss;
        out.cpu_time_ms = cpu_ms.max(cpu_after_settle);

        // DOM / ComputedStyle / BoundingClientRect / ImageAudit snapshot.
        let eval_res = bidi
            .call(
                "script.evaluate",
                json!({
                    "expression": FIREFOX_INSPECTOR_JS,
                    "target": { "context": context },
                    "awaitPromise": false,
                    "resultOwnership": "none",
                }),
                Duration::from_secs(10),
            )
            .await?;
        if let Some(json_str) = extract_eval_string(&eval_res) {
            if let Ok(snap) = serde_json::from_str::<PageInspectionSnapshot>(&json_str) {
                if out.final_url.is_empty() {
                    out.final_url = snap.url.clone();
                }
                out.snapshot = Some(snap);
            }
        }

        // Optional interactive smooth-scroll + hover/click + resize benchmark.
        if cfg.run_interactive {
            let mut inter = FirefoxInteractiveReport::default();
            if let Ok(scroll_res) = bidi
                .call(
                    "script.evaluate",
                    json!({
                        "expression": FIREFOX_SCROLL_BENCH_JS,
                        "target": { "context": context },
                        "awaitPromise": true,
                        "resultOwnership": "none",
                    }),
                    Duration::from_secs(10),
                )
                .await
            {
                if let Some(raw) = extract_eval_string(&scroll_res) {
                    if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                        let samples: Vec<f64> = v
                            .get("samples")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(Value::as_f64).collect())
                            .unwrap_or_default();
                        inter.scroll_raf = FrameTimingStats::from_samples_ms(&samples);
                        inter.scroll_steps_applied = v
                            .get("steps")
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as usize;
                    }
                }
            }

            if let Ok(target_res) = bidi
                .call(
                    "script.evaluate",
                    json!({
                        "expression": FIREFOX_FIND_INTERACTIVE_JS,
                        "target": { "context": context },
                        "awaitPromise": false,
                        "resultOwnership": "none",
                    }),
                    Duration::from_secs(5),
                )
                .await
            {
                if let Some(raw) = extract_eval_string(&target_res) {
                    if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                        let x = v.get("x").and_then(Value::as_i64).unwrap_or(0);
                        let y = v.get("y").and_then(Value::as_i64).unwrap_or(0);
                        let nodes_before = v.get("nodes").and_then(Value::as_u64).unwrap_or(0);
                        let url_before = v
                            .get("url")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        inter.hover_attempted = true;
                        inter.click_attempted = true;
                        let _ = bidi
                            .call(
                                "input.performActions",
                                json!({
                                    "context": context,
                                    "actions": [{
                                        "type": "pointer",
                                        "id": "mouse",
                                        "parameters": { "pointerType": "mouse" },
                                        "actions": [
                                            { "type": "pointerMove", "x": x, "y": y, "duration": 10 },
                                            { "type": "pointerDown", "button": 0 },
                                            { "type": "pointerUp", "button": 0 }
                                        ]
                                    }]
                                }),
                                Duration::from_secs(5),
                            )
                            .await;
                        tokio::time::sleep(Duration::from_millis(120)).await;
                        if let Ok(after_res) = bidi
                            .call(
                                "script.evaluate",
                                json!({
                                    "expression": "JSON.stringify({ nodes: document.querySelectorAll('*').length, url: location.href })",
                                    "target": { "context": context },
                                    "awaitPromise": false,
                                }),
                                Duration::from_secs(5),
                            )
                            .await
                        {
                            if let Some(araw) = extract_eval_string(&after_res) {
                                if let Ok(av) = serde_json::from_str::<Value>(&araw) {
                                    let nodes_after =
                                        av.get("nodes").and_then(Value::as_u64).unwrap_or(0);
                                    let url_after = av
                                        .get("url")
                                        .and_then(Value::as_str)
                                        .unwrap_or("");
                                    inter.click_dom_changed =
                                        nodes_after != nodes_before || url_after != url_before;
                                }
                            }
                        }
                    }
                }
            }

            let narrowed = cfg.width.saturating_sub(120).max(320);
            let r1 = bidi
                .call(
                    "browsingContext.setViewport",
                    json!({
                        "context": context,
                        "viewport": { "width": narrowed, "height": cfg.height },
                        "devicePixelRatio": cfg.dpr,
                    }),
                    Duration::from_secs(5),
                )
                .await
                .is_ok();
            let r2 = bidi
                .call(
                    "browsingContext.setViewport",
                    json!({
                        "context": context,
                        "viewport": { "width": cfg.width, "height": cfg.height },
                        "devicePixelRatio": cfg.dpr,
                    }),
                    Duration::from_secs(5),
                )
                .await
                .is_ok();
            inter.resize_ok = r1 && r2;

            if let Some(ref post_path) = cfg.post_action_png {
                if let Ok(post_shot) = bidi
                    .call(
                        "browsingContext.captureScreenshot",
                        json!({ "context": context, "origin": "viewport" }),
                        Duration::from_secs(8),
                    )
                    .await
                {
                    if let Some(b64) = post_shot.get("data").and_then(Value::as_str) {
                        if let Ok(bytes) = decode_base64(b64) {
                            let _ = fs::write(post_path, bytes);
                        }
                    }
                }
            }

            out.interactive = Some(inter);
        }

        // Optional warm reload + optional Firefox self-consistency screenshot.
        if cfg.measure_warm || cfg.self_check_png.is_some() {
            let t_warm = Instant::now();
            let _ = bidi
                .call(
                    "browsingContext.navigate",
                    json!({
                        "context": context,
                        "url": cfg.url,
                        "wait": "complete",
                    }),
                    cfg.timeout,
                )
                .await?;
            tokio::time::sleep(Duration::from_millis(200)).await;
            let warm_ms = t_warm.elapsed().as_secs_f64() * 1000.0;
            if cfg.measure_warm {
                out.warm_total_ms = Some(warm_ms);
            }
            if let Some(ref sc_path) = cfg.self_check_png {
                if let Ok(sc_shot) = bidi
                    .call(
                        "browsingContext.captureScreenshot",
                        json!({ "context": context, "origin": "viewport" }),
                        Duration::from_secs(8),
                    )
                    .await
                {
                    if let Some(b64) = sc_shot.get("data").and_then(Value::as_str) {
                        if let Ok(bytes) = decode_base64(b64) {
                            let _ = fs::write(sc_path, bytes);
                        }
                    }
                }
            }
        }

        let _ = bidi
            .call("session.end", json!({}), Duration::from_secs(3))
            .await;
        Ok(())
    }
    .await;

    out.js_errors = bidi.js_errors;
    match run_res {
        Ok(()) => out.ok = true,
        Err(e) => {
            out.ok = false;
            out.error = Some(e.to_string());
        }
    }

    drop(_child_guard);
    let _ = fs::remove_dir_all(&profile_dir);
    out
}

/// Single subtest result returned by upstream WPT `testharness.js` inside Firefox.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FirefoxWptSubtest {
    pub name: String,
    pub pass: bool,
    pub message: Option<String>,
}

/// Outcome of executing a WPT testharness test file inside Mozilla Firefox.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FirefoxWptResult {
    pub harness_ok: bool,
    pub subtests: Vec<FirefoxWptSubtest>,
}

const FIREFOX_WPT_COLLECT_JS: &str = r##"new Promise((resolve) => {
  if (typeof window.__rix_ff_wpt_result === "string") {
    resolve(window.__rix_ff_wpt_result);
    return;
  }
  let settled = false;
  const finish = (tests, status) => {
    if (settled) return;
    settled = true;
    resolve(JSON.stringify({
      harness_ok: !status || status.status === 0,
      subtests: (tests || []).map((t) => ({
        name: String(t.name || ""),
        pass: t.status === 0,
        message: t.message ? String(t.message) : null,
      })),
    }));
  };
  const readDomResults = () => {
    const rows = document.querySelectorAll("#results tbody tr");
    if (!rows || rows.length === 0) return null;
    const subtests = [];
    for (const row of rows) {
      const cells = row.querySelectorAll("td");
      if (cells.length >= 2) {
        const st = (cells[0].textContent || "").trim().toUpperCase();
        const name = (cells[1].textContent || "").trim();
        const msg = cells.length >= 3 ? (cells[2].textContent || "").trim() : "";
        subtests.push({ name, pass: st === "PASS", message: msg || null });
      }
    }
    return subtests.length > 0 ? { harness_ok: true, subtests } : null;
  };
  const existing = readDomResults();
  if (existing) {
    settled = true;
    resolve(JSON.stringify(existing));
    return;
  }
  if (typeof add_completion_callback === "function") {
    add_completion_callback(finish);
  }
  const poll = setInterval(() => {
    if (typeof window.__rix_ff_wpt_result === "string") {
      clearInterval(poll);
      if (!settled) { settled = true; resolve(window.__rix_ff_wpt_result); }
      return;
    }
    const dom = readDomResults();
    if (dom) {
      clearInterval(poll);
      if (!settled) { settled = true; resolve(JSON.stringify(dom)); }
    }
  }, 50);
  setTimeout(() => {
    clearInterval(poll);
    const dom = readDomResults();
    if (dom && !settled) { settled = true; resolve(JSON.stringify(dom)); }
    else finish([], { status: 2 });
  }, 5000);
})"##;

/// Reusable headless Mozilla Firefox WebDriver BiDi session for fast multi-test WPT differential
/// verification (`0` tests passing in Firefox while failing in RixBrowser).
pub struct PersistentFirefoxBidi {
    rt: tokio::runtime::Runtime,
    _guard: ChildGuard,
    profile_dir: PathBuf,
    bidi: BidiClient,
    context: String,
}

impl PersistentFirefoxBidi {
    /// Launches one headless Firefox instance with a clean temporary profile and opens a tab.
    pub fn launch(firefox_bin: &Path) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let profile_dir = std::env::temp_dir().join(format!(
            "rix_ff_wpt_ref_{}_{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let _ = fs::remove_dir_all(&profile_dir);
        write_clean_firefox_profile(&profile_dir, false)?;

        let mut cmd = Command::new(firefox_bin);
        cmd.arg("--headless")
            .arg("--no-remote")
            .arg("--profile")
            .arg(&profile_dir)
            .arg("--remote-debugging-port=0")
            .env("MOZ_HEADLESS_WIDTH", "800")
            .env("MOZ_HEADLESS_HEIGHT", "600")
            .env("MOZ_REMOTE_ALLOW_SYSTEM_ACCESS", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("failed to launch Firefox at {}: {e}", firefox_bin.display()))?;
        let Some(stderr) = child.stderr.take() else {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_dir_all(&profile_dir);
            bail!("failed to capture Firefox stderr");
        };
        let guard = ChildGuard(child);

        let (ws_tx, ws_rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                if let Some(pos) = line.find("WebDriver BiDi listening on ") {
                    let url = line[pos + "WebDriver BiDi listening on ".len()..]
                        .trim()
                        .to_string();
                    let _ = ws_tx.send(url);
                }
            }
        });

        let ws_base = ws_rx
            .recv_timeout(Duration::from_secs(12))
            .map_err(|_| anyhow!("timed out waiting for Firefox WebDriver BiDi endpoint"))?;
        let ws_url = format!("{}/session", ws_base.trim_end_matches('/'));

        let (bidi, context) = rt.block_on(async {
            let mut bidi = BidiClient::connect(&ws_url).await?;
            let _ = bidi
                .call(
                    "session.new",
                    json!({ "capabilities": { "alwaysMatch": { "acceptInsecureCerts": true } } }),
                    Duration::from_secs(10),
                )
                .await?;
            let created = bidi
                .call(
                    "browsingContext.create",
                    json!({ "type": "tab" }),
                    Duration::from_secs(5),
                )
                .await;
            let context = match created
                .ok()
                .and_then(|v| v.get("context").and_then(Value::as_str).map(String::from))
            {
                Some(ctx) => ctx,
                None => {
                    let tree = bidi
                        .call("browsingContext.getTree", json!({}), Duration::from_secs(5))
                        .await?;
                    tree.get("contexts")
                        .and_then(Value::as_array)
                        .and_then(|arr| arr.first())
                        .and_then(|c| c.get("context"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("Firefox BiDi returned no browsing context"))?
                        .to_string()
                }
            };
            let _ = bidi
                .call(
                    "browsingContext.setViewport",
                    json!({
                        "context": context,
                        "viewport": { "width": 800, "height": 600 },
                        "devicePixelRatio": 1.0,
                    }),
                    Duration::from_secs(5),
                )
                .await;
            Ok::<_, anyhow::Error>((bidi, context))
        })?;

        Ok(Self {
            rt,
            _guard: guard,
            profile_dir,
            bidi,
            context,
        })
    }

    /// Navigates the reusable Firefox tab to a local WPT testharness URL and collects subtests.
    pub fn run_wpt_testharness(
        &mut self,
        url: &str,
        timeout: Duration,
    ) -> Result<FirefoxWptResult> {
        let context = self.context.clone();
        self.rt.block_on(async {
            let _ = self
                .bidi
                .call(
                    "browsingContext.navigate",
                    json!({
                        "context": context,
                        "url": url,
                        "wait": "complete",
                    }),
                    timeout,
                )
                .await?;
            let eval_res = self
                .bidi
                .call(
                    "script.evaluate",
                    json!({
                        "expression": FIREFOX_WPT_COLLECT_JS,
                        "target": { "context": context },
                        "awaitPromise": true,
                        "resultOwnership": "none",
                    }),
                    timeout,
                )
                .await?;
            let Some(raw) = extract_eval_string(&eval_res) else {
                return Ok(FirefoxWptResult::default());
            };
            Ok(serde_json::from_str::<FirefoxWptResult>(&raw).unwrap_or_default())
        })
    }
}

impl Drop for PersistentFirefoxBidi {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.profile_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decoder_roundtrips_standard_vectors() {
        assert_eq!(decode_base64("SGVsbG8=").unwrap(), b"Hello");
        assert_eq!(decode_base64("Uml4QnJvd3Nlcg==").unwrap(), b"RixBrowser");
        assert_eq!(decode_base64("Zm9v\nYmFy").unwrap(), b"foobar");
    }

    #[test]
    fn proc_stat_parser_handles_spaces_in_comm() {
        let fake = "1234 (Web Content) S 1 2 3 4 5 6 7 8 9 10 25 15 0 0";
        let ms = parse_proc_stat_cpu_ms(fake).unwrap();
        assert!((ms - 400.0).abs() < 0.01);
    }
}
