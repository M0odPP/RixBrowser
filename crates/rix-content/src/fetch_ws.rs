//! WHATWG Fetch (`Headers`, `Request`, `Response`, `fetch`) and RFC 6455
//! `WebSocket` JS bindings backed by `rix-net` with ZERO polling timers:
//! async network completions and incoming WebSocket frames deliver directly
//! through an unbounded `tokio::sync::mpsc` channel into the content event loop.
//!
//! Reference: Spec 06 §2–§3 & Issue #17.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use rix_js::builtins_collections::alloc_values_iterator;
use rix_js::promise::{alloc_promise, promise_resolve_value, reject_promise, resolve_promise};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_net::{
    FetchCredentials, FetchEngine, FetchMode, FetchRedirect, FetchRequest, FetchResponse,
    FetchResponseType, HttpClient, NetError, WebSocketConnection, WsCommand, WsMessage,
};
use tokio::sync::mpsc;

use crate::dom_bridge::SharedHost;
use crate::event_loop::{run_microtask_checkpoint, SharedEventLoop};
use crate::events::{fire_object_event, SharedEvents};
use crate::streams::{create_readable_stream_from_bytes, SharedStreams};
use crate::web_apis::{
    abort_info_for_signal, alloc_array_buffer, alloc_form_data_from_urlencoded, alloc_uint8_array,
    arg, bind_ctor_proto, create_blob_object, create_dom_exception, define_global,
    define_host_accessor, define_host_getter, define_host_method, extract_bytes_from_js,
    SharedWebApis,
};

/// Reactive async event pushed from background network / WebSocket tokio tasks
/// directly into the content runtime's `tokio::select!` wakeup channel (ZERO polling).
#[derive(Debug)]
pub enum AsyncNetEvent {
    FetchDone {
        id: u64,
        result: Result<FetchResponse, FetchFailure>,
    },
    WsOpened {
        id: u64,
        protocol: String,
    },
    WsText {
        id: u64,
        text: String,
    },
    WsBinary {
        id: u64,
        bytes: Vec<u8>,
    },
    WsClosed {
        id: u64,
        code: u16,
        reason: String,
    },
    WsError {
        id: u64,
        message: String,
    },
    ExternalScriptLoaded {
        script_nid: rix_dom::NodeId,
        is_module: bool,
        code: String,
    },
}

#[derive(Debug, Clone)]
pub enum FetchFailure {
    Aborted,
    TypeError(String),
}

#[derive(Debug, Clone)]
struct HeadersEntry {
    /// Ordered `(lowercase_name, value)` pairs supporting `append` and comma-joined `get`.
    pairs: Vec<(String, String)>,
}

impl HeadersEntry {
    fn from_map(map: &HashMap<String, String>) -> Self {
        let mut pairs: Vec<(String, String)> = map
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        Self { pairs }
    }

    fn to_map(&self) -> HashMap<String, String> {
        let mut map: HashMap<String, String> = HashMap::new();
        for (k, v) in &self.pairs {
            map.entry(k.clone())
                .and_modify(|e| {
                    e.push_str(", ");
                    e.push_str(v);
                })
                .or_insert_with(|| v.clone());
        }
        map
    }

    fn get(&self, name: &str) -> Option<String> {
        let lower = name.to_ascii_lowercase();
        let mut joined = String::new();
        let mut found = false;
        for (k, v) in &self.pairs {
            if k == &lower {
                if found {
                    joined.push_str(", ");
                }
                joined.push_str(v);
                found = true;
            }
        }
        found.then_some(joined)
    }

    fn set(&mut self, name: &str, value: String) {
        let lower = name.to_ascii_lowercase();
        self.pairs.retain(|(k, _)| k != &lower);
        self.pairs.push((lower, value));
    }

    fn append(&mut self, name: &str, value: String) {
        self.pairs.push((name.to_ascii_lowercase(), value));
    }

    fn delete(&mut self, name: &str) {
        let lower = name.to_ascii_lowercase();
        self.pairs.retain(|(k, _)| k != &lower);
    }

    fn has(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        self.pairs.iter().any(|(k, _)| k == &lower)
    }

    fn combined_sorted_entries(&self) -> Vec<(String, String)> {
        let mut map = self.to_map();
        let mut out: Vec<(String, String)> = map.drain().collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

#[derive(Debug, Clone)]
struct RequestEntry {
    url: String,
    method: String,
    headers_h: HeapHandle,
    body: Vec<u8>,
    body_used: bool,
    mode: FetchMode,
    credentials: FetchCredentials,
    redirect: FetchRedirect,
    signal_val: JsValue,
}

#[derive(Debug, Clone)]
struct ResponseEntry {
    url: String,
    status: u16,
    status_text: String,
    response_type: FetchResponseType,
    redirected: bool,
    headers_h: HeapHandle,
    body: Vec<u8>,
    body_stream: Option<HeapHandle>,
    body_used: bool,
}

struct PendingFetch {
    promise: HeapHandle,
    abort_reason: Option<JsValue>,
}

struct JsWebSocketEntry {
    js_obj: HeapHandle,
    url: String,
    protocol: String,
    ready_state: u8,
    binary_type: String,
    cmd_tx: mpsc::UnboundedSender<WsCommand>,
}

#[derive(Debug, Clone, Default)]
struct NetPrototypes {
    headers_proto: Option<HeapHandle>,
    request_proto: Option<HeapHandle>,
    response_proto: Option<HeapHandle>,
    websocket_proto: Option<HeapHandle>,
}

/// Async `ServiceWorker` fetch interception hook invoked before network fetch.
/// Returns `Some(rx)` when an active ServiceWorker matches the request scope;
/// receiving `Some(result)` short-circuits `fetch()`, while `None` falls through to network.
pub(crate) type AsyncFetchInterceptor = Arc<
    dyn Fn(
            &FetchRequest,
        )
            -> Option<tokio::sync::oneshot::Receiver<Option<Result<FetchResponse, FetchFailure>>>>
        + Send
        + Sync,
>;

/// Shared network bridge between `ContentRuntime`'s JS VM and `rix-net`.
pub struct NetworkBridge {
    pub(crate) fetch_engine: Arc<FetchEngine>,
    pub(crate) fetch_interceptor: Option<AsyncFetchInterceptor>,
    pub(crate) async_tx: mpsc::UnboundedSender<AsyncNetEvent>,
    pub(crate) async_rx: RefCell<Option<mpsc::UnboundedReceiver<AsyncNetEvent>>>,
    next_id: u64,
    pending_fetches: HashMap<u64, PendingFetch>,
    websockets: HashMap<u64, JsWebSocketEntry>,
    ws_handle_to_id: HashMap<HeapHandle, u64>,
    headers: HashMap<HeapHandle, HeadersEntry>,
    requests: HashMap<HeapHandle, RequestEntry>,
    responses: HashMap<HeapHandle, ResponseEntry>,
    protos: NetPrototypes,
}

/// Shared handle to [`NetworkBridge`].
pub type SharedNetBridge = Rc<RefCell<NetworkBridge>>;

impl NetworkBridge {
    /// Returns `true` when at least one async `fetch()` request or live `WebSocket` is active.
    #[must_use]
    pub fn has_pending_io(&self) -> bool {
        !self.pending_fetches.is_empty() || !self.websockets.is_empty()
    }

    pub(crate) fn new(fetch_engine: Arc<FetchEngine>) -> Self {
        let (async_tx, async_rx) = mpsc::unbounded_channel();
        Self {
            fetch_engine,
            fetch_interceptor: None,
            async_tx,
            async_rx: RefCell::new(Some(async_rx)),
            next_id: 1,
            pending_fetches: HashMap::new(),
            websockets: HashMap::new(),
            ws_handle_to_id: HashMap::new(),
            headers: HashMap::new(),
            requests: HashMap::new(),
            responses: HashMap::new(),
            protos: NetPrototypes::default(),
        }
    }

    /// Returns `true` when there is at least one in-flight `fetch()` or active `WebSocket`.
    #[must_use]
    pub fn has_active_io(&self) -> bool {
        !self.pending_fetches.is_empty() || self.websockets.values().any(|ws| ws.ready_state < 3)
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    /// Collects GC roots for active fetch promises, open WebSockets, and prototypes.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.protos.headers_proto,
            self.protos.request_proto,
            self.protos.response_proto,
            self.protos.websocket_proto,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(p));
        }
        for pf in self.pending_fetches.values() {
            out.push(JsValue::from_object(pf.promise));
            if let Some(r) = pf.abort_reason {
                out.push(r);
            }
        }
        for ws in self.websockets.values() {
            if ws.ready_state < 3 {
                out.push(JsValue::from_object(ws.js_obj));
            }
        }
        for req in self.requests.values() {
            out.push(JsValue::from_object(req.headers_h));
            out.push(req.signal_val);
        }
        for resp in self.responses.values() {
            out.push(JsValue::from_object(resp.headers_h));
            if let Some(bs) = resp.body_stream {
                out.push(JsValue::from_object(bs));
            }
        }
    }

