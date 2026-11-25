//! Dedicated `Worker`, `SharedWorker`, `BroadcastChannel`, and `EventSource` (SSE)
//! Web APIs running isolated JS VM OS threads with `structuredClone` messaging and
//! reactive zero-polling `tokio::select!` delivery.
//!
//! Reference: Spec 06 §3, Issue #24.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rix_core::RixUrl;
use rix_js::{HeapHandle, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_net::{FetchCredentials, FetchEngine, FetchMode, FetchRedirect, FetchRequest};
use tokio::sync::{mpsc, oneshot};
use url::Url;

fn alloc_js_array(vm: &mut JsVm, items: &[JsValue]) -> HeapHandle {
    let arr = vm.alloc_array();
    for &v in items {
        vm.heap.object_push_element(arr, v);
    }
    arr
}

use crate::dom_bridge::SharedHost;
use crate::event_loop::{
    drain_ready_tasks, install_event_loop_globals, run_microtask_checkpoint, EventLoopState,
    SharedEventLoop, TaskPriority,
};
use crate::events::{fire_object_event, fire_object_event_with_proto, SharedEvents};
use crate::fetch_ws::SharedNetBridge;
use crate::frames::{decode_structured_json, encode_structured_json, SharedFrames};
use crate::web_apis::{
    arg, bind_ctor_proto, create_dom_exception, define_global, define_host_accessor,
    define_host_getter, define_host_method, origin_of_url, SharedWebApis,
};

// ---------------------------------------------------------------------------
// Global registries (process-wide SharedWorker & BroadcastChannel hubs)
// ---------------------------------------------------------------------------

static NEXT_GLOBAL_ID: AtomicU64 = AtomicU64::new(1);

fn next_global_id() -> u64 {
    NEXT_GLOBAL_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug)]
enum WorkerCommand {
    PostMessage(String),
    BroadcastMessage {
        channel_id: u64,
        origin: String,
        payload_json: String,
    },
    Terminate,
}

#[derive(Debug)]
enum SharedWorkerCommand {
    Connect {
        client_port_id: u64,
        client_tx: mpsc::UnboundedSender<AsyncWorkerEvent>,
    },
    PortMessage {
        client_port_id: u64,
        payload_json: String,
    },
    PortClose {
        client_port_id: u64,
    },
    BroadcastMessage {
        channel_id: u64,
        origin: String,
        payload_json: String,
    },
    #[allow(dead_code)]
    Terminate,
}

#[derive(Clone)]
struct SharedWorkerHandle {
    cmd_tx: std::sync::mpsc::Sender<SharedWorkerCommand>,
}

type SharedWorkerRegistry = HashMap<(String, String, String), SharedWorkerHandle>;
static SHARED_WORKERS: OnceLock<Mutex<SharedWorkerRegistry>> = OnceLock::new();

fn shared_workers_registry() -> &'static Mutex<SharedWorkerRegistry> {
    SHARED_WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone)]
