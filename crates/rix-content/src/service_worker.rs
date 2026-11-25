//! W3C `CacheStorage` / `Cache` (backed by `rix-storage` `redb`) and
//! `ServiceWorker` lifecycle (`register`, `install`, `activate`, `fetch` interception,
//! `unregister`) running in an isolated JS VM thread with zero polling.
//!
//! Reference: Spec 06 §3, Issue #24.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rix_core::Size;
use rix_dom::Document;
use rix_js::promise::{
    alloc_promise, promise_resolve_value, promise_then, reject_promise, resolve_promise,
};
use rix_js::{HeapHandle, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_net::{
    FetchCredentials, FetchEngine, FetchMode, FetchRedirect, FetchRequest, FetchResponse,
    FetchResponseType,
};
use rix_storage::{CachedHttpResponse, StorageEngine};
use rix_text::FontDatabase;
use tokio::sync::{mpsc, oneshot};
use url::Url;

use crate::dom_bridge::{install_dom_bridge, SharedHost};
use crate::event_loop::{
    drain_ready_tasks, install_event_loop_globals, run_microtask_checkpoint, EventLoopState,
    SharedEventLoop,
};
use crate::events::{fire_object_event_with_proto, install_event_system, SharedEvents};
use crate::fetch_ws::{
    alloc_request_from_url, alloc_response_from_cached, deliver_async_net_event,
    extract_request_url_and_method, extract_response_snapshot, install_fetch_and_websocket,
    FetchFailure, SharedNetBridge,
};
use crate::streams::{install_streams, SharedStreams};
use crate::web_apis::{
    arg, bind_ctor_proto, create_dom_exception, define_global, define_host_getter,
    define_host_method, install_web_apis, origin_of_url, SharedWebApis,
};

static NEXT_SW_ID: AtomicU64 = AtomicU64::new(1);

fn next_sw_id() -> u64 {
    NEXT_SW_ID.fetch_add(1, Ordering::Relaxed)
}

fn alloc_js_array(vm: &mut JsVm, items: &[JsValue]) -> HeapHandle {
    let arr = vm.alloc_array();
    for &v in items {
        vm.heap.object_push_element(arr, v);
    }
    arr
}

// ---------------------------------------------------------------------------
// Process-wide active ServiceWorker registry (keyed by (origin, scope))
// ---------------------------------------------------------------------------

enum SwCommand {
    InstallAndActivate {
        reply_tx: oneshot::Sender<Result<(), String>>,
    },
    Fetch {
        request: FetchRequest,
        reply_tx: oneshot::Sender<Option<Result<FetchResponse, FetchFailure>>>,
    },
    Terminate,
}

#[derive(Clone)]
struct ActiveServiceWorker {
    origin: String,
    scope: String,
    script_url: String,
    cmd_tx: mpsc::UnboundedSender<SwCommand>,
}

type SwRegistryMap = HashMap<(String, String), ActiveServiceWorker>;
static SERVICE_WORKERS: OnceLock<Mutex<SwRegistryMap>> = OnceLock::new();

fn sw_registry() -> &'static Mutex<SwRegistryMap> {
    SERVICE_WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn find_matching_sw(origin: &str, url: &str) -> Option<ActiveServiceWorker> {
    let reg = sw_registry().lock().ok()?;
    reg.values()
        .filter(|sw| sw.origin == origin && url.starts_with(&sw.scope))
        .max_by_key(|sw| sw.scope.len())
        .cloned()
}

// ---------------------------------------------------------------------------
// Reactive AsyncSwEvent delivered to ContentRuntime's tokio::select! loop
// ---------------------------------------------------------------------------

/// Reactive completion event emitted by asynchronous `ServiceWorker` lifecycle tasks.
#[derive(Debug)]
pub enum AsyncSwEvent {
    Registered {
        pending_id: u64,
        result: Result<RegisteredSwInfo, String>,
    },
}

#[derive(Debug, Clone)]
pub struct RegisteredSwInfo {
    origin: String,
    scope: String,
    script_url: String,
}

// ---------------------------------------------------------------------------
// Per-runtime ServiceWorker + CacheStorage state
// ---------------------------------------------------------------------------

struct PendingRegistration {
    promise: HeapHandle,
    cmd_tx: mpsc::UnboundedSender<SwCommand>,
}

struct SwRegistrationRecord {
    origin: String,
    scope: String,
    active_sw: Option<HeapHandle>,
    unregistered: bool,
}

struct SwWorkerRecord {
    script_url: String,
    state: String,
}

#[derive(Default)]
struct SwPrototypes {
    cache_storage_proto: Option<HeapHandle>,
    cache_proto: Option<HeapHandle>,
    sw_container_proto: Option<HeapHandle>,
    sw_registration_proto: Option<HeapHandle>,
    sw_proto: Option<HeapHandle>,
}

/// Content-runtime registry for `CacheStorage`, `Cache`, and `navigator.serviceWorker`.
pub struct ServiceWorkerSystem {
    async_tx: mpsc::UnboundedSender<AsyncSwEvent>,
    pub(crate) async_rx: RefCell<Option<mpsc::UnboundedReceiver<AsyncSwEvent>>>,
    caches: HashMap<HeapHandle, String>,
    pending_regs: HashMap<u64, PendingRegistration>,
    registrations: HashMap<HeapHandle, SwRegistrationRecord>,
    workers: HashMap<HeapHandle, SwWorkerRecord>,
    controller: Option<HeapHandle>,
    ready_promise: Option<HeapHandle>,
    ready_resolved: bool,
    protos: SwPrototypes,
}

/// Shared handle to [`ServiceWorkerSystem`].
pub type SharedServiceWorkers = Rc<RefCell<ServiceWorkerSystem>>;

impl ServiceWorkerSystem {
    #[must_use]
    pub fn new() -> Self {
        let (async_tx, async_rx) = mpsc::unbounded_channel();
        Self {
            async_tx,
            async_rx: RefCell::new(Some(async_rx)),
            caches: HashMap::new(),
            pending_regs: HashMap::new(),
            registrations: HashMap::new(),
            workers: HashMap::new(),
            controller: None,
            ready_promise: None,
            ready_resolved: false,
            protos: SwPrototypes::default(),
        }
    }

    /// Collects GC roots for active registrations, pending promises, controller, and prototypes.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.protos.cache_storage_proto,
            self.protos.cache_proto,
            self.protos.sw_container_proto,
            self.protos.sw_registration_proto,
            self.protos.sw_proto,
            self.controller,
            self.ready_promise,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(p));
        }
        for pr in self.pending_regs.values() {
            out.push(JsValue::from_object(pr.promise));
        }
        for (&rh, rec) in &self.registrations {
            if !rec.unregistered {
                out.push(JsValue::from_object(rh));
                if let Some(aw) = rec.active_sw {
                    out.push(JsValue::from_object(aw));
                }
            }
        }
    }

    /// Sweeps dead JS wrapper handles after Major GC.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.caches.retain(|h, _| heap.is_alive(*h));
        self.registrations
            .retain(|h, rec| !rec.unregistered || heap.is_alive(*h));
        self.workers.retain(|h, _| heap.is_alive(*h));
    }
}

