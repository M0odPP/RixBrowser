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

use rix_js::promise::{alloc_promise, reject_promise, resolve_promise};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_net::{
    FetchCache, FetchCredentials, FetchEngine, FetchMode, FetchRedirect, FetchRequest,
    FetchResponse, FetchResponseType, HttpClient, NetError, WebSocketConnection, WsCommand,
    WsMessage,
};
use tokio::sync::mpsc;

use crate::dom_bridge::SharedHost;
use crate::event_loop::{run_microtask_checkpoint, SharedEventLoop};
use crate::events::{
    fire_object_event_dispatch_only, install_object_event_handler_accessor, SharedEvents,
};
use crate::fetch_body::{
    body_is_used, body_to_array_buffer, body_to_blob, body_to_bytes, body_to_form_data,
    body_to_json, body_to_text, clone_fetch_body, consume_fetch_body_promise, extract_fetch_body,
    transfer_request_body, FetchBody,
};
use crate::fetch_headers::{
    alloc_headers_from_entry, fill_headers_from_init, install_headers, is_forbidden_method,
    is_forbidden_request_header, is_valid_header_name, is_valid_status_text,
    normalize_and_validate_header_value, normalize_method, parse_headers_init, to_byte_string,
    HeaderIterState, HeadersEntry, HeadersGuard,
};
use crate::streams::SharedStreams;
use crate::streams_bytes::create_readable_byte_stream_from_bytes;
use crate::streams_common::{promise_reject_value, set_to_string_tag};
use crate::web_apis::{
    abort_info_for_signal, alloc_array_buffer, arg, bind_ctor_proto, create_blob_object,
    create_dom_exception, create_internal_abort_controller, define_global, define_host_accessor,
    define_host_getter, define_host_method, extract_bytes_from_js, SharedWebApis,
};
use crate::websocket_stream::{
    deliver_ws_stream_binary, deliver_ws_stream_closed, deliver_ws_stream_error,
    deliver_ws_stream_opened, deliver_ws_stream_sent, deliver_ws_stream_text,
    install_websocket_stream, JsWebSocketStreamEntry, WebSocketErrorData, WsStreamPhase,
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
        extensions: String,
        set_cookies: Vec<String>,
    },
    WsText {
        id: u64,
        text: String,
    },
    WsBinary {
        id: u64,
        bytes: Vec<u8>,
    },
    WsDrained {
        id: u64,
        bytes: usize,
    },
    WsClosed {
        id: u64,
        code: u16,
        reason: String,
        was_clean: bool,
    },
    WsError {
        id: u64,
        message: String,
    },
    WsStreamOpened {
        id: u64,
        protocol: String,
        extensions: String,
        set_cookies: Vec<String>,
    },
    WsStreamText {
        id: u64,
        text: String,
    },
    WsStreamBinary {
        id: u64,
        bytes: Vec<u8>,
    },
    WsStreamSent {
        id: u64,
    },
    WsStreamClosed {
        id: u64,
        code: u16,
        reason: String,
        was_clean: bool,
    },
    WsStreamError {
        id: u64,
        message: String,
    },
    ExternalScriptLoaded {
        script_nid: rix_dom::NodeId,
        is_module: bool,
        code: String,
        ok: bool,
    },
    DynamicImageLoaded {
        img_nid: rix_dom::NodeId,
        raw_src: String,
        resolved_url: String,
        raster: Option<rix_layout::DecodedRasterBytes>,
        svg: Option<rix_layout::DecodedSvg>,
    },
    DynamicStylesheetLoaded {
        link_nid: rix_dom::NodeId,
        css: Option<String>,
    },
    XhrCompleted {
        xhr_h: HeapHandle,
        override_mime: Option<String>,
        result: Option<FetchResponse>,
    },
}

#[derive(Debug, Clone)]
pub enum FetchFailure {
    Aborted,
    TypeError(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseKind {
    Default,
    Basic,
    Cors,
    Error,
    Opaque,
    OpaqueRedirect,
}

impl ResponseKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Basic => "basic",
            Self::Cors => "cors",
            Self::Error => "error",
            Self::Opaque => "opaque",
            Self::OpaqueRedirect => "opaqueredirect",
        }
    }

    fn from_net(t: FetchResponseType) -> Self {
        match t {
            FetchResponseType::Basic => Self::Basic,
            FetchResponseType::Cors => Self::Cors,
            FetchResponseType::Opaque => Self::Opaque,
            FetchResponseType::OpaqueRedirect => Self::OpaqueRedirect,
        }
    }
}

#[derive(Debug, Clone)]
struct RequestEntry {
    url: String,
    method: String,
    headers_h: HeapHandle,
    body: Option<FetchBody>,
    mode: FetchMode,
    credentials: FetchCredentials,
    cache: String,
    redirect: FetchRedirect,
    referrer: String,
    referrer_policy: String,
    integrity: String,
    keepalive: bool,
    destination: String,
    is_reload_navigation: bool,
    is_history_navigation: bool,
    signal_h: HeapHandle,
}

#[derive(Debug, Clone)]
struct ResponseEntry {
    url: String,
    status: u16,
    status_text: String,
    response_kind: ResponseKind,
    redirected: bool,
    headers_h: HeapHandle,
    body: Option<FetchBody>,
}

struct PendingFetch {
    promise: HeapHandle,
    abort_reason: Option<JsValue>,
    keepalive_bytes: usize,
}

struct JsWebSocketEntry {
    js_obj: HeapHandle,
    url: String,
    origin: String,
    protocol: String,
    extensions: String,
    ready_state: u8,
    binary_type: String,
    buffered_amount: u64,
    cmd_tx: mpsc::UnboundedSender<WsCommand>,
    abort_connect: Option<tokio::sync::oneshot::Sender<()>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct NetPrototypes {
    pub(crate) headers_proto: Option<HeapHandle>,
    pub(crate) headers_iter_proto: Option<HeapHandle>,
    pub(crate) request_proto: Option<HeapHandle>,
    pub(crate) response_proto: Option<HeapHandle>,
    pub(crate) websocket_proto: Option<HeapHandle>,
    pub(crate) ws_stream_proto: Option<HeapHandle>,
    pub(crate) ws_error_proto: Option<HeapHandle>,
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
    pub(crate) pending_subresources: usize,
    keepalive_inflight_bytes: usize,
    websockets: HashMap<u64, JsWebSocketEntry>,
    ws_handle_to_id: HashMap<HeapHandle, u64>,
    pub(crate) ws_streams: HashMap<u64, JsWebSocketStreamEntry>,
    pub(crate) ws_stream_handle_to_id: HashMap<HeapHandle, u64>,
    pub(crate) ws_errors: HashMap<HeapHandle, WebSocketErrorData>,
    pub(crate) headers: HashMap<HeapHandle, HeadersEntry>,
    pub(crate) header_iterators: HashMap<HeapHandle, HeaderIterState>,
    requests: HashMap<HeapHandle, RequestEntry>,
    responses: HashMap<HeapHandle, ResponseEntry>,
    xhrs: Rc<RefCell<HashMap<HeapHandle, XhrState>>>,
    pending_xhrs: HashMap<HeapHandle, ()>,
    pub(crate) protos: NetPrototypes,
}

impl NetworkBridge {
    pub(crate) fn is_non_serializable_net_object(&self, h: HeapHandle) -> bool {
        self.requests.contains_key(&h)
            || self.responses.contains_key(&h)
            || self.headers.contains_key(&h)
            || self.header_iterators.contains_key(&h)
            || self.ws_stream_handle_to_id.contains_key(&h)
            || self.ws_errors.contains_key(&h)
    }
}

/// Shared handle to [`NetworkBridge`].
pub type SharedNetBridge = Rc<RefCell<NetworkBridge>>;

impl NetworkBridge {
    /// Returns `true` when at least one async `fetch()` request or live `WebSocket` is active.
    #[must_use]
    pub fn has_pending_io(&self) -> bool {
        !self.pending_fetches.is_empty()
            || self.pending_subresources > 0
            || !self.websockets.is_empty()
            || self
                .ws_streams
                .values()
                .any(|s| s.phase != WsStreamPhase::Closed)
    }

    /// Returns `true` while non-keepalive `fetch()` requests or dynamic subresources
    /// (`<img>`/`<link>`/`<script>`) are still in flight (excluding persistent WebSockets
    /// and fire-and-forget `keepalive` beacons per WHATWG Fetch §5.4).
    pub fn has_pending_load_io(&self) -> bool {
        self.pending_subresources > 0
            || self
                .pending_fetches
                .values()
                .any(|pf| pf.keepalive_bytes == 0)
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
            pending_subresources: 0,
            keepalive_inflight_bytes: 0,
            websockets: HashMap::new(),
            ws_handle_to_id: HashMap::new(),
            ws_streams: HashMap::new(),
            ws_stream_handle_to_id: HashMap::new(),
            ws_errors: HashMap::new(),
            headers: HashMap::new(),
            header_iterators: HashMap::new(),
            requests: HashMap::new(),
            responses: HashMap::new(),
            xhrs: Rc::new(RefCell::new(HashMap::new())),
            pending_xhrs: HashMap::new(),
            protos: NetPrototypes::default(),
        }
    }

    /// Returns `true` when there is at least one in-flight `fetch()` or active `WebSocket`.
    #[must_use]
    pub fn has_active_io(&self) -> bool {
        !self.pending_fetches.is_empty()
            || self.pending_subresources > 0
            || self.websockets.values().any(|ws| ws.ready_state < 3)
            || self
                .ws_streams
                .values()
                .any(|s| s.phase != WsStreamPhase::Closed)
    }

