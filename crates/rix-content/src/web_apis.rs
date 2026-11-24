//! Core WHATWG Web Platform APIs installed in [`crate::ContentRuntime`]:
//! - `URL` and `URLSearchParams` (backed by `url::Url`)
//! - `TextEncoder` and `TextDecoder` (backed by `encoding_rs`)
//! - `Blob` and `File`
//! - `structuredClone` (cycle-safe deep clone with `DataCloneError`)
//! - `AbortController` and `AbortSignal` (EventTarget + `rix_net::AbortHandle`)
//! - `ReadableStream`, `WritableStream`, and `TransformStream`
//! - `Location` and `History` (`pushState` / `replaceState` / `back` / `forward` / `go`)
//! - `localStorage` (origin-partitioned `rix_storage::StorageEngine`) and `sessionStorage`
//! - `Performance` (`now`, `mark`, `measure`, `getEntries*`, `clear*`)
//!
//! Reference: Spec 06 §2–§3 & Issue #17.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use encoding_rs::Encoding;
use rix_js::builtins_collections::alloc_values_iterator;
use rix_js::heap::{DataViewObject, TypedArrayKind, TypedArrayObject};
use rix_js::promise::promise_resolve_value;
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_net::AbortHandle;
use rix_storage::StorageEngine;
use url::Url;

use crate::dom_bridge::SharedHost;
use crate::event_loop::SharedEventLoop;
use crate::events::{fire_object_event, SharedEvents};

/// Default document URL used when a runtime is created without an explicit origin.
pub const DEFAULT_DOCUMENT_URL: &str = "http://localhost/";

#[derive(Debug, Clone)]
pub(crate) struct BlobEntry {
    pub bytes: Vec<u8>,
    pub content_type: String,
    pub name: Option<String>,
    pub last_modified: Option<f64>,
}

#[derive(Debug, Clone)]
pub(crate) struct AbortSignalEntry {
    pub aborted: bool,
    pub reason: JsValue,
    pub handle: AbortHandle,
    pub dependent_signals: Vec<HeapHandle>,
}

#[derive(Debug, Clone)]
struct SearchParamsEntry {
    pairs: Vec<(String, String)>,
    owner_url: Option<HeapHandle>,
}

#[derive(Debug, Clone)]
struct UrlEntry {
    url: Url,
    search_params: HeapHandle,
}

struct TextDecoderEntry {
    encoding: &'static Encoding,
    decoder: encoding_rs::Decoder,
    fatal: bool,
    ignore_bom: bool,
    bom_seen: bool,
    do_not_flush: bool,
}

impl TextDecoderEntry {
    fn new(encoding: &'static Encoding, fatal: bool, ignore_bom: bool) -> Self {
        let decoder = if ignore_bom {
            encoding.new_decoder_without_bom_handling()
        } else {
            encoding.new_decoder_with_bom_removal()
        };
        Self {
            encoding,
            decoder,
            fatal,
            ignore_bom,
            bom_seen: false,
            do_not_flush: false,
        }
    }

    fn reset(&mut self) {
        self.decoder = if self.ignore_bom {
            self.encoding.new_decoder_without_bom_handling()
        } else {
            self.encoding.new_decoder_with_bom_removal()
        };
        self.bom_seen = false;
        self.do_not_flush = false;
    }
}

#[derive(Debug, Clone)]
pub(crate) enum FormDataVal {
    Text(String),
    File(HeapHandle),
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FormDataEntry {
    pub pairs: Vec<(String, FormDataVal)>,
}

#[derive(Debug, Clone)]
struct HistoryItem {
    url: Url,
    state: JsValue,
}

#[derive(Debug, Clone)]
struct PerfEntry {
    entry_type: String,
    name: String,
    start_time: f64,
    duration: f64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct WebApiPrototypes {
    pub blob_proto: Option<HeapHandle>,
    pub file_proto: Option<HeapHandle>,
    pub abort_signal_proto: Option<HeapHandle>,
    pub url_proto: Option<HeapHandle>,
    pub search_params_proto: Option<HeapHandle>,
    pub form_data_proto: Option<HeapHandle>,
    pub text_decoder_proto: Option<HeapHandle>,
}

/// Shared state backing all non-network WHATWG Web APIs in a `ContentRuntime`.
pub struct WebApiState {
    pub(crate) blobs: HashMap<HeapHandle, BlobEntry>,
    pub(crate) abort_signals: HashMap<HeapHandle, AbortSignalEntry>,
    url_objects: HashMap<HeapHandle, UrlEntry>,
    search_params: HashMap<HeapHandle, SearchParamsEntry>,
    text_decoders: HashMap<HeapHandle, TextDecoderEntry>,
    pub(crate) form_data: HashMap<HeapHandle, FormDataEntry>,
    pub(crate) location_url: Url,
    history: Vec<HistoryItem>,
    history_index: usize,
    pub(crate) storage: StorageEngine,
    pub(crate) object_urls: HashMap<String, Vec<u8>>,
    pub(crate) object_url_objects: HashMap<String, HeapHandle>,
    object_url_validators: Vec<Rc<dyn Fn(HeapHandle) -> bool>>,
    next_object_url_id: u64,
    session_storage: HashMap<String, Vec<(String, String)>>,
    perf_entries: Vec<PerfEntry>,
    pub console_messages: Vec<(String, String)>,
    pub(crate) protos: WebApiPrototypes,
}

/// Shared handle to [`WebApiState`].
pub type SharedWebApis = Rc<RefCell<WebApiState>>;

impl WebApiState {
    pub(crate) fn new(origin_url: &str, storage: StorageEngine) -> Self {
        let parsed = Url::parse(origin_url)
            .unwrap_or_else(|_| Url::parse(DEFAULT_DOCUMENT_URL).expect("valid default url"));
        Self {
            blobs: HashMap::new(),
            abort_signals: HashMap::new(),
            url_objects: HashMap::new(),
            search_params: HashMap::new(),
            text_decoders: HashMap::new(),
            form_data: HashMap::new(),
            history: vec![HistoryItem {
                url: parsed.clone(),
                state: JsValue::NULL,
            }],
            history_index: 0,
            location_url: parsed,
            storage,
            object_urls: HashMap::new(),
            object_url_objects: HashMap::new(),
            object_url_validators: Vec::new(),
            next_object_url_id: 1,
            session_storage: HashMap::new(),
            perf_entries: Vec::new(),
            console_messages: Vec::new(),
            protos: WebApiPrototypes::default(),
        }
    }

    /// Records a console message (`log`, `info`, `warn`, `error`, `debug`) for Developer HUD display.
    pub fn record_console(&mut self, level: impl Into<String>, message: impl Into<String>) {
        self.console_messages.push((level.into(), message.into()));
        if self.console_messages.len() > 256 {
            self.console_messages.remove(0);
        }
    }

    /// Drains all buffered JS console entries since the last drain.
    pub fn drain_console_messages(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.console_messages)
    }

    /// Updates the current document URL (affecting `location`, `history`, `localStorage`, and CORS origin).
    pub fn set_document_url(&mut self, url_str: &str) {
        if let Ok(parsed) = Url::parse(url_str) {
            self.location_url = parsed.clone();
            if let Some(cur) = self.history.get_mut(self.history_index) {
                cur.url = parsed;
            }
        }
    }

    /// Returns the serialized origin (`scheme://host[:port]`) of the current document.
    #[must_use]
    pub fn current_origin(&self) -> String {
        origin_of_url(&self.location_url)
    }

    /// Returns the full current document URL string.
    #[must_use]
    pub fn current_href(&self) -> String {
        self.location_url.as_str().to_string()
    }

    /// Replaces the backing [`StorageEngine`] used by `localStorage`.
    pub fn set_storage(&mut self, storage: StorageEngine) {
        self.storage = storage;
    }

    /// Registers a predicate recognizing host objects (e.g., `MediaSource`) accepted by `URL.createObjectURL`.
    pub(crate) fn register_object_url_validator(
        &mut self,
        validator: impl Fn(HeapHandle) -> bool + 'static,
    ) {
        self.object_url_validators.push(Rc::new(validator));
    }

    /// Collects all GC roots retained by Web API objects and internal queues.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.protos.blob_proto,
            self.protos.file_proto,
            self.protos.abort_signal_proto,
            self.protos.url_proto,
            self.protos.search_params_proto,
            self.protos.form_data_proto,
            self.protos.text_decoder_proto,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(p));
        }
        for s in self.abort_signals.values() {
            out.push(s.reason);
            for &dep in &s.dependent_signals {
                out.push(JsValue::from_object(dep));
            }
        }
        for u in self.url_objects.values() {
            out.push(JsValue::from_object(u.search_params));
        }
        for fd in self.form_data.values() {
            for (_, v) in &fd.pairs {
                if let FormDataVal::File(fh) = v {
                    out.push(JsValue::from_object(*fh));
                }
            }
        }
        for &h in self.object_url_objects.values() {
            out.push(JsValue::from_object(h));
        }
        for item in &self.history {
            out.push(item.state);
        }
    }

    /// Drops internal native slots for GC-collected JS objects.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.blobs.retain(|&h, _| heap.is_alive(h));
        self.abort_signals.retain(|&h, _| heap.is_alive(h));
        for s in self.abort_signals.values_mut() {
            s.dependent_signals.retain(|&h| heap.is_alive(h));
        }
        self.url_objects.retain(|&h, _| heap.is_alive(h));
        self.search_params.retain(|&h, _| heap.is_alive(h));
        self.text_decoders.retain(|&h, _| heap.is_alive(h));
        self.form_data.retain(|&h, _| heap.is_alive(h));
        self.object_url_objects.retain(|_, &mut h| heap.is_alive(h));
    }
}

/// Computes the WHATWG serialized origin (`scheme://host[:port]`) for `url`.
#[must_use]
pub(crate) fn origin_of_url(u: &Url) -> String {
    let origin = u.origin().ascii_serialization();
    if origin.is_empty() {
        "null".to_string()
    } else {
        origin
    }
}

#[inline]
pub(crate) fn arg(args: &[JsValue], i: usize) -> JsValue {
    args.get(i).copied().unwrap_or(JsValue::UNDEFINED)
}

pub(crate) fn define_global(vm: &mut JsVm, name: &str, val: JsValue) {
    let k = vm.heap.intern_key(name);
    let g = vm.global;
    vm.heap
        .object_define_data(g, k, val, PropertyAttrs::METHOD, &mut vm.shapes);
}

pub(crate) fn define_host_method(
    vm: &mut JsVm,
    target: HeapHandle,
    name: &str,
    f: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
) {
    let key = vm.heap.intern_key(name);
    let func = vm.alloc_host_fn(name, f);
    vm.heap.object_define_data(
        target,
        key,
        JsValue::from_object(func),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
}

pub(crate) fn define_host_getter(
    vm: &mut JsVm,
    target: HeapHandle,
    name: &str,
    f: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
) {
    let key = vm.heap.intern_key(name);
    let g = vm.alloc_host_fn(name, f);
    vm.heap.object_define_accessor(
        target,
        key,
        Some(g),
        None,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
}

pub(crate) fn define_host_accessor(
    vm: &mut JsVm,
    target: HeapHandle,
    name: &str,
    getter: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
    setter: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
) {
    let key = vm.heap.intern_key(name);
    let g = vm.alloc_host_fn(name, getter);
    let s = vm.alloc_host_fn(name, setter);
    vm.heap.object_define_accessor(
        target,
        key,
        Some(g),
        Some(s),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
}

pub(crate) fn bind_ctor_proto(vm: &mut JsVm, ctor: HeapHandle, proto: HeapHandle) {
    vm.mark_constructor(ctor);
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        proto,
        vm.key_constructor,
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
}

pub(crate) fn alloc_array_buffer(vm: &mut JsVm, bytes: Vec<u8>) -> HeapHandle {
    vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::ArrayBuffer(Box::new(bytes)),
        Some(vm.array_buffer_proto),
    ))
}

pub(crate) fn alloc_uint8_array(vm: &mut JsVm, bytes: Vec<u8>) -> HeapHandle {
    let len = bytes.len() as u32;
    let buf = alloc_array_buffer(vm, bytes);
    vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::TypedArray(TypedArrayObject {
            kind: TypedArrayKind::Uint8,
            buffer: buf,
            byte_offset: 0,
            length: len,
        }),
        Some(vm.typed_array_protos[TypedArrayKind::Uint8 as usize]),
    ))
}

fn read_buffer_slice(vm: &JsVm, buf_h: HeapHandle, off: usize, len: usize) -> Option<Vec<u8>> {
    let obj = vm.heap.get_object(buf_h)?;
    if let Some(shared) = obj.rare.as_ref().and_then(|r| r.shared_mem.as_ref()) {
        let data = shared.data.lock().unwrap_or_else(|e| e.into_inner());
        let fixed_cap = obj
            .rare
            .as_ref()
            .and_then(|r| r.ab_fixed_byte_len)
            .map_or(data.len(), |f| f.min(data.len()));
        let end = off.saturating_add(len).min(fixed_cap);
        return Some(data[off.min(end)..end].to_vec());
    }
    if let ObjectKind::ArrayBuffer(b) = &obj.kind {
        let end = off.saturating_add(len).min(b.len());
        return Some(b[off.min(end)..end].to_vec());
    }
    None
}

fn read_buffer_all(vm: &JsVm, buf_h: HeapHandle) -> Option<Vec<u8>> {
    let obj = vm.heap.get_object(buf_h)?;
    if let Some(shared) = obj.rare.as_ref().and_then(|r| r.shared_mem.as_ref()) {
        let data = shared.data.lock().unwrap_or_else(|e| e.into_inner());
        let cap = obj
            .rare
            .as_ref()
            .and_then(|r| r.ab_fixed_byte_len)
            .map_or(data.len(), |f| f.min(data.len()));
        return Some(data[..cap].to_vec());
    }
    if let ObjectKind::ArrayBuffer(b) = &obj.kind {
        return Some((**b).clone());
    }
    None
}

fn extract_allow_shared_buffer_source(vm: &mut JsVm, val: JsValue) -> Result<Vec<u8>, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Expected an ArrayBuffer, SharedArrayBuffer, or ArrayBufferView"));
    };
    let kind_info = vm.heap.get_object(h).map(|o| match &o.kind {
        ObjectKind::ArrayBuffer(_) => 1u8,
        ObjectKind::TypedArray(_) => 2,
        ObjectKind::DataView(_) => 3,
        _ => 0,
    });
    match kind_info {
        Some(1) => read_buffer_all(vm, h).ok_or_else(|| vm.type_error("Invalid ArrayBuffer")),
        Some(2) => {
            let Some(ObjectKind::TypedArray(ta)) = vm.heap.get_object(h).map(|o| &o.kind) else {
                unreachable!()
            };
            let ta = *ta;
            let byte_len = (ta.length as usize).saturating_mul(ta.kind.bytes_per_element());
            read_buffer_slice(vm, ta.buffer, ta.byte_offset as usize, byte_len)
                .ok_or_else(|| vm.type_error("Invalid TypedArray buffer"))
        }
        Some(3) => {
            let Some(ObjectKind::DataView(dv)) = vm.heap.get_object(h).map(|o| &o.kind) else {
                unreachable!()
            };
            let dv = *dv;
            read_buffer_slice(
                vm,
                dv.buffer,
                dv.byte_offset as usize,
                dv.byte_length as usize,
            )
            .ok_or_else(|| vm.type_error("Invalid DataView buffer"))
        }
        _ => Err(vm.type_error("Expected an ArrayBuffer, SharedArrayBuffer, or ArrayBufferView")),
    }
}

/// Extracts raw bytes from JS `String`, `ArrayBuffer`, `SharedArrayBuffer`, `TypedArray`,
/// `DataView`, `Blob`, or `URLSearchParams`.
pub(crate) fn extract_bytes_from_js(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    val: JsValue,
) -> Result<Vec<u8>, JsException> {
    if val.is_undefined() || val.is_null() {
        return Ok(Vec::new());
    }
    if let Some(h) = val.as_object() {
        if let Some(entry) = web_apis.borrow().blobs.get(&h) {
            return Ok(entry.bytes.clone());
        }
        if let Some(sp) = web_apis.borrow().search_params.get(&h) {
            return Ok(serialize_query_pairs(&sp.pairs).into_bytes());
        }
        match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::ArrayBuffer(_)) => {
                if let Some(bytes) = read_buffer_all(vm, h) {
                    return Ok(bytes);
                }
            }
            Some(ObjectKind::TypedArray(ta)) => {
                let ta = *ta;
                let byte_len = (ta.length as usize).saturating_mul(ta.kind.bytes_per_element());
                if let Some(bytes) =
                    read_buffer_slice(vm, ta.buffer, ta.byte_offset as usize, byte_len)
                {
                    return Ok(bytes);
                }
            }
            Some(ObjectKind::DataView(dv)) => {
                let dv = *dv;
                if let Some(bytes) = read_buffer_slice(
                    vm,
                    dv.buffer,
                    dv.byte_offset as usize,
                    dv.byte_length as usize,
                ) {
                    return Ok(bytes);
                }
            }
            _ => {}
        }
    }
    Ok(vm.value_to_rust_string(val).into_bytes())
}

const DOM_EXCEPTION_CODES: &[(&str, i32)] = &[
    ("INDEX_SIZE_ERR", 1),
    ("DOMSTRING_SIZE_ERR", 2),
    ("HIERARCHY_REQUEST_ERR", 3),
    ("WRONG_DOCUMENT_ERR", 4),
    ("INVALID_CHARACTER_ERR", 5),
    ("NO_DATA_ALLOWED_ERR", 6),
    ("NO_MODIFICATION_ALLOWED_ERR", 7),
    ("NOT_FOUND_ERR", 8),
    ("NOT_SUPPORTED_ERR", 9),
    ("INUSE_ATTRIBUTE_ERR", 10),
    ("INVALID_STATE_ERR", 11),
    ("SYNTAX_ERR", 12),
    ("INVALID_MODIFICATION_ERR", 13),
    ("NAMESPACE_ERR", 14),
    ("INVALID_ACCESS_ERR", 15),
    ("VALIDATION_ERR", 16),
    ("TYPE_MISMATCH_ERR", 17),
    ("SECURITY_ERR", 18),
    ("NETWORK_ERR", 19),
    ("ABORT_ERR", 20),
    ("URL_MISMATCH_ERR", 21),
    ("QUOTA_EXCEEDED_ERR", 22),
    ("TIMEOUT_ERR", 23),
    ("INVALID_NODE_TYPE_ERR", 24),
    ("DATA_CLONE_ERR", 25),
];

