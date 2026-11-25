//! Cross-thread WHATWG HTML §2.7 StructuredSerialize / StructuredDeserialize
//! and Transferable codec (`StructuredClonePayload`) plus zero-polling
//! cross-thread `MessagePort` routing (`PortRoute`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use rix_ipc::MessagePortId;
use rix_js::heap::{DataViewObject, SharedBufferBacking, TypedArrayKind, TypedArrayObject};
use rix_js::{
    HeapHandle, JsBigInt, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs,
};
use tokio::sync::mpsc;

use crate::canvas2d::{
    create_image_bitmap_obj, create_image_data_obj, create_offscreen_canvas_obj, SharedCanvas2D,
};
use crate::dom_bridge::SharedHost;
use crate::events::SharedEvents;
use crate::fetch_ws::SharedNetBridge;
use crate::frames::SharedFrames;
use crate::web_apis::{
    create_blob_object, create_dom_exception, create_file_list_object, create_file_object,
    BlobEntry, SharedWebApis,
};
use crate::workers::{AsyncWorkerEvent, SharedWorkers};

/// Live cross-thread endpoint for a transferred `MessagePort`.
#[derive(Debug)]
pub(crate) enum PortRoute {
    /// Messages queued while the port is in flight between threads.
    Buffered(VecDeque<StructuredClonePayload>),
    /// Live attachment straight into the owning runtime's reactive async channel.
    Live {
        tx: mpsc::UnboundedSender<AsyncWorkerEvent>,
        port_id: MessagePortId,
    },
    /// Port has been closed or its owning runtime dropped.
    Closed,
}

impl PortRoute {
    /// Sends a serialized payload into this route (delivering immediately if `Live`
    /// or queueing in `Buffered` if the receiving thread has not attached yet).
    pub(crate) fn send_payload(route: &Arc<Mutex<Self>>, payload: StructuredClonePayload) {
        if let Ok(mut guard) = route.lock() {
            match &mut *guard {
                Self::Buffered(q) => q.push_back(payload),
                Self::Live { tx, port_id } => {
                    let _ = tx.send(AsyncWorkerEvent::BridgePortMessage {
                        port_id: *port_id,
                        payload,
                    });
                }
                Self::Closed => {}
            }
        }
    }

    /// Marks this route closed so subsequent messages are dropped cleanly.
    pub(crate) fn close(route: &Arc<Mutex<Self>>) {
        if let Ok(mut guard) = route.lock() {
            *guard = Self::Closed;
        }
    }
}

/// Pair of shared routes transferred across threads for one `MessagePort`.
#[derive(Debug, Clone)]
pub(crate) struct CrossThreadPortEnd {
    /// Route carrying inbound messages TO this transferred port.
    pub(crate) local_route: Arc<Mutex<PortRoute>>,
    /// Route carrying outbound messages FROM this transferred port to its partner.
    pub(crate) remote_route: Arc<Mutex<PortRoute>>,
}

/// Complete cross-thread `Send` structured-clone payload preserving object graph
/// identity, cycles, typed arrays, resizable buffers, shared memory, Blobs/Files,
/// ImageData/ImageBitmap/OffscreenCanvas, Errors, and transferred MessagePorts.
#[derive(Debug, Clone)]
pub struct StructuredClonePayload {
    pub(crate) root: CloneValue,
    pub(crate) nodes: Vec<CloneNode>,
    pub(crate) transferred_ports: Vec<CrossThreadPortEnd>,
    pub(crate) has_transferred_media_handle: bool,
}