    pub(crate) fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    /// Collects GC roots for active fetch promises, open WebSockets, and prototypes.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.protos.headers_proto,
            self.protos.headers_iter_proto,
            self.protos.request_proto,
            self.protos.response_proto,
            self.protos.websocket_proto,
            self.protos.ws_stream_proto,
            self.protos.ws_error_proto,
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
        for wss in self.ws_streams.values() {
            wss.collect_roots(out);
        }
        for hi in self.header_iterators.values() {
            out.push(JsValue::from_object(hi.target));
        }
        for &xhr_h in self.pending_xhrs.keys() {
            out.push(JsValue::from_object(xhr_h));
        }
        for req in self.requests.values() {
            out.push(JsValue::from_object(req.headers_h));
            out.push(JsValue::from_object(req.signal_h));
            if let Some(ref b) = req.body {
                out.push(JsValue::from_object(b.stream));
            }
        }
        for resp in self.responses.values() {
            out.push(JsValue::from_object(resp.headers_h));
            if let Some(ref b) = resp.body {
                out.push(JsValue::from_object(b.stream));
            }
        }
    }

    /// Drops internal native entries for GC-collected JS objects.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.headers.retain(|&h, _| heap.is_alive(h));
        self.header_iterators.retain(|&h, _| heap.is_alive(h));
        self.requests.retain(|&h, _| heap.is_alive(h));
        self.responses.retain(|&h, _| heap.is_alive(h));
        self.ws_handle_to_id.retain(|&h, _| heap.is_alive(h));
        self.websockets
            .retain(|_, ws| ws.ready_state < 3 || heap.is_alive(ws.js_obj));
        self.ws_stream_handle_to_id.retain(|&h, _| heap.is_alive(h));
        self.ws_streams
            .retain(|_, s| s.phase != WsStreamPhase::Closed || heap.is_alive(s.js_obj));
        self.ws_errors.retain(|&h, _| heap.is_alive(h));
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
    install_request(vm, web_apis, streams, &net);
    install_response(vm, web_apis, streams, &net);
    install_fetch_fn(vm, web_apis, streams, &net);
    install_websocket(vm, host, events, event_loop, web_apis, &net);
    install_websocket_stream(vm, events, web_apis, streams, &net);
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
            let pending = {
                let mut nb = net.borrow_mut();
                let pf = nb.pending_fetches.remove(&id);
                if let Some(ref p) = pf {
                    nb.keepalive_inflight_bytes = nb
                        .keepalive_inflight_bytes
                        .saturating_sub(p.keepalive_bytes);
                }
                pf
            };
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
        AsyncNetEvent::WsOpened {
            id,
            protocol,
            extensions,
            set_cookies,
        } => {
            let (obj, ws_url) = {
                let mut nb = net.borrow_mut();
                let Some(ws) = nb.websockets.get_mut(&id) else {
                    return;
                };
                if ws.ready_state != 0 {
                    return;
                }
                ws.ready_state = 1;
                ws.protocol = protocol;
                ws.extensions = extensions;
                ws.abort_connect = None;
                (ws.js_obj, ws.url.clone())
            };
            if !set_cookies.is_empty() {
                if let Ok(u) = url::Url::parse(&ws_url) {
                    let mut wa = web_apis.borrow_mut();
                    for sc in &set_cookies {
                        wa.set_cookie_from_header(&u, sc, false);
                    }
                }
            }
            let ev_proto = events.borrow().event_proto.unwrap_or(vm.object_proto);
            fire_object_event_dispatch_only(
                vm,
                host,
                events,
                event_loop,
                obj,
                "open",
                ev_proto,
                &[],
                None,
            );
        }
        AsyncNetEvent::WsText { id, text } => {
            let (obj, origin) = {
                let nb = net.borrow();
                let Some(ws) = nb.websockets.get(&id) else {
                    return;
                };
                if ws.ready_state != 1 {
                    return;
                }
                (ws.js_obj, ws.origin.clone())
            };
            let data_v = JsValue::from_string(vm.heap.alloc_string(&text));
            let orig_v = JsValue::from_string(vm.heap.alloc_string(&origin));
            let empty_v = JsValue::from_string(vm.heap.alloc_string(""));
            let ports_v = JsValue::from_object(vm.alloc_array());
            let proto = lookup_global_event_proto(vm, events, "MessageEvent");
            fire_object_event_dispatch_only(
                vm,
                host,
                events,
                event_loop,
                obj,
                "message",
                proto,
                &[
                    ("data", data_v),
                    ("origin", orig_v),
                    ("lastEventId", empty_v),
                    ("source", JsValue::NULL),
                    ("ports", ports_v),
                ],
                None,
            );
        }
        AsyncNetEvent::WsBinary { id, bytes } => {
            let (obj, origin, binary_type) = {
                let nb = net.borrow();
                let Some(ws) = nb.websockets.get(&id) else {
                    return;
                };
                if ws.ready_state != 1 {
                    return;
                }
                (ws.js_obj, ws.origin.clone(), ws.binary_type.clone())
            };
            let data_v = if binary_type == "arraybuffer" {
                JsValue::from_object(alloc_array_buffer(vm, bytes))
            } else {
                JsValue::from_object(create_blob_object(vm, web_apis, bytes, String::new()))
            };
            let orig_v = JsValue::from_string(vm.heap.alloc_string(&origin));
            let empty_v = JsValue::from_string(vm.heap.alloc_string(""));
            let ports_v = JsValue::from_object(vm.alloc_array());
            let proto = lookup_global_event_proto(vm, events, "MessageEvent");
            fire_object_event_dispatch_only(
                vm,
                host,
                events,
                event_loop,
                obj,
                "message",
                proto,
                &[
                    ("data", data_v),
                    ("origin", orig_v),
                    ("lastEventId", empty_v),
                    ("source", JsValue::NULL),
                    ("ports", ports_v),
                ],
                None,
            );
        }
        AsyncNetEvent::WsDrained { id, bytes } => {
            let mut nb = net.borrow_mut();
            if let Some(ws) = nb.websockets.get_mut(&id) {
                ws.buffered_amount = ws.buffered_amount.saturating_sub(bytes as u64);
            }
        }
        AsyncNetEvent::WsClosed {
            id,
            code,
            reason,
            was_clean,
        } => {
            let obj = {
                let mut nb = net.borrow_mut();
                let Some(ws) = nb.websockets.get_mut(&id) else {
                    return;
                };
                if ws.ready_state == 3 {
                    return;
                }
                ws.ready_state = 3;
                ws.abort_connect = None;
                ws.js_obj
            };
            let close_proto = lookup_global_event_proto(vm, events, "CloseEvent");
            fire_object_event_dispatch_only(
                vm,
                host,
                events,
                event_loop,
                obj,
                "close",
                close_proto,
                &[],
                Some((was_clean, code, reason)),
            );
        }
        AsyncNetEvent::WsError { id, message } => {
            let obj = {
                let mut nb = net.borrow_mut();
                let Some(ws) = nb.websockets.get_mut(&id) else {
                    return;
                };
                if ws.ready_state == 3 {
                    return;
                }
                ws.ready_state = 3;
                ws.abort_connect = None;
                ws.js_obj
            };
            let m_val = JsValue::from_string(vm.heap.alloc_string(&message));
            let ev_proto = events.borrow().event_proto.unwrap_or(vm.object_proto);
            fire_object_event_dispatch_only(
                vm,
                host,
                events,
                event_loop,
                obj,
                "error",
                ev_proto,
                &[("message", m_val)],
                None,
            );
            let close_proto = lookup_global_event_proto(vm, events, "CloseEvent");
            fire_object_event_dispatch_only(
                vm,
                host,
                events,
                event_loop,
                obj,
                "close",
                close_proto,
                &[],
                Some((false, 1006, String::new())),
            );
        }
        AsyncNetEvent::WsStreamOpened {
            id,
            protocol,
            extensions,
            set_cookies,
        } => {
            deliver_ws_stream_opened(
                vm,
                event_loop,
                web_apis,
                streams,
                net,
                id,
                protocol,
                extensions,
                set_cookies,
            );
        }
        AsyncNetEvent::WsStreamText { id, text } => {
            deliver_ws_stream_text(vm, event_loop, streams, net, id, text);
        }
        AsyncNetEvent::WsStreamBinary { id, bytes } => {
            deliver_ws_stream_binary(vm, event_loop, streams, net, id, bytes);
        }
        AsyncNetEvent::WsStreamSent { id } => {
            deliver_ws_stream_sent(vm, event_loop, net, id);
        }
        AsyncNetEvent::WsStreamClosed {
            id,
            code,
            reason,
            was_clean,
        } => {
            deliver_ws_stream_closed(vm, event_loop, streams, net, id, code, reason, was_clean);
        }
        AsyncNetEvent::WsStreamError { id, message } => {
            deliver_ws_stream_error(vm, event_loop, streams, net, id, message);
        }
        AsyncNetEvent::ExternalScriptLoaded {
            script_nid,
            is_module,
            code,
            ok,
        } => {
            {
                let mut nb = net.borrow_mut();
                nb.pending_subresources = nb.pending_subresources.saturating_sub(1);
            }
            if ok {
                crate::dom_sanitizer::eval_script_node(vm, host, script_nid, &code, is_module);
                crate::dom_sanitizer::dispatch_simple_element_event(
                    vm, host, events, event_loop, script_nid, "load",
                );
            } else {
                crate::dom_sanitizer::dispatch_simple_element_event(
                    vm, host, events, event_loop, script_nid, "error",
                );
            }
        }
        AsyncNetEvent::DynamicImageLoaded {
            img_nid,
            raw_src,
            resolved_url,
            raster,
            svg,
        } => {
            {
                let mut nb = net.borrow_mut();
                nb.pending_subresources = nb.pending_subresources.saturating_sub(1);
            }
            let mut loaded_ok = false;
            {
                let mut hb = host.borrow_mut();
                hb.pending_dynamic_images.remove(&img_nid);
                hb.completed_images.insert(img_nid);
                if let Some(r) = raster {
                    if hb
                        .layout_engine
                        .assets
                        .register_rgba8_shared_with_intrinsic(
                            resolved_url.clone(),
                            r.width,
                            r.height,
                            r.intrinsic_width,
                            r.intrinsic_height,
                            r.rgba,
                        )
                        .is_ok()
                    {
                        if raw_src != resolved_url {
                            hb.layout_engine
                                .assets
                                .register_alias(raw_src, &resolved_url);
                        }
                        loaded_ok = true;
                    }
                } else if let Some(s) = svg {
                    hb.layout_engine
                        .assets
                        .register_svg(resolved_url.clone(), s);
                    if raw_src != resolved_url {
                        hb.layout_engine
                            .assets
                            .register_alias(raw_src, &resolved_url);
                    }
                    loaded_ok = true;
                }
                if loaded_ok {
                    let doc_rc = Rc::clone(&hb.document);
                    doc_rc.borrow_mut().mark_node_dirty(
                        img_nid,
                        rix_dom::DirtyFlags::SELF_LAYOUT_DIRTY | rix_dom::DirtyFlags::PAINT_DIRTY,
                    );
                    hb.invalidate_layout_cache();
                }
            }
            let ev_type = if loaded_ok { "load" } else { "error" };
            crate::dom_sanitizer::dispatch_simple_element_event(
                vm, host, events, event_loop, img_nid, ev_type,
            );
        }
        AsyncNetEvent::DynamicStylesheetLoaded { link_nid, css } => {
            {
                let mut nb = net.borrow_mut();
                nb.pending_subresources = nb.pending_subresources.saturating_sub(1);
            }
            let loaded_ok = if let Some(css_text) = css {
                let mut hb = host.borrow_mut();
                let media = rix_css::MediaContext {
                    viewport_width: hb.viewport.width,
                    viewport_height: hb.viewport.height,
                    dark_mode: hb.dark_mode,
                    ..rix_css::MediaContext::default()
                };
                let base_url = {
                    let doc = hb.document.borrow();
                    let raw_href = match doc.get(link_nid) {
                        Some(rix_dom::NodeData::Element(el)) => {
                            el.get_attribute("href").unwrap_or("").trim().to_string()
                        }
                        _ => String::new(),
                    };
                    if !raw_href.is_empty() {
                        url::Url::parse(&hb.main_document_url)
                            .ok()
                            .and_then(|b| b.join(&raw_href).ok())
                            .map_or(raw_href, |u| u.to_string())
                    } else {
                        hb.main_document_url.clone()
                    }
                };
                let base_opt = (!base_url.is_empty()).then_some(base_url.as_str());
                let sheet = rix_css::Stylesheet::parse_cached_with_base(
                    &css_text,
                    rix_css::CascadeOrigin::Author,
                    &media,
                    base_opt,
                );
                hb.add_external_stylesheet(sheet);
                true
            } else {
                false
            };
            let ev_type = if loaded_ok { "load" } else { "error" };
            crate::dom_sanitizer::dispatch_simple_element_event(
                vm, host, events, event_loop, link_nid, ev_type,
            );
        }
        AsyncNetEvent::XhrCompleted {
            xhr_h,
            override_mime,
            result,
        } => {
            let xhrs = {
                let mut nb = net.borrow_mut();
                nb.pending_xhrs.remove(&xhr_h);
                Rc::clone(&nb.xhrs)
            };
            let ok = if let Some(resp) = result {
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
                if let Some(st) = xhrs.borrow_mut().get_mut(&xhr_h) {
                    st.status = resp.status;
                    st.status_text = resp.status_text;
                    st.response_url = resp.url;
                    st.response_bytes = resp.body;
                    st.response_text = decoded;
                    st.resp_headers = resp.headers;
                    st.ready_state = 4;
                }
                true
            } else {
                if let Some(st) = xhrs.borrow_mut().get_mut(&xhr_h) {
                    st.status = 0;
                    st.status_text.clear();
                    st.ready_state = 4;
                }
                false
            };
            let this_val = JsValue::from_object(xhr_h);
            let handlers: [&str; 3] = if ok {
                ["onreadystatechange", "onload", "onloadend"]
            } else {
                ["onreadystatechange", "onerror", "onloadend"]
            };
            for ev_name in handlers {
                let k = vm.heap.intern_key(ev_name);
                if let Ok(cb) = vm.get_property_by_key(xhr_h, k) {
                    if cb.as_object().is_some_and(|ch| vm.is_callable_handle(ch)) {
                        let _ = vm.call_value(cb, this_val, &[]);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Request (WHATWG Fetch §5.4)
// ---------------------------------------------------------------------------

fn parse_mode_checked(vm: &mut JsVm, s: &str) -> Result<FetchMode, JsException> {
    match s {
        "cors" => Ok(FetchMode::Cors),
        "no-cors" => Ok(FetchMode::NoCors),
        "same-origin" => Ok(FetchMode::SameOrigin),
        _ => Err(vm.type_error(format!("Invalid RequestMode: '{s}'"))),
    }
}

fn mode_as_str(m: FetchMode) -> &'static str {
    match m {
        FetchMode::Cors => "cors",
        FetchMode::NoCors => "no-cors",
        FetchMode::SameOrigin => "same-origin",
    }
}

fn parse_credentials_checked(vm: &mut JsVm, s: &str) -> Result<FetchCredentials, JsException> {
    match s {
        "omit" => Ok(FetchCredentials::Omit),
        "same-origin" => Ok(FetchCredentials::SameOrigin),
        "include" => Ok(FetchCredentials::Include),
        _ => Err(vm.type_error(format!("Invalid RequestCredentials: '{s}'"))),
    }
}

fn credentials_as_str(c: FetchCredentials) -> &'static str {
    match c {
        FetchCredentials::Omit => "omit",
        FetchCredentials::SameOrigin => "same-origin",
        FetchCredentials::Include => "include",
    }
}

fn parse_redirect_checked(vm: &mut JsVm, s: &str) -> Result<FetchRedirect, JsException> {
    match s {
        "follow" => Ok(FetchRedirect::Follow),
        "error" => Ok(FetchRedirect::Error),
        "manual" => Ok(FetchRedirect::Manual),
        _ => Err(vm.type_error(format!("Invalid RequestRedirect: '{s}'"))),
    }
}

fn redirect_as_str(r: FetchRedirect) -> &'static str {
    match r {
        FetchRedirect::Follow => "follow",
        FetchRedirect::Error => "error",
        FetchRedirect::Manual => "manual",
    }
}

fn validate_cache_mode(vm: &mut JsVm, s: &str) -> Result<String, JsException> {
    match s {
        "default" | "no-store" | "reload" | "no-cache" | "force-cache" | "only-if-cached" => {
            Ok(s.to_string())
        }
        _ => Err(vm.type_error(format!("Invalid RequestCache: '{s}'"))),
    }
}

fn validate_referrer_policy(vm: &mut JsVm, s: &str) -> Result<String, JsException> {
    match s {
        ""
        | "no-referrer"
        | "no-referrer-when-downgrade"
        | "same-origin"
        | "origin"
        | "strict-origin"
        | "origin-when-cross-origin"
        | "strict-origin-when-cross-origin"
        | "unsafe-url" => Ok(s.to_string()),
        _ => Err(vm.type_error(format!("Invalid ReferrerPolicy: '{s}'"))),
    }
}

fn create_fresh_abort_signal(vm: &mut JsVm, web_apis: &SharedWebApis) -> HeapHandle {
    create_internal_abort_controller(vm, web_apis)
        .map(|(_, sig)| sig)
        .unwrap_or_else(|| vm.alloc_ordinary_object())
}

fn build_request_entry(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    net: &SharedNetBridge,
    input: JsValue,
    init: JsValue,
) -> Result<RequestEntry, JsException> {
    if !init.is_undefined() && !init.is_null() && !init.is_object() {
        return Err(vm.type_error("RequestInit must be an object"));
    }
    let base_url = web_apis.borrow().location_url.clone();

    let mut input_req_handle: Option<HeapHandle> = None;
    let (
        url,
        mut method,
        mut hdr_pairs,
        mut body,
        mut mode,
        mut credentials,
        mut cache,
        mut redirect,
        mut referrer,
        mut referrer_policy,
        mut integrity,
        mut keepalive,
        destination,
        mut is_reload_navigation,
        mut is_history_navigation,
        mut signal_h,
    ) = if let Some(in_h) = input.as_object() {
        if let Some(existing) = net.borrow().requests.get(&in_h).cloned() {
            input_req_handle = Some(in_h);
            let pairs = net
                .borrow()
                .headers
                .get(&existing.headers_h)
                .map(|h| h.pairs.clone())
                .unwrap_or_default();
            (
                existing.url,
                existing.method,
                pairs,
                existing.body,
                existing.mode,
                existing.credentials,
                existing.cache,
                existing.redirect,
                existing.referrer,
                existing.referrer_policy,
                existing.integrity,
                existing.keepalive,
                existing.destination,
                existing.is_reload_navigation,
                existing.is_history_navigation,
                existing.signal_h,
            )
        } else {
            let raw = vm.to_js_string(input)?;
            let resolved = base_url
                .join(&raw)
                .or_else(|_| url::Url::parse(&raw))
                .map_err(|_| vm.type_error(format!("Invalid Request URL: {raw}")))?;
            if !resolved.username().is_empty() || resolved.password().is_some() {
                return Err(vm.type_error("Request URL cannot contain credentials"));
            }
            let sig = create_fresh_abort_signal(vm, web_apis);
            (
                resolved.as_str().to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
                FetchMode::Cors,
                FetchCredentials::SameOrigin,
                "default".to_string(),
                FetchRedirect::Follow,
                "about:client".to_string(),
                String::new(),
                String::new(),
                false,
                String::new(),
                false,
                false,
                sig,
            )
        }
    } else {
        let raw = vm.to_js_string(input)?;
        let resolved = base_url
            .join(&raw)
            .or_else(|_| url::Url::parse(&raw))
            .map_err(|_| vm.type_error(format!("Invalid Request URL: {raw}")))?;
        if !resolved.username().is_empty() || resolved.password().is_some() {
            return Err(vm.type_error("Request URL cannot contain credentials"));
        }
        let sig = create_fresh_abort_signal(vm, web_apis);
        (
            resolved.as_str().to_string(),
            "GET".to_string(),
            Vec::new(),
            None,
            FetchMode::Cors,
            FetchCredentials::SameOrigin,
            "default".to_string(),
            FetchRedirect::Follow,
            "about:client".to_string(),
            String::new(),
            String::new(),
            false,
            String::new(),
            false,
            false,
            sig,
        )
    };

    let mut init_has_body = false;
    let mut init_body_val = JsValue::UNDEFINED;
    let mut init_headers_val = JsValue::UNDEFINED;
    let mut init_non_empty = false;
    let mut init_has_duplex = false;

    if let Some(ih) = init.as_object() {
        // Check `window` member (WHATWG §5.4: only `null` or `undefined` allowed).
        let kw = vm.heap.intern_key("window");
        let wv = vm.get_property_by_key(ih, kw)?;
        if !wv.is_undefined() && !wv.is_null() {
            return Err(vm.type_error("RequestInit.window can only be null"));
        }

        let kref = vm.heap.intern_key("referrer");
        let v_ref = vm.get_property_by_key(ih, kref)?;
        if !v_ref.is_undefined() {
            init_non_empty = true;
            let rs = vm.to_js_string(v_ref)?;
            if rs.is_empty() {
                referrer = String::new();
            } else if rs == "about:client" {
                referrer = "about:client".to_string();
            } else {
                let parsed = base_url
                    .join(&rs)
                    .or_else(|_| url::Url::parse(&rs))
                    .map_err(|_| vm.type_error(format!("Invalid referrer URL: '{rs}'")))?;
                if parsed.origin() == base_url.origin() {
                    referrer = parsed.as_str().to_string();
                } else {
                    referrer = "about:client".to_string();
                }
            }
        }

        let krp = vm.heap.intern_key("referrerPolicy");
        let v_rp = vm.get_property_by_key(ih, krp)?;
        if !v_rp.is_undefined() {
            init_non_empty = true;
            let s = vm.to_js_string(v_rp)?;
            referrer_policy = validate_referrer_policy(vm, &s)?;
        }

        let kmode = vm.heap.intern_key("mode");
        let v_mode = vm.get_property_by_key(ih, kmode)?;
        if !v_mode.is_undefined() {
            init_non_empty = true;
            let s = vm.to_js_string(v_mode)?;
            mode = parse_mode_checked(vm, &s)?;
        }

        let kc = vm.heap.intern_key("credentials");
        let v_cred = vm.get_property_by_key(ih, kc)?;
        if !v_cred.is_undefined() {
            init_non_empty = true;
            let s = vm.to_js_string(v_cred)?;
            credentials = parse_credentials_checked(vm, &s)?;
        }

        let kcache = vm.heap.intern_key("cache");
        let v_cache = vm.get_property_by_key(ih, kcache)?;
        if !v_cache.is_undefined() {
            init_non_empty = true;
            let s = vm.to_js_string(v_cache)?;
            cache = validate_cache_mode(vm, &s)?;
        }

        let kredi = vm.heap.intern_key("redirect");
        let v_redi = vm.get_property_by_key(ih, kredi)?;
        if !v_redi.is_undefined() {
            init_non_empty = true;
            let s = vm.to_js_string(v_redi)?;
            redirect = parse_redirect_checked(vm, &s)?;
        }

        let kint = vm.heap.intern_key("integrity");
        let v_int = vm.get_property_by_key(ih, kint)?;
        if !v_int.is_undefined() {
            init_non_empty = true;
            integrity = vm.to_js_string(v_int)?;
        }

        let kka = vm.heap.intern_key("keepalive");
        let v_ka = vm.get_property_by_key(ih, kka)?;
        if !v_ka.is_undefined() {
            init_non_empty = true;
            keepalive = vm.to_boolean(v_ka);
        }

        let km = vm.heap.intern_key("method");
        let v_m = vm.get_property_by_key(ih, km)?;
        if !v_m.is_undefined() {
            init_non_empty = true;
            let raw_m = to_byte_string(vm, v_m)?;
            if !is_valid_header_name(&raw_m) {
                return Err(vm.type_error(format!("Invalid HTTP method: '{raw_m}'")));
            }
            if is_forbidden_method(&raw_m) {
                return Err(vm.type_error(format!("Forbidden HTTP method: '{raw_m}'")));
            }
            method = normalize_method(&raw_m);
        }

        let ks = vm.heap.intern_key("signal");
        let v_sig = vm.get_property_by_key(ih, ks)?;
        if !v_sig.is_undefined() {
            init_non_empty = true;
            if v_sig.is_null() {
                signal_h = create_fresh_abort_signal(vm, web_apis);
            } else {
                let Some(sh) = v_sig.as_object() else {
                    return Err(vm.type_error("RequestInit.signal must be an AbortSignal"));
                };
                if !web_apis.borrow().abort_signals.contains_key(&sh) {
                    return Err(vm.type_error("RequestInit.signal must be an AbortSignal"));
                }
                signal_h = sh;
            }
        }

        let kdup = vm.heap.intern_key("duplex");
        let v_dup = vm.get_property_by_key(ih, kdup)?;
        if !v_dup.is_undefined() {
            init_non_empty = true;
            init_has_duplex = true;
            let s = vm.to_js_string(v_dup)?;
            if s != "half" {
                return Err(vm.type_error(format!("Invalid RequestDuplex: '{s}'")));
            }
        }

        let kpri = vm.heap.intern_key("priority");
        let v_pri = vm.get_property_by_key(ih, kpri)?;
        if !v_pri.is_undefined() {
            init_non_empty = true;
            let s = vm.to_js_string(v_pri)?;
            if !matches!(s.as_str(), "high" | "low" | "auto") {
                return Err(vm.type_error(format!("Invalid RequestPriority: '{s}'")));
            }
        }

        let kh = vm.heap.intern_key("headers");
        let v_h = vm.get_property_by_key(ih, kh)?;
        if !v_h.is_undefined() {
            init_non_empty = true;
            init_headers_val = v_h;
        }

        let kb = vm.heap.intern_key("body");
        let v_b = vm.get_property_by_key(ih, kb)?;
        if !v_b.is_undefined() {
            init_non_empty = true;
            init_has_body = true;
            init_body_val = v_b;
        }
    }

    // Cross-field invariants per WHATWG Fetch §5.4.
    if cache == "only-if-cached" && mode != FetchMode::SameOrigin {
        return Err(vm.type_error("cache: 'only-if-cached' requires mode: 'same-origin'"));
    }
    if mode == FetchMode::NoCors && !matches!(method.as_str(), "GET" | "HEAD" | "POST") {
        return Err(vm.type_error(format!(
            "Method '{method}' is not allowed in 'no-cors' mode"
        )));
    }

    // If `init` was non-empty on a Request input with a body that is not being overridden,
    // check whether the input body was already disturbed or locked.
    let mut body_ct: Option<String> = None;
    if init_has_body {
        if !init_body_val.is_null() && matches!(method.as_str(), "GET" | "HEAD") {
            return Err(vm.type_error("Request with GET/HEAD method cannot have body"));
        }
        let body_is_stream = init_body_val
            .as_object()
            .is_some_and(|h| streams.borrow().readable.contains_key(&h));
        if body_is_stream && !init_has_duplex {
            return Err(vm.type_error(
                "RequestInit: duplex option is required when sending a ReadableStream body",
            ));
        }
        if body_is_stream && keepalive {
            return Err(
                vm.type_error("Request with keepalive: true cannot have a ReadableStream body")
            );
        }
        body = extract_fetch_body(vm, web_apis, streams, init_body_val)?;
        body_ct = body.as_ref().and_then(|b| b.content_type.clone());
    } else if input_req_handle.is_some() {
        if let Some(ref existing_body) = body {
            if matches!(method.as_str(), "GET" | "HEAD") {
                return Err(vm.type_error("Request with GET/HEAD method cannot have body"));
            }
            if keepalive && existing_body.source_bytes.is_none() {
                return Err(
                    vm.type_error("Request with keepalive: true cannot have a ReadableStream body")
                );
            }
            // WHATWG §5.4 step 36: keep input Request's original body handle intact (marked
            // locked & disturbed), and transfer contents into a fresh stream on the new Request.
            let transferred = transfer_request_body(vm, web_apis, streams, existing_body)?;
            body_ct = transferred.content_type.clone();
            body = Some(transferred);
        }
    }

    let guard = if mode == FetchMode::NoCors {
        HeadersGuard::RequestNoCors
    } else {
        HeadersGuard::Request
    };
    let mut hdr_entry = HeadersEntry::with_guard(guard);
    if init_non_empty {
        is_reload_navigation = false;
        is_history_navigation = false;
    }
    if !init_headers_val.is_undefined() {
        fill_headers_from_init(vm, net, &mut hdr_entry, init_headers_val)?;
    } else {
        for (k, v) in hdr_pairs.drain(..) {
            let _ = hdr_entry.append_checked(vm, &k, &v);
        }
    }
    if let Some(ct) = body_ct {
        if !hdr_entry.has("content-type") {
            let _ = hdr_entry.append_checked(vm, "content-type", &ct);
        }
    }

    let headers_h = alloc_headers_from_entry(vm, net, hdr_entry);
    Ok(RequestEntry {
        url: if url.is_empty() {
            base_url.as_str().to_string()
        } else {
            url
        },
        method,
        headers_h,
        body,
        mode,
        credentials,
        cache,
        redirect,
        referrer,
        referrer_policy,
        integrity,
        keepalive,
        destination,
        is_reload_navigation,
        is_history_navigation,
        signal_h,
    })
}

fn install_request(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    net: &SharedNetBridge,
) {
    let rp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(rp).unwrap().is_prototype = true;
    set_to_string_tag(vm, rp, "Request");
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
    define_host_getter(vm, rp, "destination", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let s = nb
            .borrow()
            .requests
            .get(&h)
            .map(|r| r.destination.clone())
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "referrer", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let s = nb
            .borrow()
            .requests
            .get(&h)
            .map(|r| r.referrer.clone())
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "referrerPolicy", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let s = nb
            .borrow()
            .requests
            .get(&h)
            .map(|r| r.referrer_policy.clone())
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
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
    define_host_getter(vm, rp, "cache", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let s = nb
            .borrow()
            .requests
            .get(&h)
            .map(|r| r.cache.clone())
            .unwrap_or_else(|| "default".to_string());
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
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
    define_host_getter(vm, rp, "integrity", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let s = nb
            .borrow()
            .requests
            .get(&h)
            .map(|r| r.integrity.clone())
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "keepalive", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let ka = nb.borrow().requests.get(&h).is_some_and(|r| r.keepalive);
        Ok(JsValue::from_bool(ka))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "duplex", move |vm, this_val, _| {
        let _ = require_request(vm, &nb, this_val)?;
        Ok(JsValue::from_string(vm.heap.alloc_string("half")))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "isReloadNavigation", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let v = nb
            .borrow()
            .requests
            .get(&h)
            .is_some_and(|r| r.is_reload_navigation);
        Ok(JsValue::from_bool(v))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "isHistoryNavigation", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let v = nb
            .borrow()
            .requests
            .get(&h)
            .is_some_and(|r| r.is_history_navigation);
        Ok(JsValue::from_bool(v))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "signal", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let sh = nb.borrow().requests.get(&h).map(|r| r.signal_h).unwrap();
        Ok(JsValue::from_object(sh))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, rp, "body", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let bs = nb
            .borrow()
            .requests
            .get(&h)
            .and_then(|r| r.body.as_ref().map(|b| b.stream));
        Ok(bs.map_or(JsValue::NULL, JsValue::from_object))
    });
    let nb = Rc::clone(net);
    let ss = Rc::clone(streams);
    define_host_getter(vm, rp, "bodyUsed", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let used = nb
            .borrow()
            .requests
            .get(&h)
            .is_some_and(|r| body_is_used(&ss, r.body.as_ref()));
        Ok(JsValue::from_bool(used))
    });

    // Body mixin methods (always return a Promise!)
    let nb = Rc::clone(net);
    let ss = Rc::clone(streams);
    define_host_method(vm, rp, "text", move |vm, this_val, _| {
        let h = match require_request(vm, &nb, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let body = nb.borrow().requests.get(&h).and_then(|r| r.body.clone());
        Ok(JsValue::from_object(consume_fetch_body_promise(
            vm,
            &ss,
            body.as_ref(),
            body_to_text,
        )))
    });
    let nb = Rc::clone(net);
    let ss = Rc::clone(streams);
    define_host_method(vm, rp, "json", move |vm, this_val, _| {
        let h = match require_request(vm, &nb, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let body = nb.borrow().requests.get(&h).and_then(|r| r.body.clone());
        Ok(JsValue::from_object(consume_fetch_body_promise(
            vm,
            &ss,
            body.as_ref(),
            body_to_json,
        )))
    });
    let nb = Rc::clone(net);
    let ss = Rc::clone(streams);
    define_host_method(vm, rp, "arrayBuffer", move |vm, this_val, _| {
        let h = match require_request(vm, &nb, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let body = nb.borrow().requests.get(&h).and_then(|r| r.body.clone());
        Ok(JsValue::from_object(consume_fetch_body_promise(
            vm,
            &ss,
            body.as_ref(),
            body_to_array_buffer,
        )))
    });
    let nb = Rc::clone(net);
    let ss = Rc::clone(streams);
    define_host_method(vm, rp, "bytes", move |vm, this_val, _| {
        let h = match require_request(vm, &nb, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let body = nb.borrow().requests.get(&h).and_then(|r| r.body.clone());
        Ok(JsValue::from_object(consume_fetch_body_promise(
            vm,
            &ss,
            body.as_ref(),
            body_to_bytes,
        )))
    });
    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    let ss = Rc::clone(streams);
    define_host_method(vm, rp, "blob", move |vm, this_val, _| {
        let h = match require_request(vm, &nb, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let (body, ct) = {
            let st = nb.borrow();
            let req = st.requests.get(&h);
            let body = req.and_then(|r| r.body.clone());
            let ct = req.and_then(|r| {
                st.headers
                    .get(&r.headers_h)
                    .and_then(|he| he.get("content-type"))
            });
            (body, ct)
        };
        let wa_c = Rc::clone(&wa);
        Ok(JsValue::from_object(consume_fetch_body_promise(
            vm,
            &ss,
            body.as_ref(),
            move |vm, bytes| body_to_blob(vm, &wa_c, bytes, ct),
        )))
    });
    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    let ss = Rc::clone(streams);
    define_host_method(vm, rp, "formData", move |vm, this_val, _| {
        let h = match require_request(vm, &nb, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let (body, ct) = {
            let st = nb.borrow();
            let req = st.requests.get(&h);
            let body = req.and_then(|r| r.body.clone());
            let ct = req.and_then(|r| {
                st.headers
                    .get(&r.headers_h)
                    .and_then(|he| he.get("content-type"))
            });
            (body, ct)
        };
        let wa_c = Rc::clone(&wa);
        Ok(JsValue::from_object(consume_fetch_body_promise(
            vm,
            &ss,
            body.as_ref(),
            move |vm, bytes| body_to_form_data(vm, &wa_c, bytes, ct),
        )))
    });

    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    let ss = Rc::clone(streams);
    define_host_method(vm, rp, "clone", move |vm, this_val, _| {
        let h = require_request(vm, &nb, this_val)?;
        let mut entry = nb.borrow().requests.get(&h).cloned().unwrap();
        let (b_orig, b_clone) = clone_fetch_body(vm, &wa, &ss, entry.body.as_ref())?;
        if let Some(req_mut) = nb.borrow_mut().requests.get_mut(&h) {
            req_mut.body = b_orig;
        }
        entry.body = b_clone;
        let hdr = nb
            .borrow()
            .headers
            .get(&entry.headers_h)
            .cloned()
            .unwrap_or_default();
        entry.headers_h = alloc_headers_from_entry(vm, &nb, hdr);
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rp)));
        nb.borrow_mut().requests.insert(obj, entry);
        Ok(JsValue::from_object(obj))
    });

    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    let ss = Rc::clone(streams);
    let ctor = vm.alloc_host_fn("Request", move |vm, _, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("Request constructor requires 'new'"));
        }
        if args.is_empty() {
            return Err(vm.type_error("Request constructor requires at least 1 argument"));
        }
        let entry = build_request_entry(vm, &wa, &ss, &nb, arg(args, 0), arg(args, 1))?;
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

// ---------------------------------------------------------------------------
// Response & fetch() (WHATWG Fetch §5.5)
// ---------------------------------------------------------------------------

fn is_null_body_status(status: u16) -> bool {
    matches!(status, 101 | 204 | 205 | 304)
}

pub(crate) fn alloc_response_from_fetch(
    vm: &mut JsVm,
    streams: &SharedStreams,
    net: &SharedNetBridge,
    resp: FetchResponse,
) -> HeapHandle {
    let hdr_h = alloc_headers_from_entry(
        vm,
        net,
        HeadersEntry::from_map(&resp.headers, HeadersGuard::Immutable),
    );
    let body =
        if resp.response_type == FetchResponseType::Opaque || is_null_body_status(resp.status) {
            None
        } else {
            let stream = create_readable_byte_stream_from_bytes(vm, streams, resp.body.clone());
            Some(FetchBody {
                stream,
                source_bytes: Some(resp.body),
                content_type: None,
            })
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
            response_kind: ResponseKind::from_net(resp.response_type),
            redirected: resp.redirected,
            headers_h: hdr_h,
            body,
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
    let body = entry
        .body
        .as_ref()
        .and_then(|b| b.source_bytes.clone())
        .unwrap_or_default();
    Some(rix_storage::CachedHttpResponse {
        url: entry.url.clone(),
        status: entry.status,
        status_text: entry.status_text.clone(),
        headers,
        body,
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
        guard: HeadersGuard::None,
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
    streams: &SharedStreams,
    net: &SharedNetBridge,
    val: JsValue,
) -> Result<(String, String), JsException> {
    if let Some(h) = val.as_object() {
        if let Some(req) = net.borrow().requests.get(&h) {
            return Ok((req.url.clone(), req.method.clone()));
        }
    }
    let entry = build_request_entry(vm, web_apis, streams, net, val, JsValue::UNDEFINED)?;
    Ok((entry.url, entry.method))
}

/// Allocates a JS `Request` object for a cached URL (used by `Cache.prototype.keys()`).
pub(crate) fn alloc_request_from_url(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    url: String,
) -> HeapHandle {
    let hdr_h = alloc_headers_from_entry(vm, net, HeadersEntry::with_guard(HeadersGuard::Request));
    let signal_h = create_fresh_abort_signal(vm, web_apis);
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
            body: None,
            mode: FetchMode::Cors,
            credentials: FetchCredentials::SameOrigin,
            cache: "default".to_string(),
            redirect: FetchRedirect::Follow,
            referrer: "about:client".to_string(),
            referrer_policy: String::new(),
            integrity: String::new(),
            keepalive: false,
            destination: String::new(),
            is_reload_navigation: false,
            is_history_navigation: false,
            signal_h,
        },
    );
    obj
}

fn parse_response_init(
    vm: &mut JsVm,
    net: &SharedNetBridge,
    init_val: JsValue,
) -> Result<(u16, String, HeadersEntry), JsException> {
    let mut status = 200u16;
    let mut status_text = String::new();
    let mut hdr_entry = HeadersEntry::with_guard(HeadersGuard::Response);
    if init_val.is_undefined() || init_val.is_null() {
        return Ok((status, status_text, hdr_entry));
    }
    let Some(ih) = init_val.as_object() else {
        return Err(vm.type_error("ResponseInit must be an object"));
    };
    let ks = vm.heap.intern_key("status");
    let v_s = vm.get_property_by_key(ih, ks)?;
    if !v_s.is_undefined() {
        let n = vm.to_number(v_s)?;
        // WebIDL `unsigned short`: wrap modulo 2^16, NaN/Inf -> 0.
        let raw_u16 = if n.is_finite() {
            (n.trunc() as i64).rem_euclid(65536) as u16
        } else {
            0
        };
        if !(200..=599).contains(&raw_u16) {
            return Err(vm.range_error(format!("Invalid Response status: {raw_u16}")));
        }
        status = raw_u16;
    }
    let kst = vm.heap.intern_key("statusText");
    let v_st = vm.get_property_by_key(ih, kst)?;
    if !v_st.is_undefined() {
        let s = to_byte_string(vm, v_st)?;
        if !is_valid_status_text(&s) {
            return Err(vm.type_error(format!("Invalid Response statusText: '{s}'")));
        }
        status_text = s;
    }
    let kh = vm.heap.intern_key("headers");
    let v_h = vm.get_property_by_key(ih, kh)?;
    if !v_h.is_undefined() {
        hdr_entry = parse_headers_init(vm, net, v_h, HeadersGuard::Response)?;
    }
    Ok((status, status_text, hdr_entry))
}

fn install_response(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    net: &SharedNetBridge,
) {
    let rp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(rp).unwrap().is_prototype = true;
    set_to_string_tag(vm, rp, "Response");
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
            .map_or("default", |r| r.response_kind.as_str());
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
        let bs = nb
            .borrow()
            .responses
            .get(&h)
            .and_then(|r| r.body.as_ref().map(|b| b.stream));
        Ok(bs.map_or(JsValue::NULL, JsValue::from_object))
    });
    let nb = Rc::clone(net);
    let ss = Rc::clone(streams);
    define_host_getter(vm, rp, "bodyUsed", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let used = nb
            .borrow()
            .responses
            .get(&h)
            .is_some_and(|r| body_is_used(&ss, r.body.as_ref()));
        Ok(JsValue::from_bool(used))
    });

    // Body mixin methods (always return Promises, backed by ReadableStream!)
    let nb = Rc::clone(net);
    let ss = Rc::clone(streams);
    define_host_method(vm, rp, "text", move |vm, this_val, _| {
        let h = match require_response(vm, &nb, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let body = nb.borrow().responses.get(&h).and_then(|r| r.body.clone());
        Ok(JsValue::from_object(consume_fetch_body_promise(
            vm,
            &ss,
            body.as_ref(),
            body_to_text,
        )))
    });
    let nb = Rc::clone(net);
    let ss = Rc::clone(streams);
    define_host_method(vm, rp, "json", move |vm, this_val, _| {
        let h = match require_response(vm, &nb, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let body = nb.borrow().responses.get(&h).and_then(|r| r.body.clone());
        Ok(JsValue::from_object(consume_fetch_body_promise(
            vm,
            &ss,
            body.as_ref(),
            body_to_json,
        )))
    });
    let nb = Rc::clone(net);
    let ss = Rc::clone(streams);
    define_host_method(vm, rp, "arrayBuffer", move |vm, this_val, _| {
        let h = match require_response(vm, &nb, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let body = nb.borrow().responses.get(&h).and_then(|r| r.body.clone());
        Ok(JsValue::from_object(consume_fetch_body_promise(
            vm,
            &ss,
            body.as_ref(),
            body_to_array_buffer,
        )))
    });
    let nb = Rc::clone(net);
    let ss = Rc::clone(streams);
    define_host_method(vm, rp, "bytes", move |vm, this_val, _| {
        let h = match require_response(vm, &nb, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let body = nb.borrow().responses.get(&h).and_then(|r| r.body.clone());
        Ok(JsValue::from_object(consume_fetch_body_promise(
            vm,
            &ss,
            body.as_ref(),
            body_to_bytes,
        )))
    });
    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    let ss = Rc::clone(streams);
    define_host_method(vm, rp, "blob", move |vm, this_val, _| {
        let h = match require_response(vm, &nb, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let (body, ct) = {
            let st = nb.borrow();
            let resp = st.responses.get(&h);
            let body = resp.and_then(|r| r.body.clone());
            let ct = resp.and_then(|r| {
                st.headers
                    .get(&r.headers_h)
                    .and_then(|he| he.get("content-type"))
            });
            (body, ct)
        };
        let wa_c = Rc::clone(&wa);
        Ok(JsValue::from_object(consume_fetch_body_promise(
            vm,
            &ss,
            body.as_ref(),
            move |vm, bytes| body_to_blob(vm, &wa_c, bytes, ct),
        )))
    });
    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    let ss = Rc::clone(streams);
    define_host_method(vm, rp, "formData", move |vm, this_val, _| {
        let h = match require_response(vm, &nb, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let (body, ct) = {
            let st = nb.borrow();
            let resp = st.responses.get(&h);
            let body = resp.and_then(|r| r.body.clone());
            let ct = resp.and_then(|r| {
                st.headers
                    .get(&r.headers_h)
                    .and_then(|he| he.get("content-type"))
            });
            (body, ct)
        };
        let wa_c = Rc::clone(&wa);
        Ok(JsValue::from_object(consume_fetch_body_promise(
            vm,
            &ss,
            body.as_ref(),
            move |vm, bytes| body_to_form_data(vm, &wa_c, bytes, ct),
        )))
    });

    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    let ss = Rc::clone(streams);
    define_host_method(vm, rp, "clone", move |vm, this_val, _| {
        let h = require_response(vm, &nb, this_val)?;
        let mut entry = nb.borrow().responses.get(&h).cloned().unwrap();
        let (b_orig, b_clone) = clone_fetch_body(vm, &wa, &ss, entry.body.as_ref())?;
        if let Some(resp_mut) = nb.borrow_mut().responses.get_mut(&h) {
            resp_mut.body = b_orig;
        }
        entry.body = b_clone;
        let hdr = nb
            .borrow()
            .headers
            .get(&entry.headers_h)
            .cloned()
            .unwrap_or_default();
        entry.headers_h = alloc_headers_from_entry(vm, &nb, hdr);
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
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("Response constructor requires 'new'"));
        }
        let body = extract_fetch_body(vm, &wa, &ss, arg(args, 0))?;
        let (status, status_text, mut hdr_entry) = parse_response_init(vm, &nb, arg(args, 1))?;
        if body.is_some() && is_null_body_status(status) {
            return Err(vm.type_error(format!(
                "Response with null body status ({status}) cannot have a body"
            )));
        }
        if let Some(ct) = body.as_ref().and_then(|b| b.content_type.clone()) {
            if !hdr_entry.has("content-type") {
                hdr_entry.push_unchecked("content-type", ct);
            }
        }
        let hdr_h = alloc_headers_from_entry(vm, &nb, hdr_entry);
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rp)));
        nb.borrow_mut().responses.insert(
            obj,
            ResponseEntry {
                url: String::new(),
                status,
                status_text,
                response_kind: ResponseKind::Default,
                redirected: false,
                headers_h: hdr_h,
                body,
            },
        );
        Ok(JsValue::from_object(obj))
    });

    let nb = Rc::clone(net);
    let ss = Rc::clone(streams);
    define_host_method(vm, ctor, "json", move |vm, _, args| {
        if args.is_empty() {
            return Err(vm.type_error("Response.json requires at least 1 argument"));
        }
        let k_json = vm.heap.intern_key("JSON");
        let k_str = vm.heap.intern_key("stringify");
        let json_v = vm.get_property_by_key(vm.global, k_json)?;
        let jh = json_v
            .as_object()
            .ok_or_else(|| vm.type_error("JSON unavailable"))?;
        let str_fn = vm.get_property_by_key(jh, k_str)?;
        let s_val = vm.call_value(str_fn, json_v, &[arg(args, 0)])?;
        if !s_val.is_string() {
            return Err(vm.type_error("Response.json data is not JSON serializable"));
        }
        let bytes = vm.to_js_string(s_val)?.into_bytes();
        let (status, status_text, mut hdr) = parse_response_init(vm, &nb, arg(args, 1))?;
        if is_null_body_status(status) {
            return Err(vm.type_error(format!(
                "Response.json with null body status ({status}) is invalid"
            )));
        }
        if !hdr.has("content-type") {
            hdr.set_unchecked("content-type", "application/json".to_string());
        }
        let hdr_h = alloc_headers_from_entry(vm, &nb, hdr);
        let stream = create_readable_byte_stream_from_bytes(vm, &ss, bytes.clone());
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rp)));
        nb.borrow_mut().responses.insert(
            obj,
            ResponseEntry {
                url: String::new(),
                status,
                status_text,
                response_kind: ResponseKind::Default,
                redirected: false,
                headers_h: hdr_h,
                body: Some(FetchBody {
                    stream,
                    source_bytes: Some(bytes),
                    content_type: Some("application/json".to_string()),
                }),
            },
        );
        Ok(JsValue::from_object(obj))
    });

    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    define_host_method(vm, ctor, "redirect", move |vm, _, args| {
        if args.is_empty() {
            return Err(vm.type_error("Response.redirect requires at least 1 argument"));
        }
        let raw_url = vm.to_js_string(arg(args, 0))?;
        let base_url = wa.borrow().location_url.clone();
        let parsed = base_url
            .join(&raw_url)
            .or_else(|_| url::Url::parse(&raw_url))
            .map_err(|_| vm.type_error(format!("Invalid redirect URL: '{raw_url}'")))?;
        let status = if arg(args, 1).is_undefined() {
            302u16
        } else {
            let n = vm.to_number(arg(args, 1))?;
            if n.is_finite() {
                (n.trunc() as i64).rem_euclid(65536) as u16
            } else {
                0
            }
        };
        if !matches!(status, 301 | 302 | 303 | 307 | 308) {
            return Err(vm.range_error(format!("Invalid redirect status: {status}")));
        }
        let mut hdr = HeadersEntry::with_guard(HeadersGuard::Immutable);
        hdr.push_unchecked("location", parsed.as_str().to_string());
        let hdr_h = alloc_headers_from_entry(vm, &nb, hdr);
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rp)));
        nb.borrow_mut().responses.insert(
            obj,
            ResponseEntry {
                url: String::new(),
                status,
                status_text: String::new(),
                response_kind: ResponseKind::Default,
                redirected: false,
                headers_h: hdr_h,
                body: None,
            },
        );
        Ok(JsValue::from_object(obj))
    });

    let nb = Rc::clone(net);
    define_host_method(vm, ctor, "error", move |vm, _, _| {
        let hdr_h =
            alloc_headers_from_entry(vm, &nb, HeadersEntry::with_guard(HeadersGuard::Immutable));
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rp)));
        nb.borrow_mut().responses.insert(
            obj,
            ResponseEntry {
                url: String::new(),
                status: 0,
                status_text: String::new(),
                response_kind: ResponseKind::Error,
                redirected: false,
                headers_h: hdr_h,
                body: None,
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

fn parse_fetch_cache(s: &str) -> FetchCache {
    match s {
        "no-store" => FetchCache::NoStore,
        "reload" => FetchCache::Reload,
        "no-cache" => FetchCache::NoCache,
        "force-cache" => FetchCache::ForceCache,
        "only-if-cached" => FetchCache::OnlyIfCached,
        _ => FetchCache::Default,
    }
}

/// Decodes RFC 2397 `data:` URLs per WHATWG Fetch §4.5 scheme fetch.
fn decode_data_url(url_str: &str) -> Option<(String, Vec<u8>)> {
    let rest = url_str.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let meta_trimmed = meta.trim();
    let (mime_raw, is_base64) = if let Some(prefix) = meta_trimmed.strip_suffix(";base64") {
        (prefix, true)
    } else if meta_trimmed.eq_ignore_ascii_case("base64") {
        ("", true)
    } else {
        (meta_trimmed, false)
    };
    let mime = if mime_raw.is_empty() {
        "text/plain;charset=US-ASCII".to_string()
    } else {
        mime_raw.to_string()
    };
    let raw_bytes = percent_decode_bytes(payload);
    let body = if is_base64 {
        forgiving_base64_decode(&raw_bytes)?
    } else {
        raw_bytes
    };
    Some((mime, body))
}

fn percent_decode_bytes(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn forgiving_base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut clean = Vec::with_capacity(input.len());
    for &b in input {
        if !matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0C) {
            clean.push(b);
        }
    }
    while clean.ends_with(b"=") {
        clean.pop();
    }
    if clean.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits: u8 = 0;
    for b in clean {
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(val);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

const KEEPALIVE_QUOTA_BYTES: usize = 65_536;

fn install_fetch_fn(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    net: &SharedNetBridge,
) {
    let wa = Rc::clone(web_apis);
    let ss = Rc::clone(streams);
    let nb = Rc::clone(net);
    let fetch_fn = vm.alloc_host_fn("fetch", move |vm, _, args| {
        let promise = alloc_promise(vm);
        let entry = match build_request_entry(vm, &wa, &ss, &nb, arg(args, 0), arg(args, 1)) {
            Ok(e) => e,
            Err(e) => {
                reject_promise(vm, promise, e.value);
                return Ok(JsValue::from_object(promise));
            }
        };
        let abort_info = abort_info_for_signal(&wa, JsValue::from_object(entry.signal_h));
        if let Some((_, true, reason)) = abort_info {
            let r = if reason.is_undefined() {
                create_dom_exception(vm, "AbortError", "The operation was aborted.").value
            } else {
                reason
            };
            reject_promise(vm, promise, r);
            return Ok(JsValue::from_object(promise));
        }

        // WHATWG Fetch §4.5 Scheme Fetch: `data:` URLs.
        if entry.url.starts_with("data:") {
            match decode_data_url(&entry.url) {
                Some((mime, bytes)) => {
                    let mut headers = HashMap::new();
                    headers.insert("content-type".to_string(), mime);
                    let resp_h = alloc_response_from_fetch(
                        vm,
                        &ss,
                        &nb,
                        FetchResponse {
                            url: entry.url,
                            status: 200,
                            status_text: "OK".to_string(),
                            headers,
                            body: bytes,
                            response_type: FetchResponseType::Basic,
                            redirected: false,
                        },
                    );
                    resolve_promise(vm, promise, JsValue::from_object(resp_h));
                }
                None => {
                    let err = vm.type_error("Invalid data: URL").value;
                    reject_promise(vm, promise, err);
                }
            }
            return Ok(JsValue::from_object(promise));
        }

        // WHATWG Fetch §4.5 Scheme Fetch: `blob:` URLs (GET only).
        if entry.url.starts_with("blob:") {
            if !entry.method.eq_ignore_ascii_case("GET") {
                let err = vm.type_error("blob: URLs only support GET requests").value;
                reject_promise(vm, promise, err);
                return Ok(JsValue::from_object(promise));
            }
            let lookup_url = entry
                .url
                .split('#')
                .next()
                .unwrap_or(&entry.url)
                .to_string();
            let found = {
                let st = wa.borrow();
                st.object_urls
                    .get(&lookup_url)
                    .cloned()
                    .map(|bytes| {
                        let ct = st
                            .object_url_types
                            .get(&lookup_url)
                            .cloned()
                            .unwrap_or_default();
                        (bytes, ct)
                    })
                    .or_else(|| crate::web_apis::lookup_global_object_url(&lookup_url))
            };
            match found {
                Some((bytes, ct)) => {
                    let mut headers = HashMap::new();
                    headers.insert("content-length".to_string(), bytes.len().to_string());
                    headers.insert("content-type".to_string(), ct);
                    let resp_h = alloc_response_from_fetch(
                        vm,
                        &ss,
                        &nb,
                        FetchResponse {
                            url: entry.url,
                            status: 200,
                            status_text: "OK".to_string(),
                            headers,
                            body: bytes,
                            response_type: FetchResponseType::Basic,
                            redirected: false,
                        },
                    );
                    resolve_promise(vm, promise, JsValue::from_object(resp_h));
                }
                None => {
                    let err = vm.type_error("Blob URL not found or revoked").value;
                    reject_promise(vm, promise, err);
                }
            }
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
        let req_body_bytes = entry
            .body
            .as_ref()
            .and_then(|b| b.source_bytes.clone())
            .unwrap_or_default();

        // WHATWG Fetch §4.3 keepalive rolling 64 KiB body quota.
        let keepalive_bytes = if entry.keepalive {
            let sz = req_body_bytes.len();
            let cur = nb.borrow().keepalive_inflight_bytes;
            if cur.saturating_add(sz) > KEEPALIVE_QUOTA_BYTES {
                let err = vm
                    .type_error("Keepalive request body exceeds 64 KiB quota limit")
                    .value;
                reject_promise(vm, promise, err);
                return Ok(JsValue::from_object(promise));
            }
            sz
        } else {
            0
        };

        let fetch_req = FetchRequest {
            url: entry.url,
            method: entry.method,
            headers: headers_map,
            body: req_body_bytes,
            mode: entry.mode,
            credentials: entry.credentials,
            cache: parse_fetch_cache(&entry.cache),
            redirect: entry.redirect,
            origin,
            abort_signal,
        };

        let (id, engine, tx, interceptor) = {
            let mut st = nb.borrow_mut();
            let id = st.alloc_id();
            st.keepalive_inflight_bytes =
                st.keepalive_inflight_bytes.saturating_add(keepalive_bytes);
            st.pending_fetches.insert(
                id,
                PendingFetch {
                    promise,
                    abort_reason,
                    keepalive_bytes,
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
// WebSocket (WHATWG WebSockets §9.3 backed by rix_net::WebSocketConnection)
// ---------------------------------------------------------------------------

fn lookup_global_event_proto(vm: &mut JsVm, events: &SharedEvents, name: &str) -> HeapHandle {
    if let Some(&p) = events.borrow().event_protos.get(name) {
        return p;
    }
    let k = vm.heap.intern_key(name);
    let kp = vm.key_prototype;
    vm.get_property_by_key(vm.global, k)
        .ok()
        .and_then(JsValue::as_object)
        .and_then(|c| vm.get_property_by_key(c, kp).ok())
        .and_then(JsValue::as_object)
        .or_else(|| events.borrow().event_proto)
        .unwrap_or(vm.object_proto)
}

pub(crate) fn clamp_u16(n: f64) -> u16 {
    if n.is_nan() || n <= 0.0 {
        0
    } else if n >= 65535.0 {
        65535
    } else {
        n.round_ties_even() as u16
    }
}

pub(crate) fn is_valid_ws_subprotocol(proto: &str) -> bool {
    !proto.is_empty()
        && proto.bytes().all(|b| {
            (0x21..=0x7E).contains(&b)
                && !matches!(
                    b,
                    b'(' | b')'
                        | b'<'
                        | b'>'
                        | b'@'
                        | b','
                        | b';'
                        | b':'
                        | b'\\'
                        | b'"'
                        | b'/'
                        | b'['
                        | b']'
                        | b'?'
                        | b'='
                        | b'{'
                        | b'}'
                )
        })
}

pub(crate) fn resolve_ws_url(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    raw_url: &str,
) -> Result<(String, String), JsException> {
    let base = web_apis.borrow().location_url.clone();
    let mut parsed = url::Url::options()
        .base_url(Some(&base))
        .parse(raw_url)
        .map_err(|_| {
            create_dom_exception(
                vm,
                "SyntaxError",
                &format!("Invalid WebSocket URL: {raw_url}"),
            )
        })?;
    match parsed.scheme() {
        "http" => {
            let _ = parsed.set_scheme("ws");
        }
        "https" => {
            let _ = parsed.set_scheme("wss");
        }
        "ws" | "wss" => {}
        _ => {
            return Err(create_dom_exception(
                vm,
                "SyntaxError",
                &format!("Unsupported WebSocket scheme: {}", parsed.scheme()),
            ));
        }
    }
    if parsed.fragment().is_some() {
        return Err(create_dom_exception(
            vm,
            "SyntaxError",
            "WebSocket URL must not contain a fragment",
        ));
    }
    let serialized = parsed.as_str().to_string();
    let origin = parsed.origin().ascii_serialization();
    Ok((serialized, origin))
}

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
    set_to_string_tag(vm, wsp, "WebSocket");
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
                PropertyAttrs::ENUMERABLE,
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
    define_host_getter(vm, wsp, "extensions", move |vm, this_val, _| {
        let id = require_ws_id(vm, &nb, this_val)?;
        let s = nb
            .borrow()
            .websockets
            .get(&id)
            .map(|w| w.extensions.clone())
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, wsp, "bufferedAmount", move |vm, this_val, _| {
        let id = require_ws_id(vm, &nb, this_val)?;
        let amt = nb
            .borrow()
            .websockets
            .get(&id)
            .map_or(0, |w| w.buffered_amount);
        Ok(JsValue::from_f64(amt as f64))
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
            let val = vm.to_js_string(arg(args, 0))?;
            if val == "blob" || val == "arraybuffer" {
                if let Some(w) = nb.borrow_mut().websockets.get_mut(&id) {
                    w.binary_type = val;
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    // Event handler IDL attributes (`onopen`, `onmessage`, `onerror`, `onclose`) per HTML §8.1.7.1.
    for &prop in &["onopen", "onmessage", "onerror", "onclose"] {
        let nb_v = Rc::clone(net);
        install_object_event_handler_accessor(vm, wsp, events, prop, move |vm, this_val| {
            let _ = require_ws_id(vm, &nb_v, this_val)?;
            this_val
                .as_object()
                .ok_or_else(|| vm.type_error("Incompatible WebSocket receiver"))
        });
    }

    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    define_host_method(vm, wsp, "send", move |vm, this_val, args| {
        let id = require_ws_id(vm, &nb, this_val)?;
        if args.is_empty() {
            return Err(vm.type_error("WebSocket.send requires 1 argument"));
        }
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
        let data = arg(args, 0);
        let is_binary_obj = data.as_object().is_some_and(|h| {
            wa.borrow().blobs.contains_key(&h)
                || matches!(
                    vm.heap.get_object(h).map(|o| &o.kind),
                    Some(
                        ObjectKind::ArrayBuffer(_)
                            | ObjectKind::TypedArray(_)
                            | ObjectKind::DataView(_)
                    )
                )
        });
        if is_binary_obj {
            let bytes = extract_bytes_from_js(vm, &wa, data)?;
            let byte_len = bytes.len() as u64;
            if let Some(w) = nb.borrow_mut().websockets.get_mut(&id) {
                w.buffered_amount = w.buffered_amount.saturating_add(byte_len);
            }
            if ready_state == 1 {
                let _ = cmd_tx.send(WsCommand::SendBinary(bytes));
            }
        } else {
            let text = vm.to_js_string(data)?;
            let byte_len = text.len() as u64;
            if let Some(w) = nb.borrow_mut().websockets.get_mut(&id) {
                w.buffered_amount = w.buffered_amount.saturating_add(byte_len);
            }
            if ready_state == 1 {
                let _ = cmd_tx.send(WsCommand::SendText(text));
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let nb = Rc::clone(net);
    define_host_method(vm, wsp, "close", move |vm, this_val, args| {
        let id = require_ws_id(vm, &nb, this_val)?;
        let code_arg = arg(args, 0);
        let code = if code_arg.is_undefined() {
            None
        } else {
            let n = vm.to_number(code_arg)?;
            // WebIDL `[Clamp] unsigned short`.
            let raw_u16 = clamp_u16(n);
            if raw_u16 != 1000 && !(3000..=4999).contains(&raw_u16) {
                return Err(create_dom_exception(
                    vm,
                    "InvalidAccessError",
                    &format!("Invalid WebSocket close code: {raw_u16}"),
                ));
            }
            Some(raw_u16)
        };
        let reason_arg = arg(args, 1);
        let reason = if reason_arg.is_undefined() {
            String::new()
        } else {
            let r = vm.to_js_string(reason_arg)?;
            if r.len() > 123 {
                return Err(create_dom_exception(
                    vm,
                    "SyntaxError",
                    "WebSocket close reason must not be greater than 123 UTF-8 bytes",
                ));
            }
            r
        };
        let (was_connecting, abort_tx, cmd_tx) = {
            let mut st = nb.borrow_mut();
            let Some(w) = st.websockets.get_mut(&id) else {
                return Ok(JsValue::UNDEFINED);
            };
            if w.ready_state >= 2 {
                return Ok(JsValue::UNDEFINED);
            }
            let was_conn = w.ready_state == 0;
            w.ready_state = 2;
            (was_conn, w.abort_connect.take(), w.cmd_tx.clone())
        };
        if was_connecting {
            if let Some(tx) = abort_tx {
                let _ = tx.send(());
            }
        } else {
            let _ = cmd_tx.send(WsCommand::Close { code, reason });
        }
        Ok(JsValue::UNDEFINED)
    });

    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    let h_ws = Rc::clone(host);
    let evs_ws = Rc::clone(events);
    let el_ws = Rc::clone(event_loop);
    let ctor = vm.alloc_host_fn("WebSocket", move |vm, _, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("WebSocket constructor requires 'new'"));
        }
        if args.is_empty() {
            return Err(vm.type_error("WebSocket constructor requires at least 1 argument"));
        }
        let raw_url = vm.to_js_string(arg(args, 0))?;
        let (resolved_url, ws_origin) = resolve_ws_url(vm, &wa, &raw_url)?;
        crate::trusted_types::check_connect_src_allowed(
            vm,
            &h_ws,
            &evs_ws,
            &el_ws,
            &resolved_url,
        )?;

        let mut protocols = Vec::new();
        let proto_arg = arg(args, 1);
        if !proto_arg.is_undefined() {
            if let Some(h) = proto_arg.as_object() {
                let sym = vm.key_symbol_iterator;
                let is_seq = vm
                    .get_property_by_key(h, sym)
                    .ok()
                    .and_then(JsValue::as_object)
                    .is_some_and(|f| vm.is_callable_handle(f));
                let is_boxed_string = vm
                    .heap
                    .get_object(h)
                    .and_then(|o| o.prototype)
                    == Some(vm.string_proto);
                if is_seq && !is_boxed_string {
                    for p in vm.collect_iterable(proto_arg)? {
                        protocols.push(vm.to_js_string(p)?);
                    }
                } else if !is_boxed_string {
                    // Option bag dictionary `{ protocols?: sequence<DOMString> }` (WHATWG WebSocket).
                    let kp = vm.heap.intern_key("protocols");
                    let vp = vm.get_property_by_key(h, kp)?;
                    if !vp.is_undefined() {
                        if vp.as_object().is_some() {
                            for p in vm.collect_iterable(vp)? {
                                protocols.push(vm.to_js_string(p)?);
                            }
                        } else {
                            return Err(vm.type_error("WebSocketInit.protocols must be iterable"));
                        }
                    }
                } else {
                    protocols.push(vm.to_js_string(proto_arg)?);
                }
            } else {
                protocols.push(vm.to_js_string(proto_arg)?);
            }
        }
        // Validate subprotocols per WHATWG WebSockets §9.3 / RFC 6455 §4.1 (ASCII case-insensitive uniqueness).
        let mut seen = std::collections::HashSet::with_capacity(protocols.len());
        for p in &protocols {
            if !is_valid_ws_subprotocol(p) {
                return Err(create_dom_exception(
                    vm,
                    "SyntaxError",
                    &format!("Invalid WebSocket subprotocol: '{p}'"),
                ));
            }
            if !seen.insert(p.to_ascii_lowercase()) {
                return Err(create_dom_exception(
                    vm,
                    "SyntaxError",
                    &format!("Duplicate WebSocket subprotocol: '{p}'"),
                ));
            }
        }

        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(wsp)));
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<WsCommand>();
        let (abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<()>();
        let origin = wa.borrow().current_origin();
        let mut extra_headers: Vec<(String, String)> = Vec::new();
        if let Ok(parsed_u) = url::Url::parse(&resolved_url) {
            let cookie_hdr = wa.borrow_mut().cookie_header_for_url(&parsed_u, false);
            if !cookie_hdr.is_empty() {
                extra_headers.push(("Cookie".to_string(), cookie_hdr));
            }
        }
        let (id, ev_tx) = {
            let mut st = nb.borrow_mut();
            let id = st.alloc_id();
            st.websockets.insert(
                id,
                JsWebSocketEntry {
                    js_obj: obj,
                    url: resolved_url.clone(),
                    origin: ws_origin,
                    protocol: String::new(),
                    extensions: String::new(),
                    ready_state: 0,
                    binary_type: "blob".to_string(),
                    buffered_amount: 0,
                    cmd_tx,
                    abort_connect: Some(abort_tx),
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
            let connect_res = tokio::select! {
                biased;
                _ = &mut abort_rx => {
                    let _ = ev_tx.send(AsyncNetEvent::WsError {
                        id,
                        message: "WebSocket was closed before connection was established".to_string(),
                    });
                    return;
                }
                res = WebSocketConnection::connect_with_headers(
                    &resolved_url,
                    Some(&origin),
                    &protocols,
                    &extra_headers,
                ) => res,
            };
            let mut conn = match connect_res {
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
                extensions: conn.negotiated_extensions.clone(),
                set_cookies: std::mem::take(&mut conn.response_set_cookies),
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
                            Some(Ok(WsMessage::Sent { bytes })) => {
                                let _ = ev_tx.send(AsyncNetEvent::WsDrained { id, bytes });
                            }
                            Some(Ok(WsMessage::Close { code, reason, was_clean })) => {
                                let _ = ev_tx.send(AsyncNetEvent::WsClosed { id, code, reason, was_clean });
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
                                    was_clean: false,
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

    let xhrs = Rc::clone(&net.borrow().xhrs);

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
            if args.len() < 2 {
                return Err(vm.type_error("XMLHttpRequest.setRequestHeader requires 2 arguments"));
            }
            let ready = xm.borrow().get(&h).map_or(0, |st| st.ready_state);
            if ready != 1 {
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "XMLHttpRequest must be OPENED",
                ));
            }
            let name = to_byte_string(vm, arg(args, 0))?;
            let raw_value = to_byte_string(vm, arg(args, 1))?;
            if !is_valid_header_name(&name) {
                return Err(create_dom_exception(
                    vm,
                    "SyntaxError",
                    &format!("Invalid header name: '{name}'"),
                ));
            }
            let value = normalize_and_validate_header_value(&raw_value)
                .map_err(|msg| create_dom_exception(vm, "SyntaxError", msg))?;
            let lower = name.to_ascii_lowercase();
            if is_forbidden_request_header(&lower, &value) {
                return Ok(JsValue::UNDEFINED);
            }
            if let Some(st) = xm.borrow_mut().get_mut(&h) {
                st.req_headers
                    .entry(lower)
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
            let (method, url, async_flag, headers, override_mime) = {
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
                    st.async_flag,
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
                cache: FetchCache::Default,
                redirect: FetchRedirect::Follow,
                origin,
                abort_signal: None,
            };
            // WHATWG XHR §4.5.7: Asynchronous `XMLHttpRequest.send()` MUST return immediately
            // without blocking the main JS thread, delivering `readystatechange`/`load`/`error`
            // on the event loop once the background fetch finishes.
            if async_flag {
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let tx = {
                        let mut bridge = nb.borrow_mut();
                        bridge.pending_xhrs.insert(h, ());
                        bridge.async_tx.clone()
                    };
                    handle.spawn(async move {
                        let res = tokio::time::timeout(
                            std::time::Duration::from_secs(6),
                            engine.fetch(req),
                        )
                        .await
                        .ok()
                        .and_then(|r| r.ok());
                        let _ = tx.send(AsyncNetEvent::XhrCompleted {
                            xhr_h: h,
                            override_mime,
                            result: res,
                        });
                    });
                    return Ok(JsValue::UNDEFINED);
                }
            }
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