fn dom_exception_legacy_code(name: &str) -> i32 {
    match name {
        "IndexSizeError" => 1,
        "HierarchyRequestError" => 3,
        "WrongDocumentError" => 4,
        "InvalidCharacterError" => 5,
        "NoModificationAllowedError" => 7,
        "NotFoundError" => 8,
        "NotSupportedError" => 9,
        "InUseAttributeError" => 10,
        "InvalidStateError" => 11,
        "SyntaxError" => 12,
        "InvalidModificationError" => 13,
        "NamespaceError" => 14,
        "InvalidAccessError" => 15,
        "TypeMismatchError" => 17,
        "SecurityError" => 18,
        "NetworkError" => 19,
        "AbortError" => 20,
        "URLMismatchError" => 21,
        "QuotaExceededError" => 22,
        "TimeoutError" => 23,
        "InvalidNodeTypeError" => 24,
        "DataCloneError" => 25,
        _ => 0,
    }
}

pub(crate) fn create_dom_exception(vm: &mut JsVm, name: &str, message: &str) -> JsException {
    let k_de = vm.heap.intern_key("DOMException");
    let proto = vm
        .heap
        .object_get_own(vm.global, k_de, &vm.shapes)
        .and_then(|p| p.value.as_object())
        .and_then(|ctor| vm.heap.object_get_own(ctor, vm.key_prototype, &vm.shapes))
        .and_then(|p| p.value.as_object())
        .unwrap_or(vm.error_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let k_brand = vm.heap.intern_key("__rix_dom_exception__");
    let k_name = vm.heap.intern_key("__rix_de_name__");
    let k_msg = vm.heap.intern_key("__rix_de_message__");
    let k_code = vm.heap.intern_key("__rix_de_code__");
    let n_val = JsValue::from_string(vm.heap.alloc_string(name));
    let m_val = JsValue::from_string(vm.heap.alloc_string(message));
    let c_val = JsValue::from_i32(dom_exception_legacy_code(name));
    vm.heap.object_define_data(
        obj,
        k_brand,
        JsValue::TRUE,
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap
        .object_define_data(obj, k_name, n_val, PropertyAttrs::empty(), &mut vm.shapes);
    vm.heap
        .object_define_data(obj, k_msg, m_val, PropertyAttrs::empty(), &mut vm.shapes);
    vm.heap
        .object_define_data(obj, k_code, c_val, PropertyAttrs::empty(), &mut vm.shapes);
    JsException {
        value: JsValue::from_object(obj),
        message: format!("{name}: {message}"),
    }
}

pub(crate) fn create_blob_object(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    bytes: Vec<u8>,
    content_type: String,
) -> HeapHandle {
    let proto = web_apis
        .borrow()
        .protos
        .blob_proto
        .unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    web_apis.borrow_mut().blobs.insert(
        obj,
        BlobEntry {
            bytes,
            content_type,
            name: None,
            last_modified: None,
        },
    );
    obj
}

#[allow(dead_code)]
pub(crate) fn abort_info_for_signal(
    web_apis: &SharedWebApis,
    val: JsValue,
) -> Option<(AbortHandle, bool, JsValue)> {
    let h = val.as_object()?;
    let state = web_apis.borrow();
    let entry = state.abort_signals.get(&h)?;
    Some((entry.handle.clone(), entry.aborted, entry.reason))
}

/// Installs all core non-network Web Platform APIs onto `vm`.
pub fn install_web_apis(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    origin_url: &str,
    storage: StorageEngine,
) -> SharedWebApis {
    let web_apis = Rc::new(RefCell::new(WebApiState::new(origin_url, storage)));
    host.borrow_mut().main_document_url = web_apis.borrow().current_href();
    install_dom_exception_ctor(vm);
    install_url_and_search_params(vm, &web_apis);
    install_text_encoding(vm, &web_apis);
    install_blob_and_file(vm, &web_apis);
    install_form_data(vm, host, &web_apis);
    install_structured_clone(vm, &web_apis);
    install_abort_controller(vm, host, events, event_loop, &web_apis);
    install_location_and_history(vm, host, &web_apis);
    install_web_storage(vm, &web_apis);
    install_performance(vm, event_loop, &web_apis);
    install_console(vm, &web_apis);
    install_window_and_navigator(vm, host, event_loop, &web_apis);
    web_apis
}

fn install_dom_exception_ctor(vm: &mut JsVm) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.error_proto),
    ));
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;

    let brand_check = |vm: &mut JsVm, this_val: JsValue| -> Result<HeapHandle, JsException> {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation on DOMException.prototype"));
        };
        let k_brand = vm.heap.intern_key("__rix_dom_exception__");
        if vm.heap.object_get_own(h, k_brand, &vm.shapes).is_some() {
            Ok(h)
        } else {
            Err(vm.type_error("Illegal invocation on DOMException.prototype"))
        }
    };

    for (prop, hidden) in [
        ("name", "__rix_de_name__"),
        ("message", "__rix_de_message__"),
        ("code", "__rix_de_code__"),
    ] {
        let key = vm.heap.intern_key(prop);
        let getter = vm.alloc_host_fn(prop, move |vm, this_val, _| {
            let h = brand_check(vm, this_val)?;
            let kh = vm.heap.intern_key(hidden);
            Ok(vm
                .heap
                .object_get_own(h, kh, &vm.shapes)
                .map_or(JsValue::UNDEFINED, |p| p.value))
        });
        vm.heap.object_define_accessor(
            proto,
            key,
            Some(getter),
            None,
            PropertyAttrs::ENUMERABLE | PropertyAttrs::CONFIGURABLE,
            &mut vm.shapes,
        );
    }

    let ctor = vm.alloc_host_fn("DOMException", move |vm, _this, args| {
        let msg = if arg(args, 0).is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(arg(args, 0))
        };
        let name = if arg(args, 1).is_undefined() {
            "Error".to_string()
        } else {
            vm.value_to_rust_string(arg(args, 1))
        };
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        let k_brand = vm.heap.intern_key("__rix_dom_exception__");
        let k_name = vm.heap.intern_key("__rix_de_name__");
        let k_msg = vm.heap.intern_key("__rix_de_message__");
        let k_code = vm.heap.intern_key("__rix_de_code__");
        let nv = JsValue::from_string(vm.heap.alloc_string(&name));
        let mv = JsValue::from_string(vm.heap.alloc_string(&msg));
        let cv = JsValue::from_i32(dom_exception_legacy_code(&name));
        vm.heap.object_define_data(
            obj,
            k_brand,
            JsValue::TRUE,
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
        vm.heap
            .object_define_data(obj, k_name, nv, PropertyAttrs::empty(), &mut vm.shapes);
        vm.heap
            .object_define_data(obj, k_msg, mv, PropertyAttrs::empty(), &mut vm.shapes);
        vm.heap
            .object_define_data(obj, k_code, cv, PropertyAttrs::empty(), &mut vm.shapes);
        Ok(JsValue::from_object(obj))
    });
    for &(const_name, code) in DOM_EXCEPTION_CODES {
        let k = vm.heap.intern_key(const_name);
        let v = JsValue::from_i32(code);
        vm.heap
            .object_define_data(ctor, k, v, PropertyAttrs::ENUMERABLE, &mut vm.shapes);
        vm.heap
            .object_define_data(proto, k, v, PropertyAttrs::ENUMERABLE, &mut vm.shapes);
    }
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "DOMException", JsValue::from_object(ctor));
}

// ---------------------------------------------------------------------------
// URL & URLSearchParams (WHATWG URL Standard + Encoding HTML error mode)
// ---------------------------------------------------------------------------

/// WHATWG Encoding §4.3 "get an output encoding": maps replacement/UTF-16 to UTF-8.
pub(crate) fn output_encoding_for_charset(charset: &str) -> &'static Encoding {
    let trimmed = charset.trim_matches(|c: char| matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' '));
    let Some(enc) = Encoding::for_label(trimmed.as_bytes()) else {
        return encoding_rs::UTF_8;
    };
    if enc == encoding_rs::REPLACEMENT
        || enc == encoding_rs::UTF_16LE
        || enc == encoding_rs::UTF_16BE
    {
        encoding_rs::UTF_8
    } else {
        enc
    }
}

/// Percent-encodes a URL query string using `charset` per WHATWG URL §4.3 ("query state") and
/// Encoding §4.2 (HTML error mode: unmappable scalar values become `%26%23<decimal>%3B`).
pub(crate) fn percent_encode_query_with_charset(
    query: &str,
    charset: &str,
    is_special: bool,
) -> String {
    use std::fmt::Write;
    let enc = output_encoding_for_charset(charset);
    let push_query_bytes = |bytes: &[u8], out: &mut String| {
        for &b in bytes {
            let must_encode = !(0x21..=0x7E).contains(&b)
                || matches!(b, b'"' | b'#' | b'<' | b'>')
                || (is_special && b == b'\'');
            if must_encode {
                let _ = write!(out, "%{b:02X}");
            } else {
                out.push(b as char);
            }
        }
    };
    let mut out = String::with_capacity(query.len());
    if enc == encoding_rs::UTF_8 {
        push_query_bytes(query.as_bytes(), &mut out);
        return out;
    }
    let mut encoder = enc.new_encoder();
    let mut buf = [0u8; 256];
    let mut src = query;
    loop {
        let (res, read, written) =
            encoder.encode_from_utf8_without_replacement(src, &mut buf, true);
        if written > 0 {
            push_query_bytes(&buf[..written], &mut out);
        }
        src = &src[read..];
        match res {
            encoding_rs::EncoderResult::InputEmpty => break,
            encoding_rs::EncoderResult::OutputFull => {}
            encoding_rs::EncoderResult::Unmappable(ch) => {
                let _ = write!(out, "%26%23{}%3B", ch as u32);
            }
        }
    }
    out
}

/// Encodes `(name, value)` pairs as `application/x-www-form-urlencoded` using `charset`
/// (WHATWG URL §5.2 & HTML §4.10.21.5).
pub(crate) fn form_urlencode_pairs_with_charset(
    pairs: &[(String, String)],
    charset: &str,
) -> String {
    use std::fmt::Write;
    let enc = output_encoding_for_charset(charset);
    let push_form_bytes = |bytes: &[u8], out: &mut String| {
        for &b in bytes {
            if b == b' ' {
                out.push('+');
            } else if b.is_ascii_alphanumeric() || matches!(b, b'*' | b'-' | b'.' | b'_') {
                out.push(b as char);
            } else {
                let _ = write!(out, "%{b:02X}");
            }
        }
    };
    let encode_component = |s: &str, out: &mut String| {
        if enc == encoding_rs::UTF_8 {
            push_form_bytes(s.as_bytes(), out);
            return;
        }
        let mut encoder = enc.new_encoder();
        let mut buf = [0u8; 256];
        let mut src = s;
        loop {
            let (res, read, written) =
                encoder.encode_from_utf8_without_replacement(src, &mut buf, true);
            if written > 0 {
                push_form_bytes(&buf[..written], out);
            }
            src = &src[read..];
            match res {
                encoding_rs::EncoderResult::InputEmpty => break,
                encoding_rs::EncoderResult::OutputFull => {}
                encoding_rs::EncoderResult::Unmappable(ch) => {
                    let _ = write!(out, "%26%23{}%3B", ch as u32);
                }
            }
        }
    };
    let mut out = String::new();
    for (idx, (k, v)) in pairs.iter().enumerate() {
        if idx > 0 {
            out.push('&');
        }
        encode_component(k, &mut out);
        out.push('=');
        encode_component(v, &mut out);
    }
    out
}

/// Parses `raw` relative to `base` using `charset` as the WHATWG URL query encoding override
/// (used by `<a>` / `<area>` `HTMLHyperlinkElementUtils`).
/// WHATWG URL §4.3 post-parse normalization:
/// 1) Normalize Windows drive letter `|` to `:` in `file:` URLs (`file:///w|/m` -> `file:///w:/m`).
/// 2) Percent-encode a trailing ASCII space in opaque paths before `?`/`#` (`non-special:opaque  ?hi`).
fn normalize_file_drive_pipe(mut u: Url) -> Url {
    if u.scheme() == "file" && (u.host_str().is_none() || u.host_str() == Some("")) {
        let p = u.path();
        let bytes = p.as_bytes();
        if bytes.len() >= 3
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && bytes[2] == b'|'
            && (bytes.len() == 3 || bytes[3] == b'/')
        {
            let mut fixed = String::with_capacity(p.len());
            fixed.push('/');
            fixed.push(bytes[1] as char);
            fixed.push(':');
            fixed.push_str(&p[3..]);
            u.set_path(&fixed);
        }
    }
    if u.cannot_be_a_base() {
        let p = u.path();
        if let Some(prefix) = p.strip_suffix(' ') {
            let mut rebuilt = format!("{}:{prefix}%20", u.scheme());
            if let Some(q) = u.query() {
                rebuilt.push('?');
                rebuilt.push_str(q);
            }
            if let Some(f) = u.fragment() {
                rebuilt.push('#');
                rebuilt.push_str(f);
            }
            if let Ok(reparsed) = Url::parse(&rebuilt) {
                return reparsed;
            }
        }
    }
    u
}

pub(crate) fn parse_url_with_charset(raw: &str, base: &Url, charset: &str) -> Option<Url> {
    let cleaned = strip_ascii_tab_lf(raw);
    let trimmed = cleaned.trim_matches(|c: char| (c as u32) <= 0x20);
    let enc = output_encoding_for_charset(charset);
    if enc != encoding_rs::UTF_8 {
        // Locate first `?` occurring before any `#`.
        let hash_pos = trimmed.find('#').unwrap_or(trimmed.len());
        if let Some(q_pos) = trimmed[..hash_pos].find('?') {
            let before_q = &trimmed[..q_pos];
            let raw_q = &trimmed[q_pos + 1..hash_pos];
            let hash_part = &trimmed[hash_pos..];
            if let Ok(base_parsed) = base.join(before_q) {
                if is_special_scheme(base_parsed.scheme()) {
                    let encoded_q = percent_encode_query_with_charset(raw_q, charset, true);
                    let rebuilt = format!("{before_q}?{encoded_q}{hash_part}");
                    if let Ok(u) = base.join(&rebuilt) {
                        return Some(normalize_file_drive_pipe(u));
                    }
                }
            }
        }
    }
    base.join(trimmed).ok().map(normalize_file_drive_pipe)
}

pub(crate) fn strip_ascii_tab_lf(s: &str) -> String {
    s.chars()
        .filter(|&c| !matches!(c, '\t' | '\n' | '\r'))
        .collect()
}

fn is_special_scheme(scheme: &str) -> bool {
    matches!(scheme, "http" | "https" | "ws" | "wss" | "ftp" | "file")
}

/// Parses raw `application/x-www-form-urlencoded` bytes WITHOUT stripping a leading `?`
/// (used for `URL.query()` and `Request/Response.formData()`).
pub(crate) fn parse_query_pairs_raw(query: &[u8]) -> Vec<(String, String)> {
    if query.is_empty() {
        return Vec::new();
    }
    url::form_urlencoded::parse(query)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// Parses a `URLSearchParams(string)` initializer (strips at most one leading `?`).
fn parse_query_pairs_from_input(query: &str) -> Vec<(String, String)> {
    let stripped = query.strip_prefix('?').unwrap_or(query);
    parse_query_pairs_raw(stripped.as_bytes())
}

fn serialize_query_pairs(pairs: &[(String, String)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        ser.append_pair(k, v);
    }
    ser.finish()
}

/// WHATWG URL §6: when clearing query on an opaque-path URL whose path ends in ASCII space,
/// percent-encode the last trailing space so serialization preserves all trailing spaces.
fn opaque_trailing_space_prefix(u: &Url) -> Option<String> {
    if u.cannot_be_a_base() {
        u.path().strip_suffix(' ').map(str::to_string)
    } else {
        None
    }
}

fn restore_opaque_trailing_space(u: &mut Url, trailing_prefix: Option<String>) {
    if u.cannot_be_a_base() && u.query().is_none() {
        if let Some(prefix) = trailing_prefix {
            let mut rebuilt = format!("{}:{prefix}%20", u.scheme());
            if let Some(frag) = u.fragment() {
                rebuilt.push('#');
                rebuilt.push_str(frag);
            }
            if let Ok(reparsed) = Url::parse(&rebuilt) {
                *u = reparsed;
            }
        }
    }
}

fn sync_owner_url_from_params(web_apis: &SharedWebApis, sp_h: HeapHandle) {
    let mut st = web_apis.borrow_mut();
    let Some(entry) = st.search_params.get(&sp_h) else {
        return;
    };
    let Some(owner) = entry.owner_url else {
        return;
    };
    let encoded = serialize_query_pairs(&entry.pairs);
    if let Some(u_entry) = st.url_objects.get_mut(&owner) {
        if encoded.is_empty() {
            let trailing = opaque_trailing_space_prefix(&u_entry.url);
            u_entry.url.set_query(None);
            restore_opaque_trailing_space(&mut u_entry.url, trailing);
        } else {
            u_entry.url.set_query(Some(&encoded));
        }
    }
}

fn alloc_live_search_params_iterator(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    sp_h: HeapHandle,
    mode: u8, // 0 = entries, 1 = keys, 2 = values
) -> HeapHandle {
    let iter = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.iterator_proto),
    ));
    let idx_cell = Rc::new(std::cell::Cell::new(0usize));
    let wa = Rc::clone(web_apis);
    let next_fn = vm.alloc_host_fn("next", move |vm, _, _| {
        let cur = idx_cell.get();
        let pair_opt = wa
            .borrow()
            .search_params
            .get(&sp_h)
            .and_then(|e| e.pairs.get(cur).cloned());
        let res = vm.alloc_ordinary_object();
        let k_val = vm.heap.intern_key("value");
        let k_done = vm.heap.intern_key("done");
        if let Some((k, v)) = pair_opt {
            idx_cell.set(cur + 1);
            let out_val = match mode {
                1 => JsValue::from_string(vm.heap.alloc_string(&k)),
                2 => JsValue::from_string(vm.heap.alloc_string(&v)),
                _ => {
                    let pair = vm.alloc_array();
                    let kv = JsValue::from_string(vm.heap.alloc_string(&k));
                    let vv = JsValue::from_string(vm.heap.alloc_string(&v));
                    vm.heap.object_push_element(pair, kv);
                    vm.heap.object_push_element(pair, vv);
                    JsValue::from_object(pair)
                }
            };
            vm.heap
                .object_define_data(res, k_val, out_val, PropertyAttrs::DEFAULT, &mut vm.shapes);
            vm.heap.object_define_data(
                res,
                k_done,
                JsValue::FALSE,
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
        } else {
            vm.heap.object_define_data(
                res,
                k_val,
                JsValue::UNDEFINED,
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
            vm.heap.object_define_data(
                res,
                k_done,
                JsValue::TRUE,
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
        }
        Ok(JsValue::from_object(res))
    });
    let k_next = vm.heap.intern_key("next");
    vm.heap.object_define_data(
        iter,
        k_next,
        JsValue::from_object(next_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    iter
}

fn alloc_url_instance(vm: &mut JsVm, web_apis: &SharedWebApis, parsed: Url) -> HeapHandle {
    let parsed = normalize_file_drive_pipe(parsed);
    let (up, spp) = {
        let st = web_apis.borrow();
        (
            st.protos.url_proto.unwrap_or(vm.object_proto),
            st.protos.search_params_proto.unwrap_or(vm.object_proto),
        )
    };
    let sp_obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(spp)));
    let url_obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(up)));
    let pairs = parse_query_pairs_raw(parsed.query().unwrap_or("").as_bytes());
    let mut st = web_apis.borrow_mut();
    st.search_params.insert(
        sp_obj,
        SearchParamsEntry {
            pairs,
            owner_url: Some(url_obj),
        },
    );
    st.url_objects.insert(
        url_obj,
        UrlEntry {
            url: parsed,
            search_params: sp_obj,
        },
    );
    url_obj
}