impl StructuredClonePayload {
    #[allow(dead_code)]
    #[must_use]
    pub(crate) fn undefined() -> Self {
        Self {
            root: CloneValue::Undefined,
            nodes: Vec::new(),
            transferred_ports: Vec::new(),
            has_transferred_media_handle: false,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum CloneValue {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    BigInt(String),
    Node(usize),
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ErrorCloneKind {
    Error,
    EvalError,
    RangeError,
    ReferenceError,
    SyntaxError,
    TypeError,
    URIError,
    DOMException,
}

#[derive(Debug, Clone)]
pub(crate) enum CloneNode {
    Placeholder,
    BoxedBool(bool),
    BoxedNumber(f64),
    BoxedString(String),
    BoxedBigInt(String),
    Date(f64),
    RegExp {
        pattern: String,
        flags: String,
    },
    Error {
        kind: ErrorCloneKind,
        name: Option<String>,
        message: Option<String>,
        cause: Option<CloneValue>,
        stack: Option<String>,
    },
    ArrayBuffer {
        bytes: Vec<u8>,
        max_byte_length: Option<usize>,
    },
    SharedArrayBuffer {
        backing: Arc<SharedBufferBacking>,
    },
    TypedArray {
        kind: TypedArrayKind,
        buffer_node: usize,
        byte_offset: u32,
        length: u32,
        auto_length: bool,
    },
    DataView {
        buffer_node: usize,
        byte_offset: u32,
        byte_length: u32,
        auto_length: bool,
    },
    Array {
        length: u32,
        indexed: Vec<(u32, CloneValue)>,
        extra_props: Vec<(String, CloneValue)>,
    },
    Object {
        props: Vec<(String, CloneValue)>,
    },
    Map {
        entries: Vec<(CloneValue, CloneValue)>,
    },
    Set {
        items: Vec<CloneValue>,
    },
    Blob {
        bytes: Vec<u8>,
        content_type: String,
    },
    File {
        bytes: Vec<u8>,
        content_type: String,
        name: String,
        last_modified: f64,
    },
    FileList {
        files: Vec<(Vec<u8>, String, String, f64)>,
    },
    ImageData {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    ImageBitmap {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    OffscreenCanvas {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    TransferredPort {
        port_index: usize,
    },
    MediaSourceHandle {
        bridge_id: u64,
    },
    DomPoint {
        readonly: bool,
        x: f64,
        y: f64,
        z: f64,
        w: f64,
    },
    DomRect {
        readonly: bool,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    },
    DomMatrix {
        readonly: bool,
        m: [f64; 16],
    },
}

/// Runtime context needed during structured serialization and deserialization.
pub(crate) struct CloneContext<'a> {
    pub(crate) host: &'a SharedHost,
    pub(crate) events: &'a SharedEvents,
    pub(crate) web_apis: &'a SharedWebApis,
    pub(crate) net: &'a SharedNetBridge,
    pub(crate) canvas2d: &'a SharedCanvas2D,
    pub(crate) frames: &'a SharedFrames,
    pub(crate) workers: &'a SharedWorkers,
}

enum CrossTransferItem {
    ArrayBuffer(HeapHandle),
    Port(HeapHandle),
    ImageBitmap(HeapHandle),
    OffscreenCanvas(HeapHandle),
    MediaSourceHandle(HeapHandle),
}

/// Parses a WHATWG HTML `postMessage(message, transferOrOptions)` second argument
/// into its raw `transfer` JS value (`undefined` if absent).
pub(crate) fn extract_transfer_arg(vm: &mut JsVm, second: JsValue) -> Result<JsValue, JsException> {
    if second.is_undefined() || second.is_null() {
        return Ok(JsValue::UNDEFINED);
    }
    let Some(oh) = second.as_object() else {
        return Err(vm.type_error("postMessage options/transfer must be an object or iterable"));
    };
    let is_arr = vm
        .heap
        .get_object(oh)
        .is_some_and(|o| matches!(o.kind, ObjectKind::Array));
    if is_arr {
        return Ok(second);
    }
    // Check if iterable (sequence<object>) first; otherwise treat as StructuredSerializeOptions `{ transfer }`.
    let sym_iter = vm.key_symbol_iterator;
    let is_iterable = vm
        .get_property_by_key(oh, sym_iter)?
        .as_object()
        .is_some_and(|f| vm.is_callable_handle(f));
    if is_iterable {
        return Ok(second);
    }
    let k_tr = vm.heap.intern_key("transfer");
    vm.get_property_by_key(oh, k_tr)
}

/// Performs WHATWG HTML §2.7.5 `StructuredSerializeWithTransfer` into a `Send`
/// [`StructuredClonePayload`] suitable for cross-thread delivery to/from workers.
pub(crate) fn serialize_with_transfer(
    vm: &mut JsVm,
    ctx: &CloneContext<'_>,
    value: JsValue,
    transfer_val: JsValue,
    disallowed_ports: &[HeapHandle],
) -> Result<StructuredClonePayload, JsException> {
    // 1. Validate transfer sequence per WebIDL `sequence<object>` + HTML §2.7.5.
    let mut transfer_items: Vec<CrossTransferItem> = Vec::new();
    let mut transfer_set: HashSet<HeapHandle> = HashSet::new();
    if !transfer_val.is_undefined() {
        let Some(seq_h) = transfer_val.as_object() else {
            return Err(vm.type_error("transfer list must be an iterable sequence"));
        };
        let sym_iter = vm.key_symbol_iterator;
        let has_iter = vm
            .get_property_by_key(seq_h, sym_iter)?
            .as_object()
            .is_some_and(|f| vm.is_callable_handle(f));
        if !has_iter {
            return Err(vm.type_error("transfer list must be an iterable sequence"));
        }
        let raw_items = vm.collect_iterable(transfer_val)?;
        for item in raw_items {
            // WebIDL `sequence<object>`: null / undefined / primitives throw `TypeError`!
            let Some(h) = item.as_object() else {
                return Err(vm.type_error("Value in transfer list is not an object"));
            };
            if !transfer_set.insert(h) {
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
                    "Cannot transfer source MessagePort",
                ));
            }
            if ctx.frames.borrow().is_message_port(h) {
                if ctx.frames.borrow().is_port_detached_or_closed(h) {
                    return Err(create_dom_exception(
                        vm,
                        "DataCloneError",
                        "Cannot transfer a closed or already-neutered MessagePort",
                    ));
                }
                transfer_items.push(CrossTransferItem::Port(h));
                continue;
            }
            if let Some(o) = vm.heap.get_object(h) {
                if matches!(o.kind, ObjectKind::ArrayBuffer(_)) {
                    let bad = o.rare.as_deref().is_some_and(|r| {
                        r.ab_detached
                            || r.ab_immutable
                            || r.ab_wasm_memory
                            || r.shared_mem.is_some()
                    }) || ctx.frames.borrow().is_buffer_detached(h);
                    if bad {
                        return Err(create_dom_exception(
                            vm,
                            "DataCloneError",
                            "ArrayBuffer cannot be transferred",
                        ));
                    }
                    transfer_items.push(CrossTransferItem::ArrayBuffer(h));
                    continue;
                }
            }
            if let Some(ib) = ctx.canvas2d.borrow().image_bitmaps.get(&h) {
                if ib.closed {
                    return Err(create_dom_exception(
                        vm,
                        "DataCloneError",
                        "Cannot transfer a detached ImageBitmap",
                    ));
                }
                transfer_items.push(CrossTransferItem::ImageBitmap(h));
                continue;
            }
            if let Some(oc) = ctx.canvas2d.borrow().offscreen_canvases.get(&h) {
                if oc.detached {
                    return Err(create_dom_exception(
                        vm,
                        "DataCloneError",
                        "Cannot transfer a detached OffscreenCanvas",
                    ));
                }
                transfer_items.push(CrossTransferItem::OffscreenCanvas(h));
                continue;
            }
            if let Some(ms) = ctx.frames.borrow().media_source.clone() {
                if ms.borrow().is_media_source_handle(h) {
                    if !ms.borrow().can_transfer_media_source_handle(h) {
                        return Err(create_dom_exception(
                            vm,
                            "DataCloneError",
                            "MediaSourceHandle is detached or has already been attached to a media element",
                        ));
                    }
                    transfer_items.push(CrossTransferItem::MediaSourceHandle(h));
                    continue;
                }
            }
            return Err(create_dom_exception(
                vm,
                "DataCloneError",
                "Object in transfer list is not transferable",
            ));
        }
    }

    // 2. Serialize value graph FIRST (before detaching transferables, so OOB/error checks
    //    see live buffers and if serialization throws, nothing was half-mutated).
    let mut nodes: Vec<CloneNode> = Vec::new();
    let mut memo: HashMap<HeapHandle, usize> = HashMap::new();
    let mut transferred_ports: Vec<CrossThreadPortEnd> = Vec::new();

    // Pre-reserve node slots for transferred ports so references inside `value` resolve to them.
    let mut port_handle_to_idx: HashMap<HeapHandle, usize> = HashMap::new();
    let mut next_port_idx = 0usize;
    for item in &transfer_items {
        if let CrossTransferItem::Port(ph) = *item {
            let node_idx = nodes.len();
            nodes.push(CloneNode::TransferredPort {
                port_index: next_port_idx,
            });
            memo.insert(ph, node_idx);
            port_handle_to_idx.insert(ph, next_port_idx);
            next_port_idx += 1;
        }
    }

    let root = serialize_value(vm, ctx, value, &mut nodes, &mut memo, &transfer_set, false)?;

    // 3. Commit transfers (detach ArrayBuffers, ImageBitmaps, OffscreenCanvases, MessagePorts, and MediaSourceHandles).
    let local_async_tx = ctx.workers.borrow().async_tx_clone();
    let mut has_transferred_media_handle = false;
    for item in transfer_items {
        match item {
            CrossTransferItem::ArrayBuffer(ab_h) => {
                // Capture buffer bytes/max_len if not already serialized as a node.
                if let Some(o) = vm.heap.get_object_mut(ab_h) {
                    if let ObjectKind::ArrayBuffer(buf) = &mut o.kind {
                        buf.clear();
                    }
                    o.rare_mut().ab_detached = true;
                }
                ctx.frames.borrow_mut().mark_buffer_detached(ab_h);
            }
            CrossTransferItem::ImageBitmap(ib_h) => {
                if let Some(entry) = ctx.canvas2d.borrow_mut().image_bitmaps.get_mut(&ib_h) {
                    entry.closed = true;
                    entry.rgba.clear();
                }
            }
            CrossTransferItem::OffscreenCanvas(oc_h) => {
                if let Some(entry) = ctx.canvas2d.borrow_mut().offscreen_canvases.get_mut(&oc_h) {
                    entry.detached = true;
                    entry.rgba.clear();
                }
            }
            CrossTransferItem::MediaSourceHandle(hh) => {
                if let Some(ms) = ctx.frames.borrow().media_source.clone() {
                    let _ = ms.borrow_mut().take_media_source_handle_for_transfer(hh);
                    has_transferred_media_handle = true;
                }
            }
            CrossTransferItem::Port(port_h) => {
                let end = crate::frames::detach_port_for_cross_thread_transfer(
                    vm,
                    ctx,
                    port_h,
                    local_async_tx.clone(),
                )?;
                let expected = port_handle_to_idx[&port_h];
                debug_assert_eq!(transferred_ports.len(), expected);
                transferred_ports.push(end);
            }
        }
    }

    Ok(StructuredClonePayload {
        root,
        nodes,
        transferred_ports,
        has_transferred_media_handle,
    })
}

/// Performs WHATWG HTML §2.7.4 `StructuredSerializeForStorage` (rejecting
/// `SharedArrayBuffer`, `MessagePort`, `ImageBitmap`, `OffscreenCanvas`, functions,
/// symbols, and non-serializable platform objects with `DataCloneError`).
pub(crate) fn serialize_for_storage(
    vm: &mut JsVm,
    ctx: &CloneContext<'_>,
    value: JsValue,
) -> Result<StructuredClonePayload, JsException> {
    let mut nodes: Vec<CloneNode> = Vec::new();
    let mut memo: HashMap<HeapHandle, usize> = HashMap::new();
    let transfer_set: HashSet<HeapHandle> = HashSet::new();
    let root = serialize_value(vm, ctx, value, &mut nodes, &mut memo, &transfer_set, true)?;
    Ok(StructuredClonePayload {
        root,
        nodes,
        transferred_ports: Vec::new(),
        has_transferred_media_handle: false,
    })
}

/// Deserializes a storage [`StructuredClonePayload`] back into fresh JS values on `vm`,
/// bypassing `Object.prototype` and `Array.prototype` setters per WebIDL / IndexedDB spec.
pub(crate) fn deserialize_for_storage(
    vm: &mut JsVm,
    ctx: &CloneContext<'_>,
    payload: &StructuredClonePayload,
) -> JsValue {
    deserialize_payload_ref(vm, ctx, payload, &[])
}

fn is_non_serializable_platform_object(vm: &JsVm, ctx: &CloneContext<'_>, h: HeapHandle) -> bool {
    if ctx.host.borrow().node_for_handle(vm, h).is_some() {
        return true;
    }
    if h == vm.global {
        return true;
    }
    {
        let wa = ctx.web_apis.borrow();
        if wa.url_objects_contains(h)
            || wa.search_params.contains_key(&h)
            || wa.text_decoders_contains(h)
            || wa.form_data.contains_key(&h)
            || wa.abort_signals.contains_key(&h)
        {
            return true;
        }
    }
    if ctx.net.borrow().is_non_serializable_net_object(h) {
        return true;
    }
    if ctx.events.borrow().is_event_object(h) {
        return true;
    }
    if ctx.frames.borrow().is_message_port(h) || ctx.frames.borrow().is_window_proxy(h) {
        return true;
    }
    if ctx.workers.borrow().is_worker_platform_object(h) {
        return true;
    }
    // Check EventTarget inheritance (rejects EventTarget, WebSocket, XMLHttpRequest, etc.).
    let etp = ctx.host.borrow().prototypes.event_target_proto;
    let mut cur = vm.heap.get_object(h).and_then(|o| o.prototype);
    let mut hops = 0u8;
    while let Some(p) = cur {
        if p == etp {
            return true;
        }
        cur = vm.heap.get_object(p).and_then(|o| o.prototype);
        hops += 1;
        if hops > 32 {
            break;
        }
    }
    false
}

fn classify_error_kind(
    vm: &mut JsVm,
    ctx: &CloneContext<'_>,
    h: HeapHandle,
) -> Option<ErrorCloneKind> {
    let kb = vm.heap.intern_key("__rix_dom_exception__");
    if vm.heap.object_get_own(h, kb, &vm.shapes).is_some() {
        return Some(ErrorCloneKind::DOMException);
    }
    let has_err_brand = vm
        .heap
        .get_object(h)
        .and_then(|o| o.rare.as_deref())
        .is_some_and(|r| r.is_error_data);
    let de_proto = ctx.web_apis.borrow().protos.dom_exception_proto;
    let mut cur = vm.heap.get_object(h).and_then(|o| o.prototype);
    let mut hops = 0u8;
    while let Some(p) = cur {
        if Some(p) == de_proto {
            return Some(ErrorCloneKind::DOMException);
        }
        if p == vm.type_error_proto {
            return Some(ErrorCloneKind::TypeError);
        }
        if p == vm.reference_error_proto {
            return Some(ErrorCloneKind::ReferenceError);
        }
        if p == vm.range_error_proto {
            return Some(ErrorCloneKind::RangeError);
        }
        if p == vm.syntax_error_proto {
            return Some(ErrorCloneKind::SyntaxError);
        }
        if p == vm.uri_error_proto {
            return Some(ErrorCloneKind::URIError);
        }
        if p == vm.eval_error_proto {
            return Some(ErrorCloneKind::EvalError);
        }
        if p == vm.error_proto {
            return Some(ErrorCloneKind::Error);
        }
        cur = vm.heap.get_object(p).and_then(|o| o.prototype);
        hops += 1;
        if hops > 32 {
            break;
        }
    }
    if has_err_brand {
        return Some(ErrorCloneKind::Error);
    }
    None
}

fn serialize_value(
    vm: &mut JsVm,
    ctx: &CloneContext<'_>,
    val: JsValue,
    nodes: &mut Vec<CloneNode>,
    memo: &mut HashMap<HeapHandle, usize>,
    transfer_set: &HashSet<HeapHandle>,
    for_storage: bool,
) -> Result<CloneValue, JsException> {
    if val.is_undefined() {
        return Ok(CloneValue::Undefined);
    }
    if val.is_null() {
        return Ok(CloneValue::Null);
    }
    if let Some(b) = val.as_bool() {
        return Ok(CloneValue::Bool(b));
    }
    if let Some(n) = val.as_number() {
        return Ok(CloneValue::Number(n));
    }
    if val.is_string() {
        return Ok(CloneValue::String(vm.value_to_rust_string(val)));
    }
    if let Some(bh) = val.as_bigint() {
        let s = vm
            .heap
            .get_bigint(bh)
            .map_or_else(|| "0".to_string(), JsBigInt::to_decimal_string);
        return Ok(CloneValue::BigInt(s));
    }
    if val.is_symbol() {
        return Err(create_dom_exception(
            vm,
            "DataCloneError",
            "Symbol could not be cloned",
        ));
    }
    let Some(h) = val.as_object() else {
        return Ok(CloneValue::Null);
    };
    if let Some(&idx) = memo.get(&h) {
        return Ok(CloneValue::Node(idx));
    }
    if vm.is_callable_handle(h) {
        return Err(create_dom_exception(
            vm,
            "DataCloneError",
            "Function could not be cloned",
        ));
    }

    // 1. Blob / File
    if let Some(blob) = ctx.web_apis.borrow().blobs.get(&h).cloned() {
        let idx = nodes.len();
        let node = if let Some(name) = blob.name {
            CloneNode::File {
                bytes: blob.bytes,
                content_type: blob.content_type,
                name,
                last_modified: blob.last_modified.unwrap_or(0.0),
            }
        } else {
            CloneNode::Blob {
                bytes: blob.bytes,
                content_type: blob.content_type,
            }
        };
        nodes.push(node);
        memo.insert(h, idx);
        return Ok(CloneValue::Node(idx));
    }

    // 2. FileList
    if let Some(files) = ctx.web_apis.borrow().file_lists.get(&h).cloned() {
        let idx = nodes.len();
        let items = files
            .into_iter()
            .map(|f| {
                (
                    f.bytes,
                    f.content_type,
                    f.name.unwrap_or_default(),
                    f.last_modified.unwrap_or(0.0),
                )
            })
            .collect();
        nodes.push(CloneNode::FileList { files: items });
        memo.insert(h, idx);
        return Ok(CloneValue::Node(idx));
    }

    // 3. ImageData
    if let Some((w, h_px, data_h)) = ctx.canvas2d.borrow().image_data.get(&h).copied() {
        let rgba = match vm.heap.get_object(data_h).map(|o| &o.kind) {
            Some(ObjectKind::TypedArray(ta)) => {
                let ta = *ta;
                match vm.heap.get_object(ta.buffer).map(|o| &o.kind) {
                    Some(ObjectKind::ArrayBuffer(b)) => {
                        let off = ta.byte_offset as usize;
                        let end = (off + ta.length as usize).min(b.len());
                        b[off.min(end)..end].to_vec()
                    }
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        };
        let idx = nodes.len();
        nodes.push(CloneNode::ImageData {
            width: w,
            height: h_px,
            rgba,
        });
        memo.insert(h, idx);
        return Ok(CloneValue::Node(idx));
    }

    // 4. ImageBitmap (NOT serializable for storage per HTML §2.7.4)
    if let Some(ib) = ctx.canvas2d.borrow().image_bitmaps.get(&h).cloned() {
        if for_storage {
            return Err(create_dom_exception(
                vm,
                "DataCloneError",
                "ImageBitmap cannot be serialized for storage",
            ));
        }
        if ib.closed {
            return Err(create_dom_exception(
                vm,
                "DataCloneError",
                "Cannot clone a detached ImageBitmap",
            ));
        }
        let idx = nodes.len();
        nodes.push(CloneNode::ImageBitmap {
            width: ib.width,
            height: ib.height,
            rgba: ib.rgba,
        });
        memo.insert(h, idx);
        return Ok(CloneValue::Node(idx));
    }

    // 5. OffscreenCanvas (only transferable, not cloneable unless in transfer_set!)
    if let Some(oc) = ctx.canvas2d.borrow().offscreen_canvases.get(&h).cloned() {
        if !transfer_set.contains(&h) || oc.detached {
            return Err(create_dom_exception(
                vm,
                "DataCloneError",
                "OffscreenCanvas must be transferred",
            ));
        }
        let idx = nodes.len();
        nodes.push(CloneNode::OffscreenCanvas {
            width: oc.width,
            height: oc.height,
            rgba: oc.rgba,
        });
        memo.insert(h, idx);
        return Ok(CloneValue::Node(idx));
    }

    // 5a. MediaSourceHandle (only transferable, never cloneable or storable!)
    if let Some(ms) = ctx.frames.borrow().media_source.clone() {
        if let Some(&hst) = ms.borrow().handles.get(&h) {
            if for_storage || !transfer_set.contains(&h) || hst.detached || hst.has_been_attached {
                return Err(create_dom_exception(
                    vm,
                    "DataCloneError",
                    "MediaSourceHandle cannot be cloned; it must be transferred while attached to neither another transfer nor a media element",
                ));
            }
            let idx = nodes.len();
            nodes.push(CloneNode::MediaSourceHandle {
                bridge_id: hst.bridge_id,
            });
            memo.insert(h, idx);
            return Ok(CloneValue::Node(idx));
        }
    }

    // 5b. Geometry Interfaces Level 1 ([Serializable]: DOMPoint*, DOMRect*, DOMMatrix*)
    {
        let obj_proto = vm.heap.get_object(h).and_then(|o| o.prototype);
        let geom_match = {
            let st = ctx.host.borrow();
            let gs = st.cssom_geometry_state.borrow();
            if obj_proto.is_some() && obj_proto == gs.dom_point_proto {
                Some(1u8)
            } else if obj_proto.is_some() && obj_proto == gs.dom_point_readonly_proto {
                Some(2)
            } else if obj_proto.is_some() && obj_proto == gs.dom_rect_proto {
                Some(3)
            } else if obj_proto.is_some() && obj_proto == gs.dom_rect_readonly_proto {
                Some(4)
            } else if obj_proto.is_some() && obj_proto == gs.dom_matrix_proto {
                Some(5)
            } else if obj_proto.is_some() && obj_proto == gs.dom_matrix_readonly_proto {
                Some(6)
            } else {
                None
            }
        };
        if let Some(tag) = geom_match {
            let idx = nodes.len();
            let read_num = |vm: &mut JsVm, kname: &str, def: f64| -> f64 {
                let k = vm.heap.intern_key(kname);
                vm.heap
                    .object_get_own(h, k, &vm.shapes)
                    .and_then(|p| p.value.as_number())
                    .unwrap_or(def)
            };
            let node = match tag {
                1 | 2 => CloneNode::DomPoint {
                    readonly: tag == 2,
                    x: read_num(vm, "x", 0.0),
                    y: read_num(vm, "y", 0.0),
                    z: read_num(vm, "z", 0.0),
                    w: read_num(vm, "w", 1.0),
                },
                3 | 4 => CloneNode::DomRect {
                    readonly: tag == 4,
                    x: read_num(vm, "x", 0.0),
                    y: read_num(vm, "y", 0.0),
                    width: read_num(vm, "width", 0.0),
                    height: read_num(vm, "height", 0.0),
                },
                _ => {
                    let m = crate::cssom_geometry::read_dom_matrix(vm, h)?;
                    CloneNode::DomMatrix {
                        readonly: tag == 6,
                        m,
                    }
                }
            };
            nodes.push(node);
            memo.insert(h, idx);
            return Ok(CloneValue::Node(idx));
        }
    }

    // 6. Boxed primitives (Boolean / Number / String / BigInt; Symbol throws DataCloneError)
    if let Some(boxed) = vm
        .heap
        .get_object(h)
        .and_then(|o| o.rare.as_deref())
        .and_then(|r| r.boxed_value)
    {
        if boxed.is_symbol() {
            return Err(create_dom_exception(
                vm,
                "DataCloneError",
                "Symbol object could not be cloned",
            ));
        }
        let idx = nodes.len();
        let node = if let Some(b) = boxed.as_bool() {
            CloneNode::BoxedBool(b)
        } else if let Some(n) = boxed.as_number() {
            CloneNode::BoxedNumber(n)
        } else if boxed.is_string() {
            CloneNode::BoxedString(vm.value_to_rust_string(boxed))
        } else if let Some(bh) = boxed.as_bigint() {
            let s = vm
                .heap
                .get_bigint(bh)
                .map_or_else(|| "0".to_string(), JsBigInt::to_decimal_string);
            CloneNode::BoxedBigInt(s)
        } else {
            return Err(create_dom_exception(
                vm,
                "DataCloneError",
                "Primitive wrapper could not be cloned",
            ));
        };
        nodes.push(node);
        memo.insert(h, idx);
        return Ok(CloneValue::Node(idx));
    }

    // 7. Error / DOMException
    if let Some(err_kind) = classify_error_kind(vm, ctx, h) {
        let idx = nodes.len();
        nodes.push(CloneNode::Placeholder);
        memo.insert(h, idx);
        let (name, message, cause, stack) = match err_kind {
            ErrorCloneKind::DOMException => {
                let k_n = vm.heap.intern_key("__rix_de_name__");
                let k_m = vm.heap.intern_key("__rix_de_message__");
                let n = vm
                    .heap
                    .object_get_own(h, k_n, &vm.shapes)
                    .map(|p| vm.value_to_rust_string(p.value));
                let m = vm
                    .heap
                    .object_get_own(h, k_m, &vm.shapes)
                    .map(|p| vm.value_to_rust_string(p.value));
                (n, m, None, None)
            }
            _ => {
                let k_n = vm.heap.intern_key("name");
                let k_m = vm.heap.intern_key("message");
                let k_c = vm.heap.intern_key("cause");
                let k_s = vm.heap.intern_key("stack");
                let n = vm
                    .heap
                    .object_get_own(h, k_n, &vm.shapes)
                    .map(|p| vm.value_to_rust_string(p.value));
                let m = vm
                    .heap
                    .object_get_own(h, k_m, &vm.shapes)
                    .map(|p| vm.value_to_rust_string(p.value));
                let c = if let Some(prop) = vm.heap.object_get_own(h, k_c, &vm.shapes) {
                    Some(serialize_value(
                        vm,
                        ctx,
                        prop.value,
                        nodes,
                        memo,
                        transfer_set,
                        for_storage,
                    )?)
                } else {
                    None
                };
                let s = vm
                    .heap
                    .object_get_own(h, k_s, &vm.shapes)
                    .map(|p| vm.value_to_rust_string(p.value));
                (n, m, c, s)
            }
        };
        nodes[idx] = CloneNode::Error {
            kind: err_kind,
            name,
            message,
            cause,
            stack,
        };
        return Ok(CloneValue::Node(idx));
    }

    // 8. Reject non-serializable platform objects (Response, Request, URL, Event, MessagePort not in transfer, DOM nodes, Window, etc.)
    if is_non_serializable_platform_object(vm, ctx, h) {
        return Err(create_dom_exception(
            vm,
            "DataCloneError",
            "Platform object could not be cloned",
        ));
    }

    // 9. Standard ECMAScript exotic / built-in kinds
    let kind_tag = vm.heap.get_object(h).map(|o| match &o.kind {
        ObjectKind::Array => 1u8,
        ObjectKind::ArrayBuffer(_) => 2,
        ObjectKind::TypedArray(_) => 3,
        ObjectKind::DataView(_) => 4,
        ObjectKind::Date(_) => 5,
        ObjectKind::RegExp(_) => 6,
        ObjectKind::Map(_) => 7,
        ObjectKind::Set(_) => 8,
        ObjectKind::Ordinary => 9,
        _ => 0,
    });

    match kind_tag {
        Some(1) => {
            let idx = nodes.len();
            nodes.push(CloneNode::Placeholder);
            memo.insert(h, idx);
            let k_len = vm.heap.intern_key("length");
            let arr_len = vm
                .get_property_by_key(h, k_len)
                .ok()
                .and_then(|v| vm.to_u32(v).ok())
                .unwrap_or(0);
            let keys = vm.own_enumerable_keys_including_elements(h);
            let mut indexed = Vec::new();
            let mut extra_props = Vec::new();
            for key_s in keys {
                let k = vm.heap.intern_key(&key_s);
                let prop_val = vm.get_property_by_key(h, k)?;
                let cloned =
                    serialize_value(vm, ctx, prop_val, nodes, memo, transfer_set, for_storage)?;
                if let Ok(i) = key_s.parse::<u32>() {
                    if i.to_string() == key_s && i < arr_len {
                        indexed.push((i, cloned));
                        continue;
                    }
                }
                extra_props.push((key_s, cloned));
            }
            nodes[idx] = CloneNode::Array {
                length: arr_len,
                indexed,
                extra_props,
            };
            Ok(CloneValue::Node(idx))
        }
        Some(2) => {
            let (is_detached, shared_mem, max_byte_length, bytes) = {
                let o = vm.heap.get_object(h).unwrap();
                let detached = o.rare.as_deref().is_some_and(|r| r.ab_detached)
                    || ctx.frames.borrow().is_buffer_detached(h);
                let shared = o.rare.as_deref().and_then(|r| r.shared_mem.clone());
                let max_len = o.rare.as_deref().and_then(|r| r.ab_max_byte_length);
                let raw = match &o.kind {
                    ObjectKind::ArrayBuffer(b) => (**b).clone(),
                    _ => Vec::new(),
                };
                (detached, shared, max_len, raw)
            };
            if is_detached {
                return Err(create_dom_exception(
                    vm,
                    "DataCloneError",
                    "Cannot clone a detached ArrayBuffer",
                ));
            }
            let idx = nodes.len();
            if let Some(backing) = shared_mem {
                if for_storage {
                    return Err(create_dom_exception(
                        vm,
                        "DataCloneError",
                        "SharedArrayBuffer cannot be serialized for storage",
                    ));
                }
                if !ctx.web_apis.borrow().cross_origin_isolated {
                    return Err(create_dom_exception(
                        vm,
                        "DataCloneError",
                        "SharedArrayBuffer cannot be cloned without crossOriginIsolated",
                    ));
                }
                nodes.push(CloneNode::SharedArrayBuffer { backing });
            } else {
                nodes.push(CloneNode::ArrayBuffer {
                    bytes,
                    max_byte_length,
                });
            }
            memo.insert(h, idx);
            Ok(CloneValue::Node(idx))
        }
        Some(3) => {
            let (ta, auto_length) = {
                let o = vm.heap.get_object(h).unwrap();
                let ObjectKind::TypedArray(ta) = &o.kind else {
                    unreachable!()
                };
                let auto = o.rare.as_deref().is_some_and(|r| r.view_auto_length);
                (*ta, auto)
            };
            // Check buffer detached & bounds (OOB resizable TypedArray throws DataCloneError).
            let buf_byte_len = current_buffer_byte_len(vm, ctx, ta.buffer)?;
            let bpe = ta.kind.bytes_per_element();
            let off = ta.byte_offset as usize;
            let eff_len = if auto_length {
                if off > buf_byte_len {
                    return Err(create_dom_exception(
                        vm,
                        "DataCloneError",
                        "TypedArray view is out of bounds",
                    ));
                }
                ((buf_byte_len - off) / bpe) as u32
            } else {
                let end = off.saturating_add((ta.length as usize).saturating_mul(bpe));
                if end > buf_byte_len {
                    return Err(create_dom_exception(
                        vm,
                        "DataCloneError",
                        "TypedArray view is out of bounds",
                    ));
                }
                ta.length
            };
            let CloneValue::Node(buffer_node) = serialize_value(
                vm,
                ctx,
                JsValue::from_object(ta.buffer),
                nodes,
                memo,
                transfer_set,
                for_storage,
            )?
            else {
                return Err(create_dom_exception(
                    vm,
                    "DataCloneError",
                    "Invalid TypedArray buffer",
                ));
            };
            let idx = nodes.len();
            nodes.push(CloneNode::TypedArray {
                kind: ta.kind,
                buffer_node,
                byte_offset: ta.byte_offset,
                length: eff_len,
                auto_length,
            });
            memo.insert(h, idx);
            Ok(CloneValue::Node(idx))
        }
        Some(4) => {
            let (dv, auto_length) = {
                let o = vm.heap.get_object(h).unwrap();
                let ObjectKind::DataView(dv) = &o.kind else {
                    unreachable!()
                };
                let auto = o.rare.as_deref().is_some_and(|r| r.view_auto_length);
                (*dv, auto)
            };
            let buf_byte_len = current_buffer_byte_len(vm, ctx, dv.buffer)?;
            let off = dv.byte_offset as usize;
            let eff_byte_len = if auto_length {
                if off > buf_byte_len {
                    return Err(create_dom_exception(
                        vm,
                        "DataCloneError",
                        "DataView is out of bounds",
                    ));
                }
                (buf_byte_len - off) as u32
            } else {
                let end = off.saturating_add(dv.byte_length as usize);
                if end > buf_byte_len {
                    return Err(create_dom_exception(
                        vm,
                        "DataCloneError",
                        "DataView is out of bounds",
                    ));
                }
                dv.byte_length
            };
            let CloneValue::Node(buffer_node) = serialize_value(
                vm,
                ctx,
                JsValue::from_object(dv.buffer),
                nodes,
                memo,
                transfer_set,
                for_storage,
            )?
            else {
                return Err(create_dom_exception(
                    vm,
                    "DataCloneError",
                    "Invalid DataView buffer",
                ));
            };
            let idx = nodes.len();
            nodes.push(CloneNode::DataView {
                buffer_node,
                byte_offset: dv.byte_offset,
                byte_length: eff_byte_len,
                auto_length,
            });
            memo.insert(h, idx);
            Ok(CloneValue::Node(idx))
        }
        Some(5) => {
            let ms = match vm.heap.get_object(h).map(|o| &o.kind) {
                Some(ObjectKind::Date(ms)) => *ms,
                _ => f64::NAN,
            };
            let idx = nodes.len();
            nodes.push(CloneNode::Date(ms));
            memo.insert(h, idx);
            Ok(CloneValue::Node(idx))
        }
        Some(6) => {
            let (pattern, flags) = match vm.heap.get_object(h).map(|o| &o.kind) {
                Some(ObjectKind::RegExp(re)) => (re.pattern.clone(), re.flags.clone()),
                _ => (String::new(), String::new()),
            };
            let idx = nodes.len();
            nodes.push(CloneNode::RegExp { pattern, flags });
            memo.insert(h, idx);
            Ok(CloneValue::Node(idx))
        }
        Some(7) => {
            let idx = nodes.len();
            nodes.push(CloneNode::Placeholder);
            memo.insert(h, idx);
            let raw_entries = match vm.heap.get_object(h).map(|o| &o.kind) {
                Some(ObjectKind::Map(m)) => (**m).clone(),
                _ => Vec::new(),
            };
            let mut entries = Vec::with_capacity(raw_entries.len());
            for (k, v) in raw_entries {
                let ck = serialize_value(vm, ctx, k, nodes, memo, transfer_set, for_storage)?;
                let cv = serialize_value(vm, ctx, v, nodes, memo, transfer_set, for_storage)?;
                entries.push((ck, cv));
            }
            nodes[idx] = CloneNode::Map { entries };
            Ok(CloneValue::Node(idx))
        }
        Some(8) => {
            let idx = nodes.len();
            nodes.push(CloneNode::Placeholder);
            memo.insert(h, idx);
            let raw_items = match vm.heap.get_object(h).map(|o| &o.kind) {
                Some(ObjectKind::Set(s)) => (**s).clone(),
                _ => Vec::new(),
            };
            let mut items = Vec::with_capacity(raw_items.len());
            for item in raw_items {
                items.push(serialize_value(
                    vm,
                    ctx,
                    item,
                    nodes,
                    memo,
                    transfer_set,
                    for_storage,
                )?);
            }
            nodes[idx] = CloneNode::Set { items };
            Ok(CloneValue::Node(idx))
        }
        Some(9) => {
            let idx = nodes.len();
            nodes.push(CloneNode::Placeholder);
            memo.insert(h, idx);
            let keys = vm.own_enumerable_keys_including_elements(h);
            let mut props = Vec::with_capacity(keys.len());
            for key_s in keys {
                let k = vm.heap.intern_key(&key_s);
                let prop_val = vm.get_property_by_key(h, k)?;
                let cloned =
                    serialize_value(vm, ctx, prop_val, nodes, memo, transfer_set, for_storage)?;
                props.push((key_s, cloned));
            }
            nodes[idx] = CloneNode::Object { props };
            Ok(CloneValue::Node(idx))
        }
        _ => Err(create_dom_exception(
            vm,
            "DataCloneError",
            "Object could not be cloned",
        )),
    }
}

fn current_buffer_byte_len(
    vm: &mut JsVm,
    ctx: &CloneContext<'_>,
    buf_h: HeapHandle,
) -> Result<usize, JsException> {
    let Some(o) = vm.heap.get_object(buf_h) else {
        return Err(create_dom_exception(
            vm,
            "DataCloneError",
            "Buffer is invalid",
        ));
    };
    if o.rare.as_deref().is_some_and(|r| r.ab_detached)
        || ctx.frames.borrow().is_buffer_detached(buf_h)
    {
        return Err(create_dom_exception(
            vm,
            "DataCloneError",
            "Cannot clone view over a detached ArrayBuffer",
        ));
    }
    if let Some(shared) = o.rare.as_deref().and_then(|r| r.shared_mem.as_ref()) {
        return Ok(shared.data.lock().map(|g| g.len()).unwrap_or(0));
    }
    match &o.kind {
        ObjectKind::ArrayBuffer(b) => Ok(b.len()),
        _ => Err(create_dom_exception(
            vm,
            "DataCloneError",
            "Invalid ArrayBuffer backing",
        )),
    }
}

/// Returns `true` if `payload` transfers a `MediaSourceHandle`, which is only exposed in
/// `Window` and `DedicatedWorkerGlobalScope` (never `SharedWorker` or `ServiceWorker`).
pub(crate) fn payload_has_media_source_handle(payload: &StructuredClonePayload) -> bool {
    payload.has_transferred_media_handle
        || payload
            .nodes
            .iter()
            .any(|n| matches!(n, CloneNode::MediaSourceHandle { .. }))
}

/// Deserializes a cross-thread [`StructuredClonePayload`], returning `Err(())` if any
/// transferred platform object (such as `MediaSourceHandle`) is not exposed in the target realm
/// so callers can dispatch `messageerror` per HTML §2.7.5.
pub(crate) fn try_deserialize_payload(
    vm: &mut JsVm,
    ctx: &CloneContext<'_>,
    payload: StructuredClonePayload,
) -> Result<(JsValue, Vec<HeapHandle>), ()> {
    if payload_has_media_source_handle(&payload) && ctx.frames.borrow().media_source.is_none() {
        return Err(());
    }
    Ok(deserialize_payload(vm, ctx, payload))
}

/// Deserializes a cross-thread [`StructuredClonePayload`] into live JS values on `vm`,
/// returning `(root_value, transferred_message_ports)`.
pub(crate) fn deserialize_payload(
    vm: &mut JsVm,
    ctx: &CloneContext<'_>,
    payload: StructuredClonePayload,
) -> (JsValue, Vec<HeapHandle>) {
    // 1. Attach transferred cross-thread MessagePorts into this runtime's FrameSystem.
    let local_async_tx = ctx.workers.borrow().async_tx_clone();
    let mut attached_ports: Vec<HeapHandle> = Vec::with_capacity(payload.transferred_ports.len());
    for end in payload.transferred_ports {
        let port_h = crate::frames::attach_transferred_cross_thread_port(
            vm,
            ctx.frames,
            end,
            local_async_tx.clone(),
        );
        attached_ports.push(port_h);
    }

    let root_val = deserialize_payload_ref(
        vm,
        ctx,
        &StructuredClonePayload {
            root: payload.root,
            nodes: payload.nodes,
            transferred_ports: Vec::new(),
            has_transferred_media_handle: payload.has_transferred_media_handle,
        },
        &attached_ports,
    );
    (root_val, attached_ports)
}

fn deserialize_payload_ref(
    vm: &mut JsVm,
    ctx: &CloneContext<'_>,
    payload: &StructuredClonePayload,
    attached_ports: &[HeapHandle],
) -> JsValue {
    // 2. Two-pass graph reconstruction so circular references resolve accurately.
    let n = payload.nodes.len();
    let mut handles: Vec<Option<HeapHandle>> = vec![None; n];

    // Pass 1: allocate all non-view shells (buffers, objects, arrays, maps, sets, errors, blobs, etc.).
    for (idx, node) in payload.nodes.iter().enumerate() {
        let h = match node {
            CloneNode::Placeholder => vm.alloc_ordinary_object(),
            CloneNode::BoxedBool(b) => {
                let obj = vm.heap.alloc_object(JsObject::with_kind(
                    ObjectKind::Ordinary,
                    Some(vm.boolean_proto),
                ));
                if let Some(o) = vm.heap.get_object_mut(obj) {
                    o.rare_mut().boxed_value = Some(JsValue::from_bool(*b));
                }
                obj
            }
            CloneNode::BoxedNumber(num) => {
                let obj = vm.heap.alloc_object(JsObject::with_kind(
                    ObjectKind::Ordinary,
                    Some(vm.number_proto),
                ));
                if let Some(o) = vm.heap.get_object_mut(obj) {
                    o.rare_mut().boxed_value = Some(JsValue::from_f64(*num));
                }
                obj
            }
            CloneNode::BoxedString(s) => {
                let str_h = vm.heap.alloc_string(s);
                let obj = vm.heap.alloc_object(JsObject::with_kind(
                    ObjectKind::Ordinary,
                    Some(vm.string_proto),
                ));
                let utf16_len = s.encode_utf16().count();
                if let Some(o) = vm.heap.get_object_mut(obj) {
                    o.rare_mut().boxed_value = Some(JsValue::from_string(str_h));
                }
                let k_len = vm.heap.intern_key("length");
                vm.heap.object_define_data(
                    obj,
                    k_len,
                    JsValue::from_f64(utf16_len as f64),
                    PropertyAttrs::empty(),
                    &mut vm.shapes,
                );
                obj
            }
            CloneNode::BoxedBigInt(s) => {
                let bi = JsBigInt::parse_str(s).unwrap_or_else(JsBigInt::zero);
                let bh = vm.heap.alloc_bigint(bi);
                let obj = vm.heap.alloc_object(JsObject::with_kind(
                    ObjectKind::Ordinary,
                    Some(vm.bigint_proto),
                ));
                if let Some(o) = vm.heap.get_object_mut(obj) {
                    o.rare_mut().boxed_value = Some(JsValue::from_bigint(bh));
                }
                obj
            }
            CloneNode::Date(ms) => vm.alloc_date(*ms),
            CloneNode::RegExp { pattern, flags } => vm.alloc_regexp(pattern.clone(), flags.clone()),
            CloneNode::Error {
                kind,
                name,
                message,
                stack,
                ..
            } => match kind {
                ErrorCloneKind::DOMException => {
                    let de_proto = ctx
                        .web_apis
                        .borrow()
                        .protos
                        .dom_exception_proto
                        .unwrap_or(vm.error_proto);
                    let obj = vm
                        .heap
                        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(de_proto)));
                    let n_str = name.as_deref().unwrap_or("Error");
                    let m_str = message.as_deref().unwrap_or("");
                    let k_brand = vm.heap.intern_key("__rix_dom_exception__");
                    let k_name = vm.heap.intern_key("__rix_de_name__");
                    let k_msg = vm.heap.intern_key("__rix_de_message__");
                    let k_code = vm.heap.intern_key("__rix_de_code__");
                    let nv = JsValue::from_string(vm.heap.alloc_string(n_str));
                    let mv = JsValue::from_string(vm.heap.alloc_string(m_str));
                    let cv = JsValue::from_i32(crate::web_apis::dom_exception_code(n_str));
                    vm.heap.object_define_data(
                        obj,
                        k_brand,
                        JsValue::TRUE,
                        PropertyAttrs::empty(),
                        &mut vm.shapes,
                    );
                    vm.heap.object_define_data(
                        obj,
                        k_name,
                        nv,
                        PropertyAttrs::empty(),
                        &mut vm.shapes,
                    );
                    vm.heap.object_define_data(
                        obj,
                        k_msg,
                        mv,
                        PropertyAttrs::empty(),
                        &mut vm.shapes,
                    );
                    vm.heap.object_define_data(
                        obj,
                        k_code,
                        cv,
                        PropertyAttrs::empty(),
                        &mut vm.shapes,
                    );
                    obj
                }
                _ => {
                    let proto = match kind {
                        ErrorCloneKind::EvalError => vm.eval_error_proto,
                        ErrorCloneKind::RangeError => vm.range_error_proto,
                        ErrorCloneKind::ReferenceError => vm.reference_error_proto,
                        ErrorCloneKind::SyntaxError => vm.syntax_error_proto,
                        ErrorCloneKind::TypeError => vm.type_error_proto,
                        ErrorCloneKind::URIError => vm.uri_error_proto,
                        ErrorCloneKind::Error | ErrorCloneKind::DOMException => vm.error_proto,
                    };
                    let obj = vm
                        .heap
                        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
                    if let Some(o) = vm.heap.get_object_mut(obj) {
                        o.rare_mut().is_error_data = true;
                    }
                    if let Some(n_str) = name {
                        let kn = vm.heap.intern_key("name");
                        let nv = JsValue::from_string(vm.heap.alloc_string(n_str));
                        vm.heap.object_define_data(
                            obj,
                            kn,
                            nv,
                            PropertyAttrs::WRITABLE | PropertyAttrs::CONFIGURABLE,
                            &mut vm.shapes,
                        );
                    }
                    if let Some(m_str) = message {
                        let km = vm.heap.intern_key("message");
                        let mv = JsValue::from_string(vm.heap.alloc_string(m_str));
                        vm.heap.object_define_data(
                            obj,
                            km,
                            mv,
                            PropertyAttrs::WRITABLE | PropertyAttrs::CONFIGURABLE,
                            &mut vm.shapes,
                        );
                    }
                    if let Some(s_str) = stack {
                        let ks = vm.heap.intern_key("stack");
                        let sv = JsValue::from_string(vm.heap.alloc_string(s_str));
                        vm.heap.object_define_data(
                            obj,
                            ks,
                            sv,
                            PropertyAttrs::WRITABLE | PropertyAttrs::CONFIGURABLE,
                            &mut vm.shapes,
                        );
                    }
                    obj
                }
            },
            CloneNode::ArrayBuffer {
                bytes,
                max_byte_length,
            } => {
                let ab = vm.heap.alloc_object(JsObject::with_kind(
                    ObjectKind::ArrayBuffer(Box::new(bytes.clone())),
                    Some(vm.array_buffer_proto),
                ));
                if let Some(ml) = *max_byte_length {
                    if let Some(o) = vm.heap.get_object_mut(ab) {
                        o.rare_mut().ab_max_byte_length = Some(ml);
                    }
                }
                ab
            }
            CloneNode::SharedArrayBuffer { backing } => {
                vm.alloc_shared_array_buffer_from_backing(Arc::clone(backing))
            }
            CloneNode::TypedArray { .. } | CloneNode::DataView { .. } => {
                // Allocated in Pass 1b once their backing buffer handle exists.
                continue;
            }
            CloneNode::Array { length, .. } => {
                let arr = vm.alloc_array();
                if *length > 0 {
                    let k_len = vm.heap.intern_key("length");
                    let _ =
                        vm.set_property_by_key(arr, k_len, JsValue::from_f64(f64::from(*length)));
                }
                arr
            }
            CloneNode::Object { .. } => vm.alloc_ordinary_object(),
            CloneNode::Map { .. } => vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Map(Box::default()),
                Some(vm.map_proto),
            )),
            CloneNode::Set { .. } => vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Set(Box::default()),
                Some(vm.set_proto),
            )),
            CloneNode::Blob {
                bytes,
                content_type,
            } => create_blob_object(vm, ctx.web_apis, bytes.clone(), content_type.clone()),
            CloneNode::File {
                bytes,
                content_type,
                name,
                last_modified,
            } => create_file_object(
                vm,
                ctx.web_apis,
                bytes.clone(),
                content_type.clone(),
                name.clone(),
                *last_modified,
            ),
            CloneNode::FileList { files } => {
                let entries = files
                    .iter()
                    .map(|(b, ct, n, lm)| BlobEntry {
                        bytes: b.clone(),
                        content_type: ct.clone(),
                        name: Some(n.clone()),
                        last_modified: Some(*lm),
                    })
                    .collect();
                create_file_list_object(vm, ctx.web_apis, entries)
            }
            CloneNode::ImageData {
                width,
                height,
                rgba,
            } => create_image_data_obj(vm, ctx.canvas2d, *width, *height, rgba.clone()),
            CloneNode::ImageBitmap {
                width,
                height,
                rgba,
            } => create_image_bitmap_obj(vm, ctx.canvas2d, *width, *height, rgba.clone()),
            CloneNode::OffscreenCanvas {
                width,
                height,
                rgba,
            } => create_offscreen_canvas_obj(vm, ctx.canvas2d, *width, *height, rgba.clone()),
            CloneNode::TransferredPort { port_index } => attached_ports
                .get(*port_index)
                .copied()
                .unwrap_or_else(|| vm.alloc_ordinary_object()),
            CloneNode::MediaSourceHandle { bridge_id } => {
                if let Some(ms) = ctx.frames.borrow().media_source.clone() {
                    ms.borrow_mut().alloc_media_source_handle(vm, *bridge_id)
                } else {
                    vm.alloc_ordinary_object()
                }
            }
            CloneNode::DomPoint {
                readonly,
                x,
                y,
                z,
                w,
            } => crate::cssom_geometry::alloc_dom_point(vm, ctx.host, *readonly, *x, *y, *z, *w),
            CloneNode::DomRect {
                readonly,
                x,
                y,
                width,
                height,
            } => crate::cssom_geometry::alloc_dom_rect_f64(
                vm, ctx.host, *readonly, *x, *y, *width, *height,
            ),
            CloneNode::DomMatrix { readonly, m } => {
                crate::cssom_geometry::alloc_dom_matrix(vm, ctx.host, *readonly, *m)
            }
        };
        handles[idx] = Some(h);
    }

