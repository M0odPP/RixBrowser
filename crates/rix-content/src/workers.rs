//! Dedicated `Worker`, `SharedWorker`, `BroadcastChannel`, and `EventSource` (SSE)
//! Web APIs running isolated full-stack JS VM OS threads with cross-thread
//! structured clone, Transferables (`ArrayBuffer`, `MessagePort`, `SharedArrayBuffer`,
//! `ImageBitmap`, `OffscreenCanvas`), ES Module workers, `ErrorEvent` reporting,
//! and reactive zero-polling `tokio::select!` delivery.
//!
//! Reference: WHATWG HTML §10 (Web Workers), §9.4 (BroadcastChannel), §9.2 (Server-Sent Events).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rix_core::{RixUrl, Size};
use rix_dom::Document;
use rix_ipc::MessagePortId;
use rix_js::{
    EvalError, HeapHandle, ImportMap, JsException, JsObject, JsValue, JsVm, ModuleError,
    ModuleLoader, ModuleSourceProvider, ObjectKind, PropertyAttrs,
};
use rix_net::{FetchCredentials, FetchEngine, FetchMode, FetchRedirect, FetchRequest};
use rix_storage::StorageEngine;
use rix_text::FontDatabase;
use tokio::sync::{mpsc, oneshot};
use url::Url;

use crate::dom_bridge::SharedHost;
use crate::dom_interfaces::define_to_string_tag;
use crate::event_loop::{
    drain_ready_tasks, fire_vsync, install_event_loop_globals, run_microtask_checkpoint,
    EventLoopState, SharedEventLoop,
};
use crate::events::{
    fire_error_event, fire_object_event, fire_object_event_dispatch_only,
    fire_object_event_with_proto, install_object_event_handler_accessor, SharedEvents,
};
use crate::fetch_ws::{deliver_async_net_event, SharedNetBridge};
use crate::frames::{
    attach_transferred_cross_thread_port, deliver_cross_thread_port_message, freeze_js_array,
    SharedFrames,
};
use crate::structured_clone::{
    deserialize_payload, extract_transfer_arg, serialize_with_transfer, CloneContext,
    CrossThreadPortEnd, PortRoute, StructuredClonePayload,
};
use crate::web_apis::{
    arg, bind_ctor_proto, create_dom_exception, define_global, define_host_getter,
    define_host_method, lookup_global_object_url, origin_of_url, parse_url_with_charset,
    SharedWebApis,
};

fn alloc_js_array(vm: &mut JsVm, items: &[JsValue]) -> HeapHandle {
    let arr = vm.alloc_array();
    for &v in items {
        vm.heap.object_push_element(arr, v);
    }
    arr
}

// ---------------------------------------------------------------------------
// Global registries (process-wide SharedWorker & BroadcastChannel hubs)
// ---------------------------------------------------------------------------

static NEXT_GLOBAL_ID: AtomicU64 = AtomicU64::new(1);

fn next_global_id() -> u64 {
    NEXT_GLOBAL_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerType {
    Classic,
    Module,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerCreds {
    Omit,
    SameOrigin,
    Include,
}

impl WorkerCreds {
    fn to_fetch_credentials(self) -> FetchCredentials {
        match self {
            Self::Omit => FetchCredentials::Omit,
            Self::SameOrigin => FetchCredentials::SameOrigin,
            Self::Include => FetchCredentials::Include,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerOptions {
    worker_type: WorkerType,
    credentials: WorkerCreds,
    extended_lifetime: bool,
}

#[derive(Debug)]
enum WorkerCommand {
    PostMessage(StructuredClonePayload),
    Terminate,
}

#[derive(Debug)]
enum SharedWorkerCommand {
    Connect {
        worker_port_end: CrossThreadPortEnd,
        client_error_tx: mpsc::UnboundedSender<AsyncWorkerEvent>,
        shared_worker_id: u64,
    },
    #[allow(dead_code)]
    Terminate,
}

#[derive(Clone)]
struct SharedWorkerHandle {
    cmd_tx: mpsc::UnboundedSender<SharedWorkerCommand>,
    options: WorkerOptions,
}

type SharedWorkerRegistry = HashMap<(String, String, String), SharedWorkerHandle>;
static SHARED_WORKERS: OnceLock<Mutex<SharedWorkerRegistry>> = OnceLock::new();

fn shared_workers_registry() -> &'static Mutex<SharedWorkerRegistry> {
    SHARED_WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone)]
enum BroadcastSink {
    Async(mpsc::UnboundedSender<AsyncWorkerEvent>),
}

impl BroadcastSink {
    fn send_broadcast(&self, channel_id: u64, origin: String, payload_json: String) -> bool {
        match self {
            Self::Async(tx) => tx
                .send(AsyncWorkerEvent::BroadcastMessage {
                    channel_id,
                    origin,
                    payload_json,
                })
                .is_ok(),
        }
    }
}

struct BroadcastSubscriber {
    id: u64,
    sink: BroadcastSink,
}

fn broadcast_to_peers(sender_id: u64, origin: &str, name: &str, payload_json: &str) {
    if let Ok(mut hub) = broadcast_hub().lock() {
        if let Some(subs) = hub.get_mut(&(origin.to_string(), name.to_string())) {
            subs.retain(|sub| {
                if sub.id == sender_id {
                    return true;
                }
                sub.sink
                    .send_broadcast(sub.id, origin.to_string(), payload_json.to_string())
            });
        }
    }
}

type BroadcastHub = HashMap<(String, String), Vec<BroadcastSubscriber>>;
static BROADCAST_HUB: OnceLock<Mutex<BroadcastHub>> = OnceLock::new();

fn broadcast_hub() -> &'static Mutex<BroadcastHub> {
    BROADCAST_HUB.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Reactive async events emitted from Worker threads, SharedWorker threads,
/// cross-thread MessagePorts, BroadcastChannel peers, and streaming `EventSource`
/// (SSE) tasks directly into `ContentRuntime`'s `tokio::select!` loop.
#[derive(Debug)]
pub enum AsyncWorkerEvent {
    DedicatedMessage {
        worker_id: u64,
        payload: StructuredClonePayload,
    },
    DedicatedFetchError {
        worker_id: u64,
    },
    DedicatedRuntimeError {
        worker_id: u64,
        message: String,
        filename: String,
        lineno: u32,
        colno: u32,
    },
    SharedWorkerFetchError {
        shared_worker_id: u64,
    },
    BridgePortMessage {
        port_id: MessagePortId,
        payload: StructuredClonePayload,
    },
    BroadcastMessage {
        channel_id: u64,
        origin: String,
        payload_json: String,
    },
    SseOpen {
        sse_id: u64,
    },
    SseEvent {
        sse_id: u64,
        event_type: String,
        data: String,
        last_event_id: String,
        origin: String,
    },
    SseError {
        sse_id: u64,
        closed: bool,
    },
}

// ---------------------------------------------------------------------------
// Per-runtime WorkerSystem state
// ---------------------------------------------------------------------------

struct DedicatedWorkerEntry {
    js_obj: HeapHandle,
    cmd_tx: mpsc::UnboundedSender<WorkerCommand>,
    terminated: bool,
}

struct SharedWorkerClientEntry {
    js_obj: HeapHandle,
    port_h: HeapHandle,
}

struct BroadcastChannelEntry {
    id: u64,
    js_obj: HeapHandle,
    origin: String,
    name: String,
    closed: bool,
}

struct EventSourceEntry {
    js_obj: HeapHandle,
    url: String,
    with_credentials: bool,
    ready_state: u8,
    close_tx: Option<oneshot::Sender<()>>,
}

#[derive(Default)]
struct WorkerPrototypes {
    worker_proto: Option<HeapHandle>,
    shared_worker_proto: Option<HeapHandle>,
    broadcast_channel_proto: Option<HeapHandle>,
    event_source_proto: Option<HeapHandle>,
}

/// Content-runtime registry of active `Worker`, `SharedWorker`, `BroadcastChannel`,
/// and `EventSource` instances plus the reactive async event channel.
pub struct WorkerSystem {
    async_tx: mpsc::UnboundedSender<AsyncWorkerEvent>,
    pub(crate) async_rx: RefCell<Option<mpsc::UnboundedReceiver<AsyncWorkerEvent>>>,
    workers_by_id: HashMap<u64, DedicatedWorkerEntry>,
    worker_handle_to_id: HashMap<HeapHandle, u64>,
    shared_workers_by_id: HashMap<u64, SharedWorkerClientEntry>,
    shared_worker_handle_to_id: HashMap<HeapHandle, u64>,
    broadcasts_by_id: HashMap<u64, BroadcastChannelEntry>,
    broadcast_handle_to_id: HashMap<HeapHandle, u64>,
    sse_by_id: HashMap<u64, EventSourceEntry>,
    sse_handle_to_id: HashMap<HeapHandle, u64>,
    protos: WorkerPrototypes,
}

/// Shared handle to [`WorkerSystem`].
pub type SharedWorkers = Rc<RefCell<WorkerSystem>>;

impl WorkerSystem {
    #[must_use]
    pub fn new() -> Self {
        let (async_tx, async_rx) = mpsc::unbounded_channel();
        Self {
            async_tx,
            async_rx: RefCell::new(Some(async_rx)),
            workers_by_id: HashMap::new(),
            worker_handle_to_id: HashMap::new(),
            shared_workers_by_id: HashMap::new(),
            shared_worker_handle_to_id: HashMap::new(),
            broadcasts_by_id: HashMap::new(),
            broadcast_handle_to_id: HashMap::new(),
            sse_by_id: HashMap::new(),
            sse_handle_to_id: HashMap::new(),
            protos: WorkerPrototypes::default(),
        }
    }

    pub(crate) fn async_tx_clone(&self) -> mpsc::UnboundedSender<AsyncWorkerEvent> {
        self.async_tx.clone()
    }

    pub(crate) fn is_worker_platform_object(&self, h: HeapHandle) -> bool {
        self.worker_handle_to_id.contains_key(&h)
            || self.shared_worker_handle_to_id.contains_key(&h)
            || self.broadcast_handle_to_id.contains_key(&h)
            || self.sse_handle_to_id.contains_key(&h)
    }

    /// Returns `true` if any dedicated worker, shared worker, or SSE stream is currently live.
    #[must_use]
    pub fn has_active_workers(&self) -> bool {
        self.workers_by_id.values().any(|w| !w.terminated)
            || !self.shared_workers_by_id.is_empty()
            || self.sse_by_id.values().any(|s| s.ready_state != 2)
    }

    /// Collects GC roots for active workers, channels, SSE handles, and prototypes.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.protos.worker_proto,
            self.protos.shared_worker_proto,
            self.protos.broadcast_channel_proto,
            self.protos.event_source_proto,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(p));
        }
        for w in self.workers_by_id.values() {
            if !w.terminated {
                out.push(JsValue::from_object(w.js_obj));
            }
        }
        for sw in self.shared_workers_by_id.values() {
            out.push(JsValue::from_object(sw.js_obj));
            out.push(JsValue::from_object(sw.port_h));
        }
        for bc in self.broadcasts_by_id.values() {
            if !bc.closed {
                out.push(JsValue::from_object(bc.js_obj));
            }
        }
        for sse in self.sse_by_id.values() {
            if sse.ready_state != 2 {
                out.push(JsValue::from_object(sse.js_obj));
            }
        }
    }

    /// Sweeps dead JS wrapper handles after Major GC.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.workers_by_id
            .retain(|_, w| !w.terminated || heap.is_alive(w.js_obj));
        self.worker_handle_to_id.retain(|h, _| heap.is_alive(*h));
        self.shared_workers_by_id
            .retain(|_, sw| heap.is_alive(sw.js_obj));
        self.shared_worker_handle_to_id
            .retain(|h, _| heap.is_alive(*h));
        self.broadcasts_by_id
            .retain(|_, b| !b.closed || heap.is_alive(b.js_obj));
        self.broadcast_handle_to_id.retain(|h, _| heap.is_alive(*h));
        self.sse_by_id
            .retain(|_, s| s.ready_state != 2 || heap.is_alive(s.js_obj));
        self.sse_handle_to_id.retain(|h, _| heap.is_alive(*h));
    }
}