    /// Drops internal native entries for GC-collected JS objects.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.headers.retain(|&h, _| heap.is_alive(h));
        self.requests.retain(|&h, _| heap.is_alive(h));
        self.responses.retain(|&h, _| heap.is_alive(h));
        self.ws_handle_to_id.retain(|&h, _| heap.is_alive(h));
        self.websockets
            .retain(|_, ws| ws.ready_state < 3 || heap.is_alive(ws.js_obj));
    }
}

/// Installs `Headers`, `Request`, `Response`, `fetch`, and `WebSocket` onto `vm`.
pub fn install_fetch_and_websocket(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    fetch_engine: Option<Arc<FetchEngine>>,
) -> SharedNetBridge {
    let engine = fetch_engine.unwrap_or_else(|| {
        let client = HttpClient::new(None).expect("create default HttpClient");
        Arc::new(FetchEngine::new(client))
    });
    let net = Rc::new(RefCell::new(NetworkBridge::new(engine)));
    install_headers(vm, &net);
    install_request(vm, web_apis, &net);
    install_response(vm, web_apis, streams, &net);
    install_fetch_fn(vm, web_apis, &net);
    install_websocket(vm, host, events, event_loop, web_apis, &net);
    install_xml_http_request(vm, host, web_apis, &net);
    net
}