impl Default for ServiceWorkerSystem {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// CacheStorage + Cache installation (shared between Window & ServiceWorker!)
// ---------------------------------------------------------------------------

/// Installs `CacheStorage`, `Cache`, and the global `caches` object backed by `rix-storage`.
pub fn install_cache_storage(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    net: &SharedNetBridge,
    sw_sys: &SharedServiceWorkers,
) {
    let cs_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    let cache_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    {
        let mut st = sw_sys.borrow_mut();
        st.protos.cache_storage_proto = Some(cs_proto);
        st.protos.cache_proto = Some(cache_proto);
    }

    // Cache.prototype.put(request, response) -> Promise<undefined>
    {
        let wa = Rc::clone(web_apis);
        let nb = Rc::clone(net);
        let sw = Rc::clone(sw_sys);
        define_host_method(vm, cache_proto, "put", move |vm, this_val, args| {
            let p = alloc_promise(vm);
            let Some(cache_name) = this_val
                .as_object()
                .and_then(|h| sw.borrow().caches.get(&h).cloned())
            else {
                let err = create_dom_exception(vm, "TypeError", "Illegal invocation on Cache");
                reject_promise(vm, p, err.value);
                return Ok(JsValue::from_object(p));
            };
            let (url, method) = match extract_request_url_and_method(vm, &wa, &nb, arg(args, 0)) {
                Ok(v) => v,
                Err(e) => {
                    reject_promise(vm, p, e.value);
                    return Ok(JsValue::from_object(p));
                }
            };
            if !method.eq_ignore_ascii_case("GET") {
                let err =
                    create_dom_exception(vm, "TypeError", "Cache.put only supports GET requests");
                reject_promise(vm, p, err.value);
                return Ok(JsValue::from_object(p));
            }
            let Some(mut snapshot) = extract_response_snapshot(&nb, arg(args, 1)) else {
                let err = create_dom_exception(
                    vm,
                    "TypeError",
                    "Cache.put second argument must be a Response",
                );
                reject_promise(vm, p, err.value);
                return Ok(JsValue::from_object(p));
            };
            if snapshot.url.is_empty() {
                snapshot.url = url.clone();
            }
            let (origin, storage) = {
                let s = wa.borrow();
                (s.current_origin(), s.storage.clone())
            };
            match storage.cache_put(&origin, &cache_name, &url, snapshot) {
                Ok(()) => resolve_promise(vm, p, JsValue::UNDEFINED),
                Err(e) => {
                    let err = create_dom_exception(vm, "QuotaExceededError", &e.to_string());
                    reject_promise(vm, p, err.value);
                }
            }
            Ok(JsValue::from_object(p))
        });
    }

    // Cache.prototype.match(request) -> Promise<Response | undefined>
    {
        let wa = Rc::clone(web_apis);
        let st_r = Rc::clone(streams);
        let nb = Rc::clone(net);
        let sw = Rc::clone(sw_sys);
        define_host_method(vm, cache_proto, "match", move |vm, this_val, args| {
            let p = alloc_promise(vm);
            let Some(cache_name) = this_val
                .as_object()
                .and_then(|h| sw.borrow().caches.get(&h).cloned())
            else {
                let err = create_dom_exception(vm, "TypeError", "Illegal invocation on Cache");
                reject_promise(vm, p, err.value);
                return Ok(JsValue::from_object(p));
            };
            let (url, _) = match extract_request_url_and_method(vm, &wa, &nb, arg(args, 0)) {
                Ok(v) => v,
                Err(e) => {
                    reject_promise(vm, p, e.value);
                    return Ok(JsValue::from_object(p));
                }
            };
            let (origin, storage) = {
                let s = wa.borrow();
                (s.current_origin(), s.storage.clone())
            };
            match storage.cache_match(&origin, &cache_name, &url) {
                Ok(Some(cached)) => {
                    let resp_h = alloc_response_from_cached(vm, &st_r, &nb, cached);
                    resolve_promise(vm, p, JsValue::from_object(resp_h));
                }
                Ok(None) => resolve_promise(vm, p, JsValue::UNDEFINED),
                Err(e) => {
                    let err = create_dom_exception(vm, "OperationError", &e.to_string());
                    reject_promise(vm, p, err.value);
                }
            }
            Ok(JsValue::from_object(p))
        });
    }

    // Cache.prototype.matchAll(request?) -> Promise<Response[]>
    {
        let wa = Rc::clone(web_apis);
        let st_r = Rc::clone(streams);
        let nb = Rc::clone(net);
        let sw = Rc::clone(sw_sys);
        define_host_method(vm, cache_proto, "matchAll", move |vm, this_val, args| {
            let p = alloc_promise(vm);
            let Some(cache_name) = this_val
                .as_object()
                .and_then(|h| sw.borrow().caches.get(&h).cloned())
            else {
                let arr = alloc_js_array(vm, &[]);
                resolve_promise(vm, p, JsValue::from_object(arr));
                return Ok(JsValue::from_object(p));
            };
            let filter_url = if arg(args, 0).is_undefined() {
                None
            } else {
                extract_request_url_and_method(vm, &wa, &nb, arg(args, 0))
                    .ok()
                    .map(|(u, _)| u)
            };
            let (origin, storage) = {
                let s = wa.borrow();
                (s.current_origin(), s.storage.clone())
            };
            let mut responses = Vec::new();
            if let Some(u) = filter_url {
                if let Ok(Some(c)) = storage.cache_match(&origin, &cache_name, &u) {
                    let h = alloc_response_from_cached(vm, &st_r, &nb, c);
                    responses.push(JsValue::from_object(h));
                }
            } else if let Ok(keys) = storage.cache_keys(&origin, &cache_name) {
                for u in keys {
                    if let Ok(Some(c)) = storage.cache_match(&origin, &cache_name, &u) {
                        let h = alloc_response_from_cached(vm, &st_r, &nb, c);
                        responses.push(JsValue::from_object(h));
                    }
                }
            }
            let arr = alloc_js_array(vm, &responses);
            resolve_promise(vm, p, JsValue::from_object(arr));
            Ok(JsValue::from_object(p))
        });
    }

    // Cache.prototype.delete(request) -> Promise<boolean>
    {
        let wa = Rc::clone(web_apis);
        let nb = Rc::clone(net);
        let sw = Rc::clone(sw_sys);
        define_host_method(vm, cache_proto, "delete", move |vm, this_val, args| {
            let p = alloc_promise(vm);
            let Some(cache_name) = this_val
                .as_object()
                .and_then(|h| sw.borrow().caches.get(&h).cloned())
            else {
                resolve_promise(vm, p, JsValue::from_bool(false));
                return Ok(JsValue::from_object(p));
            };
            let (url, _) = match extract_request_url_and_method(vm, &wa, &nb, arg(args, 0)) {
                Ok(v) => v,
                Err(e) => {
                    reject_promise(vm, p, e.value);
                    return Ok(JsValue::from_object(p));
                }
            };
            let (origin, storage) = {
                let s = wa.borrow();
                (s.current_origin(), s.storage.clone())
            };
            let removed = storage
                .cache_delete_entry(&origin, &cache_name, &url)
                .unwrap_or(false);
            resolve_promise(vm, p, JsValue::from_bool(removed));
            Ok(JsValue::from_object(p))
        });
    }

    // Cache.prototype.keys() -> Promise<Request[]>
    {
        let wa = Rc::clone(web_apis);
        let nb = Rc::clone(net);
        let sw = Rc::clone(sw_sys);
        define_host_method(vm, cache_proto, "keys", move |vm, this_val, _| {
            let p = alloc_promise(vm);
            let Some(cache_name) = this_val
                .as_object()
                .and_then(|h| sw.borrow().caches.get(&h).cloned())
            else {
                let arr = alloc_js_array(vm, &[]);
                resolve_promise(vm, p, JsValue::from_object(arr));
                return Ok(JsValue::from_object(p));
            };
            let (origin, storage) = {
                let s = wa.borrow();
                (s.current_origin(), s.storage.clone())
            };
            let urls = storage.cache_keys(&origin, &cache_name).unwrap_or_default();
            let reqs: Vec<JsValue> = urls
                .into_iter()
                .map(|u| JsValue::from_object(alloc_request_from_url(vm, &nb, u)))
                .collect();
            let arr = alloc_js_array(vm, &reqs);
            resolve_promise(vm, p, JsValue::from_object(arr));
            Ok(JsValue::from_object(p))
        });
    }

    // Cache.prototype.add(request) -> Promise<undefined>
    {
        let wa = Rc::clone(web_apis);
        let nb = Rc::clone(net);
        let sw = Rc::clone(sw_sys);
        define_host_method(vm, cache_proto, "add", move |vm, this_val, args| {
            cache_add_single(vm, &wa, &nb, &sw, this_val, arg(args, 0))
        });
    }

    // Cache.prototype.addAll(requests) -> Promise<undefined>
    {
        let wa = Rc::clone(web_apis);
        let nb = Rc::clone(net);
        let sw = Rc::clone(sw_sys);
        define_host_method(vm, cache_proto, "addAll", move |vm, this_val, args| {
            let Some(arr_h) = arg(args, 0).as_object() else {
                let p = alloc_promise(vm);
                resolve_promise(vm, p, JsValue::UNDEFINED);
                return Ok(JsValue::from_object(p));
            };
            let len = vm
                .heap
                .get_object(arr_h)
                .map_or(0, |o| o.elements.len() as usize);
            let mut req_vals = Vec::with_capacity(len);
            for i in 0..len {
                if let Ok(idx) = u32::try_from(i) {
                    if let Some(v) = vm.heap.get_object(arr_h).and_then(|o| o.elements.get(idx)) {
                        req_vals.push(v);
                    }
                }
            }
            if req_vals.is_empty() {
                let p = alloc_promise(vm);
                resolve_promise(vm, p, JsValue::UNDEFINED);
                return Ok(JsValue::from_object(p));
            }
            let out_promise = alloc_promise(vm);
            let remaining = Rc::new(RefCell::new(req_vals.len()));
            let failed = Rc::new(RefCell::new(false));
            for rv in req_vals {
                let item_p_val = cache_add_single(vm, &wa, &nb, &sw, this_val, rv)?;
                let item_p = promise_resolve_value(vm, item_p_val);
                let rem_ok = Rc::clone(&remaining);
                let fail_ok = Rc::clone(&failed);
                let on_ok = vm.alloc_host_fn("addAllOk", move |vm, _, _| {
                    if *fail_ok.borrow() {
                        return Ok(JsValue::UNDEFINED);
                    }
                    let mut r = rem_ok.borrow_mut();
                    *r = r.saturating_sub(1);
                    if *r == 0 {
                        resolve_promise(vm, out_promise, JsValue::UNDEFINED);
                    }
                    Ok(JsValue::UNDEFINED)
                });
                let fail_err = Rc::clone(&failed);
                let on_err = vm.alloc_host_fn("addAllErr", move |vm, _, args| {
                    if !*fail_err.borrow() {
                        *fail_err.borrow_mut() = true;
                        reject_promise(vm, out_promise, arg(args, 0));
                    }
                    Ok(JsValue::UNDEFINED)
                });
                let _ = promise_then(vm, item_p, Some(on_ok), Some(on_err));
            }
            Ok(JsValue::from_object(out_promise))
        });
    }

    // CacheStorage.prototype.open(name) -> Promise<Cache>
    {
        let wa = Rc::clone(web_apis);
        let sw = Rc::clone(sw_sys);
        define_host_method(vm, cs_proto, "open", move |vm, _, args| {
            let p = alloc_promise(vm);
            let name = vm.value_to_rust_string(arg(args, 0));
            let (origin, storage) = {
                let s = wa.borrow();
                (s.current_origin(), s.storage.clone())
            };
            if let Err(e) = storage.cache_open(&origin, &name) {
                let err = create_dom_exception(vm, "OperationError", &e.to_string());
                reject_promise(vm, p, err.value);
                return Ok(JsValue::from_object(p));
            }
            let obj = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(cache_proto)));
            sw.borrow_mut().caches.insert(obj, name);
            resolve_promise(vm, p, JsValue::from_object(obj));
            Ok(JsValue::from_object(p))
        });
    }

    // CacheStorage.prototype.has(name) -> Promise<boolean>
    {
        let wa = Rc::clone(web_apis);
        define_host_method(vm, cs_proto, "has", move |vm, _, args| {
            let p = alloc_promise(vm);
            let name = vm.value_to_rust_string(arg(args, 0));
            let (origin, storage) = {
                let s = wa.borrow();
                (s.current_origin(), s.storage.clone())
            };
            let exists = storage.cache_has(&origin, &name).unwrap_or(false);
            resolve_promise(vm, p, JsValue::from_bool(exists));
            Ok(JsValue::from_object(p))
        });
    }

    // CacheStorage.prototype.delete(name) -> Promise<boolean>
    {
        let wa = Rc::clone(web_apis);
        define_host_method(vm, cs_proto, "delete", move |vm, _, args| {
            let p = alloc_promise(vm);
            let name = vm.value_to_rust_string(arg(args, 0));
            let (origin, storage) = {
                let s = wa.borrow();
                (s.current_origin(), s.storage.clone())
            };
            let deleted = storage.cache_delete_bucket(&origin, &name).unwrap_or(false);
            resolve_promise(vm, p, JsValue::from_bool(deleted));
            Ok(JsValue::from_object(p))
        });
    }

    // CacheStorage.prototype.keys() -> Promise<string[]>
    {
        let wa = Rc::clone(web_apis);
        define_host_method(vm, cs_proto, "keys", move |vm, _, _| {
            let p = alloc_promise(vm);
            let (origin, storage) = {
                let s = wa.borrow();
                (s.current_origin(), s.storage.clone())
            };
            let names = storage.cache_list_names(&origin).unwrap_or_default();
            let vals: Vec<JsValue> = names
                .into_iter()
                .map(|n| JsValue::from_string(vm.heap.alloc_string(&n)))
                .collect();
            let arr = alloc_js_array(vm, &vals);
            resolve_promise(vm, p, JsValue::from_object(arr));
            Ok(JsValue::from_object(p))
        });
    }

    // CacheStorage.prototype.match(request, options?) -> Promise<Response | undefined>
    {
        let wa = Rc::clone(web_apis);
        let st_r = Rc::clone(streams);
        let nb = Rc::clone(net);
        define_host_method(vm, cs_proto, "match", move |vm, _, args| {
            let p = alloc_promise(vm);
            let (url, _) = match extract_request_url_and_method(vm, &wa, &nb, arg(args, 0)) {
                Ok(v) => v,
                Err(e) => {
                    reject_promise(vm, p, e.value);
                    return Ok(JsValue::from_object(p));
                }
            };
            let cache_name_opt = arg(args, 1).as_object().and_then(|opts| {
                let k = vm.heap.intern_key("cacheName");
                vm.get_property_by_key(opts, k)
                    .ok()
                    .filter(|v| !v.is_undefined() && !v.is_null())
                    .map(|v| vm.value_to_rust_string(v))
            });
            let (origin, storage) = {
                let s = wa.borrow();
                (s.current_origin(), s.storage.clone())
            };
            let res = match cache_name_opt {
                Some(name) => storage.cache_match(&origin, &name, &url),
                None => storage.cache_match_any(&origin, &url),
            };
            match res {
                Ok(Some(cached)) => {
                    let h = alloc_response_from_cached(vm, &st_r, &nb, cached);
                    resolve_promise(vm, p, JsValue::from_object(h));
                }
                Ok(None) => resolve_promise(vm, p, JsValue::UNDEFINED),
                Err(e) => {
                    let err = create_dom_exception(vm, "OperationError", &e.to_string());
                    reject_promise(vm, p, err.value);
                }
            }
            Ok(JsValue::from_object(p))
        });
    }

    let caches_obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(cs_proto)));
    define_global(vm, "caches", JsValue::from_object(caches_obj));

    let cs_ctor = vm.alloc_host_fn("CacheStorage", |vm, _, _| {
        Err(create_dom_exception(vm, "TypeError", "Illegal constructor"))
    });
    bind_ctor_proto(vm, cs_ctor, cs_proto);
    define_global(vm, "CacheStorage", JsValue::from_object(cs_ctor));

    let c_ctor = vm.alloc_host_fn("Cache", |vm, _, _| {
        Err(create_dom_exception(vm, "TypeError", "Illegal constructor"))
    });
    bind_ctor_proto(vm, c_ctor, cache_proto);
    define_global(vm, "Cache", JsValue::from_object(c_ctor));
}

