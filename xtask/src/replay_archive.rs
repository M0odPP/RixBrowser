//! Deterministic pure-Rust HTTP record/replay archive for live web differential testing.
//!
//! Captures an exact byte-for-byte snapshot of a target page and its static subresources
//! (HTML, CSS, JS, images, fonts, SVGs) and serves them over a local `http://127.0.0.1:<port>`
//! server so both Mozilla Firefox and RixBrowser render the identical frozen byte stream with
//! zero network latency jitter, CDN randomization, or A/B banner drift.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rix_core::url::{RixScheme, RixUrl};
use rix_net::HttpClient;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const MAX_RECORDED_ASSETS: usize = 250;
const ASSET_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Single archived HTTP resource stored on disk in a deterministic replay bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedResource {
    pub id: String,
    pub original_url: String,
    pub local_path: String,
    pub file_name: String,
    pub content_type: String,
    pub status: u16,
}

/// Complete manifest for a frozen single-page replay archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayManifest {
    pub original_url: String,
    pub final_url: String,
    pub origin: String,
    pub entry_path: String,
    pub entry_file: String,
    pub entry_content_type: String,
    pub resources: Vec<ArchivedResource>,
}

fn fnv1a_hex(data: &[u8]) -> String {
    let mut h1: u64 = 0xcbf2_9ce4_8422_2325;
    let mut h2: u64 = 0x8422_2325_cbf2_9ce4;
    for &b in data {
        h1 ^= u64::from(b);
        h1 = h1.wrapping_mul(0x0100_0000_01b3);
        h2 = h2.wrapping_mul(0x0100_0000_01b3) ^ u64::from(b);
    }
    format!("{h1:016x}{h2:016x}")
}

fn url_origin(url: &RixUrl) -> String {
    let raw_scheme = url.scheme();
    let scheme = match &raw_scheme {
        RixScheme::Http => "http",
        RixScheme::Https => "https",
        RixScheme::File => "file",
        RixScheme::About => "about",
        RixScheme::Data => "data",
        RixScheme::Rix => "rix",
        RixScheme::Custom(s) => s.as_str(),
    };
    let host = url.host_str().unwrap_or("");
    match url.port() {
        Some(p) => format!("{scheme}://{host}:{p}"),
        None => format!("{scheme}://{host}"),
    }
}

fn url_path_and_query(url: &RixUrl) -> String {
    let path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };
    match url.query() {
        Some(q) if !q.is_empty() => format!("{path}?{q}"),
        _ => path.to_string(),
    }
}

fn safe_basename(raw_url: &str) -> String {
    let without_q = raw_url.split(['?', '#']).next().unwrap_or("asset");
    let seg = without_q.rsplit('/').next().unwrap_or("asset");
    let clean: String = seg
        .chars()
        .take(48)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() || clean == "." || clean == ".." {
        "asset.bin".to_string()
    } else {
        clean
    }
}

fn resolve_candidate(base: &RixUrl, raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches(['"', '\'']).trim();
    if trimmed.is_empty()
        || trimmed.starts_with("data:")
        || trimmed.starts_with("javascript:")
        || trimmed.starts_with("mailto:")
        || trimmed.starts_with("blob:")
        || trimmed.starts_with('#')
    {
        return None;
    }
    let resolved = base.join(trimmed).ok()?;
    if matches!(resolved.scheme(), RixScheme::Http | RixScheme::Https) {
        Some(resolved.as_str().to_string())
    } else {
        None
    }
}

