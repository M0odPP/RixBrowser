//! Out-of-Process Iframes (OOPIF), `HTMLIFrameElement`, `sandbox` enforcement,
//! `window.open` / `window.postMessage`, `MessageEvent`, `MessageChannel`,
//! and `MessagePort` across same-origin and cross-process browsing contexts.
//!
//! Reference: Spec 02 §4, Spec 06 §3, Issue #22.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use rix_dom::{NodeData, NodeId};
use rix_html::{decode_html_bytes, parse_html};
use rix_ipc::{ContentToBrowser, MessagePortId, PipelineId, SurfaceId};
use rix_js::heap::{TypedArrayKind, TypedArrayObject};
use rix_js::{
    HeapHandle, JsBigInt, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs,
};
use rix_layout::surface_id_for_node;
use url::Url;

use crate::dom_bridge::{set_element_attr, wrap_node, SharedHost};
use crate::event_loop::{SharedEventLoop, TaskPriority};
use crate::events::{
    fire_dom_node_event, fire_object_event_with_proto, register_constructed_event, SharedEvents,
};
use crate::fetch_ws::SharedNetBridge;
use crate::web_apis::{
    alloc_array_buffer, apply_url_host_setter, apply_url_port_setter, arg, create_dom_exception,
    define_global, define_host_accessor, define_host_getter, define_host_method,
    form_urlencode_pairs_with_charset, origin_of_url, parse_url_with_charset, strip_ascii_tab_lf,
    structured_clone_inner, SharedWebApis,
};

#[inline]
fn node_id_u64(id: NodeId) -> u64 {
    usize::from(id) as u64
}

fn alloc_array_from(vm: &mut JsVm, items: impl IntoIterator<Item = JsValue>) -> HeapHandle {
    let arr = vm.alloc_array();
    for v in items {
        vm.heap.object_push_element(arr, v);
    }
    arr
}

#[inline]
fn is_js_array(vm: &JsVm, h: HeapHandle) -> bool {
    vm.heap
        .get_object(h)
        .is_some_and(|o| matches!(o.kind, ObjectKind::Array))
}

#[inline]
fn array_len(vm: &JsVm, h: HeapHandle) -> usize {
    vm.heap
        .get_object(h)
        .map_or(0, |o| o.elements.len() as usize)
}

#[inline]
fn array_elem(vm: &JsVm, h: HeapHandle, i: usize) -> JsValue {
    let Ok(idx) = u32::try_from(i) else {
        return JsValue::UNDEFINED;
    };
    vm.heap
        .get_object(h)
        .and_then(|o| o.elements.get(idx))
        .unwrap_or(JsValue::UNDEFINED)
}

static NEXT_GLOBAL_PORT_ID: AtomicU64 = AtomicU64::new(1);

fn alloc_port_id(pipeline: PipelineId) -> MessagePortId {
    let seq = NEXT_GLOBAL_PORT_ID.fetch_add(1, Ordering::Relaxed);
    // Mix pipeline ID into high 20 bits so distinct OS content processes never collide.
    MessagePortId(((pipeline.0 & 0xF_FFFF) << 44) | (seq & 0x0FFF_FFFF_FFFF))
}

// ---------------------------------------------------------------------------
// WHATWG HTML `sandbox` attribute flags
// ---------------------------------------------------------------------------

/// Parsed WHATWG HTML `<iframe sandbox="...">` enforcement flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SandboxFlags {
    pub active: bool,
    pub allow_scripts: bool,
    pub allow_same_origin: bool,
    pub allow_forms: bool,
    pub allow_popups: bool,
    pub allow_top_navigation: bool,
    pub allow_modals: bool,
    pub allow_downloads: bool,
    pub allow_popups_to_escape_sandbox: bool,
}

impl SandboxFlags {
    /// Parses an optional `sandbox` attribute value. `None` means unsandboxed.
    #[must_use]
    pub fn parse(attr: Option<&str>) -> Self {
        let Some(raw) = attr else {
            return Self::default();
        };
        let mut flags = Self {
            active: true,
            ..Self::default()
        };
        for token in raw.split_ascii_whitespace() {
            match token.to_ascii_lowercase().as_str() {
                "allow-scripts" => flags.allow_scripts = true,
                "allow-same-origin" => flags.allow_same_origin = true,
                "allow-forms" => flags.allow_forms = true,
                "allow-popups" => flags.allow_popups = true,
                "allow-top-navigation" | "allow-top-navigation-by-user-activation" => {
                    flags.allow_top_navigation = true;
                }
                "allow-modals" => flags.allow_modals = true,
                "allow-downloads" => flags.allow_downloads = true,
                "allow-popups-to-escape-sandbox" => flags.allow_popups_to_escape_sandbox = true,
                _ => {}
            }
        }
        flags
    }

    /// Returns the effective serialized origin (`"null"` when opaque-sandboxed).
    #[must_use]
    pub fn effective_origin(&self, raw_origin: &str) -> String {
        if self.active && !self.allow_same_origin {
            "null".to_string()
        } else {
            raw_origin.to_string()
        }
    }

    #[must_use]
    pub const fn scripts_allowed(&self) -> bool {
        !self.active || self.allow_scripts
    }

    #[must_use]
    pub const fn popups_allowed(&self) -> bool {
        !self.active || self.allow_popups
    }

    #[must_use]
    pub const fn forms_allowed(&self) -> bool {
        !self.active || self.allow_forms
    }

    /// Per WHATWG HTML, opaque (`"null"`) origins are NEVER same-origin even with each other.
    #[must_use]
    pub fn same_origin_with(&self, parent_origin: &str, child_raw_origin: &str) -> bool {
        if self.active && !self.allow_same_origin {
            return false;
        }
        if parent_origin == "null" || child_raw_origin == "null" {
            return false;
        }
        parent_origin == child_raw_origin
    }
}

// ---------------------------------------------------------------------------
// Internal MessagePort, WindowProxy, and Iframe state
// ---------------------------------------------------------------------------

struct QueuedPortMessage {
    data: JsValue,
    ports: Vec<HeapHandle>,
}

struct MessagePortState {
    id: MessagePortId,
    partner_id: Option<MessagePortId>,
    remote_pipeline: Option<PipelineId>,
    started: bool,
    closed: bool,
    neutered: bool,
    onmessage: JsValue,
    onmessageerror: JsValue,
    pending: VecDeque<QueuedPortMessage>,
}

#[derive(Debug, Clone)]
enum WindowProxyKind {
    /// Same-origin `<iframe>` sharing script access within the browsing-context group.
    SameOriginChild {
        iframe_node: NodeId,
        child_doc: HeapHandle,
        origin: String,
        url: String,
        name: String,
        closed: bool,
    },
    /// Cross-origin `<iframe>` (or opaque sandbox) routed to an isolated OOPIF process.
    CrossOriginRemote {
        remote_pipeline: Option<PipelineId>,
        origin: String,
        url: String,
        closed: bool,
        crashed: bool,
    },
    /// Auxiliary window opened via `window.open(...)`.
    Auxiliary {
        is_cross_origin: bool,
        origin: String,
        url: String,
        name: String,
        closed: bool,
        opener: JsValue,
        doc: Option<HeapHandle>,
    },
}

struct IframeRecord {
    node_id: NodeId,
    surface_id: SurfaceId,
    proxy: HeapHandle,
    is_cross_origin: bool,
    remote_pipeline: Option<PipelineId>,
    last_src: String,
    last_srcdoc: Option<String>,
    last_sandbox: Option<String>,
    crashed: bool,
}

/// Shared browsing-context, OOPIF, `MessageChannel`, and `postMessage` registry.
pub struct FrameSystem {
    pub pipeline_id: PipelineId,
    pub sandbox: SandboxFlags,
    pub window_name: String,
    pub window_closed: bool,
    pub opener_window: JsValue,
    pub parent_window: JsValue,
    pub top_window: JsValue,
    pub frame_element: JsValue,
    pub message_event_proto: Option<HeapHandle>,
    pub message_port_proto: Option<HeapHandle>,
    pub window_proxy_proto: Option<HeapHandle>,
    ports: HashMap<HeapHandle, MessagePortState>,
    port_by_id: HashMap<MessagePortId, HeapHandle>,
    proxies: HashMap<HeapHandle, WindowProxyKind>,
    remote_windows: HashMap<PipelineId, HeapHandle>,
    iframes: HashMap<NodeId, IframeRecord>,
    iframe_order: Vec<NodeId>,
    auxiliary_windows: Vec<HeapHandle>,
    detached_buffers: HashSet<HeapHandle>,
    outbound_ipc: VecDeque<ContentToBrowser>,
}

/// Shared handle to [`FrameSystem`].
pub type SharedFrames = Rc<RefCell<FrameSystem>>;

impl FrameSystem {
    #[must_use]
    pub fn new(global: HeapHandle) -> Self {
        let gv = JsValue::from_object(global);
        Self {
            pipeline_id: PipelineId(1),
            sandbox: SandboxFlags::default(),
            window_name: String::new(),
            window_closed: false,
            opener_window: JsValue::NULL,
            parent_window: gv,
            top_window: gv,
            frame_element: JsValue::NULL,
            message_event_proto: None,
            message_port_proto: None,
            window_proxy_proto: None,
            ports: HashMap::new(),
            port_by_id: HashMap::new(),
            proxies: HashMap::new(),
            remote_windows: HashMap::new(),
            iframes: HashMap::new(),
            iframe_order: Vec::new(),
            auxiliary_windows: Vec::new(),
            detached_buffers: HashSet::new(),
            outbound_ipc: VecDeque::new(),
        }
    }

    /// Returns the effective serialized origin of this window (respecting `sandbox`).
    #[must_use]
    pub fn effective_origin(&self, web_apis: &SharedWebApis) -> String {
        let raw = web_apis.borrow().current_origin();
        self.sandbox.effective_origin(&raw)
    }

    /// Drains all outbound OOPIF / cross-process messaging IPC events emitted by this frame.
    pub fn drain_outbound_ipc(&mut self) -> Vec<ContentToBrowser> {
        self.outbound_ipc.drain(..).collect()
    }

    /// Returns the `SurfaceId` associated with a tracked `<iframe>` node, if any.
    #[must_use]
    pub fn iframe_surface_id(&self, node_id: NodeId) -> Option<SurfaceId> {
        self.iframes.get(&node_id).map(|r| r.surface_id)
    }

    /// Returns whether a tracked `<iframe>` node is currently marked crashed.
    #[must_use]
    pub fn iframe_is_crashed(&self, node_id: NodeId) -> bool {
        self.iframes.get(&node_id).is_some_and(|r| r.crashed)
    }

    /// Collects GC roots for prototypes, proxies, ports, and queued messages.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.message_event_proto,
            self.message_port_proto,
            self.window_proxy_proto,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(p));
        }
        out.push(self.opener_window);
        out.push(self.parent_window);
        out.push(self.top_window);
        out.push(self.frame_element);
        for (&h, port) in &self.ports {
            if !port.closed && !port.neutered {
                out.push(JsValue::from_object(h));
            }
            out.push(port.onmessage);
            out.push(port.onmessageerror);
            for q in &port.pending {
                out.push(q.data);
                for &ph in &q.ports {
                    out.push(JsValue::from_object(ph));
                }
            }
        }
        for rec in self.iframes.values() {
            out.push(JsValue::from_object(rec.proxy));
        }
        for &h in &self.auxiliary_windows {
            out.push(JsValue::from_object(h));
        }
        for &h in self.remote_windows.values() {
            out.push(JsValue::from_object(h));
        }
        for kind in self.proxies.values() {
            match kind {
                WindowProxyKind::SameOriginChild { child_doc, .. } => {
                    out.push(JsValue::from_object(*child_doc));
                }
                WindowProxyKind::Auxiliary { opener, doc, .. } => {
                    out.push(*opener);
                    if let Some(d) = doc {
                        out.push(JsValue::from_object(*d));
                    }
                }
                WindowProxyKind::CrossOriginRemote { .. } => {}
            }
        }
    }

    /// Removes dead GC entries for unreachable closed ports and proxies.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.ports.retain(|&h, p| {
            let alive = heap.is_alive(h);
            if !alive {
                self.port_by_id.remove(&p.id);
            }
            alive
        });
        self.proxies.retain(|&h, _| heap.is_alive(h));
        self.detached_buffers.retain(|&h| heap.is_alive(h));
    }
}

// ---------------------------------------------------------------------------
// Cross-Process Structured JSON Codec (preserves JS types cleanly over IPC)
// ---------------------------------------------------------------------------

/// Serializes a JS value into a rich tagged JSON payload for cross-process IPC,
/// rejecting non-cloneable objects (Functions, Symbols) with `DataCloneError`.
pub(crate) fn encode_structured_json(
    vm: &mut JsVm,
    val: JsValue,
    seen: &mut HashSet<HeapHandle>,
) -> Result<serde_json::Value, JsException> {
    if val.is_undefined() {
        return Ok(serde_json::json!({ "$t": "undef" }));
    }
    if val.is_null() {
        return Ok(serde_json::Value::Null);
    }
    if let Some(b) = val.as_bool() {
        return Ok(serde_json::Value::Bool(b));
    }
    if let Some(n) = val.as_number() {
        if n.is_nan() {
            return Ok(serde_json::json!({ "$t": "num", "v": "NaN" }));
        }
        if n.is_infinite() {
            return Ok(serde_json::json!({
                "$t": "num",
                "v": if n.is_sign_positive() { "Inf" } else { "-Inf" }
            }));
        }
        if n == 0.0 && n.is_sign_negative() {
            return Ok(serde_json::json!({ "$t": "num", "v": "-0" }));
        }
        if let Some(num) = serde_json::Number::from_f64(n) {
            return Ok(serde_json::Value::Number(num));
        }
        return Ok(serde_json::Value::Null);
    }
    if val.is_string() {
        let s = vm.value_to_rust_string(val);
        return Ok(serde_json::Value::String(s));
    }
    if let Some(bh) = val.as_bigint() {
        let s = vm
            .heap
            .get_bigint(bh)
            .map_or_else(|| "0".to_string(), JsBigInt::to_decimal_string);
        return Ok(serde_json::json!({ "$t": "bigint", "v": s }));
    }
    if val.is_symbol() {
        return Err(create_dom_exception(
            vm,
            "DataCloneError",
            "Symbol could not be cloned",
        ));
    }
    let Some(h) = val.as_object() else {
        return Ok(serde_json::Value::Null);
    };
    if vm.is_callable_handle(h) {
        return Err(create_dom_exception(
            vm,
            "DataCloneError",
            "Function could not be cloned",
        ));
    }
    if !seen.insert(h) {
        return Err(create_dom_exception(
            vm,
            "DataCloneError",
            "Cyclic structure cannot be serialized across processes",
        ));
    }
    let kind_tag = vm.heap.get_object(h).map(|o| match &o.kind {
        ObjectKind::Array => 1u8,
        ObjectKind::ArrayBuffer(_) => 2,
        ObjectKind::TypedArray(_) => 3,
        ObjectKind::Date(_) => 4,
        ObjectKind::Map(_) => 5,
        ObjectKind::Set(_) => 6,
        _ => 7,
    });
    let res = match kind_tag {
        Some(1) => {
            let len = array_len(vm, h);
            let mut arr = Vec::with_capacity(len);
            for i in 0..len {
                let elem = array_elem(vm, h, i);
                arr.push(encode_structured_json(vm, elem, seen)?);
            }
            Ok(serde_json::Value::Array(arr))
        }
        Some(2) => {
            let bytes = match vm.heap.get_object(h).map(|o| &o.kind) {
                Some(ObjectKind::ArrayBuffer(b)) => (**b).clone(),
                _ => Vec::new(),
            };
            Ok(serde_json::json!({ "$t": "ab", "v": bytes }))
        }
        Some(3) => {
            let (kind, slice) = match vm.heap.get_object(h).map(|o| &o.kind) {
                Some(ObjectKind::TypedArray(ta)) => {
                    let bpe = ta.kind.bytes_per_element();
                    let off = ta.byte_offset as usize;
                    let len = ta.length as usize * bpe;
                    let raw = match vm.heap.get_object(ta.buffer).map(|o| &o.kind) {
                        Some(ObjectKind::ArrayBuffer(b)) => {
                            let end = (off + len).min(b.len());
                            if off <= end {
                                b[off..end].to_vec()
                            } else {
                                Vec::new()
                            }
                        }
                        _ => Vec::new(),
                    };
                    (format!("{:?}", ta.kind), raw)
                }
                _ => ("Uint8".to_string(), Vec::new()),
            };
            Ok(serde_json::json!({ "$t": "ta", "k": kind, "v": slice }))
        }
        Some(4) => {
            let ms = match vm.heap.get_object(h).map(|o| &o.kind) {
                Some(ObjectKind::Date(d)) => *d,
                _ => f64::NAN,
            };
            Ok(serde_json::json!({ "$t": "date", "v": ms }))
        }
        Some(5) => {
            let entries: Vec<(JsValue, JsValue)> = match vm.heap.get_object(h).map(|o| &o.kind) {
                Some(ObjectKind::Map(m)) => (**m).clone(),
                _ => Vec::new(),
            };
            let mut pairs = Vec::with_capacity(entries.len());
            for (k, v) in entries {
                pairs.push(serde_json::json!([
                    encode_structured_json(vm, k, seen)?,
                    encode_structured_json(vm, v, seen)?
                ]));
            }
            Ok(serde_json::json!({ "$t": "map", "v": pairs }))
        }
        Some(6) => {
            let items: Vec<JsValue> = match vm.heap.get_object(h).map(|o| &o.kind) {
                Some(ObjectKind::Set(s)) => (**s).clone(),
                _ => Vec::new(),
            };
            let mut arr = Vec::with_capacity(items.len());
            for item in items {
                arr.push(encode_structured_json(vm, item, seen)?);
            }
            Ok(serde_json::json!({ "$t": "set", "v": arr }))
        }
        _ => {
            let keys = vm.own_enumerable_keys_including_elements(h);
            let mut map = serde_json::Map::with_capacity(keys.len());
            for name in keys {
                let k = vm.heap.intern_key(&name);
                let v = vm.get_property_by_key(h, k)?;
                map.insert(name, encode_structured_json(vm, v, seen)?);
            }
            Ok(serde_json::Value::Object(map))
        }
    };
    seen.remove(&h);
    res
}