/// Applies one arriving [`AsyncNetEvent`] to `vm` and executes a strict WHATWG microtask checkpoint.
#[allow(clippy::too_many_arguments)]
pub fn deliver_async_net_event(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    net: &SharedNetBridge,
    ev: AsyncNetEvent,
) {
    match ev {
        AsyncNetEvent::FetchDone { id, result } => {
            let pending = net.borrow_mut().pending_fetches.remove(&id);
            let Some(pf) = pending else {
                return;
            };
            match result {
                Ok(resp) => {
                    let resp_h = alloc_response_from_fetch(vm, streams, net, resp);
                    resolve_promise(vm, pf.promise, JsValue::from_object(resp_h));
                }
                Err(FetchFailure::Aborted) => {
                    let reason = pf.abort_reason.unwrap_or_else(|| {
                        create_dom_exception(vm, "AbortError", "The operation was aborted.").value
                    });
                    reject_promise(vm, pf.promise, reason);
                }
                Err(FetchFailure::TypeError(msg)) => {
                    let err = vm.type_error(&msg).value;
                    reject_promise(vm, pf.promise, err);
                }
            }
            run_microtask_checkpoint(event_loop, vm);
        }
        AsyncNetEvent::WsOpened { id, protocol } => {
            let obj = {
                let mut nb = net.borrow_mut();
                let Some(ws) = nb.websockets.get_mut(&id) else {
                    return;
                };
                if ws.ready_state == 0 {
                    ws.ready_state = 1;
                    ws.protocol = protocol;
                }
                ws.js_obj
            };
            fire_object_event(vm, host, events, event_loop, obj, "open", &[]);
        }
        AsyncNetEvent::WsText { id, text } => {
            let (obj, origin) = {
                let nb = net.borrow();
                let Some(ws) = nb.websockets.get(&id) else {
                    return;
                };
                (ws.js_obj, ws.url.clone())
            };
            let data_v = JsValue::from_string(vm.heap.alloc_string(&text));
            let orig_v = JsValue::from_string(vm.heap.alloc_string(&origin));
            fire_object_event(
                vm,
                host,
                events,
                event_loop,
                obj,
                "message",
                &[("data", data_v), ("origin", orig_v)],
            );
        }
        AsyncNetEvent::WsBinary { id, bytes } => {
            let (obj, origin, binary_type) = {
                let nb = net.borrow();
                let Some(ws) = nb.websockets.get(&id) else {
                    return;
                };
                (ws.js_obj, ws.url.clone(), ws.binary_type.clone())
            };
            let data_v = if binary_type == "arraybuffer" {
                JsValue::from_object(alloc_array_buffer(vm, bytes))
            } else {
                JsValue::from_object(create_blob_object(vm, web_apis, bytes, String::new()))
            };
            let orig_v = JsValue::from_string(vm.heap.alloc_string(&origin));
            fire_object_event(
                vm,
                host,
                events,
                event_loop,
                obj,
                "message",
                &[("data", data_v), ("origin", orig_v)],
            );
        }
        AsyncNetEvent::WsClosed { id, code, reason } => {
            let obj = {
                let mut nb = net.borrow_mut();
                let Some(ws) = nb.websockets.get_mut(&id) else {
                    return;
                };
                if ws.ready_state == 3 {
                    return;
                }
                ws.ready_state = 3;
                ws.js_obj
            };
            let r_val = JsValue::from_string(vm.heap.alloc_string(&reason));
            fire_object_event(
                vm,
                host,
                events,
                event_loop,
                obj,
                "close",
                &[
                    ("code", JsValue::from_i32(i32::from(code))),
                    ("reason", r_val),
                    ("wasClean", JsValue::from_bool(code == 1000)),
                ],
            );
        }
        AsyncNetEvent::WsError { id, message } => {
            let obj = {
                let mut nb = net.borrow_mut();
                let Some(ws) = nb.websockets.get_mut(&id) else {
                    return;
                };
                ws.ready_state = 3;
                ws.js_obj
            };
            let m_val = JsValue::from_string(vm.heap.alloc_string(&message));
            fire_object_event(
                vm,
                host,
                events,
                event_loop,
                obj,
                "error",
                &[("message", m_val)],
            );
            let empty_str = JsValue::from_string(vm.heap.alloc_string(""));
            fire_object_event(
                vm,
                host,
                events,
                event_loop,
                obj,
                "close",
                &[
                    ("code", JsValue::from_i32(1006)),
                    ("reason", empty_str),
                    ("wasClean", JsValue::FALSE),
                ],
            );
        }
        AsyncNetEvent::ExternalScriptLoaded {
            script_nid,
            is_module,
            code,
        } => {
            crate::dom_sanitizer::eval_script_node(vm, host, script_nid, &code, is_module);
        }
    }
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

fn alloc_headers_from_entry(
    vm: &mut JsVm,
    net: &SharedNetBridge,
    entry: HeadersEntry,
) -> HeapHandle {
    let proto = net.borrow().protos.headers_proto.unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    net.borrow_mut().headers.insert(obj, entry);
    obj
}

fn parse_headers_init(
    vm: &mut JsVm,
    net: &SharedNetBridge,
    init: JsValue,
) -> Result<HeadersEntry, JsException> {
    let mut entry = HeadersEntry { pairs: Vec::new() };
    if init.is_undefined() || init.is_null() {
        return Ok(entry);
    }
    let Some(h) = init.as_object() else {
        return Err(vm.type_error("Invalid HeadersInit"));
    };
    if let Some(existing) = net.borrow().headers.get(&h) {
        return Ok(existing.clone());
    }
    if matches!(
        vm.heap.get_object(h).map(|o| &o.kind),
        Some(ObjectKind::Array)
    ) {
        for item in vm.collect_iterable(init)? {
            let pair = vm.collect_iterable(item)?;
            if pair.len() != 2 {
                return Err(vm.type_error("Headers sequence entry must have 2 elements"));
            }
            let k = vm.value_to_rust_string(pair[0]);
            let v = vm.value_to_rust_string(pair[1]);
            entry.append(&k, v);
        }
    } else {
        for k_str in vm.own_enumerable_keys_including_elements(h) {
            let key = vm.heap.intern_key(&k_str);
            let v_val = vm.get_property_by_key(h, key)?;
            let v_str = vm.value_to_rust_string(v_val);
            entry.append(&k_str, v_str);
        }
    }
    Ok(entry)
}

fn install_headers(vm: &mut JsVm, net: &SharedNetBridge) {
    let hp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(hp).unwrap().is_prototype = true;
    net.borrow_mut().protos.headers_proto = Some(hp);

    let nb = Rc::clone(net);
    define_host_method(vm, hp, "append", move |vm, this_val, args| {
        let h = require_headers(vm, &nb, this_val)?;
        let name = vm.value_to_rust_string(arg(args, 0));
        let value = vm.value_to_rust_string(arg(args, 1));
        if let Some(e) = nb.borrow_mut().headers.get_mut(&h) {
            e.append(&name, value);
        }
        Ok(JsValue::UNDEFINED)
    });

    let nb = Rc::clone(net);
    define_host_method(vm, hp, "set", move |vm, this_val, args| {
        let h = require_headers(vm, &nb, this_val)?;
        let name = vm.value_to_rust_string(arg(args, 0));
        let value = vm.value_to_rust_string(arg(args, 1));
        if let Some(e) = nb.borrow_mut().headers.get_mut(&h) {
            e.set(&name, value);
        }
        Ok(JsValue::UNDEFINED)
    });

    let nb = Rc::clone(net);
    define_host_method(vm, hp, "delete", move |vm, this_val, args| {
        let h = require_headers(vm, &nb, this_val)?;
        let name = vm.value_to_rust_string(arg(args, 0));
        if let Some(e) = nb.borrow_mut().headers.get_mut(&h) {
            e.delete(&name);
        }
        Ok(JsValue::UNDEFINED)
    });

    let nb = Rc::clone(net);
    define_host_method(vm, hp, "get", move |vm, this_val, args| {
        let h = require_headers(vm, &nb, this_val)?;
        let name = vm.value_to_rust_string(arg(args, 0));
        let val = nb.borrow().headers.get(&h).and_then(|e| e.get(&name));
        Ok(match val {
            Some(s) => JsValue::from_string(vm.heap.alloc_string(&s)),
            None => JsValue::NULL,
        })
    });

    let nb = Rc::clone(net);
    define_host_method(vm, hp, "has", move |vm, this_val, args| {
        let h = require_headers(vm, &nb, this_val)?;
        let name = vm.value_to_rust_string(arg(args, 0));
        let has = nb.borrow().headers.get(&h).is_some_and(|e| e.has(&name));
        Ok(JsValue::from_bool(has))
    });

    let nb = Rc::clone(net);
    let entries_fn = vm.alloc_host_fn("entries", move |vm, this_val, _| {
        let h = require_headers(vm, &nb, this_val)?;
        let entries = nb
            .borrow()
            .headers
            .get(&h)
            .map(HeadersEntry::combined_sorted_entries)
            .unwrap_or_default();
        let mut items = Vec::with_capacity(entries.len());
        for (k, v) in entries {
            let pair = vm.alloc_array();
            let kv = JsValue::from_string(vm.heap.alloc_string(&k));
            let vv = JsValue::from_string(vm.heap.alloc_string(&v));
            vm.heap.object_push_element(pair, kv);
            vm.heap.object_push_element(pair, vv);
            items.push(JsValue::from_object(pair));
        }
        Ok(JsValue::from_object(alloc_values_iterator(vm, items)))
    });
    let k_ent = vm.heap.intern_key("entries");
    let sym_it = vm.key_symbol_iterator;
    vm.heap.object_define_data(
        hp,
        k_ent,
        JsValue::from_object(entries_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        hp,
        sym_it,
        JsValue::from_object(entries_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    let nb = Rc::clone(net);
    define_host_method(vm, hp, "keys", move |vm, this_val, _| {
        let h = require_headers(vm, &nb, this_val)?;
        let entries = nb
            .borrow()
            .headers
            .get(&h)
            .map(HeadersEntry::combined_sorted_entries)
            .unwrap_or_default();
        let items = entries
            .into_iter()
            .map(|(k, _)| JsValue::from_string(vm.heap.alloc_string(&k)))
            .collect();
        Ok(JsValue::from_object(alloc_values_iterator(vm, items)))
    });

    let nb = Rc::clone(net);
    define_host_method(vm, hp, "values", move |vm, this_val, _| {
        let h = require_headers(vm, &nb, this_val)?;
        let entries = nb
            .borrow()
            .headers
            .get(&h)
            .map(HeadersEntry::combined_sorted_entries)
            .unwrap_or_default();
        let items = entries
            .into_iter()
            .map(|(_, v)| JsValue::from_string(vm.heap.alloc_string(&v)))
            .collect();
        Ok(JsValue::from_object(alloc_values_iterator(vm, items)))
    });

    let nb = Rc::clone(net);
    define_host_method(vm, hp, "forEach", move |vm, this_val, args| {
        let h = require_headers(vm, &nb, this_val)?;
        let cb = arg(args, 0);
        let this_arg = arg(args, 1);
        let entries = nb
            .borrow()
            .headers
            .get(&h)
            .map(HeadersEntry::combined_sorted_entries)
            .unwrap_or_default();
        for (k, v) in entries {
            let kv = JsValue::from_string(vm.heap.alloc_string(&k));
            let vv = JsValue::from_string(vm.heap.alloc_string(&v));
            vm.call_value(cb, this_arg, &[vv, kv, this_val])?;
        }
        Ok(JsValue::UNDEFINED)
    });

    let nb = Rc::clone(net);
    let ctor = vm.alloc_host_fn("Headers", move |vm, _, args| {
        let entry = parse_headers_init(vm, &nb, arg(args, 0))?;
        let obj = alloc_headers_from_entry(vm, &nb, entry);
        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, ctor, hp);
    define_global(vm, "Headers", JsValue::from_object(ctor));
}

fn require_headers(
    vm: &mut JsVm,
    net: &SharedNetBridge,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible Headers receiver"));
    };
    if net.borrow().headers.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible Headers receiver"))
    }
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

fn parse_mode(s: &str) -> FetchMode {
    match s {
        "no-cors" => FetchMode::NoCors,
        "same-origin" => FetchMode::SameOrigin,
        _ => FetchMode::Cors,
    }
}

fn mode_as_str(m: FetchMode) -> &'static str {
    match m {
        FetchMode::Cors => "cors",
        FetchMode::NoCors => "no-cors",
        FetchMode::SameOrigin => "same-origin",
    }
}

fn parse_credentials(s: &str) -> FetchCredentials {
    match s {
        "omit" => FetchCredentials::Omit,
        "include" => FetchCredentials::Include,
        _ => FetchCredentials::SameOrigin,
    }
}

fn credentials_as_str(c: FetchCredentials) -> &'static str {
    match c {
        FetchCredentials::Omit => "omit",
        FetchCredentials::SameOrigin => "same-origin",
        FetchCredentials::Include => "include",
    }
}

fn parse_redirect(s: &str) -> FetchRedirect {
    match s {
        "error" => FetchRedirect::Error,
        "manual" => FetchRedirect::Manual,
        _ => FetchRedirect::Follow,
    }
}

fn redirect_as_str(r: FetchRedirect) -> &'static str {
    match r {
        FetchRedirect::Follow => "follow",
        FetchRedirect::Error => "error",
        FetchRedirect::Manual => "manual",
    }
}

fn build_request_entry(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    input: JsValue,
    init: JsValue,
) -> Result<RequestEntry, JsException> {
    let base_url = web_apis.borrow().location_url.clone();
    let (
        mut url,
        mut method,
        mut hdr_entry,
        mut body,
        mut mode,
        mut credentials,
        mut redirect,
        mut signal_val,
    ) = if let Some(in_h) = input.as_object() {
        if let Some(existing) = net.borrow().requests.get(&in_h).cloned() {
            let existing_hdrs = net
                .borrow()
                .headers
                .get(&existing.headers_h)
                .cloned()
                .unwrap_or(HeadersEntry { pairs: Vec::new() });
            (
                existing.url,
                existing.method,
                existing_hdrs,
                existing.body,
                existing.mode,
                existing.credentials,
                existing.redirect,
                existing.signal_val,
            )
        } else {
            let raw = vm.value_to_rust_string(input);
            let resolved = base_url
                .join(&raw)
                .or_else(|_| url::Url::parse(&raw))
                .map_err(|_| vm.type_error(format!("Invalid Request URL: {raw}")))?;
            (
                resolved.as_str().to_string(),
                "GET".to_string(),
                HeadersEntry { pairs: Vec::new() },
                Vec::new(),
                FetchMode::Cors,
                FetchCredentials::SameOrigin,
                FetchRedirect::Follow,
                JsValue::UNDEFINED,
            )
        }
    } else {
        let raw = vm.value_to_rust_string(input);
        let resolved = base_url
            .join(&raw)
            .or_else(|_| url::Url::parse(&raw))
            .map_err(|_| vm.type_error(format!("Invalid Request URL: {raw}")))?;
        (
            resolved.as_str().to_string(),
            "GET".to_string(),
            HeadersEntry { pairs: Vec::new() },
            Vec::new(),
            FetchMode::Cors,
            FetchCredentials::SameOrigin,
            FetchRedirect::Follow,
            JsValue::UNDEFINED,
        )
    };

    if let Some(ih) = init.as_object() {
        let km = vm.heap.intern_key("method");
        if let Ok(v) = vm.get_property_by_key(ih, km) {
            if !v.is_undefined() {
                method = vm.value_to_rust_string(v).to_ascii_uppercase();
            }
        }
        let kh = vm.heap.intern_key("headers");
        if let Ok(v) = vm.get_property_by_key(ih, kh) {
            if !v.is_undefined() && !v.is_null() {
                hdr_entry = parse_headers_init(vm, net, v)?;
            }
        }
        let kb = vm.heap.intern_key("body");
        if let Ok(v) = vm.get_property_by_key(ih, kb) {
            if !v.is_undefined() && !v.is_null() {
                body = extract_bytes_from_js(vm, web_apis, v)?;
            }
        }
        let kmode = vm.heap.intern_key("mode");
        if let Ok(v) = vm.get_property_by_key(ih, kmode) {
            if !v.is_undefined() {
                mode = parse_mode(&vm.value_to_rust_string(v));
            }
        }
        let kc = vm.heap.intern_key("credentials");
        if let Ok(v) = vm.get_property_by_key(ih, kc) {
            if !v.is_undefined() {
                credentials = parse_credentials(&vm.value_to_rust_string(v));
            }
        }
        let kr = vm.heap.intern_key("redirect");
        if let Ok(v) = vm.get_property_by_key(ih, kr) {
            if !v.is_undefined() {
                redirect = parse_redirect(&vm.value_to_rust_string(v));
            }
        }
        let ks = vm.heap.intern_key("signal");
        if let Ok(v) = vm.get_property_by_key(ih, ks) {
            if !v.is_undefined() && !v.is_null() {
                signal_val = v;
            }
        }
    }

    // Ensure url stays normalized.
    if url.is_empty() {
        url = base_url.as_str().to_string();
    }
    let headers_h = alloc_headers_from_entry(vm, net, hdr_entry);
    Ok(RequestEntry {
        url,
        method,
        headers_h,
        body,
        body_used: false,
        mode,
        credentials,
        redirect,
        signal_val,
    })
}

fn install_request(vm: &mut JsVm, web_apis: &SharedWebApis, net: &SharedNetBridge) {
    let rp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(rp).unwrap().is_prototype = true;
    net.borrow_mut().protos.request_proto = Some(rp);

    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "url", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let s = nb
            .borrow()
            .requests
            .get(&h)
            .map(|r| r.url.clone())
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "method", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let s = nb
            .borrow()
            .requests
            .get(&h)
            .map(|r| r.method.clone())
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "headers", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let hh = nb.borrow().requests.get(&h).map(|r| r.headers_h).unwrap();
        Ok(JsValue::from_object(hh))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "mode", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let s = nb
            .borrow()
            .requests
            .get(&h)
            .map_or("cors", |r| mode_as_str(r.mode));
        Ok(JsValue::from_string(vm.heap.alloc_string(s)))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "credentials", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let s = nb
            .borrow()
            .requests
            .get(&h)
            .map_or("same-origin", |r| credentials_as_str(r.credentials));
        Ok(JsValue::from_string(vm.heap.alloc_string(s)))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "redirect", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let s = nb
            .borrow()
            .requests
            .get(&h)
            .map_or("follow", |r| redirect_as_str(r.redirect));
        Ok(JsValue::from_string(vm.heap.alloc_string(s)))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "bodyUsed", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let used = nb.borrow().requests.get(&h).is_some_and(|r| r.body_used);
        Ok(JsValue::from_bool(used))
    });

    let nb = Rc::clone(net);
    define_host_method(vm, rp, "text", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let bytes = consume_request_body(vm, &nb, h)?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let v = JsValue::from_string(vm.heap.alloc_string(&text));
        Ok(JsValue::from_object(promise_resolve_value(vm, v)))
    });
    let nb = Rc::clone(net);
    define_host_method(vm, rp, "json", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let bytes = consume_request_body(vm, &nb, h)?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let p = parse_json_promise(vm, &text);
        Ok(JsValue::from_object(p))
    });
    let nb = Rc::clone(net);
    define_host_method(vm, rp, "arrayBuffer", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let bytes = consume_request_body(vm, &nb, h)?;
        let ab = alloc_array_buffer(vm, bytes);
        Ok(JsValue::from_object(promise_resolve_value(
            vm,
            JsValue::from_object(ab),
        )))
    });
    let nb = Rc::clone(net);
    define_host_method(vm, rp, "bytes", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let bytes = consume_request_body(vm, &nb, h)?;
        let u8a = alloc_uint8_array(vm, bytes);
        Ok(JsValue::from_object(promise_resolve_value(
            vm,
            JsValue::from_object(u8a),
        )))
    });
    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    define_host_method(vm, rp, "blob", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let bytes = consume_request_body(vm, &nb, h)?;
        let blob = create_blob_object(vm, &wa, bytes, String::new());
        Ok(JsValue::from_object(promise_resolve_value(
            vm,
            JsValue::from_object(blob),
        )))
    });
    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    define_host_method(vm, rp, "formData", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let bytes = consume_request_body(vm, &nb, h)?;
        let fd = alloc_form_data_from_urlencoded(vm, &wa, &bytes);
        Ok(JsValue::from_object(promise_resolve_value(
            vm,
            JsValue::from_object(fd),
        )))
    });
    let nb = Rc::clone(net);
    define_host_method(vm, rp, "clone", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let mut entry = nb.borrow().requests.get(&h).cloned().unwrap();
        let hdr = nb
            .borrow()
            .headers
            .get(&entry.headers_h)
            .cloned()
            .unwrap_or(HeadersEntry { pairs: Vec::new() });
        entry.headers_h = alloc_headers_from_entry(vm, &nb, hdr);
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rp)));
        nb.borrow_mut().requests.insert(obj, entry);
        Ok(JsValue::from_object(obj))
    });

    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    let ctor = vm.alloc_host_fn("Request", move |vm, _, args| {
        let entry = build_request_entry(vm, &wa, &nb, arg(args, 0), arg(args, 1))?;
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rp)));
        nb.borrow_mut().requests.insert(obj, entry);
        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, ctor, rp);
    define_global(vm, "Request", JsValue::from_object(ctor));
}