fn cache_add_single(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    sw_sys: &SharedServiceWorkers,
    this_val: JsValue,
    req_val: JsValue,
) -> Result<JsValue, rix_js::JsException> {
    let out_p = alloc_promise(vm);
    let Some(cache_name) = this_val
        .as_object()
        .and_then(|h| sw_sys.borrow().caches.get(&h).cloned())
    else {
        let err = create_dom_exception(vm, "TypeError", "Illegal invocation on Cache");
        reject_promise(vm, out_p, err.value);
        return Ok(JsValue::from_object(out_p));
    };
    let (url, method) = match extract_request_url_and_method(vm, web_apis, net, req_val) {
        Ok(v) => v,
        Err(e) => {
            reject_promise(vm, out_p, e.value);
            return Ok(JsValue::from_object(out_p));
        }
    };
    if !method.eq_ignore_ascii_case("GET") {
        let err = create_dom_exception(vm, "TypeError", "Cache.add only supports GET requests");
        reject_promise(vm, out_p, err.value);
        return Ok(JsValue::from_object(out_p));
    }
    let k_fetch = vm.heap.intern_key("fetch");
    let fetch_fn = vm.get_property_by_key(vm.global, k_fetch)?;
    let fetch_p_val = vm.call_value(fetch_fn, JsValue::UNDEFINED, &[req_val])?;
    let fetch_p = promise_resolve_value(vm, fetch_p_val);

    let wa = Rc::clone(web_apis);
    let nb = Rc::clone(net);
    let url_c = url.clone();
    let on_ok = vm.alloc_host_fn("cacheAddOk", move |vm, _, args| {
        let resp_val = arg(args, 0);
        let Some(mut snap) = extract_response_snapshot(&nb, resp_val) else {
            let err = create_dom_exception(vm, "TypeError", "Invalid Response in Cache.add");
            reject_promise(vm, out_p, err.value);
            return Ok(JsValue::UNDEFINED);
        };
        if !(200..300).contains(&snap.status) {
            let err = create_dom_exception(
                vm,
                "TypeError",
                &format!("Cache.add received non-OK status {}", snap.status),
            );
            reject_promise(vm, out_p, err.value);
            return Ok(JsValue::UNDEFINED);
        }
        if snap.url.is_empty() {
            snap.url = url_c.clone();
        }
        let (origin, storage) = {
            let s = wa.borrow();
            (s.current_origin(), s.storage.clone())
        };
        match storage.cache_put(&origin, &cache_name, &url_c, snap) {
            Ok(()) => resolve_promise(vm, out_p, JsValue::UNDEFINED),
            Err(e) => {
                let err = create_dom_exception(vm, "OperationError", &e.to_string());
                reject_promise(vm, out_p, err.value);
            }
        }
        Ok(JsValue::UNDEFINED)
    });
    let on_err = vm.alloc_host_fn("cacheAddErr", move |vm, _, args| {
        reject_promise(vm, out_p, arg(args, 0));
        Ok(JsValue::UNDEFINED)
    });
    let _ = promise_then(vm, fetch_p, Some(on_ok), Some(on_err));
    Ok(JsValue::from_object(out_p))
}