    // Pass 1b: allocate TypedArray and DataView views pointing to their reconstructed buffers.
    for (idx, node) in payload.nodes.iter().enumerate() {
        match node {
            CloneNode::TypedArray {
                kind,
                buffer_node,
                byte_offset,
                length,
                auto_length,
            } => {
                let buf = handles[*buffer_node].unwrap_or_else(|| {
                    vm.heap.alloc_object(JsObject::with_kind(
                        ObjectKind::ArrayBuffer(Box::default()),
                        Some(vm.array_buffer_proto),
                    ))
                });
                let proto = vm.typed_array_protos[*kind as usize];
                let ta = vm.heap.alloc_object(JsObject::with_kind(
                    ObjectKind::TypedArray(TypedArrayObject {
                        kind: *kind,
                        buffer: buf,
                        byte_offset: *byte_offset,
                        length: *length,
                    }),
                    Some(proto),
                ));
                if *auto_length {
                    if let Some(o) = vm.heap.get_object_mut(ta) {
                        o.rare_mut().view_auto_length = true;
                    }
                }
                handles[idx] = Some(ta);
            }
            CloneNode::DataView {
                buffer_node,
                byte_offset,
                byte_length,
                auto_length,
            } => {
                let buf = handles[*buffer_node].unwrap_or_else(|| {
                    vm.heap.alloc_object(JsObject::with_kind(
                        ObjectKind::ArrayBuffer(Box::default()),
                        Some(vm.array_buffer_proto),
                    ))
                });
                let dv = vm.heap.alloc_object(JsObject::with_kind(
                    ObjectKind::DataView(DataViewObject {
                        buffer: buf,
                        byte_offset: *byte_offset,
                        byte_length: *byte_length,
                    }),
                    Some(vm.data_view_proto),
                ));
                if *auto_length {
                    if let Some(o) = vm.heap.get_object_mut(dv) {
                        o.rare_mut().view_auto_length = true;
                    }
                }
                handles[idx] = Some(dv);
            }
            _ => {}
        }
    }