/// Extracts candidate subresource URLs from HTML (`href`, `src`, `srcset`, inline `url(...)`).
pub fn extract_html_subresource_urls(html: &str, base: &RixUrl) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |raw: &str| {
        let raw_trimmed = raw.trim();
        if let Some(abs) = resolve_candidate(base, raw_trimmed) {
            if seen.insert(abs.clone()) {
                found.push((raw_trimmed.to_string(), abs));
            }
        }
    };

    // Scan HTML attribute pairs (`src=...`, `href=...`, `poster=...`, `srcset=...`).
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let Some(lt) = lower[pos..].find('<') else {
            break;
        };
        let tag_start = pos + lt;
        let Some(gt_rel) = lower[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + gt_rel;
        let tag_slice = &html[tag_start + 1..tag_end];
        let tag_lower = &lower[tag_start + 1..tag_end];
        let tag_name = tag_lower
            .split(|c: char| c.is_ascii_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        let is_resource_tag = matches!(
            tag_name,
            "link" | "script" | "img" | "source" | "video" | "audio" | "image" | "use" | "iframe"
        );
        if is_resource_tag {
            let is_link = tag_name == "link";
            let is_link_asset = !is_link
                || tag_lower.contains("stylesheet")
                || tag_lower.contains("preload")
                || tag_lower.contains("icon")
                || tag_lower.contains("modulepreload");
            if is_link_asset {
                for attr in ["src", "href", "xlink:href", "poster"] {
                    if let Some(val) = extract_attr_value(tag_slice, tag_lower, attr) {
                        push(&val);
                    }
                }
                if let Some(srcset) = extract_attr_value(tag_slice, tag_lower, "srcset") {
                    for entry in srcset.split(',') {
                        if let Some(first_tok) = entry.split_whitespace().next() {
                            push(first_tok);
                        }
                    }
                }
            }
        }
        pos = tag_end + 1;
    }

    // Also scan CSS `url(...)` inside inline `<style>` blocks or `style="..."` attributes.
    for (raw, abs) in extract_css_urls(html, base) {
        if seen.insert(abs.clone()) {
            found.push((raw, abs));
        }
    }

    found
}

fn extract_attr_value(tag_orig: &str, tag_lower: &str, attr_name: &str) -> Option<String> {
    let needle = format!("{attr_name}=");
    let mut offset = 0usize;
    while let Some(idx) = tag_lower[offset..].find(&needle) {
        let abs_idx = offset + idx;
        let boundary_ok = abs_idx == 0
            || tag_lower
                .as_bytes()
                .get(abs_idx - 1)
                .is_some_and(|b| b.is_ascii_whitespace());
        if !boundary_ok {
            offset = abs_idx + needle.len();
            continue;
        }
        let val_start = abs_idx + needle.len();
        let rest = tag_orig.get(val_start..)?.trim_start();
        let first = rest.as_bytes().first().copied()?;
        if first == b'"' || first == b'\'' {
            let quote = first as char;
            let end = rest[1..].find(quote)?;
            return Some(rest[1..1 + end].replace("&amp;", "&"));
        }
        let end = rest
            .find(|c: char| c.is_ascii_whitespace() || c == '>')
            .unwrap_or(rest.len());
        return Some(rest[..end].replace("&amp;", "&"));
    }
    None
}

/// Extracts `url(...)` and `@import "..."` references from a CSS document.
pub fn extract_css_urls(css: &str, base: &RixUrl) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let lower = css.to_ascii_lowercase();
    let mut pos = 0usize;
    while let Some(idx) = lower[pos..].find("url(") {
        let start = pos + idx + 4;
        let Some(end_rel) = css[start..].find(')') else {
            break;
        };
        let inner = css[start..start + end_rel].trim();
        let unquoted = inner.trim_matches(['"', '\'']).trim();
        if let Some(abs) = resolve_candidate(base, unquoted) {
            if seen.insert(abs.clone()) {
                found.push((unquoted.to_string(), abs));
            }
        }
        pos = start + end_rel + 1;
    }

    // Handle `@import "..."` without `url(...)`.
    pos = 0;
    while let Some(idx) = lower[pos..].find("@import") {
        let after = css[pos + idx + 7..].trim_start();
        if let Some(first) = after.as_bytes().first().copied() {
            if first == b'"' || first == b'\'' {
                let q = first as char;
                if let Some(end) = after[1..].find(q) {
                    let raw = after[1..1 + end].trim();
                    if let Some(abs) = resolve_candidate(base, raw) {
                        if seen.insert(abs.clone()) {
                            found.push((raw.to_string(), abs));
                        }
                    }
                }
            }
        }
        pos = pos + idx + 7;
    }
    found
}