enum BroadcastSink {
    Async(mpsc::UnboundedSender<AsyncWorkerEvent>),
    Dedicated(std::sync::mpsc::Sender<WorkerCommand>),
    Shared(std::sync::mpsc::Sender<SharedWorkerCommand>),
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
            Self::Dedicated(tx) => tx
                .send(WorkerCommand::BroadcastMessage {
                    channel_id,
                    origin,
                    payload_json,
                })
                .is_ok(),
            Self::Shared(tx) => tx
                .send(SharedWorkerCommand::BroadcastMessage {
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
/// BroadcastChannel peers, and streaming `EventSource` (SSE) tasks directly into
/// `ContentRuntime`'s `tokio::select!` loop (ZERO polling timers).
#[derive(Debug)]
pub enum AsyncWorkerEvent {
    DedicatedMessage {
        worker_id: u64,
        payload_json: String,
    },
    DedicatedError {
        worker_id: u64,
        message: String,
    },
    SharedPortMessage {
        port_id: u64,
        payload_json: String,
    },
    BridgePortMessage {
        port_id: rix_ipc::MessagePortId,
        payload: crate::structured_clone::StructuredClonePayload,
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
    cmd_tx: std::sync::mpsc::Sender<WorkerCommand>,
    terminated: bool,
}

struct SharedWorkerClientPort {
    js_port: HeapHandle,
    worker_tx: std::sync::mpsc::Sender<SharedWorkerCommand>,
    started: bool,
    closed: bool,
    onmessage: JsValue,
    pending: VecDeque<String>,
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
    shared_port_proto: Option<HeapHandle>,
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
    shared_ports_by_id: HashMap<u64, SharedWorkerClientPort>,
    shared_port_handle_to_id: HashMap<HeapHandle, u64>,
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
            shared_ports_by_id: HashMap::new(),
            shared_port_handle_to_id: HashMap::new(),
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
            || self.shared_port_handle_to_id.contains_key(&h)
            || self.broadcast_handle_to_id.contains_key(&h)
            || self.sse_handle_to_id.contains_key(&h)
    }

    /// Collects GC roots for active workers, ports, channels, SSE handles, and prototypes.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.protos.worker_proto,
            self.protos.shared_worker_proto,
            self.protos.shared_port_proto,
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
        for p in self.shared_ports_by_id.values() {
            if !p.closed {
                out.push(JsValue::from_object(p.js_port));
                out.push(p.onmessage);
            }
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
        self.shared_ports_by_id
            .retain(|_, p| !p.closed || heap.is_alive(p.js_port));
        self.shared_port_handle_to_id
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
        for (&port_id, p) in &mut self.shared_ports_by_id {
            if !p.closed {
                p.closed = true;
                let _ = p.worker_tx.send(SharedWorkerCommand::PortClose {
                    client_port_id: port_id,
                });
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
// Script resolution (`blob:`, `data:`, `http(s):`)
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum ScriptSource {
    Inline(String),
    Remote {
        url: String,
        origin: String,
        fetch_engine: Arc<FetchEngine>,
    },
}

fn resolve_script_url(raw: &str, base_href: &str) -> Result<String, String> {
    if let Ok(u) = Url::parse(raw) {
        return Ok(u.to_string());
    }
    let base = Url::parse(base_href).map_err(|e| format!("invalid base URL: {e}"))?;
    base.join(raw)
        .map(|u| u.to_string())
        .map_err(|e| format!("invalid script URL '{raw}': {e}"))
}

fn prepare_script_source(
    resolved_url: &str,
    origin: &str,
    object_urls: &HashMap<String, Vec<u8>>,
    fetch_engine: Arc<FetchEngine>,
) -> Result<ScriptSource, String> {
    if resolved_url.starts_with("blob:") {
        let bytes = object_urls
            .get(resolved_url)
            .ok_or_else(|| format!("unknown blob URL '{resolved_url}'"))?;
        return Ok(ScriptSource::Inline(
            String::from_utf8_lossy(bytes).into_owned(),
        ));
    }
    if let Some(rest) = resolved_url.strip_prefix("data:") {
        let decoded = decode_data_url(rest)?;
        return Ok(ScriptSource::Inline(decoded));
    }
    if resolved_url.starts_with("http://") || resolved_url.starts_with("https://") {
        return Ok(ScriptSource::Remote {
            url: resolved_url.to_string(),
            origin: origin.to_string(),
            fetch_engine,
        });
    }
    Err(format!("unsupported worker script URL '{resolved_url}'"))
}

fn load_script_on_worker_thread(
    src: &ScriptSource,
    rt: &tokio::runtime::Runtime,
) -> Result<String, String> {
    match src {
        ScriptSource::Inline(code) => Ok(code.clone()),
        ScriptSource::Remote {
            url,
            origin,
            fetch_engine,
        } => {
            let req = FetchRequest {
                url: url.clone(),
                method: "GET".to_string(),
                headers: HashMap::new(),
                body: Vec::new(),
                mode: FetchMode::SameOrigin,
                credentials: FetchCredentials::SameOrigin,
                cache: rix_net::FetchCache::Default,
                redirect: FetchRedirect::Follow,
                origin: origin.clone(),
                abort_signal: None,
            };
            let resp = rt
                .block_on(fetch_engine.fetch(req))
                .map_err(|e| format!("failed to fetch worker script '{url}': {e}"))?;
            if !(200..300).contains(&resp.status) {
                return Err(format!(
                    "worker script '{url}' returned HTTP status {}",
                    resp.status
                ));
            }
            Ok(String::from_utf8_lossy(&resp.body).into_owned())
        }
    }
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
// Isolated Worker JS VM helpers (Dedicated & Shared workers)
// ---------------------------------------------------------------------------

struct WorkerListenerTable {
    listeners: HashMap<(HeapHandle, String), Vec<HeapHandle>>,
}

impl WorkerListenerTable {
    fn new() -> Self {
        Self {
            listeners: HashMap::new(),
        }
    }

    fn add(&mut self, target: HeapHandle, ev_type: String, cb: HeapHandle) {
        let list = self.listeners.entry((target, ev_type)).or_default();
        if !list.contains(&cb) {
            list.push(cb);
        }
    }

    fn remove(&mut self, target: HeapHandle, ev_type: &str, cb: HeapHandle) {
        if let Some(list) = self.listeners.get_mut(&(target, ev_type.to_string())) {
            list.retain(|&h| h != cb);
        }
    }

    fn snapshot(&self, target: HeapHandle, ev_type: &str) -> Vec<HeapHandle> {
        self.listeners
            .get(&(target, ev_type.to_string()))
            .cloned()
            .unwrap_or_default()
    }
}

fn dispatch_worker_event(
    vm: &mut JsVm,
    listeners: &Rc<RefCell<WorkerListenerTable>>,
    target: HeapHandle,
    ev_type: &str,
    props: &[(&str, JsValue)],
) {
    let ev_h = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    let type_k = vm.heap.intern_key("type");
    let type_v = JsValue::from_string(vm.heap.alloc_string(ev_type));
    vm.heap
        .object_define_data(ev_h, type_k, type_v, PropertyAttrs::DEFAULT, &mut vm.shapes);
    for &(name, val) in props {
        let k = vm.heap.intern_key(name);
        vm.heap
            .object_define_data(ev_h, k, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }
    let handler_key = vm.heap.intern_key(&format!("on{ev_type}"));
    if let Ok(handler_val) = vm.get_property_by_key(target, handler_key) {
        if let Some(fh) = handler_val
            .as_object()
            .filter(|&h| vm.is_callable_handle(h))
        {
            let _ = vm.call_value(
                JsValue::from_object(fh),
                JsValue::from_object(target),
                &[JsValue::from_object(ev_h)],
            );
        }
    }
    let cbs = listeners.borrow().snapshot(target, ev_type);
    for cb in cbs {
        let _ = vm.call_value(
            JsValue::from_object(cb),
            JsValue::from_object(target),
            &[JsValue::from_object(ev_h)],
        );
    }
}

fn install_worker_event_target_methods(
    vm: &mut JsVm,
    obj: HeapHandle,
    listeners: &Rc<RefCell<WorkerListenerTable>>,
) {
    {
        let lt = Rc::clone(listeners);
        define_host_method(vm, obj, "addEventListener", move |vm, this_val, args| {
            let target = this_val.as_object().unwrap_or(vm.global);
            let ev_type = vm.value_to_rust_string(arg(args, 0));
            if let Some(cb) = arg(args, 1)
                .as_object()
                .filter(|&h| vm.is_callable_handle(h))
            {
                lt.borrow_mut().add(target, ev_type, cb);
            }
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let lt = Rc::clone(listeners);
        define_host_method(vm, obj, "removeEventListener", move |vm, this_val, args| {
            let target = this_val.as_object().unwrap_or(vm.global);
            let ev_type = vm.value_to_rust_string(arg(args, 0));
            if let Some(cb) = arg(args, 1).as_object() {
                lt.borrow_mut().remove(target, &ev_type, cb);
            }
            Ok(JsValue::UNDEFINED)
        });
    }
}

struct WorkerBroadcastTable {
    by_id: HashMap<u64, (HeapHandle, String, String, bool)>,
    handle_to_id: HashMap<HeapHandle, u64>,
    blobs: HashMap<HeapHandle, (Vec<u8>, String)>,
}

impl WorkerBroadcastTable {
    fn new() -> Self {
        Self {
            by_id: HashMap::new(),
            handle_to_id: HashMap::new(),
            blobs: HashMap::new(),
        }
    }

    fn unregister_all(&mut self) {
        for (id, (_, origin, name, closed)) in self.by_id.iter_mut() {
            if !*closed {
                *closed = true;
                unregister_broadcast_subscriber(origin, name, *id);
            }
        }
    }
}

fn encode_worker_json(
    vm: &mut JsVm,
    blobs: &HashMap<HeapHandle, (Vec<u8>, String)>,
    val: JsValue,
    seen: &mut HashSet<HeapHandle>,
) -> Result<serde_json::Value, rix_js::JsException> {
    if let Some(h) = val.as_object() {
        if let Some((bytes, mime)) = blobs.get(&h) {
            return Ok(serde_json::json!({
                "$t": "blob",
                "m": mime,
                "v": bytes,
            }));
        }
        // Recurse into plain objects/arrays to catch nested Blobs (e.g. `{ blob: new Blob(...) }`).
        if matches!(
            vm.heap.get_object(h).map(|o| &o.kind),
            Some(ObjectKind::Ordinary)
        ) && !vm.is_callable_handle(h)
        {
            if !seen.insert(h) {
                return Err(create_dom_exception(
                    vm,
                    "DataCloneError",
                    "Cyclic structure",
                ));
            }
            let keys = vm.own_enumerable_keys_including_elements(h);
            let mut map = serde_json::Map::with_capacity(keys.len());
            for name in keys {
                let k = vm.heap.intern_key(&name);
                let v = vm.get_property_by_key(h, k)?;
                map.insert(name, encode_worker_json(vm, blobs, v, seen)?);
            }
            seen.remove(&h);
            return Ok(serde_json::Value::Object(map));
        }
    }
    encode_structured_json(vm, val, seen)
}

#[allow(clippy::too_many_arguments)]
fn install_common_worker_globals(
    vm: &mut JsVm,
    event_loop: &SharedEventLoop,
    listeners: &Rc<RefCell<WorkerListenerTable>>,
    bc_table: &Rc<RefCell<WorkerBroadcastTable>>,
    bc_sink: BroadcastSink,
    script_url: &str,
    origin: &str,
    worker_name: &str,
    object_urls: Arc<HashMap<String, Vec<u8>>>,
    fetch_engine: Arc<FetchEngine>,
    closed_flag: Rc<RefCell<bool>>,
    rt: Rc<tokio::runtime::Runtime>,
) {
    install_event_loop_globals(vm, event_loop);
    let global = vm.global;
    define_global(vm, "self", JsValue::from_object(global));
    let name_js = JsValue::from_string(vm.heap.alloc_string(worker_name));
    define_global(vm, "name", name_js);

    // WorkerLocation (`self.location`)
    let loc_h = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    let parsed = Url::parse(script_url).ok();
    let loc_origin = parsed
        .as_ref()
        .map(origin_of_url)
        .unwrap_or_else(|| origin.to_string());
    let pathname = parsed
        .as_ref()
        .map_or_else(|| "/".to_string(), |u| u.path().to_string());
    let protocol = parsed
        .as_ref()
        .map_or_else(|| "https:".to_string(), |u| format!("{}:", u.scheme()));
    let hostname = parsed
        .as_ref()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let port_str = parsed
        .as_ref()
        .and_then(Url::port)
        .map(|p| p.to_string())
        .unwrap_or_default();
    let host = if port_str.is_empty() {
        hostname.clone()
    } else {
        format!("{hostname}:{port_str}")
    };
    let search = parsed
        .as_ref()
        .and_then(Url::query)
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let hash = parsed
        .as_ref()
        .and_then(Url::fragment)
        .map(|f| format!("#{f}"))
        .unwrap_or_default();
    for (k, v) in [
        ("href", script_url.to_string()),
        ("origin", loc_origin),
        ("pathname", pathname),
        ("protocol", protocol),
        ("hostname", hostname),
        ("port", port_str),
        ("host", host),
        ("search", search),
        ("hash", hash),
    ] {
        let key = vm.heap.intern_key(k);
        let val = JsValue::from_string(vm.heap.alloc_string(&v));
        vm.heap
            .object_define_data(loc_h, key, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }
    define_global(vm, "location", JsValue::from_object(loc_h));

    // Worker-side `Blob` constructor (supports `new Blob(parts, opts)` + BroadcastChannel transfer).
    {
        let blob_proto = vm.alloc_ordinary_object();
        let bct_s = Rc::clone(bc_table);
        define_host_getter(vm, blob_proto, "size", move |_, this_val, _| {
            let sz = this_val
                .as_object()
                .and_then(|h| bct_s.borrow().blobs.get(&h).map(|(b, _)| b.len()))
                .unwrap_or(0);
            Ok(JsValue::from_f64(sz as f64))
        });
        let bct_t = Rc::clone(bc_table);
        define_host_getter(vm, blob_proto, "type", move |vm, this_val, _| {
            let ty = this_val
                .as_object()
                .and_then(|h| bct_t.borrow().blobs.get(&h).map(|(_, m)| m.clone()))
                .unwrap_or_default();
            Ok(JsValue::from_string(vm.heap.alloc_string(&ty)))
        });
        let bct_c = Rc::clone(bc_table);
        let blob_ctor = vm.alloc_host_fn("Blob", move |vm, _, args| {
            let mut bytes = Vec::new();
            let parts = arg(args, 0);
            if parts.as_object().is_some() {
                if let Ok(items) = vm.collect_iterable(parts) {
                    for it in items {
                        if let Some(bh) = it.as_object() {
                            if let Some((b, _)) = bct_c.borrow().blobs.get(&bh) {
                                bytes.extend_from_slice(b);
                                continue;
                            }
                        }
                        bytes.extend_from_slice(vm.value_to_rust_string(it).as_bytes());
                    }
                }
            }
            let mime = arg(args, 1)
                .as_object()
                .and_then(|oh| {
                    let kt = vm.heap.intern_key("type");
                    vm.get_property_by_key(oh, kt).ok()
                })
                .filter(|v| !v.is_undefined())
                .map(|v| vm.value_to_rust_string(v).to_ascii_lowercase())
                .unwrap_or_default();
            let h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(blob_proto)));
            bct_c.borrow_mut().blobs.insert(h, (bytes, mime));
            Ok(JsValue::from_object(h))
        });
        bind_ctor_proto(vm, blob_ctor, blob_proto);
        define_global(vm, "Blob", JsValue::from_object(blob_ctor));
    }

    // Worker-side `BroadcastChannel` connected directly to the process-wide `broadcast_hub()`!
    {
        let bc_proto = vm.alloc_ordinary_object();
        install_worker_event_target_methods(vm, bc_proto, listeners);
        let bct_n = Rc::clone(bc_table);
        define_host_getter(vm, bc_proto, "name", move |vm, this_val, _| {
            let n = this_val
                .as_object()
                .and_then(|h| bct_n.borrow().handle_to_id.get(&h).copied())
                .and_then(|id| bct_n.borrow().by_id.get(&id).map(|e| e.2.clone()))
                .unwrap_or_default();
            Ok(JsValue::from_string(vm.heap.alloc_string(&n)))
        });
        let bct_p = Rc::clone(bc_table);
        define_host_method(vm, bc_proto, "postMessage", move |vm, this_val, args| {
            if args.is_empty() {
                return Err(vm.type_error("BroadcastChannel.postMessage requires 1 argument"));
            }
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Incompatible BroadcastChannel receiver"));
            };
            let info = {
                let tbl = bct_p.borrow();
                tbl.handle_to_id.get(&h).and_then(|id| {
                    tbl.by_id
                        .get(id)
                        .map(|e| (*id, e.1.clone(), e.2.clone(), e.3))
                })
            };
            let Some((sender_id, bc_origin, bc_name, closed)) = info else {
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
            let encoded = {
                let blobs = bct_p.borrow().blobs.clone();
                encode_worker_json(vm, &blobs, arg(args, 0), &mut seen)?
            };
            let payload_json = serde_json::to_string(&encoded).unwrap_or_else(|_| "null".into());
            broadcast_to_peers(sender_id, &bc_origin, &bc_name, &payload_json);
            Ok(JsValue::UNDEFINED)
        });
        let bct_cl = Rc::clone(bc_table);
        define_host_method(vm, bc_proto, "close", move |_, this_val, _| {
            if let Some(h) = this_val.as_object() {
                let mut tbl = bct_cl.borrow_mut();
                if let Some(&id) = tbl.handle_to_id.get(&h) {
                    if let Some((_, o, n, closed)) = tbl.by_id.get_mut(&id) {
                        if !*closed {
                            *closed = true;
                            unregister_broadcast_subscriber(o, n, id);
                        }
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        });
        let bct_c = Rc::clone(bc_table);
        let bc_orig = origin.to_string();
        let bc_ctor = vm.alloc_host_fn("BroadcastChannel", move |vm, _, args| {
            if vm.native_new_target.is_undefined() {
                return Err(vm.type_error("BroadcastChannel constructor requires 'new'"));
            }
            if args.is_empty() {
                return Err(vm.type_error("BroadcastChannel constructor requires 1 argument"));
            }
            let name = vm.to_js_string(arg(args, 0))?;
            let id = next_global_id();
            let obj_h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(bc_proto)));
            let k_om = vm.heap.intern_key("onmessage");
            let k_ome = vm.heap.intern_key("onmessageerror");
            vm.heap.object_define_data(
                obj_h,
                k_om,
                JsValue::NULL,
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
            vm.heap.object_define_data(
                obj_h,
                k_ome,
                JsValue::NULL,
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
            if let Ok(mut hub) = broadcast_hub().lock() {
                hub.entry((bc_orig.clone(), name.clone()))
                    .or_default()
                    .push(BroadcastSubscriber {
                        id,
                        sink: bc_sink.clone(),
                    });
            }
            let mut tbl = bct_c.borrow_mut();
            tbl.by_id.insert(id, (obj_h, bc_orig.clone(), name, false));
            tbl.handle_to_id.insert(obj_h, id);
            Ok(JsValue::from_object(obj_h))
        });
        bind_ctor_proto(vm, bc_ctor, bc_proto);
        define_global(vm, "BroadcastChannel", JsValue::from_object(bc_ctor));
    }

    // `structuredClone(value)`
    define_host_method(vm, global, "structuredClone", |vm, _, args| {
        let mut seen = HashSet::new();
        let encoded = encode_structured_json(vm, arg(args, 0), &mut seen)?;
        Ok(decode_structured_json(vm, &encoded))
    });

    // Minimal `console`
    let console_h = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    for method in ["log", "info", "warn", "error", "debug"] {
        define_host_method(vm, console_h, method, |_, _, _| Ok(JsValue::UNDEFINED));
    }
    define_global(vm, "console", JsValue::from_object(console_h));

    // `self.close()`
    {
        let closed = Rc::clone(&closed_flag);
        define_host_method(vm, global, "close", move |_, _, _| {
            *closed.borrow_mut() = true;
            Ok(JsValue::UNDEFINED)
        });
    }

    // `importScripts(...urls)`
    {
        let base_href = script_url.to_string();
        let origin_owned = origin.to_string();
        let blobs = Arc::clone(&object_urls);
        let fe = Arc::clone(&fetch_engine);
        let rt_ref = Rc::clone(&rt);
        define_host_method(vm, global, "importScripts", move |vm, _, args| {
            for &val in args {
                let raw = vm.value_to_rust_string(val);
                let resolved = resolve_script_url(&raw, &base_href)
                    .map_err(|m| create_dom_exception(vm, "SyntaxError", &m))?;
                let src = prepare_script_source(&resolved, &origin_owned, &blobs, Arc::clone(&fe))
                    .map_err(|m| create_dom_exception(vm, "NetworkError", &m))?;
                let code = load_script_on_worker_thread(&src, &rt_ref)
                    .map_err(|m| create_dom_exception(vm, "NetworkError", &m))?;
                vm.eval_script(&code).map_err(|e| match e {
                    rix_js::EvalError::Runtime(ex) => ex,
                    other => create_dom_exception(vm, "SyntaxError", &other.to_string()),
                })?;
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    install_worker_event_target_methods(vm, global, listeners);
}

#[allow(clippy::too_many_arguments)]
fn run_dedicated_worker_thread(
    worker_id: u64,
    script_url: String,
    origin: String,
    worker_name: String,
    script_src: ScriptSource,
    object_urls: Arc<HashMap<String, Vec<u8>>>,
    fetch_engine: Arc<FetchEngine>,
    parent_tx: mpsc::UnboundedSender<AsyncWorkerEvent>,
    cmd_tx_self: std::sync::mpsc::Sender<WorkerCommand>,
    cmd_rx: std::sync::mpsc::Receiver<WorkerCommand>,
) {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        let _ = parent_tx.send(AsyncWorkerEvent::DedicatedError {
            worker_id,
            message: "failed to initialize worker async runtime".to_string(),
        });
        return;
    };
    let rt = Rc::new(rt);

    let code = match load_script_on_worker_thread(&script_src, &rt) {
        Ok(c) => c,
        Err(err) => {
            let _ = parent_tx.send(AsyncWorkerEvent::DedicatedError {
                worker_id,
                message: err,
            });
            return;
        }
    };

    let mut vm = JsVm::new();
    let _jit = rix_jit::TieredJit::attach_to(&mut vm);
    let event_loop = Rc::new(RefCell::new(EventLoopState::new()));
    let listeners = Rc::new(RefCell::new(WorkerListenerTable::new()));
    let bc_table = Rc::new(RefCell::new(WorkerBroadcastTable::new()));
    let closed_flag = Rc::new(RefCell::new(false));

    install_common_worker_globals(
        &mut vm,
        &event_loop,
        &listeners,
        &bc_table,
        BroadcastSink::Dedicated(cmd_tx_self),
        &script_url,
        &origin,
        &worker_name,
        object_urls,
        fetch_engine,
        Rc::clone(&closed_flag),
        Rc::clone(&rt),
    );

    // `self.postMessage(data)`
    {
        let ptx = parent_tx.clone();
        let g = vm.global;
        define_host_method(&mut vm, g, "postMessage", move |vm, _, args| {
            let mut seen = HashSet::new();
            let encoded = encode_structured_json(vm, arg(args, 0), &mut seen)?;
            let payload_json = serde_json::to_string(&encoded).unwrap_or_else(|_| "null".into());
            let _ = ptx.send(AsyncWorkerEvent::DedicatedMessage {
                worker_id,
                payload_json,
            });
            Ok(JsValue::UNDEFINED)
        });
    }

    if let Err(err) = vm.eval_script(&code) {
        let _ = parent_tx.send(AsyncWorkerEvent::DedicatedError {
            worker_id,
            message: err.to_string(),
        });
    }
    run_microtask_checkpoint(&event_loop, &mut vm);
    drain_ready_tasks(&event_loop, &mut vm);

    while !*closed_flag.borrow() {
        let next_timer = event_loop.borrow_mut().time_until_next_timer();
        let recv_res = match next_timer {
            Some(dur) => cmd_rx.recv_timeout(dur),
            None => cmd_rx
                .recv()
                .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected),
        };
        match recv_res {
            Ok(WorkerCommand::PostMessage(payload_json)) => {
                let data = serde_json::from_str::<serde_json::Value>(&payload_json)
                    .map(|jv| decode_structured_json(&mut vm, &jv))
                    .unwrap_or(JsValue::NULL);
                let empty_s = JsValue::from_string(vm.heap.alloc_string(""));
                let ports_arr = alloc_js_array(&mut vm, &[]);
                let g = vm.global;
                dispatch_worker_event(
                    &mut vm,
                    &listeners,
                    g,
                    "message",
                    &[
                        ("data", data),
                        ("origin", empty_s),
                        ("lastEventId", empty_s),
                        ("ports", JsValue::from_object(ports_arr)),
                    ],
                );
                run_microtask_checkpoint(&event_loop, &mut vm);
                drain_ready_tasks(&event_loop, &mut vm);
            }
            Ok(WorkerCommand::BroadcastMessage {
                channel_id,
                origin: msg_origin,
                payload_json,
            }) => {
                let target = bc_table
                    .borrow()
                    .by_id
                    .get(&channel_id)
                    .filter(|e| !e.3)
                    .map(|e| e.0);
                if let Some(bc_obj) = target {
                    let data = serde_json::from_str::<serde_json::Value>(&payload_json)
                        .map(|jv| decode_structured_json(&mut vm, &jv))
                        .unwrap_or(JsValue::NULL);
                    let orig_js = JsValue::from_string(vm.heap.alloc_string(&msg_origin));
                    let empty_s = JsValue::from_string(vm.heap.alloc_string(""));
                    let ports_arr = alloc_js_array(&mut vm, &[]);
                    dispatch_worker_event(
                        &mut vm,
                        &listeners,
                        bc_obj,
                        "message",
                        &[
                            ("data", data),
                            ("origin", orig_js),
                            ("lastEventId", empty_s),
                            ("source", JsValue::NULL),
                            ("ports", JsValue::from_object(ports_arr)),
                        ],
                    );
                    run_microtask_checkpoint(&event_loop, &mut vm);
                    drain_ready_tasks(&event_loop, &mut vm);
                }
            }
            Ok(WorkerCommand::Terminate) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                drain_ready_tasks(&event_loop, &mut vm);
            }
        }
    }
    bc_table.borrow_mut().unregister_all();
}

struct SharedWorkerSidePort {
    js_obj: HeapHandle,
    client_tx: mpsc::UnboundedSender<AsyncWorkerEvent>,
    started: bool,
    closed: bool,
    onmessage: JsValue,
    pending: VecDeque<String>,
}

#[allow(clippy::too_many_arguments)]
fn run_shared_worker_thread(
    script_url: String,
    origin: String,
    worker_name: String,
    script_src: ScriptSource,
    object_urls: Arc<HashMap<String, Vec<u8>>>,
    fetch_engine: Arc<FetchEngine>,
    cmd_tx_self: std::sync::mpsc::Sender<SharedWorkerCommand>,
    cmd_rx: std::sync::mpsc::Receiver<SharedWorkerCommand>,
) {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let rt = Rc::new(rt);
    let Ok(code) = load_script_on_worker_thread(&script_src, &rt) else {
        return;
    };

    let mut vm = JsVm::new();
    let _jit = rix_jit::TieredJit::attach_to(&mut vm);
    let event_loop = Rc::new(RefCell::new(EventLoopState::new()));
    let listeners = Rc::new(RefCell::new(WorkerListenerTable::new()));
    let bc_table = Rc::new(RefCell::new(WorkerBroadcastTable::new()));
    let closed_flag = Rc::new(RefCell::new(false));
    let ports: Rc<RefCell<HashMap<u64, SharedWorkerSidePort>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let port_handle_to_id: Rc<RefCell<HashMap<HeapHandle, u64>>> =
        Rc::new(RefCell::new(HashMap::new()));

    install_common_worker_globals(
        &mut vm,
        &event_loop,
        &listeners,
        &bc_table,
        BroadcastSink::Shared(cmd_tx_self),
        &script_url,
        &origin,
        &worker_name,
        object_urls,
        fetch_engine,
        Rc::clone(&closed_flag),
        Rc::clone(&rt),
    );

    // Worker-side MessagePort prototype
    let port_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    install_worker_event_target_methods(&mut vm, port_proto, &listeners);
    {
        let ps = Rc::clone(&ports);
        let pmap = Rc::clone(&port_handle_to_id);
        define_host_method(
            &mut vm,
            port_proto,
            "postMessage",
            move |vm, this_val, args| {
                let Some(h) = this_val.as_object() else {
                    return Ok(JsValue::UNDEFINED);
                };
                let Some(&pid) = pmap.borrow().get(&h) else {
                    return Ok(JsValue::UNDEFINED);
                };
                let mut seen = HashSet::new();
                let encoded = encode_structured_json(vm, arg(args, 0), &mut seen)?;
                let payload_json =
                    serde_json::to_string(&encoded).unwrap_or_else(|_| "null".into());
                if let Some(port) = ps.borrow().get(&pid) {
                    if !port.closed {
                        let _ = port.client_tx.send(AsyncWorkerEvent::SharedPortMessage {
                            port_id: pid,
                            payload_json,
                        });
                    }
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }
    {
        let ps = Rc::clone(&ports);
        let pmap = Rc::clone(&port_handle_to_id);
        let lt = Rc::clone(&listeners);
        define_host_method(&mut vm, port_proto, "start", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Ok(JsValue::UNDEFINED);
            };
            let Some(&pid) = pmap.borrow().get(&h) else {
                return Ok(JsValue::UNDEFINED);
            };
            flush_shared_worker_side_port(vm, &ps, &lt, pid, true);
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let ps = Rc::clone(&ports);
        let pmap = Rc::clone(&port_handle_to_id);
        define_host_method(&mut vm, port_proto, "close", move |_, this_val, _| {
            if let Some(h) = this_val.as_object() {
                if let Some(&pid) = pmap.borrow().get(&h) {
                    if let Some(port) = ps.borrow_mut().get_mut(&pid) {
                        port.closed = true;
                        port.pending.clear();
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let ps_g = Rc::clone(&ports);
        let pmap_g = Rc::clone(&port_handle_to_id);
        let ps_s = Rc::clone(&ports);
        let pmap_s = Rc::clone(&port_handle_to_id);
        let lt_s = Rc::clone(&listeners);
        define_host_accessor(
            &mut vm,
            port_proto,
            "onmessage",
            move |_, this_val, _| {
                let val = this_val
                    .as_object()
                    .and_then(|h| pmap_g.borrow().get(&h).copied())
                    .and_then(|pid| ps_g.borrow().get(&pid).map(|p| p.onmessage))
                    .unwrap_or(JsValue::NULL);
                Ok(val)
            },
            move |vm, this_val, args| {
                let Some(h) = this_val.as_object() else {
                    return Ok(JsValue::UNDEFINED);
                };
                let Some(&pid) = pmap_s.borrow().get(&h) else {
                    return Ok(JsValue::UNDEFINED);
                };
                let handler = arg(args, 0);
                if let Some(port) = ps_s.borrow_mut().get_mut(&pid) {
                    port.onmessage = handler;
                }
                flush_shared_worker_side_port(vm, &ps_s, &lt_s, pid, true);
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    let _ = vm.eval_script(&code);
    run_microtask_checkpoint(&event_loop, &mut vm);
    drain_ready_tasks(&event_loop, &mut vm);

    while !*closed_flag.borrow() {
        let next_timer = event_loop.borrow_mut().time_until_next_timer();
        let recv_res = match next_timer {
            Some(dur) => cmd_rx.recv_timeout(dur),
            None => cmd_rx
                .recv()
                .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected),
        };
        match recv_res {
            Ok(SharedWorkerCommand::Connect {
                client_port_id,
                client_tx,
            }) => {
                let port_h = vm
                    .heap
                    .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(port_proto)));
                ports.borrow_mut().insert(
                    client_port_id,
                    SharedWorkerSidePort {
                        js_obj: port_h,
                        client_tx,
                        started: false,
                        closed: false,
                        onmessage: JsValue::NULL,
                        pending: VecDeque::new(),
                    },
                );
                port_handle_to_id
                    .borrow_mut()
                    .insert(port_h, client_port_id);

                let ports_arr = alloc_js_array(&mut vm, &[JsValue::from_object(port_h)]);
                let empty_s = JsValue::from_string(vm.heap.alloc_string(""));
                let g = vm.global;
                dispatch_worker_event(
                    &mut vm,
                    &listeners,
                    g,
                    "connect",
                    &[
                        ("data", empty_s),
                        ("source", JsValue::from_object(port_h)),
                        ("ports", JsValue::from_object(ports_arr)),
                    ],
                );
                run_microtask_checkpoint(&event_loop, &mut vm);
                drain_ready_tasks(&event_loop, &mut vm);
            }
            Ok(SharedWorkerCommand::PortMessage {
                client_port_id,
                payload_json,
            }) => {
                let started = {
                    let mut map = ports.borrow_mut();
                    match map.get_mut(&client_port_id) {
                        Some(p) if !p.closed => {
                            p.pending.push_back(payload_json);
                            p.started
                        }
                        _ => false,
                    }
                };
                if started {
                    flush_shared_worker_side_port(
                        &mut vm,
                        &ports,
                        &listeners,
                        client_port_id,
                        false,
                    );
                    run_microtask_checkpoint(&event_loop, &mut vm);
                    drain_ready_tasks(&event_loop, &mut vm);
                }
            }
            Ok(SharedWorkerCommand::PortClose { client_port_id }) => {
                if let Some(p) = ports.borrow_mut().remove(&client_port_id) {
                    port_handle_to_id.borrow_mut().remove(&p.js_obj);
                }
            }
            Ok(SharedWorkerCommand::BroadcastMessage {
                channel_id,
                origin: msg_origin,
                payload_json,
            }) => {
                let target = bc_table
                    .borrow()
                    .by_id
                    .get(&channel_id)
                    .filter(|e| !e.3)
                    .map(|e| e.0);
                if let Some(bc_obj) = target {
                    let data = serde_json::from_str::<serde_json::Value>(&payload_json)
                        .map(|jv| decode_structured_json(&mut vm, &jv))
                        .unwrap_or(JsValue::NULL);
                    let orig_js = JsValue::from_string(vm.heap.alloc_string(&msg_origin));
                    let empty_s = JsValue::from_string(vm.heap.alloc_string(""));
                    let ports_arr = alloc_js_array(&mut vm, &[]);
                    dispatch_worker_event(
                        &mut vm,
                        &listeners,
                        bc_obj,
                        "message",
                        &[
                            ("data", data),
                            ("origin", orig_js),
                            ("lastEventId", empty_s),
                            ("source", JsValue::NULL),
                            ("ports", JsValue::from_object(ports_arr)),
                        ],
                    );
                    run_microtask_checkpoint(&event_loop, &mut vm);
                    drain_ready_tasks(&event_loop, &mut vm);
                }
            }
            Ok(SharedWorkerCommand::Terminate)
            | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                drain_ready_tasks(&event_loop, &mut vm);
            }
        }
    }
    bc_table.borrow_mut().unregister_all();
}

fn flush_shared_worker_side_port(
    vm: &mut JsVm,
    ports: &Rc<RefCell<HashMap<u64, SharedWorkerSidePort>>>,
    listeners: &Rc<RefCell<WorkerListenerTable>>,
    port_id: u64,
    mark_started: bool,
) {
    loop {
        let next = {
            let mut map = ports.borrow_mut();
            let Some(port) = map.get_mut(&port_id) else {
                break;
            };
            if mark_started {
                port.started = true;
            }
            if port.closed || !port.started {
                break;
            }
            port.pending
                .pop_front()
                .map(|payload| (port.js_obj, payload))
        };
        let Some((port_h, payload_json)) = next else {
            break;
        };
        let data = serde_json::from_str::<serde_json::Value>(&payload_json)
            .map(|jv| decode_structured_json(vm, &jv))
            .unwrap_or(JsValue::NULL);
        let empty_s = JsValue::from_string(vm.heap.alloc_string(""));
        let ports_arr = alloc_js_array(vm, &[]);
        dispatch_worker_event(
            vm,
            listeners,
            port_h,
            "message",
            &[
                ("data", data),
                ("origin", empty_s),
                ("lastEventId", empty_s),
                ("ports", JsValue::from_object(ports_arr)),
            ],
        );
    }
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
// Delivery into `ContentRuntime` event loop
// ---------------------------------------------------------------------------

/// Delivers one reactive [`AsyncWorkerEvent`] into `ContentRuntime`'s JS VM,
/// followed by a strict WHATWG microtask checkpoint.
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
        AsyncWorkerEvent::DedicatedMessage {
            worker_id,
            payload_json,
        } => {
            let target = {
                let ws = workers.borrow();
                ws.workers_by_id
                    .get(&worker_id)
                    .filter(|w| !w.terminated)
                    .map(|w| w.js_obj)
            };
            let Some(target) = target else { return };
            let data = serde_json::from_str::<serde_json::Value>(&payload_json)
                .map(|jv| decode_structured_json(vm, &jv))
                .unwrap_or(JsValue::NULL);
            let empty_s = JsValue::from_string(vm.heap.alloc_string(""));
            let ports_arr = alloc_js_array(vm, &[]);
            fire_object_event_with_proto(
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
            );
        }
        AsyncWorkerEvent::DedicatedError { worker_id, message } => {
            let target = {
                let ws = workers.borrow();
                ws.workers_by_id
                    .get(&worker_id)
                    .filter(|w| !w.terminated)
                    .map(|w| w.js_obj)
            };
            let Some(target) = target else { return };
            let msg_js = JsValue::from_string(vm.heap.alloc_string(&message));
            fire_object_event_with_proto(
                vm,
                host,
                events,
                event_loop,
                target,
                "error",
                base_event_proto,
                &[("message", msg_js)],
            );
        }
        AsyncWorkerEvent::SharedPortMessage {
            port_id,
            payload_json,
        } => {
            let should_flush = {
                let mut ws = workers.borrow_mut();
                match ws.shared_ports_by_id.get_mut(&port_id) {
                    Some(p) if !p.closed => {
                        p.pending.push_back(payload_json);
                        p.started
                    }
                    _ => false,
                }
            };
            if should_flush {
                flush_shared_client_port(
                    vm, host, events, event_loop, frames, workers, port_id, false,
                );
            }
        }
        AsyncWorkerEvent::BridgePortMessage { port_id, payload } => {
            let (nb_opt, c2d_opt) = {
                let fs = frames.borrow();
                (fs.net.clone(), fs.canvas2d.clone())
            };
            if let (Some(nb), Some(c2d)) = (nb_opt, c2d_opt) {
                crate::frames::deliver_cross_thread_port_message(
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
            crate::frames::freeze_js_array(vm, ports_arr);
            crate::events::fire_object_event_dispatch_only(
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

#[allow(clippy::too_many_arguments)]
fn flush_shared_client_port(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    frames: &SharedFrames,
    workers: &SharedWorkers,
    port_id: u64,
    mark_started: bool,
) {
    let msg_proto = frames
        .borrow()
        .message_event_proto
        .or_else(|| events.borrow().event_proto)
        .unwrap_or(vm.object_proto);
    loop {
        let next = {
            let mut ws = workers.borrow_mut();
            let Some(port) = ws.shared_ports_by_id.get_mut(&port_id) else {
                break;
            };
            if mark_started {
                port.started = true;
            }
            if port.closed || !port.started {
                break;
            }
            port.pending
                .pop_front()
                .map(|payload| (port.js_port, payload))
        };
        let Some((port_h, payload_json)) = next else {
            break;
        };
        let data = serde_json::from_str::<serde_json::Value>(&payload_json)
            .map(|jv| decode_structured_json(vm, &jv))
            .unwrap_or(JsValue::NULL);
        let empty_s = JsValue::from_string(vm.heap.alloc_string(""));
        let ports_arr = alloc_js_array(vm, &[]);
        fire_object_event_with_proto(
            vm,
            host,
            events,
            event_loop,
            port_h,
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
    }
}

// ---------------------------------------------------------------------------
// Installation onto `ContentRuntime`'s `JsVm`
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

    install_dedicated_worker(vm, event_target_proto, web_apis, net, &workers);
    install_shared_worker(
        vm,
        event_target_proto,
        host,
        events,
        event_loop,
        web_apis,
        net,
        frames,
        &workers,
    );
    install_broadcast_channel(vm, event_target_proto, events, web_apis, &workers);
    install_event_source(vm, event_target_proto, web_apis, net, &workers);

    workers
}

fn install_dedicated_worker(
    vm: &mut JsVm,
    event_target_proto: HeapHandle,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    workers: &SharedWorkers,
) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(event_target_proto),
    ));
    workers.borrow_mut().protos.worker_proto = Some(proto);

    {
        let ws = Rc::clone(workers);
        define_host_method(vm, proto, "postMessage", move |vm, this_val, args| {
            let Some(h) = this_val.as_object() else {
                return Ok(JsValue::UNDEFINED);
            };
            let mut seen = HashSet::new();
            let encoded = encode_structured_json(vm, arg(args, 0), &mut seen)?;
            let payload_json = serde_json::to_string(&encoded).unwrap_or_else(|_| "null".into());
            let map = ws.borrow();
            if let Some(&wid) = map.worker_handle_to_id.get(&h) {
                if let Some(entry) = map.workers_by_id.get(&wid) {
                    if !entry.terminated {
                        let _ = entry.cmd_tx.send(WorkerCommand::PostMessage(payload_json));
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let ws = Rc::clone(workers);
        define_host_method(vm, proto, "terminate", move |_, this_val, _| {
            if let Some(h) = this_val.as_object() {
                let mut map = ws.borrow_mut();
                if let Some(&wid) = map.worker_handle_to_id.get(&h) {
                    if let Some(entry) = map.workers_by_id.get_mut(&wid) {
                        if !entry.terminated {
                            entry.terminated = true;
                            let _ = entry.cmd_tx.send(WorkerCommand::Terminate);
                        }
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    let wa = Rc::clone(web_apis);
    let nb = Rc::clone(net);
    let ws = Rc::clone(workers);
    let ctor = vm.alloc_host_fn("Worker", move |vm, _, args| {
        let raw_url = vm.value_to_rust_string(arg(args, 0));
        let worker_name = if let Some(opts) = arg(args, 1).as_object() {
            let nk = vm.heap.intern_key("name");
            vm.get_property_by_key(opts, nk)
                .ok()
                .filter(|v| !v.is_undefined() && !v.is_null())
                .map(|v| vm.value_to_rust_string(v))
                .unwrap_or_default()
        } else {
            String::new()
        };

        let (base_href, origin, object_urls_snapshot) = {
            let state = wa.borrow();
            (
                state.current_href(),
                state.current_origin(),
                Arc::new(state.object_urls.clone()),
            )
        };
        let resolved_url = resolve_script_url(&raw_url, &base_href)
            .map_err(|m| create_dom_exception(vm, "SyntaxError", &m))?;
        let fetch_engine = Arc::clone(&nb.borrow().fetch_engine);
        let script_src = prepare_script_source(
            &resolved_url,
            &origin,
            &object_urls_snapshot,
            Arc::clone(&fetch_engine),
        )
        .map_err(|m| create_dom_exception(vm, "SecurityError", &m))?;

        let worker_id = next_global_id();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let cmd_tx_self = cmd_tx.clone();
        let parent_tx = ws.borrow().async_tx.clone();

        let _ = std::thread::Builder::new()
            .name(format!("rix-worker-{worker_id}"))
            .spawn(move || {
                run_dedicated_worker_thread(
                    worker_id,
                    resolved_url,
                    origin,
                    worker_name,
                    script_src,
                    object_urls_snapshot,
                    fetch_engine,
                    parent_tx,
                    cmd_tx_self,
                    cmd_rx,
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
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    frames: &SharedFrames,
    workers: &SharedWorkers,
) {
    let sw_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(event_target_proto),
    ));
    let port_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(event_target_proto),
    ));
    {
        let mut ws = workers.borrow_mut();
        ws.protos.shared_worker_proto = Some(sw_proto);
        ws.protos.shared_port_proto = Some(port_proto);
    }

    // SharedWorker client MessagePort methods
    {
        let ws = Rc::clone(workers);
        define_host_method(vm, port_proto, "postMessage", move |vm, this_val, args| {
            let Some(h) = this_val.as_object() else {
                return Ok(JsValue::UNDEFINED);
            };
            let mut seen = HashSet::new();
            let encoded = encode_structured_json(vm, arg(args, 0), &mut seen)?;
            let payload_json = serde_json::to_string(&encoded).unwrap_or_else(|_| "null".into());
            let map = ws.borrow();
            if let Some(&pid) = map.shared_port_handle_to_id.get(&h) {
                if let Some(port) = map.shared_ports_by_id.get(&pid) {
                    if !port.closed {
                        let _ = port.worker_tx.send(SharedWorkerCommand::PortMessage {
                            client_port_id: pid,
                            payload_json,
                        });
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let fr_r = Rc::clone(frames);
        let ws_r = Rc::clone(workers);
        define_host_method(vm, port_proto, "start", move |_, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Ok(JsValue::UNDEFINED);
            };
            let pid_opt = ws_r.borrow().shared_port_handle_to_id.get(&h).copied();
            if let Some(pid) = pid_opt {
                if let Some(port) = ws_r.borrow_mut().shared_ports_by_id.get_mut(&pid) {
                    port.started = true;
                }
                let h2 = Rc::clone(&h_r);
                let ev2 = Rc::clone(&ev_r);
                let el2 = Rc::clone(&el_r);
                let fr2 = Rc::clone(&fr_r);
                let ws2 = Rc::clone(&ws_r);
                el_r.borrow_mut()
                    .post_host_task(TaskPriority::Networking, move |vm| {
                        flush_shared_client_port(vm, &h2, &ev2, &el2, &fr2, &ws2, pid, false);
                    });
            }
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let ws = Rc::clone(workers);
        define_host_method(vm, port_proto, "close", move |_, this_val, _| {
            if let Some(h) = this_val.as_object() {
                let mut map = ws.borrow_mut();
                if let Some(&pid) = map.shared_port_handle_to_id.get(&h) {
                    if let Some(port) = map.shared_ports_by_id.get_mut(&pid) {
                        if !port.closed {
                            port.closed = true;
                            port.pending.clear();
                            let _ = port.worker_tx.send(SharedWorkerCommand::PortClose {
                                client_port_id: pid,
                            });
                        }
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let ws_g = Rc::clone(workers);
        let h_s = Rc::clone(host);
        let ev_s = Rc::clone(events);
        let el_s = Rc::clone(event_loop);
        let fr_s = Rc::clone(frames);
        let ws_s = Rc::clone(workers);
        define_host_accessor(
            vm,
            port_proto,
            "onmessage",
            move |_, this_val, _| {
                let val = this_val
                    .as_object()
                    .and_then(|h| ws_g.borrow().shared_port_handle_to_id.get(&h).copied())
                    .and_then(|pid| {
                        ws_g.borrow()
                            .shared_ports_by_id
                            .get(&pid)
                            .map(|p| p.onmessage)
                    })
                    .unwrap_or(JsValue::NULL);
                Ok(val)
            },
            move |_, this_val, args| {
                let Some(h) = this_val.as_object() else {
                    return Ok(JsValue::UNDEFINED);
                };
                let pid_opt = ws_s.borrow().shared_port_handle_to_id.get(&h).copied();
                if let Some(pid) = pid_opt {
                    let handler = arg(args, 0);
                    if let Some(port) = ws_s.borrow_mut().shared_ports_by_id.get_mut(&pid) {
                        port.onmessage = handler;
                        port.started = true;
                    }
                    let h2 = Rc::clone(&h_s);
                    let ev2 = Rc::clone(&ev_s);
                    let el2 = Rc::clone(&el_s);
                    let fr2 = Rc::clone(&fr_s);
                    let ws2 = Rc::clone(&ws_s);
                    el_s.borrow_mut()
                        .post_host_task(TaskPriority::Networking, move |vm| {
                            flush_shared_client_port(vm, &h2, &ev2, &el2, &fr2, &ws2, pid, false);
                        });
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    let wa = Rc::clone(web_apis);
    let nb = Rc::clone(net);
    let ws = Rc::clone(workers);
    let ctor = vm.alloc_host_fn("SharedWorker", move |vm, _, args| {
        let raw_url = vm.value_to_rust_string(arg(args, 0));
        let worker_name = if arg(args, 1).is_string() {
            vm.value_to_rust_string(arg(args, 1))
        } else if let Some(opts) = arg(args, 1).as_object() {
            let nk = vm.heap.intern_key("name");
            vm.get_property_by_key(opts, nk)
                .ok()
                .filter(|v| !v.is_undefined() && !v.is_null())
                .map(|v| vm.value_to_rust_string(v))
                .unwrap_or_default()
        } else {
            String::new()
        };

        let (base_href, origin, object_urls_snapshot) = {
            let state = wa.borrow();
            (
                state.current_href(),
                state.current_origin(),
                Arc::new(state.object_urls.clone()),
            )
        };
        let resolved_url = resolve_script_url(&raw_url, &base_href)
            .map_err(|m| create_dom_exception(vm, "SyntaxError", &m))?;
        let fetch_engine = Arc::clone(&nb.borrow().fetch_engine);

        let registry_key = (origin.clone(), resolved_url.clone(), worker_name.clone());
        let worker_tx = {
            let mut reg = shared_workers_registry()
                .lock()
                .expect("shared_workers lock");
            if let Some(existing) = reg.get(&registry_key) {
                existing.cmd_tx.clone()
            } else {
                let script_src = prepare_script_source(
                    &resolved_url,
                    &origin,
                    &object_urls_snapshot,
                    Arc::clone(&fetch_engine),
                )
                .map_err(|m| create_dom_exception(vm, "SecurityError", &m))?;
                let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
                let cmd_tx_self = cmd_tx.clone();
                let url_c = resolved_url.clone();
                let origin_c = origin.clone();
                let name_c = worker_name.clone();
                let _ = std::thread::Builder::new()
                    .name(format!("rix-shared-worker-{}", next_global_id()))
                    .spawn(move || {
                        run_shared_worker_thread(
                            url_c,
                            origin_c,
                            name_c,
                            script_src,
                            object_urls_snapshot,
                            fetch_engine,
                            cmd_tx_self,
                            cmd_rx,
                        );
                    });
                reg.insert(
                    registry_key,
                    SharedWorkerHandle {
                        cmd_tx: cmd_tx.clone(),
                    },
                );
                cmd_tx
            }
        };

        let client_port_id = next_global_id();
        let port_h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(port_proto)));
        let sw_h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(sw_proto)));
        let port_k = vm.heap.intern_key("port");
        vm.heap.object_define_data(
            sw_h,
            port_k,
            JsValue::from_object(port_h),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );

        let client_tx = ws.borrow().async_tx.clone();
        let _ = worker_tx.send(SharedWorkerCommand::Connect {
            client_port_id,
            client_tx,
        });

        {
            let mut state = ws.borrow_mut();
            state.shared_ports_by_id.insert(
                client_port_id,
                SharedWorkerClientPort {
                    js_port: port_h,
                    worker_tx,
                    started: false,
                    closed: false,
                    onmessage: JsValue::NULL,
                    pending: VecDeque::new(),
                },
            );
            state
                .shared_port_handle_to_id
                .insert(port_h, client_port_id);
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
        crate::events::install_object_event_handler_accessor(
            vm,
            proto,
            events,
            attr,
            move |vm, this_val| {
                let Some(h) = this_val.as_object() else {
                    return Err(vm.type_error("Incompatible BroadcastChannel receiver"));
                };
                if ws_v.borrow().broadcast_handle_to_id.contains_key(&h) {
                    Ok(h)
                } else {
                    Err(vm.type_error("Incompatible BroadcastChannel receiver"))
                }
            },
        );
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
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    workers: &SharedWorkers,
) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(event_target_proto),
    ));
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
        let (base_href, origin) = {
            let state = wa.borrow();
            (state.current_href(), state.current_origin())
        };
        let resolved_url = resolve_script_url(&raw_url, &base_href)
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