    // Pass 2: populate container edges (Array elements/props, Object props, Map, Set, Error.cause).
    for (idx, node) in payload.nodes.iter().enumerate() {
        let Some(h) = handles[idx] else { continue };
        match node {
            CloneNode::Array {
                length,
                indexed,
                extra_props,
            } => {
                for (i, cv) in indexed {
                    let v = materialize_value(vm, cv, &handles);
                    if let Some(o) = vm.heap.get_object_mut(h) {
                        o.elements.set(*i, v);
                    }
                    vm.heap.write_barrier(h, v);
                }
                if let Some(o) = vm.heap.get_object_mut(h) {
                    if o.elements.len() < *length {
                        o.elements.set_len(*length);
                    }
                }
                for (k_s, cv) in extra_props {
                    let v = materialize_value(vm, cv, &handles);
                    let k = vm.heap.intern_key(k_s);
                    vm.heap
                        .object_define_data(h, k, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
                }
            }
            CloneNode::Object { props } => {
                for (k_s, cv) in props {
                    let v = materialize_value(vm, cv, &handles);
                    let k = vm.heap.intern_key(k_s);
                    vm.heap
                        .object_define_data(h, k, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
                }
            }
            CloneNode::Map { entries } => {
                let mut pairs = Vec::with_capacity(entries.len());
                for (ck, cv) in entries {
                    pairs.push((
                        materialize_value(vm, ck, &handles),
                        materialize_value(vm, cv, &handles),
                    ));
                }
                if let Some(ObjectKind::Map(m)) = vm.heap.get_object_mut(h).map(|o| &mut o.kind) {
                    **m = pairs;
                }
            }
            CloneNode::Set { items } => {
                let mut vals = Vec::with_capacity(items.len());
                for ci in items {
                    vals.push(materialize_value(vm, ci, &handles));
                }
                if let Some(ObjectKind::Set(s)) = vm.heap.get_object_mut(h).map(|o| &mut o.kind) {
                    **s = vals;
                }
            }
            CloneNode::Error {
                cause: Some(c_val), ..
            } => {
                let v = materialize_value(vm, c_val, &handles);
                let kc = vm.heap.intern_key("cause");
                vm.heap.object_define_data(
                    h,
                    kc,
                    v,
                    PropertyAttrs::WRITABLE | PropertyAttrs::CONFIGURABLE,
                    &mut vm.shapes,
                );
            }
            _ => {}
        }
    }

    materialize_value(vm, &payload.root, &handles)
}

fn materialize_value(vm: &mut JsVm, val: &CloneValue, handles: &[Option<HeapHandle>]) -> JsValue {
    match val {
        CloneValue::Undefined => JsValue::UNDEFINED,
        CloneValue::Null => JsValue::NULL,
        CloneValue::Bool(b) => JsValue::from_bool(*b),
        CloneValue::Number(n) => JsValue::from_f64(*n),
        CloneValue::String(s) => JsValue::from_string(vm.heap.alloc_string(s)),
        CloneValue::BigInt(s) => {
            let bi = JsBigInt::parse_str(s).unwrap_or_else(JsBigInt::zero);
            JsValue::from_bigint(vm.heap.alloc_bigint(bi))
        }
        CloneValue::Node(idx) => handles
            .get(*idx)
            .and_then(|o| *o)
            .map_or(JsValue::NULL, JsValue::from_object),
    }
}

// ---------------------------------------------------------------------------
// Compact binary codec for persistent IndexedDB structured-clone values
// ---------------------------------------------------------------------------

const SC_BIN_MAGIC: &[u8; 7] = b"RIXSC01";

fn sc_write_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn sc_write_f64(out: &mut Vec<u8>, v: f64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn sc_write_bytes(out: &mut Vec<u8>, b: &[u8]) {
    sc_write_u32(out, b.len() as u32);
    out.extend_from_slice(b);
}

fn sc_write_str(out: &mut Vec<u8>, s: &str) {
    sc_write_bytes(out, s.as_bytes());
}

fn sc_write_opt_str(out: &mut Vec<u8>, s: Option<&str>) {
    match s {
        None => out.push(0),
        Some(v) => {
            out.push(1);
            sc_write_str(out, v);
        }
    }
}

fn sc_write_val(out: &mut Vec<u8>, v: &CloneValue) {
    match v {
        CloneValue::Undefined => out.push(0),
        CloneValue::Null => out.push(1),
        CloneValue::Bool(false) => out.push(2),
        CloneValue::Bool(true) => out.push(3),
        CloneValue::Number(n) => {
            out.push(4);
            sc_write_f64(out, *n);
        }
        CloneValue::String(s) => {
            out.push(5);
            sc_write_str(out, s);
        }
        CloneValue::BigInt(s) => {
            out.push(6);
            sc_write_str(out, s);
        }
        CloneValue::Node(idx) => {
            out.push(7);
            sc_write_u32(out, *idx as u32);
        }
    }
}

/// Serializes a storage [`StructuredClonePayload`] into compact exact binary bytes
/// (exact IEEE-754 float bits, raw `Vec<u8>` buffers/Blobs, full cyclic object graph).
pub(crate) fn encode_storage_payload(payload: &StructuredClonePayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(SC_BIN_MAGIC);
    sc_write_val(&mut out, &payload.root);
    sc_write_u32(&mut out, payload.nodes.len() as u32);
    for node in &payload.nodes {
        match node {
            CloneNode::Placeholder => out.push(0),
            CloneNode::BoxedBool(b) => {
                out.push(1);
                out.push(u8::from(*b));
            }
            CloneNode::BoxedNumber(n) => {
                out.push(2);
                sc_write_f64(&mut out, *n);
            }
            CloneNode::BoxedString(s) => {
                out.push(3);
                sc_write_str(&mut out, s);
            }
            CloneNode::BoxedBigInt(s) => {
                out.push(4);
                sc_write_str(&mut out, s);
            }
            CloneNode::Date(ms) => {
                out.push(5);
                sc_write_f64(&mut out, *ms);
            }
            CloneNode::RegExp { pattern, flags } => {
                out.push(6);
                sc_write_str(&mut out, pattern);
                sc_write_str(&mut out, flags);
            }
            CloneNode::Error {
                kind,
                name,
                message,
                cause,
                stack,
            } => {
                out.push(7);
                let tag: u8 = match kind {
                    ErrorCloneKind::Error => 0,
                    ErrorCloneKind::EvalError => 1,
                    ErrorCloneKind::RangeError => 2,
                    ErrorCloneKind::ReferenceError => 3,
                    ErrorCloneKind::SyntaxError => 4,
                    ErrorCloneKind::TypeError => 5,
                    ErrorCloneKind::URIError => 6,
                    ErrorCloneKind::DOMException => 7,
                };
                out.push(tag);
                sc_write_opt_str(&mut out, name.as_deref());
                sc_write_opt_str(&mut out, message.as_deref());
                match cause {
                    None => out.push(0),
                    Some(cv) => {
                        out.push(1);
                        sc_write_val(&mut out, cv);
                    }
                }
                sc_write_opt_str(&mut out, stack.as_deref());
            }
            CloneNode::ArrayBuffer {
                bytes,
                max_byte_length,
            } => {
                out.push(8);
                sc_write_bytes(&mut out, bytes);
                match max_byte_length {
                    None => out.push(0),
                    Some(ml) => {
                        out.push(1);
                        sc_write_u32(&mut out, *ml as u32);
                    }
                }
            }
            CloneNode::TypedArray {
                kind,
                buffer_node,
                byte_offset,
                length,
                auto_length,
            } => {
                out.push(9);
                out.push(*kind as u8);
                sc_write_u32(&mut out, *buffer_node as u32);
                sc_write_u32(&mut out, *byte_offset);
                sc_write_u32(&mut out, *length);
                out.push(u8::from(*auto_length));
            }
            CloneNode::DataView {
                buffer_node,
                byte_offset,
                byte_length,
                auto_length,
            } => {
                out.push(10);
                sc_write_u32(&mut out, *buffer_node as u32);
                sc_write_u32(&mut out, *byte_offset);
                sc_write_u32(&mut out, *byte_length);
                out.push(u8::from(*auto_length));
            }
            CloneNode::Array {
                length,
                indexed,
                extra_props,
            } => {
                out.push(11);
                sc_write_u32(&mut out, *length);
                sc_write_u32(&mut out, indexed.len() as u32);
                for (i, v) in indexed {
                    sc_write_u32(&mut out, *i);
                    sc_write_val(&mut out, v);
                }
                sc_write_u32(&mut out, extra_props.len() as u32);
                for (k, v) in extra_props {
                    sc_write_str(&mut out, k);
                    sc_write_val(&mut out, v);
                }
            }
            CloneNode::Object { props } => {
                out.push(12);
                sc_write_u32(&mut out, props.len() as u32);
                for (k, v) in props {
                    sc_write_str(&mut out, k);
                    sc_write_val(&mut out, v);
                }
            }
            CloneNode::Map { entries } => {
                out.push(13);
                sc_write_u32(&mut out, entries.len() as u32);
                for (k, v) in entries {
                    sc_write_val(&mut out, k);
                    sc_write_val(&mut out, v);
                }
            }
            CloneNode::Set { items } => {
                out.push(14);
                sc_write_u32(&mut out, items.len() as u32);
                for it in items {
                    sc_write_val(&mut out, it);
                }
            }
            CloneNode::Blob {
                bytes,
                content_type,
            } => {
                out.push(15);
                sc_write_bytes(&mut out, bytes);
                sc_write_str(&mut out, content_type);
            }
            CloneNode::File {
                bytes,
                content_type,
                name,
                last_modified,
            } => {
                out.push(16);
                sc_write_bytes(&mut out, bytes);
                sc_write_str(&mut out, content_type);
                sc_write_str(&mut out, name);
                sc_write_f64(&mut out, *last_modified);
            }
            CloneNode::FileList { files } => {
                out.push(17);
                sc_write_u32(&mut out, files.len() as u32);
                for (b, ct, n, lm) in files {
                    sc_write_bytes(&mut out, b);
                    sc_write_str(&mut out, ct);
                    sc_write_str(&mut out, n);
                    sc_write_f64(&mut out, *lm);
                }
            }
            CloneNode::ImageData {
                width,
                height,
                rgba,
            } => {
                out.push(18);
                sc_write_u32(&mut out, *width);
                sc_write_u32(&mut out, *height);
                sc_write_bytes(&mut out, rgba);
            }
            CloneNode::DomPoint {
                readonly,
                x,
                y,
                z,
                w,
            } => {
                out.push(19);
                out.push(u8::from(*readonly));
                sc_write_f64(&mut out, *x);
                sc_write_f64(&mut out, *y);
                sc_write_f64(&mut out, *z);
                sc_write_f64(&mut out, *w);
            }
            CloneNode::DomRect {
                readonly,
                x,
                y,
                width,
                height,
            } => {
                out.push(20);
                out.push(u8::from(*readonly));
                sc_write_f64(&mut out, *x);
                sc_write_f64(&mut out, *y);
                sc_write_f64(&mut out, *width);
                sc_write_f64(&mut out, *height);
            }
            CloneNode::DomMatrix { readonly, m } => {
                out.push(21);
                out.push(u8::from(*readonly));
                for &v in m {
                    sc_write_f64(&mut out, v);
                }
            }
            CloneNode::SharedArrayBuffer { .. }
            | CloneNode::ImageBitmap { .. }
            | CloneNode::OffscreenCanvas { .. }
            | CloneNode::TransferredPort { .. }
            | CloneNode::MediaSourceHandle { .. } => {
                out.push(0);
            }
        }
    }
    out
}

struct ScReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ScReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn u8(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
    fn u32(&mut self) -> Option<u32> {
        let s = self.buf.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_le_bytes(s.try_into().ok()?))
    }
    fn f64(&mut self) -> Option<f64> {
        let s = self.buf.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        Some(f64::from_le_bytes(s.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let len = self.u32()? as usize;
        let s = self.buf.get(self.pos..self.pos + len)?;
        self.pos += len;
        Some(s.to_vec())
    }
    fn str(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?).ok()
    }
    fn opt_str(&mut self) -> Option<Option<String>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.str()?)),
            _ => None,
        }
    }
    fn val(&mut self) -> Option<CloneValue> {
        match self.u8()? {
            0 => Some(CloneValue::Undefined),
            1 => Some(CloneValue::Null),
            2 => Some(CloneValue::Bool(false)),
            3 => Some(CloneValue::Bool(true)),
            4 => Some(CloneValue::Number(self.f64()?)),
            5 => Some(CloneValue::String(self.str()?)),
            6 => Some(CloneValue::BigInt(self.str()?)),
            7 => Some(CloneValue::Node(self.u32()? as usize)),
            _ => None,
        }
    }
}