fn require_request(
    vm: &mut JsVm,
    net: &SharedNetBridge,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible Request receiver"));
    };
    if net.borrow().requests.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible Request receiver"))
    }
}

fn consume_request_body(
    vm: &mut JsVm,
    net: &SharedNetBridge,
    h: HeapHandle,
) -> Result<Vec<u8>, JsException> {
    let mut nb = net.borrow_mut();
    let req = nb
        .requests
        .get_mut(&h)
        .ok_or_else(|| vm.type_error("Invalid Request"))?;
    if req.body_used {
        return Err(vm.type_error("Request body has already been consumed"));
    }
    req.body_used = true;
    Ok(req.body.clone())
}

// ---------------------------------------------------------------------------
// Response & fetch()
// ---------------------------------------------------------------------------

pub(crate) fn alloc_response_from_fetch(
    vm: &mut JsVm,
    streams: &SharedStreams,
    net: &SharedNetBridge,
    resp: FetchResponse,
) -> HeapHandle {
    let hdr_h = alloc_headers_from_entry(vm, net, HeadersEntry::from_map(&resp.headers));
    let stream_h = if resp.response_type == FetchResponseType::Opaque {
        None
    } else {
        Some(create_readable_stream_from_bytes(
            vm,
            streams,
            resp.body.clone(),
        ))
    };
    let proto = net
        .borrow()
        .protos
        .response_proto
        .unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    net.borrow_mut().responses.insert(
        obj,
        ResponseEntry {
            url: resp.url,
            status: resp.status,
            status_text: resp.status_text,
            response_type: resp.response_type,
            redirected: resp.redirected,
            headers_h: hdr_h,
            body: resp.body,
            body_stream: stream_h,
            body_used: false,
        },
    );
    obj
}

/// Extracts a serializable snapshot of a JS `Response` object for `CacheStorage` / `ServiceWorker`.
pub(crate) fn extract_response_snapshot(
    net: &SharedNetBridge,
    val: JsValue,
) -> Option<rix_storage::CachedHttpResponse> {
    let h = val.as_object()?;
    let st = net.borrow();
    let entry = st.responses.get(&h)?;
    let headers = st
        .headers
        .get(&entry.headers_h)
        .map(|h| h.pairs.clone())
        .unwrap_or_default();
    Some(rix_storage::CachedHttpResponse {
        url: entry.url.clone(),
        status: entry.status,
        status_text: entry.status_text.clone(),
        headers,
        body: entry.body.clone(),
    })
}

/// Allocates a JS `Response` from a persisted [`rix_storage::CachedHttpResponse`].
pub(crate) fn alloc_response_from_cached(
    vm: &mut JsVm,
    streams: &SharedStreams,
    net: &SharedNetBridge,
    cached: rix_storage::CachedHttpResponse,
) -> HeapHandle {
    let headers_map = HeadersEntry {
        pairs: cached.headers,
    }
    .to_map();
    alloc_response_from_fetch(
        vm,
        streams,
        net,
        FetchResponse {
            url: cached.url,
            status: cached.status,
            status_text: cached.status_text,
            headers: headers_map,
            body: cached.body,
            response_type: FetchResponseType::Basic,
            redirected: false,
        },
    )
}

/// Resolves a JS `RequestInfo` (`string`, `URL`, or `Request`) to `(url, method)`.
pub(crate) fn extract_request_url_and_method(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    val: JsValue,
) -> Result<(String, String), JsException> {
    let entry = build_request_entry(vm, web_apis, net, val, JsValue::UNDEFINED)?;
    Ok((entry.url, entry.method))
}

/// Allocates a JS `Request` object for a cached URL (used by `Cache.prototype.keys()`).
pub(crate) fn alloc_request_from_url(
    vm: &mut JsVm,
    net: &SharedNetBridge,
    url: String,
) -> HeapHandle {
    let hdr_h = alloc_headers_from_entry(vm, net, HeadersEntry { pairs: Vec::new() });
    let proto = net.borrow().protos.request_proto.unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    net.borrow_mut().requests.insert(
        obj,
        RequestEntry {
            url,
            method: "GET".to_string(),
            headers_h: hdr_h,
            body: Vec::new(),
            body_used: false,
            mode: FetchMode::Cors,
            credentials: FetchCredentials::SameOrigin,
            redirect: FetchRedirect::Follow,
            signal_val: JsValue::UNDEFINED,
        },
    );
    obj
}