pub(crate) fn apply_url_host_setter(u: &mut Url, raw: &str, allow_port: bool) {
    let cleaned_full = strip_ascii_tab_lf(raw);
    if u.cannot_be_a_base() {
        return;
    }
    let special = is_special_scheme(u.scheme());
    // WHATWG URL §4.3 "host state": terminate input at `/`, `?`, `#` (and `\` for special schemes).
    let term_pos = cleaned_full
        .find(|c: char| matches!(c, '/' | '?' | '#') || (special && c == '\\'))
        .unwrap_or(cleaned_full.len());
    let cleaned = &cleaned_full[..term_pos];

    // Reject U+0000 in all host setters and forbidden host code points in special schemes.
    if cleaned.contains('\0') {
        return;
    }
    if special
        && cleaned.bytes().any(|b| {
            b <= 0x1F
                || b == 0x7F
                || matches!(
                    b,
                    b' ' | b'#' | b'/' | b'<' | b'>' | b'?' | b'@' | b'\\' | b'^' | b'|'
                )
        })
    {
        return;
    }
    if !allow_port && !cleaned.starts_with('[') && cleaned.contains(':') {
        return;
    }
    if allow_port && cleaned.starts_with('[') {
        if let Some(close_bracket) = cleaned.find(']') {
            let host_part = &cleaned[..=close_bracket];
            let rest = &cleaned[close_bracket + 1..];
            if rest.is_empty() {
                let _ = u.set_host(Some(host_part));
                return;
            }
            if let Some(port_part) = rest.strip_prefix(':') {
                if u.set_host(Some(host_part)).is_ok() && !port_part.is_empty() {
                    apply_url_port_setter(u, port_part);
                }
            }
            return;
        }
    }
    if allow_port && !cleaned.starts_with('[') {
        if let Some(colon) = cleaned.rfind(':') {
            let host_part = &cleaned[..colon];
            let port_part = &cleaned[colon + 1..];
            if special && u.scheme() != "file" && host_part.is_empty() {
                return;
            }
            if !special && host_part.is_empty() && !port_part.is_empty() {
                return;
            }
            let set_res = if host_part.is_empty() {
                if u.scheme() == "file" {
                    u.set_host(None)
                } else {
                    u.set_host(Some(""))
                }
            } else {
                u.set_host(Some(host_part))
            };
            if set_res.is_ok() && !port_part.is_empty() {
                apply_url_port_setter(u, port_part);
            }
            return;
        }
    }
    if cleaned.is_empty() {
        if special {
            if u.scheme() == "file" {
                let _ = u.set_host(None);
            }
        } else if u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.port().is_none()
        {
            let _ = u.set_host(Some(""));
        }
        return;
    }
    let _ = u.set_host(Some(cleaned));
}

pub(crate) fn apply_url_port_setter(u: &mut Url, raw: &str) {
    if u.cannot_be_a_base() || u.host_str().is_none() || u.scheme() == "file" {
        return;
    }
    let cleaned = strip_ascii_tab_lf(raw);
    if cleaned.is_empty() {
        let _ = u.set_port(None);
        return;
    }
    let digits_end = cleaned.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits_end == 0 {
        return;
    }
    if let Ok(port) = cleaned[..digits_end].parse::<u16>() {
        let _ = u.set_port(Some(port));
    }
}