/// Rewrites recorded external URLs inside HTML or CSS text to local `/__rix_replay/...` paths.
fn rewrite_text_urls(
    mut text: String,
    base: &RixUrl,
    url_to_local: &HashMap<String, String>,
    raw_pairs: &[(String, String)],
) -> String {
    // Longest raw tokens first to avoid partial prefix replacements.
    let mut replacements: Vec<(String, String)> = Vec::new();
    let mut seen = HashSet::new();
    for (raw, abs) in raw_pairs {
        if let Some(local) = url_to_local.get(abs) {
            if !raw.is_empty() && seen.insert(raw.clone()) {
                replacements.push((raw.clone(), local.clone()));
            }
            if seen.insert(abs.clone()) {
                replacements.push((abs.clone(), local.clone()));
            }
            // Also handle HTML-escaped `&amp;` form.
            let escaped = raw.replace('&', "&amp;");
            if escaped != *raw && seen.insert(escaped.clone()) {
                replacements.push((escaped, local.clone()));
            }
        }
    }
    let _ = base;
    replacements.sort_by_key(|a| std::cmp::Reverse(a.0.len()));
    for (from, to) in replacements {
        if from.starts_with("http://")
            || from.starts_with("https://")
            || from.starts_with("//")
            || from.starts_with('/')
            || from.starts_with("./")
            || from.starts_with("../")
        {
            text = text.replace(&from, &to);
        }
    }
    text
}

/// Removes `integrity="..."` / `integrity='...'` attributes from HTML tags in a frozen replay
/// document so rewritten local subresources are not rejected by browser SRI hash verification.
fn strip_sri_integrity_attrs(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    if !lower.contains("integrity=") {
        return html.to_string();
    }
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(rel) = lower[i..].find("integrity=") {
            let pos = i + rel;
            let boundary_ok = pos > 0 && bytes[pos - 1].is_ascii_whitespace();
            if boundary_ok {
                out.push_str(&html[i..pos]);
                let mut k = pos + "integrity=".len();
                while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                    k += 1;
                }
                if k < bytes.len() && (bytes[k] == b'"' || bytes[k] == b'\'') {
                    let q = bytes[k];
                    k += 1;
                    while k < bytes.len() && bytes[k] != q {
                        k += 1;
                    }
                    if k < bytes.len() {
                        k += 1;
                    }
                } else {
                    while k < bytes.len() && !bytes[k].is_ascii_whitespace() && bytes[k] != b'>' {
                        k += 1;
                    }
                }
                i = k;
                continue;
            }
            out.push_str(&html[i..pos + "integrity=".len()]);
            i = pos + "integrity=".len();
        } else {
            out.push_str(&html[i..]);
            break;
        }
    }
    out
}

/// Records `url` and its transitive static subresources into a frozen deterministic replay bundle
/// under `archive_dir`.
pub fn record_site_archive(url: &str, archive_dir: &Path) -> Result<ReplayManifest> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .context("build record runtime")?;
    rt.block_on(record_site_archive_async(url, archive_dir))
}