impl Default for WorkerSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WorkerSystem {
    fn drop(&mut self) {
        for w in self.workers_by_id.values_mut() {
            if !w.terminated {
                w.terminated = true;
                let _ = w.cmd_tx.send(WorkerCommand::Terminate);
            }
        }
        for bc in self.broadcasts_by_id.values_mut() {
            if !bc.closed {
                bc.closed = true;
                unregister_broadcast_subscriber(&bc.origin, &bc.name, bc.id);
            }
        }
        for sse in self.sse_by_id.values_mut() {
            if let Some(tx) = sse.close_tx.take() {
                let _ = tx.send(());
            }
        }
    }
}

fn unregister_broadcast_subscriber(origin: &str, name: &str, id: u64) {
    if let Ok(mut hub) = broadcast_hub().lock() {
        let key = (origin.to_string(), name.to_string());
        if let Some(list) = hub.get_mut(&key) {
            list.retain(|s| s.id != id);
            if list.is_empty() {
                hub.remove(&key);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Script resolution, MIME validation, and URL decoding (`blob:`, `data:`, `http(s):`)
// ---------------------------------------------------------------------------

/// WHATWG MIME Sniffing §4.6 JavaScript MIME type essence check.
fn is_javascript_mime(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        essence.as_str(),
        "application/ecmascript"
            | "application/javascript"
            | "application/x-ecmascript"
            | "application/x-javascript"
            | "text/ecmascript"
            | "text/javascript"
            | "text/javascript1.0"
            | "text/javascript1.1"
            | "text/javascript1.2"
            | "text/javascript1.3"
            | "text/javascript1.4"
            | "text/javascript1.5"
            | "text/jscript"
            | "text/livescript"
            | "text/x-ecmascript"
            | "text/x-javascript"
    )
}

fn resolve_script_url(raw: &str, base_href: &str, charset: &str) -> Result<String, String> {
    let base = Url::parse(base_href).unwrap_or_else(|_| {
        Url::parse(crate::web_apis::DEFAULT_DOCUMENT_URL).expect("valid default url")
    });
    parse_url_with_charset(raw, &base, charset)
        .map(|u| u.to_string())
        .ok_or_else(|| format!("invalid script URL '{raw}'"))
}

struct FetchedScript {
    code: String,
    final_url: String,
    content_type: String,
    nosniff: bool,
}

static WORKER_FETCH_RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn worker_fetch_rt() -> &'static tokio::runtime::Runtime {
    WORKER_FETCH_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("worker fetch runtime")
    })
}

#[allow(clippy::too_many_arguments)]
fn fetch_script_sync(
    resolved_url: &str,
    caller_origin: &str,
    referer: &str,
    credentials: WorkerCreds,
    require_same_origin: bool,
    require_js_mime_for_http: bool,
    object_urls_snapshot: &HashMap<String, Vec<u8>>,
    fetch_engine: &Arc<FetchEngine>,
    cookie_header: Option<&str>,
) -> Result<FetchedScript, String> {
    if resolved_url.starts_with("blob:") {
        let lookup = resolved_url.split('#').next().unwrap_or(resolved_url);
        let bytes = lookup_global_object_url(lookup)
            .map(|(b, _)| b)
            .or_else(|| object_urls_snapshot.get(lookup).cloned())
            .ok_or_else(|| format!("unknown blob URL '{resolved_url}'"))?;
        return Ok(FetchedScript {
            code: String::from_utf8_lossy(&bytes).into_owned(),
            final_url: resolved_url.to_string(),
            content_type: "text/javascript".to_string(),
            nosniff: false,
        });
    }
    if let Some(rest) = resolved_url.strip_prefix("data:") {
        let decoded = decode_data_url(rest)?;
        return Ok(FetchedScript {
            code: decoded,
            final_url: resolved_url.to_string(),
            content_type: "text/javascript".to_string(),
            nosniff: false,
        });
    }
    if resolved_url.starts_with("http://") || resolved_url.starts_with("https://") {
        let parsed = Url::parse(resolved_url).map_err(|e| format!("invalid URL: {e}"))?;
        let target_origin = origin_of_url(&parsed);
        let same_origin = caller_origin != "null" && target_origin == caller_origin;
        if require_same_origin && !same_origin {
            return Err(format!(
                "cross-origin worker script '{resolved_url}' blocked"
            ));
        }
        let mut headers = HashMap::new();
        if !referer.is_empty() && !referer.starts_with("data:") && !referer.starts_with("blob:") {
            headers.insert("referer".to_string(), referer.to_string());
        }
        let send_cookies = match credentials {
            WorkerCreds::Omit => false,
            WorkerCreds::SameOrigin => same_origin,
            WorkerCreds::Include => true,
        };
        if send_cookies {
            if let Some(ck) = cookie_header {
                if !ck.is_empty() {
                    headers.insert("cookie".to_string(), ck.to_string());
                }
            }
        }
        let req = FetchRequest {
            url: resolved_url.to_string(),
            method: "GET".to_string(),
            headers,
            body: Vec::new(),
            mode: if require_same_origin {
                FetchMode::SameOrigin
            } else {
                FetchMode::Cors
            },
            credentials: credentials.to_fetch_credentials(),
            cache: rix_net::FetchCache::Default,
            redirect: FetchRedirect::Follow,
            origin: caller_origin.to_string(),
            abort_signal: None,
        };
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let fe_c = Arc::clone(fetch_engine);
        worker_fetch_rt().spawn(async move {
            let _ = tx.send(fe_c.fetch(req).await);
        });
        let resp = rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|e| format!("worker script fetch timed out for '{resolved_url}': {e}"))?
            .map_err(|e| format!("failed to fetch script '{resolved_url}': {e}"))?;
        if !(200..300).contains(&resp.status) {
            return Err(format!(
                "script '{resolved_url}' returned HTTP status {}",
                resp.status
            ));
        }
        let ct = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let nosniff = resp.headers.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("x-content-type-options")
                && v.trim().eq_ignore_ascii_case("nosniff")
        });
        if require_js_mime_for_http && !is_javascript_mime(&ct) {
            return Err(format!(
                "invalid JavaScript MIME type '{ct}' for '{resolved_url}'"
            ));
        }
        return Ok(FetchedScript {
            code: String::from_utf8_lossy(&resp.body).into_owned(),
            final_url: if resp.url.is_empty() {
                resolved_url.to_string()
            } else {
                resp.url
            },
            content_type: ct,
            nosniff,
        });
    }
    Err(format!("unsupported worker script URL '{resolved_url}'"))
}