/// Decodes compact binary bytes back into a [`StructuredClonePayload`].
pub(crate) fn decode_storage_payload(raw: &[u8]) -> Option<StructuredClonePayload> {
    if raw.len() < SC_BIN_MAGIC.len() || &raw[..SC_BIN_MAGIC.len()] != SC_BIN_MAGIC {
        return None;
    }
    let mut r = ScReader::new(&raw[SC_BIN_MAGIC.len()..]);
    let root = r.val()?;
    let n = r.u32()? as usize;
    let mut nodes = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        let node = match r.u8()? {
            0 => CloneNode::Placeholder,
            1 => CloneNode::BoxedBool(r.u8()? != 0),
            2 => CloneNode::BoxedNumber(r.f64()?),
            3 => CloneNode::BoxedString(r.str()?),
            4 => CloneNode::BoxedBigInt(r.str()?),
            5 => CloneNode::Date(r.f64()?),
            6 => CloneNode::RegExp {
                pattern: r.str()?,
                flags: r.str()?,
            },
            7 => {
                let kind = match r.u8()? {
                    0 => ErrorCloneKind::Error,
                    1 => ErrorCloneKind::EvalError,
                    2 => ErrorCloneKind::RangeError,
                    3 => ErrorCloneKind::ReferenceError,
                    4 => ErrorCloneKind::SyntaxError,
                    5 => ErrorCloneKind::TypeError,
                    6 => ErrorCloneKind::URIError,
                    7 => ErrorCloneKind::DOMException,
                    _ => return None,
                };
                let name = r.opt_str()?;
                let message = r.opt_str()?;
                let cause = match r.u8()? {
                    0 => None,
                    1 => Some(r.val()?),
                    _ => return None,
                };
                let stack = r.opt_str()?;
                CloneNode::Error {
                    kind,
                    name,
                    message,
                    cause,
                    stack,
                }
            }
            8 => {
                let bytes = r.bytes()?;
                let max_byte_length = match r.u8()? {
                    0 => None,
                    1 => Some(r.u32()? as usize),
                    _ => return None,
                };
                CloneNode::ArrayBuffer {
                    bytes,
                    max_byte_length,
                }
            }
            9 => {
                let kind_idx = r.u8()? as usize;
                let kind = *TypedArrayKind::ALL.get(kind_idx)?;
                let buffer_node = r.u32()? as usize;
                let byte_offset = r.u32()?;
                let length = r.u32()?;
                let auto_length = r.u8()? != 0;
                CloneNode::TypedArray {
                    kind,
                    buffer_node,
                    byte_offset,
                    length,
                    auto_length,
                }
            }
            10 => {
                let buffer_node = r.u32()? as usize;
                let byte_offset = r.u32()?;
                let byte_length = r.u32()?;
                let auto_length = r.u8()? != 0;
                CloneNode::DataView {
                    buffer_node,
                    byte_offset,
                    byte_length,
                    auto_length,
                }
            }
            11 => {
                let length = r.u32()?;
                let ilen = r.u32()? as usize;
                let mut indexed = Vec::with_capacity(ilen.min(4096));
                for _ in 0..ilen {
                    indexed.push((r.u32()?, r.val()?));
                }
                let elen = r.u32()? as usize;
                let mut extra_props = Vec::with_capacity(elen.min(4096));
                for _ in 0..elen {
                    extra_props.push((r.str()?, r.val()?));
                }
                CloneNode::Array {
                    length,
                    indexed,
                    extra_props,
                }
            }
            12 => {
                let plen = r.u32()? as usize;
                let mut props = Vec::with_capacity(plen.min(4096));
                for _ in 0..plen {
                    props.push((r.str()?, r.val()?));
                }
                CloneNode::Object { props }
            }
            13 => {
                let elen = r.u32()? as usize;
                let mut entries = Vec::with_capacity(elen.min(4096));
                for _ in 0..elen {
                    entries.push((r.val()?, r.val()?));
                }
                CloneNode::Map { entries }
            }
            14 => {
                let ilen = r.u32()? as usize;
                let mut items = Vec::with_capacity(ilen.min(4096));
                for _ in 0..ilen {
                    items.push(r.val()?);
                }
                CloneNode::Set { items }
            }
            15 => CloneNode::Blob {
                bytes: r.bytes()?,
                content_type: r.str()?,
            },
            16 => CloneNode::File {
                bytes: r.bytes()?,
                content_type: r.str()?,
                name: r.str()?,
                last_modified: r.f64()?,
            },
            17 => {
                let flen = r.u32()? as usize;
                let mut files = Vec::with_capacity(flen.min(256));
                for _ in 0..flen {
                    files.push((r.bytes()?, r.str()?, r.str()?, r.f64()?));
                }
                CloneNode::FileList { files }
            }
            18 => CloneNode::ImageData {
                width: r.u32()?,
                height: r.u32()?,
                rgba: r.bytes()?,
            },
            19 => CloneNode::DomPoint {
                readonly: r.u8()? != 0,
                x: r.f64()?,
                y: r.f64()?,
                z: r.f64()?,
                w: r.f64()?,
            },
            20 => CloneNode::DomRect {
                readonly: r.u8()? != 0,
                x: r.f64()?,
                y: r.f64()?,
                width: r.f64()?,
                height: r.f64()?,
            },
            21 => {
                let readonly = r.u8()? != 0;
                let mut m = [0.0f64; 16];
                for slot in &mut m {
                    *slot = r.f64()?;
                }
                CloneNode::DomMatrix { readonly, m }
            }
            _ => return None,
        };
        nodes.push(node);
    }
    Some(StructuredClonePayload {
        root,
        nodes,
        transferred_ports: Vec::new(),
        has_transferred_media_handle: false,
    })
}