async fn record_site_archive_async(url: &str, archive_dir: &Path) -> Result<ReplayManifest> {
    let assets_dir = archive_dir.join("assets");
    fs::create_dir_all(&assets_dir)?;

    let client = HttpClient::new(None)?;
    let parsed = RixUrl::parse(url).map_err(|e| anyhow::anyhow!("invalid URL {url}: {e}"))?;
    let doc_resp = tokio::time::timeout(Duration::from_secs(20), client.fetch(&parsed))
        .await
        .context("main document fetch timed out")??;
    let final_url = doc_resp.final_url.clone();
    let html_raw = String::from_utf8_lossy(&doc_resp.body).into_owned();
    let entry_ct = if doc_resp.content_type.is_empty() {
        "text/html; charset=utf-8".to_string()
    } else {
        doc_resp.content_type.clone()
    };

    let html_refs = extract_html_subresource_urls(&html_raw, &final_url);
    let mut resources: Vec<ArchivedResource> = Vec::new();
    let mut url_to_local: HashMap<String, String> = HashMap::new();
    let mut fetched_urls: HashSet<String> = HashSet::new();
    let mut css_to_rewrite: Vec<(usize, String, RixUrl)> = Vec::new();

    // Wave 1: fetch HTML-referenced subresources with bounded concurrency.
    let wave1_urls: Vec<String> = html_refs
        .iter()
        .map(|(_, abs)| abs.clone())
        .filter(|u| fetched_urls.insert(u.clone()))
        .take(MAX_RECORDED_ASSETS)
        .collect();

    let wave1_results = fetch_many(&client, wave1_urls).await;
    let mut wave2_queue: Vec<String> = Vec::new();

    for (orig_url, resp) in wave1_results {
        let id = fnv1a_hex(orig_url.as_bytes());
        let base_name = safe_basename(&orig_url);
        let file_name = format!("{id}_{base_name}");
        let local_path = format!("/__rix_replay/{id}/{base_name}");
        let ct = if resp.content_type.is_empty() {
            guess_mime(&base_name).to_string()
        } else {
            resp.content_type.clone()
        };
        let is_css = ct.to_ascii_lowercase().contains("text/css")
            || base_name.to_ascii_lowercase().ends_with(".css");

        fs::write(assets_dir.join(&file_name), &resp.body)?;
        url_to_local.insert(orig_url.clone(), local_path.clone());
        url_to_local.insert(resp.final_url.as_str().to_string(), local_path.clone());

        let idx = resources.len();
        resources.push(ArchivedResource {
            id,
            original_url: orig_url.clone(),
            local_path,
            file_name,
            content_type: ct,
            status: resp.status,
        });

        if is_css {
            let css_text = String::from_utf8_lossy(&resp.body).into_owned();
            for (_, nested_abs) in extract_css_urls(&css_text, &resp.final_url) {
                if resources.len() + wave2_queue.len() < MAX_RECORDED_ASSETS
                    && fetched_urls.insert(nested_abs.clone())
                {
                    wave2_queue.push(nested_abs);
                }
            }
            css_to_rewrite.push((idx, css_text, resp.final_url));
        }
    }

    // Wave 2: fetch CSS-referenced fonts, background images, and `@import` sheets.
    if !wave2_queue.is_empty() {
        let wave2_results = fetch_many(&client, wave2_queue).await;
        for (orig_url, resp) in wave2_results {
            let id = fnv1a_hex(orig_url.as_bytes());
            let base_name = safe_basename(&orig_url);
            let file_name = format!("{id}_{base_name}");
            let local_path = format!("/__rix_replay/{id}/{base_name}");
            let ct = if resp.content_type.is_empty() {
                guess_mime(&base_name).to_string()
            } else {
                resp.content_type.clone()
            };
            fs::write(assets_dir.join(&file_name), &resp.body)?;
            url_to_local.insert(orig_url.clone(), local_path.clone());
            url_to_local.insert(resp.final_url.as_str().to_string(), local_path.clone());
            resources.push(ArchivedResource {
                id,
                original_url: orig_url,
                local_path,
                file_name,
                content_type: ct,
                status: resp.status,
            });
        }
    }

    // Rewrite CSS files so `url(...)` points strictly to local frozen replay assets.
    for (res_idx, css_text, css_base) in css_to_rewrite {
        let css_refs = extract_css_urls(&css_text, &css_base);
        let rewritten = rewrite_text_urls(css_text, &css_base, &url_to_local, &css_refs);
        let file_name = &resources[res_idx].file_name;
        fs::write(assets_dir.join(file_name), rewritten.as_bytes())?;
    }

    // Rewrite entry HTML document so subresources resolve deterministically offline.
    // Strip `integrity="..."` attributes because rewriting CSS `url(...)` references inside
    // archived stylesheets alters their byte digest, which would otherwise cause strict SRI
    // checks in Mozilla Firefox to reject frozen local stylesheets.
    let rewritten_html = strip_sri_integrity_attrs(&rewrite_text_urls(
        html_raw,
        &final_url,
        &url_to_local,
        &html_refs,
    ));
    let entry_file = "entry.html".to_string();
    fs::write(archive_dir.join(&entry_file), rewritten_html.as_bytes())?;

    let manifest = ReplayManifest {
        original_url: url.to_string(),
        final_url: final_url.as_str().to_string(),
        origin: url_origin(&final_url),
        entry_path: url_path_and_query(&final_url),
        entry_file,
        entry_content_type: entry_ct,
        resources,
    };
    fs::write(
        archive_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    Ok(manifest)
}

async fn fetch_many(
    client: &HttpClient,
    urls: Vec<String>,
) -> Vec<(String, rix_net::HttpResponse)> {
    let sem = Arc::new(tokio::sync::Semaphore::new(12));
    let mut handles = Vec::with_capacity(urls.len());
    for u in urls {
        let sem = Arc::clone(&sem);
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                return None;
            };
            let parsed = RixUrl::parse(&u).ok()?;
            let resp = tokio::time::timeout(ASSET_FETCH_TIMEOUT, client.fetch(&parsed))
                .await
                .ok()?
                .ok()?;
            if (200..400).contains(&resp.status) {
                Some((u, resp))
            } else {
                None
            }
        }));
    }
    let mut out = Vec::new();
    for h in handles {
        if let Ok(Some(pair)) = h.await {
            out.push(pair);
        }
    }
    out
}