fn parse_json_promise(vm: &mut JsVm, text: &str) -> HeapHandle {
    let p = alloc_promise(vm);
    let k_json = vm.heap.intern_key("JSON");
    let k_parse = vm.heap.intern_key("parse");
    let res = vm
        .get_property_by_key(vm.global, k_json)
        .and_then(|json_v| {
            let jh = json_v
                .as_object()
                .ok_or_else(|| vm.type_error("JSON global unavailable"))?;
            let parse_fn = vm.get_property_by_key(jh, k_parse)?;
            let s_val = JsValue::from_string(vm.heap.alloc_string(text));
            vm.call_value(parse_fn, json_v, &[s_val])
        });
    match res {
        Ok(v) => resolve_promise(vm, p, v),
        Err(e) => reject_promise(vm, p, e.value),
    }
    p
}

fn install_response(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    net: &SharedNetBridge,
) {
    let rp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(rp).unwrap().is_prototype = true;
    net.borrow_mut().protos.response_proto = Some(rp);

    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "ok", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let ok = nb
            .borrow()
            .responses
            .get(&h)
            .is_some_and(|r| (200..300).contains(&r.status));
        Ok(JsValue::from_bool(ok))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "status", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let s = nb.borrow().responses.get(&h).map_or(0, |r| r.status);
        Ok(JsValue::from_i32(i32::from(s)))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "statusText", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let s = nb
            .borrow()
            .responses
            .get(&h)
            .map(|r| r.status_text.clone())
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "url", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let s = nb
            .borrow()
            .responses
            .get(&h)
            .map(|r| r.url.clone())
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "type", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let s = nb
            .borrow()
            .responses
            .get(&h)
            .map_or("default", |r| r.response_type.as_str());
        Ok(JsValue::from_string(vm.heap.alloc_string(s)))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "redirected", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let red = nb.borrow().responses.get(&h).is_some_and(|r| r.redirected);
        Ok(JsValue::from_bool(red))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "headers", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let hh = nb.borrow().responses.get(&h).map(|r| r.headers_h).unwrap();
        Ok(JsValue::from_object(hh))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "body", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let bs = nb.borrow().responses.get(&h).and_then(|r| r.body_stream);
        Ok(bs.map_or(JsValue::NULL, JsValue::from_object))
    });
    let nb = Rc::clone(net);
    let ss = Rc::clone(streams);
    define_host_getter(vm, rp, "bodyUsed", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let used = nb.borrow().responses.get(&h).is_some_and(|r| {
            r.body_used
                || r.body_stream
                    .is_some_and(|bs| ss.borrow().is_readable_disturbed(bs))
        });
        Ok(JsValue::from_bool(used))
    });

    let nb = Rc::clone(net);
    define_host_method(vm, rp, "text", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let bytes = consume_response_body(vm, &nb, h)?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let v = JsValue::from_string(vm.heap.alloc_string(&text));
        Ok(JsValue::from_object(promise_resolve_value(vm, v)))
    });
    let nb = Rc::clone(net);
    define_host_method(vm, rp, "json", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let bytes = consume_response_body(vm, &nb, h)?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let p = parse_json_promise(vm, &text);
        Ok(JsValue::from_object(p))
    });
    let nb = Rc::clone(net);
    define_host_method(vm, rp, "arrayBuffer", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let bytes = consume_response_body(vm, &nb, h)?;
        let ab = alloc_array_buffer(vm, bytes);
        Ok(JsValue::from_object(promise_resolve_value(
            vm,
            JsValue::from_object(ab),
        )))
    });
    let nb = Rc::clone(net);
    define_host_method(vm, rp, "bytes", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let bytes = consume_response_body(vm, &nb, h)?;
        let u8a = alloc_uint8_array(vm, bytes);
        Ok(JsValue::from_object(promise_resolve_value(
            vm,
            JsValue::from_object(u8a),
        )))
    });
    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    define_host_method(vm, rp, "blob", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let (bytes, ct) = {
            let bytes = consume_response_body(vm, &nb, h)?;
            let ct = nb
                .borrow()
                .responses
                .get(&h)
                .and_then(|r| {
                    nb.borrow()
                        .headers
                        .get(&r.headers_h)
                        .and_then(|he| he.get("content-type"))
                })
                .unwrap_or_default();
            (bytes, ct)
        };
        let blob = create_blob_object(vm, &wa, bytes, ct);
        Ok(JsValue::from_object(promise_resolve_value(
            vm,
            JsValue::from_object(blob),
        )))
    });
    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    define_host_method(vm, rp, "formData", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let bytes = consume_response_body(vm, &nb, h)?;
        let fd = alloc_form_data_from_urlencoded(vm, &wa, &bytes);
        Ok(JsValue::from_object(promise_resolve_value(
            vm,
            JsValue::from_object(fd),
        )))
    });
    let nb = Rc::clone(net);
    let ss = Rc::clone(streams);
    define_host_method(vm, rp, "clone", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let mut entry = nb.borrow().responses.get(&h).cloned().unwrap();
        let hdr = nb
            .borrow()
            .headers
            .get(&entry.headers_h)
            .cloned()
            .unwrap_or(HeadersEntry { pairs: Vec::new() });
        entry.headers_h = alloc_headers_from_entry(vm, &nb, hdr);
        entry.body_stream = Some(create_readable_stream_from_bytes(
            vm,
            &ss,
            entry.body.clone(),
        ));
        entry.body_used = false;
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rp)));
        nb.borrow_mut().responses.insert(obj, entry);
        Ok(JsValue::from_object(obj))
    });

    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    let ss = Rc::clone(streams);
    let ctor = vm.alloc_host_fn("Response", move |vm, _, args| {
        let body = if arg(args, 0).is_undefined() || arg(args, 0).is_null() {
            Vec::new()
        } else {
            extract_bytes_from_js(vm, &wa, arg(args, 0))?
        };
        let mut status = 200u16;
        let mut status_text = "OK".to_string();
        let mut hdr_entry = HeadersEntry { pairs: Vec::new() };
        if let Some(ih) = arg(args, 1).as_object() {
            let ks = vm.heap.intern_key("status");
            if let Ok(v) = vm.get_property_by_key(ih, ks) {
                if !v.is_undefined() {
                    status = vm.to_number(v)?.clamp(0.0, 599.0) as u16;
                }
            }
            let kst = vm.heap.intern_key("statusText");
            if let Ok(v) = vm.get_property_by_key(ih, kst) {
                if !v.is_undefined() {
                    status_text = vm.value_to_rust_string(v);
                }
            }
            let kh = vm.heap.intern_key("headers");
            if let Ok(v) = vm.get_property_by_key(ih, kh) {
                if !v.is_undefined() && !v.is_null() {
                    hdr_entry = parse_headers_init(vm, &nb, v)?;
                }
            }
        }
        let hdr_h = alloc_headers_from_entry(vm, &nb, hdr_entry);
        let stream_h = Some(create_readable_stream_from_bytes(vm, &ss, body.clone()));
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rp)));
        nb.borrow_mut().responses.insert(
            obj,
            ResponseEntry {
                url: String::new(),
                status,
                status_text,
                response_type: FetchResponseType::Basic,
                redirected: false,
                headers_h: hdr_h,
                body,
                body_stream: stream_h,
                body_used: false,
            },
        );
        Ok(JsValue::from_object(obj))
    });

    let nb = Rc::clone(net);
    let ss = Rc::clone(streams);
    define_host_method(vm, ctor, "json", move |vm, _, args| {
        let k_json = vm.heap.intern_key("JSON");
        let k_str = vm.heap.intern_key("stringify");
        let json_v = vm.get_property_by_key(vm.global, k_json)?;
        let jh = json_v
            .as_object()
            .ok_or_else(|| vm.type_error("JSON unavailable"))?;
        let str_fn = vm.get_property_by_key(jh, k_str)?;
        let s_val = vm.call_value(str_fn, json_v, &[arg(args, 0)])?;
        let bytes = vm.value_to_rust_string(s_val).into_bytes();
        let mut hdr = HeadersEntry { pairs: Vec::new() };
        hdr.set("content-type", "application/json".to_string());
        let hdr_h = alloc_headers_from_entry(vm, &nb, hdr);
        let stream_h = Some(create_readable_stream_from_bytes(vm, &ss, bytes.clone()));
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rp)));
        nb.borrow_mut().responses.insert(
            obj,
            ResponseEntry {
                url: String::new(),
                status: 200,
                status_text: "OK".to_string(),
                response_type: FetchResponseType::Basic,
                redirected: false,
                headers_h: hdr_h,
                body: bytes,
                body_stream: stream_h,
                body_used: false,
            },
        );
        Ok(JsValue::from_object(obj))
    });

    let nb = Rc::clone(net);
    define_host_method(vm, ctor, "error", move |vm, _, _| {
        let hdr_h = alloc_headers_from_entry(vm, &nb, HeadersEntry { pairs: Vec::new() });
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rp)));
        nb.borrow_mut().responses.insert(
            obj,
            ResponseEntry {
                url: String::new(),
                status: 0,
                status_text: String::new(),
                response_type: FetchResponseType::Opaque,
                redirected: false,
                headers_h: hdr_h,
                body: Vec::new(),
                body_stream: None,
                body_used: false,
            },
        );
        Ok(JsValue::from_object(obj))
    });

    bind_ctor_proto(vm, ctor, rp);
    define_global(vm, "Response", JsValue::from_object(ctor));
}

fn require_response(
    vm: &mut JsVm,
    net: &SharedNetBridge,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible Response receiver"));
    };
    if net.borrow().responses.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible Response receiver"))
    }
}

