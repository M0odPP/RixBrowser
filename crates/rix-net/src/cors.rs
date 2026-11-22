//! WHATWG Fetch CORS enforcement, `OPTIONS` preflight with
//! `Access-Control-Max-Age` caching, `Access-Control-Expose-Headers` filtering,
//! opaque `no-cors` responses, and zero-polling `AbortController` cancellation.
//!
//! Reference: Spec 06 §2 & Issue #17.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rix_core::url::RixUrl;
use tokio::sync::{Mutex, Notify};

use crate::client::{same_origin, HttpClient};
use crate::error::{NetError, Result};

/// Maximum preflight cache TTL per Fetch standard (2 hours).
const MAX_PREFLIGHT_AGE_SECS: u64 = 7_200;

/// WHATWG Fetch request `mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FetchMode {
    #[default]
    Cors,
    NoCors,
    SameOrigin,
}

/// WHATWG Fetch request `credentials`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FetchCredentials {
    Omit,
    #[default]
    SameOrigin,
    Include,
}

/// WHATWG Fetch request `redirect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FetchRedirect {
    #[default]
    Follow,
    Error,
    Manual,
}

/// WHATWG Fetch response `type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchResponseType {
    Basic,
    Cors,
    Opaque,
    OpaqueRedirect,
}

impl FetchResponseType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Cors => "cors",
            Self::Opaque => "opaque",
            Self::OpaqueRedirect => "opaqueredirect",
        }
    }
}

/// Full Fetch request descriptor passed into [`FetchEngine::fetch`].
#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub url: String,
    pub method: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub mode: FetchMode,
    pub credentials: FetchCredentials,
    pub redirect: FetchRedirect,
    /// Initiating document origin (e.g. `"https://app.example"` or `"http://127.0.0.1:8080"`).
    pub origin: String,
    /// Optional instant-wakeup abort signal (`AbortController`).
    pub abort_signal: Option<AbortHandle>,
}

/// Zero-polling reactive abort handle shared between JS `AbortController` and `FetchEngine`.
#[derive(Debug, Clone, Default)]
pub struct AbortHandle {
    aborted: Arc<std::sync::atomic::AtomicBool>,
    notify: Arc<Notify>,
}

impl AbortHandle {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn abort(&self) {
        self.aborted
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.aborted.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub async fn wait_aborted(&self) {
        if self.is_aborted() {
            return;
        }
        let fut = self.notify.notified();
        if self.is_aborted() {
            return;
        }
        fut.await;
    }
}

/// CORS-filtered WHATWG Fetch response returned by [`FetchEngine::fetch`].
#[derive(Debug, Clone)]
pub struct FetchResponse {
    pub url: String,
    pub status: u16,
    pub status_text: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub response_type: FetchResponseType,
    pub redirected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PreflightCacheKey {
    origin: String,
    url: String,
    include_credentials: bool,
}

#[derive(Debug, Clone)]
struct PreflightCacheEntry {
    expires_at: Instant,
    methods: HashSet<String>,
    headers: HashSet<String>,
    allow_wildcard_methods: bool,
    allow_wildcard_headers: bool,
}

/// Per-origin Fetch CORS preflight cache (`Access-Control-Max-Age`).
#[derive(Debug, Default)]
pub struct PreflightCache {
    entries: HashMap<PreflightCacheKey, PreflightCacheEntry>,
}

impl PreflightCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn is_permitted(
        &mut self,
        key: &PreflightCacheKey,
        method: &str,
        non_safelisted_headers: &[String],
    ) -> bool {
        let now = Instant::now();
        let Some(entry) = self.entries.get(key) else {
            return false;
        };
        if now >= entry.expires_at {
            self.entries.remove(key);
            return false;
        }
        let method_ok = is_cors_safelisted_method(method)
            || entry.methods.contains(method)
            || (entry.allow_wildcard_methods && !key.include_credentials);
        if !method_ok {
            return false;
        }
        non_safelisted_headers.iter().all(|h| {
            entry.headers.contains(h)
                || (entry.allow_wildcard_headers
                    && !key.include_credentials
                    && h != "authorization")
        })
    }