// ---------------------------------------------------------------------------
// Isolated ServiceWorker OS thread runtime
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_service_worker_thread(
    script_url: String,
    origin: String,
    scope: String,
    inline_code: Option<String>,
    storage: StorageEngine,
    fetch_engine: Arc<FetchEngine>,
    mut cmd_rx: mpsc::UnboundedReceiver<SwCommand>,
) {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };

    let local = tokio::task::LocalSet::new();
    rt.block_on(local.run_until(async move {
        let code = if let Some(c) = inline_code {
            c
        } else {
            let req = FetchRequest {
                url: script_url.clone(),
                method: "GET".to_string(),
                headers: HashMap::new(),
                body: Vec::new(),
                mode: FetchMode::SameOrigin,
                credentials: FetchCredentials::SameOrigin,
                redirect: FetchRedirect::Follow,
                origin: origin.clone(),
                abort_signal: None,
            };
            match fetch_engine.fetch(req).await {
                Ok(resp) if (200..300).contains(&resp.status) => {
                    String::from_utf8_lossy(&resp.body).into_owned()
                }
                Ok(resp) => {
                    fail_install_loop(&mut cmd_rx, format!("HTTP status {}", resp.status)).await;
                    return;
                }
                Err(e) => {
                    fail_install_loop(&mut cmd_rx, format!("fetch failed: {e}")).await;
                    return;
                }
            }
        };

        let mut vm = JsVm::new();
        let _jit = rix_jit::TieredJit::attach_to(&mut vm);
        let event_loop = Rc::new(RefCell::new(EventLoopState::new()));
        install_event_loop_globals(&mut vm, &event_loop);
        let doc = Rc::new(RefCell::new(Document::new()));
        let font_db = Arc::new(FontDatabase::new());
        let host = install_dom_bridge(&mut vm, doc, Size::new(1.0, 1.0), font_db);
        let events = install_event_system(&mut vm, &host, &event_loop);
        let web_apis = install_web_apis(&mut vm, &host, &events, &event_loop, &script_url, storage);
        let streams = install_streams(&mut vm, &web_apis);
        // SW's own NetworkBridge has `fetch_interceptor: None` to avoid recursive interception!
        let net = install_fetch_and_websocket(
            &mut vm,
            &host,
            &events,
            &event_loop,
            &web_apis,
            &streams,
            Some(Arc::clone(&fetch_engine)),
        );
        let local_sw = Rc::new(RefCell::new(ServiceWorkerSystem::new()));
        install_cache_storage(&mut vm, &web_apis, &streams, &net, &local_sw);

        let g = vm.global;
        define_global(&mut vm, "self", JsValue::from_object(g));

        // `self.skipWaiting()` -> Promise<undefined>
        define_host_method(&mut vm, g, "skipWaiting", |vm, _, _| {
            let p = alloc_promise(vm);
            resolve_promise(vm, p, JsValue::UNDEFINED);
            Ok(JsValue::from_object(p))
        });

        // `self.clients` (`claim()`, `matchAll()`)
        let clients_h = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(vm.object_proto),
        ));
        define_host_method(&mut vm, clients_h, "claim", |vm, _, _| {
            let p = alloc_promise(vm);
            resolve_promise(vm, p, JsValue::UNDEFINED);
            Ok(JsValue::from_object(p))
        });
        define_host_method(&mut vm, clients_h, "matchAll", |vm, _, _| {
            let p = alloc_promise(vm);
            let arr = alloc_js_array(vm, &[]);
            resolve_promise(vm, p, JsValue::from_object(arr));
            Ok(JsValue::from_object(p))
        });
        define_global(&mut vm, "clients", JsValue::from_object(clients_h));

        // `self.registration` (`scope`)
        let reg_h = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(vm.object_proto),
        ));
        let scope_k = vm.heap.intern_key("scope");
        let scope_v = JsValue::from_string(vm.heap.alloc_string(&scope));
        vm.heap.object_define_data(
            reg_h,
            scope_k,
            scope_v,
            PropertyAttrs::READONLY,
            &mut vm.shapes,
        );
        define_global(&mut vm, "registration", JsValue::from_object(reg_h));

        // Take net_rx once for our reactive tokio::select! loop
        let Some(mut net_rx) = net.borrow().async_rx.borrow_mut().take() else {
            return;
        };

        if let Err(err) = vm.eval_script(&code) {
            fail_install_loop(&mut cmd_rx, err.to_string()).await;
            return;
        }
        run_microtask_checkpoint(&event_loop, &mut vm);
        drain_ready_tasks(&event_loop, &mut vm);

        let base_ev_proto = events.borrow().event_proto.unwrap_or(vm.object_proto);

        loop {
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

            let next_timer = event_loop
                .borrow_mut()
                .time_until_next_timer()
                .map(|d| tokio::time::Instant::now() + d);

            enum SwWake {
                Cmd(Option<SwCommand>),
                Net(Option<crate::fetch_ws::AsyncNetEvent>),
                Timer,
            }

            let wake = match next_timer {
                Some(deadline) => tokio::select! {
                    biased;
                    c = cmd_rx.recv() => SwWake::Cmd(c),
                    n = net_rx.recv() => SwWake::Net(n),
                    () = tokio::time::sleep_until(deadline) => SwWake::Timer,
                },
                None => tokio::select! {
                    biased;
                    c = cmd_rx.recv() => SwWake::Cmd(c),
                    n = net_rx.recv() => SwWake::Net(n),
                },
            };

            match wake {
                SwWake::Timer => {
                    drain_ready_tasks(&event_loop, &mut vm);
                }
                SwWake::Net(Some(nev)) => {
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
                SwWake::Net(None) | SwWake::Cmd(None) | SwWake::Cmd(Some(SwCommand::Terminate)) => {
                    break;
                }
                SwWake::Cmd(Some(SwCommand::InstallAndActivate { reply_tx })) => {
                    // Dispatch `install`, await waitUntil promises, then dispatch `activate`.
                    let install_res = dispatch_extendable_event_and_await(
                        &mut vm,
                        &host,
                        &events,
                        &event_loop,
                        &web_apis,
                        &streams,
                        &net,
                        &mut net_rx,
                        base_ev_proto,
                        "install",
                    )
                    .await;
                    if let Err(err) = install_res {
                        let _ = reply_tx.send(Err(err));
                        continue;
                    }
                    let activate_res = dispatch_extendable_event_and_await(
                        &mut vm,
                        &host,
                        &events,
                        &event_loop,
                        &web_apis,
                        &streams,
                        &net,
                        &mut net_rx,
                        base_ev_proto,
                        "activate",
                    )
                    .await;
                    let _ = reply_tx.send(activate_res);
                }
                SwWake::Cmd(Some(SwCommand::Fetch { request, reply_tx })) => {
                    handle_sw_fetch_event(
                        &mut vm,
                        &host,
                        &events,
                        &event_loop,
                        &net,
                        base_ev_proto,
                        request,
                        reply_tx,
                    );
                    run_microtask_checkpoint(&event_loop, &mut vm);
                    drain_ready_tasks(&event_loop, &mut vm);
                }
            }
        }
    }));
}