/// Decodes a cross-process structured JSON payload back into live JS values on `vm`.
pub(crate) fn decode_structured_json(vm: &mut JsVm, jv: &serde_json::Value) -> JsValue {
    match jv {
        serde_json::Value::Null => JsValue::NULL,
        serde_json::Value::Bool(b) => JsValue::from_bool(*b),
        serde_json::Value::Number(n) => JsValue::from_f64(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => JsValue::from_string(vm.heap.alloc_string(s)),
        serde_json::Value::Array(items) => {
            let elems: Vec<JsValue> = items
                .iter()
                .map(|it| decode_structured_json(vm, it))
                .collect();
            JsValue::from_object(alloc_array_from(vm, elems))
        }
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(tag)) = map.get("$t") {
                match tag.as_str() {
                    "undef" => return JsValue::UNDEFINED,
                    "num" => {
                        return match map.get("v").and_then(serde_json::Value::as_str) {
                            Some("NaN") => JsValue::from_f64(f64::NAN),
                            Some("Inf") => JsValue::from_f64(f64::INFINITY),
                            Some("-Inf") => JsValue::from_f64(f64::NEG_INFINITY),
                            Some("-0") => JsValue::from_f64(-0.0),
                            _ => JsValue::from_f64(0.0),
                        };
                    }
                    "bigint" => {
                        let txt = map
                            .get("v")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("0");
                        let bi = JsBigInt::parse_str(txt).unwrap_or_else(JsBigInt::zero);
                        return JsValue::from_bigint(vm.heap.alloc_bigint(bi));
                    }
                    "ab" => {
                        let bytes: Vec<u8> = map
                            .get("v")
                            .and_then(serde_json::Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_u64().map(|n| n as u8))
                                    .collect()
                            })
                            .unwrap_or_default();
                        return JsValue::from_object(alloc_array_buffer(vm, bytes));
                    }
                    "ta" => {
                        let bytes: Vec<u8> = map
                            .get("v")
                            .and_then(serde_json::Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_u64().map(|n| n as u8))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let kind = match map.get("k").and_then(serde_json::Value::as_str) {
                            Some("Int8") => TypedArrayKind::Int8,
                            Some("Uint8Clamped") => TypedArrayKind::Uint8Clamped,
                            Some("Int16") => TypedArrayKind::Int16,
                            Some("Uint16") => TypedArrayKind::Uint16,
                            Some("Int32") => TypedArrayKind::Int32,
                            Some("Uint32") => TypedArrayKind::Uint32,
                            Some("Float32") => TypedArrayKind::Float32,
                            Some("Float64") => TypedArrayKind::Float64,
                            _ => TypedArrayKind::Uint8,
                        };
                        let len = (bytes.len() / kind.bytes_per_element()) as u32;
                        let buf = alloc_array_buffer(vm, bytes);
                        let proto = vm.typed_array_protos[kind as usize];
                        let ta = vm.heap.alloc_object(JsObject::with_kind(
                            ObjectKind::TypedArray(TypedArrayObject {
                                kind,
                                buffer: buf,
                                byte_offset: 0,
                                length: len,
                            }),
                            Some(proto),
                        ));
                        return JsValue::from_object(ta);
                    }
                    "date" => {
                        let ms = map
                            .get("v")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(f64::NAN);
                        let h = vm.heap.alloc_object(JsObject::with_kind(
                            ObjectKind::Date(ms),
                            Some(vm.date_proto),
                        ));
                        return JsValue::from_object(h);
                    }
                    _ => {}
                }
            }
            let obj = vm.alloc_ordinary_object();
            for (k, v) in map {
                let key = vm.heap.intern_key(k);
                let val = decode_structured_json(vm, v);
                vm.heap
                    .object_define_data(obj, key, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
            }
            JsValue::from_object(obj)
        }
    }
}

// ---------------------------------------------------------------------------
// Transfer list validation & structured cloning
// ---------------------------------------------------------------------------

enum TransferEntry {
    Port(HeapHandle),
    ArrayBuffer(HeapHandle),
}

fn parse_transfer_list(
    vm: &mut JsVm,
    frames: &SharedFrames,
    transfer_val: JsValue,
    disallowed_ports: &[HeapHandle],
) -> Result<Vec<TransferEntry>, JsException> {
    if transfer_val.is_undefined() || transfer_val.is_null() {
        return Ok(Vec::new());
    }
    let Some(arr_h) = transfer_val.as_object() else {
        return Err(vm.type_error("transfer list must be an iterable array"));
    };
    let len = if is_js_array(vm, arr_h) {
        array_len(vm, arr_h)
    } else {
        let lk = vm.heap.intern_key("length");
        vm.get_property_by_key(arr_h, lk)
            .ok()
            .and_then(|v| v.as_number())
            .map_or(0, |n| n.max(0.0) as usize)
    };
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let item = if is_js_array(vm, arr_h) {
            array_elem(vm, arr_h, i)
        } else {
            let ik = vm.heap.intern_key(&i.to_string());
            vm.get_property_by_key(arr_h, ik)
                .unwrap_or(JsValue::UNDEFINED)
        };
        let Some(h) = item.as_object() else {
            return Err(create_dom_exception(
                vm,
                "DataCloneError",
                "Value in transfer list is not transferable",
            ));
        };
        if !seen.insert(h) {
            return Err(create_dom_exception(
                vm,
                "DataCloneError",
                "Duplicate object in transfer list",
            ));
        }
        if disallowed_ports.contains(&h) {
            return Err(create_dom_exception(
                vm,
                "DataCloneError",
                "Cannot transfer source or target MessagePort",
            ));
        }
        if let Some(port) = frames.borrow().ports.get(&h) {
            if port.neutered {
                return Err(create_dom_exception(
                    vm,
                    "DataCloneError",
                    "MessagePort has already been neutered",
                ));
            }
            out.push(TransferEntry::Port(h));
            continue;
        }
        if vm
            .heap
            .get_object(h)
            .is_some_and(|o| matches!(o.kind, ObjectKind::ArrayBuffer(_)))
        {
            if frames.borrow().detached_buffers.contains(&h) {
                return Err(create_dom_exception(
                    vm,
                    "DataCloneError",
                    "ArrayBuffer is already detached",
                ));
            }
            out.push(TransferEntry::ArrayBuffer(h));
            continue;
        }
        return Err(create_dom_exception(
            vm,
            "DataCloneError",
            "Object in transfer list is not transferable",
        ));
    }
    Ok(out)
}

/// Transfers `entries` for in-process delivery: creates fresh transferred `MessagePort`
/// handles wired to the original partners, neuters the source ports, and detaches any
/// transferred `ArrayBuffer`s (returning a memo map so structured clone preserves identity!).
fn execute_in_process_transfer(
    vm: &mut JsVm,
    frames: &SharedFrames,
    entries: Vec<TransferEntry>,
) -> (Vec<HeapHandle>, HashMap<HeapHandle, HeapHandle>) {
    let mut transferred_ports = Vec::new();
    let mut memo = HashMap::new();
    for entry in entries {
        match entry {
            TransferEntry::Port(old_h) => {
                let (port_id, partner_id, remote_pipeline, closed, pending) = {
                    let mut fs = frames.borrow_mut();
                    let Some(p) = fs.ports.get_mut(&old_h) else {
                        continue;
                    };
                    p.neutered = true;
                    let pending = std::mem::take(&mut p.pending);
                    (p.id, p.partner_id, p.remote_pipeline, p.closed, pending)
                };
                let proto = frames
                    .borrow()
                    .message_port_proto
                    .unwrap_or(vm.object_proto);
                let new_h = vm
                    .heap
                    .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
                {
                    let mut fs = frames.borrow_mut();
                    fs.port_by_id.insert(port_id, new_h);
                    fs.ports.insert(
                        new_h,
                        MessagePortState {
                            id: port_id,
                            partner_id,
                            remote_pipeline,
                            started: false,
                            closed,
                            neutered: false,
                            onmessage: JsValue::NULL,
                            onmessageerror: JsValue::NULL,
                            pending,
                        },
                    );
                }
                memo.insert(old_h, new_h);
                transferred_ports.push(new_h);
            }
            TransferEntry::ArrayBuffer(old_h) => {
                let bytes = match vm.heap.get_object_mut(old_h).map(|o| &mut o.kind) {
                    Some(ObjectKind::ArrayBuffer(b)) => std::mem::take(b.as_mut()),
                    _ => Vec::new(),
                };
                frames.borrow_mut().detached_buffers.insert(old_h);
                let new_h = alloc_array_buffer(vm, bytes);
                memo.insert(old_h, new_h);
            }
        }
    }
    (transferred_ports, memo)
}

/// Transfers `entries` across an OS process boundary: neuters local ports, updates their
/// local partners to route future sends to `remote_pipeline`, and returns their `MessagePortId`s.
fn execute_cross_process_transfer(
    vm: &mut JsVm,
    frames: &SharedFrames,
    remote_pipeline: PipelineId,
    entries: Vec<TransferEntry>,
) -> Vec<MessagePortId> {
    let mut port_ids = Vec::new();
    for entry in entries {
        match entry {
            TransferEntry::Port(old_h) => {
                let mut fs = frames.borrow_mut();
                let (port_id, partner_id) = match fs.ports.get_mut(&old_h) {
                    Some(p) => {
                        p.neutered = true;
                        (p.id, p.partner_id)
                    }
                    None => continue,
                };
                fs.port_by_id.remove(&port_id);
                if let Some(pid) = partner_id {
                    if let Some(&partner_h) = fs.port_by_id.get(&pid) {
                        if let Some(partner) = fs.ports.get_mut(&partner_h) {
                            partner.remote_pipeline = Some(remote_pipeline);
                        }
                    }
                }
                port_ids.push(port_id);
            }
            TransferEntry::ArrayBuffer(old_h) => {
                if let Some(ObjectKind::ArrayBuffer(b)) =
                    vm.heap.get_object_mut(old_h).map(|o| &mut o.kind)
                {
                    b.clear();
                }
                frames.borrow_mut().detached_buffers.insert(old_h);
            }
        }
    }
    port_ids
}

fn validate_target_origin(
    vm: &mut JsVm,
    caller_origin: &str,
    raw: &str,
) -> Result<String, JsException> {
    if raw == "*" {
        return Ok("*".to_string());
    }
    if raw == "/" {
        return Ok(caller_origin.to_string());
    }
    match Url::parse(raw) {
        Ok(u) => Ok(origin_of_url(&u)),
        Err(_) => Err(create_dom_exception(
            vm,
            "SyntaxError",
            &format!("Failed to execute 'postMessage': Invalid target origin '{raw}'"),
        )),
    }
}

fn parse_post_message_args(
    vm: &mut JsVm,
    args: &[JsValue],
) -> Result<(JsValue, String, JsValue), JsException> {
    if args.is_empty() {
        return Err(vm.type_error("postMessage requires at least 1 argument"));
    }
    let msg = arg(args, 0);
    let second = arg(args, 1);
    if second.is_undefined() {
        return Ok((msg, "/".to_string(), JsValue::UNDEFINED));
    }
    if let Some(opt_h) = second.as_object() {
        if !is_js_array(vm, opt_h) {
            let k_to = vm.heap.intern_key("targetOrigin");
            let k_tr = vm.heap.intern_key("transfer");
            let to_val = vm.get_property_by_key(opt_h, k_to)?;
            let target_origin = if to_val.is_undefined() {
                "/".to_string()
            } else {
                vm.value_to_rust_string(to_val)
            };
            let transfer = vm.get_property_by_key(opt_h, k_tr)?;
            return Ok((msg, target_origin, transfer));
        }
    }
    let target_origin = vm.value_to_rust_string(second);
    let transfer = arg(args, 2);
    Ok((msg, target_origin, transfer))
}

fn security_error(vm: &mut JsVm, caller_origin: &str, target_origin: &str) -> JsException {
    create_dom_exception(
        vm,
        "SecurityError",
        &format!(
            "Blocked a frame with origin \"{caller_origin}\" from accessing a cross-origin frame with origin \"{target_origin}\"."
        ),
    )
}

// ---------------------------------------------------------------------------
// MessagePort delivery helpers
// ---------------------------------------------------------------------------

fn schedule_port_flush(
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    frames: &SharedFrames,
    port_h: HeapHandle,
) {
    let h = Rc::clone(host);
    let ev = Rc::clone(events);
    let el = Rc::clone(event_loop);
    let fr = Rc::clone(frames);
    event_loop
        .borrow_mut()
        .post_host_task(TaskPriority::Networking, move |vm| loop {
            let next_msg = {
                let mut fs = fr.borrow_mut();
                let Some(port) = fs.ports.get_mut(&port_h) else {
                    break;
                };
                if !port.started || port.closed || port.neutered {
                    break;
                }
                port.pending.pop_front()
            };
            let Some(queued) = next_msg else {
                break;
            };
            let ports_arr =
                alloc_array_from(vm, queued.ports.iter().map(|&ph| JsValue::from_object(ph)));
            let empty_s = JsValue::from_string(vm.heap.alloc_string(""));
            let proto = fr
                .borrow()
                .message_event_proto
                .or_else(|| ev.borrow().event_proto)
                .unwrap_or(vm.object_proto);
            fire_object_event_with_proto(
                vm,
                &h,
                &ev,
                &el,
                port_h,
                "message",
                proto,
                &[
                    ("data", queued.data),
                    ("origin", empty_s),
                    ("lastEventId", empty_s),
                    ("source", JsValue::NULL),
                    ("ports", JsValue::from_object(ports_arr)),
                ],
            );
        });
}