fn guess_mime(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if lower.ends_with(".js") || lower.ends_with(".mjs") {
        "application/javascript; charset=utf-8"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".avif") {
        "image/avif"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".woff2") {
        "font/woff2"
    } else if lower.ends_with(".woff") {
        "font/woff"
    } else if lower.ends_with(".ttf") {
        "font/ttf"
    } else if lower.ends_with(".json") {
        "application/json; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

/// Running local HTTP/1.1 replay server serving one frozen [`ReplayManifest`] deterministically.
pub struct ReplayServer {
    _rt: tokio::runtime::Runtime,
    pub entry_url: String,
    pub port: u16,
}

impl ReplayServer {
    /// Starts an ephemeral loopback HTTP server serving the archive stored in `archive_dir`.
    pub fn start(archive_dir: &Path) -> Result<Self> {
        let manifest_raw = fs::read_to_string(archive_dir.join("manifest.json"))
            .with_context(|| format!("read replay manifest in {}", archive_dir.display()))?;
        let manifest: ReplayManifest = serde_json::from_str(&manifest_raw)?;
        let raw_entry = fs::read(archive_dir.join(&manifest.entry_file))?;
        let entry_bytes = if manifest
            .entry_content_type
            .to_ascii_lowercase()
            .contains("html")
        {
            let html = String::from_utf8_lossy(&raw_entry);
            Arc::new(strip_sri_integrity_attrs(&html).into_bytes())
        } else {
            Arc::new(raw_entry)
        };
        let entry_ct = Arc::new(manifest.entry_content_type.clone());
        let entry_path = manifest.entry_path.clone();

        let mut routes: HashMap<String, (PathBuf, String, u16)> = HashMap::new();
        let assets_dir = archive_dir.join("assets");
        for res in &manifest.resources {
            let file_path = assets_dir.join(&res.file_name);
            let val = (file_path.clone(), res.content_type.clone(), res.status);
            routes.insert(res.local_path.clone(), val.clone());
            // Also register same-origin path mapping for dynamic JS `fetch("/...")` requests.
            if let Some(rel) = res.original_url.strip_prefix(&manifest.origin) {
                let rel_path = if rel.starts_with('/') {
                    rel.to_string()
                } else {
                    format!("/{rel}")
                };
                routes.entry(rel_path).or_insert(val);
            }
        }
        let routes = Arc::new(routes);
        let entry_path_only = entry_path.split('?').next().unwrap_or("/").to_string();
        let entry_path_arc = Arc::new(entry_path_only);

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .context("build replay server runtime")?;
        let (listener, port) = rt.block_on(async {
            let l = TcpListener::bind("127.0.0.1:0").await?;
            let p = l.local_addr()?.port();
            Ok::<_, anyhow::Error>((l, p))
        })?;

        let routes_c = Arc::clone(&routes);
        let entry_bytes_c = Arc::clone(&entry_bytes);
        let entry_ct_c = Arc::clone(&entry_ct);
        let entry_path_c = Arc::clone(&entry_path_arc);

        rt.spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let routes = Arc::clone(&routes_c);
                let entry_bytes = Arc::clone(&entry_bytes_c);
                let entry_ct = Arc::clone(&entry_ct_c);
                let entry_path = Arc::clone(&entry_path_c);
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let Ok(Ok(n)) =
                        tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf)).await
                    else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let target = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let target_path = target.split('?').next().unwrap_or("/");

                    if target == entry_path.as_str()
                        || target_path == entry_path.as_str()
                        || target_path == "/"
                    {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nCache-Control: public, max-age=3600\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            entry_ct,
                            entry_bytes.len()
                        );
                        let _ = stream.write_all(head.as_bytes()).await;
                        let _ = stream.write_all(&entry_bytes).await;
                        let _ = stream.shutdown().await;
                        return;
                    }

                    let matched = routes.get(target).or_else(|| routes.get(target_path));
                    if let Some((path, ct, status)) = matched {
                        if let Ok(body) = tokio::fs::read(path).await {
                            let reason = if *status == 200 { "OK" } else { "Replay" };
                            let head = format!(
                                "HTTP/1.1 {status} {reason}\r\nContent-Type: {ct}\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: public, max-age=31536000, immutable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(head.as_bytes()).await;
                            let _ = stream.write_all(&body).await;
                            let _ = stream.shutdown().await;
                            return;
                        }
                    }

                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 404 Not Found\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        let norm_entry = if entry_path.starts_with('/') {
            entry_path
        } else {
            format!("/{entry_path}")
        };
        let entry_url = format!("http://127.0.0.1:{port}{norm_entry}");
        if entry_url.is_empty() {
            bail!("empty replay entry url");
        }
        Ok(Self {
            _rt: rt,
            entry_url,
            port,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_html_and_css_subresources_cleanly() {
        let base = RixUrl::parse("https://example.org/docs/index.html").unwrap();
        let html = r#"<!doctype html>
        <link rel="stylesheet" href="/static/app.css">
        <script src="./bundle.js"></script>
        <img src="https://cdn.example.net/hero.png" srcset="hero-1x.png 1x, hero-2x.png 2x">
        <style>.bg { background: url('../img/tile.svg'); }</style>"#;
        let refs = extract_html_subresource_urls(html, &base);
        let urls: Vec<&str> = refs.iter().map(|(_, u)| u.as_str()).collect();
        assert!(urls.contains(&"https://example.org/static/app.css"));
        assert!(urls.contains(&"https://example.org/docs/bundle.js"));
        assert!(urls.contains(&"https://cdn.example.net/hero.png"));
        assert!(urls.contains(&"https://example.org/docs/hero-1x.png"));
        assert!(urls.contains(&"https://example.org/docs/hero-2x.png"));
        assert!(urls.contains(&"https://example.org/img/tile.svg"));
    }

    #[test]
    fn replay_server_serves_frozen_manifest_offline() {
        let dir = std::env::temp_dir().join(format!("rix_replay_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("assets")).unwrap();
        fs::write(dir.join("entry.html"), b"<html><body>frozen</body></html>").unwrap();
        fs::write(dir.join("assets/a1_app.css"), b"body { color: red; }").unwrap();
        let manifest = ReplayManifest {
            original_url: "https://example.org/page".into(),
            final_url: "https://example.org/page".into(),
            origin: "https://example.org".into(),
            entry_path: "/page".into(),
            entry_file: "entry.html".into(),
            entry_content_type: "text/html; charset=utf-8".into(),
            resources: vec![ArchivedResource {
                id: "a1".into(),
                original_url: "https://example.org/app.css".into(),
                local_path: "/__rix_replay/a1/app.css".into(),
                file_name: "a1_app.css".into(),
                content_type: "text/css".into(),
                status: 200,
            }],
        };
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let srv = ReplayServer::start(&dir).unwrap();
        let client = HttpClient::new(None).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let html = rt
            .block_on(client.fetch(&RixUrl::parse(&srv.entry_url).unwrap()))
            .unwrap();
        assert_eq!(html.status, 200);
        assert!(String::from_utf8_lossy(&html.body).contains("frozen"));

        let css_url = format!("http://127.0.0.1:{}/__rix_replay/a1/app.css", srv.port);
        let css = rt
            .block_on(client.fetch(&RixUrl::parse(&css_url).unwrap()))
            .unwrap();
        assert_eq!(css.status, 200);
        assert_eq!(String::from_utf8_lossy(&css.body), "body { color: red; }");

        let _ = fs::remove_dir_all(&dir);
    }
}