async fn fail_install_loop(cmd_rx: &mut mpsc::UnboundedReceiver<SwCommand>, msg: String) {
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            SwCommand::InstallAndActivate { reply_tx } => {
                let _ = reply_tx.send(Err(msg));
                return;
            }
            SwCommand::Fetch { reply_tx, .. } => {
                let _ = reply_tx.send(None);
            }
            SwCommand::Terminate => return,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_extendable_event_and_await(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    net: &SharedNetBridge,
    net_rx: &mut mpsc::UnboundedReceiver<crate::fetch_ws::AsyncNetEvent>,
    base_ev_proto: HeapHandle,
    ev_type: &str,
) -> Result<(), String> {
    let waited: Rc<RefCell<Vec<JsValue>>> = Rc::new(RefCell::new(Vec::new()));
    let waited_clone = Rc::clone(&waited);
    let wait_until_fn = vm.alloc_host_fn("waitUntil", move |_, _, args| {
        waited_clone.borrow_mut().push(arg(args, 0));
        Ok(JsValue::UNDEFINED)
    });
    let g = vm.global;
    fire_object_event_with_proto(
        vm,
        host,
        events,
        event_loop,
        g,
        ev_type,
        base_ev_proto,
        &[("waitUntil", JsValue::from_object(wait_until_fn))],
    );
    run_microtask_checkpoint(event_loop, vm);
    drain_ready_tasks(event_loop, vm);

    let promises: Vec<JsValue> = std::mem::take(&mut *waited.borrow_mut());
    if promises.is_empty() {
        return Ok(());
    }

    let remaining = Rc::new(RefCell::new(promises.len()));
    let error_msg: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    for pv in promises {
        let p = promise_resolve_value(vm, pv);
        let rem_ok = Rc::clone(&remaining);
        let on_ok = vm.alloc_host_fn("swWaitOk", move |_, _, _| {
            let mut r = rem_ok.borrow_mut();
            *r = r.saturating_sub(1);
            Ok(JsValue::UNDEFINED)
        });
        let rem_err = Rc::clone(&remaining);
        let err_slot = Rc::clone(&error_msg);
        let on_err = vm.alloc_host_fn("swWaitErr", move |vm, _, args| {
            let msg = vm.value_to_rust_string(arg(args, 0));
            let mut slot = err_slot.borrow_mut();
            if slot.is_none() {
                *slot = Some(msg);
            }
            let mut r = rem_err.borrow_mut();
            *r = r.saturating_sub(1);
            Ok(JsValue::UNDEFINED)
        });
        let _ = promise_then(vm, p, Some(on_ok), Some(on_err));
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        run_microtask_checkpoint(event_loop, vm);
        drain_ready_tasks(event_loop, vm);
        if *remaining.borrow() == 0 {
            break;
        }
        if let Some(err) = error_msg.borrow().clone() {
            return Err(err);
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(format!("ServiceWorker '{ev_type}' waitUntil timed out"));
        }
        let next_wake = match event_loop.borrow_mut().time_until_next_timer() {
            Some(d) => (now + d).min(deadline),
            None => deadline,
        };
        tokio::select! {
            biased;
            maybe_nev = net_rx.recv() => {
                if let Some(nev) = maybe_nev {
                    deliver_async_net_event(vm, host, events, event_loop, web_apis, streams, net, nev);
                } else {
                    break;
                }
            }
            () = tokio::time::sleep_until(next_wake) => {}
        }
    }
    let final_err = error_msg.borrow().clone();
    match final_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_sw_fetch_event(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    net: &SharedNetBridge,
    base_ev_proto: HeapHandle,
    request: FetchRequest,
    reply_tx: oneshot::Sender<Option<Result<FetchResponse, FetchFailure>>>,
) {
    let req_h = alloc_request_from_url(vm, net, request.url.clone());
    // Attach method/headers on Request object via JS Request constructor if non-GET
    let req_val = if request.method.eq_ignore_ascii_case("GET") && request.headers.is_empty() {
        JsValue::from_object(req_h)
    } else {
        let init_h = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(vm.object_proto),
        ));
        let mk = vm.heap.intern_key("method");
        let mv = JsValue::from_string(vm.heap.alloc_string(&request.method));
        vm.heap
            .object_define_data(init_h, mk, mv, PropertyAttrs::DEFAULT, &mut vm.shapes);
        let hdrs_h = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(vm.object_proto),
        ));
        for (hk, hv) in &request.headers {
            let k = vm.heap.intern_key(hk);
            let v = JsValue::from_string(vm.heap.alloc_string(hv));
            vm.heap
                .object_define_data(hdrs_h, k, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
        }
        let hkey = vm.heap.intern_key("headers");
        vm.heap.object_define_data(
            init_h,
            hkey,
            JsValue::from_object(hdrs_h),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        let rk = vm.heap.intern_key("Request");
        let url_js = JsValue::from_string(vm.heap.alloc_string(&request.url));
        vm.get_property_by_key(vm.global, rk)
            .ok()
            .and_then(|ctor| {
                vm.call_value(
                    ctor,
                    JsValue::UNDEFINED,
                    &[url_js, JsValue::from_object(init_h)],
                )
                .ok()
            })
            .unwrap_or(JsValue::from_object(req_h))
    };

    let responded: Rc<RefCell<Option<JsValue>>> = Rc::new(RefCell::new(None));
    let responded_c = Rc::clone(&responded);
    let respond_with_fn = vm.alloc_host_fn("respondWith", move |vm, _, args| {
        let mut slot = responded_c.borrow_mut();
        if slot.is_some() {
            return Err(create_dom_exception(
                vm,
                "InvalidStateError",
                "FetchEvent.respondWith() was already called",
            ));
        }
        *slot = Some(arg(args, 0));
        Ok(JsValue::UNDEFINED)
    });

    let client_id = JsValue::from_string(vm.heap.alloc_string("client-1"));
    let g = vm.global;
    fire_object_event_with_proto(
        vm,
        host,
        events,
        event_loop,
        g,
        "fetch",
        base_ev_proto,
        &[
            ("request", req_val),
            ("clientId", client_id),
            ("respondWith", JsValue::from_object(respond_with_fn)),
        ],
    );

    let Some(resp_or_promise) = responded.borrow_mut().take() else {
        let _ = reply_tx.send(None);
        return;
    };

    // Fast path: direct Response object passed to respondWith(new Response(...))
    if let Some(snap) = extract_response_snapshot(net, resp_or_promise) {
        let _ = reply_tx.send(Some(Ok(cached_to_fetch_response(snap, &request.url))));
        return;
    }

    // Promise path: attach reactive `.then(onFulfilled, onRejected)` sending directly on `reply_tx`!
    let p = promise_resolve_value(vm, resp_or_promise);
    let tx_slot = Rc::new(RefCell::new(Some(reply_tx)));
    let nb_ok = Rc::clone(net);
    let tx_ok = Rc::clone(&tx_slot);
    let req_url = request.url;
    let on_ok = vm.alloc_host_fn("swFetchOk", move |_, _, args| {
        let val = arg(args, 0);
        if let Some(tx) = tx_ok.borrow_mut().take() {
            if let Some(snap) = extract_response_snapshot(&nb_ok, val) {
                let _ = tx.send(Some(Ok(cached_to_fetch_response(snap, &req_url))));
            } else {
                let _ = tx.send(Some(Err(FetchFailure::TypeError(
                    "FetchEvent.respondWith resolved to non-Response value".to_string(),
                ))));
            }
        }
        Ok(JsValue::UNDEFINED)
    });
    let tx_err = Rc::clone(&tx_slot);
    let on_err = vm.alloc_host_fn("swFetchErr", move |vm, _, args| {
        let msg = vm.value_to_rust_string(arg(args, 0));
        if let Some(tx) = tx_err.borrow_mut().take() {
            let _ = tx.send(Some(Err(FetchFailure::TypeError(format!(
                "ServiceWorker respondWith rejected: {msg}"
            )))));
        }
        Ok(JsValue::UNDEFINED)
    });
    let _ = promise_then(vm, p, Some(on_ok), Some(on_err));
}