    fn store(&mut self, key: PreflightCacheKey, entry: PreflightCacheEntry) {
        if entry.expires_at > Instant::now() {
            self.entries.insert(key, entry);
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// High-level WHATWG Fetch engine wrapping [`HttpClient`] with full CORS enforcement
/// and preflight caching.
#[derive(Clone)]
pub struct FetchEngine {
    client: HttpClient,
    preflight_cache: Arc<Mutex<PreflightCache>>,
}

impl FetchEngine {
    #[must_use]
    pub fn new(client: HttpClient) -> Self {
        Self {
            client,
            preflight_cache: Arc::new(Mutex::new(PreflightCache::new())),
        }
    }

    /// Returns a reference to the underlying [`HttpClient`].
    #[must_use]
    pub fn client(&self) -> &HttpClient {
        &self.client
    }

    /// Executes a WHATWG Fetch request with full CORS preflight, header filtering,
    /// opaque `no-cors` handling, and instant zero-polling `AbortHandle` cancellation.
    pub async fn fetch(&self, req: FetchRequest) -> Result<FetchResponse> {
        let abort = req.abort_signal.clone();
        if abort.as_ref().is_some_and(AbortHandle::is_aborted) {
            return Err(NetError::Aborted);
        }
        if let Some(ab) = abort {
            tokio::select! {
                biased;
                () = ab.wait_aborted() => Err(NetError::Aborted),
                res = self.fetch_inner(req) => res,
            }
        } else {
            self.fetch_inner(req).await
        }
    }

    async fn fetch_inner(&self, req: FetchRequest) -> Result<FetchResponse> {
        let target_url = RixUrl::parse(&req.url)?;
        let origin_url = RixUrl::parse(&req.origin).ok();
        let is_same_origin = origin_url
            .as_ref()
            .is_some_and(|orig| same_origin(orig, &target_url));
        let method = req.method.to_ascii_uppercase();

        if !is_same_origin && req.mode == FetchMode::SameOrigin {
            return Err(NetError::Cors(format!(
                "Cross-origin request to {} blocked by mode 'same-origin'",
                req.url
            )));
        }

        // Cross-origin `no-cors` restricts to CORS-safelisted methods/headers and yields an opaque response.
        if !is_same_origin && req.mode == FetchMode::NoCors {
            if !is_cors_safelisted_method(&method) {
                return Err(NetError::Cors(format!(
                    "Method '{method}' is not allowed in 'no-cors' mode"
                )));
            }
            let mut filtered_headers = HashMap::new();
            for (k, v) in &req.headers {
                if is_cors_safelisted_request_header(k, v) {
                    filtered_headers.insert(k.clone(), v.clone());
                }
            }
            let _ = self
                .send_raw(&target_url, &method, &filtered_headers, req.body, false)
                .await?;
            return Ok(FetchResponse {
                url: String::new(),
                status: 0,
                status_text: String::new(),
                headers: HashMap::new(),
                body: Vec::new(),
                response_type: FetchResponseType::Opaque,
                redirected: false,
            });
        }

        let include_credentials = match req.credentials {
            FetchCredentials::Include => true,
            FetchCredentials::SameOrigin => is_same_origin,
            FetchCredentials::Omit => false,
        };

        // Cross-origin CORS mode: determine if an automatic `OPTIONS` preflight is required.
        let mut wire_headers = req.headers.clone();
        if !is_same_origin && req.mode == FetchMode::Cors {
            wire_headers.insert("Origin".to_string(), req.origin.clone());
            let mut unsafe_headers: Vec<String> = req
                .headers
                .iter()
                .filter(|(k, v)| !is_cors_safelisted_request_header(k, v))
                .map(|(k, _)| k.to_ascii_lowercase())
                .collect();
            unsafe_headers.sort();
            unsafe_headers.dedup();

            let needs_preflight = !is_cors_safelisted_method(&method) || !unsafe_headers.is_empty();
            if needs_preflight {
                self.ensure_cors_preflight(
                    &target_url,
                    &req.origin,
                    &method,
                    &unsafe_headers,
                    include_credentials,
                )
                .await?;
            }
        }

        let resp = self
            .send_raw(
                &target_url,
                &method,
                &wire_headers,
                req.body,
                include_credentials,
            )
            .await?;

        let redirected = resp.final_url.as_str() != target_url.as_str();
        if redirected && req.redirect == FetchRedirect::Error {
            return Err(NetError::Http(
                "Redirect blocked by redirect: 'error'".into(),
            ));
        }

        if is_same_origin {
            return Ok(FetchResponse {
                url: resp.final_url.as_str().to_string(),
                status: resp.status,
                status_text: reason_phrase(resp.status).to_string(),
                headers: resp.headers,
                body: resp.body,
                response_type: FetchResponseType::Basic,
                redirected,
            });
        }

        // Cross-origin CORS response validation.
        validate_cors_response(&resp.headers, &req.origin, include_credentials)?;
        let filtered = filter_cors_response_headers(&resp.headers, include_credentials);

        Ok(FetchResponse {
            url: resp.final_url.as_str().to_string(),
            status: resp.status,
            status_text: reason_phrase(resp.status).to_string(),
            headers: filtered,
            body: resp.body,
            response_type: FetchResponseType::Cors,
            redirected,
        })
    }

    async fn ensure_cors_preflight(
        &self,
        url: &RixUrl,
        origin: &str,
        method: &str,
        unsafe_headers: &[String],
        include_credentials: bool,
    ) -> Result<()> {
        let cache_key = PreflightCacheKey {
            origin: origin.to_string(),
            url: url.as_str().to_string(),
            include_credentials,
        };
        if self
            .preflight_cache
            .lock()
            .await
            .is_permitted(&cache_key, method, unsafe_headers)
        {
            return Ok(());
        }

        let mut preflight_headers = HashMap::new();
        preflight_headers.insert("Origin".to_string(), origin.to_string());
        preflight_headers.insert(
            "Access-Control-Request-Method".to_string(),
            method.to_string(),
        );
        if !unsafe_headers.is_empty() {
            preflight_headers.insert(
                "Access-Control-Request-Headers".to_string(),
                unsafe_headers.join(", "),
            );
        }

        let pre_resp = self
            .send_raw(url, "OPTIONS", &preflight_headers, Vec::new(), false)
            .await?;

        if !(200..300).contains(&pre_resp.status) {
            return Err(NetError::Cors(format!(
                "Preflight OPTIONS returned non-OK status {}",
                pre_resp.status
            )));
        }

        validate_cors_response(&pre_resp.headers, origin, include_credentials)?;

        // Validate Access-Control-Allow-Methods.
        let allow_methods_raw =
            get_header_ci(&pre_resp.headers, "access-control-allow-methods").unwrap_or_default();
        let mut allowed_methods = HashSet::new();
        let mut allow_wildcard_methods = false;
        for tok in allow_methods_raw.split(',') {
            let t = tok.trim();
            if t == "*" {
                allow_wildcard_methods = true;
            } else if !t.is_empty() {
                allowed_methods.insert(t.to_ascii_uppercase());
            }
        }
        let method_ok = is_cors_safelisted_method(method)
            || allowed_methods.contains(method)
            || (allow_wildcard_methods && !include_credentials);
        if !method_ok {
            return Err(NetError::Cors(format!(
                "Method '{method}' is not allowed by Access-Control-Allow-Methods"
            )));
        }

        // Validate Access-Control-Allow-Headers.
        let allow_headers_raw =
            get_header_ci(&pre_resp.headers, "access-control-allow-headers").unwrap_or_default();
        let mut allowed_headers = HashSet::new();
        let mut allow_wildcard_headers = false;
        for tok in allow_headers_raw.split(',') {
            let t = tok.trim().to_ascii_lowercase();
            if t == "*" {
                allow_wildcard_headers = true;
            } else if !t.is_empty() {
                allowed_headers.insert(t);
            }
        }
        for req_h in unsafe_headers {
            let ok = allowed_headers.contains(req_h)
                || (allow_wildcard_headers && !include_credentials && req_h != "authorization");
            if !ok {
                return Err(NetError::Cors(format!(
                    "Header '{req_h}' is not allowed by Access-Control-Allow-Headers"
                )));
            }
        }

        // Cache preflight result according to `Access-Control-Max-Age` (defaults to 5s per Fetch spec).
        let max_age_secs = get_header_ci(&pre_resp.headers, "access-control-max-age")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(5)
            .min(MAX_PREFLIGHT_AGE_SECS);
        if max_age_secs > 0 {
            self.preflight_cache.lock().await.store(
                cache_key,
                PreflightCacheEntry {
                    expires_at: Instant::now() + Duration::from_secs(max_age_secs),
                    methods: allowed_methods,
                    headers: allowed_headers,
                    allow_wildcard_methods,
                    allow_wildcard_headers,
                },
            );
        }
        Ok(())
    }

    async fn send_raw(
        &self,
        url: &RixUrl,
        method: &str,
        headers: &HashMap<String, String>,
        body: Vec<u8>,
        include_credentials: bool,
    ) -> Result<crate::client::HttpResponse> {
        let mut client = self.client.clone();
        if !include_credentials {
            client.set_storage(None);
        }
        client.request(url, method, headers, body).await
    }
}

/// Returns `true` if `method` is a CORS-safelisted HTTP method (`GET`, `HEAD`, `POST`).
#[must_use]
pub fn is_cors_safelisted_method(method: &str) -> bool {
    matches!(method, "GET" | "HEAD" | "POST")
}

/// Returns `true` if `(name, value)` is a CORS-safelisted request header per Fetch spec.
#[must_use]
pub fn is_cors_safelisted_request_header(name: &str, value: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "accept" | "accept-language" | "content-language" | "range" => true,
        "content-type" => {
            let essence = value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            matches!(
                essence.as_str(),
                "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain"
            )
        }
        _ => false,
    }
}

/// Validates `Access-Control-Allow-Origin` and `Access-Control-Allow-Credentials`.
pub fn validate_cors_response(
    headers: &HashMap<String, String>,
    origin: &str,
    include_credentials: bool,
) -> Result<()> {
    let Some(acao) = get_header_ci(headers, "access-control-allow-origin") else {
        return Err(NetError::Cors(
            "Missing Access-Control-Allow-Origin header".into(),
        ));
    };
    let acao = acao.trim();
    if include_credentials {
        if acao == "*" {
            return Err(NetError::Cors(
                "Access-Control-Allow-Origin cannot be '*' when credentials mode is 'include'"
                    .into(),
            ));
        }
        if acao != origin {
            return Err(NetError::Cors(format!(
                "Access-Control-Allow-Origin '{acao}' does not match origin '{origin}'"
            )));
        }
        let acac = get_header_ci(headers, "access-control-allow-credentials").unwrap_or("");
        if acac.trim() != "true" {
            return Err(NetError::Cors(
                "Missing 'Access-Control-Allow-Credentials: true' for credentialed request".into(),
            ));
        }
    } else if acao != "*" && acao != origin {
        return Err(NetError::Cors(format!(
            "Access-Control-Allow-Origin '{acao}' does not match origin '{origin}'"
        )));
    }
    Ok(())
}

/// Filters cross-origin response headers to CORS-safelisted + `Access-Control-Expose-Headers`.
#[must_use]
pub fn filter_cors_response_headers(
    headers: &HashMap<String, String>,
    include_credentials: bool,
) -> HashMap<String, String> {
    let expose_raw = get_header_ci(headers, "access-control-expose-headers").unwrap_or("");
    let mut expose_set = HashSet::new();
    let mut expose_all = false;
    for tok in expose_raw.split(',') {
        let t = tok.trim().to_ascii_lowercase();
        if t == "*" && !include_credentials {
            expose_all = true;
        } else if !t.is_empty() {
            expose_set.insert(t);
        }
    }

    let mut out = HashMap::new();
    for (k, v) in headers {
        let lower = k.to_ascii_lowercase();
        if lower == "set-cookie" || lower == "set-cookie2" {
            continue;
        }
        let safelisted = matches!(
            lower.as_str(),
            "cache-control"
                | "content-language"
                | "content-length"
                | "content-type"
                | "expires"
                | "last-modified"
                | "pragma"
        );
        if safelisted || expose_all || expose_set.contains(&lower) {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

fn get_header_ci<'a>(headers: &'a HashMap<String, String>, target_lower: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(target_lower))
        .map(|(_, v)| v.as_str())
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "",
    }
}