fn consume_response_body(
    vm: &mut JsVm,
    net: &SharedNetBridge,
    h: HeapHandle,
) -> Result<Vec<u8>, JsException> {
    let mut nb = net.borrow_mut();
    let resp = nb
        .responses
        .get_mut(&h)
        .ok_or_else(|| vm.type_error("Invalid Response"))?;
    if resp.body_used {
        return Err(vm.type_error("Response body has already been consumed"));
    }
    resp.body_used = true;
    Ok(resp.body.clone())
}

fn install_fetch_fn(vm: &mut JsVm, web_apis: &SharedWebApis, net: &SharedNetBridge) {
    let wa = Rc::clone(web_apis);
    let nb = Rc::clone(net);
    let fetch_fn = vm.alloc_host_fn("fetch", move |vm, _, args| {
        let promise = alloc_promise(vm);
        let entry = match build_request_entry(vm, &wa, &nb, arg(args, 0), arg(args, 1)) {
            Ok(e) => e,
            Err(e) => {
                reject_promise(vm, promise, e.value);
                return Ok(JsValue::from_object(promise));
            }
        };
        let abort_info = abort_info_for_signal(&wa, entry.signal_val);
        if let Some((_, true, reason)) = abort_info {
            let r = if reason.is_undefined() {
                create_dom_exception(vm, "AbortError", "The operation was aborted.").value
            } else {
                reason
            };
            reject_promise(vm, promise, r);
            return Ok(JsValue::from_object(promise));
        }

        let headers_map = nb
            .borrow()
            .headers
            .get(&entry.headers_h)
            .map(HeadersEntry::to_map)
            .unwrap_or_default();
        let origin = wa.borrow().current_origin();
        let abort_signal = abort_info.as_ref().map(|(h, _, _)| h.clone());
        let abort_reason = abort_info.map(|(_, _, r)| r).filter(|r| !r.is_undefined());

        let fetch_req = FetchRequest {
            url: entry.url,
            method: entry.method,
            headers: headers_map,
            body: entry.body,
            mode: entry.mode,
            credentials: entry.credentials,
            redirect: entry.redirect,
            origin,
            abort_signal,
        };

        let (id, engine, tx, interceptor) = {
            let mut st = nb.borrow_mut();
            let id = st.alloc_id();
            st.pending_fetches.insert(
                id,
                PendingFetch {
                    promise,
                    abort_reason,
                },
            );
            (
                id,
                Arc::clone(&st.fetch_engine),
                st.async_tx.clone(),
                st.fetch_interceptor.clone(),
            )
        };

        // Spawn reactive async network task (zero polling!), giving any active
        // ServiceWorker covering the request scope first opportunity to intercept.
        // Guard with `Handle::try_current()` so synchronous/offline runtimes (e.g. WPT harness)
        // cleanly reject the Promise instead of panicking with "no reactor running".
        if let Ok(rt_handle) = tokio::runtime::Handle::try_current() {
            rt_handle.spawn(async move {
                if let Some(intercept_fn) = interceptor {
                    if let Some(rx) = intercept_fn(&fetch_req) {
                        if let Ok(Some(sw_result)) = rx.await {
                            let _ = tx.send(AsyncNetEvent::FetchDone {
                                id,
                                result: sw_result,
                            });
                            return;
                        }
                    }
                }
                let result = match engine.fetch(fetch_req).await {
                    Ok(resp) => Ok(resp),
                    Err(NetError::Aborted) => Err(FetchFailure::Aborted),
                    Err(e) => Err(FetchFailure::TypeError(e.to_string())),
                };
                let _ = tx.send(AsyncNetEvent::FetchDone { id, result });
            });
        } else {
            nb.borrow_mut().pending_fetches.remove(&id);
            let err = vm.type_error("Network runtime unavailable");
            reject_promise(vm, promise, err.value);
        }

        Ok(JsValue::from_object(promise))
    });
    define_global(vm, "fetch", JsValue::from_object(fetch_fn));
}

// ---------------------------------------------------------------------------
// WebSocket (RFC 6455 backed by rix_net::WebSocketConnection)
// ---------------------------------------------------------------------------

fn install_websocket(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
) {
    let etp = host.borrow().prototypes.event_target_proto;
    let wsp = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(etp)));
    vm.heap.get_object_mut(wsp).unwrap().is_prototype = true;
    net.borrow_mut().protos.websocket_proto = Some(wsp);

    let define_ws_consts = |vm: &mut JsVm, target: HeapHandle| {
        for (name, val) in [
            ("CONNECTING", 0),
            ("OPEN", 1),
            ("CLOSING", 2),
            ("CLOSED", 3),
        ] {
            let k = vm.heap.intern_key(name);
            vm.heap.object_define_data(
                target,
                k,
                JsValue::from_i32(val),
                PropertyAttrs::empty(),
                &mut vm.shapes,
            );
        }
    };
    define_ws_consts(vm, wsp);

    let nb = Rc::clone(net);
    define_host_getter(vm, wsp, "readyState", move |vm, this_val, _| {
        let id = require_ws_id(vm, &nb, this_val)?;
        let rs = nb.borrow().websockets.get(&id).map_or(3, |w| w.ready_state);
        Ok(JsValue::from_i32(i32::from(rs)))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, wsp, "url", move |vm, this_val, _| {
        let id = require_ws_id(vm, &nb, this_val)?;
        let s = nb
            .borrow()
            .websockets
            .get(&id)
            .map(|w| w.url.clone())
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, wsp, "protocol", move |vm, this_val, _| {
        let id = require_ws_id(vm, &nb, this_val)?;
        let s = nb
            .borrow()
            .websockets
            .get(&id)
            .map(|w| w.protocol.clone())
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });
    let nb = Rc::clone(net);
    define_host_accessor(
        vm,
        wsp,
        "binaryType",
        {
            let nb = Rc::clone(&nb);
            move |vm, this_val, _| {
                let id = require_ws_id(vm, &nb, this_val)?;
                let s = nb
                    .borrow()
                    .websockets
                    .get(&id)
                    .map(|w| w.binary_type.clone())
                    .unwrap_or_else(|| "blob".to_string());
                Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
            }
        },
        move |vm, this_val, args| {
            let id = require_ws_id(vm, &nb, this_val)?;
            let val = vm.value_to_rust_string(arg(args, 0));
            if val == "blob" || val == "arraybuffer" {
                if let Some(w) = nb.borrow_mut().websockets.get_mut(&id) {
                    w.binary_type = val;
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    define_host_method(vm, wsp, "send", move |vm, this_val, args| {
        let id = require_ws_id(vm, &nb, this_val)?;
        let (ready_state, cmd_tx) = {
            let st = nb.borrow();
            let w = st
                .websockets
                .get(&id)
                .ok_or_else(|| vm.type_error("Invalid WebSocket"))?;
            (w.ready_state, w.cmd_tx.clone())
        };
        if ready_state == 0 {
            return Err(create_dom_exception(
                vm,
                "InvalidStateError",
                "WebSocket is still in CONNECTING state",
            ));
        }
        if ready_state >= 2 {
            return Ok(JsValue::UNDEFINED);
        }
        let data = arg(args, 0);
        if data.is_string() {
            let text = vm.value_to_rust_string(data);
            let _ = cmd_tx.send(WsCommand::SendText(text));
        } else if data.as_object().is_some_and(|h| {
            wa.borrow().blobs.contains_key(&h)
                || matches!(
                    vm.heap.get_object(h).map(|o| &o.kind),
                    Some(
                        ObjectKind::ArrayBuffer(_)
                            | ObjectKind::TypedArray(_)
                            | ObjectKind::DataView(_)
                    )
                )
        }) {
            let bytes = extract_bytes_from_js(vm, &wa, data)?;
            let _ = cmd_tx.send(WsCommand::SendBinary(bytes));
        } else {
            let text = vm.value_to_rust_string(data);
            let _ = cmd_tx.send(WsCommand::SendText(text));
        }
        Ok(JsValue::UNDEFINED)
    });

    let nb = Rc::clone(net);
    define_host_method(vm, wsp, "close", move |vm, this_val, args| {
        let id = require_ws_id(vm, &nb, this_val)?;
        let code = if arg(args, 0).is_undefined() {
            1000u16
        } else {
            vm.to_number(arg(args, 0))?.clamp(1000.0, 4999.0) as u16
        };
        let reason = if arg(args, 1).is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(arg(args, 1))
        };
        let cmd_tx = {
            let mut st = nb.borrow_mut();
            let Some(w) = st.websockets.get_mut(&id) else {
                return Ok(JsValue::UNDEFINED);
            };
            if w.ready_state >= 2 {
                return Ok(JsValue::UNDEFINED);
            }
            w.ready_state = 2;
            w.cmd_tx.clone()
        };
        let _ = cmd_tx.send(WsCommand::Close { code, reason });
        Ok(JsValue::UNDEFINED)
    });

    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    let h_ws = Rc::clone(host);
    let evs_ws = Rc::clone(events);
    let el_ws = Rc::clone(event_loop);
    let ctor = vm.alloc_host_fn("WebSocket", move |vm, _, args| {
        let raw_url = vm.value_to_rust_string(arg(args, 0));
        crate::trusted_types::check_connect_src_allowed(vm, &h_ws, &evs_ws, &el_ws, &raw_url)?;
        if !raw_url.starts_with("ws://") && !raw_url.starts_with("wss://") {
            return Err(create_dom_exception(
                vm,
                "SyntaxError",
                &format!("Invalid WebSocket URL: {raw_url}"),
            ));
        }
        let mut protocols = Vec::new();
        let proto_arg = arg(args, 1);
        if !proto_arg.is_undefined() {
            if proto_arg.is_string() {
                protocols.push(vm.value_to_rust_string(proto_arg));
            } else if proto_arg.as_object().is_some() {
                for p in vm.collect_iterable(proto_arg)? {
                    protocols.push(vm.value_to_rust_string(p));
                }
            }
        }
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(wsp)));
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<WsCommand>();
        let origin = wa.borrow().current_origin();
        let (id, ev_tx) = {
            let mut st = nb.borrow_mut();
            let id = st.alloc_id();
            st.websockets.insert(
                id,
                JsWebSocketEntry {
                    js_obj: obj,
                    url: raw_url.clone(),
                    protocol: String::new(),
                    ready_state: 0,
                    binary_type: "blob".to_string(),
                    cmd_tx,
                },
            );
            st.ws_handle_to_id.insert(obj, id);
            (id, st.async_tx.clone())
        };

        // Spawn reactive RFC 6455 WebSocket task (ZERO polling!).
        let Ok(rt_handle) = tokio::runtime::Handle::try_current() else {
            let _ = ev_tx.send(AsyncNetEvent::WsError {
                id,
                message: "Network runtime unavailable".to_string(),
            });
            return Ok(JsValue::from_object(obj));
        };
        rt_handle.spawn(async move {
            let mut conn = match WebSocketConnection::connect(
                &raw_url,
                Some(&origin),
                &protocols,
            )
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = ev_tx.send(AsyncNetEvent::WsError {
                        id,
                        message: e.to_string(),
                    });
                    return;
                }
            };
            let _ = ev_tx.send(AsyncNetEvent::WsOpened {
                id,
                protocol: conn.negotiated_protocol.clone(),
            });

            loop {
                tokio::select! {
                    biased;
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(WsCommand::SendText(t)) => {
                                let _ = conn.send_text(t);
                            }
                            Some(WsCommand::SendBinary(b)) => {
                                let _ = conn.send_binary(b);
                            }
                            Some(WsCommand::Close { code, reason }) => {
                                let _ = conn.close(code, reason);
                            }
                            None => break,
                        }
                    }
                    incoming = conn.recv() => {
                        match incoming {
                            Some(Ok(WsMessage::Text(text))) => {
                                let _ = ev_tx.send(AsyncNetEvent::WsText { id, text });
                            }
                            Some(Ok(WsMessage::Binary(bytes))) => {
                                let _ = ev_tx.send(AsyncNetEvent::WsBinary { id, bytes });
                            }
                            Some(Ok(WsMessage::Close { code, reason })) => {
                                let _ = ev_tx.send(AsyncNetEvent::WsClosed { id, code, reason });
                                break;
                            }
                            Some(Err(e)) => {
                                let _ = ev_tx.send(AsyncNetEvent::WsError { id, message: e.to_string() });
                                break;
                            }
                            None => {
                                let _ = ev_tx.send(AsyncNetEvent::WsClosed {
                                    id,
                                    code: 1006,
                                    reason: String::new(),
                                });
                                break;
                            }
                        }
                    }
                }
            }
        });

        Ok(JsValue::from_object(obj))
    });
    define_ws_consts(vm, ctor);
    bind_ctor_proto(vm, ctor, wsp);
    define_global(vm, "WebSocket", JsValue::from_object(ctor));
}