fn decode_data_url(rest: &str) -> Result<String, String> {
    let Some((meta, payload)) = rest.split_once(',') else {
        return Err("malformed data: URL".to_string());
    };
    let is_base64 = meta
        .split(';')
        .any(|part| part.trim().eq_ignore_ascii_case("base64"));
    if is_base64 {
        let bytes = decode_base64(payload)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        Ok(percent_decode(payload))
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    let mut vals = Vec::with_capacity(input.len());
    for b in input.bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            vals.push(64u8);
            continue;
        }
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return Err("invalid base64 character in data: URL".to_string()),
        };
        vals.push(v);
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    for chunk in vals.chunks(4) {
        if chunk.len() < 2 {
            break;
        }
        let a = chunk[0];
        let b = chunk[1];
        if a == 64 || b == 64 {
            break;
        }
        out.push((a << 2) | (b >> 4));
        if chunk.len() > 2 && chunk[2] != 64 {
            let c = chunk[2];
            out.push((b << 4) | (c >> 2));
            if chunk.len() > 3 && chunk[3] != 64 {
                let d = chunk[3];
                out.push((c << 6) | d);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Worker ES Module provider (`type: "module"` + dynamic `import()`)
// ---------------------------------------------------------------------------

struct WorkerModuleProvider {
    entry_url: String,
    entry_code: String,
    origin: String,
    referer: String,
    credentials: WorkerCreds,
    object_urls: Arc<HashMap<String, Vec<u8>>>,
    fetch_engine: Arc<FetchEngine>,
    cookie_header: String,
}

impl ModuleSourceProvider for WorkerModuleProvider {
    fn load_source(&self, url: &str) -> Result<String, String> {
        if url == self.entry_url && !self.entry_code.is_empty() {
            return Ok(self.entry_code.clone());
        }
        let fetched = fetch_script_sync(
            url,
            &self.origin,
            &self.referer,
            self.credentials,
            false,
            true,
            &self.object_urls,
            &self.fetch_engine,
            Some(&self.cookie_header),
        )?;
        Ok(fetched.code)
    }
}

// ---------------------------------------------------------------------------
// Worker Error Reporting helper (HTML §8.1.7.3)
// ---------------------------------------------------------------------------

fn format_exception_message(vm: &mut JsVm, ex: &JsException) -> String {
    if let Some(eh) = ex.value.as_object() {
        let kn = vm.heap.intern_key("name");
        let km = vm.heap.intern_key("message");
        let name = vm
            .get_property_by_key(eh, kn)
            .ok()
            .filter(|v| !v.is_undefined())
            .map(|v| vm.value_to_rust_string(v))
            .unwrap_or_default();
        let msg = vm
            .get_property_by_key(eh, km)
            .ok()
            .filter(|v| !v.is_undefined())
            .map(|v| vm.value_to_rust_string(v))
            .unwrap_or_default();
        if !name.is_empty() && !msg.is_empty() {
            return format!("Uncaught {name}: {msg}");
        }
        if !msg.is_empty() {
            return format!("Uncaught {msg}");
        }
        if !name.is_empty() {
            return format!("Uncaught {name}");
        }
    }
    if !ex.message.is_empty() {
        return format!("Uncaught {}", ex.message);
    }
    format!("Uncaught {}", vm.value_to_rust_string(ex.value))
}

#[allow(clippy::too_many_arguments)]
fn report_worker_error(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    error_val: JsValue,
    message: &str,
    filename: &str,
    lineno: u32,
    colno: u32,
    dedicated_parent: Option<(u64, &mpsc::UnboundedSender<AsyncWorkerEvent>)>,
    in_onerror_guard: &Rc<RefCell<bool>>,
) {
    if *in_onerror_guard.borrow() {
        // Already inside an `onerror` handler: forward directly to parent without re-entering.
        if let Some((worker_id, ptx)) = dedicated_parent {
            let _ = ptx.send(AsyncWorkerEvent::DedicatedRuntimeError {
                worker_id,
                message: message.to_string(),
                filename: filename.to_string(),
                lineno,
                colno,
            });
        }
        return;
    }

    *in_onerror_guard.borrow_mut() = true;
    let g = vm.global;
    let (prevented, handler_ex) = fire_error_event(
        vm, host, events, event_loop, g, message, filename, lineno, colno, error_val, true,
    );
    *in_onerror_guard.borrow_mut() = false;

    if let Some((worker_id, ptx)) = dedicated_parent {
        if !prevented {
            let _ = ptx.send(AsyncWorkerEvent::DedicatedRuntimeError {
                worker_id,
                message: message.to_string(),
                filename: filename.to_string(),
                lineno,
                colno,
            });
        }
        if let Some(hex) = handler_ex {
            let hmsg = format_exception_message(vm, &hex);
            let _ = ptx.send(AsyncWorkerEvent::DedicatedRuntimeError {
                worker_id,
                message: hmsg,
                filename: filename.to_string(),
                lineno,
                colno,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// WorkerGlobalScope / WorkerLocation / WorkerNavigator installation
// ---------------------------------------------------------------------------

fn install_worker_location(vm: &mut JsVm, script_url: &str, effective_origin: &str) {
    let loc_proto = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(loc_proto).unwrap().is_prototype = true;
    define_to_string_tag(vm, loc_proto, "WorkerLocation");

    let parsed = Url::parse(script_url).ok();
    let href_s = script_url.to_string();
    let origin_s = if script_url.starts_with("data:") {
        "null".to_string()
    } else if let Some(rest) = script_url.strip_prefix("blob:") {
        Url::parse(rest)
            .map(|u| origin_of_url(&u))
            .unwrap_or_else(|_| effective_origin.to_string())
    } else {
        parsed
            .as_ref()
            .map(origin_of_url)
            .unwrap_or_else(|| effective_origin.to_string())
    };
    let protocol_s = parsed
        .as_ref()
        .map_or_else(|| "http:".to_string(), |u| format!("{}:", u.scheme()));
    let hostname_s = parsed
        .as_ref()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let port_s = parsed
        .as_ref()
        .and_then(Url::port)
        .map(|p| p.to_string())
        .unwrap_or_default();
    let host_s = if port_s.is_empty() {
        hostname_s.clone()
    } else {
        format!("{hostname_s}:{port_s}")
    };
    let pathname_s = parsed
        .as_ref()
        .map_or_else(|| "/".to_string(), |u| u.path().to_string());
    // WHATWG URL: empty query `?` or empty fragment `#` returns `""` (not `"?"`/`"#"`).
    let search_s = parsed
        .as_ref()
        .and_then(Url::query)
        .filter(|q| !q.is_empty())
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let hash_s = parsed
        .as_ref()
        .and_then(Url::fragment)
        .filter(|f| !f.is_empty())
        .map(|f| format!("#{f}"))
        .unwrap_or_default();

    for (prop, val_s) in [
        ("href", href_s.clone()),
        ("origin", origin_s),
        ("protocol", protocol_s),
        ("host", host_s),
        ("hostname", hostname_s),
        ("port", port_s),
        ("pathname", pathname_s),
        ("search", search_s),
        ("hash", hash_s),
    ] {
        define_host_getter(vm, loc_proto, prop, move |vm, _, _| {
            Ok(JsValue::from_string(vm.heap.alloc_string(&val_s)))
        });
    }
    let href_for_str = href_s;
    define_host_method(vm, loc_proto, "toString", move |vm, _, _| {
        Ok(JsValue::from_string(vm.heap.alloc_string(&href_for_str)))
    });

    let loc_ctor = vm.alloc_host_fn("WorkerLocation", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: WorkerLocation"))
    });
    bind_ctor_proto(vm, loc_ctor, loc_proto);
    define_global(vm, "WorkerLocation", JsValue::from_object(loc_ctor));

    let loc_obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(loc_proto)));
    let k_loc = vm.heap.intern_key("location");
    let g = vm.global;
    let getter = vm.alloc_host_fn("get location", move |_, _, _| {
        Ok(JsValue::from_object(loc_obj))
    });
    vm.heap.object_define_accessor(
        g,
        k_loc,
        Some(getter),
        None,
        PropertyAttrs::ENUMERABLE | PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
}

fn install_worker_navigator(vm: &mut JsVm) {
    let nav_proto = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(nav_proto).unwrap().is_prototype = true;
    define_to_string_tag(vm, nav_proto, "WorkerNavigator");

    let ua_str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) RixBrowser/0.2.0 Safari/537.36";
    let app_ver = "5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) RixBrowser/0.2.0 Safari/537.36";
    let cores = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get) as f64;

    for (prop, val) in [
        ("appCodeName", "Mozilla"),
        ("appName", "Netscape"),
        ("appVersion", app_ver),
        ("platform", "Linux x86_64"),
        ("product", "Gecko"),
        ("userAgent", ua_str),
        ("language", "en-US"),
    ] {
        let v = val.to_string();
        define_host_getter(vm, nav_proto, prop, move |vm, _, _| {
            Ok(JsValue::from_string(vm.heap.alloc_string(&v)))
        });
    }
    define_host_getter(vm, nav_proto, "onLine", |_, _, _| Ok(JsValue::TRUE));
    define_host_getter(vm, nav_proto, "hardwareConcurrency", move |_, _, _| {
        Ok(JsValue::from_number(cores))
    });
    let langs_arr = vm.alloc_array();
    let en_us = JsValue::from_string(vm.heap.alloc_string("en-US"));
    vm.heap.object_push_element(langs_arr, en_us);
    freeze_js_array(vm, langs_arr);
    define_host_getter(vm, nav_proto, "languages", move |_, _, _| {
        Ok(JsValue::from_object(langs_arr))
    });

    let nav_ctor = vm.alloc_host_fn("WorkerNavigator", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: WorkerNavigator"))
    });
    bind_ctor_proto(vm, nav_ctor, nav_proto);
    define_global(vm, "WorkerNavigator", JsValue::from_object(nav_ctor));

    let nav_obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(nav_proto)));
    define_global(vm, "navigator", JsValue::from_object(nav_obj));
}

// ---------------------------------------------------------------------------
// Unified Full-Stack Worker Thread Runtime (Dedicated & Shared)
// ---------------------------------------------------------------------------

enum WorkerRuntimeMode {
    Dedicated {
        worker_id: u64,
        parent_tx: mpsc::UnboundedSender<AsyncWorkerEvent>,
        cmd_rx: mpsc::UnboundedReceiver<WorkerCommand>,
    },
    Shared {
        cmd_rx: mpsc::UnboundedReceiver<SharedWorkerCommand>,
    },
}

#[allow(clippy::too_many_arguments)]
fn run_worker_thread(
    script_url: String,
    caller_origin: String,
    referer: String,
    worker_name: String,
    options: WorkerOptions,
    cross_origin_isolated: bool,
    object_urls_snapshot: Arc<HashMap<String, Vec<u8>>>,
    fetch_engine: Arc<FetchEngine>,
    cookie_header: String,
    mode: WorkerRuntimeMode,
) {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        if let WorkerRuntimeMode::Dedicated {
            worker_id,
            parent_tx,
            ..
        } = mode
        {
            let _ = parent_tx.send(AsyncWorkerEvent::DedicatedFetchError { worker_id });
        }
        return;
    };
    let rt = Rc::new(rt);
    let is_dedicated = matches!(mode, WorkerRuntimeMode::Dedicated { .. });

    // 1. Fetch top-level worker script (enforcing same-origin for http(s), JS MIME, and redirects).
    let fetched = match fetch_script_sync(
        &script_url,
        &caller_origin,
        &referer,
        options.credentials,
        true,
        true,
        &object_urls_snapshot,
        &fetch_engine,
        Some(&cookie_header),
    ) {
        Ok(f) => f,
        Err(_) => {
            match mode {
                WorkerRuntimeMode::Dedicated {
                    worker_id,
                    parent_tx,
                    ..
                } => {
                    let _ = parent_tx.send(AsyncWorkerEvent::DedicatedFetchError { worker_id });
                }
                WorkerRuntimeMode::Shared { mut cmd_rx } => {
                    while let Ok(cmd) = cmd_rx.try_recv() {
                        if let SharedWorkerCommand::Connect {
                            client_error_tx,
                            shared_worker_id,
                            ..
                        } = cmd
                        {
                            let _ =
                                client_error_tx.send(AsyncWorkerEvent::SharedWorkerFetchError {
                                    shared_worker_id,
                                });
                        }
                    }
                }
            }
            return;
        }
    };

    let final_url = fetched.final_url;
    let code = fetched.code;
    let effective_origin = if final_url.starts_with("data:") {
        "null".to_string()
    } else if let Some(rest) = final_url.strip_prefix("blob:") {
        Url::parse(rest)
            .map(|u| origin_of_url(&u))
            .unwrap_or_else(|_| caller_origin.clone())
    } else {
        Url::parse(&final_url)
            .map(|u| origin_of_url(&u))
            .unwrap_or_else(|_| caller_origin.clone())
    };

    // 2. Initialize full `rix-content` Web Platform stack on this isolated worker thread.
    let mut vm = JsVm::new();
    let _jit = rix_jit::TieredJit::attach_to(&mut vm);
    let _wasm = rix_jit::install_webassembly(&mut vm);
    vm.max_loop_iterations = Some(5_000_000);

    let event_loop = Rc::new(RefCell::new(EventLoopState::new()));
    install_event_loop_globals(&mut vm, &event_loop);
    let doc = Rc::new(RefCell::new(Document::new()));
    let font_db = Arc::new(FontDatabase::new());
    let host = crate::dom_bridge::install_dom_bridge(&mut vm, doc, Size::new(1.0, 1.0), font_db);
    let events = crate::events::install_event_system(&mut vm, &host, &event_loop);
    let web_apis = crate::web_apis::install_web_apis(
        &mut vm,
        &host,
        &events,
        &event_loop,
        &final_url,
        StorageEngine::in_memory().expect("worker in-memory storage"),
    );
    web_apis.borrow_mut().cross_origin_isolated = cross_origin_isolated;
    let streams = crate::streams::install_streams(&mut vm, &web_apis);
    let net = crate::fetch_ws::install_fetch_and_websocket(
        &mut vm,
        &host,
        &events,
        &event_loop,
        &web_apis,
        &streams,
        Some(Arc::clone(&fetch_engine)),
    );
    let canvas2d = crate::canvas2d::install_canvas2d(&mut vm, &host, &web_apis);
    let frames = crate::frames::install_frame_and_messaging_apis(
        &mut vm,
        &host,
        &events,
        &event_loop,
        &web_apis,
        &net,
    );
    {
        let mut fs = frames.borrow_mut();
        fs.canvas2d = Some(Rc::clone(&canvas2d));
        fs.net = Some(Rc::clone(&net));
    }
    let workers = install_worker_and_sse_apis(
        &mut vm,
        &host,
        &events,
        &event_loop,
        &web_apis,
        &net,
        &frames,
    );
    frames.borrow_mut().workers = Some(Rc::clone(&workers));
    let indexeddb =
        crate::indexeddb::install_indexeddb(&mut vm, &host, &events, &event_loop, &web_apis);

    // Register host GC root collectors so long-running workers never sweep live host wrappers.
    {
        let el_r = Rc::clone(&event_loop);
        let ev_r = Rc::clone(&events);
        let wa_r = Rc::clone(&web_apis);
        let st_r = Rc::clone(&streams);
        let nb_r = Rc::clone(&net);
        let c2_r = Rc::clone(&canvas2d);
        let fr_r = Rc::clone(&frames);
        let ws_r = Rc::clone(&workers);
        let idb_r = Rc::clone(&indexeddb);
        let h_r = Rc::clone(&host);
        vm.add_host_root_collector(move |roots| {
            if let Ok(g) = el_r.try_borrow() {
                g.collect_roots(roots);
            }
            if let Ok(g) = ev_r.try_borrow() {
                g.collect_roots(roots);
            }
            if let Ok(g) = wa_r.try_borrow() {
                g.collect_roots(roots);
            }
            if let Ok(g) = st_r.try_borrow() {
                g.collect_roots(roots);
            }
            if let Ok(g) = nb_r.try_borrow() {
                g.collect_roots(roots);
            }
            if let Ok(g) = c2_r.try_borrow() {
                g.collect_roots(roots);
            }
            if let Ok(g) = fr_r.try_borrow() {
                g.collect_roots(roots);
            }
            if let Ok(g) = ws_r.try_borrow() {
                g.collect_roots(roots);
            }
            if let Ok(g) = idb_r.try_borrow() {
                g.collect_roots(roots);
            }
            if let Ok(g) = h_r.try_borrow() {
                g.collect_roots(roots);
            }
        });
    }

    // 3. Remove Window-only own properties from `vm.global` so they neither leak into
    //    `WorkerGlobalScope` nor shadow `WorkerGlobalScope.prototype` / `DedicatedWorkerGlobalScope.prototype`
    //    methods (`postMessage`, `name`, `close`, `location`, `navigator`, `onmessage`, `onerror`, `onconnect`).
    for win_only in [
        "window",
        "document",
        "parent",
        "top",
        "frames",
        "opener",
        "frameElement",
        "postMessage",
        "name",
        "close",
        "closed",
        "open",
        "print",
        "stop",
        "location",
        "navigator",
        "history",
        "localStorage",
        "sessionStorage",
        "getComputedStyle",
        "getSelection",
        "alert",
        "confirm",
        "prompt",
        "DOMParser",
        "XMLSerializer",
        "XPathEvaluator",
        "innerWidth",
        "innerHeight",
        "outerWidth",
        "outerHeight",
        "pageXOffset",
        "pageYOffset",
        "scrollX",
        "scrollY",
        "screen",
        "screenX",
        "screenY",
        "screenLeft",
        "screenTop",
        "length",
        "status",
        "defaultStatus",
        "event",
        "Image",
        "Option",
        "Audio",
        "onerror",
        "onmessage",
        "onmessageerror",
        "onconnect",
        "crossOriginIsolated",
    ] {
        let k = vm.heap.intern_key(win_only);
        let g = vm.global;
        let _ = vm.delete_property_by_key(g, k);
    }

    // 4. Install WorkerGlobalScope / DedicatedWorkerGlobalScope / SharedWorkerGlobalScope hierarchy.
    let etp = host.borrow().prototypes.event_target_proto;
    let wgs_proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(etp)));
    vm.heap.get_object_mut(wgs_proto).unwrap().is_prototype = true;
    define_to_string_tag(&mut vm, wgs_proto, "WorkerGlobalScope");
    let wgs_ctor = vm.alloc_host_fn("WorkerGlobalScope", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: WorkerGlobalScope"))
    });
    bind_ctor_proto(&mut vm, wgs_ctor, wgs_proto);
    define_global(&mut vm, "WorkerGlobalScope", JsValue::from_object(wgs_ctor));

    let concrete_proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(wgs_proto)));
    vm.heap.get_object_mut(concrete_proto).unwrap().is_prototype = true;
    if is_dedicated {
        define_to_string_tag(&mut vm, concrete_proto, "DedicatedWorkerGlobalScope");
        let dw_ctor = vm.alloc_host_fn("DedicatedWorkerGlobalScope", |vm, _, _| {
            Err(vm.type_error("Illegal constructor: DedicatedWorkerGlobalScope"))
        });
        bind_ctor_proto(&mut vm, dw_ctor, concrete_proto);
        define_global(
            &mut vm,
            "DedicatedWorkerGlobalScope",
            JsValue::from_object(dw_ctor),
        );
    } else {
        define_to_string_tag(&mut vm, concrete_proto, "SharedWorkerGlobalScope");
        let sw_ctor = vm.alloc_host_fn("SharedWorkerGlobalScope", |vm, _, _| {
            Err(vm.type_error("Illegal constructor: SharedWorkerGlobalScope"))
        });
        bind_ctor_proto(&mut vm, sw_ctor, concrete_proto);
        define_global(
            &mut vm,
            "SharedWorkerGlobalScope",
            JsValue::from_object(sw_ctor),
        );
    }
    let global_h = vm.global;
    vm.heap
        .object_set_prototype(global_h, Some(concrete_proto), &mut vm.shapes);

    define_global(&mut vm, "self", JsValue::from_object(global_h));
    define_global(
        &mut vm,
        "crossOriginIsolated",
        JsValue::from_bool(cross_origin_isolated),
    );
    // WorkerGlobalScope / DedicatedWorkerGlobalScope / SharedWorkerGlobalScope EventHandler IDL slots
    {
        let k_oe = vm.heap.intern_key("onerror");
        vm.heap.object_define_data(
            wgs_proto,
            k_oe,
            JsValue::NULL,
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        if is_dedicated {
            for attr in ["onmessage", "onmessageerror"] {
                let k = vm.heap.intern_key(attr);
                vm.heap.object_define_data(
                    concrete_proto,
                    k,
                    JsValue::NULL,
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
            }
        } else {
            let k_oc = vm.heap.intern_key("onconnect");
            vm.heap.object_define_data(
                concrete_proto,
                k_oc,
                JsValue::NULL,
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
        }
    }
    {
        let wn = worker_name.clone();
        let k_name = vm.heap.intern_key("name");
        let get_fn = vm.alloc_host_fn("get name", move |vm, _, _| {
            Ok(JsValue::from_string(vm.heap.alloc_string(&wn)))
        });
        // HTML §10.2.2 `[Replaceable]` `name` attribute: assigning `name = x` defines an own data property.
        let set_fn = vm.alloc_host_fn("set name", move |vm, this_val, args| {
            let target = this_val.as_object().unwrap_or(vm.global);
            let k = vm.heap.intern_key("name");
            let v = arg(args, 0);
            vm.heap
                .object_define_data(target, k, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
            Ok(JsValue::UNDEFINED)
        });
        vm.heap.object_define_accessor(
            global_h,
            k_name,
            Some(get_fn),
            Some(set_fn),
            PropertyAttrs::ENUMERABLE | PropertyAttrs::CONFIGURABLE,
            &mut vm.shapes,
        );
    }
    // Opaque-origin (`data:` URL) workers must reject IndexedDB access with `SecurityError` per W3C IDB §4.1.
    if effective_origin == "null" {
        let k_idb = vm.heap.intern_key("indexedDB");
        if let Ok(idb_val) = vm.get_property_by_key(global_h, k_idb) {
            if let Some(idb_h) = idb_val.as_object() {
                for m in ["open", "deleteDatabase", "databases"] {
                    define_host_method(&mut vm, idb_h, m, |vm, _, _| {
                        Err(create_dom_exception(
                            vm,
                            "SecurityError",
                            "IndexedDB access is denied in opaque-origin worker",
                        ))
                    });
                }
            }
        }
    }
    install_worker_location(&mut vm, &final_url, &effective_origin);
    install_worker_navigator(&mut vm);

    let closed_flag = Rc::new(RefCell::new(false));
    {
        let cf = Rc::clone(&closed_flag);
        define_host_method(&mut vm, concrete_proto, "close", move |_, _, _| {
            *cf.borrow_mut() = true;
            Ok(JsValue::UNDEFINED)
        });
    }

    // `self.importScripts(...urls)` (disallowed in module workers per HTML §10.2.5.1)
    {
        let base_href = final_url.clone();
        let origin_owned = effective_origin.clone();
        let blobs = Arc::clone(&object_urls_snapshot);
        let fe = Arc::clone(&fetch_engine);
        let ck = cookie_header.clone();
        let is_mod = options.worker_type == WorkerType::Module;
        let creds = options.credentials;
        define_host_method(&mut vm, wgs_proto, "importScripts", move |vm, _, args| {
            if is_mod {
                return Err(vm.type_error(
                    "Module scripts don't support importScripts(). Use static or dynamic import instead.",
                ));
            }
            let mut resolved_urls = Vec::with_capacity(args.len());
            for &val in args {
                let raw = vm.to_js_string(val)?;
                let resolved = resolve_script_url(&raw, &base_href, "utf-8")
                    .map_err(|m| create_dom_exception(vm, "SyntaxError", &m))?;
                resolved_urls.push(resolved);
            }
            for resolved in resolved_urls {
                let fetched = fetch_script_sync(
                    &resolved,
                    &origin_owned,
                    &base_href,
                    creds,
                    false,
                    false,
                    &blobs,
                    &fe,
                    Some(&ck),
                )
                .map_err(|m| create_dom_exception(vm, "NetworkError", &m))?;
                if fetched.nosniff && !is_javascript_mime(&fetched.content_type) {
                    return Err(create_dom_exception(
                        vm,
                        "NetworkError",
                        &format!(
                            "Blocked importScripts due to nosniff MIME type '{}'",
                            fetched.content_type
                        ),
                    ));
                }
                vm.eval_script(&fetched.code).map_err(|e| match e {
                    EvalError::Runtime(ex) => ex,
                    other => vm.make_error("SyntaxError", other.to_string()),
                })?;
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // Dedicated worker `self.postMessage(message, transferOrOptions?)`
    if let WorkerRuntimeMode::Dedicated {
        worker_id,
        ref parent_tx,
        ..
    } = mode
    {
        let ptx = parent_tx.clone();
        let h_pm = Rc::clone(&host);
        let ev_pm = Rc::clone(&events);
        let wa_pm = Rc::clone(&web_apis);
        let nb_pm = Rc::clone(&net);
        let c2_pm = Rc::clone(&canvas2d);
        let fr_pm = Rc::clone(&frames);
        let ws_pm = Rc::clone(&workers);
        define_host_method(
            &mut vm,
            concrete_proto,
            "postMessage",
            move |vm, _, args| {
                if args.is_empty() {
                    return Err(vm.type_error("postMessage requires at least 1 argument"));
                }
                let transfer_val = extract_transfer_arg(vm, arg(args, 1))?;
                let ctx = CloneContext {
                    host: &h_pm,
                    events: &ev_pm,
                    web_apis: &wa_pm,
                    net: &nb_pm,
                    canvas2d: &c2_pm,
                    frames: &fr_pm,
                    workers: &ws_pm,
                };
                let payload = serialize_with_transfer(vm, &ctx, arg(args, 0), transfer_val, &[])?;
                let _ = ptx.send(AsyncWorkerEvent::DedicatedMessage { worker_id, payload });
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // Install uncaught error reporting hooks on event_loop + events!
    let in_onerror_guard = Rc::new(RefCell::new(false));
    let ded_parent_info: Option<(u64, mpsc::UnboundedSender<AsyncWorkerEvent>)> = match &mode {
        WorkerRuntimeMode::Dedicated {
            worker_id,
            parent_tx,
            ..
        } => Some((*worker_id, parent_tx.clone())),
        WorkerRuntimeMode::Shared { .. } => None,
    };
    {
        let h_e = Rc::clone(&host);
        let ev_e = Rc::clone(&events);
        let el_e = Rc::clone(&event_loop);
        let fn_e = final_url.clone();
        let dpi_e = ded_parent_info.clone();
        let guard_e = Rc::clone(&in_onerror_guard);
        let hook: crate::event_loop::UncaughtErrorHook = Rc::new(move |vm, ex| {
            let msg = format_exception_message(vm, &ex);
            let parent_ref = dpi_e.as_ref().map(|(id, tx)| (*id, tx));
            report_worker_error(
                vm, &h_e, &ev_e, &el_e, ex.value, &msg, &fn_e, 1, 1, parent_ref, &guard_e,
            );
        });
        event_loop.borrow_mut().on_uncaught_error = Some(Rc::clone(&hook));
        events.borrow_mut().on_uncaught_error = Some(hook);
    }

    // 5. Install ES Module loader (supports both `type: "module"` workers AND dynamic `import()`!).
    let mod_provider = WorkerModuleProvider {
        entry_url: final_url.clone(),
        entry_code: if options.worker_type == WorkerType::Module {
            code.clone()
        } else {
            String::new()
        },
        origin: effective_origin.clone(),
        referer: final_url.clone(),
        credentials: options.credentials,
        object_urls: Arc::clone(&object_urls_snapshot),
        fetch_engine: Arc::clone(&fetch_engine),
        cookie_header: cookie_header.clone(),
    };
    let import_map = ImportMap {
        base_url: final_url.clone(),
        ..ImportMap::default()
    };
    let mod_loader = Rc::new(RefCell::new(ModuleLoader::new(
        Box::new(mod_provider),
        import_map,
    )));
    ModuleLoader::install_on_vm(&mod_loader, &mut vm);

    // 6. Evaluate top-level worker script (Module or Classic).
    let mut mode = mode;
    if options.worker_type == WorkerType::Module {
        match ModuleLoader::load_and_evaluate(&mod_loader, &mut vm, &final_url) {
            Ok(eval_p) => {
                run_microtask_checkpoint(&event_loop, &mut vm);
                // If module evaluation rejected synchronously/microtask-wise, report runtime error.
                let rejected_reason = if let Some(ObjectKind::Promise(po)) =
                    vm.heap.get_object(eval_p).map(|o| &o.kind)
                {
                    if let rix_js::heap::PromiseState::Rejected(reason) = po.state {
                        Some(reason)
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(reason) = rejected_reason {
                    let ex = JsException {
                        value: reason,
                        message: vm.value_to_rust_string(reason),
                    };
                    let msg = format_exception_message(&mut vm, &ex);
                    let parent_ref = ded_parent_info.as_ref().map(|(id, tx)| (*id, tx));
                    report_worker_error(
                        &mut vm,
                        &host,
                        &events,
                        &event_loop,
                        reason,
                        &msg,
                        &final_url,
                        1,
                        1,
                        parent_ref,
                        &in_onerror_guard,
                    );
                }
            }
            Err(
                ModuleError::Load { .. } | ModuleError::Compile { .. } | ModuleError::Link { .. },
            )
            | Err(_) => {
                match &mut mode {
                    WorkerRuntimeMode::Dedicated {
                        worker_id,
                        parent_tx,
                        ..
                    } => {
                        let _ = parent_tx.send(AsyncWorkerEvent::DedicatedFetchError {
                            worker_id: *worker_id,
                        });
                    }
                    WorkerRuntimeMode::Shared { cmd_rx } => {
                        while let Ok(cmd) = cmd_rx.try_recv() {
                            if let SharedWorkerCommand::Connect {
                                client_error_tx,
                                shared_worker_id,
                                ..
                            } = cmd
                            {
                                let _ = client_error_tx.send(
                                    AsyncWorkerEvent::SharedWorkerFetchError { shared_worker_id },
                                );
                            }
                        }
                    }
                }
                return;
            }
        }
    } else if let Err(err) = vm.eval_script(&code) {
        match err {
            EvalError::Runtime(ex) => {
                let msg = format_exception_message(&mut vm, &ex);
                let parent_ref = ded_parent_info.as_ref().map(|(id, tx)| (*id, tx));
                report_worker_error(
                    &mut vm,
                    &host,
                    &events,
                    &event_loop,
                    ex.value,
                    &msg,
                    &final_url,
                    1,
                    1,
                    parent_ref,
                    &in_onerror_guard,
                );
            }
            _ => {
                // WHATWG HTML §10.2.5: top-level script parse/compile failure dispatches a plain
                // `Event("error")` at the `Worker` / `SharedWorker` (not an `ErrorEvent`).
                match &mut mode {
                    WorkerRuntimeMode::Dedicated {
                        worker_id,
                        parent_tx,
                        ..
                    } => {
                        let _ = parent_tx.send(AsyncWorkerEvent::DedicatedFetchError {
                            worker_id: *worker_id,
                        });
                    }
                    WorkerRuntimeMode::Shared { cmd_rx } => {
                        while let Ok(cmd) = cmd_rx.try_recv() {
                            if let SharedWorkerCommand::Connect {
                                client_error_tx,
                                shared_worker_id,
                                ..
                            } = cmd
                            {
                                let _ = client_error_tx.send(
                                    AsyncWorkerEvent::SharedWorkerFetchError { shared_worker_id },
                                );
                            }
                        }
                    }
                }
                return;
            }
        }
    }
    run_microtask_checkpoint(&event_loop, &mut vm);
    drain_ready_tasks(&event_loop, &mut vm);

    // 7. Take async receivers and enter zero-polling `tokio::select!` event loop.
    let Some(mut net_rx) = net.borrow().async_rx.borrow_mut().take() else {
        return;
    };
    let Some(mut worker_rx) = workers.borrow().async_rx.borrow_mut().take() else {
        return;
    };
    let msg_proto = frames
        .borrow()
        .message_event_proto
        .or_else(|| events.borrow().event_proto)
        .unwrap_or(vm.object_proto);
    let rt_handle = Rc::clone(&rt);

    rt_handle.block_on(async move {
        while !*closed_flag.borrow() {
            drain_ready_tasks(&event_loop, &mut vm);
            while let Ok(nev) = net_rx.try_recv() {
                deliver_async_net_event(
                    &mut vm,
                    &host,
                    &events,
                    &event_loop,
                    &web_apis,
                    &streams,
                    &net,
                    nev,
                );
                drain_ready_tasks(&event_loop, &mut vm);
            }
            while let Ok(wev) = worker_rx.try_recv() {
                deliver_async_worker_event(
                    &mut vm,
                    &host,
                    &events,
                    &event_loop,
                    &web_apis,
                    &frames,
                    &workers,
                    wev,
                );
                drain_ready_tasks(&event_loop, &mut vm);
            }
            if *closed_flag.borrow() {
                break;
            }

            let next_timer = event_loop.borrow_mut().time_until_next_timer();
            let raf_due = if event_loop.borrow().has_pending_raf() {
                Some(Duration::from_millis(16))
            } else {
                None
            };
            let sleep_dur = match (next_timer, raf_due) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let deadline = sleep_dur.map(|d| tokio::time::Instant::now() + d);

            match &mut mode {
                WorkerRuntimeMode::Dedicated { cmd_rx, .. } => {
                    enum DedWake {
                        Cmd(Option<WorkerCommand>),
                        Net(Option<crate::fetch_ws::AsyncNetEvent>),
                        Worker(Option<AsyncWorkerEvent>),
                        Timer,
                    }
                    let wake = match deadline {
                        Some(dl) => tokio::select! {
                            biased;
                            c = cmd_rx.recv() => DedWake::Cmd(c),
                            n = net_rx.recv() => DedWake::Net(n),
                            w = worker_rx.recv() => DedWake::Worker(w),
                            () = tokio::time::sleep_until(dl) => DedWake::Timer,
                        },
                        None => tokio::select! {
                            biased;
                            c = cmd_rx.recv() => DedWake::Cmd(c),
                            n = net_rx.recv() => DedWake::Net(n),
                            w = worker_rx.recv() => DedWake::Worker(w),
                        },
                    };
                    match wake {
                        DedWake::Cmd(Some(WorkerCommand::PostMessage(payload))) => {
                            let clone_ctx = CloneContext {
                                host: &host,
                                events: &events,
                                web_apis: &web_apis,
                                net: &net,
                                canvas2d: &canvas2d,
                                frames: &frames,
                                workers: &workers,
                            };
                            let (data, ports) = deserialize_payload(&mut vm, &clone_ctx, payload);
                            let ports_arr = alloc_js_array(
                                &mut vm,
                                &ports
                                    .iter()
                                    .map(|&p| JsValue::from_object(p))
                                    .collect::<Vec<_>>(),
                            );
                            freeze_js_array(&mut vm, ports_arr);
                            let empty_s = JsValue::from_string(vm.heap.alloc_string(""));
                            let g = vm.global;
                            fire_object_event_with_proto(
                                &mut vm,
                                &host,
                                &events,
                                &event_loop,
                                g,
                                "message",
                                msg_proto,
                                &[
                                    ("data", data),
                                    ("origin", empty_s),
                                    ("lastEventId", empty_s),
                                    ("source", JsValue::NULL),
                                    ("ports", JsValue::from_object(ports_arr)),
                                ],
                            );
                            run_microtask_checkpoint(&event_loop, &mut vm);
                            drain_ready_tasks(&event_loop, &mut vm);
                        }
                        DedWake::Cmd(Some(WorkerCommand::Terminate)) | DedWake::Cmd(None) => break,
                        DedWake::Net(Some(nev)) => {
                            deliver_async_net_event(
                                &mut vm,
                                &host,
                                &events,
                                &event_loop,
                                &web_apis,
                                &streams,
                                &net,
                                nev,
                            );
                        }
                        DedWake::Worker(Some(wev)) => {
                            deliver_async_worker_event(
                                &mut vm,
                                &host,
                                &events,
                                &event_loop,
                                &web_apis,
                                &frames,
                                &workers,
                                wev,
                            );
                        }
                        DedWake::Timer => {
                            if event_loop.borrow().has_pending_raf() {
                                let now_ms = event_loop.borrow().now_ms();
                                fire_vsync(&event_loop, &mut vm, now_ms);
                            }
                            drain_ready_tasks(&event_loop, &mut vm);
                        }
                        DedWake::Net(None) | DedWake::Worker(None) => {}
                    }
                }
                WorkerRuntimeMode::Shared { cmd_rx } => {
                    enum SharedWake {
                        Cmd(Option<SharedWorkerCommand>),
                        Net(Option<crate::fetch_ws::AsyncNetEvent>),
                        Worker(Option<AsyncWorkerEvent>),
                        Timer,
                    }
                    let wake = match deadline {
                        Some(dl) => tokio::select! {
                            biased;
                            c = cmd_rx.recv() => SharedWake::Cmd(c),
                            n = net_rx.recv() => SharedWake::Net(n),
                            w = worker_rx.recv() => SharedWake::Worker(w),
                            () = tokio::time::sleep_until(dl) => SharedWake::Timer,
                        },
                        None => tokio::select! {
                            biased;
                            c = cmd_rx.recv() => SharedWake::Cmd(c),
                            n = net_rx.recv() => SharedWake::Net(n),
                            w = worker_rx.recv() => SharedWake::Worker(w),
                        },
                    };
                    match wake {
                        SharedWake::Cmd(Some(SharedWorkerCommand::Connect {
                            worker_port_end,
                            ..
                        })) => {
                            let local_tx = workers.borrow().async_tx_clone();
                            let port_h = attach_transferred_cross_thread_port(
                                &mut vm,
                                &frames,
                                worker_port_end,
                                local_tx,
                            );
                            let ports_arr =
                                alloc_js_array(&mut vm, &[JsValue::from_object(port_h)]);
                            freeze_js_array(&mut vm, ports_arr);
                            let empty_s = JsValue::from_string(vm.heap.alloc_string(""));
                            let g = vm.global;
                            fire_object_event_with_proto(
                                &mut vm,
                                &host,
                                &events,
                                &event_loop,
                                g,
                                "connect",
                                msg_proto,
                                &[
                                    ("data", empty_s),
                                    ("origin", empty_s),
                                    ("lastEventId", empty_s),
                                    ("source", JsValue::from_object(port_h)),
                                    ("ports", JsValue::from_object(ports_arr)),
                                ],
                            );
                            run_microtask_checkpoint(&event_loop, &mut vm);
                            drain_ready_tasks(&event_loop, &mut vm);
                        }
                        SharedWake::Cmd(Some(SharedWorkerCommand::Terminate))
                        | SharedWake::Cmd(None) => break,
                        SharedWake::Net(Some(nev)) => {
                            deliver_async_net_event(
                                &mut vm,
                                &host,
                                &events,
                                &event_loop,
                                &web_apis,
                                &streams,
                                &net,
                                nev,
                            );
                        }
                        SharedWake::Worker(Some(wev)) => {
                            deliver_async_worker_event(
                                &mut vm,
                                &host,
                                &events,
                                &event_loop,
                                &web_apis,
                                &frames,
                                &workers,
                                wev,
                            );
                        }
                        SharedWake::Timer => {
                            if event_loop.borrow().has_pending_raf() {
                                let now_ms = event_loop.borrow().now_ms();
                                fire_vsync(&event_loop, &mut vm, now_ms);
                            }
                            drain_ready_tasks(&event_loop, &mut vm);
                        }
                        SharedWake::Net(None) | SharedWake::Worker(None) => {}
                    }
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// WHATWG Server-Sent Events (EventSource) incremental parser + streaming task
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct SseParser {
    line_buf: Vec<u8>,
    saw_cr: bool,
    stripped_bom: bool,
    event_type: String,
    data_buf: String,
    last_event_id: String,
    retry_ms: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedSseEvent {
    event_type: String,
    data: String,
    last_event_id: String,
}

impl SseParser {
    fn feed(&mut self, mut chunk: &[u8]) -> Vec<ParsedSseEvent> {
        let mut events = Vec::new();
        if !self.stripped_bom {
            self.stripped_bom = true;
            if let Some(rest) = chunk.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
                chunk = rest;
            }
        }
        for &b in chunk {
            if self.saw_cr {
                self.saw_cr = false;
                if b == b'\n' {
                    continue;
                }
            }
            if b == b'\r' {
                self.saw_cr = true;
                self.process_line(&mut events);
            } else if b == b'\n' {
                self.process_line(&mut events);
            } else {
                self.line_buf.push(b);
            }
        }
        events
    }

    fn process_line(&mut self, out: &mut Vec<ParsedSseEvent>) {
        let line = String::from_utf8_lossy(&self.line_buf).into_owned();
        self.line_buf.clear();

        if line.is_empty() {
            if !self.data_buf.is_empty() {
                if self.data_buf.ends_with('\n') {
                    self.data_buf.pop();
                }
                let ev_type = if self.event_type.is_empty() {
                    "message".to_string()
                } else {
                    std::mem::take(&mut self.event_type)
                };
                out.push(ParsedSseEvent {
                    event_type: ev_type,
                    data: std::mem::take(&mut self.data_buf),
                    last_event_id: self.last_event_id.clone(),
                });
            } else {
                self.event_type.clear();
            }
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, raw_value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line.as_str(), ""),
        };
        match field {
            "event" => self.event_type = raw_value.to_string(),
            "data" => {
                self.data_buf.push_str(raw_value);
                self.data_buf.push('\n');
            }
            "id" => {
                if !raw_value.contains('\0') {
                    self.last_event_id = raw_value.to_string();
                }
            }
            "retry" if !raw_value.is_empty() && raw_value.bytes().all(|c| c.is_ascii_digit()) => {
                if let Ok(ms) = raw_value.parse::<u64>() {
                    self.retry_ms = Some(ms);
                }
            }
            _ => {}
        }
    }
}

async fn run_sse_stream_task(
    sse_id: u64,
    url_str: String,
    origin: String,
    fetch_engine: Arc<FetchEngine>,
    tx: mpsc::UnboundedSender<AsyncWorkerEvent>,
    mut close_rx: oneshot::Receiver<()>,
) {
    let Ok(rix_url) = RixUrl::parse(&url_str) else {
        let _ = tx.send(AsyncWorkerEvent::SseError {
            sse_id,
            closed: true,
        });
        return;
    };
    let mut headers = HashMap::new();
    headers.insert("accept".to_string(), "text/event-stream".to_string());
    headers.insert("cache-control".to_string(), "no-cache".to_string());

    let client = fetch_engine.client().clone();
    let resp_res = tokio::select! {
        biased;
        _ = &mut close_rx => return,
        res = client.request_stream(&rix_url, "GET", &headers, Vec::new()) => res,
    };

    let Ok(mut resp) = resp_res else {
        let _ = tx.send(AsyncWorkerEvent::SseError {
            sse_id,
            closed: true,
        });
        return;
    };

    let mime_ok = resp
        .content_type
        .split(';')
        .next()
        .is_some_and(|m| m.trim().eq_ignore_ascii_case("text/event-stream"));
    if resp.status != 200 || !mime_ok {
        let _ = tx.send(AsyncWorkerEvent::SseError {
            sse_id,
            closed: true,
        });
        return;
    }

    let sse_origin = Url::parse(resp.final_url.as_str())
        .map(|u| origin_of_url(&u))
        .unwrap_or(origin);
    let _ = tx.send(AsyncWorkerEvent::SseOpen { sse_id });

    let mut parser = SseParser::default();
    loop {
        let next_chunk = tokio::select! {
            biased;
            _ = &mut close_rx => break,
            chunk = resp.body.next_chunk() => chunk,
        };
        match next_chunk {
            Some(Ok(bytes)) => {
                for ev in parser.feed(&bytes) {
                    let _ = tx.send(AsyncWorkerEvent::SseEvent {
                        sse_id,
                        event_type: ev.event_type,
                        data: ev.data,
                        last_event_id: ev.last_event_id,
                        origin: sse_origin.clone(),
                    });
                }
            }
            Some(Err(_)) | None => {
                let _ = tx.send(AsyncWorkerEvent::SseError {
                    sse_id,
                    closed: true,
                });
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Delivery into `ContentRuntime` (or nested Worker) event loop
// ---------------------------------------------------------------------------

/// Delivers one reactive [`AsyncWorkerEvent`] into `vm`, followed by a microtask checkpoint.
#[allow(clippy::too_many_arguments)]
pub fn deliver_async_worker_event(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    frames: &SharedFrames,
    workers: &SharedWorkers,
    ev: AsyncWorkerEvent,
) {
    let msg_proto = frames
        .borrow()
        .message_event_proto
        .or_else(|| events.borrow().event_proto)
        .unwrap_or(vm.object_proto);
    let base_event_proto = events.borrow().event_proto.unwrap_or(vm.object_proto);

    match ev {
        AsyncWorkerEvent::DedicatedMessage { worker_id, payload } => {
            let target = {
                let ws = workers.borrow();
                ws.workers_by_id
                    .get(&worker_id)
                    .filter(|w| !w.terminated)
                    .map(|w| w.js_obj)
            };
            let Some(target) = target else { return };
            let (nb_opt, c2d_opt) = {
                let fs = frames.borrow();
                (fs.net.clone(), fs.canvas2d.clone())
            };
            let (data, ports) = if let (Some(nb), Some(c2d)) = (nb_opt, c2d_opt) {
                let ctx = CloneContext {
                    host,
                    events,
                    web_apis,
                    net: &nb,
                    canvas2d: &c2d,
                    frames,
                    workers,
                };
                deserialize_payload(vm, &ctx, payload)
            } else {
                (JsValue::NULL, Vec::new())
            };
            let empty_s = JsValue::from_string(vm.heap.alloc_string(""));
            let ports_arr = alloc_js_array(
                vm,
                &ports
                    .iter()
                    .map(|&p| JsValue::from_object(p))
                    .collect::<Vec<_>>(),
            );
            freeze_js_array(vm, ports_arr);
            fire_object_event_dispatch_only(
                vm,
                host,
                events,
                event_loop,
                target,
                "message",
                msg_proto,
                &[
                    ("data", data),
                    ("origin", empty_s),
                    ("lastEventId", empty_s),
                    ("source", JsValue::NULL),
                    ("ports", JsValue::from_object(ports_arr)),
                ],
                None,
            );
        }
        AsyncWorkerEvent::DedicatedFetchError { worker_id } => {
            let target = {
                let ws = workers.borrow();
                ws.workers_by_id
                    .get(&worker_id)
                    .filter(|w| !w.terminated)
                    .map(|w| w.js_obj)
            };
            let Some(target) = target else { return };
            fire_object_event_dispatch_only(
                vm,
                host,
                events,
                event_loop,
                target,
                "error",
                base_event_proto,
                &[],
                None,
            );
        }
        AsyncWorkerEvent::DedicatedRuntimeError {
            worker_id,
            message,
            filename,
            lineno,
            colno,
        } => {
            let target = {
                let ws = workers.borrow();
                ws.workers_by_id
                    .get(&worker_id)
                    .filter(|w| !w.terminated)
                    .map(|w| w.js_obj)
            };
            let Some(target) = target else { return };
            // HTML §10.2.5: parent `Worker` receives cancelable `ErrorEvent` with `error: null`.
            let (prevented, _) = fire_error_event(
                vm,
                host,
                events,
                event_loop,
                target,
                &message,
                &filename,
                lineno,
                colno,
                JsValue::NULL,
                false,
            );
            if !prevented {
                // Propagate uncanceled error up to parent global (`window.onerror` / parent worker).
                let g = vm.global;
                let _ = fire_error_event(
                    vm,
                    host,
                    events,
                    event_loop,
                    g,
                    &message,
                    &filename,
                    lineno,
                    colno,
                    JsValue::NULL,
                    true,
                );
            }
        }
        AsyncWorkerEvent::SharedWorkerFetchError { shared_worker_id } => {
            let target = workers
                .borrow()
                .shared_workers_by_id
                .get(&shared_worker_id)
                .map(|sw| sw.js_obj);
            let Some(target) = target else { return };
            fire_object_event_dispatch_only(
                vm,
                host,
                events,
                event_loop,
                target,
                "error",
                base_event_proto,
                &[],
                None,
            );
        }
        AsyncWorkerEvent::BridgePortMessage { port_id, payload } => {
            let (nb_opt, c2d_opt) = {
                let fs = frames.borrow();
                (fs.net.clone(), fs.canvas2d.clone())
            };
            if let (Some(nb), Some(c2d)) = (nb_opt, c2d_opt) {
                deliver_cross_thread_port_message(
                    vm, host, events, event_loop, web_apis, &nb, &c2d, frames, workers, port_id,
                    payload,
                );
            }
        }
        AsyncWorkerEvent::BroadcastMessage {
            channel_id,
            origin,
            payload_json,
        } => {
            let target = {
                let ws = workers.borrow();
                ws.broadcasts_by_id
                    .get(&channel_id)
                    .filter(|b| !b.closed)
                    .map(|b| b.js_obj)
            };
            let Some(target) = target else { return };
            let data = serde_json::from_str::<serde_json::Value>(&payload_json)
                .map(|jv| crate::frames::decode_structured_json_with_blobs(vm, web_apis, &jv))
                .unwrap_or(JsValue::NULL);
            let origin_js = JsValue::from_string(vm.heap.alloc_string(&origin));
            let empty_s = JsValue::from_string(vm.heap.alloc_string(""));
            let ports_arr = alloc_js_array(vm, &[]);
            freeze_js_array(vm, ports_arr);
            fire_object_event_dispatch_only(
                vm,
                host,
                events,
                event_loop,
                target,
                "message",
                msg_proto,
                &[
                    ("data", data),
                    ("origin", origin_js),
                    ("lastEventId", empty_s),
                    ("source", JsValue::NULL),
                    ("ports", JsValue::from_object(ports_arr)),
                ],
                None,
            );
        }
        AsyncWorkerEvent::SseOpen { sse_id } => {
            let target = {
                let mut ws = workers.borrow_mut();
                ws.sse_by_id
                    .get_mut(&sse_id)
                    .filter(|s| s.ready_state != 2)
                    .map(|s| {
                        s.ready_state = 1;
                        s.js_obj
                    })
            };
            if let Some(target) = target {
                fire_object_event(vm, host, events, event_loop, target, "open", &[]);
            }
        }
        AsyncWorkerEvent::SseEvent {
            sse_id,
            event_type,
            data,
            last_event_id,
            origin,
        } => {
            let target = {
                let ws = workers.borrow();
                ws.sse_by_id
                    .get(&sse_id)
                    .filter(|s| s.ready_state != 2)
                    .map(|s| s.js_obj)
            };
            let Some(target) = target else { return };
            let data_js = JsValue::from_string(vm.heap.alloc_string(&data));
            let origin_js = JsValue::from_string(vm.heap.alloc_string(&origin));
            let id_js = JsValue::from_string(vm.heap.alloc_string(&last_event_id));
            let ports_arr = alloc_js_array(vm, &[]);
            fire_object_event_with_proto(
                vm,
                host,
                events,
                event_loop,
                target,
                &event_type,
                msg_proto,
                &[
                    ("data", data_js),
                    ("origin", origin_js),
                    ("lastEventId", id_js),
                    ("source", JsValue::NULL),
                    ("ports", JsValue::from_object(ports_arr)),
                ],
            );
        }
        AsyncWorkerEvent::SseError { sse_id, closed } => {
            let target = {
                let mut ws = workers.borrow_mut();
                ws.sse_by_id.get_mut(&sse_id).and_then(|s| {
                    if s.ready_state == 2 {
                        return None;
                    }
                    if closed {
                        s.ready_state = 2;
                        s.close_tx = None;
                    }
                    Some(s.js_obj)
                })
            };
            if let Some(target) = target {
                fire_object_event(vm, host, events, event_loop, target, "error", &[]);
            }
        }
    }
    run_microtask_checkpoint(event_loop, vm);
}

// ---------------------------------------------------------------------------
// Options parsing helpers for `Worker` and `SharedWorker`
// ---------------------------------------------------------------------------

fn parse_worker_options_obj(
    vm: &mut JsVm,
    opts_h: HeapHandle,
) -> Result<(String, WorkerOptions), JsException> {
    let kt = vm.heap.intern_key("type");
    let kc = vm.heap.intern_key("credentials");
    let kn = vm.heap.intern_key("name");
    let ke = vm.heap.intern_key("extendedLifetime");

    let tv = vm.get_property_by_key(opts_h, kt)?;
    let worker_type = if tv.is_undefined() {
        WorkerType::Classic
    } else {
        match vm.to_js_string(tv)?.as_str() {
            "classic" => WorkerType::Classic,
            "module" => WorkerType::Module,
            other => {
                return Err(vm.type_error(format!("Invalid WorkerOptions.type value '{other}'")));
            }
        }
    };

    let cv = vm.get_property_by_key(opts_h, kc)?;
    let credentials = if cv.is_undefined() {
        WorkerCreds::SameOrigin
    } else {
        match vm.to_js_string(cv)?.as_str() {
            "omit" => WorkerCreds::Omit,
            "same-origin" => WorkerCreds::SameOrigin,
            "include" => WorkerCreds::Include,
            other => {
                return Err(
                    vm.type_error(format!("Invalid WorkerOptions.credentials value '{other}'"))
                );
            }
        }
    };

    let nv = vm.get_property_by_key(opts_h, kn)?;
    let name = if nv.is_undefined() {
        String::new()
    } else {
        vm.to_js_string(nv)?
    };

    let ev = vm.get_property_by_key(opts_h, ke)?;
    let extended_lifetime = !ev.is_undefined() && vm.to_boolean(ev);

    Ok((
        name,
        WorkerOptions {
            worker_type,
            credentials,
            extended_lifetime,
        },
    ))
}

fn current_doc_charset(host: &SharedHost) -> String {
    host.borrow().document.borrow().character_set().to_string()
}

/// Resolves the document base URL honoring any `<base href="...">` element per HTML §4.2.3.
fn document_base_href(host: &SharedHost, web_apis: &SharedWebApis) -> String {
    let doc_href = web_apis.borrow().current_href();
    let doc_rc = Rc::clone(&host.borrow().document);
    let doc = doc_rc.borrow();
    for nid in doc.descendants(doc.root()) {
        if let Some(rix_dom::NodeData::Element(el)) = doc.get(nid) {
            if el.name.local.as_str().eq_ignore_ascii_case("base") {
                if let Some(href) = el.get_attribute("href") {
                    if let Ok(base) = Url::parse(&doc_href) {
                        if let Ok(joined) = base.join(href.trim()) {
                            return joined.to_string();
                        }
                    }
                }
            }
        }
    }
    doc_href
}

// ---------------------------------------------------------------------------
// Installation onto `ContentRuntime`'s (or Worker's) `JsVm`
// ---------------------------------------------------------------------------

/// Installs `Worker`, `SharedWorker`, `BroadcastChannel`, and `EventSource`
/// constructors and prototypes on `vm`.
#[allow(clippy::too_many_arguments)]
pub fn install_worker_and_sse_apis(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    frames: &SharedFrames,
) -> SharedWorkers {
    let workers = Rc::new(RefCell::new(WorkerSystem::new()));
    let event_target_proto = host.borrow().prototypes.event_target_proto;

    install_dedicated_worker(
        vm,
        event_target_proto,
        host,
        events,
        web_apis,
        net,
        frames,
        &workers,
    );
    install_shared_worker(
        vm,
        event_target_proto,
        host,
        events,
        web_apis,
        net,
        frames,
        &workers,
    );
    install_broadcast_channel(vm, event_target_proto, events, web_apis, &workers);
    install_event_source(vm, event_target_proto, host, web_apis, net, &workers);

    let _ = event_loop;
    workers
}

#[allow(clippy::too_many_arguments)]
fn install_dedicated_worker(
    vm: &mut JsVm,
    event_target_proto: HeapHandle,
    host: &SharedHost,
    events: &SharedEvents,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    frames: &SharedFrames,
    workers: &SharedWorkers,
) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(event_target_proto),
    ));
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;
    define_to_string_tag(vm, proto, "Worker");
    workers.borrow_mut().protos.worker_proto = Some(proto);

    for &attr in &["onmessage", "onmessageerror", "onerror"] {
        let ws_v = Rc::clone(workers);
        install_object_event_handler_accessor(vm, proto, events, attr, move |vm, this_val| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Incompatible Worker receiver"));
            };
            if ws_v.borrow().worker_handle_to_id.contains_key(&h) {
                Ok(h)
            } else {
                Err(vm.type_error("Incompatible Worker receiver"))
            }
        });
    }

    {
        let h_pm = Rc::clone(host);
        let ev_pm = Rc::clone(events);
        let wa_pm = Rc::clone(web_apis);
        let nb_pm = Rc::clone(net);
        let fr_pm = Rc::clone(frames);
        let ws_pm = Rc::clone(workers);
        define_host_method(vm, proto, "postMessage", move |vm, this_val, args| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Incompatible Worker receiver"));
            };
            if !ws_pm.borrow().worker_handle_to_id.contains_key(&h) {
                return Err(vm.type_error("Incompatible Worker receiver"));
            }
            if args.is_empty() {
                return Err(vm.type_error("Worker.postMessage requires at least 1 argument"));
            }
            let transfer_val = extract_transfer_arg(vm, arg(args, 1))?;
            let c2d_opt = fr_pm.borrow().canvas2d.clone();
            let payload = if let Some(c2d) = c2d_opt {
                let ctx = CloneContext {
                    host: &h_pm,
                    events: &ev_pm,
                    web_apis: &wa_pm,
                    net: &nb_pm,
                    canvas2d: &c2d,
                    frames: &fr_pm,
                    workers: &ws_pm,
                };
                serialize_with_transfer(vm, &ctx, arg(args, 0), transfer_val, &[])?
            } else {
                StructuredClonePayload::undefined()
            };
            let map = ws_pm.borrow();
            if let Some(&wid) = map.worker_handle_to_id.get(&h) {
                if let Some(entry) = map.workers_by_id.get(&wid) {
                    if !entry.terminated {
                        let _ = entry.cmd_tx.send(WorkerCommand::PostMessage(payload));
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let ws = Rc::clone(workers);
        define_host_method(vm, proto, "terminate", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Incompatible Worker receiver"));
            };
            let mut map = ws.borrow_mut();
            let Some(&wid) = map.worker_handle_to_id.get(&h) else {
                return Err(vm.type_error("Incompatible Worker receiver"));
            };
            if let Some(entry) = map.workers_by_id.get_mut(&wid) {
                if !entry.terminated {
                    entry.terminated = true;
                    let _ = entry.cmd_tx.send(WorkerCommand::Terminate);
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    let h_c = Rc::clone(host);
    let wa = Rc::clone(web_apis);
    let nb = Rc::clone(net);
    let fr = Rc::clone(frames);
    let ws = Rc::clone(workers);
    let ctor = vm.alloc_host_fn("Worker", move |vm, _, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("Worker constructor requires 'new'"));
        }
        if args.is_empty() {
            return Err(vm.type_error("Worker constructor requires at least 1 argument"));
        }
        let raw_url = vm.to_js_string(arg(args, 0))?;
        let second = arg(args, 1);
        let (worker_name, options) = if second.is_undefined() || second.is_null() {
            (
                String::new(),
                WorkerOptions {
                    worker_type: WorkerType::Classic,
                    credentials: WorkerCreds::SameOrigin,
                    extended_lifetime: false,
                },
            )
        } else if let Some(oh) = second.as_object() {
            parse_worker_options_obj(vm, oh)?
        } else {
            return Err(vm.type_error("Worker options must be an object"));
        };

        let charset = current_doc_charset(&h_c);
        let base_href = document_base_href(&h_c, &wa);
        let (doc_href, origin, cross_origin_isolated, object_urls_snapshot, cookie_header) = {
            let mut state = wa.borrow_mut();
            let href = state.current_href();
            let eff_origin = fr
                .borrow()
                .sandbox
                .effective_origin(&state.current_origin());
            let ck = Url::parse(&href)
                .map(|u| state.cookie_header_for_url(&u, false))
                .unwrap_or_default();
            (
                href,
                eff_origin,
                state.cross_origin_isolated,
                Arc::new(state.object_urls.clone()),
                ck,
            )
        };
        let resolved_url = resolve_script_url(&raw_url, &base_href, &charset)
            .map_err(|m| create_dom_exception(vm, "SyntaxError", &m))?;
        let fetch_engine = Arc::clone(&nb.borrow().fetch_engine);

        let worker_id = next_global_id();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let parent_tx = ws.borrow().async_tx.clone();

        let _ = std::thread::Builder::new()
            .name(format!("rix-worker-{worker_id}"))
            .spawn(move || {
                run_worker_thread(
                    resolved_url,
                    origin,
                    doc_href,
                    worker_name,
                    options,
                    cross_origin_isolated,
                    object_urls_snapshot,
                    fetch_engine,
                    cookie_header,
                    WorkerRuntimeMode::Dedicated {
                        worker_id,
                        parent_tx,
                        cmd_rx,
                    },
                );
            });

        let obj_h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        {
            let mut state = ws.borrow_mut();
            state.workers_by_id.insert(
                worker_id,
                DedicatedWorkerEntry {
                    js_obj: obj_h,
                    cmd_tx,
                    terminated: false,
                },
            );
            state.worker_handle_to_id.insert(obj_h, worker_id);
        }
        Ok(JsValue::from_object(obj_h))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "Worker", JsValue::from_object(ctor));
}

#[allow(clippy::too_many_arguments)]
fn install_shared_worker(
    vm: &mut JsVm,
    event_target_proto: HeapHandle,
    host: &SharedHost,
    events: &SharedEvents,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    frames: &SharedFrames,
    workers: &SharedWorkers,
) {
    let sw_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(event_target_proto),
    ));
    vm.heap.get_object_mut(sw_proto).unwrap().is_prototype = true;
    define_to_string_tag(vm, sw_proto, "SharedWorker");
    workers.borrow_mut().protos.shared_worker_proto = Some(sw_proto);

    {
        let ws_v = Rc::clone(workers);
        install_object_event_handler_accessor(
            vm,
            sw_proto,
            events,
            "onerror",
            move |vm, this_val| {
                let Some(h) = this_val.as_object() else {
                    return Err(vm.type_error("Incompatible SharedWorker receiver"));
                };
                if ws_v.borrow().shared_worker_handle_to_id.contains_key(&h) {
                    Ok(h)
                } else {
                    Err(vm.type_error("Incompatible SharedWorker receiver"))
                }
            },
        );
    }

    // Readonly `SharedWorker.prototype.port` getter per HTML §10.2.6.
    {
        let ws = Rc::clone(workers);
        define_host_getter(vm, sw_proto, "port", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Incompatible SharedWorker receiver"));
            };
            let state = ws.borrow();
            let Some(&sw_id) = state.shared_worker_handle_to_id.get(&h) else {
                return Err(vm.type_error("Incompatible SharedWorker receiver"));
            };
            let Some(entry) = state.shared_workers_by_id.get(&sw_id) else {
                return Err(vm.type_error("Incompatible SharedWorker receiver"));
            };
            Ok(JsValue::from_object(entry.port_h))
        });
    }

    let h_c = Rc::clone(host);
    let wa = Rc::clone(web_apis);
    let nb = Rc::clone(net);
    let fr = Rc::clone(frames);
    let ws = Rc::clone(workers);
    let ctor = vm.alloc_host_fn("SharedWorker", move |vm, _, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("SharedWorker constructor requires 'new'"));
        }
        if args.is_empty() {
            return Err(vm.type_error("SharedWorker constructor requires at least 1 argument"));
        }
        let raw_url = vm.to_js_string(arg(args, 0))?;
        let second = arg(args, 1);
        let (worker_name, options) = if second.is_undefined() || second.is_null() {
            (
                String::new(),
                WorkerOptions {
                    worker_type: WorkerType::Classic,
                    credentials: WorkerCreds::SameOrigin,
                    extended_lifetime: false,
                },
            )
        } else if let Some(oh) = second.as_object() {
            parse_worker_options_obj(vm, oh)?
        } else {
            // WebIDL `(DOMString or WorkerOptions)`: numbers (`1`, `NaN`, `Infinity`),
            // booleans, `null`, and strings coerce to `DOMString` name.
            let name = vm.to_js_string(second)?;
            (
                name,
                WorkerOptions {
                    worker_type: WorkerType::Classic,
                    credentials: WorkerCreds::SameOrigin,
                    extended_lifetime: false,
                },
            )
        };

        let charset = current_doc_charset(&h_c);
        let base_href = document_base_href(&h_c, &wa);
        let (doc_href, origin, cross_origin_isolated, object_urls_snapshot, cookie_header) = {
            let mut state = wa.borrow_mut();
            let href = state.current_href();
            let eff_origin = fr
                .borrow()
                .sandbox
                .effective_origin(&state.current_origin());
            let ck = Url::parse(&href)
                .map(|u| state.cookie_header_for_url(&u, false))
                .unwrap_or_default();
            (
                href,
                eff_origin,
                state.cross_origin_isolated,
                Arc::new(state.object_urls.clone()),
                ck,
            )
        };
        let resolved_url = resolve_script_url(&raw_url, &base_href, &charset)
            .map_err(|m| create_dom_exception(vm, "SyntaxError", &m))?;
        let fetch_engine = Arc::clone(&nb.borrow().fetch_engine);

        // Allocate paired cross-thread MessagePort routes so `worker.port` is a genuine `MessagePort`!
        let route_to_client = Arc::new(Mutex::new(PortRoute::Buffered(
            std::collections::VecDeque::new(),
        )));
        let route_to_worker = Arc::new(Mutex::new(PortRoute::Buffered(
            std::collections::VecDeque::new(),
        )));
        let client_tx = ws.borrow().async_tx.clone();
        let client_port_h = attach_transferred_cross_thread_port(
            vm,
            &fr,
            CrossThreadPortEnd {
                local_route: Arc::clone(&route_to_client),
                remote_route: Arc::clone(&route_to_worker),
            },
            client_tx.clone(),
        );

        let shared_worker_id = next_global_id();
        let sw_h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(sw_proto)));
        {
            let mut state = ws.borrow_mut();
            state.shared_workers_by_id.insert(
                shared_worker_id,
                SharedWorkerClientEntry {
                    js_obj: sw_h,
                    port_h: client_port_h,
                },
            );
            state
                .shared_worker_handle_to_id
                .insert(sw_h, shared_worker_id);
        }

        let registry_key = (origin.clone(), resolved_url.clone(), worker_name.clone());
        let connect_target = {
            let mut reg = shared_workers_registry()
                .lock()
                .expect("shared_workers lock");
            // Prune dead registry entries whose thread already closed.
            if let Some(existing) = reg.get(&registry_key) {
                if existing.cmd_tx.is_closed() {
                    reg.remove(&registry_key);
                }
            }
            if let Some(existing) = reg.get(&registry_key) {
                if existing.options != options {
                    // HTML §10.2.6.2: mismatched type / credentials / extendedLifetime -> async error event!
                    None
                } else {
                    Some(existing.cmd_tx.clone())
                }
            } else {
                let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
                let url_c = resolved_url.clone();
                let origin_c = origin.clone();
                let href_c = doc_href.clone();
                let name_c = worker_name.clone();
                let _ = std::thread::Builder::new()
                    .name(format!("rix-shared-worker-{shared_worker_id}"))
                    .spawn(move || {
                        run_worker_thread(
                            url_c,
                            origin_c,
                            href_c,
                            name_c,
                            options,
                            cross_origin_isolated,
                            object_urls_snapshot,
                            fetch_engine,
                            cookie_header,
                            WorkerRuntimeMode::Shared { cmd_rx },
                        );
                    });
                reg.insert(
                    registry_key,
                    SharedWorkerHandle {
                        cmd_tx: cmd_tx.clone(),
                        options,
                    },
                );
                Some(cmd_tx)
            }
        };

        if let Some(worker_tx) = connect_target {
            let _ = worker_tx.send(SharedWorkerCommand::Connect {
                worker_port_end: CrossThreadPortEnd {
                    local_route: route_to_worker,
                    remote_route: route_to_client,
                },
                client_error_tx: client_tx,
                shared_worker_id,
            });
        } else {
            PortRoute::close(&route_to_worker);
            let _ = client_tx.send(AsyncWorkerEvent::SharedWorkerFetchError { shared_worker_id });
        }

        Ok(JsValue::from_object(sw_h))
    });
    bind_ctor_proto(vm, ctor, sw_proto);
    define_global(vm, "SharedWorker", JsValue::from_object(ctor));
}

fn install_broadcast_channel(
    vm: &mut JsVm,
    event_target_proto: HeapHandle,
    events: &SharedEvents,
    web_apis: &SharedWebApis,
    workers: &SharedWorkers,
) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(event_target_proto),
    ));
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;
    crate::streams_common::set_to_string_tag(vm, proto, "BroadcastChannel");
    workers.borrow_mut().protos.broadcast_channel_proto = Some(proto);

    // EventHandler IDL attributes (`onmessage`, `onmessageerror`) per HTML §8.1.7.1.
    for &attr in &["onmessage", "onmessageerror"] {
        let ws_v = Rc::clone(workers);
        install_object_event_handler_accessor(vm, proto, events, attr, move |vm, this_val| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Incompatible BroadcastChannel receiver"));
            };
            if ws_v.borrow().broadcast_handle_to_id.contains_key(&h) {
                Ok(h)
            } else {
                Err(vm.type_error("Incompatible BroadcastChannel receiver"))
            }
        });
    }

    {
        let ws = Rc::clone(workers);
        define_host_getter(vm, proto, "name", move |vm, this_val, _| {
            let name = this_val
                .as_object()
                .and_then(|h| ws.borrow().broadcast_handle_to_id.get(&h).copied())
                .and_then(|id| {
                    ws.borrow()
                        .broadcasts_by_id
                        .get(&id)
                        .map(|b| b.name.clone())
                })
                .unwrap_or_default();
            Ok(JsValue::from_string(vm.heap.alloc_string(&name)))
        });
    }
    {
        let ws = Rc::clone(workers);
        let wa = Rc::clone(web_apis);
        define_host_method(vm, proto, "postMessage", move |vm, this_val, args| {
            if args.is_empty() {
                return Err(vm.type_error("BroadcastChannel.postMessage requires 1 argument"));
            }
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Incompatible BroadcastChannel receiver"));
            };
            let info = {
                let state = ws.borrow();
                state
                    .broadcast_handle_to_id
                    .get(&h)
                    .and_then(|id| state.broadcasts_by_id.get(id))
                    .map(|b| (b.id, b.origin.clone(), b.name.clone(), b.closed))
            };
            let Some((sender_id, origin, name, closed)) = info else {
                return Err(vm.type_error("Incompatible BroadcastChannel receiver"));
            };
            if closed {
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "BroadcastChannel is closed",
                ));
            }
            let mut seen = HashSet::new();
            let encoded =
                crate::frames::encode_structured_json_with_blobs(vm, &wa, arg(args, 0), &mut seen)?;
            let payload_json = serde_json::to_string(&encoded).unwrap_or_else(|_| "null".into());
            broadcast_to_peers(sender_id, &origin, &name, &payload_json);
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let ws = Rc::clone(workers);
        define_host_method(vm, proto, "close", move |_, this_val, _| {
            if let Some(h) = this_val.as_object() {
                let mut state = ws.borrow_mut();
                if let Some(&id) = state.broadcast_handle_to_id.get(&h) {
                    if let Some(bc) = state.broadcasts_by_id.get_mut(&id) {
                        if !bc.closed {
                            bc.closed = true;
                            unregister_broadcast_subscriber(&bc.origin, &bc.name, id);
                        }
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    let wa = Rc::clone(web_apis);
    let ws = Rc::clone(workers);
    let ctor = vm.alloc_host_fn("BroadcastChannel", move |vm, _, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("BroadcastChannel constructor requires 'new'"));
        }
        if args.is_empty() {
            return Err(vm.type_error("BroadcastChannel constructor requires 1 argument"));
        }
        let name = vm.to_js_string(arg(args, 0))?;
        let origin = wa.borrow().current_origin();
        let id = next_global_id();
        let obj_h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        let tx = ws.borrow().async_tx.clone();
        if let Ok(mut hub) = broadcast_hub().lock() {
            hub.entry((origin.clone(), name.clone()))
                .or_default()
                .push(BroadcastSubscriber {
                    id,
                    sink: BroadcastSink::Async(tx),
                });
        }
        {
            let mut state = ws.borrow_mut();
            state.broadcasts_by_id.insert(
                id,
                BroadcastChannelEntry {
                    id,
                    js_obj: obj_h,
                    origin,
                    name,
                    closed: false,
                },
            );
            state.broadcast_handle_to_id.insert(obj_h, id);
        }
        Ok(JsValue::from_object(obj_h))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "BroadcastChannel", JsValue::from_object(ctor));
}

fn install_event_source(
    vm: &mut JsVm,
    event_target_proto: HeapHandle,
    host: &SharedHost,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    workers: &SharedWorkers,
) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(event_target_proto),
    ));
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;
    define_to_string_tag(vm, proto, "EventSource");
    workers.borrow_mut().protos.event_source_proto = Some(proto);

    for (const_name, val) in [("CONNECTING", 0), ("OPEN", 1), ("CLOSED", 2)] {
        let k = vm.heap.intern_key(const_name);
        vm.heap.object_define_data(
            proto,
            k,
            JsValue::from_number(f64::from(val)),
            PropertyAttrs::READONLY,
            &mut vm.shapes,
        );
    }

    {
        let ws = Rc::clone(workers);
        define_host_getter(vm, proto, "url", move |vm, this_val, _| {
            let u = this_val
                .as_object()
                .and_then(|h| ws.borrow().sse_handle_to_id.get(&h).copied())
                .and_then(|id| ws.borrow().sse_by_id.get(&id).map(|s| s.url.clone()))
                .unwrap_or_default();
            Ok(JsValue::from_string(vm.heap.alloc_string(&u)))
        });
    }
    {
        let ws = Rc::clone(workers);
        define_host_getter(vm, proto, "withCredentials", move |_, this_val, _| {
            let wc = this_val
                .as_object()
                .and_then(|h| ws.borrow().sse_handle_to_id.get(&h).copied())
                .and_then(|id| ws.borrow().sse_by_id.get(&id).map(|s| s.with_credentials))
                .unwrap_or(false);
            Ok(JsValue::from_bool(wc))
        });
    }
    {
        let ws = Rc::clone(workers);
        define_host_getter(vm, proto, "readyState", move |_, this_val, _| {
            let rs = this_val
                .as_object()
                .and_then(|h| ws.borrow().sse_handle_to_id.get(&h).copied())
                .and_then(|id| ws.borrow().sse_by_id.get(&id).map(|s| s.ready_state))
                .unwrap_or(2);
            Ok(JsValue::from_number(f64::from(rs)))
        });
    }
    {
        let ws = Rc::clone(workers);
        define_host_method(vm, proto, "close", move |_, this_val, _| {
            if let Some(h) = this_val.as_object() {
                let mut state = ws.borrow_mut();
                if let Some(&id) = state.sse_handle_to_id.get(&h) {
                    if let Some(entry) = state.sse_by_id.get_mut(&id) {
                        entry.ready_state = 2;
                        if let Some(tx) = entry.close_tx.take() {
                            let _ = tx.send(());
                        }
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    let h_c = Rc::clone(host);
    let wa = Rc::clone(web_apis);
    let nb = Rc::clone(net);
    let ws = Rc::clone(workers);
    let ctor = vm.alloc_host_fn("EventSource", move |vm, _, args| {
        let raw_url = vm.value_to_rust_string(arg(args, 0));
        let with_credentials = if let Some(opts) = arg(args, 1).as_object() {
            let k = vm.heap.intern_key("withCredentials");
            vm.get_property_by_key(opts, k)
                .ok()
                .is_some_and(|v| vm.to_boolean(v))
        } else {
            false
        };
        let charset = current_doc_charset(&h_c);
        let (base_href, origin) = {
            let state = wa.borrow();
            (state.current_href(), state.current_origin())
        };
        let resolved_url = resolve_script_url(&raw_url, &base_href, &charset)
            .map_err(|m| create_dom_exception(vm, "SyntaxError", &m))?;
        let sse_id = next_global_id();
        let (close_tx, close_rx) = oneshot::channel();
        let tx = ws.borrow().async_tx.clone();
        let fe = Arc::clone(&nb.borrow().fetch_engine);
        let url_clone = resolved_url.clone();

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(run_sse_stream_task(
                sse_id, url_clone, origin, fe, tx, close_rx,
            ));
        } else {
            let _ = std::thread::Builder::new()
                .name(format!("rix-sse-{sse_id}"))
                .spawn(move || {
                    if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        rt.block_on(run_sse_stream_task(
                            sse_id, url_clone, origin, fe, tx, close_rx,
                        ));
                    }
                });
        }

        let obj_h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        {
            let mut state = ws.borrow_mut();
            state.sse_by_id.insert(
                sse_id,
                EventSourceEntry {
                    js_obj: obj_h,
                    url: resolved_url,
                    with_credentials,
                    ready_state: 0,
                    close_tx: Some(close_tx),
                },
            );
            state.sse_handle_to_id.insert(obj_h, sse_id);
        }
        Ok(JsValue::from_object(obj_h))
    });
    for (const_name, val) in [("CONNECTING", 0), ("OPEN", 1), ("CLOSED", 2)] {
        let k = vm.heap.intern_key(const_name);
        vm.heap.object_define_data(
            ctor,
            k,
            JsValue::from_number(f64::from(val)),
            PropertyAttrs::READONLY,
            &mut vm.shapes,
        );
    }
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "EventSource", JsValue::from_object(ctor));
}

#[cfg(test)]
mod tests {
    use super::{ParsedSseEvent, SseParser};

    #[test]
    fn sse_parser_handles_chunked_multiline_events_ids_and_comments() {
        let mut p = SseParser::default();
        let mut out = Vec::new();
        out.extend(p.feed(b"\xEF\xBB\xBF: comment ignored\r\n"));
        out.extend(p.feed(b"id: 42\ndata: hello "));
        out.extend(p.feed(b"world\ndata:line2\n\n"));
        out.extend(p.feed(b"event: ping\nretry: 1500\ndata: pong\r\r"));
        assert_eq!(
            out,
            vec![
                ParsedSseEvent {
                    event_type: "message".to_string(),
                    data: "hello world\nline2".to_string(),
                    last_event_id: "42".to_string(),
                },
                ParsedSseEvent {
                    event_type: "ping".to_string(),
                    data: "pong".to_string(),
                    last_event_id: "42".to_string(),
                },
            ]
        );
        assert_eq!(p.retry_ms, Some(1500));
    }
}