fn cached_to_fetch_response(snap: CachedHttpResponse, fallback_url: &str) -> FetchResponse {
    let mut headers = HashMap::new();
    for (k, v) in snap.headers {
        let lower = k.to_ascii_lowercase();
        headers
            .entry(lower)
            .and_modify(|e: &mut String| {
                e.push_str(", ");
                e.push_str(&v);
            })
            .or_insert(v);
    }
    FetchResponse {
        url: if snap.url.is_empty() {
            fallback_url.to_string()
        } else {
            snap.url
        },
        status: snap.status,
        status_text: snap.status_text,
        headers,
        body: snap.body,
        response_type: FetchResponseType::Basic,
        redirected: false,
    }
}

// ---------------------------------------------------------------------------
// Delivery into ContentRuntime event loop
// ---------------------------------------------------------------------------

/// Delivers a reactive [`AsyncSwEvent`] into `ContentRuntime`'s JS VM.
pub fn deliver_async_sw_event(
    vm: &mut JsVm,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    sw_sys: &SharedServiceWorkers,
    ev: AsyncSwEvent,
) {
    match ev {
        AsyncSwEvent::Registered { pending_id, result } => {
            let pending = sw_sys.borrow_mut().pending_regs.remove(&pending_id);
            let Some(pending) = pending else { return };
            match result {
                Ok(info) => {
                    if let Ok(mut reg) = sw_registry().lock() {
                        if let Some(old) = reg.insert(
                            (info.origin.clone(), info.scope.clone()),
                            ActiveServiceWorker {
                                origin: info.origin.clone(),
                                scope: info.scope.clone(),
                                script_url: info.script_url.clone(),
                                cmd_tx: pending.cmd_tx,
                            },
                        ) {
                            let _ = old.cmd_tx.send(SwCommand::Terminate);
                        }
                    }
                    let (reg_h, sw_h) = alloc_registration_objects(
                        vm,
                        sw_sys,
                        info.origin.clone(),
                        info.scope.clone(),
                        info.script_url,
                    );
                    let cur_href = web_apis.borrow().current_href();
                    let ready_to_resolve = {
                        let mut st = sw_sys.borrow_mut();
                        if cur_href.starts_with(&info.scope) {
                            st.controller = Some(sw_h);
                            if !st.ready_resolved {
                                if let Some(rp) = st.ready_promise {
                                    st.ready_resolved = true;
                                    Some(rp)
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };
                    if let Some(rp) = ready_to_resolve {
                        resolve_promise(vm, rp, JsValue::from_object(reg_h));
                    }
                    resolve_promise(vm, pending.promise, JsValue::from_object(reg_h));
                }
                Err(msg) => {
                    let err = create_dom_exception(vm, "TypeError", &msg);
                    reject_promise(vm, pending.promise, err.value);
                }
            }
        }
    }
    run_microtask_checkpoint(event_loop, vm);
}

fn alloc_registration_objects(
    vm: &mut JsVm,
    sw_sys: &SharedServiceWorkers,
    origin: String,
    scope: String,
    script_url: String,
) -> (HeapHandle, HeapHandle) {
    let (reg_proto, sw_proto) = {
        let st = sw_sys.borrow();
        (
            st.protos.sw_registration_proto.unwrap_or(vm.object_proto),
            st.protos.sw_proto.unwrap_or(vm.object_proto),
        )
    };
    let sw_h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(sw_proto)));
    let reg_h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(reg_proto)));
    let mut st = sw_sys.borrow_mut();
    st.workers.insert(
        sw_h,
        SwWorkerRecord {
            script_url: script_url.clone(),
            state: "activated".to_string(),
        },
    );
    st.registrations.insert(
        reg_h,
        SwRegistrationRecord {
            origin,
            scope,
            active_sw: Some(sw_h),
            unregistered: false,
        },
    );
    (reg_h, sw_h)
}

// ---------------------------------------------------------------------------
// Installation onto `ContentRuntime`'s `JsVm`
// ---------------------------------------------------------------------------