fn alloc_message_port(
    vm: &mut JsVm,
    frames: &SharedFrames,
    id: MessagePortId,
    partner_id: Option<MessagePortId>,
    remote_pipeline: Option<PipelineId>,
) -> HeapHandle {
    let proto = frames
        .borrow()
        .message_port_proto
        .unwrap_or(vm.object_proto);
    let h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let mut fs = frames.borrow_mut();
    fs.port_by_id.insert(id, h);
    fs.ports.insert(
        h,
        MessagePortState {
            id,
            partner_id,
            remote_pipeline,
            started: false,
            closed: false,
            neutered: false,
            onmessage: JsValue::NULL,
            onmessageerror: JsValue::NULL,
            pending: VecDeque::new(),
        },
    );
    h
}

// ---------------------------------------------------------------------------
// Same-origin child Document / Window & Cross-origin WindowProxy builders
// ---------------------------------------------------------------------------

fn is_xml_mime_or_url(content_type: &str, url: &str) -> bool {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if matches!(
        ct.as_str(),
        "text/xml" | "application/xml" | "application/xhtml+xml" | "image/svg+xml"
    ) || ct.ends_with("+xml")
    {
        return true;
    }
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.ends_with(".xml") || path.ends_with(".xhtml") || path.ends_with(".svg")
}

fn normalized_xml_mime(content_type: &str, url: &str) -> String {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if matches!(
        ct.as_str(),
        "text/xml" | "application/xml" | "application/xhtml+xml" | "image/svg+xml"
    ) || ct.ends_with("+xml")
    {
        return ct;
    }
    let path = url.split(['?', '#']).next().unwrap_or(url);
    if path.ends_with(".xhtml") {
        "application/xhtml+xml".to_string()
    } else if path.ends_with(".svg") {
        "image/svg+xml".to_string()
    } else {
        "application/xml".to_string()
    }
}

enum SubdocScript {
    Inline(String),
    External(String),
}