fn require_ws_id(vm: &mut JsVm, net: &SharedNetBridge, val: JsValue) -> Result<u64, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible WebSocket receiver"));
    };
    net.borrow()
        .ws_handle_to_id
        .get(&h)
        .copied()
        .ok_or_else(|| vm.type_error("Incompatible WebSocket receiver"))
}

// ---------------------------------------------------------------------------
// XMLHttpRequest (WHATWG XHR Standard backed by FetchEngine)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct XhrState {
    method: String,
    url: String,
    async_flag: bool,
    ready_state: i32,
    status: u16,
    status_text: String,
    response_url: String,
    response_text: String,
    response_bytes: Vec<u8>,
    response_type: String,
    override_mime: Option<String>,
    req_headers: HashMap<String, String>,
    resp_headers: HashMap<String, String>,
}

impl Default for XhrState {
    fn default() -> Self {
        Self {
            method: String::new(),
            url: String::new(),
            async_flag: true,
            ready_state: 0,
            status: 0,
            status_text: String::new(),
            response_url: String::new(),
            response_text: String::new(),
            response_bytes: Vec::new(),
            response_type: String::new(),
            override_mime: None,
            req_headers: HashMap::new(),
            resp_headers: HashMap::new(),
        }
    }
}

fn is_valid_http_method_token(m: &str) -> bool {
    !m.is_empty()
        && m.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn install_xml_http_request(
    vm: &mut JsVm,
    host: &SharedHost,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
) {
    let etp = host.borrow().prototypes.event_target_proto;
    let xp = vm.heap.alloc_empty_object();
    vm.heap.object_set_prototype(xp, Some(etp), &mut vm.shapes);
    vm.heap.get_object_mut(xp).unwrap().is_prototype = true;

    let xhrs: Rc<RefCell<HashMap<HeapHandle, XhrState>>> = Rc::new(RefCell::new(HashMap::new()));

    let define_xhr_consts = |vm: &mut JsVm, obj: HeapHandle| {
        for (name, v) in [
            ("UNSENT", 0),
            ("OPENED", 1),
            ("HEADERS_RECEIVED", 2),
            ("LOADING", 3),
            ("DONE", 4),
        ] {
            let k = vm.heap.intern_key(name);
            vm.heap.object_define_data(
                obj,
                k,
                JsValue::from_i32(v),
                PropertyAttrs::ENUMERABLE,
                &mut vm.shapes,
            );
        }
    };
    define_xhr_consts(vm, xp);

    let require_xhr = |vm: &mut JsVm,
                       map: &Rc<RefCell<HashMap<HeapHandle, XhrState>>>,
                       val: JsValue|
     -> Result<HeapHandle, JsException> {
        let Some(h) = val.as_object() else {
            return Err(vm.type_error("Incompatible XMLHttpRequest receiver"));
        };
        if map.borrow().contains_key(&h) {
            Ok(h)
        } else {
            Err(vm.type_error("Incompatible XMLHttpRequest receiver"))
        }
    };

    // open(method, url, async=true, username?, password?)
    {
        let xm = Rc::clone(&xhrs);
        let wa = Rc::clone(web_apis);
        define_host_method(vm, xp, "open", move |vm, this_val, args| {
            let h = require_xhr(vm, &xm, this_val)?;
            if args.len() < 2 {
                return Err(vm.type_error("XMLHttpRequest.open requires at least 2 arguments"));
            }
            let raw_method = vm.value_to_rust_string(arg(args, 0));
            if !is_valid_http_method_token(&raw_method) {
                return Err(create_dom_exception(
                    vm,
                    "SyntaxError",
                    &format!("Invalid HTTP method: {raw_method}"),
                ));
            }
            let upper = raw_method.to_ascii_uppercase();
            if matches!(upper.as_str(), "CONNECT" | "TRACE" | "TRACK") {
                return Err(create_dom_exception(
                    vm,
                    "SecurityError",
                    &format!("Forbidden HTTP method: {raw_method}"),
                ));
            }
            let method = if matches!(
                upper.as_str(),
                "DELETE" | "GET" | "HEAD" | "OPTIONS" | "POST" | "PUT"
            ) {
                upper
            } else {
                raw_method
            };
            let raw_url = vm.value_to_rust_string(arg(args, 1));
            let base = wa.borrow().location_url.clone();
            let parsed = base.join(&raw_url).map_err(|_| {
                create_dom_exception(vm, "SyntaxError", &format!("Invalid URL: {raw_url}"))
            })?;
            let async_flag = if args.len() >= 3 && !arg(args, 2).is_undefined() {
                vm.to_boolean(arg(args, 2))
            } else {
                true
            };
            if let Some(st) = xm.borrow_mut().get_mut(&h) {
                st.method = method;
                st.url = parsed.to_string();
                st.async_flag = async_flag;
                st.ready_state = 1;
                st.status = 0;
                st.status_text.clear();
                st.response_text.clear();
                st.response_bytes.clear();
                st.req_headers.clear();
                st.resp_headers.clear();
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // setRequestHeader(name, value)
    {
        let xm = Rc::clone(&xhrs);
        define_host_method(vm, xp, "setRequestHeader", move |vm, this_val, args| {
            let h = require_xhr(vm, &xm, this_val)?;
            let name = vm.value_to_rust_string(arg(args, 0));
            let value = vm.value_to_rust_string(arg(args, 1));
            if let Some(st) = xm.borrow_mut().get_mut(&h) {
                if st.ready_state != 1 {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "XMLHttpRequest must be OPENED",
                    ));
                }
                st.req_headers
                    .entry(name.to_ascii_lowercase())
                    .and_modify(|v| {
                        v.push_str(", ");
                        v.push_str(&value);
                    })
                    .or_insert(value);
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // overrideMimeType(mime)
    {
        let xm = Rc::clone(&xhrs);
        define_host_method(vm, xp, "overrideMimeType", move |vm, this_val, args| {
            let h = require_xhr(vm, &xm, this_val)?;
            let mime = vm.value_to_rust_string(arg(args, 0));
            if let Some(st) = xm.borrow_mut().get_mut(&h) {
                st.override_mime = Some(mime);
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // abort()
    {
        let xm = Rc::clone(&xhrs);
        define_host_method(vm, xp, "abort", move |vm, this_val, _| {
            let h = require_xhr(vm, &xm, this_val)?;
            if let Some(st) = xm.borrow_mut().get_mut(&h) {
                st.ready_state = 0;
                st.status = 0;
                st.response_text.clear();
                st.response_bytes.clear();
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // send(body?)
    {
        let xm = Rc::clone(&xhrs);
        let wa = Rc::clone(web_apis);
        let nb = Rc::clone(net);
        define_host_method(vm, xp, "send", move |vm, this_val, args| {
            let h = require_xhr(vm, &xm, this_val)?;
            let body = if args.is_empty() || arg(args, 0).is_undefined() || arg(args, 0).is_null() {
                Vec::new()
            } else {
                extract_bytes_from_js(vm, &wa, arg(args, 0))?
            };
            let (method, url, headers, override_mime) = {
                let map = xm.borrow();
                let st = map.get(&h).unwrap();
                if st.ready_state != 1 {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "XMLHttpRequest must be OPENED before send()",
                    ));
                }
                (
                    st.method.clone(),
                    st.url.clone(),
                    st.req_headers.clone(),
                    st.override_mime.clone(),
                )
            };
            let origin = wa.borrow().current_origin();
            let engine = Arc::clone(&nb.borrow().fetch_engine);
            let req = FetchRequest {
                url: url.clone(),
                method,
                headers,
                body,
                mode: FetchMode::Cors,
                credentials: FetchCredentials::SameOrigin,
                redirect: FetchRedirect::Follow,
                origin,
                abort_signal: None,
            };
            let fetched = if let Ok(handle) = tokio::runtime::Handle::try_current() {
                if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread {
                    std::thread::spawn(move || {
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .ok()
                            .and_then(|rt| rt.block_on(engine.fetch(req)).ok())
                    })
                    .join()
                    .ok()
                    .flatten()
                } else {
                    tokio::task::block_in_place(|| handle.block_on(engine.fetch(req)).ok())
                }
            } else {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()
                    .and_then(|rt| rt.block_on(engine.fetch(req)).ok())
            };
            let Some(resp) = fetched else {
                return Err(create_dom_exception(
                    vm,
                    "NetworkError",
                    "Network request failed",
                ));
            };
            let ct = override_mime.or_else(|| {
                resp.headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                    .map(|(_, v)| v.clone())
            });
            let http_charset = ct.as_deref().and_then(|c| {
                c.split(';').skip(1).find_map(|p| {
                    let (k, v) = p.trim().split_once('=')?;
                    k.trim()
                        .eq_ignore_ascii_case("charset")
                        .then(|| v.trim().trim_matches('"').trim_matches('\'').to_string())
                })
            });
            let (decoded, _) = rix_html::decode_html_bytes(&resp.body, http_charset.as_deref());
            if let Some(st) = xm.borrow_mut().get_mut(&h) {
                st.status = resp.status;
                st.status_text = resp.status_text;
                st.response_url = resp.url;
                st.response_bytes = resp.body;
                st.response_text = decoded;
                st.resp_headers = resp.headers;
                st.ready_state = 4;
            }
            for ev_name in ["onreadystatechange", "onload", "onloadend"] {
                let k = vm.heap.intern_key(ev_name);
                if let Ok(cb) = vm.get_property_by_key(h, k) {
                    if cb.as_object().is_some_and(|ch| vm.is_callable_handle(ch)) {
                        let _ = vm.call_value(cb, this_val, &[]);
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // getResponseHeader / getAllResponseHeaders
    {
        let xm = Rc::clone(&xhrs);
        define_host_method(vm, xp, "getResponseHeader", move |vm, this_val, args| {
            let h = require_xhr(vm, &xm, this_val)?;
            let name = vm.value_to_rust_string(arg(args, 0)).to_ascii_lowercase();
            let found = xm.borrow().get(&h).and_then(|st| {
                st.resp_headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(&name))
                    .map(|(_, v)| v.clone())
            });
            Ok(match found {
                Some(v) => JsValue::from_string(vm.heap.alloc_string(&v)),
                None => JsValue::NULL,
            })
        });
    }
    {
        let xm = Rc::clone(&xhrs);
        define_host_method(vm, xp, "getAllResponseHeaders", move |vm, this_val, _| {
            let h = require_xhr(vm, &xm, this_val)?;
            let out = xm
                .borrow()
                .get(&h)
                .map(|st| {
                    let mut pairs: Vec<(&String, &String)> = st.resp_headers.iter().collect();
                    pairs.sort_by(|a, b| a.0.cmp(b.0));
                    pairs
                        .into_iter()
                        .map(|(k, v)| format!("{}: {}\r\n", k.to_ascii_lowercase(), v))
                        .collect::<String>()
                })
                .unwrap_or_default();
            Ok(JsValue::from_string(vm.heap.alloc_string(&out)))
        });
    }

    // Readonly getters (`readyState`, `status`, `statusText`, `responseURL`, `responseText`, `response`)
    {
        let xm = Rc::clone(&xhrs);
        define_host_getter(vm, xp, "readyState", move |vm, this_val, _| {
            let h = require_xhr(vm, &xm, this_val)?;
            let v = xm.borrow().get(&h).map_or(0, |s| s.ready_state);
            Ok(JsValue::from_i32(v))
        });
    }
    {
        let xm = Rc::clone(&xhrs);
        define_host_getter(vm, xp, "status", move |vm, this_val, _| {
            let h = require_xhr(vm, &xm, this_val)?;
            let v = xm.borrow().get(&h).map_or(0, |s| i32::from(s.status));
            Ok(JsValue::from_i32(v))
        });
    }
    {
        let xm = Rc::clone(&xhrs);
        define_host_getter(vm, xp, "statusText", move |vm, this_val, _| {
            let h = require_xhr(vm, &xm, this_val)?;
            let s = xm
                .borrow()
                .get(&h)
                .map(|st| st.status_text.clone())
                .unwrap_or_default();
            Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
        });
    }
    {
        let xm = Rc::clone(&xhrs);
        define_host_getter(vm, xp, "responseURL", move |vm, this_val, _| {
            let h = require_xhr(vm, &xm, this_val)?;
            let s = xm
                .borrow()
                .get(&h)
                .map(|st| st.response_url.clone())
                .unwrap_or_default();
            Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
        });
    }
    {
        let xm = Rc::clone(&xhrs);
        define_host_getter(vm, xp, "responseText", move |vm, this_val, _| {
            let h = require_xhr(vm, &xm, this_val)?;
            let s = xm
                .borrow()
                .get(&h)
                .map(|st| st.response_text.clone())
                .unwrap_or_default();
            Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
        });
    }
    {
        let xm = Rc::clone(&xhrs);
        define_host_getter(vm, xp, "response", move |vm, this_val, _| {
            let h = require_xhr(vm, &xm, this_val)?;
            let (ty, txt, bytes) = xm
                .borrow()
                .get(&h)
                .map(|st| {
                    (
                        st.response_type.clone(),
                        st.response_text.clone(),
                        st.response_bytes.clone(),
                    )
                })
                .unwrap_or_default();
            if ty == "arraybuffer" {
                Ok(JsValue::from_object(alloc_array_buffer(vm, bytes)))
            } else {
                Ok(JsValue::from_string(vm.heap.alloc_string(&txt)))
            }
        });
    }
    {
        let xmg = Rc::clone(&xhrs);
        let xms = Rc::clone(&xhrs);
        define_host_accessor(
            vm,
            xp,
            "responseType",
            move |vm, this_val, _| {
                let h = require_xhr(vm, &xmg, this_val)?;
                let s = xmg
                    .borrow()
                    .get(&h)
                    .map(|st| st.response_type.clone())
                    .unwrap_or_default();
                Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
            },
            move |vm, this_val, args| {
                let h = require_xhr(vm, &xms, this_val)?;
                let v = vm.value_to_rust_string(arg(args, 0));
                if let Some(st) = xms.borrow_mut().get_mut(&h) {
                    st.response_type = v;
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    let xm_ctor = Rc::clone(&xhrs);
    let ctor = vm.alloc_host_fn("XMLHttpRequest", move |vm, _, _| {
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(xp)));
        xm_ctor.borrow_mut().insert(obj, XhrState::default());
        Ok(JsValue::from_object(obj))
    });
    define_xhr_consts(vm, ctor);
    bind_ctor_proto(vm, ctor, xp);
    define_global(vm, "XMLHttpRequest", JsValue::from_object(ctor));
}