fn install_url_and_search_params(vm: &mut JsVm, web_apis: &SharedWebApis) {
    // --- URLSearchParams ---
    let spp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(spp).unwrap().is_prototype = true;
    web_apis.borrow_mut().protos.search_params_proto = Some(spp);

    let wa = Rc::clone(web_apis);
    define_host_method(vm, spp, "append", move |vm, this_val, args| {
        let h = require_search_params(vm, &wa, this_val)?;
        let k = vm.value_to_rust_string(arg(args, 0));
        let v = vm.value_to_rust_string(arg(args, 1));
        if let Some(e) = wa.borrow_mut().search_params.get_mut(&h) {
            e.pairs.push((k, v));
        }
        sync_owner_url_from_params(&wa, h);
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, spp, "set", move |vm, this_val, args| {
        let h = require_search_params(vm, &wa, this_val)?;
        let k = vm.value_to_rust_string(arg(args, 0));
        let v = vm.value_to_rust_string(arg(args, 1));
        if let Some(e) = wa.borrow_mut().search_params.get_mut(&h) {
            let mut replaced = false;
            let mut next = Vec::with_capacity(e.pairs.len() + 1);
            for (ek, ev) in e.pairs.drain(..) {
                if ek == k {
                    if !replaced {
                        next.push((k.clone(), v.clone()));
                        replaced = true;
                    }
                } else {
                    next.push((ek, ev));
                }
            }
            if !replaced {
                next.push((k, v));
            }
            e.pairs = next;
        }
        sync_owner_url_from_params(&wa, h);
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, spp, "delete", move |vm, this_val, args| {
        let h = require_search_params(vm, &wa, this_val)?;
        let k = vm.value_to_rust_string(arg(args, 0));
        let val_filter = if args.len() > 1 && !arg(args, 1).is_undefined() {
            Some(vm.value_to_rust_string(arg(args, 1)))
        } else {
            None
        };
        if let Some(e) = wa.borrow_mut().search_params.get_mut(&h) {
            e.pairs.retain(|(ek, ev)| {
                if ek != &k {
                    return true;
                }
                if let Some(ref vf) = val_filter {
                    ev != vf
                } else {
                    false
                }
            });
        }
        sync_owner_url_from_params(&wa, h);
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, spp, "get", move |vm, this_val, args| {
        let h = require_search_params(vm, &wa, this_val)?;
        let k = vm.value_to_rust_string(arg(args, 0));
        let found = wa.borrow().search_params.get(&h).and_then(|e| {
            e.pairs
                .iter()
                .find(|(ek, _)| ek == &k)
                .map(|(_, v)| v.clone())
        });
        Ok(match found {
            Some(s) => JsValue::from_string(vm.heap.alloc_string(&s)),
            None => JsValue::NULL,
        })
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, spp, "getAll", move |vm, this_val, args| {
        let h = require_search_params(vm, &wa, this_val)?;
        let k = vm.value_to_rust_string(arg(args, 0));
        let vals: Vec<String> = wa
            .borrow()
            .search_params
            .get(&h)
            .map(|e| {
                e.pairs
                    .iter()
                    .filter(|(ek, _)| ek == &k)
                    .map(|(_, v)| v.clone())
                    .collect()
            })
            .unwrap_or_default();
        let arr = vm.alloc_array();
        for s in vals {
            let v = JsValue::from_string(vm.heap.alloc_string(&s));
            vm.heap.object_push_element(arr, v);
        }
        Ok(JsValue::from_object(arr))
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, spp, "has", move |vm, this_val, args| {
        let h = require_search_params(vm, &wa, this_val)?;
        let k = vm.value_to_rust_string(arg(args, 0));
        let val_filter = if args.len() > 1 && !arg(args, 1).is_undefined() {
            Some(vm.value_to_rust_string(arg(args, 1)))
        } else {
            None
        };
        let has = wa.borrow().search_params.get(&h).is_some_and(|e| {
            e.pairs
                .iter()
                .any(|(ek, ev)| ek == &k && val_filter.as_ref().is_none_or(|vf| ev == vf))
        });
        Ok(JsValue::from_bool(has))
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, spp, "sort", move |vm, this_val, _| {
        let h = require_search_params(vm, &wa, this_val)?;
        if let Some(e) = wa.borrow_mut().search_params.get_mut(&h) {
            // WHATWG URL §6.1: sort stably by UTF-16 code units.
            e.pairs
                .sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
        }
        sync_owner_url_from_params(&wa, h);
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, spp, "toString", move |vm, this_val, _| {
        let h = require_search_params(vm, &wa, this_val)?;
        let s = wa
            .borrow()
            .search_params
            .get(&h)
            .map(|e| serialize_query_pairs(&e.pairs))
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, spp, "size", move |vm, this_val, _| {
        let h = require_search_params(vm, &wa, this_val)?;
        let n = wa
            .borrow()
            .search_params
            .get(&h)
            .map_or(0, |e| e.pairs.len());
        Ok(JsValue::from_i32(n as i32))
    });

    let wa = Rc::clone(web_apis);
    let entries_fn = vm.alloc_host_fn("entries", move |vm, this_val, _| {
        let h = require_search_params(vm, &wa, this_val)?;
        Ok(JsValue::from_object(alloc_live_search_params_iterator(
            vm, &wa, h, 0,
        )))
    });
    let k_entries = vm.heap.intern_key("entries");
    vm.heap.object_define_data(
        spp,
        k_entries,
        JsValue::from_object(entries_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let sym_key = vm.key_symbol_iterator;
    vm.heap.object_define_data(
        spp,
        sym_key,
        JsValue::from_object(entries_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    let wa = Rc::clone(web_apis);
    define_host_method(vm, spp, "keys", move |vm, this_val, _| {
        let h = require_search_params(vm, &wa, this_val)?;
        Ok(JsValue::from_object(alloc_live_search_params_iterator(
            vm, &wa, h, 1,
        )))
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, spp, "values", move |vm, this_val, _| {
        let h = require_search_params(vm, &wa, this_val)?;
        Ok(JsValue::from_object(alloc_live_search_params_iterator(
            vm, &wa, h, 2,
        )))
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, spp, "forEach", move |vm, this_val, args| {
        let h = require_search_params(vm, &wa, this_val)?;
        let cb = arg(args, 0);
        let this_arg = arg(args, 1);
        let mut idx = 0usize;
        loop {
            let pair_opt = wa
                .borrow()
                .search_params
                .get(&h)
                .and_then(|e| e.pairs.get(idx).cloned());
            let Some((k, v)) = pair_opt else {
                break;
            };
            idx += 1;
            let kv = JsValue::from_string(vm.heap.alloc_string(&k));
            let vv = JsValue::from_string(vm.heap.alloc_string(&v));
            vm.call_value(cb, this_arg, &[vv, kv, this_val])?;
        }
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    let sp_ctor = vm.alloc_host_fn("URLSearchParams", move |vm, _this, args| {
        let init = arg(args, 0);
        let mut pairs = Vec::new();
        if !init.is_undefined() && !init.is_null() {
            if let Some(ih) = init.as_object() {
                let iter_method = vm
                    .get_property_by_key(ih, vm.key_symbol_iterator)
                    .ok()
                    .filter(|v| !v.is_undefined() && !v.is_null());
                if let Some(existing) = wa.borrow().search_params.get(&ih).cloned() {
                    if iter_method.is_some() {
                        // Use iterable protocol if user overrode @@iterator, or fast-path default.
                        let items = vm.collect_iterable(init)?;
                        for item in items {
                            let sub = vm.collect_iterable(item)?;
                            if sub.len() != 2 {
                                return Err(
                                    vm.type_error("URLSearchParams pair must have 2 elements")
                                );
                            }
                            let k = vm.value_to_rust_string(sub[0]);
                            let v = vm.value_to_rust_string(sub[1]);
                            pairs.push((k, v));
                        }
                    } else {
                        pairs = existing.pairs;
                    }
                } else if iter_method.is_some() {
                    let items = vm.collect_iterable(init)?;
                    for item in items {
                        let sub = vm.collect_iterable(item)?;
                        if sub.len() != 2 {
                            return Err(vm.type_error("URLSearchParams pair must have 2 elements"));
                        }
                        let k = vm.value_to_rust_string(sub[0]);
                        let v = vm.value_to_rust_string(sub[1]);
                        pairs.push((k, v));
                    }
                } else {
                    for k_str in vm.own_enumerable_keys_including_elements(ih) {
                        let key = vm.heap.intern_key(&k_str);
                        let v_val = vm.get_property_by_key(ih, key)?;
                        let v_str = vm.value_to_rust_string(v_val);
                        pairs.push((k_str, v_str));
                    }
                }
            } else {
                let s = vm.value_to_rust_string(init);
                pairs = parse_query_pairs_from_input(&s);
            }
        }
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(spp)));
        wa.borrow_mut().search_params.insert(
            obj,
            SearchParamsEntry {
                pairs,
                owner_url: None,
            },
        );
        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, sp_ctor, spp);
    define_global(vm, "URLSearchParams", JsValue::from_object(sp_ctor));

    // --- URL ---
    let up = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(up).unwrap().is_prototype = true;
    web_apis.borrow_mut().protos.url_proto = Some(up);

    let wa = Rc::clone(web_apis);
    define_host_accessor(
        vm,
        up,
        "href",
        {
            let wa = Rc::clone(&wa);
            move |vm, this_val, _| {
                let h = require_url(vm, &wa, this_val)?;
                let s = wa
                    .borrow()
                    .url_objects
                    .get(&h)
                    .map(|u| u.url.as_str().to_string())
                    .unwrap_or_default();
                Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
            }
        },
        move |vm, this_val, args| {
            let h = require_url(vm, &wa, this_val)?;
            let raw = vm.value_to_rust_string(arg(args, 0));
            let parsed =
                Url::parse(&raw).map_err(|_| vm.type_error(format!("Invalid URL: {raw}")))?;
            let new_pairs = parse_query_pairs_raw(parsed.query().unwrap_or("").as_bytes());
            let mut st = wa.borrow_mut();
            let sp_h = st.url_objects.get(&h).map(|u| u.search_params);
            if let Some(u) = st.url_objects.get_mut(&h) {
                u.url = parsed;
            }
            if let Some(sp_h) = sp_h {
                if let Some(sp) = st.search_params.get_mut(&sp_h) {
                    sp.pairs = new_pairs;
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, up, "origin", move |vm, this_val, _| {
        let h = require_url(vm, &wa, this_val)?;
        let s = wa
            .borrow()
            .url_objects
            .get(&h)
            .map(|u| origin_of_url(&u.url))
            .unwrap_or_else(|| "null".to_string());
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });

    let wa = Rc::clone(web_apis);
    define_host_accessor(
        vm,
        up,
        "protocol",
        {
            let wa = Rc::clone(&wa);
            move |vm, this_val, _| {
                let h = require_url(vm, &wa, this_val)?;
                let s = wa
                    .borrow()
                    .url_objects
                    .get(&h)
                    .map(|u| format!("{}:", u.url.scheme()))
                    .unwrap_or_default();
                Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
            }
        },
        move |vm, this_val, args| {
            let h = require_url(vm, &wa, this_val)?;
            let raw = vm.value_to_rust_string(arg(args, 0));
            let cleaned = strip_ascii_tab_lf(&raw);
            let scheme = cleaned.split(':').next().unwrap_or("");
            if !scheme.is_empty() {
                if let Some(u) = wa.borrow_mut().url_objects.get_mut(&h) {
                    let _ = u.url.set_scheme(scheme);
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let wa = Rc::clone(web_apis);
    define_host_accessor(
        vm,
        up,
        "username",
        {
            let wa = Rc::clone(&wa);
            move |vm, this_val, _| {
                let h = require_url(vm, &wa, this_val)?;
                let s = wa
                    .borrow()
                    .url_objects
                    .get(&h)
                    .map(|u| u.url.username().to_string())
                    .unwrap_or_default();
                Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
            }
        },
        move |vm, this_val, args| {
            let h = require_url(vm, &wa, this_val)?;
            let val = vm.value_to_rust_string(arg(args, 0));
            if let Some(u) = wa.borrow_mut().url_objects.get_mut(&h) {
                if !u.url.cannot_be_a_base()
                    && u.url.host_str().is_some()
                    && u.url.scheme() != "file"
                {
                    let _ = u.url.set_username(&val);
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let wa = Rc::clone(web_apis);
    define_host_accessor(
        vm,
        up,
        "password",
        {
            let wa = Rc::clone(&wa);
            move |vm, this_val, _| {
                let h = require_url(vm, &wa, this_val)?;
                let s = wa
                    .borrow()
                    .url_objects
                    .get(&h)
                    .and_then(|u| u.url.password().map(str::to_string))
                    .unwrap_or_default();
                Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
            }
        },
        move |vm, this_val, args| {
            let h = require_url(vm, &wa, this_val)?;
            let val = vm.value_to_rust_string(arg(args, 0));
            if let Some(u) = wa.borrow_mut().url_objects.get_mut(&h) {
                if !u.url.cannot_be_a_base()
                    && u.url.host_str().is_some()
                    && u.url.scheme() != "file"
                {
                    let _ = u
                        .url
                        .set_password(if val.is_empty() { None } else { Some(&val) });
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let wa = Rc::clone(web_apis);
    define_host_accessor(
        vm,
        up,
        "hostname",
        {
            let wa = Rc::clone(&wa);
            move |vm, this_val, _| {
                let h = require_url(vm, &wa, this_val)?;
                let s = wa
                    .borrow()
                    .url_objects
                    .get(&h)
                    .and_then(|u| u.url.host_str().map(str::to_string))
                    .unwrap_or_default();
                Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
            }
        },
        move |vm, this_val, args| {
            let h = require_url(vm, &wa, this_val)?;
            let val = vm.value_to_rust_string(arg(args, 0));
            if let Some(u) = wa.borrow_mut().url_objects.get_mut(&h) {
                apply_url_host_setter(&mut u.url, &val, false);
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let wa = Rc::clone(web_apis);
    define_host_accessor(
        vm,
        up,
        "port",
        {
            let wa = Rc::clone(&wa);
            move |vm, this_val, _| {
                let h = require_url(vm, &wa, this_val)?;
                let s = wa
                    .borrow()
                    .url_objects
                    .get(&h)
                    .and_then(|u| u.url.port().map(|p| p.to_string()))
                    .unwrap_or_default();
                Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
            }
        },
        move |vm, this_val, args| {
            let h = require_url(vm, &wa, this_val)?;
            let val = vm.value_to_rust_string(arg(args, 0));
            if let Some(u) = wa.borrow_mut().url_objects.get_mut(&h) {
                apply_url_port_setter(&mut u.url, &val);
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let wa = Rc::clone(web_apis);
    define_host_accessor(
        vm,
        up,
        "host",
        {
            let wa = Rc::clone(&wa);
            move |vm, this_val, _| {
                let h = require_url(vm, &wa, this_val)?;
                let s = wa
                    .borrow()
                    .url_objects
                    .get(&h)
                    .map(|u| match (u.url.host_str(), u.url.port()) {
                        (Some(host), Some(port)) => format!("{host}:{port}"),
                        (Some(host), None) => host.to_string(),
                        _ => String::new(),
                    })
                    .unwrap_or_default();
                Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
            }
        },
        move |vm, this_val, args| {
            let h = require_url(vm, &wa, this_val)?;
            let val = vm.value_to_rust_string(arg(args, 0));
            if let Some(u) = wa.borrow_mut().url_objects.get_mut(&h) {
                apply_url_host_setter(&mut u.url, &val, true);
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let wa = Rc::clone(web_apis);
    define_host_accessor(
        vm,
        up,
        "pathname",
        {
            let wa = Rc::clone(&wa);
            move |vm, this_val, _| {
                let h = require_url(vm, &wa, this_val)?;
                let s = wa
                    .borrow()
                    .url_objects
                    .get(&h)
                    .map(|u| u.url.path().to_string())
                    .unwrap_or_default();
                Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
            }
        },
        move |vm, this_val, args| {
            let h = require_url(vm, &wa, this_val)?;
            let val = strip_ascii_tab_lf(&vm.value_to_rust_string(arg(args, 0)));
            if let Some(u) = wa.borrow_mut().url_objects.get_mut(&h) {
                if !u.url.cannot_be_a_base() {
                    u.url.set_path(&val);
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let wa = Rc::clone(web_apis);
    define_host_accessor(
        vm,
        up,
        "search",
        {
            let wa = Rc::clone(&wa);
            move |vm, this_val, _| {
                let h = require_url(vm, &wa, this_val)?;
                let s = wa
                    .borrow()
                    .url_objects
                    .get(&h)
                    .and_then(|u| u.url.query())
                    .filter(|q| !q.is_empty())
                    .map(|q| format!("?{q}"))
                    .unwrap_or_default();
                Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
            }
        },
        move |vm, this_val, args| {
            let h = require_url(vm, &wa, this_val)?;
            let raw = vm.value_to_rust_string(arg(args, 0));
            let trimmed = raw.strip_prefix('?').unwrap_or(&raw);
            let cleaned = strip_ascii_tab_lf(trimmed);
            let mut st = wa.borrow_mut();
            let sp_h = st.url_objects.get(&h).map(|u| u.search_params);
            let final_query = if let Some(u) = st.url_objects.get_mut(&h) {
                if raw.is_empty() {
                    let trailing = opaque_trailing_space_prefix(&u.url);
                    u.url.set_query(None);
                    restore_opaque_trailing_space(&mut u.url, trailing);
                } else if trimmed.is_empty() {
                    u.url.set_query(Some(""));
                } else {
                    u.url.set_query(Some(&cleaned));
                }
                u.url.query().unwrap_or("").to_string()
            } else {
                String::new()
            };
            let new_pairs = parse_query_pairs_raw(final_query.as_bytes());
            if let Some(sp_h) = sp_h {
                if let Some(sp) = st.search_params.get_mut(&sp_h) {
                    sp.pairs = new_pairs;
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let wa = Rc::clone(web_apis);
    define_host_accessor(
        vm,
        up,
        "hash",
        {
            let wa = Rc::clone(&wa);
            move |vm, this_val, _| {
                let h = require_url(vm, &wa, this_val)?;
                let s = wa
                    .borrow()
                    .url_objects
                    .get(&h)
                    .and_then(|u| u.url.fragment())
                    .filter(|f| !f.is_empty())
                    .map(|f| format!("#{f}"))
                    .unwrap_or_default();
                Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
            }
        },
        move |vm, this_val, args| {
            let h = require_url(vm, &wa, this_val)?;
            let raw = vm.value_to_rust_string(arg(args, 0));
            let trimmed = raw.strip_prefix('#').unwrap_or(&raw);
            let cleaned = strip_ascii_tab_lf(trimmed);
            if let Some(u) = wa.borrow_mut().url_objects.get_mut(&h) {
                if raw.is_empty() {
                    let trailing = opaque_trailing_space_prefix(&u.url);
                    u.url.set_fragment(None);
                    restore_opaque_trailing_space(&mut u.url, trailing);
                } else {
                    u.url.set_fragment(Some(&cleaned));
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, up, "searchParams", move |vm, this_val, _| {
        let h = require_url(vm, &wa, this_val)?;
        let sp = wa
            .borrow()
            .url_objects
            .get(&h)
            .map(|u| u.search_params)
            .ok_or_else(|| vm.type_error("Invalid URL"))?;
        Ok(JsValue::from_object(sp))
    });

    let wa = Rc::clone(web_apis);
    let to_str = vm.alloc_host_fn("toString", move |vm, this_val, _| {
        let h = require_url(vm, &wa, this_val)?;
        let s = wa
            .borrow()
            .url_objects
            .get(&h)
            .map(|u| u.url.as_str().to_string())
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });
    let k_ts = vm.heap.intern_key("toString");
    let k_tj = vm.heap.intern_key("toJSON");
    vm.heap.object_define_data(
        up,
        k_ts,
        JsValue::from_object(to_str),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        up,
        k_tj,
        JsValue::from_object(to_str),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    let wa = Rc::clone(web_apis);
    let url_ctor = vm.alloc_host_fn("URL", move |vm, _this, args| {
        if args.is_empty() {
            return Err(vm.type_error("URL constructor requires at least 1 argument"));
        }
        let raw = vm.to_js_string(arg(args, 0))?;
        let parsed = if args.len() > 1 && !arg(args, 1).is_undefined() {
            let base_raw = vm.to_js_string(arg(args, 1))?;
            let base = Url::parse(&base_raw)
                .map_err(|_| vm.type_error(format!("Invalid base URL: {base_raw}")))?;
            base.join(&raw)
                .map_err(|_| vm.type_error(format!("Invalid URL: {raw}")))?
        } else {
            Url::parse(&raw).map_err(|_| vm.type_error(format!("Invalid URL: {raw}")))?
        };
        Ok(JsValue::from_object(alloc_url_instance(vm, &wa, parsed)))
    });
    vm.set_fn_length(url_ctor, 1);
    {
        let tag_v = JsValue::from_string(vm.heap.alloc_string("URL"));
        let k_tag = vm.key_symbol_to_string_tag;
        vm.heap.object_define_data(
            up,
            k_tag,
            tag_v,
            PropertyAttrs::CONFIGURABLE,
            &mut vm.shapes,
        );
        let sp_tag_v = JsValue::from_string(vm.heap.alloc_string("URLSearchParams"));
        vm.heap.object_define_data(
            spp,
            k_tag,
            sp_tag_v,
            PropertyAttrs::CONFIGURABLE,
            &mut vm.shapes,
        );
        for (method, arity) in [
            ("append", 2u32),
            ("set", 2),
            ("delete", 1),
            ("get", 1),
            ("getAll", 1),
            ("has", 1),
            ("forEach", 1),
        ] {
            let km = vm.heap.intern_key(method);
            if let Some(mh) = vm
                .heap
                .object_get_own(spp, km, &vm.shapes)
                .and_then(|p| p.value.as_object())
            {
                vm.set_fn_length(mh, arity);
            }
        }
    }
    {
        let wa_p = Rc::clone(web_apis);
        define_host_method(vm, url_ctor, "parse", move |vm, _, args| {
            if args.is_empty() {
                return Err(vm.type_error("URL.parse requires at least 1 argument"));
            }
            let raw = vm.to_js_string(arg(args, 0))?;
            let parsed = if args.len() > 1 && !arg(args, 1).is_undefined() {
                let base_raw = vm.to_js_string(arg(args, 1))?;
                Url::parse(&base_raw).and_then(|b| b.join(&raw)).ok()
            } else {
                Url::parse(&raw).ok()
            };
            Ok(match parsed {
                Some(u) => JsValue::from_object(alloc_url_instance(vm, &wa_p, u)),
                None => JsValue::NULL,
            })
        });
        let kp = vm.heap.intern_key("parse");
        if let Some(fh) = vm
            .heap
            .object_get_own(url_ctor, kp, &vm.shapes)
            .and_then(|p| p.value.as_object())
        {
            vm.set_fn_length(fh, 1);
        }
    }
    define_host_method(vm, url_ctor, "canParse", |vm, _, args| {
        if args.is_empty() {
            return Err(vm.type_error("URL.canParse requires at least 1 argument"));
        }
        let raw = vm.to_js_string(arg(args, 0))?;
        let ok = if args.len() > 1 && !arg(args, 1).is_undefined() {
            let base_raw = vm.to_js_string(arg(args, 1))?;
            Url::parse(&base_raw).and_then(|b| b.join(&raw)).is_ok()
        } else {
            Url::parse(&raw).is_ok()
        };
        Ok(JsValue::from_bool(ok))
    });
    {
        let kc = vm.heap.intern_key("canParse");
        if let Some(fh) = vm
            .heap
            .object_get_own(url_ctor, kc, &vm.shapes)
            .and_then(|p| p.value.as_object())
        {
            vm.set_fn_length(fh, 1);
        }
    }
    {
        let wa_c = Rc::clone(web_apis);
        define_host_method(vm, url_ctor, "createObjectURL", move |vm, _, args| {
            let Some(bh) = arg(args, 0).as_object() else {
                return Err(
                    vm.type_error("URL.createObjectURL requires a Blob or MediaSource argument")
                );
            };
            let (blob_bytes, validators) = {
                let st = wa_c.borrow();
                (
                    st.blobs.get(&bh).map(|b| b.bytes.clone()),
                    st.object_url_validators.clone(),
                )
            };
            let is_valid_obj = blob_bytes.is_none() && validators.iter().any(|v| v(bh));
            if blob_bytes.is_none() && !is_valid_obj {
                return Err(
                    vm.type_error("URL.createObjectURL argument must be a Blob or MediaSource")
                );
            }
            let mut st = wa_c.borrow_mut();
            let id = st.next_object_url_id;
            st.next_object_url_id = st.next_object_url_id.wrapping_add(1).max(1);
            let origin = origin_of_url(&st.location_url);
            let url = format!("blob:{origin}/{id}");
            if let Some(bytes) = blob_bytes {
                st.object_urls.insert(url.clone(), bytes);
            } else {
                st.object_url_objects.insert(url.clone(), bh);
            }
            Ok(JsValue::from_string(vm.heap.alloc_string(&url)))
        });
    }
    {
        let wa_r = Rc::clone(web_apis);
        define_host_method(vm, url_ctor, "revokeObjectURL", move |vm, _, args| {
            let url = vm.value_to_rust_string(arg(args, 0));
            let mut st = wa_r.borrow_mut();
            st.object_urls.remove(&url);
            st.object_url_objects.remove(&url);
            Ok(JsValue::UNDEFINED)
        });
    }
    bind_ctor_proto(vm, url_ctor, up);
    define_global(vm, "URL", JsValue::from_object(url_ctor));
}

fn require_url(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible URL receiver"));
    };
    if web_apis.borrow().url_objects.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible URL receiver"))
    }
}

fn require_search_params(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible URLSearchParams receiver"));
    };
    if web_apis.borrow().search_params.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible URLSearchParams receiver"))
    }
}

// ---------------------------------------------------------------------------
// TextEncoder & TextDecoder
// ---------------------------------------------------------------------------

fn install_text_encoding(vm: &mut JsVm, web_apis: &SharedWebApis) {
    // TextEncoder
    let ep = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(ep).unwrap().is_prototype = true;
    define_host_getter(vm, ep, "encoding", move |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Incompatible TextEncoder receiver"));
        };
        let k_b = vm.heap.intern_key("__rix_text_encoder__");
        if vm.heap.object_get_own(h, k_b, &vm.shapes).is_none() {
            return Err(vm.type_error("Incompatible TextEncoder receiver"));
        }
        Ok(JsValue::from_string(vm.heap.alloc_string("utf-8")))
    });

    define_host_method(vm, ep, "encode", |vm, this_val, args| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Incompatible TextEncoder receiver"));
        };
        let k_b = vm.heap.intern_key("__rix_text_encoder__");
        if vm.heap.object_get_own(h, k_b, &vm.shapes).is_none() {
            return Err(vm.type_error("Incompatible TextEncoder receiver"));
        }
        let s = if arg(args, 0).is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(arg(args, 0))
        };
        let u8a = alloc_uint8_array(vm, s.into_bytes());
        Ok(JsValue::from_object(u8a))
    });

    define_host_method(vm, ep, "encodeInto", |vm, this_val, args| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Incompatible TextEncoder receiver"));
        };
        let k_b = vm.heap.intern_key("__rix_text_encoder__");
        if vm.heap.object_get_own(h, k_b, &vm.shapes).is_none() {
            return Err(vm.type_error("Incompatible TextEncoder receiver"));
        }
        let s = if arg(args, 0).is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(arg(args, 0))
        };
        let Some(dst_h) = arg(args, 1).as_object() else {
            return Err(vm.type_error("TextEncoder.encodeInto requires a Uint8Array"));
        };
        let Some(ObjectKind::TypedArray(ta)) = vm.heap.get_object(dst_h).map(|o| &o.kind) else {
            return Err(vm.type_error("TextEncoder.encodeInto requires a Uint8Array"));
        };
        let ta = *ta;
        if ta.kind != TypedArrayKind::Uint8 {
            return Err(vm.type_error("TextEncoder.encodeInto requires a Uint8Array"));
        }
        let off = ta.byte_offset as usize;
        // Determine available byte capacity in either ArrayBuffer or SharedArrayBuffer.
        let avail_len = vm.heap.get_object(ta.buffer).map_or(0usize, |o| {
            if let Some(shared) = o.rare.as_ref().and_then(|r| r.shared_mem.as_ref()) {
                let data_len = shared.byte_len();
                let cap = o
                    .rare
                    .as_ref()
                    .and_then(|r| r.ab_fixed_byte_len)
                    .map_or(data_len, |f| f.min(data_len));
                cap.saturating_sub(off).min(ta.length as usize)
            } else if let ObjectKind::ArrayBuffer(buf) = &o.kind {
                buf.len().saturating_sub(off).min(ta.length as usize)
            } else {
                0
            }
        });
        let mut read_units = 0usize;
        let mut out_bytes = Vec::with_capacity(avail_len.min(s.len()));
        for ch in s.chars() {
            let mut tmp = [0u8; 4];
            let encoded = ch.encode_utf8(&mut tmp).as_bytes();
            if out_bytes.len() + encoded.len() > avail_len {
                break;
            }
            out_bytes.extend_from_slice(encoded);
            read_units += ch.len_utf16();
        }
        let written = out_bytes.len();
        if written > 0 {
            if let Some(buf_obj) = vm.heap.get_object_mut(ta.buffer) {
                if let Some(shared) = buf_obj.rare.as_ref().and_then(|r| r.shared_mem.as_ref()) {
                    let mut guard = shared.data.lock().unwrap_or_else(|e| e.into_inner());
                    let end = off.saturating_add(written).min(guard.len());
                    let copy_len = end.saturating_sub(off);
                    guard[off..off + copy_len].copy_from_slice(&out_bytes[..copy_len]);
                } else if let ObjectKind::ArrayBuffer(buf) = &mut buf_obj.kind {
                    let end = off.saturating_add(written).min(buf.len());
                    let copy_len = end.saturating_sub(off);
                    buf[off..off + copy_len].copy_from_slice(&out_bytes[..copy_len]);
                }
            }
        }
        let res = vm.alloc_ordinary_object();
        let k_read = vm.heap.intern_key("read");
        let k_written = vm.heap.intern_key("written");
        vm.heap.object_define_data(
            res,
            k_read,
            JsValue::from_f64(read_units as f64),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            res,
            k_written,
            JsValue::from_f64(written as f64),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        Ok(JsValue::from_object(res))
    });

    let enc_ctor = vm.alloc_host_fn("TextEncoder", move |vm, _, _| {
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(ep)));
        let k_b = vm.heap.intern_key("__rix_text_encoder__");
        vm.heap.object_define_data(
            obj,
            k_b,
            JsValue::TRUE,
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, enc_ctor, ep);
    define_global(vm, "TextEncoder", JsValue::from_object(enc_ctor));

    // TextDecoder (full WHATWG Encoding Standard stateful decoder via encoding_rs)
    let dp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(dp).unwrap().is_prototype = true;
    web_apis.borrow_mut().protos.text_decoder_proto = Some(dp);

    let require_decoder =
        |vm: &mut JsVm, wa: &SharedWebApis, val: JsValue| -> Result<HeapHandle, JsException> {
            let Some(h) = val.as_object() else {
                return Err(vm.type_error("Incompatible TextDecoder receiver"));
            };
            if wa.borrow().text_decoders.contains_key(&h) {
                Ok(h)
            } else {
                Err(vm.type_error("Incompatible TextDecoder receiver"))
            }
        };

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, dp, "encoding", move |vm, this_val, _| {
        let h = require_decoder(vm, &wa, this_val)?;
        let name = wa
            .borrow()
            .text_decoders
            .get(&h)
            .map(|d| d.encoding.name().to_ascii_lowercase())
            .unwrap_or_else(|| "utf-8".to_string());
        Ok(JsValue::from_string(vm.heap.alloc_string(&name)))
    });

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, dp, "fatal", move |vm, this_val, _| {
        let h = require_decoder(vm, &wa, this_val)?;
        let fatal = wa.borrow().text_decoders.get(&h).is_some_and(|d| d.fatal);
        Ok(JsValue::from_bool(fatal))
    });

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, dp, "ignoreBOM", move |vm, this_val, _| {
        let h = require_decoder(vm, &wa, this_val)?;
        let ignore_bom = wa
            .borrow()
            .text_decoders
            .get(&h)
            .is_some_and(|d| d.ignore_bom);
        Ok(JsValue::from_bool(ignore_bom))
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, dp, "decode", move |vm, this_val, args| {
        let h = require_decoder(vm, &wa, this_val)?;
        let input = arg(args, 0);
        let bytes = if input.is_undefined() {
            Vec::new()
        } else {
            extract_allow_shared_buffer_source(vm, input)?
        };
        let mut stream = false;
        let opt_val = arg(args, 1);
        if !opt_val.is_undefined() && !opt_val.is_null() {
            let Some(opt_h) = opt_val.as_object() else {
                return Err(vm.type_error("TextDecoder.decode options must be an object"));
            };
            let k_stream = vm.heap.intern_key("stream");
            stream = vm
                .get_property_by_key(opt_h, k_stream)
                .map(|v| vm.to_boolean(v))?;
        }
        let mut st = wa.borrow_mut();
        let dec = st
            .text_decoders
            .get_mut(&h)
            .ok_or_else(|| vm.type_error("Incompatible TextDecoder receiver"))?;
        if !dec.do_not_flush {
            dec.reset();
        }
        dec.do_not_flush = stream;
        let last = !stream;
        let max_len = dec
            .decoder
            .max_utf8_buffer_length_without_replacement(bytes.len())
            .unwrap_or(bytes.len().saturating_mul(4).saturating_add(16))
            .max(16);
        let mut out = String::with_capacity(max_len.min(65536));
        let mut offset = 0usize;
        let mut had_error = false;
        loop {
            let chunk = &bytes[offset..];
            if dec.fatal {
                let (res, read) = dec
                    .decoder
                    .decode_to_string_without_replacement(chunk, &mut out, last);
                offset += read;
                match res {
                    encoding_rs::DecoderResult::InputEmpty => break,
                    encoding_rs::DecoderResult::OutputFull => {
                        out.reserve(chunk.len().saturating_mul(3).max(64));
                    }
                    encoding_rs::DecoderResult::Malformed(_, _) => {
                        had_error = true;
                        break;
                    }
                }
            } else {
                let (res, read, _) = dec.decoder.decode_to_string(chunk, &mut out, last);
                offset += read;
                match res {
                    encoding_rs::CoderResult::InputEmpty => break,
                    encoding_rs::CoderResult::OutputFull => {
                        out.reserve(chunk.len().saturating_mul(3).max(64));
                    }
                }
            }
        }
        if last || had_error {
            dec.reset();
        }
        drop(st);
        if had_error {
            return Err(vm.type_error("The encoded data was not valid for encoding"));
        }
        Ok(JsValue::from_string(vm.heap.alloc_string(&out)))
    });

    let wa = Rc::clone(web_apis);
    let dec_ctor = vm.alloc_host_fn("TextDecoder", move |vm, _, args| {
        let label = if arg(args, 0).is_undefined() {
            "utf-8".to_string()
        } else {
            vm.value_to_rust_string(arg(args, 0))
        };
        // WHATWG Encoding §4.2: trim ONLY ASCII whitespace (U+0009, U+000A, U+000C, U+000D, U+0020).
        let trimmed = label.trim_matches(|c: char| matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' '));
        let Some(enc) = Encoding::for_label(trimmed.as_bytes()) else {
            return Err(vm.range_error(format!("Unsupported encoding label: {label}")));
        };
        // WHATWG Encoding §7.4 step 2: replacement encoding MUST throw RangeError.
        if enc == encoding_rs::REPLACEMENT {
            return Err(vm.range_error(format!("Unsupported encoding label: {label}")));
        }
        let mut fatal = false;
        let mut ignore_bom = false;
        let opt_val = arg(args, 1);
        if !opt_val.is_undefined() && !opt_val.is_null() {
            let Some(opt_h) = opt_val.as_object() else {
                return Err(vm.type_error("TextDecoder options must be an object"));
            };
            let kf = vm.heap.intern_key("fatal");
            let kb = vm.heap.intern_key("ignoreBOM");
            fatal = vm
                .get_property_by_key(opt_h, kf)
                .map(|v| vm.to_boolean(v))?;
            ignore_bom = vm
                .get_property_by_key(opt_h, kb)
                .map(|v| vm.to_boolean(v))?;
        }
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(dp)));
        wa.borrow_mut()
            .text_decoders
            .insert(obj, TextDecoderEntry::new(enc, fatal, ignore_bom));
        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, dec_ctor, dp);
    define_global(vm, "TextDecoder", JsValue::from_object(dec_ctor));
}

// ---------------------------------------------------------------------------
// FormData (WHATWG XMLHttpRequest / Fetch)
// ---------------------------------------------------------------------------

pub(crate) fn alloc_form_data_from_urlencoded(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    bytes: &[u8],
) -> HeapHandle {
    let pairs = parse_query_pairs_raw(bytes)
        .into_iter()
        .map(|(k, v)| (k, FormDataVal::Text(v)))
        .collect();
    let proto = web_apis
        .borrow()
        .protos
        .form_data_proto
        .unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    web_apis
        .borrow_mut()
        .form_data
        .insert(obj, FormDataEntry { pairs });
    obj
}

fn require_form_data(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible FormData receiver"));
    };
    if web_apis.borrow().form_data.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible FormData receiver"))
    }
}

fn form_data_val_to_js(vm: &mut JsVm, v: &FormDataVal) -> JsValue {
    match v {
        FormDataVal::Text(s) => JsValue::from_string(vm.heap.alloc_string(s)),
        FormDataVal::File(fh) => JsValue::from_object(*fh),
    }
}

fn install_form_data(vm: &mut JsVm, host: &SharedHost, web_apis: &SharedWebApis) {
    let fdp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(fdp).unwrap().is_prototype = true;
    web_apis.borrow_mut().protos.form_data_proto = Some(fdp);

    let wa = Rc::clone(web_apis);
    define_host_method(vm, fdp, "append", move |vm, this_val, args| {
        let h = require_form_data(vm, &wa, this_val)?;
        let name = vm.value_to_rust_string(arg(args, 0));
        let val = arg(args, 1);
        let entry_val = if let Some(bh) = val.as_object() {
            if wa.borrow().blobs.contains_key(&bh) {
                FormDataVal::File(bh)
            } else {
                FormDataVal::Text(vm.value_to_rust_string(val))
            }
        } else {
            FormDataVal::Text(vm.value_to_rust_string(val))
        };
        if let Some(fd) = wa.borrow_mut().form_data.get_mut(&h) {
            fd.pairs.push((name, entry_val));
        }
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, fdp, "set", move |vm, this_val, args| {
        let h = require_form_data(vm, &wa, this_val)?;
        let name = vm.value_to_rust_string(arg(args, 0));
        let val = arg(args, 1);
        let entry_val = if let Some(bh) = val.as_object() {
            if wa.borrow().blobs.contains_key(&bh) {
                FormDataVal::File(bh)
            } else {
                FormDataVal::Text(vm.value_to_rust_string(val))
            }
        } else {
            FormDataVal::Text(vm.value_to_rust_string(val))
        };
        if let Some(fd) = wa.borrow_mut().form_data.get_mut(&h) {
            let mut replaced = false;
            let mut next = Vec::with_capacity(fd.pairs.len() + 1);
            for (k, v) in fd.pairs.drain(..) {
                if k == name {
                    if !replaced {
                        next.push((name.clone(), entry_val.clone()));
                        replaced = true;
                    }
                } else {
                    next.push((k, v));
                }
            }
            if !replaced {
                next.push((name, entry_val));
            }
            fd.pairs = next;
        }
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, fdp, "delete", move |vm, this_val, args| {
        let h = require_form_data(vm, &wa, this_val)?;
        let name = vm.value_to_rust_string(arg(args, 0));
        if let Some(fd) = wa.borrow_mut().form_data.get_mut(&h) {
            fd.pairs.retain(|(k, _)| k != &name);
        }
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, fdp, "get", move |vm, this_val, args| {
        let h = require_form_data(vm, &wa, this_val)?;
        let name = vm.value_to_rust_string(arg(args, 0));
        let found = wa.borrow().form_data.get(&h).and_then(|fd| {
            fd.pairs
                .iter()
                .find(|(k, _)| k == &name)
                .map(|(_, v)| v.clone())
        });
        Ok(match found {
            Some(v) => form_data_val_to_js(vm, &v),
            None => JsValue::NULL,
        })
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, fdp, "getAll", move |vm, this_val, args| {
        let h = require_form_data(vm, &wa, this_val)?;
        let name = vm.value_to_rust_string(arg(args, 0));
        let vals: Vec<FormDataVal> = wa
            .borrow()
            .form_data
            .get(&h)
            .map(|fd| {
                fd.pairs
                    .iter()
                    .filter(|(k, _)| k == &name)
                    .map(|(_, v)| v.clone())
                    .collect()
            })
            .unwrap_or_default();
        let arr = vm.alloc_array();
        for v in vals {
            let jv = form_data_val_to_js(vm, &v);
            vm.heap.object_push_element(arr, jv);
        }
        Ok(JsValue::from_object(arr))
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, fdp, "has", move |vm, this_val, args| {
        let h = require_form_data(vm, &wa, this_val)?;
        let name = vm.value_to_rust_string(arg(args, 0));
        let found = wa
            .borrow()
            .form_data
            .get(&h)
            .is_some_and(|fd| fd.pairs.iter().any(|(k, _)| k == &name));
        Ok(JsValue::from_bool(found))
    });

    let wa = Rc::clone(web_apis);
    let entries_fn = vm.alloc_host_fn("entries", move |vm, this_val, _| {
        let h = require_form_data(vm, &wa, this_val)?;
        let pairs = wa
            .borrow()
            .form_data
            .get(&h)
            .map(|fd| fd.pairs.clone())
            .unwrap_or_default();
        let mut items = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            let pair = vm.alloc_array();
            let kv = JsValue::from_string(vm.heap.alloc_string(&k));
            let vv = form_data_val_to_js(vm, &v);
            vm.heap.object_push_element(pair, kv);
            vm.heap.object_push_element(pair, vv);
            items.push(JsValue::from_object(pair));
        }
        Ok(JsValue::from_object(alloc_values_iterator(vm, items)))
    });
    let k_ent = vm.heap.intern_key("entries");
    vm.heap.object_define_data(
        fdp,
        k_ent,
        JsValue::from_object(entries_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let sym_it = vm.key_symbol_iterator;
    vm.heap.object_define_data(
        fdp,
        sym_it,
        JsValue::from_object(entries_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    let wa = Rc::clone(web_apis);
    define_host_method(vm, fdp, "keys", move |vm, this_val, _| {
        let h = require_form_data(vm, &wa, this_val)?;
        let pairs = wa
            .borrow()
            .form_data
            .get(&h)
            .map(|fd| fd.pairs.clone())
            .unwrap_or_default();
        let items = pairs
            .into_iter()
            .map(|(k, _)| JsValue::from_string(vm.heap.alloc_string(&k)))
            .collect();
        Ok(JsValue::from_object(alloc_values_iterator(vm, items)))
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, fdp, "values", move |vm, this_val, _| {
        let h = require_form_data(vm, &wa, this_val)?;
        let pairs = wa
            .borrow()
            .form_data
            .get(&h)
            .map(|fd| fd.pairs.clone())
            .unwrap_or_default();
        let items = pairs
            .iter()
            .map(|(_, v)| form_data_val_to_js(vm, v))
            .collect();
        Ok(JsValue::from_object(alloc_values_iterator(vm, items)))
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, fdp, "forEach", move |vm, this_val, args| {
        let h = require_form_data(vm, &wa, this_val)?;
        let cb = arg(args, 0);
        let this_arg = arg(args, 1);
        let pairs = wa
            .borrow()
            .form_data
            .get(&h)
            .map(|fd| fd.pairs.clone())
            .unwrap_or_default();
        for (k, v) in pairs {
            let kv = JsValue::from_string(vm.heap.alloc_string(&k));
            let vv = form_data_val_to_js(vm, &v);
            vm.call_value(cb, this_arg, &[vv, kv, this_val])?;
        }
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    let h_dom = Rc::clone(host);
    let ctor = vm.alloc_host_fn("FormData", move |vm, _, args| {
        let mut pairs = Vec::new();
        let form_val = arg(args, 0);
        if !form_val.is_undefined() && !form_val.is_null() {
            if let Some(form_nid) = h_dom.borrow().node_for_value(vm, form_val) {
                let doc_rc = Rc::clone(&h_dom.borrow().document);
                let doc = doc_rc.borrow();
                for desc in doc.descendants(form_nid) {
                    if let Some(rix_dom::NodeData::Element(el)) = doc.get(desc) {
                        let tag = el.name.local.as_str();
                        if matches!(tag, "input" | "textarea" | "select")
                            && !el.has_attribute("disabled")
                        {
                            if let Some(name) = el.get_attribute("name").filter(|n| !n.is_empty()) {
                                let val = el.get_attribute("value").unwrap_or("").to_string();
                                pairs.push((name.to_string(), FormDataVal::Text(val)));
                            }
                        }
                    }
                }
            }
        }
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(fdp)));
        wa.borrow_mut()
            .form_data
            .insert(obj, FormDataEntry { pairs });
        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, ctor, fdp);
    define_global(vm, "FormData", JsValue::from_object(ctor));
}

// ---------------------------------------------------------------------------
// Blob & File
// ---------------------------------------------------------------------------

fn install_blob_and_file(vm: &mut JsVm, web_apis: &SharedWebApis) {
    let bp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(bp).unwrap().is_prototype = true;
    web_apis.borrow_mut().protos.blob_proto = Some(bp);

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, bp, "size", move |vm, this_val, _| {
        let h = require_blob(vm, &wa, this_val)?;
        let n = wa.borrow().blobs.get(&h).map_or(0, |b| b.bytes.len());
        Ok(JsValue::from_f64(n as f64))
    });

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, bp, "type", move |vm, this_val, _| {
        let h = require_blob(vm, &wa, this_val)?;
        let ty = wa
            .borrow()
            .blobs
            .get(&h)
            .map(|b| b.content_type.clone())
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&ty)))
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, bp, "text", move |vm, this_val, _| {
        let h = require_blob(vm, &wa, this_val)?;
        let bytes = wa
            .borrow()
            .blobs
            .get(&h)
            .map(|b| b.bytes.clone())
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let v = JsValue::from_string(vm.heap.alloc_string(&text));
        Ok(JsValue::from_object(promise_resolve_value(vm, v)))
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, bp, "arrayBuffer", move |vm, this_val, _| {
        let h = require_blob(vm, &wa, this_val)?;
        let bytes = wa
            .borrow()
            .blobs
            .get(&h)
            .map(|b| b.bytes.clone())
            .unwrap_or_default();
        let ab = alloc_array_buffer(vm, bytes);
        Ok(JsValue::from_object(promise_resolve_value(
            vm,
            JsValue::from_object(ab),
        )))
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, bp, "bytes", move |vm, this_val, _| {
        let h = require_blob(vm, &wa, this_val)?;
        let bytes = wa
            .borrow()
            .blobs
            .get(&h)
            .map(|b| b.bytes.clone())
            .unwrap_or_default();
        let u8a = alloc_uint8_array(vm, bytes);
        Ok(JsValue::from_object(promise_resolve_value(
            vm,
            JsValue::from_object(u8a),
        )))
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, bp, "slice", move |vm, this_val, args| {
        let h = require_blob(vm, &wa, this_val)?;
        let bytes = wa
            .borrow()
            .blobs
            .get(&h)
            .map(|b| b.bytes.clone())
            .unwrap_or_default();
        let len = bytes.len() as isize;
        let clamp_idx = |v: JsValue, default: isize, vm: &mut JsVm| -> Result<usize, JsException> {
            if v.is_undefined() {
                return Ok(default.clamp(0, len) as usize);
            }
            let n = vm.to_number(v)?.trunc() as isize;
            let rel = if n < 0 { len + n } else { n };
            Ok(rel.clamp(0, len) as usize)
        };
        let start = clamp_idx(arg(args, 0), 0, vm)?;
        let end = clamp_idx(arg(args, 1), len, vm)?;
        let slice = if end > start {
            bytes[start..end].to_vec()
        } else {
            Vec::new()
        };
        let ty = if arg(args, 2).is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(arg(args, 2)).to_ascii_lowercase()
        };
        let obj = create_blob_object(vm, &wa, slice, ty);
        Ok(JsValue::from_object(obj))
    });

    let wa = Rc::clone(web_apis);
    let blob_ctor = vm.alloc_host_fn("Blob", move |vm, _, args| {
        let mut bytes = Vec::new();
        let parts = arg(args, 0);
        if !parts.is_undefined() && !parts.is_null() {
            for item in vm.collect_iterable(parts)? {
                bytes.extend_from_slice(&extract_bytes_from_js(vm, &wa, item)?);
            }
        }
        let mut content_type = String::new();
        if let Some(opt_h) = arg(args, 1).as_object() {
            let kt = vm.heap.intern_key("type");
            if let Ok(v) = vm.get_property_by_key(opt_h, kt) {
                if !v.is_undefined() {
                    content_type = vm.value_to_rust_string(v).to_ascii_lowercase();
                }
            }
        }
        let obj = create_blob_object(vm, &wa, bytes, content_type);
        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, blob_ctor, bp);
    define_global(vm, "Blob", JsValue::from_object(blob_ctor));

    // File inherits from Blob.prototype
    let fp = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(bp)));
    vm.heap.get_object_mut(fp).unwrap().is_prototype = true;
    web_apis.borrow_mut().protos.file_proto = Some(fp);

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, fp, "name", move |vm, this_val, _| {
        let h = require_blob(vm, &wa, this_val)?;
        let s = wa
            .borrow()
            .blobs
            .get(&h)
            .and_then(|b| b.name.clone())
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, fp, "lastModified", move |vm, this_val, _| {
        let h = require_blob(vm, &wa, this_val)?;
        let lm = wa
            .borrow()
            .blobs
            .get(&h)
            .and_then(|b| b.last_modified)
            .unwrap_or(0.0);
        Ok(JsValue::from_f64(lm))
    });

    let wa = Rc::clone(web_apis);
    let file_ctor = vm.alloc_host_fn("File", move |vm, _, args| {
        if args.len() < 2 {
            return Err(vm.type_error("File constructor requires 2 arguments"));
        }
        let mut bytes = Vec::new();
        for item in vm.collect_iterable(arg(args, 0))? {
            bytes.extend_from_slice(&extract_bytes_from_js(vm, &wa, item)?);
        }
        let name = vm.value_to_rust_string(arg(args, 1));
        let mut content_type = String::new();
        let mut last_modified = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_millis() as f64);
        if let Some(opt_h) = arg(args, 2).as_object() {
            let kt = vm.heap.intern_key("type");
            let kl = vm.heap.intern_key("lastModified");
            if let Ok(v) = vm.get_property_by_key(opt_h, kt) {
                if !v.is_undefined() {
                    content_type = vm.value_to_rust_string(v).to_ascii_lowercase();
                }
            }
            if let Ok(v) = vm.get_property_by_key(opt_h, kl) {
                if !v.is_undefined() {
                    last_modified = vm.to_number(v)?;
                }
            }
        }
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(fp)));
        wa.borrow_mut().blobs.insert(
            obj,
            BlobEntry {
                bytes,
                content_type,
                name: Some(name),
                last_modified: Some(last_modified),
            },
        );
        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, file_ctor, fp);
    define_global(vm, "File", JsValue::from_object(file_ctor));
}

fn require_blob(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible Blob receiver"));
    };
    if web_apis.borrow().blobs.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible Blob receiver"))
    }
}

// ---------------------------------------------------------------------------
// structuredClone (cycle-preserving deep clone)
// ---------------------------------------------------------------------------

fn install_structured_clone(vm: &mut JsVm, web_apis: &SharedWebApis) {
    let wa = Rc::clone(web_apis);
    let sc_fn = vm.alloc_host_fn("structuredClone", move |vm, _, args| {
        let mut memo = HashMap::new();
        structured_clone_inner(vm, &wa, arg(args, 0), &mut memo)
    });
    define_global(vm, "structuredClone", JsValue::from_object(sc_fn));
}

pub(crate) fn structured_clone_inner(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    val: JsValue,
    memo: &mut HashMap<HeapHandle, HeapHandle>,
) -> Result<JsValue, JsException> {
    if val.is_undefined()
        || val.is_null()
        || val.is_bool()
        || val.is_number()
        || val.is_string()
        || val.is_bigint()
    {
        return Ok(val);
    }
    if val.is_symbol() {
        return Err(create_dom_exception(
            vm,
            "DataCloneError",
            "Symbol could not be cloned",
        ));
    }
    let Some(h) = val.as_object() else {
        return Ok(val);
    };
    if let Some(&cloned) = memo.get(&h) {
        return Ok(JsValue::from_object(cloned));
    }
    if vm.is_callable_handle(h) {
        return Err(create_dom_exception(
            vm,
            "DataCloneError",
            "Function could not be cloned",
        ));
    }
    {
        let st = web_apis.borrow();
        if st.url_objects.contains_key(&h)
            || st.search_params.contains_key(&h)
            || st.text_decoders.contains_key(&h)
            || st.form_data.contains_key(&h)
            || st.abort_signals.contains_key(&h)
        {
            return Err(create_dom_exception(
                vm,
                "DataCloneError",
                "Object could not be cloned",
            ));
        }
    }
    let blob_opt = web_apis.borrow().blobs.get(&h).cloned();
    if let Some(blob) = blob_opt {
        let proto = if blob.name.is_some() {
            web_apis.borrow().protos.file_proto
        } else {
            web_apis.borrow().protos.blob_proto
        }
        .unwrap_or(vm.object_proto);
        let cloned = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        web_apis.borrow_mut().blobs.insert(cloned, blob);
        memo.insert(h, cloned);
        return Ok(JsValue::from_object(cloned));
    }

    let kind_tag = vm.heap.get_object(h).map(|o| match &o.kind {
        ObjectKind::Array => 1u8,
        ObjectKind::ArrayBuffer(_) => 2,
        ObjectKind::TypedArray(_) => 3,
        ObjectKind::DataView(_) => 4,
        ObjectKind::Map(_) => 5,
        ObjectKind::Set(_) => 6,
        ObjectKind::Date(_) => 7,
        ObjectKind::RegExp(_) => 8,
        ObjectKind::Ordinary => 9,
        _ => 0,
    });

    match kind_tag {
        Some(1) => {
            let cloned = vm.alloc_array();
            memo.insert(h, cloned);
            let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len());
            for i in 0..len {
                let elem = vm
                    .heap
                    .get_object(h)
                    .and_then(|o| o.elements.get(i))
                    .unwrap_or(JsValue::UNDEFINED);
                let c_elem = structured_clone_inner(vm, web_apis, elem, memo)?;
                vm.heap.object_push_element(cloned, c_elem);
            }
            Ok(JsValue::from_object(cloned))
        }
        Some(2) => {
            let bytes = match vm.heap.get_object(h).map(|o| &o.kind) {
                Some(ObjectKind::ArrayBuffer(b)) => (**b).clone(),
                _ => Vec::new(),
            };
            let cloned = alloc_array_buffer(vm, bytes);
            memo.insert(h, cloned);
            Ok(JsValue::from_object(cloned))
        }
        Some(3) => {
            let Some(ObjectKind::TypedArray(ta)) = vm.heap.get_object(h).map(|o| &o.kind) else {
                unreachable!()
            };
            let ta = *ta;
            let buf_val =
                structured_clone_inner(vm, web_apis, JsValue::from_object(ta.buffer), memo)?;
            let new_buf = buf_val.as_object().expect("cloned ArrayBuffer");
            let proto = vm.typed_array_protos[ta.kind as usize];
            let cloned = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::TypedArray(TypedArrayObject {
                    kind: ta.kind,
                    buffer: new_buf,
                    byte_offset: ta.byte_offset,
                    length: ta.length,
                }),
                Some(proto),
            ));
            memo.insert(h, cloned);
            Ok(JsValue::from_object(cloned))
        }
        Some(4) => {
            let Some(ObjectKind::DataView(dv)) = vm.heap.get_object(h).map(|o| &o.kind) else {
                unreachable!()
            };
            let dv = *dv;
            let buf_val =
                structured_clone_inner(vm, web_apis, JsValue::from_object(dv.buffer), memo)?;
            let new_buf = buf_val.as_object().expect("cloned ArrayBuffer");
            let cloned = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::DataView(DataViewObject {
                    buffer: new_buf,
                    byte_offset: dv.byte_offset,
                    byte_length: dv.byte_length,
                }),
                Some(vm.data_view_proto),
            ));
            memo.insert(h, cloned);
            Ok(JsValue::from_object(cloned))
        }
        Some(5) => {
            let entries = match vm.heap.get_object(h).map(|o| &o.kind) {
                Some(ObjectKind::Map(m)) => (**m).clone(),
                _ => Vec::new(),
            };
            let cloned = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Map(Box::default()),
                Some(vm.map_proto),
            ));
            memo.insert(h, cloned);
            let mut new_entries = Vec::with_capacity(entries.len());
            for (k, v) in entries {
                let ck = structured_clone_inner(vm, web_apis, k, memo)?;
                let cv = structured_clone_inner(vm, web_apis, v, memo)?;
                new_entries.push((ck, cv));
            }
            if let Some(ObjectKind::Map(m)) = vm.heap.get_object_mut(cloned).map(|o| &mut o.kind) {
                **m = new_entries;
            }
            Ok(JsValue::from_object(cloned))
        }
        Some(6) => {
            let items = match vm.heap.get_object(h).map(|o| &o.kind) {
                Some(ObjectKind::Set(s)) => (**s).clone(),
                _ => Vec::new(),
            };
            let cloned = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Set(Box::default()),
                Some(vm.set_proto),
            ));
            memo.insert(h, cloned);
            let mut new_items = Vec::with_capacity(items.len());
            for v in items {
                new_items.push(structured_clone_inner(vm, web_apis, v, memo)?);
            }
            if let Some(ObjectKind::Set(s)) = vm.heap.get_object_mut(cloned).map(|o| &mut o.kind) {
                **s = new_items;
            }
            Ok(JsValue::from_object(cloned))
        }
        Some(7) => {
            let ms = match vm.heap.get_object(h).map(|o| &o.kind) {
                Some(ObjectKind::Date(ms)) => *ms,
                _ => f64::NAN,
            };
            let cloned = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Date(ms),
                Some(vm.date_proto),
            ));
            memo.insert(h, cloned);
            Ok(JsValue::from_object(cloned))
        }
        Some(8) => {
            let Some(ObjectKind::RegExp(re)) = vm.heap.get_object(h).map(|o| &o.kind) else {
                unreachable!()
            };
            let cloned_re = re.clone();
            let cloned = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::RegExp(cloned_re),
                Some(vm.regexp_proto),
            ));
            memo.insert(h, cloned);
            Ok(JsValue::from_object(cloned))
        }
        Some(9) => {
            let cloned = vm.alloc_ordinary_object();
            memo.insert(h, cloned);
            for k_str in vm.own_enumerable_keys_including_elements(h) {
                let key = vm.heap.intern_key(&k_str);
                let prop = vm.get_property_by_key(h, key)?;
                let c_prop = structured_clone_inner(vm, web_apis, prop, memo)?;
                vm.heap.object_define_data(
                    cloned,
                    key,
                    c_prop,
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
            }
            Ok(JsValue::from_object(cloned))
        }
        _ => Err(create_dom_exception(
            vm,
            "DataCloneError",
            "Object could not be cloned",
        )),
    }
}

// ---------------------------------------------------------------------------
// AbortController & AbortSignal
// ---------------------------------------------------------------------------

fn trigger_abort_signal(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    sig_h: HeapHandle,
    reason_arg: JsValue,
) {
    let already_aborted = web_apis
        .borrow()
        .abort_signals
        .get(&sig_h)
        .is_none_or(|e| e.aborted);
    if already_aborted {
        return;
    }
    let reason = if reason_arg.is_undefined() {
        create_dom_exception(vm, "AbortError", "The operation was aborted.").value
    } else {
        reason_arg
    };
    // Per WHATWG DOM §3.1.2 "signal abort": mark this signal AND all non-aborted dependent
    // signals as aborted BEFORE firing any `abort` events!
    let mut to_notify: Vec<HeapHandle> = Vec::new();
    let mut handles_to_abort: Vec<AbortHandle> = Vec::new();
    {
        let mut st = web_apis.borrow_mut();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(sig_h);
        while let Some(cur) = queue.pop_front() {
            let Some(entry) = st.abort_signals.get_mut(&cur) else {
                continue;
            };
            if entry.aborted {
                continue;
            }
            entry.aborted = true;
            entry.reason = reason;
            handles_to_abort.push(entry.handle.clone());
            to_notify.push(cur);
            let deps = entry.dependent_signals.clone();
            for dep in deps {
                queue.push_back(dep);
            }
        }
    }
    for h in handles_to_abort {
        h.abort();
    }
    for s in to_notify {
        fire_object_event(vm, host, events, event_loop, s, "abort", &[]);
    }
}

fn install_abort_controller(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
) {
    let etp = host.borrow().prototypes.event_target_proto;
    let sp = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(etp)));
    vm.heap.get_object_mut(sp).unwrap().is_prototype = true;
    web_apis.borrow_mut().protos.abort_signal_proto = Some(sp);

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, sp, "aborted", move |vm, this_val, _| {
        let h = require_abort_signal(vm, &wa, this_val)?;
        let aborted = wa.borrow().abort_signals.get(&h).is_some_and(|e| e.aborted);
        Ok(JsValue::from_bool(aborted))
    });

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, sp, "reason", move |vm, this_val, _| {
        let h = require_abort_signal(vm, &wa, this_val)?;
        let reason = wa
            .borrow()
            .abort_signals
            .get(&h)
            .map_or(JsValue::UNDEFINED, |e| e.reason);
        Ok(reason)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, sp, "throwIfAborted", move |vm, this_val, _| {
        let h = require_abort_signal(vm, &wa, this_val)?;
        let aborted = wa
            .borrow()
            .abort_signals
            .get(&h)
            .and_then(|e| e.aborted.then_some(e.reason));
        if let Some(reason) = aborted {
            let msg = vm.value_to_rust_string(reason);
            return Err(JsException {
                value: reason,
                message: msg,
            });
        }
        Ok(JsValue::UNDEFINED)
    });

    let sig_ctor = vm.alloc_host_fn("AbortSignal", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: AbortSignal"))
    });
    let wa = Rc::clone(web_apis);
    define_host_method(vm, sig_ctor, "abort", move |vm, _, args| {
        let sig = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(sp)));
        let reason = if arg(args, 0).is_undefined() {
            create_dom_exception(vm, "AbortError", "The operation was aborted.").value
        } else {
            arg(args, 0)
        };
        let handle = AbortHandle::new();
        handle.abort();
        wa.borrow_mut().abort_signals.insert(
            sig,
            AbortSignalEntry {
                aborted: true,
                reason,
                handle,
                dependent_signals: Vec::new(),
            },
        );
        Ok(JsValue::from_object(sig))
    });

    // AbortSignal.any(signals) — WHATWG DOM §3.1.2
    let wa = Rc::clone(web_apis);
    define_host_method(vm, sig_ctor, "any", move |vm, _, args| {
        let iter_arg = arg(args, 0);
        let Some(iter_h) = iter_arg.as_object() else {
            return Err(
                vm.type_error("AbortSignal.any requires an iterable of AbortSignal objects")
            );
        };
        let mut input_signals: Vec<HeapHandle> = Vec::new();
        let sym_iter = vm.key_symbol_iterator;
        let iter_method = vm.get_property_by_key(iter_h, sym_iter)?;
        if !iter_method
            .as_object()
            .is_some_and(|fh| vm.is_callable_handle(fh))
        {
            return Err(vm.type_error("AbortSignal.any argument is not iterable"));
        }
        let iterator = vm.call_value(iter_method, iter_arg, &[])?;
        let Some(it_h) = iterator.as_object() else {
            return Err(vm.type_error("Iterator must be an object"));
        };
        let k_next = vm.heap.intern_key("next");
        let k_done = vm.heap.intern_key("done");
        let k_value = vm.heap.intern_key("value");
        let next_fn = vm.get_property_by_key(it_h, k_next)?;
        for _ in 0..100_000 {
            let step = vm.call_value(next_fn, iterator, &[])?;
            let Some(sh) = step.as_object() else {
                return Err(vm.type_error("Iterator result is not an object"));
            };
            let done = vm.get_property_by_key(sh, k_done)?;
            if vm.to_boolean(done) {
                break;
            }
            let item = vm.get_property_by_key(sh, k_value)?;
            let sig_h = require_abort_signal(vm, &wa, item)?;
            input_signals.push(sig_h);
        }

        let result_sig = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(sp)));
        // Check if any source signal is already aborted (using the first aborted signal's reason).
        let first_aborted_reason = {
            let st = wa.borrow();
            input_signals.iter().find_map(|sh| {
                st.abort_signals
                    .get(sh)
                    .and_then(|e| e.aborted.then_some(e.reason))
            })
        };
        if let Some(reason) = first_aborted_reason {
            let handle = AbortHandle::new();
            handle.abort();
            wa.borrow_mut().abort_signals.insert(
                result_sig,
                AbortSignalEntry {
                    aborted: true,
                    reason,
                    handle,
                    dependent_signals: Vec::new(),
                },
            );
            return Ok(JsValue::from_object(result_sig));
        }

        let mut st = wa.borrow_mut();
        st.abort_signals.insert(
            result_sig,
            AbortSignalEntry {
                aborted: false,
                reason: JsValue::UNDEFINED,
                handle: AbortHandle::new(),
                dependent_signals: Vec::new(),
            },
        );
        for src in input_signals {
            if let Some(entry) = st.abort_signals.get_mut(&src) {
                if !entry.dependent_signals.contains(&result_sig) {
                    entry.dependent_signals.push(result_sig);
                }
            }
        }
        Ok(JsValue::from_object(result_sig))
    });

    let wa = Rc::clone(web_apis);
    let h_to = Rc::clone(host);
    let ev_to = Rc::clone(events);
    let el_to = Rc::clone(event_loop);
    define_host_method(vm, sig_ctor, "timeout", move |vm, _, args| {
        let ms = vm.to_number(arg(args, 0))?;
        let sig = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(sp)));
        wa.borrow_mut().abort_signals.insert(
            sig,
            AbortSignalEntry {
                aborted: false,
                reason: JsValue::UNDEFINED,
                handle: AbortHandle::new(),
                dependent_signals: Vec::new(),
            },
        );
        let wa_cb = Rc::clone(&wa);
        let h_cb = Rc::clone(&h_to);
        let ev_cb = Rc::clone(&ev_to);
        let el_cb = Rc::clone(&el_to);
        let cb = vm.alloc_host_fn_rooted(
            "abort_timeout",
            &[JsValue::from_object(sig)],
            move |vm, _, _| {
                let reason = create_dom_exception(vm, "TimeoutError", "signal timed out").value;
                trigger_abort_signal(vm, &h_cb, &ev_cb, &el_cb, &wa_cb, sig, reason);
                Ok(JsValue::UNDEFINED)
            },
        );
        el_to.borrow_mut().schedule_timer(cb, ms, Vec::new(), false);
        Ok(JsValue::from_object(sig))
    });
    bind_ctor_proto(vm, sig_ctor, sp);
    define_global(vm, "AbortSignal", JsValue::from_object(sig_ctor));

    // AbortController
    let acp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(acp).unwrap().is_prototype = true;

    let wa = Rc::clone(web_apis);
    let h_ab = Rc::clone(host);
    let ev_ab = Rc::clone(events);
    let el_ab = Rc::clone(event_loop);
    define_host_method(vm, acp, "abort", move |vm, this_val, args| {
        let Some(ch) = this_val.as_object() else {
            return Err(vm.type_error("Incompatible AbortController receiver"));
        };
        let k_sig = vm.heap.intern_key("signal");
        let sig_v = vm.get_property_by_key(ch, k_sig)?;
        let sig_h = require_abort_signal(vm, &wa, sig_v)?;
        trigger_abort_signal(vm, &h_ab, &ev_ab, &el_ab, &wa, sig_h, arg(args, 0));
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    let ac_ctor = vm.alloc_host_fn("AbortController", move |vm, _, _| {
        let sig = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(sp)));
        wa.borrow_mut().abort_signals.insert(
            sig,
            AbortSignalEntry {
                aborted: false,
                reason: JsValue::UNDEFINED,
                handle: AbortHandle::new(),
                dependent_signals: Vec::new(),
            },
        );
        let ctrl = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(acp)));
        let k_sig = vm.heap.intern_key("signal");
        vm.heap.object_define_data(
            ctrl,
            k_sig,
            JsValue::from_object(sig),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        Ok(JsValue::from_object(ctrl))
    });
    bind_ctor_proto(vm, ac_ctor, acp);
    define_global(vm, "AbortController", JsValue::from_object(ac_ctor));
}

fn require_abort_signal(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible AbortSignal receiver"));
    };
    if web_apis.borrow().abort_signals.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible AbortSignal receiver"))
    }
}

// ---------------------------------------------------------------------------
// Location & History
// ---------------------------------------------------------------------------

fn install_location_and_history(vm: &mut JsVm, host: &SharedHost, web_apis: &SharedWebApis) {
    let loc = vm.alloc_ordinary_object();

    let wa = Rc::clone(web_apis);
    let h_href = Rc::clone(host);
    define_host_accessor(
        vm,
        loc,
        "href",
        {
            let wa = Rc::clone(&wa);
            move |vm, _, _| {
                let s = wa.borrow().location_url.as_str().to_string();
                Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
            }
        },
        move |vm, _, args| {
            let raw = vm.value_to_rust_string(arg(args, 0));
            let base = wa.borrow().location_url.clone();
            if let Ok(next) = base.join(&raw).or_else(|_| Url::parse(&raw)) {
                h_href.borrow_mut().main_document_url = next.to_string();
                wa.borrow_mut().location_url = next;
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, loc, "origin", move |vm, _, _| {
        let s = wa.borrow().current_origin();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, loc, "protocol", move |vm, _, _| {
        let s = format!("{}:", wa.borrow().location_url.scheme());
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, loc, "hostname", move |vm, _, _| {
        let s = wa
            .borrow()
            .location_url
            .host_str()
            .unwrap_or("")
            .to_string();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, loc, "port", move |vm, _, _| {
        let s = wa
            .borrow()
            .location_url
            .port()
            .map(|p| p.to_string())
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, loc, "host", move |vm, _, _| {
        let u = &wa.borrow().location_url;
        let s = match (u.host_str(), u.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            _ => String::new(),
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, loc, "pathname", move |vm, _, _| {
        let s = wa.borrow().location_url.path().to_string();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, loc, "search", move |vm, _, _| {
        let s = wa
            .borrow()
            .location_url
            .query()
            .filter(|q| !q.is_empty())
            .map(|q| format!("?{q}"))
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });

    let wa = Rc::clone(web_apis);
    define_host_accessor(
        vm,
        loc,
        "hash",
        {
            let wa = Rc::clone(&wa);
            move |vm, _, _| {
                let s = wa
                    .borrow()
                    .location_url
                    .fragment()
                    .filter(|f| !f.is_empty())
                    .map(|f| format!("#{f}"))
                    .unwrap_or_default();
                Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
            }
        },
        move |vm, _, args| {
            let raw = vm.value_to_rust_string(arg(args, 0));
            let trimmed = raw.strip_prefix('#').unwrap_or(&raw);
            let mut st = wa.borrow_mut();
            if trimmed.is_empty() {
                st.location_url.set_fragment(None);
            } else {
                st.location_url.set_fragment(Some(trimmed));
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let wa = Rc::clone(web_apis);
    define_host_method(vm, loc, "assign", move |vm, _, args| {
        let raw = vm.value_to_rust_string(arg(args, 0));
        let base = wa.borrow().location_url.clone();
        if let Ok(next) = base.join(&raw).or_else(|_| Url::parse(&raw)) {
            let mut st = wa.borrow_mut();
            let cut = st.history_index + 1;
            st.history.truncate(cut);
            st.history.push(HistoryItem {
                url: next.clone(),
                state: JsValue::NULL,
            });
            st.history_index = st.history.len() - 1;
            st.location_url = next;
        }
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, loc, "replace", move |vm, _, args| {
        let raw = vm.value_to_rust_string(arg(args, 0));
        let base = wa.borrow().location_url.clone();
        if let Ok(next) = base.join(&raw).or_else(|_| Url::parse(&raw)) {
            let mut st = wa.borrow_mut();
            let idx = st.history_index;
            if let Some(cur) = st.history.get_mut(idx) {
                cur.url = next.clone();
            }
            st.location_url = next;
        }
        Ok(JsValue::UNDEFINED)
    });

    define_host_method(vm, loc, "reload", |_, _, _| Ok(JsValue::UNDEFINED));

    let wa = Rc::clone(web_apis);
    define_host_method(vm, loc, "toString", move |vm, _, _| {
        let s = wa.borrow().location_url.as_str().to_string();
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });
    define_global(vm, "location", JsValue::from_object(loc));

    // --- History ---
    let hist = vm.alloc_ordinary_object();
    let wa = Rc::clone(web_apis);
    define_host_getter(vm, hist, "length", move |_, _, _| {
        Ok(JsValue::from_i32(wa.borrow().history.len() as i32))
    });

    let wa = Rc::clone(web_apis);
    define_host_getter(vm, hist, "state", move |_, _, _| {
        let st = wa.borrow();
        let v = st
            .history
            .get(st.history_index)
            .map_or(JsValue::NULL, |h| h.state);
        Ok(v)
    });

    let k_sr = vm.heap.intern_key("scrollRestoration");
    let auto_val = JsValue::from_string(vm.heap.alloc_string("auto"));
    vm.heap
        .object_define_data(hist, k_sr, auto_val, PropertyAttrs::DEFAULT, &mut vm.shapes);

    let wa = Rc::clone(web_apis);
    let h_ps = Rc::clone(host);
    define_host_method(vm, hist, "pushState", move |vm, _, args| {
        let state_val = arg(args, 0);
        let url_arg = arg(args, 2);
        let mut memo = HashMap::new();
        let cloned_state = structured_clone_inner(vm, &wa, state_val, &mut memo)?;
        let cur_url = wa.borrow().location_url.clone();
        let next_url = if !url_arg.is_undefined() && !url_arg.is_null() {
            let raw = vm.value_to_rust_string(url_arg);
            let resolved = cur_url
                .join(&raw)
                .map_err(|_| create_dom_exception(vm, "SecurityError", "Invalid history URL"))?;
            if origin_of_url(&resolved) != origin_of_url(&cur_url) {
                return Err(create_dom_exception(
                    vm,
                    "SecurityError",
                    "pushState URL must be same-origin",
                ));
            }
            resolved
        } else {
            cur_url
        };
        let mut st = wa.borrow_mut();
        let cut = st.history_index + 1;
        st.history.truncate(cut);
        st.history.push(HistoryItem {
            url: next_url.clone(),
            state: cloned_state,
        });
        st.history_index = st.history.len() - 1;
        h_ps.borrow_mut().main_document_url = next_url.to_string();
        st.location_url = next_url;
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    let h_rs = Rc::clone(host);
    define_host_method(vm, hist, "replaceState", move |vm, _, args| {
        let state_val = arg(args, 0);
        let url_arg = arg(args, 2);
        let mut memo = HashMap::new();
        let cloned_state = structured_clone_inner(vm, &wa, state_val, &mut memo)?;
        let cur_url = wa.borrow().location_url.clone();
        let next_url = if !url_arg.is_undefined() && !url_arg.is_null() {
            let raw = vm.value_to_rust_string(url_arg);
            let resolved = cur_url
                .join(&raw)
                .map_err(|_| create_dom_exception(vm, "SecurityError", "Invalid history URL"))?;
            if origin_of_url(&resolved) != origin_of_url(&cur_url) {
                return Err(create_dom_exception(
                    vm,
                    "SecurityError",
                    "replaceState URL must be same-origin",
                ));
            }
            resolved
        } else {
            cur_url
        };
        let mut st = wa.borrow_mut();
        let idx = st.history_index;
        if let Some(cur) = st.history.get_mut(idx) {
            cur.url = next_url.clone();
            cur.state = cloned_state;
        }
        h_rs.borrow_mut().main_document_url = next_url.to_string();
        st.location_url = next_url;
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, hist, "go", move |vm, _, args| {
        let delta = if arg(args, 0).is_undefined() {
            0
        } else {
            vm.to_number(arg(args, 0))?.trunc() as isize
        };
        let mut st = wa.borrow_mut();
        let target = st.history_index as isize + delta;
        if target >= 0 && (target as usize) < st.history.len() {
            st.history_index = target as usize;
            st.location_url = st.history[st.history_index].url.clone();
        }
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, hist, "back", move |_, _, _| {
        let mut st = wa.borrow_mut();
        if st.history_index > 0 {
            st.history_index -= 1;
            st.location_url = st.history[st.history_index].url.clone();
        }
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, hist, "forward", move |_, _, _| {
        let mut st = wa.borrow_mut();
        if st.history_index + 1 < st.history.len() {
            st.history_index += 1;
            st.location_url = st.history[st.history_index].url.clone();
        }
        Ok(JsValue::UNDEFINED)
    });

    define_global(vm, "history", JsValue::from_object(hist));
}

// ---------------------------------------------------------------------------
// localStorage (backed by rix_storage::StorageEngine) & sessionStorage
// ---------------------------------------------------------------------------

fn install_web_storage(vm: &mut JsVm, web_apis: &SharedWebApis) {
    // localStorage
    let ls = vm.alloc_ordinary_object();
    let wa = Rc::clone(web_apis);
    define_host_getter(vm, ls, "length", move |_, _, _| {
        let st = wa.borrow();
        let origin = st.current_origin();
        let n = st
            .storage
            .get_all_local_storage(&origin)
            .map_or(0, |m| m.len());
        Ok(JsValue::from_i32(n as i32))
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, ls, "getItem", move |vm, _, args| {
        let key = vm.value_to_rust_string(arg(args, 0));
        let st = wa.borrow();
        let origin = st.current_origin();
        let val = st.storage.get_local_storage(&origin, &key).ok().flatten();
        Ok(match val {
            Some(s) => JsValue::from_string(vm.heap.alloc_string(&s)),
            None => JsValue::NULL,
        })
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, ls, "setItem", move |vm, _, args| {
        let key = vm.value_to_rust_string(arg(args, 0));
        let val = vm.value_to_rust_string(arg(args, 1));
        let st = wa.borrow();
        let origin = st.current_origin();
        st.storage
            .set_local_storage(&origin, &key, &val)
            .map_err(|e| create_dom_exception(vm, "QuotaExceededError", &e.to_string()))?;
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, ls, "removeItem", move |vm, _, args| {
        let key = vm.value_to_rust_string(arg(args, 0));
        let st = wa.borrow();
        let origin = st.current_origin();
        let _ = st.storage.remove_local_storage(&origin, &key);
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, ls, "clear", move |_, _, _| {
        let st = wa.borrow();
        let origin = st.current_origin();
        let _ = st.storage.clear_local_storage(&origin);
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, ls, "key", move |vm, _, args| {
        let idx = vm.to_u32(arg(args, 0))? as usize;
        let st = wa.borrow();
        let origin = st.current_origin();
        let mut keys: Vec<String> = st
            .storage
            .get_all_local_storage(&origin)
            .map(|m| m.into_keys().collect())
            .unwrap_or_default();
        keys.sort();
        Ok(match keys.get(idx) {
            Some(k) => JsValue::from_string(vm.heap.alloc_string(k)),
            None => JsValue::NULL,
        })
    });
    define_global(vm, "localStorage", JsValue::from_object(ls));

    // sessionStorage
    let ss = vm.alloc_ordinary_object();
    let wa = Rc::clone(web_apis);
    define_host_getter(vm, ss, "length", move |_, _, _| {
        let st = wa.borrow();
        let origin = st.current_origin();
        let n = st.session_storage.get(&origin).map_or(0, Vec::len);
        Ok(JsValue::from_i32(n as i32))
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, ss, "getItem", move |vm, _, args| {
        let key = vm.value_to_rust_string(arg(args, 0));
        let st = wa.borrow();
        let origin = st.current_origin();
        let val = st
            .session_storage
            .get(&origin)
            .and_then(|list| list.iter().find(|(k, _)| k == &key).map(|(_, v)| v.clone()));
        Ok(match val {
            Some(s) => JsValue::from_string(vm.heap.alloc_string(&s)),
            None => JsValue::NULL,
        })
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, ss, "setItem", move |vm, _, args| {
        let key = vm.value_to_rust_string(arg(args, 0));
        let val = vm.value_to_rust_string(arg(args, 1));
        let mut st = wa.borrow_mut();
        let origin = st.current_origin();
        let list = st.session_storage.entry(origin).or_default();
        if let Some((_, existing)) = list.iter_mut().find(|(k, _)| k == &key) {
            *existing = val;
        } else {
            list.push((key, val));
        }
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, ss, "removeItem", move |vm, _, args| {
        let key = vm.value_to_rust_string(arg(args, 0));
        let mut st = wa.borrow_mut();
        let origin = st.current_origin();
        if let Some(list) = st.session_storage.get_mut(&origin) {
            list.retain(|(k, _)| k != &key);
        }
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, ss, "clear", move |_, _, _| {
        let mut st = wa.borrow_mut();
        let origin = st.current_origin();
        st.session_storage.remove(&origin);
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, ss, "key", move |vm, _, args| {
        let idx = vm.to_u32(arg(args, 0))? as usize;
        let st = wa.borrow();
        let origin = st.current_origin();
        let found = st
            .session_storage
            .get(&origin)
            .and_then(|list| list.get(idx))
            .map(|(k, _)| k.clone());
        Ok(match found {
            Some(k) => JsValue::from_string(vm.heap.alloc_string(&k)),
            None => JsValue::NULL,
        })
    });
    define_global(vm, "sessionStorage", JsValue::from_object(ss));
}

// ---------------------------------------------------------------------------
// Performance (Full User Timing + High-Resolution Time)
// ---------------------------------------------------------------------------

fn perf_entry_to_js(vm: &mut JsVm, e: &PerfEntry) -> JsValue {
    let obj = vm.alloc_ordinary_object();
    let et_val = JsValue::from_string(vm.heap.alloc_string(&e.entry_type));
    let nm_val = JsValue::from_string(vm.heap.alloc_string(&e.name));
    for (k, v) in [
        ("entryType", et_val),
        ("name", nm_val),
        ("startTime", JsValue::from_f64(e.start_time)),
        ("duration", JsValue::from_f64(e.duration)),
    ] {
        let key = vm.heap.intern_key(k);
        vm.heap
            .object_define_data(obj, key, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }
    JsValue::from_object(obj)
}

fn install_performance(vm: &mut JsVm, event_loop: &SharedEventLoop, web_apis: &SharedWebApis) {
    let perf = vm.alloc_ordinary_object();
    let time_origin = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0);
    let k_to = vm.heap.intern_key("timeOrigin");
    vm.heap.object_define_data(
        perf,
        k_to,
        JsValue::from_f64(time_origin),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );

    let el = Rc::clone(event_loop);
    define_host_method(vm, perf, "now", move |_, _, _| {
        Ok(JsValue::from_f64(el.borrow().now_ms()))
    });

    let el = Rc::clone(event_loop);
    let wa = Rc::clone(web_apis);
    define_host_method(vm, perf, "mark", move |vm, _, args| {
        let name = vm.value_to_rust_string(arg(args, 0));
        let now = el.borrow().now_ms();
        let entry = PerfEntry {
            entry_type: "mark".to_string(),
            name,
            start_time: now,
            duration: 0.0,
        };
        let js = perf_entry_to_js(vm, &entry);
        wa.borrow_mut().perf_entries.push(entry);
        Ok(js)
    });

    let el = Rc::clone(event_loop);
    let wa = Rc::clone(web_apis);
    define_host_method(vm, perf, "measure", move |vm, _, args| {
        let name = vm.value_to_rust_string(arg(args, 0));
        let start_arg = arg(args, 1);
        let end_arg = arg(args, 2);
        let now = el.borrow().now_ms();
        let find_mark = |mark_name: &str| -> Option<f64> {
            wa.borrow()
                .perf_entries
                .iter()
                .rev()
                .find(|e| e.entry_type == "mark" && e.name == mark_name)
                .map(|e| e.start_time)
        };
        let start_time = if start_arg.is_undefined() {
            0.0
        } else {
            let s = vm.value_to_rust_string(start_arg);
            find_mark(&s).unwrap_or(0.0)
        };
        let end_time = if end_arg.is_undefined() {
            now
        } else {
            let s = vm.value_to_rust_string(end_arg);
            find_mark(&s).unwrap_or(now)
        };
        let entry = PerfEntry {
            entry_type: "measure".to_string(),
            name,
            start_time,
            duration: (end_time - start_time).max(0.0),
        };
        let js = perf_entry_to_js(vm, &entry);
        wa.borrow_mut().perf_entries.push(entry);
        Ok(js)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, perf, "getEntries", move |vm, _, _| {
        let list = wa.borrow().perf_entries.clone();
        let arr = vm.alloc_array();
        for e in &list {
            let v = perf_entry_to_js(vm, e);
            vm.heap.object_push_element(arr, v);
        }
        Ok(JsValue::from_object(arr))
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, perf, "getEntriesByType", move |vm, _, args| {
        let ty = vm.value_to_rust_string(arg(args, 0));
        let list: Vec<PerfEntry> = wa
            .borrow()
            .perf_entries
            .iter()
            .filter(|e| e.entry_type == ty)
            .cloned()
            .collect();
        let arr = vm.alloc_array();
        for e in &list {
            let v = perf_entry_to_js(vm, e);
            vm.heap.object_push_element(arr, v);
        }
        Ok(JsValue::from_object(arr))
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, perf, "getEntriesByName", move |vm, _, args| {
        let name = vm.value_to_rust_string(arg(args, 0));
        let ty = if arg(args, 1).is_undefined() {
            None
        } else {
            Some(vm.value_to_rust_string(arg(args, 1)))
        };
        let list: Vec<PerfEntry> = wa
            .borrow()
            .perf_entries
            .iter()
            .filter(|e| e.name == name && ty.as_ref().is_none_or(|t| &e.entry_type == t))
            .cloned()
            .collect();
        let arr = vm.alloc_array();
        for e in &list {
            let v = perf_entry_to_js(vm, e);
            vm.heap.object_push_element(arr, v);
        }
        Ok(JsValue::from_object(arr))
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, perf, "clearMarks", move |vm, _, args| {
        let filter = if arg(args, 0).is_undefined() {
            None
        } else {
            Some(vm.value_to_rust_string(arg(args, 0)))
        };
        wa.borrow_mut()
            .perf_entries
            .retain(|e| e.entry_type != "mark" || filter.as_ref().is_some_and(|f| &e.name != f));
        Ok(JsValue::UNDEFINED)
    });

    let wa = Rc::clone(web_apis);
    define_host_method(vm, perf, "clearMeasures", move |vm, _, args| {
        let filter = if arg(args, 0).is_undefined() {
            None
        } else {
            Some(vm.value_to_rust_string(arg(args, 0)))
        };
        wa.borrow_mut()
            .perf_entries
            .retain(|e| e.entry_type != "measure" || filter.as_ref().is_some_and(|f| &e.name != f));
        Ok(JsValue::UNDEFINED)
    });

    define_global(vm, "performance", JsValue::from_object(perf));
}

// ---------------------------------------------------------------------------
// Console, Window Viewport/Scroll, matchMedia, requestIdleCallback, Navigator
// ---------------------------------------------------------------------------

fn install_console(vm: &mut JsVm, web_apis: &SharedWebApis) {
    let console_obj = vm.alloc_ordinary_object();
    for level in ["log", "info", "warn", "error", "debug", "dir", "table"] {
        let wa = Rc::clone(web_apis);
        let lvl = level.to_string();
        define_host_method(vm, console_obj, level, move |vm, _, args| {
            let formatted: Vec<String> = args.iter().map(|&v| vm.value_to_rust_string(v)).collect();
            wa.borrow_mut()
                .record_console(lvl.clone(), formatted.join(" "));
            Ok(JsValue::UNDEFINED)
        });
    }
    let wa_assert = Rc::clone(web_apis);
    define_host_method(vm, console_obj, "assert", move |vm, _, args| {
        let ok = args.first().copied().is_some_and(|v| vm.to_boolean(v));
        if !ok {
            let msg: Vec<String> = args
                .iter()
                .skip(1)
                .map(|&v| vm.value_to_rust_string(v))
                .collect();
            let text = if msg.is_empty() {
                "Assertion failed".to_string()
            } else {
                format!("Assertion failed: {}", msg.join(" "))
            };
            wa_assert.borrow_mut().record_console("error", text);
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_method(vm, console_obj, "clear", |_, _, _| Ok(JsValue::UNDEFINED));
    define_host_method(vm, console_obj, "group", |_, _, _| Ok(JsValue::UNDEFINED));
    define_host_method(vm, console_obj, "groupEnd", |_, _, _| {
        Ok(JsValue::UNDEFINED)
    });
    define_host_method(vm, console_obj, "time", |_, _, _| Ok(JsValue::UNDEFINED));
    define_host_method(vm, console_obj, "timeEnd", |_, _, _| Ok(JsValue::UNDEFINED));
    define_global(vm, "console", JsValue::from_object(console_obj));
}

fn install_window_and_navigator(
    vm: &mut JsVm,
    host: &SharedHost,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
) {
    let global = vm.global;

    // Live viewport & scroll getters on `window` / `globalThis`.
    let h = Rc::clone(host);
    define_host_getter(vm, global, "innerWidth", move |_, _, _| {
        Ok(JsValue::from_number(f64::from(h.borrow().viewport.width)))
    });

    let h = Rc::clone(host);
    define_host_getter(vm, global, "innerHeight", move |_, _, _| {
        Ok(JsValue::from_number(f64::from(h.borrow().viewport.height)))
    });

    let h = Rc::clone(host);
    define_host_getter(vm, global, "devicePixelRatio", move |_, _, _| {
        Ok(JsValue::from_number(f64::from(
            h.borrow().device_pixel_ratio,
        )))
    });

    let h = Rc::clone(host);
    define_host_getter(vm, global, "scrollX", move |_, _, _| {
        Ok(JsValue::from_number(f64::from(h.borrow().scroll_offset.x)))
    });

    let h = Rc::clone(host);
    define_host_getter(vm, global, "pageXOffset", move |_, _, _| {
        Ok(JsValue::from_number(f64::from(h.borrow().scroll_offset.x)))
    });

    let h = Rc::clone(host);
    define_host_getter(vm, global, "scrollY", move |_, _, _| {
        Ok(JsValue::from_number(f64::from(h.borrow().scroll_offset.y)))
    });

    let h = Rc::clone(host);
    define_host_getter(vm, global, "pageYOffset", move |_, _, _| {
        Ok(JsValue::from_number(f64::from(h.borrow().scroll_offset.y)))
    });

    for (name, relative) in [("scrollTo", false), ("scroll", false), ("scrollBy", true)] {
        let h = Rc::clone(host);
        define_host_method(vm, global, name, move |vm, _, args| {
            let first = arg(args, 0);
            let (mut x_opt, mut y_opt) = (None, None);
            if let Some(obj) = first.as_object() {
                let k_top = vm.heap.intern_key("top");
                let k_left = vm.heap.intern_key("left");
                if let Ok(v) = vm.get_property_by_key(obj, k_left) {
                    if !v.is_undefined() {
                        let x = vm.to_number(v)?;
                        if x.is_finite() {
                            x_opt = Some(x as f32);
                        }
                    }
                }
                if let Ok(v) = vm.get_property_by_key(obj, k_top) {
                    if !v.is_undefined() {
                        let y = vm.to_number(v)?;
                        if y.is_finite() {
                            y_opt = Some(y as f32);
                        }
                    }
                }
            } else {
                let x = vm.to_number(first)?;
                let y = vm.to_number(arg(args, 1))?;
                if x.is_finite() {
                    x_opt = Some(x as f32);
                }
                if y.is_finite() {
                    y_opt = Some(y as f32);
                }
            }
            h.borrow_mut().set_root_scroll(x_opt, y_opt, relative);
            Ok(JsValue::UNDEFINED)
        });
    }

    // window.matchMedia(query)
    let h = Rc::clone(host);
    define_host_method(vm, global, "matchMedia", move |vm, _, args| {
        let query = vm.value_to_rust_string(arg(args, 0));
        let (vp, dark) = {
            let hs = h.borrow();
            (hs.viewport, hs.dark_mode)
        };
        let matches = evaluate_simple_media_query(&query, vp.width, vp.height, dark);
        let mql = vm.alloc_ordinary_object();
        let k_matches = vm.heap.intern_key("matches");
        let k_media = vm.heap.intern_key("media");
        let media_s = JsValue::from_string(vm.heap.alloc_string(&query));
        vm.heap.object_define_data(
            mql,
            k_matches,
            JsValue::from_bool(matches),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            mql,
            k_media,
            media_s,
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        define_host_method(vm, mql, "addEventListener", |_, _, _| {
            Ok(JsValue::UNDEFINED)
        });
        define_host_method(vm, mql, "removeEventListener", |_, _, _| {
            Ok(JsValue::UNDEFINED)
        });
        define_host_method(vm, mql, "addListener", |_, _, _| Ok(JsValue::UNDEFINED));
        define_host_method(vm, mql, "removeListener", |_, _, _| Ok(JsValue::UNDEFINED));
        Ok(JsValue::from_object(mql))
    });

    // window.requestIdleCallback / cancelIdleCallback
    let el = Rc::clone(event_loop);
    define_host_method(vm, global, "requestIdleCallback", move |vm, _, args| {
        let Some(cb) = arg(args, 0)
            .as_object()
            .filter(|&h| vm.is_callable_handle(h))
        else {
            return Ok(JsValue::from_i32(0));
        };
        let deadline = vm.alloc_ordinary_object();
        let k_did = vm.heap.intern_key("didTimeout");
        vm.heap.object_define_data(
            deadline,
            k_did,
            JsValue::FALSE,
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        define_host_method(vm, deadline, "timeRemaining", |_, _, _| {
            Ok(JsValue::from_number(12.0))
        });
        let id =
            el.borrow_mut()
                .schedule_timer(cb, 1.0, vec![JsValue::from_object(deadline)], false);
        Ok(JsValue::from_i32(id as i32))
    });
    let el = Rc::clone(event_loop);
    define_host_method(vm, global, "cancelIdleCallback", move |vm, _, args| {
        let id = vm.to_number(arg(args, 0))? as u32;
        el.borrow_mut().clear_timer(id);
        Ok(JsValue::UNDEFINED)
    });

    // navigator object (preserved & extended by service_worker.rs and webgpu.rs).
    let nav = vm.alloc_ordinary_object();
    let ua = JsValue::from_string(
        vm.heap
            .alloc_string("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) RixBrowser/0.2.0 Safari/537.36"),
    );
    let lang = JsValue::from_string(vm.heap.alloc_string("en-US"));
    let langs = vm.alloc_array();
    vm.heap.object_push_element(langs, lang);
    let plat = JsValue::from_string(vm.heap.alloc_string("Linux x86_64"));
    let cores = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get) as f64;
    for (k, v) in [
        ("userAgent", ua),
        ("language", lang),
        ("languages", JsValue::from_object(langs)),
        ("platform", plat),
        ("hardwareConcurrency", JsValue::from_number(cores)),
        ("onLine", JsValue::TRUE),
        ("cookieEnabled", JsValue::TRUE),
    ] {
        let key = vm.heap.intern_key(k);
        vm.heap
            .object_define_data(nav, key, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }
    {
        let wa_sb = Rc::clone(web_apis);
        define_host_method(vm, nav, "sendBeacon", move |vm, _, args| {
            if args.is_empty() {
                return Err(vm.type_error("navigator.sendBeacon requires a url argument"));
            }
            let raw_url = vm.value_to_rust_string(arg(args, 0));
            let base = wa_sb.borrow().location_url.clone();
            let parsed = base
                .join(&raw_url)
                .map_err(|_| vm.type_error(format!("Invalid URL: {raw_url}")))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(vm.type_error("navigator.sendBeacon only supports http(s) URLs"));
            }
            Ok(JsValue::TRUE)
        });
    }
    define_global(vm, "navigator", JsValue::from_object(nav));

    // HTML §8.4 base64 utilities: `btoa` & `atob`
    define_host_method(vm, global, "btoa", |vm, _, args| {
        if args.is_empty() {
            return Err(vm.type_error("btoa requires 1 argument"));
        }
        let s = vm.value_to_rust_string(arg(args, 0));
        let mut bytes = Vec::with_capacity(s.len());
        for ch in s.chars() {
            let cp = ch as u32;
            if cp > 0xFF {
                return Err(create_dom_exception(
                    vm,
                    "InvalidCharacterError",
                    "The string to be encoded contains characters outside of the Latin1 range.",
                ));
            }
            bytes.push(cp as u8);
        }
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        let mut i = 0;
        while i < bytes.len() {
            let b0 = bytes[i];
            let b1 = if i + 1 < bytes.len() { bytes[i + 1] } else { 0 };
            let b2 = if i + 2 < bytes.len() { bytes[i + 2] } else { 0 };
            out.push(TABLE[(b0 >> 2) as usize] as char);
            out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
            if i + 1 < bytes.len() {
                out.push(TABLE[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char);
            } else {
                out.push('=');
            }
            if i + 2 < bytes.len() {
                out.push(TABLE[(b2 & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
            i += 3;
        }
        Ok(JsValue::from_string(vm.heap.alloc_string(&out)))
    });

    define_host_method(vm, global, "atob", |vm, _, args| {
        if args.is_empty() {
            return Err(vm.type_error("atob requires 1 argument"));
        }
        let raw = vm.value_to_rust_string(arg(args, 0));
        let cleaned: Vec<u8> = raw
            .bytes()
            .filter(|b| !matches!(b, b'\t' | b'\n' | b'\x0c' | b'\r' | b' '))
            .collect();
        let mut bytes = cleaned.as_slice();
        if bytes.len().is_multiple_of(4) {
            if bytes.ends_with(b"==") {
                bytes = &bytes[..bytes.len() - 2];
            } else if bytes.ends_with(b"=") {
                bytes = &bytes[..bytes.len() - 1];
            }
        }
        if bytes.len() % 4 == 1 {
            return Err(create_dom_exception(
                vm,
                "InvalidCharacterError",
                "The string to be decoded is not correctly encoded.",
            ));
        }
        let decode_val = |b: u8| -> Option<u32> {
            match b {
                b'A'..=b'Z' => Some(u32::from(b - b'A')),
                b'a'..=b'z' => Some(u32::from(b - b'a') + 26),
                b'0'..=b'9' => Some(u32::from(b - b'0') + 52),
                b'+' => Some(62),
                b'/' => Some(63),
                _ => None,
            }
        };
        let mut out = String::with_capacity(bytes.len());
        let mut acc = 0u32;
        let mut bits = 0u32;
        for &b in bytes {
            let Some(v) = decode_val(b) else {
                return Err(create_dom_exception(
                    vm,
                    "InvalidCharacterError",
                    "The string to be decoded is not correctly encoded.",
                ));
            };
            acc = (acc << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                let byte = ((acc >> bits) & 0xFF) as u8;
                out.push(char::from(byte));
            }
        }
        Ok(JsValue::from_string(vm.heap.alloc_string(&out)))
    });
}

fn evaluate_simple_media_query(query: &str, vw: f32, vh: f32, dark_mode: bool) -> bool {
    let lower = query.trim().to_ascii_lowercase();
    if lower.is_empty() || lower == "all" || lower == "screen" {
        return true;
    }
    if lower == "print" {
        return false;
    }
    if lower.contains("prefers-color-scheme: dark") || lower.contains("prefers-color-scheme:dark") {
        return dark_mode;
    }
    if lower.contains("prefers-color-scheme: light") || lower.contains("prefers-color-scheme:light")
    {
        return !dark_mode;
    }
    if lower.contains("prefers-reduced-motion: reduce") {
        return false;
    }
    if let Some(px) = extract_media_px(&lower, "min-width:") {
        return vw >= px;
    }
    if let Some(px) = extract_media_px(&lower, "max-width:") {
        return vw <= px;
    }
    if let Some(px) = extract_media_px(&lower, "min-height:") {
        return vh >= px;
    }
    if let Some(px) = extract_media_px(&lower, "max-height:") {
        return vh <= px;
    }
    true
}

fn extract_media_px(haystack: &str, key: &str) -> Option<f32> {
    let idx = haystack.find(key)?;
    let rest = haystack[idx + key.len()..].trim_start();
    let num_end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(rest.len());
    rest[..num_end].parse::<f32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whatwg_url_and_form_charset_encoding_matches_spec() {
        let base = Url::parse("https://example.com/").unwrap();
        let u = parse_url_with_charset("https://example.com/?¡", &base, "EUC-KR").unwrap();
        assert_eq!(u.query(), Some("%A2%AE"));
        let u_err = parse_url_with_charset("https://example.com/?丂", &base, "EUC-KR").unwrap();
        assert_eq!(u_err.query(), Some("%26%2319970%3B"));
        let form =
            form_urlencode_pairs_with_charset(&[("q".to_string(), "¡ 丂".to_string())], "EUC-KR");
        assert_eq!(form, "q=%A2%AE+%26%2319970%3B");
    }
}