/// Creates a REAL `NodeData::Document` subtree inside `host.document` for a same-origin
/// `<iframe>` or `window.open()` browsing context, with full `Document.prototype` / `XMLDocument.prototype`,
/// `DocumentMeta`, `ownerDocument` adoption, and ordered inline/external script extraction.
fn build_sub_document(
    vm: &mut JsVm,
    host: &SharedHost,
    url: &str,
    content: &str,
    charset: &str,
    content_type: &str,
    window_proxy: HeapHandle,
) -> (HeapHandle, String, Vec<SubdocScript>) {
    let is_xml = is_xml_mime_or_url(content_type, url);
    let (sub_doc_id, title, scripts) = if is_xml {
        let mime = normalized_xml_mime(content_type, url);
        let sub_id = {
            let doc_rc = Rc::clone(&host.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            let sid = doc.new_node(NodeData::Document);
            if let Err(err_msg) =
                crate::dom_interfaces::parse_xml_into_document(&mut doc, sid, content)
            {
                // Remove partial children and insert standard parsererror element.
                let partial: Vec<NodeId> = doc.children(sid).collect();
                for p in partial {
                    doc.remove_child(p);
                }
                let err_el = doc.new_node(NodeData::Element(rix_dom::ElementData::new(
                    rix_dom::QualName::new(
                        None,
                        rix_dom::Namespace::from(
                            "http://www.mozilla.org/newlayout/xml/parsererror.xml",
                        ),
                        rix_dom::LocalName::from("parsererror"),
                    ),
                )));
                let err_txt = doc.create_text(&err_msg);
                doc.append_child(err_el, err_txt);
                doc.append_child(sid, err_el);
            }
            sid
        };
        let mut meta = crate::dom_interfaces::DocumentMeta::xml_document(mime, url);
        meta.character_set = charset.to_string();
        meta.default_view = Some(window_proxy);
        {
            let mut hb = host.borrow_mut();
            hb.doc_meta.insert(sub_id, meta);
            hb.adopt_subtree(sub_id, sub_id);
        }
        let (title, scripts) = collect_subdoc_title_and_scripts(host, sub_id);
        (sub_id, title, scripts)
    } else {
        let mut parsed_doc = parse_html(content);
        parsed_doc.set_character_set(charset);
        let sub_id = {
            let doc_rc = Rc::clone(&host.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            let sid = doc.new_node(NodeData::Document);
            let pr = parsed_doc.root();
            for child in parsed_doc.children(pr) {
                let cloned = doc.clone_subtree_from(&parsed_doc, child);
                doc.append_child(sid, cloned);
            }
            sid
        };
        let mut meta = crate::dom_interfaces::DocumentMeta::html(url);
        meta.character_set = charset.to_string();
        meta.quirks_mode = parsed_doc.quirks_mode();
        meta.default_view = Some(window_proxy);
        {
            let mut hb = host.borrow_mut();
            hb.doc_meta.insert(sub_id, meta);
            hb.adopt_subtree(sub_id, sub_id);
        }
        let (title, scripts) = collect_subdoc_title_and_scripts(host, sub_id);
        (sub_id, title, scripts)
    };
    let wrapped = wrap_node(vm, host, sub_doc_id)
        .as_object()
        .unwrap_or_else(|| vm.alloc_ordinary_object());
    (wrapped, title, scripts)
}

fn collect_subdoc_title_and_scripts(
    host: &SharedHost,
    sub_doc_id: NodeId,
) -> (String, Vec<SubdocScript>) {
    let doc_rc = Rc::clone(&host.borrow().document);
    let doc = doc_rc.borrow();
    let mut title = String::new();
    let mut scripts = Vec::new();
    for nid in doc.descendants(sub_doc_id) {
        if let Some(NodeData::Element(el)) = doc.get(nid) {
            let tag = el.name.local.as_str();
            if tag.eq_ignore_ascii_case("title") && title.is_empty() {
                title = doc.inner_text(nid);
            } else if tag.eq_ignore_ascii_case("script") {
                if let Some(src) = el.get_attribute("src") {
                    let trimmed = src.trim();
                    if !trimmed.is_empty() {
                        scripts.push(SubdocScript::External(trimmed.to_string()));
                    }
                } else {
                    let code = doc.inner_text(nid);
                    if !code.trim().is_empty() {
                        scripts.push(SubdocScript::Inline(code));
                    }
                }
            }
        }
    }
    (title, scripts)
}

/// Copies immutable JS/DOM intrinsic bindings onto a same-origin child `WindowProxy`
/// so child scripts execute in an isolated global environment without polluting parent `window`.
fn install_same_origin_child_globals(
    vm: &mut JsVm,
    proxy_h: HeapHandle,
    child_doc: HeapHandle,
    parent_global: HeapHandle,
    top_val: JsValue,
    iframe_el: JsValue,
) {
    let intrinsics = [
        "Object",
        "Function",
        "Array",
        "String",
        "Number",
        "Boolean",
        "Symbol",
        "BigInt",
        "Math",
        "Date",
        "RegExp",
        "Error",
        "TypeError",
        "RangeError",
        "SyntaxError",
        "ReferenceError",
        "URIError",
        "EvalError",
        "Map",
        "Set",
        "WeakMap",
        "WeakSet",
        "Promise",
        "Proxy",
        "Reflect",
        "JSON",
        "ArrayBuffer",
        "Uint8Array",
        "Int32Array",
        "Float64Array",
        "DataView",
        "eval",
        "isNaN",
        "isFinite",
        "parseInt",
        "parseFloat",
        "encodeURI",
        "decodeURI",
        "encodeURIComponent",
        "decodeURIComponent",
        "undefined",
        "NaN",
        "Infinity",
        "console",
        "DOMException",
        "EventTarget",
        "Event",
        "CustomEvent",
        "UIEvent",
        "MouseEvent",
        "KeyboardEvent",
        "MessageEvent",
        "Node",
        "Element",
        "HTMLElement",
        "SVGElement",
        "MathMLElement",
        "Document",
        "HTMLDocument",
        "XMLDocument",
        "DocumentFragment",
        "DocumentType",
        "CharacterData",
        "Text",
        "CDATASection",
        "Comment",
        "ProcessingInstruction",
        "Attr",
        "Range",
        "StaticRange",
        "AbstractRange",
        "NodeFilter",
        "DOMParser",
        "XMLSerializer",
        "DOMPointReadOnly",
        "DOMPoint",
        "DOMRectReadOnly",
        "DOMRect",
        "DOMRectList",
        "DOMQuad",
        "DOMMatrixReadOnly",
        "DOMMatrix",
        "MediaQueryList",
        "MediaQueryListEvent",
        "MutationObserver",
        "ResizeObserver",
        "ResizeObserverEntry",
        "ResizeObserverSize",
        "IntersectionObserver",
        "IntersectionObserverEntry",
        "performance",
        "CustomElementRegistry",
        "ElementInternals",
        "CustomStateSet",
        "ShadowRoot",
        "getComputedStyle",
        "URL",
        "URLSearchParams",
        "AbortController",
        "AbortSignal",
        "setTimeout",
        "clearTimeout",
        "requestAnimationFrame",
        "cancelAnimationFrame",
        "queueMicrotask",
    ];
    for name in intrinsics {
        let k = vm.heap.intern_key(name);
        if vm.heap.object_get_own(proxy_h, k, &vm.shapes).is_none() {
            if let Ok(v) = vm.get_property_by_key(parent_global, k) {
                vm.heap
                    .object_define_data(proxy_h, k, v, PropertyAttrs::METHOD, &mut vm.shapes);
            }
        }
    }
    for (name, val) in [
        ("window", JsValue::from_object(proxy_h)),
        ("self", JsValue::from_object(proxy_h)),
        ("globalThis", JsValue::from_object(proxy_h)),
        ("document", JsValue::from_object(child_doc)),
        ("parent", JsValue::from_object(parent_global)),
        ("top", top_val),
        ("frameElement", iframe_el),
    ] {
        let k = vm.heap.intern_key(name);
        vm.heap
            .object_define_data(proxy_h, k, val, PropertyAttrs::CONFIGURABLE, &mut vm.shapes);
    }
}

/// Wraps any own JS functions newly declared on a same-origin child `WindowProxy`
/// so cross-frame calls (e.g. `iframe.contentWindow.run()`) bind `vm.global` to the child frame.
fn bind_child_frame_functions(
    vm: &mut JsVm,
    proxy_h: HeapHandle,
    pre_keys: &HashSet<rix_js::PropertyKey>,
) {
    let own_keys: Vec<(rix_js::PropertyKey, JsValue)> = vm
        .heap
        .get_object(proxy_h)
        .map(|o| {
            let mut out = Vec::new();
            if let Some(rare) = &o.rare {
                for (&k, prop) in &rare.dict_props {
                    if !pre_keys.contains(&k) && !prop.attrs.contains(PropertyAttrs::ACCESSOR) {
                        out.push((k, prop.value));
                    }
                }
            } else if let Some(shape) = vm.shapes.get(rix_js::ShapeId(o.shape_id)) {
                for &k in shape.keys() {
                    if !pre_keys.contains(&k) {
                        if let Some(slot) = shape.lookup(k) {
                            if let Some(&v) = o.slots.get(slot.offset as usize) {
                                out.push((k, v));
                            }
                        }
                    }
                }
            }
            out
        })
        .unwrap_or_default();
    for (k, val) in own_keys {
        if let Some(fh) = val.as_object().filter(|&h| vm.is_callable_handle(h)) {
            let wrapped = vm.alloc_host_fn_rooted(
                "frame_fn",
                &[JsValue::from_object(proxy_h), JsValue::from_object(fh)],
                move |vm, this_val, args| {
                    let prev_global = vm.global;
                    vm.global = proxy_h;
                    let res = vm.call_value(JsValue::from_object(fh), this_val, args);
                    vm.global = prev_global;
                    res
                },
            );
            vm.heap.object_define_data(
                proxy_h,
                k,
                JsValue::from_object(wrapped),
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
        }
    }
}

fn read_proxy_url(
    vm: &mut JsVm,
    fr: &SharedFrames,
    wa: &SharedWebApis,
    proxy_h: HeapHandle,
) -> Result<String, JsException> {
    let caller = fr.borrow().effective_origin(wa);
    let fs = fr.borrow();
    match fs.proxies.get(&proxy_h) {
        Some(WindowProxyKind::SameOriginChild { url, .. }) => Ok(url.clone()),
        Some(WindowProxyKind::Auxiliary {
            is_cross_origin: false,
            url,
            ..
        }) => Ok(url.clone()),
        Some(WindowProxyKind::CrossOriginRemote { origin, .. })
        | Some(WindowProxyKind::Auxiliary {
            is_cross_origin: true,
            origin,
            ..
        }) => Err(security_error(vm, &caller, origin)),
        None => Ok("about:blank".to_string()),
    }
}

fn set_proxy_url(
    vm: &mut JsVm,
    fr: &SharedFrames,
    wa: &SharedWebApis,
    proxy_h: HeapHandle,
    new_url: String,
) -> Result<(), JsException> {
    let cur_url = {
        let fs = fr.borrow();
        match fs.proxies.get(&proxy_h) {
            Some(
                WindowProxyKind::SameOriginChild { url, .. }
                | WindowProxyKind::CrossOriginRemote { url, .. }
                | WindowProxyKind::Auxiliary { url, .. },
            ) => url.clone(),
            None => wa.borrow().current_href(),
        }
    };
    let base = Url::parse(&cur_url)
        .ok()
        .filter(|u| !u.cannot_be_a_base())
        .unwrap_or_else(|| wa.borrow().location_url.clone());
    let resolved = base
        .join(&new_url)
        .map_err(|_| create_dom_exception(vm, "SyntaxError", &format!("Invalid URL: {new_url}")))?;
    let mut fs = fr.borrow_mut();
    if let Some(proxy) = fs.proxies.get_mut(&proxy_h) {
        match proxy {
            WindowProxyKind::SameOriginChild { url, .. }
            | WindowProxyKind::CrossOriginRemote { url, .. }
            | WindowProxyKind::Auxiliary { url, .. } => {
                *url = resolved.to_string();
            }
        }
    }
    Ok(())
}

fn attach_proxy_location(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    frames: &SharedFrames,
    proxy_h: HeapHandle,
) {
    let loc_obj = vm.alloc_ordinary_object();
    let fr = Rc::clone(frames);
    let wa = Rc::clone(web_apis);
    define_host_accessor(
        vm,
        loc_obj,
        "href",
        move |vm, _, _| {
            let url = read_proxy_url(vm, &fr, &wa, proxy_h)?;
            Ok(JsValue::from_string(vm.heap.alloc_string(&url)))
        },
        {
            let fr = Rc::clone(frames);
            let wa = Rc::clone(web_apis);
            move |vm, _, args| {
                let new_url = vm.value_to_rust_string(arg(args, 0));
                set_proxy_url(vm, &fr, &wa, proxy_h, new_url)?;
                Ok(JsValue::UNDEFINED)
            }
        },
    );
    for (prop, part) in [
        ("origin", 0u8),
        ("protocol", 1),
        ("host", 2),
        ("hostname", 3),
        ("port", 4),
        ("pathname", 5),
        ("search", 6),
        ("hash", 7),
    ] {
        let fr_p = Rc::clone(frames);
        let wa_p = Rc::clone(web_apis);
        define_host_getter(vm, loc_obj, prop, move |vm, _, _| {
            let url_str = read_proxy_url(vm, &fr_p, &wa_p, proxy_h)?;
            let out = Url::parse(&url_str)
                .map(|u| match part {
                    0 => origin_of_url(&u),
                    1 => format!("{}:", u.scheme()),
                    2 => match (u.host_str(), u.port()) {
                        (Some(h), Some(p)) => format!("{h}:{p}"),
                        (Some(h), None) => h.to_string(),
                        _ => String::new(),
                    },
                    3 => u.host_str().unwrap_or("").to_string(),
                    4 => u.port().map(|p| p.to_string()).unwrap_or_default(),
                    5 => u.path().to_string(),
                    6 => u
                        .query()
                        .filter(|q| !q.is_empty())
                        .map(|q| format!("?{q}"))
                        .unwrap_or_default(),
                    _ => u
                        .fragment()
                        .filter(|f| !f.is_empty())
                        .map(|f| format!("#{f}"))
                        .unwrap_or_default(),
                })
                .unwrap_or_default();
            Ok(JsValue::from_string(vm.heap.alloc_string(&out)))
        });
    }
    {
        let fr_ts = Rc::clone(frames);
        let wa_ts = Rc::clone(web_apis);
        define_host_method(vm, loc_obj, "toString", move |vm, _, _| {
            let url_str = read_proxy_url(vm, &fr_ts, &wa_ts, proxy_h)?;
            Ok(JsValue::from_string(vm.heap.alloc_string(&url_str)))
        });
    }
    // HTML §7.2.4 `[PutForwards=href]`: setting `window.location = url` forwards to `location.href`.
    // Root `loc_obj` via `alloc_host_fn_rooted` so GC never collects it while `proxy_h` is live.
    let fr_loc = Rc::clone(frames);
    let wa_loc = Rc::clone(web_apis);
    let k_loc = vm.heap.intern_key("location");
    let loc_getter = vm.alloc_host_fn_rooted(
        "get location",
        &[JsValue::from_object(loc_obj)],
        move |_, _, _| Ok(JsValue::from_object(loc_obj)),
    );
    let loc_setter = vm.alloc_host_fn("set location", move |vm, _, args| {
        let new_url = vm.value_to_rust_string(arg(args, 0));
        set_proxy_url(vm, &fr_loc, &wa_loc, proxy_h, new_url)?;
        Ok(JsValue::UNDEFINED)
    });
    vm.heap.object_define_accessor(
        proxy_h,
        k_loc,
        Some(loc_getter),
        Some(loc_setter),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    // Expose core DOM/JS constructors on same-origin WindowProxy so cross-frame tests work.
    for ctor_name in [
        "DOMException",
        "Node",
        "Element",
        "Document",
        "XMLDocument",
        "Range",
        "Event",
        "EventTarget",
        "Error",
        "TypeError",
        "Object",
        "Array",
    ] {
        let k = vm.heap.intern_key(ctor_name);
        if let Ok(v) = vm.get_property_by_key(vm.global, k) {
            if !v.is_undefined() {
                vm.heap
                    .object_define_data(proxy_h, k, v, PropertyAttrs::METHOD, &mut vm.shapes);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn alloc_window_proxy(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    frames: &SharedFrames,
    kind: WindowProxyKind,
) -> HeapHandle {
    let proto = frames
        .borrow()
        .window_proxy_proto
        .unwrap_or(vm.object_proto);
    let proxy_h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    frames.borrow_mut().proxies.insert(proxy_h, kind);
    attach_proxy_location(vm, web_apis, frames, proxy_h);
    proxy_h
}

/// Dispatches `postMessage` on `target_h` (which may be `vm.global` or any `WindowProxy`).
#[allow(clippy::too_many_arguments)]
fn perform_window_post_message(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    frames: &SharedFrames,
    target_h: HeapHandle,
    source_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let (msg_val, raw_target_origin, transfer_val) = parse_post_message_args(vm, args)?;
    let caller_origin = frames.borrow().effective_origin(web_apis);
    let expected_origin = validate_target_origin(vm, &caller_origin, &raw_target_origin)?;
    let transfer_entries = parse_transfer_list(vm, frames, transfer_val, &[])?;

    // Case 1: Cross-origin remote OOPIF bound to another OS content process.
    let remote_info = frames
        .borrow()
        .proxies
        .get(&target_h)
        .and_then(|k| match k {
            WindowProxyKind::CrossOriginRemote {
                remote_pipeline: Some(pid),
                origin,
                closed,
                crashed,
                ..
            } => Some((*pid, origin.clone(), *closed || *crashed)),
            _ => None,
        });
    if let Some((remote_pipeline, remote_origin, is_dead)) = remote_info {
        if is_dead {
            return Ok(JsValue::UNDEFINED);
        }
        let mut seen = HashSet::new();
        let json_val = encode_structured_json(vm, msg_val, &mut seen)?;
        let data_json = serde_json::to_string(&json_val).unwrap_or_else(|_| "null".to_string());
        let transfer_ports =
            execute_cross_process_transfer(vm, frames, remote_pipeline, transfer_entries);
        // Check origin match when known (also enforced by the receiving process).
        if expected_origin == "*" || expected_origin == remote_origin || remote_origin.is_empty() {
            let source_pipeline = frames.borrow().pipeline_id;
            frames
                .borrow_mut()
                .outbound_ipc
                .push_back(ContentToBrowser::PostMessageToFrame {
                    source_pipeline,
                    target_pipeline: remote_pipeline,
                    data_json,
                    source_origin: caller_origin,
                    target_origin: expected_origin,
                    transfer_ports,
                });
        }
        return Ok(JsValue::UNDEFINED);
    }

    // Case 2: In-process target (`window` itself, same-origin child iframe, or auxiliary window).
    let (target_origin, is_closed) = if target_h == vm.global {
        let closed = frames.borrow().window_closed;
        (caller_origin.clone(), closed)
    } else {
        match frames.borrow().proxies.get(&target_h) {
            Some(WindowProxyKind::SameOriginChild { origin, closed, .. })
            | Some(WindowProxyKind::CrossOriginRemote { origin, closed, .. })
            | Some(WindowProxyKind::Auxiliary { origin, closed, .. }) => (origin.clone(), *closed),
            None => return Ok(JsValue::UNDEFINED),
        }
    };

    // Always validate/clone & execute transfers BEFORE async queueing (so DataCloneError throws sync!).
    let (transferred_ports, mut memo) = execute_in_process_transfer(vm, frames, transfer_entries);
    let cloned_data = structured_clone_inner(vm, web_apis, msg_val, &mut memo)?;

    if is_closed {
        return Ok(JsValue::UNDEFINED);
    }
    if expected_origin != "*" && expected_origin != target_origin {
        return Ok(JsValue::UNDEFINED);
    }

    // Deliver strictly asynchronously on the WHATWG task queue!
    let h = Rc::clone(host);
    let ev = Rc::clone(events);
    let el = Rc::clone(event_loop);
    let fr = Rc::clone(frames);
    let origin_str = caller_origin;
    event_loop
        .borrow_mut()
        .post_host_task(TaskPriority::Networking, move |vm| {
            let ports_arr = alloc_array_from(
                vm,
                transferred_ports.iter().map(|&ph| JsValue::from_object(ph)),
            );
            let origin_js = JsValue::from_string(vm.heap.alloc_string(&origin_str));
            let empty_js = JsValue::from_string(vm.heap.alloc_string(""));
            let proto = fr
                .borrow()
                .message_event_proto
                .or_else(|| ev.borrow().event_proto)
                .unwrap_or(vm.object_proto);
            fire_object_event_with_proto(
                vm,
                &h,
                &ev,
                &el,
                target_h,
                "message",
                proto,
                &[
                    ("data", cloned_data),
                    ("origin", origin_js),
                    ("lastEventId", empty_js),
                    ("source", source_val),
                    ("ports", JsValue::from_object(ports_arr)),
                ],
            );
        });
    Ok(JsValue::UNDEFINED)
}

// ---------------------------------------------------------------------------
// Iframe discovery, navigation, same-origin execution & OOPIF IPC emission
// ---------------------------------------------------------------------------

/// Synchronizes all `<iframe>` elements currently in `host.document` (creating or updating
/// same-origin child documents and emitting `CreateChildFrame`/`NavigateChildFrame` IPC
/// events for cross-origin OOPIFs).
#[allow(clippy::too_many_arguments)]
pub fn sync_all_iframes_in_document(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    frames: &SharedFrames,
) {
    let iframe_nodes: Vec<NodeId> = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        doc.descendants(doc.root())
            .filter(|&nid| {
                matches!(
                    doc.get(nid),
                    Some(NodeData::Element(el))
                        if el.name.local.as_str().eq_ignore_ascii_case("iframe")
                )
            })
            .collect()
    };
    for nid in iframe_nodes {
        sync_iframe_node(vm, host, events, event_loop, web_apis, net, frames, nid);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn sync_iframe_node(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    frames: &SharedFrames,
    node_id: NodeId,
) {
    let (src_attr, srcdoc_attr, sandbox_attr, name_attr, width, height) = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        let Some(NodeData::Element(el)) = doc.get(node_id) else {
            return;
        };
        let src = el.get_attribute("src").unwrap_or("").trim().to_string();
        let srcdoc = el.get_attribute("srcdoc").map(ToString::to_string);
        let sandbox = el.get_attribute("sandbox").map(ToString::to_string);
        let name = el.get_attribute("name").unwrap_or("").to_string();
        let w = el
            .get_attribute("width")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(300);
        let h = el
            .get_attribute("height")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(150);
        (src, srcdoc, sandbox, name, w, h)
    };

    if !host.borrow().document.borrow().is_connected(node_id) {
        return;
    }

    // Skip redundant re-sync if attributes have not changed since last sync.
    if let Some(existing) = frames.borrow().iframes.get(&node_id) {
        if existing.last_src == src_attr
            && existing.last_srcdoc == srcdoc_attr
            && existing.last_sandbox == sandbox_attr
        {
            return;
        }
    }

    let parent_href = web_apis.borrow().current_href();
    let parent_origin = frames.borrow().effective_origin(web_apis);
    let sandbox = SandboxFlags::parse(sandbox_attr.as_deref());

    let resolved_url = if srcdoc_attr.is_some() {
        "about:srcdoc".to_string()
    } else if src_attr.is_empty() || src_attr.eq_ignore_ascii_case("about:blank") {
        "about:blank".to_string()
    } else if let Ok(base) = Url::parse(&parent_href) {
        base.join(&src_attr)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| src_attr.clone())
    } else {
        src_attr.clone()
    };

    let raw_child_origin = if resolved_url == "about:blank" || resolved_url == "about:srcdoc" {
        parent_origin.clone()
    } else {
        Url::parse(&resolved_url)
            .map(|u| origin_of_url(&u))
            .unwrap_or_else(|_| "null".to_string())
    };
    let effective_child_origin = sandbox.effective_origin(&raw_child_origin);
    let is_same_origin = sandbox.same_origin_with(&parent_origin, &raw_child_origin);
    let surface_id = surface_id_for_node(node_id);

    let existing_same_origin_proxy = frames
        .borrow()
        .iframes
        .get(&node_id)
        .filter(|r| !r.is_cross_origin)
        .map(|r| r.proxy);
    let was_existing = frames.borrow().iframes.contains_key(&node_id);
    if is_same_origin {
        let (html_content, child_charset, child_ct) = if let Some(ref sd) = srcdoc_attr {
            let cs = rix_html::detect_html_charset(sd.as_bytes());
            (sd.clone(), cs, "text/html".to_string())
        } else if resolved_url.starts_with("blob:") {
            if let Some(bytes) = web_apis.borrow().object_urls.get(&resolved_url) {
                let cs = rix_html::detect_html_charset(bytes);
                (
                    String::from_utf8_lossy(bytes).into_owned(),
                    cs,
                    "text/html".to_string(),
                )
            } else {
                (String::new(), "UTF-8", "text/html".to_string())
            }
        } else if resolved_url.starts_with("http://") || resolved_url.starts_with("https://") {
            fetch_iframe_url_sync(net, &resolved_url, &parent_origin)
                .unwrap_or_else(|| (String::new(), "UTF-8", "text/html".to_string()))
        } else {
            (String::new(), "UTF-8", "text/html".to_string())
        };

        let proxy_h = if let Some(existing_p) = existing_same_origin_proxy {
            frames.borrow_mut().proxies.insert(
                existing_p,
                WindowProxyKind::SameOriginChild {
                    iframe_node: node_id,
                    child_doc: vm.global,
                    origin: effective_child_origin.clone(),
                    url: resolved_url.clone(),
                    name: name_attr.clone(),
                    closed: false,
                },
            );
            existing_p
        } else {
            alloc_window_proxy(
                vm,
                web_apis,
                frames,
                WindowProxyKind::SameOriginChild {
                    iframe_node: node_id,
                    child_doc: vm.global, // replaced immediately below
                    origin: effective_child_origin.clone(),
                    url: resolved_url.clone(),
                    name: name_attr.clone(),
                    closed: false,
                },
            )
        };
        let (child_doc, _title, inline_scripts) = build_sub_document(
            vm,
            host,
            &resolved_url,
            &html_content,
            child_charset,
            &child_ct,
            proxy_h,
        );
        let child_doc_nid = host.borrow().node_for_handle(vm, child_doc);
        if let Some(cd_nid) = child_doc_nid {
            host.borrow()
                .document
                .borrow_mut()
                .set_content_document(node_id, Some(cd_nid));
        }
        if let Some(WindowProxyKind::SameOriginChild {
            child_doc: slot, ..
        }) = frames.borrow_mut().proxies.get_mut(&proxy_h)
        {
            *slot = child_doc;
        }
        {
            let mut fs = frames.borrow_mut();
            if !was_existing {
                fs.iframe_order.push(node_id);
            }
            fs.iframes.insert(
                node_id,
                IframeRecord {
                    node_id,
                    surface_id,
                    proxy: proxy_h,
                    is_cross_origin: false,
                    remote_pipeline: None,
                    last_src: src_attr,
                    last_srcdoc: srcdoc_attr,
                    last_sandbox: sandbox_attr,
                    crashed: false,
                },
            );
        }

        // Always populate child global intrinsics & child-bound `DOMParser`, even for scriptless frames.
        let saved_global = vm.global;
        let iframe_el = wrap_node(vm, host, node_id);
        let top_val = frames.borrow().top_window;
        install_same_origin_child_globals(vm, proxy_h, child_doc, saved_global, top_val, iframe_el);
        let child_dp =
            crate::dom_parsing::create_dom_parser_ctor_for_doc(vm, host, web_apis, child_doc_nid);
        let k_dp = vm.heap.intern_key("DOMParser");
        vm.heap.object_define_data(
            proxy_h,
            k_dp,
            JsValue::from_object(child_dp),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
        // Every same-origin child browsing context gets its own global `CustomElementRegistry`
        // and document-scoped CSSOM View viewport/scroll/matchMedia bindings.
        if let Some(cd_nid) = child_doc_nid {
            let child_ce_reg =
                crate::custom_elements::ensure_child_global_registry(vm, host, cd_nid);
            let k_ce = vm.heap.intern_key("customElements");
            vm.heap.object_define_data(
                proxy_h,
                k_ce,
                JsValue::from_object(child_ce_reg),
                PropertyAttrs::METHOD,
                &mut vm.shapes,
            );
            crate::cssom_view::install_child_window_cssom_view(
                vm, host, events, event_loop, proxy_h, cd_nid,
            );
        }

        // Execute external & inline scripts inside the same-origin child iframe's isolated global.
        if sandbox.scripts_allowed() && !inline_scripts.is_empty() {
            let pre_keys: HashSet<rix_js::PropertyKey> = vm
                .heap
                .get_object(proxy_h)
                .map(|o| {
                    if let Some(rare) = &o.rare {
                        rare.dict_props.keys().copied().collect()
                    } else if let Some(shape) = vm.shapes.get(rix_js::ShapeId(o.shape_id)) {
                        shape.keys().iter().copied().collect()
                    } else {
                        HashSet::new()
                    }
                })
                .unwrap_or_default();
            let base_url = Url::parse(&resolved_url).ok();
            for script in inline_scripts {
                let code = match script {
                    SubdocScript::Inline(c) => Some(c),
                    SubdocScript::External(src) => {
                        let script_url = base_url
                            .as_ref()
                            .and_then(|b| b.join(&src).ok())
                            .map(|u| u.to_string())
                            .unwrap_or(src);
                        if script_url.starts_with("http://") || script_url.starts_with("https://") {
                            fetch_iframe_url_sync(net, &script_url, &effective_child_origin)
                                .map(|(body, _, _)| body)
                        } else {
                            None
                        }
                    }
                };
                if let Some(code) = code {
                    vm.global = proxy_h;
                    let _ = vm.eval_script(&code);
                    vm.global = saved_global;
                }
            }
            bind_child_frame_functions(vm, proxy_h, &pre_keys);
        }

        // Fire async `load` event on the `<iframe>` element (supports both `.onload` and inline `onload="..."`).
        let h = Rc::clone(host);
        let ev = Rc::clone(events);
        let el = Rc::clone(event_loop);
        event_loop
            .borrow_mut()
            .post_host_task(TaskPriority::Networking, move |vm| {
                let _ = fire_dom_node_event(vm, &h, &ev, &el, node_id, "load", false, false, &[]);
            });
    } else {
        // Cross-origin OOPIF (or opaque sandbox): allocate restricted WindowProxy + emit IPC.
        let existing_remote = frames
            .borrow()
            .iframes
            .get(&node_id)
            .and_then(|r| r.remote_pipeline);
        let proxy_h = alloc_window_proxy(
            vm,
            web_apis,
            frames,
            WindowProxyKind::CrossOriginRemote {
                remote_pipeline: existing_remote,
                origin: effective_child_origin,
                url: resolved_url.clone(),
                closed: false,
                crashed: false,
            },
        );
        let parent_pipeline = frames.borrow().pipeline_id;
        let mut fs = frames.borrow_mut();
        if !was_existing {
            fs.iframe_order.push(node_id);
            fs.outbound_ipc
                .push_back(ContentToBrowser::CreateChildFrame {
                    parent_pipeline,
                    frame_node_id: node_id_u64(node_id),
                    surface_id,
                    url: resolved_url,
                    srcdoc: srcdoc_attr.clone(),
                    sandbox: sandbox_attr.clone(),
                    width,
                    height,
                });
        } else {
            fs.outbound_ipc
                .push_back(ContentToBrowser::NavigateChildFrame {
                    parent_pipeline,
                    frame_node_id: node_id_u64(node_id),
                    url: resolved_url,
                    srcdoc: srcdoc_attr.clone(),
                    sandbox: sandbox_attr.clone(),
                });
        }
        fs.iframes.insert(
            node_id,
            IframeRecord {
                node_id,
                surface_id,
                proxy: proxy_h,
                is_cross_origin: true,
                remote_pipeline: existing_remote,
                last_src: src_attr,
                last_srcdoc: srcdoc_attr,
                last_sandbox: sandbox_attr,
                crashed: false,
            },
        );
    }
}

fn extract_content_type_charset(headers: &HashMap<String, String>) -> Option<String> {
    let ct = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.as_str())?;
    for part in ct.split(';').skip(1) {
        let trimmed = part.trim();
        if let Some((k, v)) = trimmed.split_once('=') {
            if k.trim().eq_ignore_ascii_case("charset") {
                let val = v.trim().trim_matches('"').trim_matches('\'');
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}

pub(crate) fn fetch_iframe_url_sync(
    net: &SharedNetBridge,
    url: &str,
    origin: &str,
) -> Option<(String, &'static str, String)> {
    let engine = std::sync::Arc::clone(&net.borrow().fetch_engine);
    let req = rix_net::FetchRequest {
        url: url.to_string(),
        method: "GET".to_string(),
        headers: HashMap::new(),
        body: Vec::new(),
        mode: rix_net::FetchMode::SameOrigin,
        credentials: rix_net::FetchCredentials::SameOrigin,
        redirect: rix_net::FetchRedirect::Follow,
        origin: origin.to_string(),
        abort_signal: None,
    };
    let res = if let Ok(handle) = tokio::runtime::Handle::try_current() {
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
    res.map(|r| {
        let ct = r
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "text/html".to_string());
        let http_cs = extract_content_type_charset(&r.headers);
        let (decoded, cs) = decode_html_bytes(&r.body, http_cs.as_deref());
        (decoded, cs, ct)
    })
}

// ---------------------------------------------------------------------------
// Host IPC delivery into `ContentRuntime` (cross-process OOPIF support)
// ---------------------------------------------------------------------------

/// Binds a spawned cross-origin OOPIF `child_pipeline` to an `<iframe>` node in this parent runtime.
pub fn bind_child_frame(
    frames: &SharedFrames,
    frame_node_id: u64,
    child_pipeline: PipelineId,
    origin: String,
) {
    let mut fs = frames.borrow_mut();
    let mut proxy_to_update = None;
    for rec in fs.iframes.values_mut() {
        if node_id_u64(rec.node_id) == frame_node_id {
            rec.remote_pipeline = Some(child_pipeline);
            rec.crashed = false;
            proxy_to_update = Some(rec.proxy);
            break;
        }
    }
    if let Some(proxy_h) = proxy_to_update {
        fs.remote_windows.insert(child_pipeline, proxy_h);
        if let Some(WindowProxyKind::CrossOriginRemote {
            remote_pipeline,
            origin: o,
            crashed,
            ..
        }) = fs.proxies.get_mut(&proxy_h)
        {
            *remote_pipeline = Some(child_pipeline);
            if !origin.is_empty() {
                *o = origin;
            }
            *crashed = false;
        }
    }
}

/// Marks a bound cross-origin OOPIF as crashed inside the parent runtime without harming the parent.
pub fn mark_child_frame_crashed(frames: &SharedFrames, child_pipeline: PipelineId) -> Vec<NodeId> {
    let mut fs = frames.borrow_mut();
    let mut affected = Vec::new();
    let mut crashed_proxies = Vec::new();
    for rec in fs.iframes.values_mut() {
        if rec.remote_pipeline == Some(child_pipeline) {
            rec.crashed = true;
            affected.push(rec.node_id);
            crashed_proxies.push(rec.proxy);
        }
    }
    for ph in crashed_proxies {
        if let Some(WindowProxyKind::CrossOriginRemote { crashed, .. }) = fs.proxies.get_mut(&ph) {
            *crashed = true;
        }
    }
    affected
}

/// Delivers an inbound cross-process `window.postMessage` IPC payload into this runtime.
#[allow(clippy::too_many_arguments)]
pub fn deliver_ipc_post_message(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    frames: &SharedFrames,
    source_pipeline: PipelineId,
    data_json: &str,
    origin: &str,
    transfer_ports: &[MessagePortId],
) {
    let data = serde_json::from_str::<serde_json::Value>(data_json)
        .map(|jv| decode_structured_json(vm, &jv))
        .unwrap_or(JsValue::NULL);

    // Ensure `event.source` is a cross-origin WindowProxy pointing back to `source_pipeline`.
    let existing_proxy = frames
        .borrow()
        .remote_windows
        .get(&source_pipeline)
        .copied();
    let source_proxy = if let Some(existing) = existing_proxy {
        existing
    } else {
        let p = alloc_window_proxy(
            vm,
            web_apis,
            frames,
            WindowProxyKind::CrossOriginRemote {
                remote_pipeline: Some(source_pipeline),
                origin: origin.to_string(),
                url: origin.to_string(),
                closed: false,
                crashed: false,
            },
        );
        frames
            .borrow_mut()
            .remote_windows
            .insert(source_pipeline, p);
        p
    };

    // Materialize transferred `MessagePort` endpoints wired back to `source_pipeline`.
    let mut port_handles = Vec::with_capacity(transfer_ports.len());
    for &pid in transfer_ports {
        let ph = alloc_message_port(vm, frames, pid, Some(pid), Some(source_pipeline));
        port_handles.push(ph);
    }

    let h = Rc::clone(host);
    let ev = Rc::clone(events);
    let el = Rc::clone(event_loop);
    let fr = Rc::clone(frames);
    let origin_str = origin.to_string();
    let target_h = vm.global;
    event_loop
        .borrow_mut()
        .post_host_task(TaskPriority::Networking, move |vm| {
            let ports_arr =
                alloc_array_from(vm, port_handles.iter().map(|&ph| JsValue::from_object(ph)));
            let origin_js = JsValue::from_string(vm.heap.alloc_string(&origin_str));
            let empty_js = JsValue::from_string(vm.heap.alloc_string(""));
            let proto = fr
                .borrow()
                .message_event_proto
                .or_else(|| ev.borrow().event_proto)
                .unwrap_or(vm.object_proto);
            fire_object_event_with_proto(
                vm,
                &h,
                &ev,
                &el,
                target_h,
                "message",
                proto,
                &[
                    ("data", data),
                    ("origin", origin_js),
                    ("lastEventId", empty_js),
                    ("source", JsValue::from_object(source_proxy)),
                    ("ports", JsValue::from_object(ports_arr)),
                ],
            );
        });
}

/// Delivers an inbound cross-process `MessagePort` IPC payload into this runtime.
#[allow(clippy::too_many_arguments)]
pub fn deliver_ipc_port_message(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    frames: &SharedFrames,
    source_pipeline: PipelineId,
    port_id: MessagePortId,
    data_json: &str,
    transfer_ports: &[MessagePortId],
) {
    let target_port_h = {
        let fs = frames.borrow();
        fs.port_by_id.get(&port_id).copied().or_else(|| {
            fs.ports
                .iter()
                .find(|(_, p)| !p.closed && !p.neutered && p.partner_id == Some(port_id))
                .map(|(&h, _)| h)
        })
    };
    let Some(target_port_h) = target_port_h else {
        return;
    };
    let data = serde_json::from_str::<serde_json::Value>(data_json)
        .map(|jv| decode_structured_json(vm, &jv))
        .unwrap_or(JsValue::NULL);
    let mut port_handles = Vec::with_capacity(transfer_ports.len());
    for &pid in transfer_ports {
        let ph = alloc_message_port(vm, frames, pid, Some(pid), Some(source_pipeline));
        port_handles.push(ph);
    }
    let should_flush = {
        let mut fs = frames.borrow_mut();
        match fs.ports.get_mut(&target_port_h) {
            Some(p) if !p.closed && !p.neutered => {
                p.pending.push_back(QueuedPortMessage {
                    data,
                    ports: port_handles,
                });
                p.started
            }
            _ => false,
        }
    };
    if should_flush {
        schedule_port_flush(host, events, event_loop, frames, target_port_h);
    }
}

// ---------------------------------------------------------------------------
// Installation on `JsVm`
// ---------------------------------------------------------------------------

/// Installs `MessageEvent`, `MessageChannel`, `MessagePort`, `window.postMessage`,
/// `window.open` / `window.close` / browsing context getters, and `HTMLIFrameElement`
/// properties onto `vm`.
#[allow(clippy::too_many_arguments)]
pub fn install_frame_and_messaging_apis(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
) -> SharedFrames {
    let frames = Rc::new(RefCell::new(FrameSystem::new(vm.global)));

    install_message_event(vm, events, &frames);
    install_message_channel_and_port(vm, host, events, event_loop, web_apis, &frames);
    install_window_proxy_proto(vm, host, events, event_loop, web_apis, &frames);
    install_window_browsing_context(vm, host, events, event_loop, web_apis, &frames);
    install_html_iframe_element_proto(vm, host, events, event_loop, web_apis, net, &frames);
    install_hyperlink_and_form_protos(vm, host, events, event_loop, web_apis, net, &frames);

    frames
}

fn install_message_event(vm: &mut JsVm, events: &SharedEvents, frames: &SharedFrames) {
    let ep = events.borrow().event_proto.unwrap_or(vm.object_proto);
    let me_proto = vm.heap.alloc_empty_object();
    vm.heap
        .object_set_prototype(me_proto, Some(ep), &mut vm.shapes);
    vm.heap.get_object_mut(me_proto).unwrap().is_prototype = true;
    frames.borrow_mut().message_event_proto = Some(me_proto);

    let ev_ref = Rc::clone(events);
    let fr_ref = Rc::clone(frames);
    let ctor = vm.alloc_host_fn("MessageEvent", move |vm, _, args| {
        if args.is_empty() {
            return Err(vm.type_error("MessageEvent constructor requires a type argument"));
        }
        let ev_type = vm.value_to_rust_string(arg(args, 0));
        let init = arg(args, 1);
        let mut bubbles = false;
        let mut cancelable = false;
        let mut composed = false;
        let mut data = JsValue::NULL;
        let mut origin = String::new();
        let mut last_event_id = String::new();
        let mut source = JsValue::NULL;
        let mut ports_vec: Vec<JsValue> = Vec::new();

        if let Some(ih) = init.as_object() {
            let get_prop = |vm: &mut JsVm, name: &str| {
                let k = vm.heap.intern_key(name);
                vm.get_property_by_key(ih, k)
            };
            let v_b = get_prop(vm, "bubbles")?;
            if !v_b.is_undefined() {
                bubbles = vm.to_boolean(v_b);
            }
            let v_c = get_prop(vm, "cancelable")?;
            if !v_c.is_undefined() {
                cancelable = vm.to_boolean(v_c);
            }
            let v_co = get_prop(vm, "composed")?;
            if !v_co.is_undefined() {
                composed = vm.to_boolean(v_co);
            }
            let v_d = get_prop(vm, "data")?;
            if !v_d.is_undefined() {
                data = v_d;
            }
            let v_o = get_prop(vm, "origin")?;
            if !v_o.is_undefined() {
                origin = vm.value_to_rust_string(v_o);
            }
            let v_lid = get_prop(vm, "lastEventId")?;
            if !v_lid.is_undefined() {
                last_event_id = vm.value_to_rust_string(v_lid);
            }
            let v_src = get_prop(vm, "source")?;
            if !v_src.is_undefined() {
                source = v_src;
            }
            let v_ports = get_prop(vm, "ports")?;
            if !v_ports.is_undefined() && !v_ports.is_null() {
                let Some(ph) = v_ports.as_object() else {
                    return Err(vm.type_error("MessageEventInit.ports must be a sequence"));
                };
                let len = array_len(vm, ph);
                for i in 0..len {
                    let pv = array_elem(vm, ph, i);
                    let valid = pv
                        .as_object()
                        .is_some_and(|h| fr_ref.borrow().ports.contains_key(&h));
                    if !valid {
                        return Err(vm.type_error(
                            "MessageEventInit.ports must contain MessagePort objects",
                        ));
                    }
                    ports_vec.push(pv);
                }
            }
        }

        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(me_proto)));
        register_constructed_event(&ev_ref, obj, ev_type, bubbles, cancelable, composed);
        let ports_arr = alloc_array_from(vm, ports_vec);
        for (name, val) in [
            ("data", data),
            (
                "origin",
                JsValue::from_string(vm.heap.alloc_string(&origin)),
            ),
            (
                "lastEventId",
                JsValue::from_string(vm.heap.alloc_string(&last_event_id)),
            ),
            ("source", source),
            ("ports", JsValue::from_object(ports_arr)),
        ] {
            let k = vm.heap.intern_key(name);
            vm.heap
                .object_define_data(obj, k, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
        }
        Ok(JsValue::from_object(obj))
    });
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(me_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        me_proto,
        vm.key_constructor,
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.mark_constructor(ctor);
    define_global(vm, "MessageEvent", JsValue::from_object(ctor));
}

fn install_message_channel_and_port(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    frames: &SharedFrames,
) {
    let etp = host.borrow().prototypes.event_target_proto;
    let port_proto = vm.heap.alloc_empty_object();
    vm.heap
        .object_set_prototype(port_proto, Some(etp), &mut vm.shapes);
    vm.heap.get_object_mut(port_proto).unwrap().is_prototype = true;
    frames.borrow_mut().message_port_proto = Some(port_proto);

    // MessagePort.prototype.start()
    {
        let h = Rc::clone(host);
        let ev = Rc::clone(events);
        let el = Rc::clone(event_loop);
        let fr = Rc::clone(frames);
        define_host_method(vm, port_proto, "start", move |vm, this_val, _| {
            let Some(ph) = this_val.as_object() else {
                return Err(vm.type_error("MessagePort.start called on invalid receiver"));
            };
            let has_pending = {
                let mut fs = fr.borrow_mut();
                let Some(p) = fs.ports.get_mut(&ph) else {
                    return Err(vm.type_error("Illegal invocation of MessagePort.start"));
                };
                p.started = true;
                !p.pending.is_empty() && !p.closed && !p.neutered
            };
            if has_pending {
                schedule_port_flush(&h, &ev, &el, &fr, ph);
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // MessagePort.prototype.close()
    {
        let fr = Rc::clone(frames);
        define_host_method(vm, port_proto, "close", move |vm, this_val, _| {
            let Some(ph) = this_val.as_object() else {
                return Err(vm.type_error("MessagePort.close called on invalid receiver"));
            };
            let mut fs = fr.borrow_mut();
            let (id, remote) = match fs.ports.get_mut(&ph) {
                Some(p) => {
                    p.closed = true;
                    p.partner_id = None;
                    p.pending.clear();
                    (p.id, p.remote_pipeline.take())
                }
                None => return Err(vm.type_error("Illegal invocation of MessagePort.close")),
            };
            if remote.is_some() {
                fs.outbound_ipc
                    .push_back(ContentToBrowser::MessagePortClose { port_id: id });
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // MessagePort.prototype.onmessage (setting it implicitly invokes `start()` per WHATWG spec!)
    {
        let h = Rc::clone(host);
        let ev = Rc::clone(events);
        let el = Rc::clone(event_loop);
        let fr = Rc::clone(frames);
        define_host_accessor(
            vm,
            port_proto,
            "onmessage",
            {
                let fr = Rc::clone(&fr);
                move |_vm, this_val, _| {
                    let Some(ph) = this_val.as_object() else {
                        return Ok(JsValue::NULL);
                    };
                    Ok(fr
                        .borrow()
                        .ports
                        .get(&ph)
                        .map_or(JsValue::NULL, |p| p.onmessage))
                }
            },
            move |_vm, this_val, args| {
                let Some(ph) = this_val.as_object() else {
                    return Ok(JsValue::UNDEFINED);
                };
                let handler = arg(args, 0);
                let val = if handler.as_object().is_some() {
                    handler
                } else {
                    JsValue::NULL
                };
                let has_pending = {
                    let mut fs = fr.borrow_mut();
                    if let Some(p) = fs.ports.get_mut(&ph) {
                        p.onmessage = val;
                        p.started = true;
                        !p.pending.is_empty() && !p.closed && !p.neutered
                    } else {
                        false
                    }
                };
                if has_pending {
                    schedule_port_flush(&h, &ev, &el, &fr, ph);
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // MessagePort.prototype.onmessageerror
    {
        let fr = Rc::clone(frames);
        define_host_accessor(
            vm,
            port_proto,
            "onmessageerror",
            {
                let fr = Rc::clone(&fr);
                move |_vm, this_val, _| {
                    let Some(ph) = this_val.as_object() else {
                        return Ok(JsValue::NULL);
                    };
                    Ok(fr
                        .borrow()
                        .ports
                        .get(&ph)
                        .map_or(JsValue::NULL, |p| p.onmessageerror))
                }
            },
            move |_vm, this_val, args| {
                let Some(ph) = this_val.as_object() else {
                    return Ok(JsValue::UNDEFINED);
                };
                let v = arg(args, 0);
                if let Some(p) = fr.borrow_mut().ports.get_mut(&ph) {
                    p.onmessageerror = if v.as_object().is_some() {
                        v
                    } else {
                        JsValue::NULL
                    };
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // MessagePort.prototype.postMessage(message, transferOrOptions?)
    {
        let h = Rc::clone(host);
        let ev = Rc::clone(events);
        let el = Rc::clone(event_loop);
        let wa = Rc::clone(web_apis);
        let fr = Rc::clone(frames);
        define_host_method(vm, port_proto, "postMessage", move |vm, this_val, args| {
            let Some(src_h) = this_val.as_object() else {
                return Err(vm.type_error("MessagePort.postMessage called on invalid receiver"));
            };
            if args.is_empty() {
                return Err(vm.type_error("MessagePort.postMessage requires at least 1 argument"));
            }
            let msg_val = arg(args, 0);
            let second = arg(args, 1);
            let transfer_val = if let Some(oh) = second.as_object() {
                if is_js_array(vm, oh) {
                    second
                } else {
                    let kt = vm.heap.intern_key("transfer");
                    vm.get_property_by_key(oh, kt)?
                }
            } else {
                JsValue::UNDEFINED
            };

            let (closed_or_neutered, partner_id, partner_h, remote_pipeline) = {
                let fs = fr.borrow();
                let Some(src) = fs.ports.get(&src_h) else {
                    return Err(vm.type_error("Illegal invocation of MessagePort.postMessage"));
                };
                let ph = src
                    .partner_id
                    .and_then(|pid| fs.port_by_id.get(&pid).copied());
                (
                    src.closed || src.neutered,
                    src.partner_id,
                    ph,
                    src.remote_pipeline,
                )
            };

            let mut disallowed = vec![src_h];
            if let Some(ph) = partner_h {
                disallowed.push(ph);
            }
            let transfer_entries = parse_transfer_list(vm, &fr, transfer_val, &disallowed)?;

            if closed_or_neutered {
                // Validate cloning even when closed? Per spec, cloning still runs before drop.
                let mut memo = HashMap::new();
                let _ = structured_clone_inner(vm, &wa, msg_val, &mut memo)?;
                return Ok(JsValue::UNDEFINED);
            }

            // Cross-process partner port!
            if let (Some(pid), Some(remote_pipe)) = (partner_id, remote_pipeline) {
                let mut seen = HashSet::new();
                let json_val = encode_structured_json(vm, msg_val, &mut seen)?;
                let data_json =
                    serde_json::to_string(&json_val).unwrap_or_else(|_| "null".to_string());
                let transfer_ports =
                    execute_cross_process_transfer(vm, &fr, remote_pipe, transfer_entries);
                let source_pipeline = fr.borrow().pipeline_id;
                fr.borrow_mut()
                    .outbound_ipc
                    .push_back(ContentToBrowser::MessagePortSend {
                        source_pipeline,
                        port_id: pid,
                        data_json,
                        transfer_ports,
                    });
                return Ok(JsValue::UNDEFINED);
            }

            // In-process partner port!
            let (transferred_ports, mut memo) =
                execute_in_process_transfer(vm, &fr, transfer_entries);
            let cloned_data = structured_clone_inner(vm, &wa, msg_val, &mut memo)?;
            let Some(target_h) = partner_h else {
                return Ok(JsValue::UNDEFINED);
            };

            let should_flush = {
                let mut fs = fr.borrow_mut();
                match fs.ports.get_mut(&target_h) {
                    Some(partner) if !partner.closed && !partner.neutered => {
                        partner.pending.push_back(QueuedPortMessage {
                            data: cloned_data,
                            ports: transferred_ports,
                        });
                        partner.started
                    }
                    _ => false,
                }
            };
            if should_flush {
                schedule_port_flush(&h, &ev, &el, &fr, target_h);
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // MessagePort global constructor (cannot be directly constructed in JS, exposes prototype)
    let port_ctor = vm.alloc_host_fn("MessagePort", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: use new MessageChannel()"))
    });
    vm.heap.object_define_data(
        port_ctor,
        vm.key_prototype,
        JsValue::from_object(port_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        port_proto,
        vm.key_constructor,
        JsValue::from_object(port_ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.mark_constructor(port_ctor);
    define_global(vm, "MessagePort", JsValue::from_object(port_ctor));

    // MessageChannel constructor
    let fr_mc = Rc::clone(frames);
    let mc_ctor = vm.alloc_host_fn("MessageChannel", move |vm, _, _| {
        let pipe = fr_mc.borrow().pipeline_id;
        let id1 = alloc_port_id(pipe);
        let id2 = alloc_port_id(pipe);
        let p1 = alloc_message_port(vm, &fr_mc, id1, Some(id2), None);
        let p2 = alloc_message_port(vm, &fr_mc, id2, Some(id1), None);
        let ch = vm.alloc_ordinary_object();
        let k1 = vm.heap.intern_key("port1");
        let k2 = vm.heap.intern_key("port2");
        vm.heap.object_define_data(
            ch,
            k1,
            JsValue::from_object(p1),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            ch,
            k2,
            JsValue::from_object(p2),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        Ok(JsValue::from_object(ch))
    });
    vm.mark_constructor(mc_ctor);
    define_global(vm, "MessageChannel", JsValue::from_object(mc_ctor));
}

fn install_window_proxy_proto(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    frames: &SharedFrames,
) {
    let etp = host.borrow().prototypes.event_target_proto;
    let wp = vm.heap.alloc_empty_object();
    vm.heap.object_set_prototype(wp, Some(etp), &mut vm.shapes);
    vm.heap.get_object_mut(wp).unwrap().is_prototype = true;
    frames.borrow_mut().window_proxy_proto = Some(wp);

    // proxy.postMessage(message, targetOriginOrOptions, transfer?)
    {
        let h = Rc::clone(host);
        let ev = Rc::clone(events);
        let el = Rc::clone(event_loop);
        let wa = Rc::clone(web_apis);
        let fr = Rc::clone(frames);
        define_host_method(vm, wp, "postMessage", move |vm, this_val, args| {
            let Some(target_h) = this_val.as_object() else {
                return Err(vm.type_error("postMessage called on invalid window"));
            };
            let source_val = JsValue::from_object(vm.global);
            perform_window_post_message(vm, &h, &ev, &el, &wa, &fr, target_h, source_val, args)
        });
    }

    // proxy.close()
    {
        let fr = Rc::clone(frames);
        define_host_method(vm, wp, "close", move |_vm, this_val, _| {
            if let Some(h) = this_val.as_object() {
                if let Some(proxy) = fr.borrow_mut().proxies.get_mut(&h) {
                    match proxy {
                        WindowProxyKind::SameOriginChild { closed, .. }
                        | WindowProxyKind::CrossOriginRemote { closed, .. }
                        | WindowProxyKind::Auxiliary { closed, .. } => *closed = true,
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // proxy.focus() / proxy.blur() (allowed cross-origin no-ops)
    define_host_method(vm, wp, "focus", |_vm, _, _| Ok(JsValue::UNDEFINED));
    define_host_method(vm, wp, "blur", |_vm, _, _| Ok(JsValue::UNDEFINED));

    // proxy.closed (always readable cross-origin!)
    {
        let fr = Rc::clone(frames);
        define_host_getter(vm, wp, "closed", move |_vm, this_val, _| {
            let c = this_val
                .as_object()
                .and_then(|h| {
                    fr.borrow().proxies.get(&h).map(|p| match p {
                        WindowProxyKind::SameOriginChild { closed, .. }
                        | WindowProxyKind::CrossOriginRemote { closed, .. }
                        | WindowProxyKind::Auxiliary { closed, .. } => *closed,
                    })
                })
                .unwrap_or(false);
            Ok(JsValue::from_bool(c))
        });
    }

    // proxy.self / proxy.window / proxy.frames
    define_host_getter(vm, wp, "self", |_vm, this_val, _| Ok(this_val));
    define_host_getter(vm, wp, "window", |_vm, this_val, _| Ok(this_val));
    define_host_getter(vm, wp, "frames", |_vm, this_val, _| Ok(this_val));
    define_host_getter(vm, wp, "length", |_vm, _, _| Ok(JsValue::from_i32(0)));

    // proxy.parent / proxy.top
    {
        let fr = Rc::clone(frames);
        define_host_getter(vm, wp, "parent", move |vm, this_val, _| {
            let is_aux = this_val.as_object().is_some_and(|h| {
                matches!(
                    fr.borrow().proxies.get(&h),
                    Some(WindowProxyKind::Auxiliary { .. })
                )
            });
            if is_aux {
                Ok(this_val)
            } else {
                Ok(JsValue::from_object(vm.global))
            }
        });
    }
    {
        let fr = Rc::clone(frames);
        define_host_getter(vm, wp, "top", move |_vm, this_val, _| {
            let is_aux = this_val.as_object().is_some_and(|h| {
                matches!(
                    fr.borrow().proxies.get(&h),
                    Some(WindowProxyKind::Auxiliary { .. })
                )
            });
            if is_aux {
                Ok(this_val)
            } else {
                Ok(fr.borrow().top_window)
            }
        });
    }

    // proxy.opener (readable & clearable for auxiliary windows)
    {
        let fr = Rc::clone(frames);
        define_host_accessor(
            vm,
            wp,
            "opener",
            {
                let fr = Rc::clone(&fr);
                move |_vm, this_val, _| {
                    Ok(this_val
                        .as_object()
                        .and_then(|h| match fr.borrow().proxies.get(&h) {
                            Some(WindowProxyKind::Auxiliary { opener, .. }) => Some(*opener),
                            _ => None,
                        })
                        .unwrap_or(JsValue::NULL))
                }
            },
            move |_vm, this_val, args| {
                if let Some(h) = this_val.as_object() {
                    if let Some(WindowProxyKind::Auxiliary { opener, .. }) =
                        fr.borrow_mut().proxies.get_mut(&h)
                    {
                        *opener = arg(args, 0);
                    }
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // proxy.name (same-origin readable/writable; cross-origin throws SecurityError!)
    {
        let fr = Rc::clone(frames);
        let wa = Rc::clone(web_apis);
        define_host_accessor(
            vm,
            wp,
            "name",
            {
                let fr = Rc::clone(&fr);
                let wa = Rc::clone(&wa);
                move |vm, this_val, _| {
                    let caller = fr.borrow().effective_origin(&wa);
                    let Some(h) = this_val.as_object() else {
                        return Ok(JsValue::from_string(vm.heap.alloc_string("")));
                    };
                    match fr.borrow().proxies.get(&h) {
                        Some(WindowProxyKind::SameOriginChild { name, .. })
                        | Some(WindowProxyKind::Auxiliary {
                            is_cross_origin: false,
                            name,
                            ..
                        }) => Ok(JsValue::from_string(vm.heap.alloc_string(name))),
                        Some(WindowProxyKind::CrossOriginRemote { origin, .. })
                        | Some(WindowProxyKind::Auxiliary {
                            is_cross_origin: true,
                            origin,
                            ..
                        }) => Err(security_error(vm, &caller, origin)),
                        None => Ok(JsValue::from_string(vm.heap.alloc_string(""))),
                    }
                }
            },
            move |vm, this_val, args| {
                let caller = fr.borrow().effective_origin(&wa);
                let Some(h) = this_val.as_object() else {
                    return Ok(JsValue::UNDEFINED);
                };
                let new_name = vm.value_to_rust_string(arg(args, 0));
                let mut fs = fr.borrow_mut();
                match fs.proxies.get_mut(&h) {
                    Some(WindowProxyKind::SameOriginChild { name, .. })
                    | Some(WindowProxyKind::Auxiliary {
                        is_cross_origin: false,
                        name,
                        ..
                    }) => {
                        *name = new_name;
                        Ok(JsValue::UNDEFINED)
                    }
                    Some(WindowProxyKind::CrossOriginRemote { origin, .. })
                    | Some(WindowProxyKind::Auxiliary {
                        is_cross_origin: true,
                        origin,
                        ..
                    }) => {
                        let o = origin.clone();
                        drop(fs);
                        Err(security_error(vm, &caller, &o))
                    }
                    None => Ok(JsValue::UNDEFINED),
                }
            },
        );
    }

    // proxy.document & proxy.origin (same-origin allowed; cross-origin throws SecurityError!)
    {
        let fr = Rc::clone(frames);
        let wa = Rc::clone(web_apis);
        define_host_getter(vm, wp, "document", move |vm, this_val, _| {
            let caller = fr.borrow().effective_origin(&wa);
            let Some(h) = this_val.as_object() else {
                return Ok(JsValue::NULL);
            };
            match fr.borrow().proxies.get(&h) {
                Some(WindowProxyKind::SameOriginChild { child_doc, .. }) => {
                    Ok(JsValue::from_object(*child_doc))
                }
                Some(WindowProxyKind::Auxiliary {
                    is_cross_origin: false,
                    doc: Some(d),
                    ..
                }) => Ok(JsValue::from_object(*d)),
                Some(WindowProxyKind::CrossOriginRemote { origin, .. })
                | Some(WindowProxyKind::Auxiliary {
                    is_cross_origin: true,
                    origin,
                    ..
                }) => Err(security_error(vm, &caller, origin)),
                _ => Ok(JsValue::NULL),
            }
        });
    }
    {
        let fr = Rc::clone(frames);
        let wa = Rc::clone(web_apis);
        define_host_getter(vm, wp, "origin", move |vm, this_val, _| {
            let caller = fr.borrow().effective_origin(&wa);
            let Some(h) = this_val.as_object() else {
                return Ok(JsValue::from_string(vm.heap.alloc_string("null")));
            };
            match fr.borrow().proxies.get(&h) {
                Some(WindowProxyKind::SameOriginChild { origin, .. })
                | Some(WindowProxyKind::Auxiliary {
                    is_cross_origin: false,
                    origin,
                    ..
                }) => Ok(JsValue::from_string(vm.heap.alloc_string(origin))),
                Some(WindowProxyKind::CrossOriginRemote { origin, .. })
                | Some(WindowProxyKind::Auxiliary {
                    is_cross_origin: true,
                    origin,
                    ..
                }) => Err(security_error(vm, &caller, origin)),
                None => Ok(JsValue::from_string(vm.heap.alloc_string("null"))),
            }
        });
    }
    {
        let fr = Rc::clone(frames);
        let h_ref = Rc::clone(host);
        let wa = Rc::clone(web_apis);
        define_host_getter(vm, wp, "frameElement", move |vm, this_val, _| {
            let caller = fr.borrow().effective_origin(&wa);
            let Some(h) = this_val.as_object() else {
                return Ok(JsValue::NULL);
            };
            match fr.borrow().proxies.get(&h) {
                Some(WindowProxyKind::SameOriginChild { iframe_node, .. }) => {
                    Ok(wrap_node(vm, &h_ref, *iframe_node))
                }
                Some(WindowProxyKind::Auxiliary {
                    is_cross_origin: false,
                    ..
                }) => Ok(JsValue::NULL),
                Some(WindowProxyKind::CrossOriginRemote { origin, .. })
                | Some(WindowProxyKind::Auxiliary {
                    is_cross_origin: true,
                    origin,
                    ..
                }) => Err(security_error(vm, &caller, origin)),
                None => Ok(JsValue::NULL),
            }
        });
    }
}

fn install_window_browsing_context(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    frames: &SharedFrames,
) {
    let g = vm.global;

    // window.postMessage(message, targetOriginOrOptions, transfer?)
    {
        let h = Rc::clone(host);
        let ev = Rc::clone(events);
        let el = Rc::clone(event_loop);
        let wa = Rc::clone(web_apis);
        let fr = Rc::clone(frames);
        define_host_method(vm, g, "postMessage", move |vm, _this_val, args| {
            let target_h = vm.global;
            let source_val = JsValue::from_object(vm.global);
            perform_window_post_message(vm, &h, &ev, &el, &wa, &fr, target_h, source_val, args)
        });
    }

    // window.open(url?, target?, features?)
    {
        let h_open = Rc::clone(host);
        let wa = Rc::clone(web_apis);
        let fr = Rc::clone(frames);
        define_host_method(vm, g, "open", move |vm, _, args| {
            if !fr.borrow().sandbox.popups_allowed() {
                return Ok(JsValue::NULL);
            }
            let raw_url = if args.is_empty() || arg(args, 0).is_undefined() {
                String::new()
            } else {
                vm.value_to_rust_string(arg(args, 0))
            };
            let target = if args.len() < 2 || arg(args, 1).is_undefined() {
                "_blank".to_string()
            } else {
                vm.value_to_rust_string(arg(args, 1))
            };
            let features_str = if args.len() < 3 || arg(args, 2).is_undefined() {
                String::new()
            } else {
                vm.value_to_rust_string(arg(args, 2))
            };

            let (noopener, noreferrer) = parse_window_features(&features_str);
            let suppress_opener = noopener || noreferrer;

            // Special targets (`_self`, `_parent`, `_top`)
            if target.eq_ignore_ascii_case("_self") {
                return Ok(if suppress_opener {
                    JsValue::NULL
                } else {
                    JsValue::from_object(vm.global)
                });
            }
            if target.eq_ignore_ascii_case("_parent") {
                let p = fr.borrow().parent_window;
                return Ok(if suppress_opener { JsValue::NULL } else { p });
            }
            if target.eq_ignore_ascii_case("_top") {
                let t = fr.borrow().top_window;
                return Ok(if suppress_opener { JsValue::NULL } else { t });
            }

            let parent_href = wa.borrow().current_href();
            let parent_origin = fr.borrow().effective_origin(&wa);
            let resolved_url = if raw_url.is_empty() || raw_url.eq_ignore_ascii_case("about:blank")
            {
                "about:blank".to_string()
            } else {
                let base = Url::parse(&parent_href).map_err(|_| {
                    create_dom_exception(
                        vm,
                        "SyntaxError",
                        &format!("Invalid base URL: {parent_href}"),
                    )
                })?;
                base.join(&raw_url).map(|u| u.to_string()).map_err(|_| {
                    create_dom_exception(vm, "SyntaxError", &format!("Invalid URL: {raw_url}"))
                })?
            };

            let target_origin = if resolved_url == "about:blank" {
                parent_origin.clone()
            } else {
                Url::parse(&resolved_url)
                    .map(|u| origin_of_url(&u))
                    .unwrap_or_else(|_| "null".to_string())
            };
            let is_cross_origin = parent_origin == "null"
                || target_origin == "null"
                || parent_origin != target_origin;

            // Reuse existing open named auxiliary window (unless `_blank` or empty name).
            let effective_name = if target.is_empty() || target.eq_ignore_ascii_case("_blank") {
                String::new()
            } else {
                target.clone()
            };
            if !effective_name.is_empty() {
                let found = {
                    let fs = fr.borrow();
                    fs.auxiliary_windows.iter().copied().find(|&wh| {
                        matches!(
                            fs.proxies.get(&wh),
                            Some(WindowProxyKind::Auxiliary { name, closed: false, .. })
                                if name == &effective_name
                        )
                    })
                };
                if let Some(existing_h) = found {
                    if let Some(WindowProxyKind::Auxiliary {
                        url,
                        origin,
                        is_cross_origin: ico,
                        opener,
                        ..
                    }) = fr.borrow_mut().proxies.get_mut(&existing_h)
                    {
                        if !raw_url.is_empty() {
                            *url = resolved_url;
                            *origin = target_origin;
                            *ico = is_cross_origin;
                        }
                        if suppress_opener {
                            *opener = JsValue::NULL;
                        }
                    }
                    return Ok(if suppress_opener {
                        JsValue::NULL
                    } else {
                        JsValue::from_object(existing_h)
                    });
                }
            }

            let opener_val = if suppress_opener {
                JsValue::NULL
            } else {
                JsValue::from_object(vm.global)
            };
            let proxy_h = alloc_window_proxy(
                vm,
                &wa,
                &fr,
                WindowProxyKind::Auxiliary {
                    is_cross_origin,
                    origin: target_origin,
                    url: resolved_url.clone(),
                    name: effective_name,
                    closed: false,
                    opener: opener_val,
                    doc: None,
                },
            );
            if !is_cross_origin {
                let (doc_h, _, _) = build_sub_document(
                    vm,
                    &h_open,
                    &resolved_url,
                    "",
                    "UTF-8",
                    "text/html",
                    proxy_h,
                );
                if let Some(WindowProxyKind::Auxiliary { doc, .. }) =
                    fr.borrow_mut().proxies.get_mut(&proxy_h)
                {
                    *doc = Some(doc_h);
                }
            }
            fr.borrow_mut().auxiliary_windows.push(proxy_h);

            // Per WHATWG HTML §7.1.5: `noopener` or `noreferrer` MUST return `null`!
            Ok(if suppress_opener {
                JsValue::NULL
            } else {
                JsValue::from_object(proxy_h)
            })
        });
    }

    // window.close()
    {
        let fr = Rc::clone(frames);
        define_host_method(vm, g, "close", move |_vm, _, _| {
            fr.borrow_mut().window_closed = true;
            Ok(JsValue::UNDEFINED)
        });
    }

    // window.origin (respecting opaque sandbox!)
    {
        let wa = Rc::clone(web_apis);
        let fr = Rc::clone(frames);
        define_host_getter(vm, g, "origin", move |vm, _, _| {
            let o = fr.borrow().effective_origin(&wa);
            Ok(JsValue::from_string(vm.heap.alloc_string(&o)))
        });
    }

    // window.name
    {
        let fr = Rc::clone(frames);
        define_host_accessor(
            vm,
            g,
            "name",
            {
                let fr = Rc::clone(&fr);
                move |vm, _, _| {
                    let s = fr.borrow().window_name.clone();
                    Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
                }
            },
            move |vm, _, args| {
                fr.borrow_mut().window_name = vm.value_to_rust_string(arg(args, 0));
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // window.closed, opener, parent, top, frameElement, frames, length
    {
        let fr = Rc::clone(frames);
        define_host_getter(vm, g, "closed", move |_vm, _, _| {
            Ok(JsValue::from_bool(fr.borrow().window_closed))
        });
    }
    {
        let fr = Rc::clone(frames);
        define_host_accessor(
            vm,
            g,
            "opener",
            {
                let fr = Rc::clone(&fr);
                move |_vm, _, _| Ok(fr.borrow().opener_window)
            },
            move |_vm, _, args| {
                fr.borrow_mut().opener_window = arg(args, 0);
                Ok(JsValue::UNDEFINED)
            },
        );
    }
    {
        let fr = Rc::clone(frames);
        define_host_getter(vm, g, "parent", move |_vm, _, _| {
            Ok(fr.borrow().parent_window)
        });
    }
    {
        let fr = Rc::clone(frames);
        define_host_getter(vm, g, "top", move |_vm, _, _| Ok(fr.borrow().top_window));
    }
    {
        let fr = Rc::clone(frames);
        define_host_getter(vm, g, "frameElement", move |_vm, _, _| {
            Ok(fr.borrow().frame_element)
        });
    }
    {
        let fr = Rc::clone(frames);
        define_host_getter(vm, g, "length", move |_vm, _, _| {
            Ok(JsValue::from_i32(fr.borrow().iframe_order.len() as i32))
        });
    }
    {
        let fr = Rc::clone(frames);
        define_host_getter(vm, g, "frames", move |vm, _, _| {
            let fs = fr.borrow();
            let items: Vec<JsValue> = fs
                .iframe_order
                .iter()
                .filter_map(|nid| fs.iframes.get(nid).map(|r| JsValue::from_object(r.proxy)))
                .collect();
            Ok(JsValue::from_object(alloc_array_from(vm, items)))
        });
    }
}

fn parse_window_features(raw: &str) -> (bool, bool) {
    let mut noopener = false;
    let mut noreferrer = false;
    for part in raw.split(|c: char| c == ',' || c.is_ascii_whitespace()) {
        let token = part.trim();
        if token.is_empty() {
            continue;
        }
        let (k, v) = match token.split_once('=') {
            Some((a, b)) => (
                a.trim().to_ascii_lowercase(),
                Some(b.trim().to_ascii_lowercase()),
            ),
            None => (token.to_ascii_lowercase(), None),
        };
        let enabled = match v.as_deref() {
            None => true,
            Some("yes" | "true" | "1" | "") => true,
            Some(other) => other.parse::<i32>().is_ok_and(|n| n != 0),
        };
        if k == "noopener" && enabled {
            noopener = true;
        } else if k == "noreferrer" && enabled {
            noreferrer = true;
            noopener = true;
        }
    }
    (noopener, noreferrer)
}

#[allow(clippy::too_many_arguments)]
fn install_html_iframe_element_proto(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    frames: &SharedFrames,
) {
    let ip = host.borrow().prototypes.html_iframe_element_proto;

    // Register dynamic connect hook so `document.body.appendChild(iframe)` immediately
    // loads the child browsing context and queues its async `load` event.
    // Use `Weak` references to avoid an `Rc` cycle with `HostState`.
    {
        let h_hook = Rc::downgrade(host);
        let ev_hook = Rc::downgrade(events);
        let el_hook = Rc::downgrade(event_loop);
        let wa_hook = Rc::downgrade(web_apis);
        let net_hook = Rc::downgrade(net);
        let fr_hook = Rc::downgrade(frames);
        host.borrow_mut().iframe_connect_hook = Some(Rc::new(move |vm, nid| {
            if let (Some(h), Some(ev), Some(el), Some(wa), Some(net), Some(fr)) = (
                h_hook.upgrade(),
                ev_hook.upgrade(),
                el_hook.upgrade(),
                wa_hook.upgrade(),
                net_hook.upgrade(),
                fr_hook.upgrade(),
            ) {
                sync_iframe_node(vm, &h, &ev, &el, &wa, &net, &fr, nid);
            }
        }));
    }

    // Reflected string attributes that trigger iframe re-sync on mutation (`src`, `srcdoc`).
    // Note: `HTMLIFrameElement.prototype.sandbox` is a `[PutForwards=value] DOMTokenList`
    // installed by `dom_collections::install_collections_and_traversal` (HTML §4.8.5).
    for attr_name in ["src", "srcdoc"] {
        let h_get = Rc::clone(host);
        let h_set = Rc::clone(host);
        let ev_set = Rc::clone(events);
        let el_set = Rc::clone(event_loop);
        let wa_set = Rc::clone(web_apis);
        let net_set = Rc::clone(net);
        let fr_set = Rc::clone(frames);
        let attr_g = attr_name.to_string();
        let attr_s = attr_name.to_string();
        define_host_accessor(
            vm,
            ip,
            attr_name,
            move |vm, this_val, _| {
                let Some(nid) = this_val
                    .as_object()
                    .and_then(|h| h_get.borrow().node_for_handle(vm, h))
                else {
                    return Ok(JsValue::from_string(vm.heap.alloc_string("")));
                };
                let val = {
                    let doc_rc = Rc::clone(&h_get.borrow().document);
                    let doc = doc_rc.borrow();
                    match doc.get(nid) {
                        Some(NodeData::Element(el)) => {
                            el.get_attribute(&attr_g).unwrap_or("").to_string()
                        }
                        _ => String::new(),
                    }
                };
                Ok(JsValue::from_string(vm.heap.alloc_string(&val)))
            },
            move |vm, this_val, args| {
                let Some(nid) = this_val
                    .as_object()
                    .and_then(|h| h_set.borrow().node_for_handle(vm, h))
                else {
                    return Ok(JsValue::UNDEFINED);
                };
                let val = vm.value_to_rust_string(arg(args, 0));
                set_element_attr(vm, &h_set, nid, &attr_s, &val);
                sync_iframe_node(
                    vm, &h_set, &ev_set, &el_set, &wa_set, &net_set, &fr_set, nid,
                );
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // Simple reflected attributes (`name`, `width`, `height`, `allow`, `referrerPolicy`, `loading`)
    for (prop, attr) in [
        ("name", "name"),
        ("width", "width"),
        ("height", "height"),
        ("allow", "allow"),
        ("referrerPolicy", "referrerpolicy"),
        ("loading", "loading"),
    ] {
        let h_get = Rc::clone(host);
        let h_set = Rc::clone(host);
        let attr_g = attr.to_string();
        let attr_s = attr.to_string();
        define_host_accessor(
            vm,
            ip,
            prop,
            move |vm, this_val, _| {
                let Some(nid) = this_val
                    .as_object()
                    .and_then(|h| h_get.borrow().node_for_handle(vm, h))
                else {
                    return Ok(JsValue::from_string(vm.heap.alloc_string("")));
                };
                let val = {
                    let doc_rc = Rc::clone(&h_get.borrow().document);
                    let doc = doc_rc.borrow();
                    match doc.get(nid) {
                        Some(NodeData::Element(el)) => {
                            el.get_attribute(&attr_g).unwrap_or("").to_string()
                        }
                        _ => String::new(),
                    }
                };
                Ok(JsValue::from_string(vm.heap.alloc_string(&val)))
            },
            move |vm, this_val, args| {
                let Some(nid) = this_val
                    .as_object()
                    .and_then(|h| h_set.borrow().node_for_handle(vm, h))
                else {
                    return Ok(JsValue::UNDEFINED);
                };
                let val = vm.value_to_rust_string(arg(args, 0));
                set_element_attr(vm, &h_set, nid, &attr_s, &val);
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // iframe.contentWindow
    {
        let h = Rc::clone(host);
        let ev = Rc::clone(events);
        let el = Rc::clone(event_loop);
        let wa = Rc::clone(web_apis);
        let net_r = Rc::clone(net);
        let fr = Rc::clone(frames);
        define_host_getter(vm, ip, "contentWindow", move |vm, this_val, _| {
            let Some(nid) = this_val
                .as_object()
                .and_then(|oh| h.borrow().node_for_handle(vm, oh))
            else {
                return Ok(JsValue::NULL);
            };
            sync_iframe_node(vm, &h, &ev, &el, &wa, &net_r, &fr, nid);
            Ok(fr
                .borrow()
                .iframes
                .get(&nid)
                .map_or(JsValue::NULL, |r| JsValue::from_object(r.proxy)))
        });
    }

    // iframe.contentDocument (returns child Document for same-origin, null for cross-origin/opaque!)
    {
        let h = Rc::clone(host);
        let ev = Rc::clone(events);
        let el = Rc::clone(event_loop);
        let wa = Rc::clone(web_apis);
        let net_r = Rc::clone(net);
        let fr = Rc::clone(frames);
        define_host_getter(vm, ip, "contentDocument", move |vm, this_val, _| {
            let Some(nid) = this_val
                .as_object()
                .and_then(|oh| h.borrow().node_for_handle(vm, oh))
            else {
                return Ok(JsValue::NULL);
            };
            sync_iframe_node(vm, &h, &ev, &el, &wa, &net_r, &fr, nid);
            let fs = fr.borrow();
            let Some(rec) = fs.iframes.get(&nid) else {
                return Ok(JsValue::NULL);
            };
            if rec.is_cross_origin {
                return Ok(JsValue::NULL);
            }
            match fs.proxies.get(&rec.proxy) {
                Some(WindowProxyKind::SameOriginChild { child_doc, .. }) => {
                    Ok(JsValue::from_object(*child_doc))
                }
                _ => Ok(JsValue::NULL),
            }
        });
    }
}

fn document_base_url(h: &SharedHost, wa: &SharedWebApis) -> Url {
    let fallback = wa.borrow().location_url.clone();
    let doc_rc = Rc::clone(&h.borrow().document);
    let doc = doc_rc.borrow();
    for nid in doc.descendants(doc.root()) {
        if let Some(NodeData::Element(el)) = doc.get(nid) {
            if el.name.local.as_str().eq_ignore_ascii_case("base") {
                if let Some(href) = el.get_attribute("href") {
                    if let Ok(resolved) = fallback.join(href) {
                        return resolved;
                    }
                }
            }
        }
    }
    fallback
}

fn resolve_hyperlink_url(
    vm: &JsVm,
    h: &SharedHost,
    wa: &SharedWebApis,
    this_val: JsValue,
) -> Option<(NodeId, Option<Url>, String)> {
    let obj_h = this_val.as_object()?;
    let nid = h.borrow().node_for_handle(vm, obj_h)?;
    let (raw_opt, charset) = {
        let doc_rc = Rc::clone(&h.borrow().document);
        let doc = doc_rc.borrow();
        let raw = match doc.get(nid) {
            Some(NodeData::Element(el)) => el.get_attribute("href").map(str::to_string),
            _ => None,
        };
        (raw, doc.character_set().to_string())
    };
    let raw = match raw_opt {
        Some(r) => r,
        None => return Some((nid, None, String::new())),
    };
    let base = document_base_url(h, wa);
    let parsed = parse_url_with_charset(&raw, &base, &charset);
    Some((nid, parsed, raw))
}

/// Installs WHATWG `HTMLHyperlinkElementUtils` on `HTMLAnchorElement.prototype` (`<a>`) and
/// `HTMLAreaElement.prototype` (`<area>`), plus `HTMLFormElement.prototype` (`<form>`) submission.
#[allow(clippy::too_many_arguments)]
fn install_hyperlink_and_form_protos(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    frames: &SharedFrames,
) {
    let protos = host.borrow().prototypes;

    // HTMLBaseElement (`<base>`): reflect `href` and `target` attributes (HTML §4.2.3).
    {
        let bp = protos.html_base_element_proto;
        let hg = Rc::clone(host);
        let wag = Rc::clone(web_apis);
        let hs = Rc::clone(host);
        define_host_accessor(
            vm,
            bp,
            "href",
            move |vm, this_val, _| {
                let Some(nid) = this_val
                    .as_object()
                    .and_then(|oh| hg.borrow().node_for_handle(vm, oh))
                else {
                    return Ok(JsValue::from_string(vm.heap.alloc_string("")));
                };
                let raw_opt = {
                    let doc_rc = Rc::clone(&hg.borrow().document);
                    let doc = doc_rc.borrow();
                    match doc.get(nid) {
                        Some(NodeData::Element(el)) => el.get_attribute("href").map(str::to_string),
                        _ => None,
                    }
                };
                let fallback = wag.borrow().location_url.clone();
                let out = match raw_opt {
                    Some(r) => fallback.join(&r).map(|u| u.to_string()).unwrap_or(r),
                    None => fallback.to_string(),
                };
                Ok(JsValue::from_string(vm.heap.alloc_string(&out)))
            },
            move |vm, this_val, args| {
                if let Some(nid) = this_val
                    .as_object()
                    .and_then(|oh| hs.borrow().node_for_handle(vm, oh))
                {
                    let val = vm.value_to_rust_string(arg(args, 0));
                    set_element_attr(vm, &hs, nid, "href", &val);
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    for proto in [
        protos.html_anchor_element_proto,
        protos.html_area_element_proto,
    ] {
        // href (getter + setter)
        {
            let hg = Rc::clone(host);
            let wag = Rc::clone(web_apis);
            let hs = Rc::clone(host);
            define_host_accessor(
                vm,
                proto,
                "href",
                move |vm, this_val, _| {
                    let Some((_, parsed, raw)) = resolve_hyperlink_url(vm, &hg, &wag, this_val)
                    else {
                        return Ok(JsValue::from_string(vm.heap.alloc_string("")));
                    };
                    let out = parsed.map_or(raw, |u| u.as_str().to_string());
                    Ok(JsValue::from_string(vm.heap.alloc_string(&out)))
                },
                move |vm, this_val, args| {
                    if let Some(nid) = this_val
                        .as_object()
                        .and_then(|oh| hs.borrow().node_for_handle(vm, oh))
                    {
                        let val = vm.value_to_rust_string(arg(args, 0));
                        set_element_attr(vm, &hs, nid, "href", &val);
                    }
                    Ok(JsValue::UNDEFINED)
                },
            );
        }

        // toString() mirrors href
        {
            let hg = Rc::clone(host);
            let wag = Rc::clone(web_apis);
            define_host_method(vm, proto, "toString", move |vm, this_val, _| {
                let Some((_, parsed, raw)) = resolve_hyperlink_url(vm, &hg, &wag, this_val) else {
                    return Ok(JsValue::from_string(vm.heap.alloc_string("")));
                };
                let out = parsed.map_or(raw, |u| u.as_str().to_string());
                Ok(JsValue::from_string(vm.heap.alloc_string(&out)))
            });
        }

        // origin (readonly)
        {
            let hg = Rc::clone(host);
            let wag = Rc::clone(web_apis);
            define_host_getter(vm, proto, "origin", move |vm, this_val, _| {
                let out = resolve_hyperlink_url(vm, &hg, &wag, this_val)
                    .and_then(|(_, parsed, _)| parsed)
                    .map(|u| origin_of_url(&u))
                    .unwrap_or_default();
                Ok(JsValue::from_string(vm.heap.alloc_string(&out)))
            });
        }

        // Mutable URL component accessors (`protocol`, `username`, `password`, `host`, `hostname`, `port`, `pathname`, `search`, `hash`)
        for (prop, kind) in [
            ("protocol", 0u8),
            ("username", 1),
            ("password", 2),
            ("host", 3),
            ("hostname", 4),
            ("port", 5),
            ("pathname", 6),
            ("search", 7),
            ("hash", 8),
        ] {
            let hg = Rc::clone(host);
            let wag = Rc::clone(web_apis);
            let hs = Rc::clone(host);
            let was = Rc::clone(web_apis);
            define_host_accessor(
                vm,
                proto,
                prop,
                move |vm, this_val, _| {
                    let out = resolve_hyperlink_url(vm, &hg, &wag, this_val)
                        .and_then(|(_, parsed, _)| parsed)
                        .map(|u| match kind {
                            0 => format!("{}:", u.scheme()),
                            1 => u.username().to_string(),
                            2 => u.password().unwrap_or("").to_string(),
                            3 => match (u.host_str(), u.port()) {
                                (Some(h), Some(p)) => format!("{h}:{p}"),
                                (Some(h), None) => h.to_string(),
                                _ => String::new(),
                            },
                            4 => u.host_str().unwrap_or("").to_string(),
                            5 => u.port().map(|p| p.to_string()).unwrap_or_default(),
                            6 => u.path().to_string(),
                            7 => u
                                .query()
                                .filter(|q| !q.is_empty())
                                .map(|q| format!("?{q}"))
                                .unwrap_or_default(),
                            _ => u
                                .fragment()
                                .filter(|f| !f.is_empty())
                                .map(|f| format!("#{f}"))
                                .unwrap_or_default(),
                        })
                        .unwrap_or_else(|| {
                            if kind == 0 {
                                ":".to_string()
                            } else {
                                String::new()
                            }
                        });
                    Ok(JsValue::from_string(vm.heap.alloc_string(&out)))
                },
                move |vm, this_val, args| {
                    let Some((nid, Some(mut u), _)) =
                        resolve_hyperlink_url(vm, &hs, &was, this_val)
                    else {
                        return Ok(JsValue::UNDEFINED);
                    };
                    let val = vm.value_to_rust_string(arg(args, 0));
                    match kind {
                        0 => {
                            let cleaned = strip_ascii_tab_lf(&val);
                            let scheme = cleaned.split(':').next().unwrap_or("");
                            if !scheme.is_empty() {
                                let _ = u.set_scheme(scheme);
                            }
                        }
                        1 => {
                            if !u.cannot_be_a_base()
                                && u.host_str().is_some()
                                && u.scheme() != "file"
                            {
                                let _ = u.set_username(&val);
                            }
                        }
                        2 => {
                            if !u.cannot_be_a_base()
                                && u.host_str().is_some()
                                && u.scheme() != "file"
                            {
                                let _ =
                                    u.set_password(if val.is_empty() { None } else { Some(&val) });
                            }
                        }
                        3 => apply_url_host_setter(&mut u, &val, true),
                        4 => apply_url_host_setter(&mut u, &val, false),
                        5 => apply_url_port_setter(&mut u, &val),
                        6 => {
                            if !u.cannot_be_a_base() {
                                let cleaned = strip_ascii_tab_lf(&val);
                                u.set_path(&cleaned);
                            }
                        }
                        7 => {
                            let trimmed = val.strip_prefix('?').unwrap_or(&val);
                            let cleaned = strip_ascii_tab_lf(trimmed);
                            if val.is_empty() {
                                u.set_query(None);
                            } else if trimmed.is_empty() {
                                u.set_query(Some(""));
                            } else {
                                u.set_query(Some(&cleaned));
                            }
                        }
                        _ => {
                            let trimmed = val.strip_prefix('#').unwrap_or(&val);
                            let cleaned = strip_ascii_tab_lf(trimmed);
                            if val.is_empty() {
                                u.set_fragment(None);
                            } else {
                                u.set_fragment(Some(&cleaned));
                            }
                        }
                    }
                    set_element_attr(vm, &hs, nid, "href", u.as_str());
                    Ok(JsValue::UNDEFINED)
                },
            );
        }
    }

    // --- HTMLFormElement (`<form>`) ---
    let fp = protos.html_form_element_proto;
    for (prop, attr) in [
        ("action", "action"),
        ("method", "method"),
        ("target", "target"),
        ("acceptCharset", "accept-charset"),
        ("enctype", "enctype"),
        ("name", "name"),
    ] {
        let hg = Rc::clone(host);
        let hs = Rc::clone(host);
        let attr_g = attr.to_string();
        let attr_s = attr.to_string();
        define_host_accessor(
            vm,
            fp,
            prop,
            move |vm, this_val, _| {
                let Some(nid) = this_val
                    .as_object()
                    .and_then(|oh| hg.borrow().node_for_handle(vm, oh))
                else {
                    return Ok(JsValue::from_string(vm.heap.alloc_string("")));
                };
                let val = {
                    let doc_rc = Rc::clone(&hg.borrow().document);
                    let doc = doc_rc.borrow();
                    match doc.get(nid) {
                        Some(NodeData::Element(el)) => {
                            el.get_attribute(&attr_g).unwrap_or("").to_string()
                        }
                        _ => String::new(),
                    }
                };
                Ok(JsValue::from_string(vm.heap.alloc_string(&val)))
            },
            move |vm, this_val, args| {
                if let Some(nid) = this_val
                    .as_object()
                    .and_then(|oh| hs.borrow().node_for_handle(vm, oh))
                {
                    let val = vm.value_to_rust_string(arg(args, 0));
                    set_element_attr(vm, &hs, nid, &attr_s, &val);
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // form.submit(): WHATWG HTML §4.10.21.3 GET/target-iframe form submission with charset encoding
    {
        let h = Rc::clone(host);
        let ev = Rc::clone(events);
        let el = Rc::clone(event_loop);
        let wa = Rc::clone(web_apis);
        let net_r = Rc::clone(net);
        let fr = Rc::clone(frames);
        define_host_method(vm, fp, "submit", move |vm, this_val, _| {
            let Some(form_nid) = this_val
                .as_object()
                .and_then(|oh| h.borrow().node_for_handle(vm, oh))
            else {
                return Ok(JsValue::UNDEFINED);
            };
            let (action, method, target, charset, pairs, target_iframe) = {
                let doc_rc = Rc::clone(&h.borrow().document);
                let doc = doc_rc.borrow();
                let Some(NodeData::Element(form_el)) = doc.get(form_nid) else {
                    return Ok(JsValue::UNDEFINED);
                };
                let action = form_el.get_attribute("action").unwrap_or("").to_string();
                let method = form_el
                    .get_attribute("method")
                    .unwrap_or("get")
                    .to_ascii_lowercase();
                let target = form_el.get_attribute("target").unwrap_or("").to_string();
                let accept_cs = form_el
                    .get_attribute("accept-charset")
                    .and_then(|s| s.split_ascii_whitespace().next())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| doc.character_set().to_string());
                let mut pairs = Vec::new();
                for desc in doc.descendants(form_nid) {
                    if let Some(NodeData::Element(ctrl)) = doc.get(desc) {
                        let tag = ctrl.name.local.as_str();
                        if matches!(tag, "input" | "textarea" | "select")
                            && !ctrl.has_attribute("disabled")
                        {
                            if let Some(name) = ctrl.get_attribute("name").filter(|n| !n.is_empty())
                            {
                                let val = ctrl.get_attribute("value").unwrap_or("").to_string();
                                pairs.push((name.to_string(), val));
                            }
                        }
                    }
                }
                let target_iframe = if !target.is_empty() {
                    doc.descendants(doc.root()).find(|&n| {
                        matches!(doc.get(n), Some(NodeData::Element(e))
                            if e.name.local.as_str().eq_ignore_ascii_case("iframe")
                                && (e.get_attribute("name") == Some(target.as_str())
                                    || e.get_attribute("id") == Some(target.as_str())))
                    })
                } else {
                    None
                };
                (action, method, target, accept_cs, pairs, target_iframe)
            };
            let _ = target;
            if method == "get" || method.is_empty() {
                let encoded = form_urlencode_pairs_with_charset(&pairs, &charset);
                let base = wa.borrow().location_url.clone();
                let mut dest = if action.is_empty() {
                    base
                } else {
                    base.join(&action).unwrap_or(base)
                };
                dest.set_query(if encoded.is_empty() {
                    None
                } else {
                    Some(&encoded)
                });
                let dest_str = dest.as_str().to_string();
                if let Some(iframe_nid) = target_iframe {
                    // Force re-navigation even if src matches previous submission.
                    if let Some(rec) = fr.borrow_mut().iframes.get_mut(&iframe_nid) {
                        rec.last_src.clear();
                    }
                    set_element_attr(vm, &h, iframe_nid, "src", &dest_str);
                    sync_iframe_node(vm, &h, &ev, &el, &wa, &net_r, &fr, iframe_nid);
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }
}