/// Installs `CacheStorage`, `Cache`, `navigator.serviceWorker`, and wires
/// active ServiceWorker `FetchEvent` interception into [`NetworkBridge`].
pub fn install_service_worker_and_cache_apis(
    vm: &mut JsVm,
    host: &SharedHost,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    net: &SharedNetBridge,
) -> SharedServiceWorkers {
    let sw_sys = Rc::new(RefCell::new(ServiceWorkerSystem::new()));
    install_cache_storage(vm, web_apis, streams, net, &sw_sys);

    // Wire zero-polling fetch interception hook into NetworkBridge!
    net.borrow_mut().fetch_interceptor = Some(Arc::new(|req: &FetchRequest| {
        let sw = find_matching_sw(&req.origin, &req.url)?;
        let (reply_tx, reply_rx) = oneshot::channel();
        sw.cmd_tx
            .send(SwCommand::Fetch {
                request: req.clone(),
                reply_tx,
            })
            .ok()?;
        Some(reply_rx)
    }));

    let event_target_proto = host.borrow().prototypes.event_target_proto;
    let sw_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(event_target_proto),
    ));
    let reg_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(event_target_proto),
    ));
    let container_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(event_target_proto),
    ));
    {
        let mut st = sw_sys.borrow_mut();
        st.protos.sw_proto = Some(sw_proto);
        st.protos.sw_registration_proto = Some(reg_proto);
        st.protos.sw_container_proto = Some(container_proto);
    }

    // ServiceWorker getters: `scriptURL`, `state`
    {
        let sw = Rc::clone(&sw_sys);
        define_host_getter(vm, sw_proto, "scriptURL", move |vm, this_val, _| {
            let u = this_val
                .as_object()
                .and_then(|h| sw.borrow().workers.get(&h).map(|w| w.script_url.clone()))
                .unwrap_or_default();
            Ok(JsValue::from_string(vm.heap.alloc_string(&u)))
        });
    }
    {
        let sw = Rc::clone(&sw_sys);
        define_host_getter(vm, sw_proto, "state", move |vm, this_val, _| {
            let s = this_val
                .as_object()
                .and_then(|h| sw.borrow().workers.get(&h).map(|w| w.state.clone()))
                .unwrap_or_else(|| "redundant".to_string());
            Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
        });
    }

    // ServiceWorkerRegistration getters & `unregister()`
    {
        let sw = Rc::clone(&sw_sys);
        define_host_getter(vm, reg_proto, "scope", move |vm, this_val, _| {
            let s = this_val
                .as_object()
                .and_then(|h| sw.borrow().registrations.get(&h).map(|r| r.scope.clone()))
                .unwrap_or_default();
            Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
        });
    }
    {
        let sw = Rc::clone(&sw_sys);
        define_host_getter(vm, reg_proto, "active", move |_, this_val, _| {
            let v = this_val
                .as_object()
                .and_then(|h| sw.borrow().registrations.get(&h).and_then(|r| r.active_sw))
                .map_or(JsValue::NULL, JsValue::from_object);
            Ok(v)
        });
    }
    define_host_getter(vm, reg_proto, "installing", |_, _, _| Ok(JsValue::NULL));
    define_host_getter(vm, reg_proto, "waiting", |_, _, _| Ok(JsValue::NULL));
    {
        let sw = Rc::clone(&sw_sys);
        define_host_method(vm, reg_proto, "unregister", move |vm, this_val, _| {
            let p = alloc_promise(vm);
            let removed = if let Some(h) = this_val.as_object() {
                let mut st = sw.borrow_mut();
                let key_opt = st.registrations.get_mut(&h).and_then(|rec| {
                    if rec.unregistered {
                        return None;
                    }
                    rec.unregistered = true;
                    let aw = rec.active_sw.take();
                    Some((rec.origin.clone(), rec.scope.clone(), aw))
                });
                if let Some((origin, scope, aw)) = key_opt {
                    if let Some(sw_h) = aw {
                        if let Some(w) = st.workers.get_mut(&sw_h) {
                            w.state = "redundant".to_string();
                        }
                        if st.controller == Some(sw_h) {
                            st.controller = None;
                        }
                    }
                    if let Ok(mut reg) = sw_registry().lock() {
                        if let Some(active) = reg.remove(&(origin.clone(), scope.clone())) {
                            let _ = active.cmd_tx.send(SwCommand::Terminate);
                        }
                    }
                    if let Some(ctrl_h) = st.controller {
                        if find_matching_sw(&origin, &scope).is_none() {
                            if let Some(w) = st.workers.get_mut(&ctrl_h) {
                                w.state = "redundant".to_string();
                            }
                            st.controller = None;
                            st.ready_resolved = false;
                            st.ready_promise = None;
                        }
                    }
                    true
                } else {
                    false
                }
            } else {
                false
            };
            resolve_promise(vm, p, JsValue::from_bool(removed));
            Ok(JsValue::from_object(p))
        });
    }

    // ServiceWorkerContainer.prototype.register(scriptURL, options?)
    {
        let wa = Rc::clone(web_apis);
        let nb = Rc::clone(net);
        let sw = Rc::clone(&sw_sys);
        define_host_method(vm, container_proto, "register", move |vm, _, args| {
            let p = alloc_promise(vm);
            let raw_script = vm.value_to_rust_string(arg(args, 0));
            let raw_scope = arg(args, 1).as_object().and_then(|opts| {
                let k = vm.heap.intern_key("scope");
                vm.get_property_by_key(opts, k)
                    .ok()
                    .filter(|v| !v.is_undefined() && !v.is_null())
                    .map(|v| vm.value_to_rust_string(v))
            });

            let (base_href, doc_origin, storage, blob_code_opt) = {
                let state = wa.borrow();
                let base_href = state.current_href();
                let doc_origin = state.current_origin();
                let storage = state.storage.clone();
                let blob_code = state
                    .object_urls
                    .get(&raw_script)
                    .map(|b| String::from_utf8_lossy(b).into_owned());
                (base_href, doc_origin, storage, blob_code)
            };

            let Ok(base_url) = Url::parse(&base_href) else {
                let err = create_dom_exception(vm, "SecurityError", "Invalid document URL");
                reject_promise(vm, p, err.value);
                return Ok(JsValue::from_object(p));
            };
            let Ok(resolved_script) =
                Url::parse(&raw_script).or_else(|_| base_url.join(&raw_script))
            else {
                let err = create_dom_exception(vm, "TypeError", "Invalid ServiceWorker scriptURL");
                reject_promise(vm, p, err.value);
                return Ok(JsValue::from_object(p));
            };

            let inline_code = if resolved_script.scheme() == "blob" {
                blob_code_opt
            } else if resolved_script.scheme() == "data" {
                resolved_script
                    .as_str()
                    .split_once(',')
                    .map(|(_, body)| body.to_string())
            } else {
                if origin_of_url(&resolved_script) != doc_origin {
                    let err = create_dom_exception(
                        vm,
                        "SecurityError",
                        "ServiceWorker scriptURL must be same-origin",
                    );
                    reject_promise(vm, p, err.value);
                    return Ok(JsValue::from_object(p));
                }
                None
            };

            let scope_url = if let Some(sc) = raw_scope {
                match base_url.join(&sc) {
                    Ok(u) if origin_of_url(&u) == doc_origin => u.to_string(),
                    _ => {
                        let err = create_dom_exception(
                            vm,
                            "SecurityError",
                            "ServiceWorker scope must be same-origin",
                        );
                        reject_promise(vm, p, err.value);
                        return Ok(JsValue::from_object(p));
                    }
                }
            } else {
                default_sw_scope(&base_url, &resolved_script)
            };

            let sw_id = next_sw_id();
            let pending_id = next_sw_id();
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let fetch_engine = Arc::clone(&nb.borrow().fetch_engine);
            let script_url_str = resolved_script.to_string();

            let script_t = script_url_str.clone();
            let origin_t = doc_origin.clone();
            let scope_t = scope_url.clone();
            let _ = std::thread::Builder::new()
                .name(format!("rix-sw-{sw_id}"))
                .spawn(move || {
                    run_service_worker_thread(
                        script_t,
                        origin_t,
                        scope_t,
                        inline_code,
                        storage,
                        fetch_engine,
                        cmd_rx,
                    );
                });

            let (reply_tx, reply_rx) = oneshot::channel();
            let _ = cmd_tx.send(SwCommand::InstallAndActivate { reply_tx });

            let async_tx = sw.borrow().async_tx.clone();
            sw.borrow_mut()
                .pending_regs
                .insert(pending_id, PendingRegistration { promise: p, cmd_tx });

            let info = RegisteredSwInfo {
                origin: doc_origin,
                scope: scope_url,
                script_url: script_url_str,
            };
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let res = match reply_rx.await {
                        Ok(Ok(())) => Ok(info),
                        Ok(Err(msg)) => Err(msg),
                        Err(_) => Err("ServiceWorker terminated during installation".to_string()),
                    };
                    let _ = async_tx.send(AsyncSwEvent::Registered {
                        pending_id,
                        result: res,
                    });
                });
            } else {
                let _ = std::thread::spawn(move || {
                    let res = match reply_rx.blocking_recv() {
                        Ok(Ok(())) => Ok(info),
                        Ok(Err(msg)) => Err(msg),
                        Err(_) => Err("ServiceWorker terminated during installation".to_string()),
                    };
                    let _ = async_tx.send(AsyncSwEvent::Registered {
                        pending_id,
                        result: res,
                    });
                });
            }

            Ok(JsValue::from_object(p))
        });
    }

    // ServiceWorkerContainer.prototype.getRegistration(clientURL?)
    {
        let wa = Rc::clone(web_apis);
        let sw = Rc::clone(&sw_sys);
        define_host_method(
            vm,
            container_proto,
            "getRegistration",
            move |vm, _, args| {
                let p = alloc_promise(vm);
                let (base_href, origin) = {
                    let s = wa.borrow();
                    (s.current_href(), s.current_origin())
                };
                let target_url = if arg(args, 0).is_undefined() {
                    base_href
                } else {
                    let raw = vm.value_to_rust_string(arg(args, 0));
                    Url::parse(&base_href)
                        .and_then(|b| b.join(&raw))
                        .map_or(raw, |u| u.to_string())
                };
                if let Some(active) = find_matching_sw(&origin, &target_url) {
                    let (reg_h, _) = alloc_registration_objects(
                        vm,
                        &sw,
                        active.origin,
                        active.scope,
                        active.script_url,
                    );
                    resolve_promise(vm, p, JsValue::from_object(reg_h));
                } else {
                    resolve_promise(vm, p, JsValue::UNDEFINED);
                }
                Ok(JsValue::from_object(p))
            },
        );
    }

    // ServiceWorkerContainer.prototype.getRegistrations()
    {
        let wa = Rc::clone(web_apis);
        let sw = Rc::clone(&sw_sys);
        define_host_method(vm, container_proto, "getRegistrations", move |vm, _, _| {
            let p = alloc_promise(vm);
            let origin = wa.borrow().current_origin();
            let list: Vec<ActiveServiceWorker> = sw_registry()
                .lock()
                .map(|r| r.values().filter(|a| a.origin == origin).cloned().collect())
                .unwrap_or_default();
            let handles: Vec<JsValue> = list
                .into_iter()
                .map(|a| {
                    let (rh, _) =
                        alloc_registration_objects(vm, &sw, a.origin, a.scope, a.script_url);
                    JsValue::from_object(rh)
                })
                .collect();
            let arr = alloc_js_array(vm, &handles);
            resolve_promise(vm, p, JsValue::from_object(arr));
            Ok(JsValue::from_object(p))
        });
    }

    // ServiceWorkerContainer.prototype.controller
    {
        let wa = Rc::clone(web_apis);
        let sw = Rc::clone(&sw_sys);
        define_host_getter(vm, container_proto, "controller", move |vm, _, _| {
            if let Some(c) = sw.borrow().controller {
                return Ok(JsValue::from_object(c));
            }
            let (href, origin) = {
                let s = wa.borrow();
                (s.current_href(), s.current_origin())
            };
            if let Some(active) = find_matching_sw(&origin, &href) {
                let (_, sw_h) = alloc_registration_objects(
                    vm,
                    &sw,
                    active.origin,
                    active.scope,
                    active.script_url,
                );
                sw.borrow_mut().controller = Some(sw_h);
                return Ok(JsValue::from_object(sw_h));
            }
            Ok(JsValue::NULL)
        });
    }

    // ServiceWorkerContainer.prototype.ready
    {
        let wa = Rc::clone(web_apis);
        let sw = Rc::clone(&sw_sys);
        define_host_getter(vm, container_proto, "ready", move |vm, _, _| {
            let existing = sw.borrow().ready_promise;
            let p = if let Some(ep) = existing {
                ep
            } else {
                let np = alloc_promise(vm);
                sw.borrow_mut().ready_promise = Some(np);
                np
            };
            if !sw.borrow().ready_resolved {
                let (href, origin) = {
                    let s = wa.borrow();
                    (s.current_href(), s.current_origin())
                };
                if let Some(active) = find_matching_sw(&origin, &href) {
                    let (reg_h, sw_h) = alloc_registration_objects(
                        vm,
                        &sw,
                        active.origin,
                        active.scope,
                        active.script_url,
                    );
                    {
                        let mut st = sw.borrow_mut();
                        st.controller = Some(sw_h);
                        st.ready_resolved = true;
                    }
                    resolve_promise(vm, p, JsValue::from_object(reg_h));
                }
            }
            Ok(JsValue::from_object(p))
        });
    }

    // Ensure `window.navigator` exists and attach `navigator.serviceWorker`
    let container_obj = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(container_proto),
    ));
    let nav_k = vm.heap.intern_key("navigator");
    let nav_h = vm
        .get_property_by_key(vm.global, nav_k)
        .ok()
        .and_then(JsValue::as_object)
        .unwrap_or_else(|| {
            let nh = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Ordinary,
                Some(vm.object_proto),
            ));
            let ua_k = vm.heap.intern_key("userAgent");
            let ua_v = JsValue::from_string(vm.heap.alloc_string("RixBrowser/0.2"));
            vm.heap
                .object_define_data(nh, ua_k, ua_v, PropertyAttrs::READONLY, &mut vm.shapes);
            let on_k = vm.heap.intern_key("onLine");
            vm.heap.object_define_data(
                nh,
                on_k,
                JsValue::from_bool(true),
                PropertyAttrs::READONLY,
                &mut vm.shapes,
            );
            define_global(vm, "navigator", JsValue::from_object(nh));
            nh
        });
    let sw_k = vm.heap.intern_key("serviceWorker");
    vm.heap.object_define_data(
        nav_h,
        sw_k,
        JsValue::from_object(container_obj),
        PropertyAttrs::READONLY,
        &mut vm.shapes,
    );

    let sw_ctor = vm.alloc_host_fn("ServiceWorker", |vm, _, _| {
        Err(create_dom_exception(vm, "TypeError", "Illegal constructor"))
    });
    bind_ctor_proto(vm, sw_ctor, sw_proto);
    define_global(vm, "ServiceWorker", JsValue::from_object(sw_ctor));

    let reg_ctor = vm.alloc_host_fn("ServiceWorkerRegistration", |vm, _, _| {
        Err(create_dom_exception(vm, "TypeError", "Illegal constructor"))
    });
    bind_ctor_proto(vm, reg_ctor, reg_proto);
    define_global(
        vm,
        "ServiceWorkerRegistration",
        JsValue::from_object(reg_ctor),
    );

    sw_sys
}

fn default_sw_scope(base_url: &Url, script_url: &Url) -> String {
    if script_url.scheme() == "http" || script_url.scheme() == "https" {
        let mut s = script_url.clone();
        s.set_query(None);
        s.set_fragment(None);
        let path = s.path().to_string();
        let dir = match path.rfind('/') {
            Some(idx) => &path[..=idx],
            None => "/",
        };
        s.set_path(dir);
        s.to_string()
    } else {
        let mut s = base_url.clone();
        s.set_query(None);
        s.set_fragment(None);
        s.set_path("/");
        s.to_string()
    }
}
