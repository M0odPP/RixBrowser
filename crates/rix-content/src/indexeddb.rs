//! Full async transactional W3C IndexedDB (`IDBFactory`, `IDBOpenDBRequest`,
//! `IDBRequest`, `IDBDatabase`, `IDBTransaction`, `IDBObjectStore`, `IDBIndex`,
//! `IDBCursor`, `IDBKeyRange`) persisted atomically per origin in `rix-storage` (`redb`).
//!
//! Reference: Spec 06 §3, Issue #24.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use rix_js::promise::{alloc_promise, reject_promise, resolve_promise};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_storage::{IdbDatabaseSnapshot, IdbIndexSchema, IdbStoreSnapshot};

use crate::canvas2d::SharedCanvas2D;
use crate::dom_bridge::SharedHost;
use crate::dom_interfaces::define_to_string_tag;
use crate::event_loop::{SharedEventLoop, TaskPriority};
use crate::events::{
    fire_idb_event, install_object_event_handler_accessor, register_constructed_event, SharedEvents,
};
use crate::fetch_ws::SharedNetBridge;
use crate::frames::{decode_structured_json, SharedFrames};
use crate::indexeddb_engine::{
    alloc_dom_string_list, alloc_js_array, collect_index_entries, collect_store_entries,
    enforce_range_idb_version, enforce_range_u32, idb_key_to_js, js_to_idb_key, parse_key_path_arg,
    store_put_or_add_payload, validate_put_or_add_sync, CursorDirection, IdbCursorEntry, KeyRange,
};
use crate::structured_clone::{
    decode_storage_payload, deserialize_for_storage, serialize_for_storage, CloneContext,
    StructuredClonePayload,
};
use crate::web_apis::{
    arg, bind_ctor_proto, create_dom_exception, define_global, define_host_getter,
    define_host_method, SharedWebApis,
};
use crate::workers::SharedWorkers;

fn idb_frames(idb: &SharedIndexedDb) -> Option<SharedFrames> {
    idb.borrow()
        .clone_deps
        .as_ref()
        .map(|d| Rc::clone(&d.frames))
}

fn idb_effective_origin(web_apis: &SharedWebApis, idb: &SharedIndexedDb) -> String {
    if let Some(frames) = idb_frames(idb) {
        return frames.borrow().effective_origin(web_apis);
    }
    web_apis.borrow().current_origin()
}

fn idb_serialize_value(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    web_apis: &SharedWebApis,
    idb: &SharedIndexedDb,
    val: JsValue,
) -> Result<StructuredClonePayload, JsException> {
    let deps = idb
        .borrow()
        .clone_deps
        .clone()
        .ok_or_else(|| create_dom_exception(vm, "UnknownError", "Clone context unavailable"))?;
    let ctx = CloneContext {
        host,
        events,
        web_apis,
        net: &deps.net,
        canvas2d: &deps.canvas2d,
        frames: &deps.frames,
        workers: &deps.workers,
    };
    serialize_for_storage(vm, &ctx, val)
}

#[allow(clippy::too_many_arguments)]
fn idb_deserialize_entry(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    web_apis: &SharedWebApis,
    deps: Option<&CloneDeps>,
    value_bytes: &[u8],
    value_json: &str,
) -> JsValue {
    if !value_bytes.is_empty() {
        if let Some(payload) = decode_storage_payload(value_bytes) {
            if let Some(deps) = deps {
                let ctx = CloneContext {
                    host,
                    events,
                    web_apis,
                    net: &deps.net,
                    canvas2d: &deps.canvas2d,
                    frames: &deps.frames,
                    workers: &deps.workers,
                };
                return deserialize_for_storage(vm, &ctx, &payload);
            }
        }
    }
    if !value_json.is_empty() {
        if let Ok(jv) = serde_json::from_str::<serde_json::Value>(value_json) {
            return decode_structured_json(vm, &jv);
        }
    }
    JsValue::UNDEFINED
}

fn alloc_idb_record(
    vm: &mut JsVm,
    record_proto: HeapHandle,
    key: JsValue,
    primary_key: JsValue,
    value: JsValue,
) -> JsValue {
    let h = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(record_proto),
    ));
    let k_k = vm.heap.intern_key("__rix_rec_key__");
    let k_pk = vm.heap.intern_key("__rix_rec_pk__");
    let k_v = vm.heap.intern_key("__rix_rec_val__");
    vm.heap
        .object_define_data(h, k_k, key, PropertyAttrs::empty(), &mut vm.shapes);
    vm.heap
        .object_define_data(h, k_pk, primary_key, PropertyAttrs::empty(), &mut vm.shapes);
    vm.heap
        .object_define_data(h, k_v, value, PropertyAttrs::empty(), &mut vm.shapes);
    JsValue::from_object(h)
}

#[allow(clippy::too_many_arguments)]
fn materialize_idb_query_result(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    web_apis: &SharedWebApis,
    deps: Option<&CloneDeps>,
    record_proto: HeapHandle,
    entries: &[IdbCursorEntry],
    mode_tag: u8,
    count_limit: Option<usize>,
) -> JsValue {
    match mode_tag {
        0 => entries.first().map_or(JsValue::UNDEFINED, |e| {
            idb_deserialize_entry(
                vm,
                host,
                events,
                web_apis,
                deps,
                &e.value_bytes,
                &e.value_json,
            )
        }),
        1 => entries
            .first()
            .map_or(JsValue::UNDEFINED, |e| idb_key_to_js(vm, &e.primary_key)),
        2 => {
            let limit = count_limit.unwrap_or(entries.len());
            let vals: Vec<JsValue> = entries
                .iter()
                .take(limit)
                .map(|e| {
                    idb_deserialize_entry(
                        vm,
                        host,
                        events,
                        web_apis,
                        deps,
                        &e.value_bytes,
                        &e.value_json,
                    )
                })
                .collect();
            JsValue::from_object(alloc_js_array(vm, &vals))
        }
        3 => {
            let limit = count_limit.unwrap_or(entries.len());
            let vals: Vec<JsValue> = entries
                .iter()
                .take(limit)
                .map(|e| idb_key_to_js(vm, &e.primary_key))
                .collect();
            JsValue::from_object(alloc_js_array(vm, &vals))
        }
        5 => {
            let limit = count_limit.unwrap_or(entries.len());
            let vals: Vec<JsValue> = entries
                .iter()
                .take(limit)
                .map(|e| {
                    let k = idb_key_to_js(vm, &e.key);
                    let pk = idb_key_to_js(vm, &e.primary_key);
                    let v = idb_deserialize_entry(
                        vm,
                        host,
                        events,
                        web_apis,
                        deps,
                        &e.value_bytes,
                        &e.value_json,
                    );
                    alloc_idb_record(vm, record_proto, k, pk, v)
                })
                .collect();
            JsValue::from_object(alloc_js_array(vm, &vals))
        }
        _ => JsValue::from_number(entries.len() as f64),
    }
}

fn key_path_to_js(vm: &mut JsVm, kp: Option<(Vec<String>, bool)>) -> JsValue {
    match kp {
        Some((ref v, false)) if v.len() == 1 => JsValue::from_string(vm.heap.alloc_string(&v[0])),
        Some((v, _)) => {
            let elems: Vec<JsValue> = v
                .iter()
                .map(|s| JsValue::from_string(vm.heap.alloc_string(s)))
                .collect();
            JsValue::from_object(alloc_js_array(vm, &elems))
        }
        None => JsValue::NULL,
    }
}

fn parse_cursor_direction(vm: &mut JsVm, val: JsValue) -> Result<CursorDirection, JsException> {
    if val.is_undefined() {
        return Ok(CursorDirection::Next);
    }
    let s = vm.to_js_string(val)?;
    CursorDirection::parse(&s)
        .ok_or_else(|| vm.type_error(format!("Invalid cursor direction '{s}'")))
}

fn query_to_key_range(
    vm: &mut JsVm,
    idb: &SharedIndexedDb,
    val: JsValue,
) -> Result<Option<KeyRange>, JsException> {
    if val.is_undefined() || val.is_null() {
        return Ok(None);
    }
    if let Some(h) = val.as_object() {
        if let Some(kr) = idb.borrow().key_ranges.get(&h) {
            return Ok(Some(kr.clone()));
        }
    }
    let frames = idb_frames(idb);
    let mut seen = HashSet::new();
    let key = js_to_idb_key(vm, frames.as_ref(), val, &mut seen)?;
    Ok(Some(KeyRange::only(key)))
}

fn require_query_key_range(
    vm: &mut JsVm,
    idb: &SharedIndexedDb,
    val: JsValue,
) -> Result<KeyRange, JsException> {
    if let Some(kr) = query_to_key_range(vm, idb, val)? {
        Ok(kr)
    } else {
        Err(create_dom_exception(
            vm,
            "DataError",
            "A valid key or IDBKeyRange is required",
        ))
    }
}

/// Parses `IDBObjectStore`/`IDBIndex` `getAll`/`getAllKeys`/`getAllRecords` arguments:
/// either `(IDBGetAllOptions)` dictionary or `(query?, count?)` positional arguments.
fn parse_get_all_args(
    vm: &mut JsVm,
    idb: &SharedIndexedDb,
    args: &[JsValue],
    records_mode: bool,
) -> Result<(Option<KeyRange>, Option<usize>, CursorDirection), JsException> {
    let first = arg(args, 0);
    if records_mode {
        if first.is_undefined() || first.is_null() {
            return Ok((None, None, CursorDirection::Next));
        }
        let Some(oh) = first.as_object() else {
            return Err(vm.type_error("IDBGetAllOptions must be an object"));
        };
        return parse_get_all_options_dict(vm, idb, oh);
    }
    // Per W3C IDB 3.0 WebIDL overload resolution: when arg 0 is a plain object that is
    // NOT an `IDBKeyRange`, `Date`, `Array`, `ArrayBuffer`, `TypedArray`, or `DataView`,
    // interpret it as `IDBGetAllOptions` (ignoring any trailing positional `count`).
    if let Some(oh) = first.as_object() {
        let is_kr = idb.borrow().key_ranges.contains_key(&oh);
        let is_ordinary = vm
            .heap
            .get_object(oh)
            .is_some_and(|o| matches!(o.kind, ObjectKind::Ordinary));
        if !is_kr && is_ordinary {
            return parse_get_all_options_dict(vm, idb, oh);
        }
    }
    let range = query_to_key_range(vm, idb, first)?;
    let count = if arg(args, 1).is_undefined() {
        None
    } else {
        let c = enforce_range_u32(vm, arg(args, 1))?;
        if c == 0 {
            None
        } else {
            Some(c as usize)
        }
    };
    Ok((range, count, CursorDirection::Next))
}

fn parse_get_all_options_dict(
    vm: &mut JsVm,
    idb: &SharedIndexedDb,
    oh: HeapHandle,
) -> Result<(Option<KeyRange>, Option<usize>, CursorDirection), JsException> {
    let k_count = vm.heap.intern_key("count");
    let k_dir = vm.heap.intern_key("direction");
    let k_query = vm.heap.intern_key("query");

    // WebIDL dictionary member conversion order is lexicographical: `count`, `direction`, `query`.
    let count_val = vm.get_property_by_key(oh, k_count)?;
    let count = if count_val.is_undefined() {
        None
    } else {
        let c = enforce_range_u32(vm, count_val)?;
        if c == 0 {
            None
        } else {
            Some(c as usize)
        }
    };

    let dir_val = vm.get_property_by_key(oh, k_dir)?;
    let direction = if dir_val.is_undefined() {
        CursorDirection::Next
    } else {
        let s = vm.to_js_string(dir_val)?;
        CursorDirection::parse(&s)
            .ok_or_else(|| vm.type_error(format!("Invalid cursor direction '{s}'")))?
    };

    let query_val = vm.get_property_by_key(oh, k_query)?;
    let range = query_to_key_range(vm, idb, query_val)?;
    Ok((range, count, direction))
}

// ---------------------------------------------------------------------------
// Internal IndexedDB runtime tables & ACID transaction state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxMode {
    ReadOnly,
    ReadWrite,
    VersionChange,
}

impl TxMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "readonly",
            Self::ReadWrite => "readwrite",
            Self::VersionChange => "versionchange",
        }
    }
}

struct IdbRequestState {
    result: JsValue,
    error: JsValue,
    ready_state: &'static str,
    source: JsValue,
    transaction: JsValue,
}

struct IdbDatabaseState {
    origin: String,
    name: String,
    version: u64,
    store_names: Vec<String>,
    closed: bool,
    active_versionchange_tx: Option<HeapHandle>,
}

struct IdbTransactionState {
    db_h: HeapHandle,
    origin: String,
    mode: TxMode,
    durability: &'static str,
    scope: Vec<String>,
    working: IdbDatabaseSnapshot,
    old_version: u64,
    finished: bool,
    aborted: bool,
    pending_requests: usize,
    commit_scheduled: bool,
    error: JsValue,
    versionchange_open_req: Option<HeapHandle>,
}

struct IdbStoreHandle {
    tx_h: HeapHandle,
    name: String,
    cached_key_path: Option<JsValue>,
}

struct IdbIndexHandle {
    tx_h: HeapHandle,
    store_h: HeapHandle,
    store_name: String,
    index_name: String,
    cached_key_path: Option<JsValue>,
}

struct IdbCursorState {
    tx_h: HeapHandle,
    req_h: HeapHandle,
    source_h: HeapHandle,
    store_name: String,
    index_name: Option<String>,
    range: Option<KeyRange>,
    direction: CursorDirection,
    key_only: bool,
    initialized: bool,
    entries: Vec<IdbCursorEntry>,
    index: usize,
    got_value: bool,
}

#[derive(Default)]
struct IdbPrototypes {
    factory_proto: Option<HeapHandle>,
    request_proto: Option<HeapHandle>,
    open_request_proto: Option<HeapHandle>,
    database_proto: Option<HeapHandle>,
    transaction_proto: Option<HeapHandle>,
    object_store_proto: Option<HeapHandle>,
    index_proto: Option<HeapHandle>,
    cursor_proto: Option<HeapHandle>,
    cursor_with_value_proto: Option<HeapHandle>,
    key_range_proto: Option<HeapHandle>,
    version_change_event_proto: Option<HeapHandle>,
    dom_string_list_proto: Option<HeapHandle>,
    record_proto: Option<HeapHandle>,
}

#[derive(Clone)]
struct CloneDeps {
    net: SharedNetBridge,
    canvas2d: SharedCanvas2D,
    frames: SharedFrames,
    workers: SharedWorkers,
}

/// Content-runtime registry of active IndexedDB handles, transactions, and prototypes.
#[derive(Default)]
pub struct IndexedDbSystem {
    requests: HashMap<HeapHandle, IdbRequestState>,
    databases: HashMap<HeapHandle, IdbDatabaseState>,
    transactions: HashMap<HeapHandle, IdbTransactionState>,
    stores: HashMap<HeapHandle, IdbStoreHandle>,
    indexes: HashMap<HeapHandle, IdbIndexHandle>,
    cursors: HashMap<HeapHandle, IdbCursorState>,
    key_ranges: HashMap<HeapHandle, KeyRange>,
    protos: IdbPrototypes,
    clone_deps: Option<CloneDeps>,
}

/// Shared handle to [`IndexedDbSystem`].
pub type SharedIndexedDb = Rc<RefCell<IndexedDbSystem>>;

impl IndexedDbSystem {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Collects GC roots for active transactions, requests, databases, cursors, and prototypes.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.protos.factory_proto,
            self.protos.request_proto,
            self.protos.open_request_proto,
            self.protos.database_proto,
            self.protos.transaction_proto,
            self.protos.object_store_proto,
            self.protos.index_proto,
            self.protos.cursor_proto,
            self.protos.cursor_with_value_proto,
            self.protos.key_range_proto,
            self.protos.version_change_event_proto,
            self.protos.dom_string_list_proto,
            self.protos.record_proto,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(p));
        }
        for (&th, tx) in &self.transactions {
            if !tx.finished {
                out.push(JsValue::from_object(th));
                out.push(JsValue::from_object(tx.db_h));
                out.push(tx.error);
                if let Some(req) = tx.versionchange_open_req {
                    out.push(JsValue::from_object(req));
                }
            }
        }
        for (&rh, req) in &self.requests {
            if req.ready_state == "pending" {
                out.push(JsValue::from_object(rh));
            }
            out.push(req.result);
            out.push(req.error);
            out.push(req.source);
            out.push(req.transaction);
        }
        for s in self.stores.values() {
            if let Some(kp) = s.cached_key_path {
                out.push(kp);
            }
        }
        for i in self.indexes.values() {
            if let Some(kp) = i.cached_key_path {
                out.push(kp);
            }
        }
        for (&ch, c) in &self.cursors {
            let active = self.transactions.get(&c.tx_h).is_some_and(|t| !t.finished)
                || self
                    .requests
                    .get(&c.req_h)
                    .is_some_and(|r| r.ready_state == "pending");
            if active {
                out.push(JsValue::from_object(ch));
                out.push(JsValue::from_object(c.req_h));
                out.push(JsValue::from_object(c.source_h));
                out.push(JsValue::from_object(c.tx_h));
            }
        }
    }

    /// Sweeps dead JS wrapper handles after Major GC.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.requests
            .retain(|h, r| r.ready_state == "pending" || heap.is_alive(*h));
        self.databases.retain(|h, _| heap.is_alive(*h));
        self.transactions
            .retain(|h, tx| !tx.finished || heap.is_alive(*h));
        self.stores.retain(|h, _| heap.is_alive(*h));
        self.indexes.retain(|h, _| heap.is_alive(*h));
        self.cursors.retain(|h, _| heap.is_alive(*h));
        self.key_ranges.retain(|h, _| heap.is_alive(*h));
    }
}

// ---------------------------------------------------------------------------
// Request & Transaction async scheduling (Networking vs Idle priority!)
// ---------------------------------------------------------------------------

fn alloc_request(
    vm: &mut JsVm,
    events: &SharedEvents,
    idb: &SharedIndexedDb,
    is_open: bool,
    source: JsValue,
    transaction: JsValue,
) -> HeapHandle {
    let proto = {
        let st = idb.borrow();
        if is_open {
            st.protos.open_request_proto
        } else {
            st.protos.request_proto
        }
        .unwrap_or(vm.object_proto)
    };
    let h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    if let Some(tx_h) = transaction.as_object() {
        events.borrow_mut().set_object_event_parent(h, Some(tx_h));
    }
    idb.borrow_mut().requests.insert(
        h,
        IdbRequestState {
            result: JsValue::UNDEFINED,
            error: JsValue::NULL,
            ready_state: "pending",
            source,
            transaction,
        },
    );
    h
}

fn abort_tx_with_error(
    vm: &mut JsVm,
    idb: &SharedIndexedDb,
    tx_h: HeapHandle,
    err_name: &str,
    err_msg: &str,
) {
    let dom_err = create_dom_exception(vm, err_name, err_msg).value;
    if let Some(tx) = idb.borrow_mut().transactions.get_mut(&tx_h) {
        if !tx.finished {
            tx.aborted = true;
            tx.error = dom_err;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn schedule_tx_commit_check(
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    idb: &SharedIndexedDb,
    tx_h: HeapHandle,
) {
    let should_schedule = {
        let mut st = idb.borrow_mut();
        match st.transactions.get_mut(&tx_h) {
            Some(tx) if !tx.finished && !tx.commit_scheduled => {
                tx.commit_scheduled = true;
                true
            }
            _ => false,
        }
    };
    if !should_schedule {
        return;
    }
    let h = Rc::clone(host);
    let ev = Rc::clone(events);
    let el = Rc::clone(event_loop);
    let wa = Rc::clone(web_apis);
    let db_sys = Rc::clone(idb);
    event_loop
        .borrow_mut()
        .post_host_task(TaskPriority::Idle, move |vm| {
            let outcome = {
                let mut st = db_sys.borrow_mut();
                let Some(tx) = st.transactions.get_mut(&tx_h) else {
                    return;
                };
                tx.commit_scheduled = false;
                if tx.finished {
                    return;
                }
                if tx.pending_requests > 0 {
                    // More requests still in flight at `Networking` priority; re-check later.
                    None
                } else {
                    tx.finished = true;
                    let db_h = tx.db_h;
                    let aborted = tx.aborted;
                    let mode = tx.mode;
                    let origin = tx.origin.clone();
                    let old_version = tx.old_version;
                    let working = tx.working.clone();
                    let open_req = tx.versionchange_open_req.take();
                    let err_val = tx.error;
                    if let Some(db) = st.databases.get_mut(&db_h) {
                        if db.active_versionchange_tx == Some(tx_h) {
                            db.active_versionchange_tx = None;
                        }
                        if mode == TxMode::VersionChange {
                            if aborted {
                                db.version = old_version;
                            } else {
                                db.version = working.version;
                                db.store_names = working.stores.keys().cloned().collect();
                            }
                        }
                    }
                    Some((db_h, aborted, mode, origin, working, open_req, err_val))
                }
            };

            let Some((db_h, aborted, mode, origin, working, open_req, err_val)) = outcome else {
                schedule_tx_commit_check(&h, &ev, &el, &wa, &db_sys, tx_h);
                return;
            };

            let ev_proto = ev.borrow().event_proto.unwrap_or(vm.object_proto);

            if aborted {
                // Transaction rolled back: ZERO writes to redb!
                // W3C IDB §5.5: `abort` event bubbles (`transaction -> database`), not cancelable.
                let _ = fire_idb_event(vm, &h, &ev, &el, tx_h, "abort", true, false, ev_proto, &[]);
                if let Some(req_h) = open_req {
                    let abort_err = if err_val.is_null() || err_val.is_undefined() {
                        create_dom_exception(vm, "AbortError", "Version change transaction aborted")
                            .value
                    } else {
                        err_val
                    };
                    if let Some(r) = db_sys.borrow_mut().requests.get_mut(&req_h) {
                        r.ready_state = "done";
                        r.result = JsValue::UNDEFINED;
                        r.error = abort_err;
                        r.transaction = JsValue::NULL;
                    }
                    ev.borrow_mut().set_object_event_parent(req_h, None);
                    let _ =
                        fire_idb_event(vm, &h, &ev, &el, req_h, "error", true, true, ev_proto, &[]);
                }
                return;
            }

            if mode != TxMode::ReadOnly {
                let storage = wa.borrow().storage.clone();
                if let Err(e) = storage.idb_commit_database(&origin, &working) {
                    let dom_err = create_dom_exception(vm, "AbortError", &e.to_string()).value;
                    if let Some(tx) = db_sys.borrow_mut().transactions.get_mut(&tx_h) {
                        tx.aborted = true;
                        tx.error = dom_err;
                    }
                    let _ =
                        fire_idb_event(vm, &h, &ev, &el, tx_h, "abort", true, false, ev_proto, &[]);
                    return;
                }
            }

            let _ = fire_idb_event(
                vm,
                &h,
                &ev,
                &el,
                tx_h,
                "complete",
                false,
                false,
                ev_proto,
                &[],
            );

            if let Some(req_h) = open_req {
                if let Some(r) = db_sys.borrow_mut().requests.get_mut(&req_h) {
                    r.ready_state = "done";
                    r.result = JsValue::from_object(db_h);
                    r.error = JsValue::NULL;
                    r.transaction = JsValue::NULL;
                }
                ev.borrow_mut().set_object_event_parent(req_h, None);
                let _ = fire_idb_event(
                    vm,
                    &h,
                    &ev,
                    &el,
                    req_h,
                    "success",
                    false,
                    false,
                    ev_proto,
                    &[],
                );
            }
        });
}

#[allow(clippy::too_many_arguments)]
fn enqueue_tx_request<F>(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    idb: &SharedIndexedDb,
    tx_h: HeapHandle,
    source_h: HeapHandle,
    op: F,
) -> Result<HeapHandle, JsException>
where
    F: FnOnce(&mut JsVm, &mut IdbTransactionState) -> Result<JsValue, (&'static str, String)>
        + 'static,
{
    {
        let mut st = idb.borrow_mut();
        let Some(tx) = st.transactions.get_mut(&tx_h) else {
            return Err(create_dom_exception(
                vm,
                "TransactionInactiveError",
                "IndexedDB transaction does not exist",
            ));
        };
        if tx.finished || tx.aborted {
            return Err(create_dom_exception(
                vm,
                "TransactionInactiveError",
                "IndexedDB transaction is not active",
            ));
        }
        tx.pending_requests += 1;
    }

    let req_h = alloc_request(
        vm,
        events,
        idb,
        false,
        JsValue::from_object(source_h),
        JsValue::from_object(tx_h),
    );
    schedule_tx_commit_check(host, events, event_loop, web_apis, idb, tx_h);

    let h = Rc::clone(host);
    let ev = Rc::clone(events);
    let el = Rc::clone(event_loop);
    let wa = Rc::clone(web_apis);
    let db_sys = Rc::clone(idb);
    event_loop
        .borrow_mut()
        .post_host_task(TaskPriority::Networking, move |vm| {
            let mut aborted_early = false;
            let op_res = {
                let mut st = db_sys.borrow_mut();
                let Some(tx) = st.transactions.get_mut(&tx_h) else {
                    return;
                };
                tx.pending_requests = tx.pending_requests.saturating_sub(1);
                if tx.aborted || tx.finished {
                    aborted_early = true;
                    Err(("AbortError", "Transaction was aborted".to_string()))
                } else {
                    op(vm, tx)
                }
            };

            let ev_proto = ev.borrow().event_proto.unwrap_or(vm.object_proto);
            match op_res {
                Ok(result_val) => {
                    if let Some(r) = db_sys.borrow_mut().requests.get_mut(&req_h) {
                        r.ready_state = "done";
                        r.result = result_val;
                        r.error = JsValue::NULL;
                    }
                    let (_, threw) = fire_idb_event(
                        vm,
                        &h,
                        &ev,
                        &el,
                        req_h,
                        "success",
                        false,
                        false,
                        ev_proto,
                        &[],
                    );
                    if threw {
                        abort_tx_with_error(
                            vm,
                            &db_sys,
                            tx_h,
                            "AbortError",
                            "Uncaught exception in success event handler",
                        );
                    }
                }
                Err((err_name, err_msg)) => {
                    let dom_err = create_dom_exception(vm, err_name, &err_msg).value;
                    if let Some(r) = db_sys.borrow_mut().requests.get_mut(&req_h) {
                        r.ready_state = "done";
                        r.result = JsValue::UNDEFINED;
                        r.error = dom_err;
                    }
                    // W3C IDB §5.5: fire bubbling, cancelable `error` event along `req -> tx -> db`.
                    // If ANY listener threw an uncaught exception, abort tx with `AbortError`
                    // (even if `preventDefault()` was called!). Otherwise, if NOT canceled and
                    // not already aborted, abort tx with the request's error.
                    let (prevented, threw) =
                        fire_idb_event(vm, &h, &ev, &el, req_h, "error", true, true, ev_proto, &[]);
                    if !aborted_early {
                        if threw {
                            abort_tx_with_error(
                                vm,
                                &db_sys,
                                tx_h,
                                "AbortError",
                                "Uncaught exception in error event handler",
                            );
                        } else if !prevented {
                            if let Some(tx) = db_sys.borrow_mut().transactions.get_mut(&tx_h) {
                                if !tx.finished {
                                    tx.aborted = true;
                                    tx.error = dom_err;
                                }
                            }
                        }
                    }
                }
            }
            schedule_tx_commit_check(&h, &ev, &el, &wa, &db_sys, tx_h);
        });

    Ok(req_h)
}

// ---------------------------------------------------------------------------
// Installation onto `ContentRuntime`'s `JsVm`
// ---------------------------------------------------------------------------

/// Installs `window.indexedDB` (`IDBFactory`), `IDBOpenDBRequest`, `IDBRequest`,
/// `IDBDatabase`, `IDBTransaction`, `IDBObjectStore`, `IDBIndex`, `IDBCursor`,
/// `IDBCursorWithValue`, `IDBKeyRange`, `IDBVersionChangeEvent`, `IDBRecord`,
/// and `DOMStringList` onto `vm`, backed by `rix-storage` (`redb`).
#[allow(clippy::too_many_arguments)]
pub fn install_indexeddb(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    canvas2d: &SharedCanvas2D,
    frames: &SharedFrames,
    workers: &SharedWorkers,
) -> SharedIndexedDb {
    let idb = Rc::new(RefCell::new(IndexedDbSystem::new()));
    idb.borrow_mut().clone_deps = Some(CloneDeps {
        net: Rc::clone(net),
        canvas2d: Rc::clone(canvas2d),
        frames: Rc::clone(frames),
        workers: Rc::clone(workers),
    });
    let event_target_proto = host.borrow().prototypes.event_target_proto;

    install_dom_string_list(vm, &idb);
    install_idb_version_change_event(vm, events, &idb);
    install_idb_record(vm, &idb);
    install_idb_key_range(vm, &idb);
    install_idb_request_protos(vm, event_target_proto, events, &idb);
    install_idb_cursor_proto(vm, host, events, event_loop, web_apis, &idb);
    install_idb_index_proto(vm, host, events, event_loop, web_apis, &idb);
    install_idb_object_store_proto(vm, host, events, event_loop, web_apis, &idb);
    install_idb_transaction_proto(vm, host, events, event_loop, web_apis, &idb);
    install_idb_database_proto(vm, host, events, event_loop, web_apis, &idb);
    install_idb_factory(vm, host, events, event_loop, web_apis, &idb);

    idb
}

fn install_illegal_ctor(vm: &mut JsVm, name: &'static str, proto: HeapHandle) {
    let ctor = vm.alloc_host_fn(name, |vm, _, _| Err(vm.type_error("Illegal constructor")));
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, name, JsValue::from_object(ctor));
}

fn install_dom_string_list(vm: &mut JsVm, idb: &SharedIndexedDb) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    define_to_string_tag(vm, proto, "DOMStringList");
    idb.borrow_mut().protos.dom_string_list_proto = Some(proto);

    define_host_getter(vm, proto, "length", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let k = vm.heap.intern_key("__rix_dsl_len__");
        let v = vm
            .heap
            .object_get_own(h, k, &vm.shapes)
            .map_or(JsValue::from_number(0.0), |p| p.value);
        Ok(v)
    });
    define_host_method(vm, proto, "item", |vm, this_val, args| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        if args.is_empty() {
            return Err(vm.type_error("DOMStringList.item requires 1 argument"));
        }
        let idx = vm.to_u32(arg(args, 0))?;
        let k = vm.heap.intern_key(&idx.to_string());
        Ok(vm
            .heap
            .object_get_own(h, k, &vm.shapes)
            .map_or(JsValue::NULL, |p| p.value))
    });
    define_host_method(vm, proto, "contains", |vm, this_val, args| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        if args.is_empty() {
            return Err(vm.type_error("DOMStringList.contains requires 1 argument"));
        }
        let needle = vm.to_js_string(arg(args, 0))?;
        let k_len = vm.heap.intern_key("__rix_dsl_len__");
        let len = vm
            .heap
            .object_get_own(h, k_len, &vm.shapes)
            .and_then(|p| p.value.as_number())
            .unwrap_or(0.0) as usize;
        for i in 0..len {
            let ki = vm.heap.intern_key(&i.to_string());
            if let Some(p) = vm.heap.object_get_own(h, ki, &vm.shapes) {
                if vm.value_to_rust_string(p.value) == needle {
                    return Ok(JsValue::TRUE);
                }
            }
        }
        Ok(JsValue::FALSE)
    });
    // Support `for..of` / spread / `Array.from(domStringList)` via `[Symbol.iterator]`
    let iter_fn = vm.alloc_host_fn("values", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let k_len = vm.heap.intern_key("__rix_dsl_len__");
        let len = vm
            .heap
            .object_get_own(h, k_len, &vm.shapes)
            .and_then(|p| p.value.as_number())
            .unwrap_or(0.0) as usize;
        let mut items = Vec::with_capacity(len);
        for i in 0..len {
            let ki = vm.heap.intern_key(&i.to_string());
            if let Some(p) = vm.heap.object_get_own(h, ki, &vm.shapes) {
                items.push(p.value);
            }
        }
        let arr = alloc_js_array(vm, &items);
        let sym_iter = vm.key_symbol_iterator;
        let iter_m = vm.get_property_by_key(arr, sym_iter)?;
        vm.call_value(iter_m, JsValue::from_object(arr), &[])
    });
    let sym_iter = vm.key_symbol_iterator;
    vm.heap.object_define_data(
        proto,
        sym_iter,
        JsValue::from_object(iter_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    install_illegal_ctor(vm, "DOMStringList", proto);
}

fn install_idb_version_change_event(vm: &mut JsVm, events: &SharedEvents, idb: &SharedIndexedDb) {
    let event_proto = events.borrow().event_proto.unwrap_or(vm.object_proto);
    let proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(event_proto)));
    define_to_string_tag(vm, proto, "IDBVersionChangeEvent");
    idb.borrow_mut().protos.version_change_event_proto = Some(proto);

    define_host_getter(vm, proto, "oldVersion", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let k = vm.heap.intern_key("__rix_idb_old_ver__");
        if let Some(p) = vm.heap.object_get_own(h, k, &vm.shapes) {
            return Ok(p.value);
        }
        Err(vm.type_error("Illegal invocation"))
    });
    define_host_getter(vm, proto, "newVersion", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let k = vm.heap.intern_key("__rix_idb_new_ver__");
        if let Some(p) = vm.heap.object_get_own(h, k, &vm.shapes) {
            return Ok(p.value);
        }
        Err(vm.type_error("Illegal invocation"))
    });

    let ev_r = Rc::clone(events);
    let ctor = vm.alloc_host_fn("IDBVersionChangeEvent", move |vm, _, args| {
        if args.is_empty() {
            return Err(vm.type_error("IDBVersionChangeEvent requires at least 1 argument"));
        }
        let ev_type = vm.to_js_string(arg(args, 0))?;
        let mut old_ver = 0.0f64;
        let mut new_ver = JsValue::NULL;
        let mut bubbles = false;
        let mut cancelable = false;
        let mut composed = false;
        if let Some(init) = arg(args, 1).as_object() {
            let k_old = vm.heap.intern_key("oldVersion");
            let v_old = vm.get_property_by_key(init, k_old)?;
            if !v_old.is_undefined() {
                let n = vm.to_number(v_old)?;
                old_ver = if n.is_finite() && n >= 0.0 {
                    n.trunc()
                } else {
                    0.0
                };
            }
            let k_new = vm.heap.intern_key("newVersion");
            let v_new = vm.get_property_by_key(init, k_new)?;
            if !v_new.is_undefined() && !v_new.is_null() {
                let n = vm.to_number(v_new)?;
                new_ver = JsValue::from_number(if n.is_finite() && n >= 0.0 {
                    n.trunc()
                } else {
                    0.0
                });
            }
            let k_b = vm.heap.intern_key("bubbles");
            let k_c = vm.heap.intern_key("cancelable");
            let k_co = vm.heap.intern_key("composed");
            let vb = vm.get_property_by_key(init, k_b)?;
            let vc = vm.get_property_by_key(init, k_c)?;
            let vco = vm.get_property_by_key(init, k_co)?;
            bubbles = vm.to_boolean(vb);
            cancelable = vm.to_boolean(vc);
            composed = vm.to_boolean(vco);
        }
        let ev_h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        register_constructed_event(&ev_r, ev_h, ev_type, bubbles, cancelable, composed);
        let k_ov = vm.heap.intern_key("__rix_idb_old_ver__");
        let k_nv = vm.heap.intern_key("__rix_idb_new_ver__");
        vm.heap.object_define_data(
            ev_h,
            k_ov,
            JsValue::from_number(old_ver),
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
        vm.heap
            .object_define_data(ev_h, k_nv, new_ver, PropertyAttrs::empty(), &mut vm.shapes);
        Ok(JsValue::from_object(ev_h))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "IDBVersionChangeEvent", JsValue::from_object(ctor));
}

fn install_idb_record(vm: &mut JsVm, idb: &SharedIndexedDb) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    define_to_string_tag(vm, proto, "IDBRecord");
    idb.borrow_mut().protos.record_proto = Some(proto);

    for (prop, slot) in [
        ("key", "__rix_rec_key__"),
        ("primaryKey", "__rix_rec_pk__"),
        ("value", "__rix_rec_val__"),
    ] {
        define_host_getter(vm, proto, prop, move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let k = vm.heap.intern_key(slot);
            vm.heap
                .object_get_own(h, k, &vm.shapes)
                .map(|p| p.value)
                .ok_or_else(|| vm.type_error("Illegal invocation"))
        });
    }

    install_illegal_ctor(vm, "IDBRecord", proto);
}

fn install_idb_key_range(vm: &mut JsVm, idb: &SharedIndexedDb) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    define_to_string_tag(vm, proto, "IDBKeyRange");
    idb.borrow_mut().protos.key_range_proto = Some(proto);

    for (prop, which) in [
        ("lower", 0u8),
        ("upper", 1),
        ("lowerOpen", 2),
        ("upperOpen", 3),
    ] {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, prop, move |vm, this_val, _| {
            let kr = this_val
                .as_object()
                .and_then(|h| db.borrow().key_ranges.get(&h).cloned());
            let Some(kr) = kr else {
                return Err(vm.type_error("Illegal invocation"));
            };
            Ok(match which {
                0 => kr
                    .lower
                    .as_ref()
                    .map_or(JsValue::UNDEFINED, |k| idb_key_to_js(vm, k)),
                1 => kr
                    .upper
                    .as_ref()
                    .map_or(JsValue::UNDEFINED, |k| idb_key_to_js(vm, k)),
                2 => JsValue::from_bool(kr.lower_open),
                _ => JsValue::from_bool(kr.upper_open),
            })
        });
    }

    {
        let db = Rc::clone(idb);
        define_host_method(vm, proto, "includes", move |vm, this_val, args| {
            let Some(kr) = this_val
                .as_object()
                .and_then(|h| db.borrow().key_ranges.get(&h).cloned())
            else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if args.is_empty() {
                return Err(vm.type_error("IDBKeyRange.includes requires 1 argument"));
            }
            let frames = idb_frames(&db);
            let mut seen = HashSet::new();
            let k = js_to_idb_key(vm, frames.as_ref(), arg(args, 0), &mut seen)?;
            Ok(JsValue::from_bool(kr.matches(&k)))
        });
    }

    let ctor = vm.alloc_host_fn("IDBKeyRange", |vm, _, _| {
        Err(vm.type_error("Illegal constructor"))
    });
    bind_ctor_proto(vm, ctor, proto);

    let alloc_kr = move |vm: &mut JsVm, db: &SharedIndexedDb, kr: KeyRange| -> JsValue {
        let h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        db.borrow_mut().key_ranges.insert(h, kr);
        JsValue::from_object(h)
    };

    {
        let db = Rc::clone(idb);
        define_host_method(vm, ctor, "only", move |vm, _, args| {
            if args.is_empty() {
                return Err(vm.type_error("IDBKeyRange.only requires 1 argument"));
            }
            let frames = idb_frames(&db);
            let mut seen = HashSet::new();
            let k = js_to_idb_key(vm, frames.as_ref(), arg(args, 0), &mut seen)?;
            Ok(alloc_kr(vm, &db, KeyRange::only(k)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_method(vm, ctor, "lowerBound", move |vm, _, args| {
            if args.is_empty() {
                return Err(vm.type_error("IDBKeyRange.lowerBound requires at least 1 argument"));
            }
            let frames = idb_frames(&db);
            let mut seen = HashSet::new();
            let lo = js_to_idb_key(vm, frames.as_ref(), arg(args, 0), &mut seen)?;
            let lo_open = vm.to_boolean(arg(args, 1));
            Ok(alloc_kr(
                vm,
                &db,
                KeyRange {
                    lower: Some(lo),
                    upper: None,
                    lower_open: lo_open,
                    upper_open: true,
                },
            ))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_method(vm, ctor, "upperBound", move |vm, _, args| {
            if args.is_empty() {
                return Err(vm.type_error("IDBKeyRange.upperBound requires at least 1 argument"));
            }
            let frames = idb_frames(&db);
            let mut seen = HashSet::new();
            let hi = js_to_idb_key(vm, frames.as_ref(), arg(args, 0), &mut seen)?;
            let hi_open = vm.to_boolean(arg(args, 1));
            Ok(alloc_kr(
                vm,
                &db,
                KeyRange {
                    lower: None,
                    upper: Some(hi),
                    lower_open: true,
                    upper_open: hi_open,
                },
            ))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_method(vm, ctor, "bound", move |vm, _, args| {
            if args.len() < 2 {
                return Err(vm.type_error("IDBKeyRange.bound requires at least 2 arguments"));
            }
            let frames = idb_frames(&db);
            let mut seen = HashSet::new();
            let lo = js_to_idb_key(vm, frames.as_ref(), arg(args, 0), &mut seen)?;
            seen.clear();
            let hi = js_to_idb_key(vm, frames.as_ref(), arg(args, 1), &mut seen)?;
            let lo_open = vm.to_boolean(arg(args, 2));
            let hi_open = vm.to_boolean(arg(args, 3));
            match lo.idb_cmp(&hi) {
                Ordering::Greater => {
                    return Err(create_dom_exception(
                        vm,
                        "DataError",
                        "IDBKeyRange.bound lower key is greater than upper key",
                    ));
                }
                Ordering::Equal if lo_open || hi_open => {
                    return Err(create_dom_exception(
                        vm,
                        "DataError",
                        "IDBKeyRange.bound keys are equal and range is open",
                    ));
                }
                _ => {}
            }
            Ok(alloc_kr(
                vm,
                &db,
                KeyRange {
                    lower: Some(lo),
                    upper: Some(hi),
                    lower_open: lo_open,
                    upper_open: hi_open,
                },
            ))
        });
    }

    define_global(vm, "IDBKeyRange", JsValue::from_object(ctor));
}

fn install_idb_request_protos(
    vm: &mut JsVm,
    event_target_proto: HeapHandle,
    events: &SharedEvents,
    idb: &SharedIndexedDb,
) {
    let req_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(event_target_proto),
    ));
    define_to_string_tag(vm, req_proto, "IDBRequest");
    let open_proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(req_proto)));
    define_to_string_tag(vm, open_proto, "IDBOpenDBRequest");
    {
        let mut st = idb.borrow_mut();
        st.protos.request_proto = Some(req_proto);
        st.protos.open_request_proto = Some(open_proto);
    }

    {
        let db = Rc::clone(idb);
        define_host_getter(vm, req_proto, "result", move |vm, this_val, _| {
            let state = this_val.as_object().and_then(|h| {
                db.borrow()
                    .requests
                    .get(&h)
                    .map(|r| (r.ready_state, r.result))
            });
            match state {
                Some(("done", res)) => Ok(res),
                Some(_) => Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "The request has not finished",
                )),
                None => Err(vm.type_error("Illegal invocation")),
            }
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, req_proto, "error", move |vm, this_val, _| {
            let state = this_val.as_object().and_then(|h| {
                db.borrow()
                    .requests
                    .get(&h)
                    .map(|r| (r.ready_state, r.error))
            });
            match state {
                Some(("done", err)) => Ok(err),
                Some(_) => Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "The request has not finished",
                )),
                None => Err(vm.type_error("Illegal invocation")),
            }
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, req_proto, "readyState", move |vm, this_val, _| {
            let Some(s) = this_val
                .as_object()
                .and_then(|h| db.borrow().requests.get(&h).map(|r| r.ready_state))
            else {
                return Err(vm.type_error("Illegal invocation"));
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(s)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, req_proto, "source", move |vm, this_val, _| {
            this_val
                .as_object()
                .and_then(|h| db.borrow().requests.get(&h).map(|r| r.source))
                .ok_or_else(|| vm.type_error("Illegal invocation"))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, req_proto, "transaction", move |vm, this_val, _| {
            this_val
                .as_object()
                .and_then(|h| db.borrow().requests.get(&h).map(|r| r.transaction))
                .ok_or_else(|| vm.type_error("Illegal invocation"))
        });
    }

    // WHATWG HTML §8.1.7.1 EventHandler IDL accessors on IDBRequest & IDBOpenDBRequest.
    for attr in ["onsuccess", "onerror"] {
        let db = Rc::clone(idb);
        install_object_event_handler_accessor(vm, req_proto, events, attr, move |vm, this_v| {
            this_v
                .as_object()
                .filter(|h| db.borrow().requests.contains_key(h))
                .ok_or_else(|| vm.type_error("Illegal invocation"))
        });
    }
    for attr in ["onblocked", "onupgradeneeded"] {
        let db = Rc::clone(idb);
        install_object_event_handler_accessor(vm, open_proto, events, attr, move |vm, this_v| {
            this_v
                .as_object()
                .filter(|h| db.borrow().requests.contains_key(h))
                .ok_or_else(|| vm.type_error("Illegal invocation"))
        });
    }

    install_illegal_ctor(vm, "IDBRequest", req_proto);
    install_illegal_ctor(vm, "IDBOpenDBRequest", open_proto);
}

#[allow(clippy::too_many_arguments)]
fn install_idb_cursor_proto(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    idb: &SharedIndexedDb,
) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    define_to_string_tag(vm, proto, "IDBCursor");
    let with_val_proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    define_to_string_tag(vm, with_val_proto, "IDBCursorWithValue");
    {
        let mut st = idb.borrow_mut();
        st.protos.cursor_proto = Some(proto);
        st.protos.cursor_with_value_proto = Some(with_val_proto);
    }

    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "source", move |vm, this_val, _| {
            this_val
                .as_object()
                .and_then(|h| {
                    db.borrow()
                        .cursors
                        .get(&h)
                        .map(|c| JsValue::from_object(c.source_h))
                })
                .ok_or_else(|| vm.type_error("Illegal invocation"))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "direction", move |vm, this_val, _| {
            let Some(s) = this_val
                .as_object()
                .and_then(|h| db.borrow().cursors.get(&h).map(|c| c.direction.as_str()))
            else {
                return Err(vm.type_error("Illegal invocation"));
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(s)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "request", move |vm, this_val, _| {
            this_val
                .as_object()
                .and_then(|h| {
                    db.borrow()
                        .cursors
                        .get(&h)
                        .map(|c| JsValue::from_object(c.req_h))
                })
                .ok_or_else(|| vm.type_error("Illegal invocation"))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "key", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let key = {
                let st = db.borrow();
                let Some(c) = st.cursors.get(&h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                c.entries.get(c.index).map(|e| e.key.clone())
            };
            Ok(key
                .as_ref()
                .map_or(JsValue::UNDEFINED, |k| idb_key_to_js(vm, k)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "primaryKey", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let pk = {
                let st = db.borrow();
                let Some(c) = st.cursors.get(&h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                c.entries.get(c.index).map(|e| e.primary_key.clone())
            };
            Ok(pk
                .as_ref()
                .map_or(JsValue::UNDEFINED, |k| idb_key_to_js(vm, k)))
        });
    }
    // `.value` belongs strictly on `IDBCursorWithValue.prototype`, NOT `IDBCursor.prototype`!
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let wa_r = Rc::clone(web_apis);
        let db = Rc::clone(idb);
        define_host_getter(vm, with_val_proto, "value", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let (val_data, deps) = {
                let st = db.borrow();
                let Some(c) = st.cursors.get(&h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if c.key_only {
                    return Err(vm.type_error("Illegal invocation"));
                }
                (
                    c.entries
                        .get(c.index)
                        .map(|e| (e.value_bytes.clone(), e.value_json.clone())),
                    st.clone_deps.clone(),
                )
            };
            let Some((vb, vj)) = val_data else {
                return Ok(JsValue::UNDEFINED);
            };
            Ok(idb_deserialize_entry(
                vm,
                &h_r,
                &ev_r,
                &wa_r,
                deps.as_ref(),
                &vb,
                &vj,
            ))
        });
    }

    // cursor.continue(key?)
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, "continue", move |vm, this_val, args| {
            let Some(ch) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let target_key = if arg(args, 0).is_undefined() {
                None
            } else {
                let frames = idb_frames(&db_r);
                let mut seen = HashSet::new();
                Some(js_to_idb_key(vm, frames.as_ref(), arg(args, 0), &mut seen)?)
            };
            let (tx_h, req_h) = {
                let mut st = db_r.borrow_mut();
                let Some(c) = st.cursors.get_mut(&ch) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if !c.got_value {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Cursor has already been iterated",
                    ));
                }
                if let Some(ref tk) = target_key {
                    if let Some(cur_entry) = c.entries.get(c.index) {
                        let invalid = if c.direction.is_reverse() {
                            tk.idb_cmp(&cur_entry.key) != Ordering::Less
                        } else {
                            tk.idb_cmp(&cur_entry.key) != Ordering::Greater
                        };
                        if invalid {
                            return Err(create_dom_exception(
                                vm,
                                "DataError",
                                "Cursor continue() key does not advance in cursor direction",
                            ));
                        }
                    }
                    let mut next_idx = c.index + 1;
                    while next_idx < c.entries.len() {
                        let ek = &c.entries[next_idx].key;
                        let reached = if c.direction.is_reverse() {
                            ek.idb_cmp(tk) != Ordering::Greater
                        } else {
                            ek.idb_cmp(tk) != Ordering::Less
                        };
                        if reached {
                            break;
                        }
                        next_idx += 1;
                    }
                    c.index = next_idx;
                } else {
                    c.index += 1;
                }
                c.got_value = false;
                let tx_h = c.tx_h;
                let req_h = c.req_h;
                if let Some(r) = st.requests.get_mut(&req_h) {
                    r.ready_state = "pending";
                }
                if let Some(tx) = st.transactions.get_mut(&tx_h) {
                    tx.pending_requests += 1;
                }
                (tx_h, req_h)
            };
            schedule_cursor_step(&h_r, &ev_r, &el_r, &wa_r, &db_r, tx_h, req_h, ch);
            Ok(JsValue::UNDEFINED)
        });
    }

    // cursor.continuePrimaryKey(key, primaryKey)
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(
            vm,
            proto,
            "continuePrimaryKey",
            move |vm, this_val, args| {
                let Some(ch) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if args.len() < 2 {
                    return Err(vm.type_error("IDBCursor.continuePrimaryKey requires 2 arguments"));
                }
                let (tx_h, req_h, is_index, dir, got_value, cur_key, cur_pk) = {
                    let st = db_r.borrow();
                    let Some(c) = st.cursors.get(&ch) else {
                        return Err(vm.type_error("Illegal invocation"));
                    };
                    let cur = c.entries.get(c.index);
                    (
                        c.tx_h,
                        c.req_h,
                        c.index_name.is_some(),
                        c.direction,
                        c.got_value,
                        cur.map(|e| e.key.clone()),
                        cur.map(|e| e.primary_key.clone()),
                    )
                };
                if !is_index || dir.is_unique() {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidAccessError",
                        "continuePrimaryKey() requires a non-unique index cursor",
                    ));
                }
                if !got_value {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Cursor has already been iterated",
                    ));
                }
                let frames = idb_frames(&db_r);
                let mut seen = HashSet::new();
                let target_key = js_to_idb_key(vm, frames.as_ref(), arg(args, 0), &mut seen)?;
                seen.clear();
                let target_pk = js_to_idb_key(vm, frames.as_ref(), arg(args, 1), &mut seen)?;
                if let (Some(ref ck), Some(ref cpk)) = (cur_key, cur_pk) {
                    let cmp_k = target_key.idb_cmp(ck);
                    let cmp_pk = target_pk.idb_cmp(cpk);
                    let invalid = if dir.is_reverse() {
                        cmp_k == Ordering::Greater
                            || (cmp_k == Ordering::Equal && cmp_pk != Ordering::Less)
                    } else {
                        cmp_k == Ordering::Less
                            || (cmp_k == Ordering::Equal && cmp_pk != Ordering::Greater)
                    };
                    if invalid {
                        return Err(create_dom_exception(
                        vm,
                        "DataError",
                        "continuePrimaryKey() key/primaryKey does not advance in cursor direction",
                    ));
                    }
                }
                {
                    let mut st = db_r.borrow_mut();
                    if let Some(c) = st.cursors.get_mut(&ch) {
                        let mut next_idx = c.index + 1;
                        while next_idx < c.entries.len() {
                            let e = &c.entries[next_idx];
                            let cmp_k = e.key.idb_cmp(&target_key);
                            let cmp_pk = e.primary_key.idb_cmp(&target_pk);
                            let reached = if c.direction.is_reverse() {
                                cmp_k == Ordering::Less
                                    || (cmp_k == Ordering::Equal && cmp_pk != Ordering::Greater)
                            } else {
                                cmp_k == Ordering::Greater
                                    || (cmp_k == Ordering::Equal && cmp_pk != Ordering::Less)
                            };
                            if reached {
                                break;
                            }
                            next_idx += 1;
                        }
                        c.index = next_idx;
                        c.got_value = false;
                    }
                    if let Some(r) = st.requests.get_mut(&req_h) {
                        r.ready_state = "pending";
                    }
                    if let Some(tx) = st.transactions.get_mut(&tx_h) {
                        tx.pending_requests += 1;
                    }
                }
                schedule_cursor_step(&h_r, &ev_r, &el_r, &wa_r, &db_r, tx_h, req_h, ch);
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // cursor.advance(count)
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, "advance", move |vm, this_val, args| {
            let Some(ch) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if args.is_empty() {
                return Err(vm.type_error("IDBCursor.advance requires 1 argument"));
            }
            let count = enforce_range_u32(vm, arg(args, 0))?;
            if count == 0 {
                return Err(vm.type_error("IDBCursor.advance count must be >= 1"));
            }
            let step = count as usize;
            let (tx_h, req_h) = {
                let mut st = db_r.borrow_mut();
                let Some(c) = st.cursors.get_mut(&ch) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if !c.got_value {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Cursor has already been iterated",
                    ));
                }
                c.index = c.index.saturating_add(step);
                c.got_value = false;
                let tx_h = c.tx_h;
                let req_h = c.req_h;
                if let Some(r) = st.requests.get_mut(&req_h) {
                    r.ready_state = "pending";
                }
                if let Some(tx) = st.transactions.get_mut(&tx_h) {
                    tx.pending_requests += 1;
                }
                (tx_h, req_h)
            };
            schedule_cursor_step(&h_r, &ev_r, &el_r, &wa_r, &db_r, tx_h, req_h, ch);
            Ok(JsValue::UNDEFINED)
        });
    }

    // cursor.update(value)
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, "update", move |vm, this_val, args| {
            let Some(ch) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if args.is_empty() {
                return Err(vm.type_error("IDBCursor.update requires 1 argument"));
            }
            let info = {
                let st = db_r.borrow();
                let c = st
                    .cursors
                    .get(&ch)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                if c.key_only {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Cannot call update() on a key cursor",
                    ));
                }
                let entry = c.entries.get(c.index).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Cursor has no current value")
                })?;
                (c.tx_h, c.store_name.clone(), entry.primary_key.clone())
            };
            let (tx_h, store_name, pk) = info;
            let is_ro = db_r
                .borrow()
                .transactions
                .get(&tx_h)
                .is_some_and(|t| t.mode == TxMode::ReadOnly);
            if is_ro {
                return Err(create_dom_exception(
                    vm,
                    "ReadOnlyError",
                    "Cannot call cursor.update() inside a readonly transaction",
                ));
            }
            let payload = idb_serialize_value(vm, &h_r, &ev_r, &wa_r, &db_r, arg(args, 0))?;
            // W3C IDB §4.7: if store uses inline keys, evaluated key MUST equal cursor's primaryKey!
            {
                let st = db_r.borrow();
                if let Some(store) = st
                    .transactions
                    .get(&tx_h)
                    .and_then(|tx| tx.working.stores.get(&store_name))
                {
                    if store.key_path.is_some() {
                        match validate_put_or_add_sync(store, &payload, None) {
                            Ok(Some(extracted)) => {
                                if extracted.idb_cmp(&pk) != Ordering::Equal {
                                    return Err(create_dom_exception(
                                        vm,
                                        "DataError",
                                        "Updated value's key does not match cursor's primaryKey",
                                    ));
                                }
                            }
                            Ok(None) => {
                                return Err(create_dom_exception(
                                    vm,
                                    "DataError",
                                    "Updated value is missing inline key",
                                ));
                            }
                            Err((en, em)) => return Err(create_dom_exception(vm, en, &em)),
                        }
                    }
                }
            }
            let req_h = enqueue_tx_request(
                vm,
                &h_r,
                &ev_r,
                &el_r,
                &wa_r,
                &db_r,
                tx_h,
                ch,
                move |vm, tx| {
                    let store = tx
                        .working
                        .stores
                        .get_mut(&store_name)
                        .ok_or(("NotFoundError", "Object store not found".to_string()))?;
                    let key = store_put_or_add_payload(store, payload, Some(pk), false)?;
                    Ok(idb_key_to_js(vm, &key))
                },
            )?;
            Ok(JsValue::from_object(req_h))
        });
    }

    // cursor.delete()
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, "delete", move |vm, this_val, _| {
            let Some(ch) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let info = {
                let st = db_r.borrow();
                let c = st
                    .cursors
                    .get(&ch)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                if c.key_only {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Cannot call delete() on a key cursor",
                    ));
                }
                let entry = c.entries.get(c.index).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Cursor has no current value")
                })?;
                (c.tx_h, c.store_name.clone(), entry.primary_key.clone())
            };
            let (tx_h, store_name, pk) = info;
            let is_ro = db_r
                .borrow()
                .transactions
                .get(&tx_h)
                .is_some_and(|t| t.mode == TxMode::ReadOnly);
            if is_ro {
                return Err(create_dom_exception(
                    vm,
                    "ReadOnlyError",
                    "Cannot call cursor.delete() inside a readonly transaction",
                ));
            }
            let req_h = enqueue_tx_request(
                vm,
                &h_r,
                &ev_r,
                &el_r,
                &wa_r,
                &db_r,
                tx_h,
                ch,
                move |_, tx| {
                    let store = tx
                        .working
                        .stores
                        .get_mut(&store_name)
                        .ok_or(("NotFoundError", "Object store not found".to_string()))?;
                    store.records.remove(&pk.to_encoded_bytes());
                    Ok(JsValue::UNDEFINED)
                },
            )?;
            Ok(JsValue::from_object(req_h))
        });
    }

    install_illegal_ctor(vm, "IDBCursor", proto);
    install_illegal_ctor(vm, "IDBCursorWithValue", with_val_proto);
}

#[allow(clippy::too_many_arguments)]
fn schedule_cursor_step(
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    idb: &SharedIndexedDb,
    tx_h: HeapHandle,
    req_h: HeapHandle,
    cursor_h: HeapHandle,
) {
    schedule_tx_commit_check(host, events, event_loop, web_apis, idb, tx_h);
    let h = Rc::clone(host);
    let ev = Rc::clone(events);
    let el = Rc::clone(event_loop);
    let wa = Rc::clone(web_apis);
    let db_sys = Rc::clone(idb);
    event_loop
        .borrow_mut()
        .post_host_task(TaskPriority::Networking, move |vm| {
            let result = {
                let mut st = db_sys.borrow_mut();
                let mut needs_init = false;
                if let Some(tx) = st.transactions.get_mut(&tx_h) {
                    tx.pending_requests = tx.pending_requests.saturating_sub(1);
                    if tx.aborted || tx.finished {
                        None
                    } else {
                        if let Some(c) = st.cursors.get(&cursor_h) {
                            needs_init = !c.initialized;
                        }
                        if needs_init {
                            let collected = st.cursors.get(&cursor_h).and_then(|c| {
                                let tx = st.transactions.get(&tx_h)?;
                                let store = tx.working.stores.get(&c.store_name)?;
                                Some(if let Some(ref idx_name) = c.index_name {
                                    if let Some(idx_schema) = store.indexes.get(idx_name) {
                                        collect_index_entries(
                                            store,
                                            idx_schema,
                                            c.range.as_ref(),
                                            c.direction,
                                        )
                                    } else {
                                        Vec::new()
                                    }
                                } else {
                                    collect_store_entries(store, c.range.as_ref(), c.direction)
                                })
                            });
                            if let (Some(c), Some(entries)) =
                                (st.cursors.get_mut(&cursor_h), collected)
                            {
                                c.entries = entries;
                                c.initialized = true;
                            }
                        }
                        let has_item = match st.cursors.get_mut(&cursor_h) {
                            Some(c) if c.index < c.entries.len() => {
                                c.got_value = true;
                                true
                            }
                            _ => false,
                        };
                        Some(if has_item {
                            JsValue::from_object(cursor_h)
                        } else {
                            JsValue::NULL
                        })
                    }
                } else {
                    None
                }
            };
            if let Some(res_val) = result {
                if let Some(r) = db_sys.borrow_mut().requests.get_mut(&req_h) {
                    r.ready_state = "done";
                    r.result = res_val;
                    r.error = JsValue::NULL;
                }
                let ev_proto = ev.borrow().event_proto.unwrap_or(vm.object_proto);
                let (_, threw) = fire_idb_event(
                    vm,
                    &h,
                    &ev,
                    &el,
                    req_h,
                    "success",
                    false,
                    false,
                    ev_proto,
                    &[],
                );
                if threw {
                    abort_tx_with_error(
                        vm,
                        &db_sys,
                        tx_h,
                        "AbortError",
                        "Uncaught exception in cursor success handler",
                    );
                }
            }
            schedule_tx_commit_check(&h, &ev, &el, &wa, &db_sys, tx_h);
        });
}

#[allow(clippy::too_many_arguments)]
fn install_idb_index_proto(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    idb: &SharedIndexedDb,
) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    define_to_string_tag(vm, proto, "IDBIndex");
    idb.borrow_mut().protos.index_proto = Some(proto);

    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "name", move |vm, this_val, _| {
            let Some(s) = this_val
                .as_object()
                .and_then(|h| db.borrow().indexes.get(&h).map(|i| i.index_name.clone()))
            else {
                return Err(vm.type_error("Illegal invocation"));
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "objectStore", move |vm, this_val, _| {
            this_val
                .as_object()
                .and_then(|h| {
                    db.borrow()
                        .indexes
                        .get(&h)
                        .map(|i| JsValue::from_object(i.store_h))
                })
                .ok_or_else(|| vm.type_error("Illegal invocation"))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "keyPath", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let existing_cached = {
                let st = db.borrow();
                let Some(idx) = st.indexes.get(&h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                idx.cached_key_path
            };
            if let Some(v) = existing_cached {
                return Ok(v);
            }
            let kp = {
                let st = db.borrow();
                st.indexes.get(&h).and_then(|idx| {
                    let tx = st.transactions.get(&idx.tx_h)?;
                    let store = tx.working.stores.get(&idx.store_name)?;
                    store
                        .indexes
                        .get(&idx.index_name)
                        .map(|s| (s.key_path.clone(), s.key_path_is_array))
                })
            };
            let val = key_path_to_js(vm, kp);
            if let Some(idx) = db.borrow_mut().indexes.get_mut(&h) {
                idx.cached_key_path = Some(val);
            }
            Ok(val)
        });
    }
    for (prop, is_unique) in [("unique", true), ("multiEntry", false)] {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, prop, move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let b = {
                let st = db.borrow();
                let Some(idx) = st.indexes.get(&h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                st.transactions
                    .get(&idx.tx_h)
                    .and_then(|tx| tx.working.stores.get(&idx.store_name))
                    .and_then(|store| store.indexes.get(&idx.index_name))
                    .map(|s| if is_unique { s.unique } else { s.multi_entry })
                    .unwrap_or(false)
            };
            Ok(JsValue::from_bool(b))
        });
    }

    // get / getKey / getAll / getAllKeys / getAllRecords / count on IDBIndex
    for (method_name, mode_tag) in [
        ("get", 0u8),
        ("getKey", 1),
        ("getAll", 2),
        ("getAllKeys", 3),
        ("count", 4),
        ("getAllRecords", 5),
    ] {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, method_name, move |vm, this_val, args| {
            let Some(ih) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if (mode_tag == 0 || mode_tag == 1) && args.is_empty() {
                return Err(vm.type_error(format!("IDBIndex.{method_name} requires 1 argument")));
            }
            let (tx_h, store_name, index_name) = {
                let st = db_r.borrow();
                let idx = st
                    .indexes
                    .get(&ih)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                // W3C IDB: check deleted store/index BEFORE transaction inactive / key conversion!
                let is_deleted = st
                    .transactions
                    .get(&idx.tx_h)
                    .and_then(|tx| tx.working.stores.get(&idx.store_name))
                    .and_then(|store| store.indexes.get(&idx.index_name))
                    .is_none();
                if is_deleted {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Index or its object store has been deleted",
                    ));
                }
                (idx.tx_h, idx.store_name.clone(), idx.index_name.clone())
            };
            let (range, count_limit, direction) = match mode_tag {
                0 | 1 => (
                    Some(require_query_key_range(vm, &db_r, arg(args, 0))?),
                    None,
                    CursorDirection::Next,
                ),
                2 | 3 => parse_get_all_args(vm, &db_r, args, false)?,
                5 => parse_get_all_args(vm, &db_r, args, true)?,
                _ => (
                    query_to_key_range(vm, &db_r, arg(args, 0))?,
                    None,
                    CursorDirection::Next,
                ),
            };
            let h_c = Rc::clone(&h_r);
            let ev_c = Rc::clone(&ev_r);
            let wa_c = Rc::clone(&wa_r);
            let (deps_c, record_proto) = {
                let st = db_r.borrow();
                (
                    st.clone_deps.clone(),
                    st.protos.record_proto.unwrap_or(vm.object_proto),
                )
            };
            let req_h = enqueue_tx_request(
                vm,
                &h_r,
                &ev_r,
                &el_r,
                &wa_r,
                &db_r,
                tx_h,
                ih,
                move |vm, tx| {
                    let store = tx
                        .working
                        .stores
                        .get(&store_name)
                        .ok_or(("NotFoundError", "Object store not found".to_string()))?;
                    let idx_schema = store
                        .indexes
                        .get(&index_name)
                        .ok_or(("NotFoundError", "Index not found".to_string()))?;
                    let entries =
                        collect_index_entries(store, idx_schema, range.as_ref(), direction);
                    Ok(materialize_idb_query_result(
                        vm,
                        &h_c,
                        &ev_c,
                        &wa_c,
                        deps_c.as_ref(),
                        record_proto,
                        &entries,
                        mode_tag,
                        count_limit,
                    ))
                },
            )?;
            Ok(JsValue::from_object(req_h))
        });
    }

    // openCursor / openKeyCursor on IDBIndex
    for (method_name, key_only) in [("openCursor", false), ("openKeyCursor", true)] {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, method_name, move |vm, this_val, args| {
            let Some(ih) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let (tx_h, store_name, index_name) = {
                let st = db_r.borrow();
                let idx = st
                    .indexes
                    .get(&ih)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                let is_deleted = st
                    .transactions
                    .get(&idx.tx_h)
                    .and_then(|tx| tx.working.stores.get(&idx.store_name))
                    .and_then(|store| store.indexes.get(&idx.index_name))
                    .is_none();
                if is_deleted {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Index or its object store has been deleted",
                    ));
                }
                (idx.tx_h, idx.store_name.clone(), idx.index_name.clone())
            };
            let range = query_to_key_range(vm, &db_r, arg(args, 0))?;
            let dir = parse_cursor_direction(vm, arg(args, 1))?;
            let req_h = open_cursor_common(
                vm,
                &h_r,
                &ev_r,
                &el_r,
                &wa_r,
                &db_r,
                tx_h,
                ih,
                store_name,
                Some(index_name),
                range,
                dir,
                key_only,
            )?;
            Ok(JsValue::from_object(req_h))
        });
    }

    install_illegal_ctor(vm, "IDBIndex", proto);
}

#[allow(clippy::too_many_arguments)]
fn open_cursor_common(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    idb: &SharedIndexedDb,
    tx_h: HeapHandle,
    source_h: HeapHandle,
    store_name: String,
    index_name: Option<String>,
    range: Option<KeyRange>,
    direction: CursorDirection,
    key_only: bool,
) -> Result<HeapHandle, JsException> {
    {
        let mut st = idb.borrow_mut();
        let Some(tx) = st.transactions.get_mut(&tx_h) else {
            return Err(create_dom_exception(
                vm,
                "TransactionInactiveError",
                "Transaction inactive",
            ));
        };
        if tx.finished || tx.aborted {
            return Err(create_dom_exception(
                vm,
                "TransactionInactiveError",
                "Transaction inactive",
            ));
        }
        tx.pending_requests += 1;
    }
    let req_h = alloc_request(
        vm,
        events,
        idb,
        false,
        JsValue::from_object(source_h),
        JsValue::from_object(tx_h),
    );
    let cursor_proto = {
        let st = idb.borrow();
        if key_only {
            st.protos.cursor_proto
        } else {
            st.protos.cursor_with_value_proto
        }
        .unwrap_or(vm.object_proto)
    };
    let cursor_h = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(cursor_proto),
    ));
    idb.borrow_mut().cursors.insert(
        cursor_h,
        IdbCursorState {
            tx_h,
            req_h,
            source_h,
            store_name,
            index_name,
            range,
            direction,
            key_only,
            initialized: false,
            entries: Vec::new(),
            index: 0,
            got_value: false,
        },
    );
    schedule_cursor_step(
        host, events, event_loop, web_apis, idb, tx_h, req_h, cursor_h,
    );
    Ok(req_h)
}

#[allow(clippy::too_many_arguments)]
fn install_idb_object_store_proto(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    idb: &SharedIndexedDb,
) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    define_to_string_tag(vm, proto, "IDBObjectStore");
    idb.borrow_mut().protos.object_store_proto = Some(proto);

    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "name", move |vm, this_val, _| {
            let Some(s) = this_val
                .as_object()
                .and_then(|h| db.borrow().stores.get(&h).map(|s| s.name.clone()))
            else {
                return Err(vm.type_error("Illegal invocation"));
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "transaction", move |vm, this_val, _| {
            this_val
                .as_object()
                .and_then(|h| {
                    db.borrow()
                        .stores
                        .get(&h)
                        .map(|s| JsValue::from_object(s.tx_h))
                })
                .ok_or_else(|| vm.type_error("Illegal invocation"))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "keyPath", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let existing = {
                let st = db.borrow();
                let Some(sh) = st.stores.get(&h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                sh.cached_key_path
            };
            if let Some(v) = existing {
                return Ok(v);
            }
            let kp = {
                let st = db.borrow();
                st.stores.get(&h).and_then(|sh| {
                    let tx = st.transactions.get(&sh.tx_h)?;
                    let s = tx.working.stores.get(&sh.name)?;
                    s.key_path.clone().map(|v| (v, s.key_path_is_array))
                })
            };
            let val = key_path_to_js(vm, kp);
            if let Some(sh) = db.borrow_mut().stores.get_mut(&h) {
                sh.cached_key_path = Some(val);
            }
            Ok(val)
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "autoIncrement", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let ai = {
                let st = db.borrow();
                let Some(sh) = st.stores.get(&h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                st.transactions
                    .get(&sh.tx_h)
                    .and_then(|tx| tx.working.stores.get(&sh.name))
                    .map(|s| s.auto_increment)
                    .unwrap_or(false)
            };
            Ok(JsValue::from_bool(ai))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "indexNames", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let (names, dsl_proto) = {
                let st = db.borrow();
                let Some(sh) = st.stores.get(&h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                let n = st
                    .transactions
                    .get(&sh.tx_h)
                    .and_then(|tx| tx.working.stores.get(&sh.name))
                    .map(|s| s.indexes.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                (
                    n,
                    st.protos.dom_string_list_proto.unwrap_or(vm.object_proto),
                )
            };
            Ok(JsValue::from_object(alloc_dom_string_list(
                vm, dsl_proto, names,
            )))
        });
    }

    // put(value, key?) & add(value, key?)
    for (method_name, no_overwrite) in [("put", false), ("add", true)] {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, method_name, move |vm, this_val, args| {
            let Some(sh) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if args.is_empty() {
                return Err(vm.type_error(format!(
                    "IDBObjectStore.{method_name} requires at least 1 argument"
                )));
            }
            let (tx_h, store_name) = {
                let st = db_r.borrow();
                let s = st
                    .stores
                    .get(&sh)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                let tx = st.transactions.get(&s.tx_h).ok_or_else(|| {
                    create_dom_exception(vm, "TransactionInactiveError", "Transaction inactive")
                })?;
                if !tx.working.stores.contains_key(&s.name) {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Object store has been deleted",
                    ));
                }
                if tx.finished || tx.aborted {
                    return Err(create_dom_exception(
                        vm,
                        "TransactionInactiveError",
                        "Transaction inactive",
                    ));
                }
                if tx.mode == TxMode::ReadOnly {
                    return Err(create_dom_exception(
                        vm,
                        "ReadOnlyError",
                        "Cannot mutate object store in a readonly transaction",
                    ));
                }
                (s.tx_h, s.name.clone())
            };
            // W3C IDB §4.5: clone value BEFORE evaluating keyPath (so getters only run once!).
            let payload = idb_serialize_value(vm, &h_r, &ev_r, &wa_r, &db_r, arg(args, 0))?;
            let explicit_key = if arg(args, 1).is_undefined() {
                None
            } else {
                let frames = idb_frames(&db_r);
                let mut kseen = HashSet::new();
                Some(js_to_idb_key(
                    vm,
                    frames.as_ref(),
                    arg(args, 1),
                    &mut kseen,
                )?)
            };
            let resolved_key = {
                let st = db_r.borrow();
                let tx = st.transactions.get(&tx_h).ok_or_else(|| {
                    create_dom_exception(vm, "TransactionInactiveError", "Transaction inactive")
                })?;
                let store = tx.working.stores.get(&store_name).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Object store has been deleted")
                })?;
                validate_put_or_add_sync(store, &payload, explicit_key)
                    .map_err(|(en, em)| create_dom_exception(vm, en, &em))?
            };
            let req_h = enqueue_tx_request(
                vm,
                &h_r,
                &ev_r,
                &el_r,
                &wa_r,
                &db_r,
                tx_h,
                sh,
                move |vm, tx| {
                    let store = tx
                        .working
                        .stores
                        .get_mut(&store_name)
                        .ok_or(("NotFoundError", "Object store not found".to_string()))?;
                    let key = store_put_or_add_payload(store, payload, resolved_key, no_overwrite)?;
                    Ok(idb_key_to_js(vm, &key))
                },
            )?;
            Ok(JsValue::from_object(req_h))
        });
    }

    // get / getKey / getAll / getAllKeys / getAllRecords / count
    for (method_name, mode_tag) in [
        ("get", 0u8),
        ("getKey", 1),
        ("getAll", 2),
        ("getAllKeys", 3),
        ("count", 4),
        ("getAllRecords", 5),
    ] {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, method_name, move |vm, this_val, args| {
            let Some(sh) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if (mode_tag == 0 || mode_tag == 1) && args.is_empty() {
                return Err(
                    vm.type_error(format!("IDBObjectStore.{method_name} requires 1 argument"))
                );
            }
            let (tx_h, store_name) = {
                let st = db_r.borrow();
                let s = st
                    .stores
                    .get(&sh)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                let deleted = st
                    .transactions
                    .get(&s.tx_h)
                    .and_then(|tx| tx.working.stores.get(&s.name))
                    .is_none();
                if deleted {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Object store has been deleted",
                    ));
                }
                (s.tx_h, s.name.clone())
            };
            let (range, count_limit, direction) = match mode_tag {
                0 | 1 => (
                    Some(require_query_key_range(vm, &db_r, arg(args, 0))?),
                    None,
                    CursorDirection::Next,
                ),
                2 | 3 => parse_get_all_args(vm, &db_r, args, false)?,
                5 => parse_get_all_args(vm, &db_r, args, true)?,
                _ => (
                    query_to_key_range(vm, &db_r, arg(args, 0))?,
                    None,
                    CursorDirection::Next,
                ),
            };
            let h_c = Rc::clone(&h_r);
            let ev_c = Rc::clone(&ev_r);
            let wa_c = Rc::clone(&wa_r);
            let (deps_c, record_proto) = {
                let st = db_r.borrow();
                (
                    st.clone_deps.clone(),
                    st.protos.record_proto.unwrap_or(vm.object_proto),
                )
            };
            let req_h = enqueue_tx_request(
                vm,
                &h_r,
                &ev_r,
                &el_r,
                &wa_r,
                &db_r,
                tx_h,
                sh,
                move |vm, tx| {
                    let store = tx
                        .working
                        .stores
                        .get(&store_name)
                        .ok_or(("NotFoundError", "Object store not found".to_string()))?;
                    let entries = collect_store_entries(store, range.as_ref(), direction);
                    Ok(materialize_idb_query_result(
                        vm,
                        &h_c,
                        &ev_c,
                        &wa_c,
                        deps_c.as_ref(),
                        record_proto,
                        &entries,
                        mode_tag,
                        count_limit,
                    ))
                },
            )?;
            Ok(JsValue::from_object(req_h))
        });
    }

    // delete(query)
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, "delete", move |vm, this_val, args| {
            let Some(sh) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if args.is_empty() {
                return Err(vm.type_error("IDBObjectStore.delete requires 1 argument"));
            }
            let (tx_h, store_name) = {
                let st = db_r.borrow();
                let s = st
                    .stores
                    .get(&sh)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                let tx = st.transactions.get(&s.tx_h).ok_or_else(|| {
                    create_dom_exception(vm, "TransactionInactiveError", "Transaction inactive")
                })?;
                if !tx.working.stores.contains_key(&s.name) {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Object store has been deleted",
                    ));
                }
                if tx.finished || tx.aborted {
                    return Err(create_dom_exception(
                        vm,
                        "TransactionInactiveError",
                        "Transaction inactive",
                    ));
                }
                if tx.mode == TxMode::ReadOnly {
                    return Err(create_dom_exception(
                        vm,
                        "ReadOnlyError",
                        "Cannot delete in a readonly transaction",
                    ));
                }
                (s.tx_h, s.name.clone())
            };
            let range = require_query_key_range(vm, &db_r, arg(args, 0))?;
            let req_h = enqueue_tx_request(
                vm,
                &h_r,
                &ev_r,
                &el_r,
                &wa_r,
                &db_r,
                tx_h,
                sh,
                move |_, tx| {
                    let store = tx
                        .working
                        .stores
                        .get_mut(&store_name)
                        .ok_or(("NotFoundError", "Object store not found".to_string()))?;
                    store.records.retain(|_, rec| !range.matches(&rec.key));
                    Ok(JsValue::UNDEFINED)
                },
            )?;
            Ok(JsValue::from_object(req_h))
        });
    }

    // clear()
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, "clear", move |vm, this_val, _| {
            let Some(sh) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let (tx_h, store_name) = {
                let st = db_r.borrow();
                let s = st
                    .stores
                    .get(&sh)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                let tx = st.transactions.get(&s.tx_h).ok_or_else(|| {
                    create_dom_exception(vm, "TransactionInactiveError", "Transaction inactive")
                })?;
                if !tx.working.stores.contains_key(&s.name) {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Object store has been deleted",
                    ));
                }
                if tx.finished || tx.aborted {
                    return Err(create_dom_exception(
                        vm,
                        "TransactionInactiveError",
                        "Transaction inactive",
                    ));
                }
                if tx.mode == TxMode::ReadOnly {
                    return Err(create_dom_exception(
                        vm,
                        "ReadOnlyError",
                        "Cannot clear in a readonly transaction",
                    ));
                }
                (s.tx_h, s.name.clone())
            };
            let req_h = enqueue_tx_request(
                vm,
                &h_r,
                &ev_r,
                &el_r,
                &wa_r,
                &db_r,
                tx_h,
                sh,
                move |_, tx| {
                    let store = tx
                        .working
                        .stores
                        .get_mut(&store_name)
                        .ok_or(("NotFoundError", "Object store not found".to_string()))?;
                    store.records.clear();
                    Ok(JsValue::UNDEFINED)
                },
            )?;
            Ok(JsValue::from_object(req_h))
        });
    }

    // createIndex(name, keyPath, options?)
    {
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, "createIndex", move |vm, this_val, args| {
            let Some(sh) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if args.len() < 2 {
                return Err(vm.type_error("IDBObjectStore.createIndex requires 2 arguments"));
            }
            let idx_name = vm.to_js_string(arg(args, 0))?;
            let Some((key_path, key_path_is_array)) = parse_key_path_arg(vm, arg(args, 1), false)?
            else {
                return Err(create_dom_exception(
                    vm,
                    "SyntaxError",
                    "Index keyPath is required",
                ));
            };
            let (unique, multi_entry) = if let Some(opts) = arg(args, 2).as_object() {
                let uk = vm.heap.intern_key("unique");
                let mk = vm.heap.intern_key("multiEntry");
                let u = vm
                    .get_property_by_key(opts, uk)
                    .ok()
                    .is_some_and(|v| vm.to_boolean(v));
                let m = vm
                    .get_property_by_key(opts, mk)
                    .ok()
                    .is_some_and(|v| vm.to_boolean(v));
                (u, m)
            } else {
                (false, false)
            };
            if multi_entry && (key_path_is_array || key_path.len() > 1) {
                return Err(create_dom_exception(
                    vm,
                    "InvalidAccessError",
                    "multiEntry cannot be true for array keyPath",
                ));
            }
            let (tx_h, store_name, index_proto) = {
                let mut st = db_r.borrow_mut();
                let s = st
                    .stores
                    .get(&sh)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                let tx_h = s.tx_h;
                let store_name = s.name.clone();
                let tx = st.transactions.get_mut(&tx_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Transaction inactive")
                })?;
                if tx.mode != TxMode::VersionChange {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "createIndex() is only allowed inside a versionchange transaction",
                    ));
                }
                let store = tx.working.stores.get_mut(&store_name).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Store not found")
                })?;
                if store.indexes.contains_key(&idx_name) {
                    return Err(create_dom_exception(
                        vm,
                        "ConstraintError",
                        &format!("Index '{idx_name}' already exists"),
                    ));
                }
                store.indexes.insert(
                    idx_name.clone(),
                    IdbIndexSchema {
                        name: idx_name.clone(),
                        key_path,
                        key_path_is_array,
                        unique,
                        multi_entry,
                    },
                );
                (
                    tx_h,
                    store_name,
                    st.protos.index_proto.unwrap_or(vm.object_proto),
                )
            };
            let idx_h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(index_proto)));
            db_r.borrow_mut().indexes.insert(
                idx_h,
                IdbIndexHandle {
                    tx_h,
                    store_h: sh,
                    store_name,
                    index_name: idx_name,
                    cached_key_path: None,
                },
            );
            Ok(JsValue::from_object(idx_h))
        });
    }

    // deleteIndex(name)
    {
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, "deleteIndex", move |vm, this_val, args| {
            let Some(sh) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if args.is_empty() {
                return Err(vm.type_error("IDBObjectStore.deleteIndex requires 1 argument"));
            }
            let idx_name = vm.to_js_string(arg(args, 0))?;
            let mut st = db_r.borrow_mut();
            let s = st
                .stores
                .get(&sh)
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            let tx_h = s.tx_h;
            let store_name = s.name.clone();
            let tx = st.transactions.get_mut(&tx_h).ok_or_else(|| {
                create_dom_exception(vm, "InvalidStateError", "Transaction inactive")
            })?;
            if tx.mode != TxMode::VersionChange {
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "deleteIndex() is only allowed inside a versionchange transaction",
                ));
            }
            let store = tx.working.stores.get_mut(&store_name).ok_or_else(|| {
                create_dom_exception(vm, "InvalidStateError", "Object store has been deleted")
            })?;
            if store.indexes.remove(&idx_name).is_none() {
                return Err(create_dom_exception(
                    vm,
                    "NotFoundError",
                    &format!("Index '{idx_name}' not found"),
                ));
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // index(name)
    {
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, "index", move |vm, this_val, args| {
            let Some(sh) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if args.is_empty() {
                return Err(vm.type_error("IDBObjectStore.index requires 1 argument"));
            }
            let idx_name = vm.to_js_string(arg(args, 0))?;
            let (tx_h, store_name, index_proto) = {
                let st = db_r.borrow();
                let s = st
                    .stores
                    .get(&sh)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                let tx = st.transactions.get(&s.tx_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Transaction inactive")
                })?;
                if tx.finished {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Transaction has already finished",
                    ));
                }
                let store = tx.working.stores.get(&s.name).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Object store has been deleted")
                })?;
                if !store.indexes.contains_key(&idx_name) {
                    return Err(create_dom_exception(
                        vm,
                        "NotFoundError",
                        &format!("Index '{idx_name}' not found"),
                    ));
                }
                (
                    s.tx_h,
                    s.name.clone(),
                    st.protos.index_proto.unwrap_or(vm.object_proto),
                )
            };
            let idx_h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(index_proto)));
            db_r.borrow_mut().indexes.insert(
                idx_h,
                IdbIndexHandle {
                    tx_h,
                    store_h: sh,
                    store_name,
                    index_name: idx_name,
                    cached_key_path: None,
                },
            );
            Ok(JsValue::from_object(idx_h))
        });
    }

    // openCursor / openKeyCursor on IDBObjectStore
    for (method_name, key_only) in [("openCursor", false), ("openKeyCursor", true)] {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, method_name, move |vm, this_val, args| {
            let Some(sh) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let (tx_h, store_name) = {
                let st = db_r.borrow();
                let s = st
                    .stores
                    .get(&sh)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                let deleted = st
                    .transactions
                    .get(&s.tx_h)
                    .and_then(|tx| tx.working.stores.get(&s.name))
                    .is_none();
                if deleted {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Object store has been deleted",
                    ));
                }
                (s.tx_h, s.name.clone())
            };
            let range = query_to_key_range(vm, &db_r, arg(args, 0))?;
            let dir = parse_cursor_direction(vm, arg(args, 1))?;
            let req_h = open_cursor_common(
                vm, &h_r, &ev_r, &el_r, &wa_r, &db_r, tx_h, sh, store_name, None, range, dir,
                key_only,
            )?;
            Ok(JsValue::from_object(req_h))
        });
    }

    install_illegal_ctor(vm, "IDBObjectStore", proto);
}

#[allow(clippy::too_many_arguments)]
fn install_idb_transaction_proto(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    idb: &SharedIndexedDb,
) {
    let event_target_proto = host.borrow().prototypes.event_target_proto;
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(event_target_proto),
    ));
    define_to_string_tag(vm, proto, "IDBTransaction");
    idb.borrow_mut().protos.transaction_proto = Some(proto);

    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "db", move |vm, this_val, _| {
            this_val
                .as_object()
                .and_then(|h| {
                    db.borrow()
                        .transactions
                        .get(&h)
                        .map(|t| JsValue::from_object(t.db_h))
                })
                .ok_or_else(|| vm.type_error("Illegal invocation"))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "mode", move |vm, this_val, _| {
            let Some(m) = this_val
                .as_object()
                .and_then(|h| db.borrow().transactions.get(&h).map(|t| t.mode.as_str()))
            else {
                return Err(vm.type_error("Illegal invocation"));
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(m)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "durability", move |vm, this_val, _| {
            let Some(d) = this_val
                .as_object()
                .and_then(|h| db.borrow().transactions.get(&h).map(|t| t.durability))
            else {
                return Err(vm.type_error("Illegal invocation"));
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(d)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "error", move |vm, this_val, _| {
            this_val
                .as_object()
                .and_then(|h| db.borrow().transactions.get(&h).map(|t| t.error))
                .ok_or_else(|| vm.type_error("Illegal invocation"))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "objectStoreNames", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let (names, dsl_proto) = {
                let st = db.borrow();
                let Some(t) = st.transactions.get(&h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                (
                    t.scope.clone(),
                    st.protos.dom_string_list_proto.unwrap_or(vm.object_proto),
                )
            };
            Ok(JsValue::from_object(alloc_dom_string_list(
                vm, dsl_proto, names,
            )))
        });
    }

    for attr in ["onabort", "oncomplete", "onerror"] {
        let db = Rc::clone(idb);
        install_object_event_handler_accessor(vm, proto, events, attr, move |vm, this_v| {
            this_v
                .as_object()
                .filter(|h| db.borrow().transactions.contains_key(h))
                .ok_or_else(|| vm.type_error("Illegal invocation"))
        });
    }

    // tx.objectStore(name)
    {
        let db = Rc::clone(idb);
        define_host_method(vm, proto, "objectStore", move |vm, this_val, args| {
            let Some(tx_h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if args.is_empty() {
                return Err(vm.type_error("IDBTransaction.objectStore requires 1 argument"));
            }
            let name = vm.to_js_string(arg(args, 0))?;
            let store_proto = {
                let st = db.borrow();
                let tx = st
                    .transactions
                    .get(&tx_h)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                if tx.finished {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Transaction has already finished",
                    ));
                }
                let in_scope =
                    tx.mode == TxMode::VersionChange || tx.scope.iter().any(|s| s == &name);
                if !in_scope || !tx.working.stores.contains_key(&name) {
                    return Err(create_dom_exception(
                        vm,
                        "NotFoundError",
                        &format!("Object store '{name}' not in transaction scope"),
                    ));
                }
                st.protos.object_store_proto.unwrap_or(vm.object_proto)
            };
            let store_h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(store_proto)));
            db.borrow_mut().stores.insert(
                store_h,
                IdbStoreHandle {
                    tx_h,
                    name,
                    cached_key_path: None,
                },
            );
            Ok(JsValue::from_object(store_h))
        });
    }

    // tx.abort()
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db = Rc::clone(idb);
        define_host_method(vm, proto, "abort", move |vm, this_val, _| {
            let Some(tx_h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            {
                let mut st = db.borrow_mut();
                let Some(tx) = st.transactions.get_mut(&tx_h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if tx.finished {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Cannot abort a finished transaction",
                    ));
                }
                tx.aborted = true;
                tx.error = JsValue::NULL;
            }
            schedule_tx_commit_check(&h_r, &ev_r, &el_r, &wa_r, &db, tx_h);
            Ok(JsValue::UNDEFINED)
        });
    }

    // tx.commit()
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db = Rc::clone(idb);
        define_host_method(vm, proto, "commit", move |vm, this_val, _| {
            let Some(tx_h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            {
                let st = db.borrow();
                let Some(tx) = st.transactions.get(&tx_h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if tx.finished || tx.aborted {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Cannot commit a finished or aborted transaction",
                    ));
                }
            }
            schedule_tx_commit_check(&h_r, &ev_r, &el_r, &wa_r, &db, tx_h);
            Ok(JsValue::UNDEFINED)
        });
    }

    install_illegal_ctor(vm, "IDBTransaction", proto);
}

#[allow(clippy::too_many_arguments)]
fn install_idb_database_proto(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    idb: &SharedIndexedDb,
) {
    let event_target_proto = host.borrow().prototypes.event_target_proto;
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(event_target_proto),
    ));
    define_to_string_tag(vm, proto, "IDBDatabase");
    idb.borrow_mut().protos.database_proto = Some(proto);

    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "name", move |vm, this_val, _| {
            let Some(n) = this_val
                .as_object()
                .and_then(|h| db.borrow().databases.get(&h).map(|d| d.name.clone()))
            else {
                return Err(vm.type_error("Illegal invocation"));
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(&n)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "version", move |vm, this_val, _| {
            let Some(v) = this_val
                .as_object()
                .and_then(|h| db.borrow().databases.get(&h).map(|d| d.version))
            else {
                return Err(vm.type_error("Illegal invocation"));
            };
            Ok(JsValue::from_number(v as f64))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "objectStoreNames", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let (names, dsl_proto) = {
                let st = db.borrow();
                let Some(d) = st.databases.get(&h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                let n = if let Some(vc_tx) = d.active_versionchange_tx {
                    if let Some(tx) = st.transactions.get(&vc_tx) {
                        tx.working.stores.keys().cloned().collect::<Vec<_>>()
                    } else {
                        d.store_names.clone()
                    }
                } else {
                    d.store_names.clone()
                };
                (
                    n,
                    st.protos.dom_string_list_proto.unwrap_or(vm.object_proto),
                )
            };
            Ok(JsValue::from_object(alloc_dom_string_list(
                vm, dsl_proto, names,
            )))
        });
    }

    for attr in ["onabort", "onclose", "onerror", "onversionchange"] {
        let db = Rc::clone(idb);
        install_object_event_handler_accessor(vm, proto, events, attr, move |vm, this_v| {
            this_v
                .as_object()
                .filter(|h| db.borrow().databases.contains_key(h))
                .ok_or_else(|| vm.type_error("Illegal invocation"))
        });
    }

    // db.createObjectStore(name, options?)
    {
        let db = Rc::clone(idb);
        define_host_method(vm, proto, "createObjectStore", move |vm, this_val, args| {
            let Some(db_h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if args.is_empty() {
                return Err(vm.type_error("IDBDatabase.createObjectStore requires 1 argument"));
            }
            let name = vm.to_js_string(arg(args, 0))?;
            let (key_path_pair, auto_increment) = if let Some(opts) = arg(args, 1).as_object() {
                let kk = vm.heap.intern_key("keyPath");
                let ak = vm.heap.intern_key("autoIncrement");
                let ai = vm
                    .get_property_by_key(opts, ak)
                    .ok()
                    .is_some_and(|v| vm.to_boolean(v));
                let kp_val = vm.get_property_by_key(opts, kk)?;
                let kp = parse_key_path_arg(vm, kp_val, true)?;
                (kp, ai)
            } else {
                (None, false)
            };
            let (key_path, key_path_is_array) = match key_path_pair {
                Some((v, is_arr)) => (Some(v), is_arr),
                None => (None, false),
            };
            if auto_increment
                && (key_path_is_array
                    || key_path
                        .as_ref()
                        .is_some_and(|kp| kp.len() > 1 || kp[0].is_empty()))
            {
                return Err(create_dom_exception(
                    vm,
                    "InvalidAccessError",
                    "autoIncrement cannot be combined with empty or compound keyPath",
                ));
            }

            let (tx_h, store_proto) = {
                let mut st = db.borrow_mut();
                if !st.databases.contains_key(&db_h) {
                    return Err(vm.type_error("Illegal invocation"));
                }
                let vc_tx = st
                    .databases
                    .get(&db_h)
                    .and_then(|d| d.active_versionchange_tx)
                    .ok_or_else(|| {
                        create_dom_exception(
                            vm,
                            "InvalidStateError",
                            "createObjectStore() is only allowed during a versionchange transaction",
                        )
                    })?;
                let tx = st.transactions.get_mut(&vc_tx).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Transaction inactive")
                })?;
                if tx.working.stores.contains_key(&name) {
                    return Err(create_dom_exception(
                        vm,
                        "ConstraintError",
                        &format!("Object store '{name}' already exists"),
                    ));
                }
                let mut snap_store = IdbStoreSnapshot::new(name.clone(), key_path, auto_increment);
                snap_store.key_path_is_array = key_path_is_array;
                tx.working.stores.insert(name.clone(), snap_store);
                if !tx.scope.contains(&name) {
                    tx.scope.push(name.clone());
                }
                (
                    vc_tx,
                    st.protos.object_store_proto.unwrap_or(vm.object_proto),
                )
            };
            let store_h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(store_proto)));
            db.borrow_mut().stores.insert(
                store_h,
                IdbStoreHandle {
                    tx_h,
                    name,
                    cached_key_path: None,
                },
            );
            Ok(JsValue::from_object(store_h))
        });
    }

    // db.deleteObjectStore(name)
    {
        let db = Rc::clone(idb);
        define_host_method(vm, proto, "deleteObjectStore", move |vm, this_val, args| {
            let Some(db_h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if args.is_empty() {
                return Err(vm.type_error("IDBDatabase.deleteObjectStore requires 1 argument"));
            }
            let name = vm.to_js_string(arg(args, 0))?;
            let mut st = db.borrow_mut();
            if !st.databases.contains_key(&db_h) {
                return Err(vm.type_error("Illegal invocation"));
            }
            let vc_tx = st
                .databases
                .get(&db_h)
                .and_then(|d| d.active_versionchange_tx)
                .ok_or_else(|| {
                    create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "deleteObjectStore() is only allowed during a versionchange transaction",
                    )
                })?;
            let tx = st.transactions.get_mut(&vc_tx).ok_or_else(|| {
                create_dom_exception(vm, "InvalidStateError", "Transaction inactive")
            })?;
            if tx.working.stores.remove(&name).is_none() {
                return Err(create_dom_exception(
                    vm,
                    "NotFoundError",
                    &format!("Object store '{name}' does not exist"),
                ));
            }
            tx.scope.retain(|s| s != &name);
            Ok(JsValue::UNDEFINED)
        });
    }

    // db.transaction(storeNames, mode?, options?)
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db = Rc::clone(idb);
        define_host_method(vm, proto, "transaction", move |vm, this_val, args| {
            let Some(db_h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if args.is_empty() {
                return Err(vm.type_error("IDBDatabase.transaction requires at least 1 argument"));
            }
            let names_arg = arg(args, 0);
            let scope: Vec<String> = if names_arg.is_string() {
                vec![vm.value_to_rust_string(names_arg)]
            } else if let Some(ah) = names_arg.as_object() {
                let sym_iter = vm.key_symbol_iterator;
                let has_iter = vm
                    .get_property_by_key(ah, sym_iter)?
                    .as_object()
                    .is_some_and(|f| vm.is_callable_handle(f));
                if has_iter {
                    let raw = vm.collect_iterable(names_arg)?;
                    let mut v = Vec::with_capacity(raw.len());
                    for item in raw {
                        let s = vm.to_js_string(item)?;
                        if !v.contains(&s) {
                            v.push(s);
                        }
                    }
                    v
                } else {
                    vec![vm.to_js_string(names_arg)?]
                }
            } else {
                vec![vm.to_js_string(names_arg)?]
            };
            let mode = if arg(args, 1).is_undefined() {
                TxMode::ReadOnly
            } else {
                let ms = vm.to_js_string(arg(args, 1))?;
                match ms.as_str() {
                    "readonly" => TxMode::ReadOnly,
                    "readwrite" => TxMode::ReadWrite,
                    other => {
                        return Err(vm.type_error(format!("Invalid transaction mode '{other}'")));
                    }
                }
            };
            let durability = if let Some(opts) = arg(args, 2).as_object() {
                let kd = vm.heap.intern_key("durability");
                let dv = vm.get_property_by_key(opts, kd)?;
                if dv.is_undefined() {
                    "default"
                } else {
                    match vm.to_js_string(dv)?.as_str() {
                        "default" => "default",
                        "strict" => "strict",
                        "relaxed" => "relaxed",
                        other => {
                            return Err(
                                vm.type_error(format!("Invalid transaction durability '{other}'"))
                            );
                        }
                    }
                }
            } else {
                "default"
            };

            let (origin, db_name, tx_proto) = {
                let st = db.borrow();
                let d = st
                    .databases
                    .get(&db_h)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                if d.active_versionchange_tx.is_some() {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "A versionchange transaction is running",
                    ));
                }
                if d.closed {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Database connection is closing",
                    ));
                }
                (
                    d.origin.clone(),
                    d.name.clone(),
                    st.protos.transaction_proto.unwrap_or(vm.object_proto),
                )
            };

            if scope.is_empty() {
                return Err(create_dom_exception(
                    vm,
                    "InvalidAccessError",
                    "Transaction storeNames must not be empty",
                ));
            }

            let storage = wa_r.borrow().storage.clone();
            let working = storage
                .idb_load_database(&origin, &db_name)
                .map_err(|e| create_dom_exception(vm, "UnknownError", &e.to_string()))?
                .unwrap_or_else(|| IdbDatabaseSnapshot::new(db_name.clone(), 1));

            for sname in &scope {
                if !working.stores.contains_key(sname) {
                    return Err(create_dom_exception(
                        vm,
                        "NotFoundError",
                        &format!("Object store '{sname}' does not exist"),
                    ));
                }
            }

            let tx_h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(tx_proto)));
            ev_r.borrow_mut().set_object_event_parent(tx_h, Some(db_h));
            let ver = working.version;
            db.borrow_mut().transactions.insert(
                tx_h,
                IdbTransactionState {
                    db_h,
                    origin,
                    mode,
                    durability,
                    scope,
                    working,
                    old_version: ver,
                    finished: false,
                    aborted: false,
                    pending_requests: 0,
                    commit_scheduled: false,
                    error: JsValue::NULL,
                    versionchange_open_req: None,
                },
            );
            schedule_tx_commit_check(&h_r, &ev_r, &el_r, &wa_r, &db, tx_h);
            Ok(JsValue::from_object(tx_h))
        });
    }

    // db.close()
    {
        let db = Rc::clone(idb);
        define_host_method(vm, proto, "close", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if let Some(d) = db.borrow_mut().databases.get_mut(&h) {
                d.closed = true;
                Ok(JsValue::UNDEFINED)
            } else {
                Err(vm.type_error("Illegal invocation"))
            }
        });
    }

    install_illegal_ctor(vm, "IDBDatabase", proto);
}

#[allow(clippy::too_many_arguments)]
fn install_idb_factory(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    idb: &SharedIndexedDb,
) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    define_to_string_tag(vm, proto, "IDBFactory");
    idb.borrow_mut().protos.factory_proto = Some(proto);

    // indexedDB.open(name, version?)
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, "open", move |vm, _, args| {
            if args.is_empty() {
                return Err(vm.type_error("IDBFactory.open requires at least 1 argument"));
            }
            let origin_check = idb_effective_origin(&wa_r, &db_r);
            if origin_check == "null" {
                return Err(create_dom_exception(
                    vm,
                    "SecurityError",
                    "IndexedDB access is denied in an opaque origin",
                ));
            }
            let db_name = vm.to_js_string(arg(args, 0))?;
            let requested_version = if arg(args, 1).is_undefined() {
                None
            } else {
                Some(enforce_range_idb_version(vm, arg(args, 1))?)
            };

            let req_h = alloc_request(vm, &ev_r, &db_r, true, JsValue::NULL, JsValue::NULL);
            let h = Rc::clone(&h_r);
            let ev = Rc::clone(&ev_r);
            let el = Rc::clone(&el_r);
            let wa = Rc::clone(&wa_r);
            let db_sys = Rc::clone(&db_r);

            el_r.borrow_mut()
                .post_host_task(TaskPriority::Networking, move |vm| {
                    let origin = idb_effective_origin(&wa, &db_sys);
                    let storage = wa.borrow().storage.clone();
                    let ev_proto = ev.borrow().event_proto.unwrap_or(vm.object_proto);
                    let existing = match storage.idb_load_database(&origin, &db_name) {
                        Ok(opt) => opt,
                        Err(e) => {
                            let err = create_dom_exception(vm, "UnknownError", &e.to_string()).value;
                            if let Some(r) = db_sys.borrow_mut().requests.get_mut(&req_h) {
                                r.ready_state = "done";
                                r.error = err;
                            }
                            let _ = fire_idb_event(
                                vm, &h, &ev, &el, req_h, "error", true, true, ev_proto, &[],
                            );
                            return;
                        }
                    };

                    let old_version = existing.as_ref().map_or(0, |d| d.version);
                    let target_version = requested_version.unwrap_or_else(|| old_version.max(1));

                    if old_version > 0 && target_version < old_version {
                        let err = create_dom_exception(
                            vm,
                            "VersionError",
                            &format!(
                                "Requested version ({target_version}) is less than existing version ({old_version})"
                            ),
                        )
                        .value;
                        if let Some(r) = db_sys.borrow_mut().requests.get_mut(&req_h) {
                            r.ready_state = "done";
                            r.error = err;
                        }
                        let _ = fire_idb_event(
                            vm, &h, &ev, &el, req_h, "error", true, true, ev_proto, &[],
                        );
                        return;
                    }

                    let mut snap = existing
                        .unwrap_or_else(|| IdbDatabaseSnapshot::new(db_name.clone(), target_version));
                    let needs_upgrade = old_version == 0 || target_version > old_version;
                    snap.version = target_version;

                    let (db_proto, tx_proto, vc_proto) = {
                        let st = db_sys.borrow();
                        (
                            st.protos.database_proto.unwrap_or(vm.object_proto),
                            st.protos.transaction_proto.unwrap_or(vm.object_proto),
                            st.protos.version_change_event_proto.unwrap_or(ev_proto),
                        )
                    };
                    let db_h = vm
                        .heap
                        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(db_proto)));
                    let store_names: Vec<String> = snap.stores.keys().cloned().collect();

                    if !needs_upgrade {
                        db_sys.borrow_mut().databases.insert(
                            db_h,
                            IdbDatabaseState {
                                origin,
                                name: db_name,
                                version: snap.version,
                                store_names,
                                closed: false,
                                active_versionchange_tx: None,
                            },
                        );
                        if let Some(r) = db_sys.borrow_mut().requests.get_mut(&req_h) {
                            r.ready_state = "done";
                            r.result = JsValue::from_object(db_h);
                            r.error = JsValue::NULL;
                        }
                        let _ = fire_idb_event(
                            vm, &h, &ev, &el, req_h, "success", false, false, ev_proto, &[],
                        );
                        return;
                    }

                    // Upgrade needed: create `versionchange` transaction and fire `upgradeneeded`!
                    let tx_h = vm
                        .heap
                        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(tx_proto)));
                    ev.borrow_mut().set_object_event_parent(tx_h, Some(db_h));
                    ev.borrow_mut().set_object_event_parent(req_h, Some(tx_h));
                    {
                        let mut st = db_sys.borrow_mut();
                        st.databases.insert(
                            db_h,
                            IdbDatabaseState {
                                origin: origin.clone(),
                                name: db_name,
                                version: target_version,
                                store_names: store_names.clone(),
                                closed: false,
                                active_versionchange_tx: Some(tx_h),
                            },
                        );
                        st.transactions.insert(
                            tx_h,
                            IdbTransactionState {
                                db_h,
                                origin,
                                mode: TxMode::VersionChange,
                                durability: "default",
                                scope: store_names,
                                working: snap,
                                old_version,
                                finished: false,
                                aborted: false,
                                pending_requests: 0,
                                commit_scheduled: false,
                                error: JsValue::NULL,
                                versionchange_open_req: Some(req_h),
                            },
                        );
                        if let Some(r) = st.requests.get_mut(&req_h) {
                            r.ready_state = "done";
                            r.result = JsValue::from_object(db_h);
                            r.transaction = JsValue::from_object(tx_h);
                        }
                    }

                    let (_, threw) = fire_idb_event(
                        vm,
                        &h,
                        &ev,
                        &el,
                        req_h,
                        "upgradeneeded",
                        false,
                        false,
                        vc_proto,
                        &[
                            ("__rix_idb_old_ver__", JsValue::from_number(old_version as f64)),
                            ("__rix_idb_new_ver__", JsValue::from_number(target_version as f64)),
                        ],
                    );
                    if threw {
                        abort_tx_with_error(
                            vm,
                            &db_sys,
                            tx_h,
                            "AbortError",
                            "Uncaught exception in upgradeneeded event handler",
                        );
                    }
                    schedule_tx_commit_check(&h, &ev, &el, &wa, &db_sys, tx_h);
                });

            Ok(JsValue::from_object(req_h))
        });
    }

    // indexedDB.deleteDatabase(name)
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, "deleteDatabase", move |vm, _, args| {
            if args.is_empty() {
                return Err(vm.type_error("IDBFactory.deleteDatabase requires 1 argument"));
            }
            if idb_effective_origin(&wa_r, &db_r) == "null" {
                return Err(create_dom_exception(
                    vm,
                    "SecurityError",
                    "IndexedDB access is denied in an opaque origin",
                ));
            }
            let db_name = vm.to_js_string(arg(args, 0))?;
            let req_h = alloc_request(vm, &ev_r, &db_r, true, JsValue::NULL, JsValue::NULL);
            let h = Rc::clone(&h_r);
            let ev = Rc::clone(&ev_r);
            let el = Rc::clone(&el_r);
            let wa = Rc::clone(&wa_r);
            let db_sys = Rc::clone(&db_r);
            el_r.borrow_mut()
                .post_host_task(TaskPriority::Networking, move |vm| {
                    let origin = idb_effective_origin(&wa, &db_sys);
                    let storage = wa.borrow().storage.clone();
                    let old_ver = storage
                        .idb_load_database(&origin, &db_name)
                        .ok()
                        .flatten()
                        .map_or(0, |d| d.version);
                    let _ = storage.idb_delete_database(&origin, &db_name);
                    if let Some(r) = db_sys.borrow_mut().requests.get_mut(&req_h) {
                        r.ready_state = "done";
                        r.result = JsValue::UNDEFINED;
                        r.error = JsValue::NULL;
                    }
                    let ev_proto = ev.borrow().event_proto.unwrap_or(vm.object_proto);
                    let vc_proto = db_sys
                        .borrow()
                        .protos
                        .version_change_event_proto
                        .unwrap_or(ev_proto);
                    let _ = fire_idb_event(
                        vm,
                        &h,
                        &ev,
                        &el,
                        req_h,
                        "success",
                        false,
                        false,
                        vc_proto,
                        &[
                            ("__rix_idb_old_ver__", JsValue::from_number(old_ver as f64)),
                            ("__rix_idb_new_ver__", JsValue::NULL),
                        ],
                    );
                });
            Ok(JsValue::from_object(req_h))
        });
    }

    // indexedDB.databases() -> Promise<Array<{name, version}>>
    {
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, "databases", move |vm, _, _| {
            let p = alloc_promise(vm);
            let origin = idb_effective_origin(&wa_r, &db_r);
            if origin == "null" {
                let err = create_dom_exception(
                    vm,
                    "SecurityError",
                    "IndexedDB access is denied in an opaque origin",
                )
                .value;
                reject_promise(vm, p, err);
                return Ok(JsValue::from_object(p));
            }
            let storage = wa_r.borrow().storage.clone();
            let list = storage.idb_list_databases(&origin).unwrap_or_default();
            let mut items = Vec::with_capacity(list.len());
            for (name, version) in list {
                let obj = vm.heap.alloc_object(JsObject::with_kind(
                    ObjectKind::Ordinary,
                    Some(vm.object_proto),
                ));
                let nk = vm.heap.intern_key("name");
                let nv = JsValue::from_string(vm.heap.alloc_string(&name));
                vm.heap
                    .object_define_data(obj, nk, nv, PropertyAttrs::DEFAULT, &mut vm.shapes);
                let vk = vm.heap.intern_key("version");
                vm.heap.object_define_data(
                    obj,
                    vk,
                    JsValue::from_number(version as f64),
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
                items.push(JsValue::from_object(obj));
            }
            let arr = alloc_js_array(vm, &items);
            resolve_promise(vm, p, JsValue::from_object(arr));
            Ok(JsValue::from_object(p))
        });
    }

    // indexedDB.cmp(first, second) -> -1 | 0 | 1
    {
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, "cmp", move |vm, _, args| {
            if args.len() < 2 {
                return Err(vm.type_error("IDBFactory.cmp requires 2 arguments"));
            }
            let frames = idb_frames(&db_r);
            let mut seen = HashSet::new();
            let a = js_to_idb_key(vm, frames.as_ref(), arg(args, 0), &mut seen)?;
            seen.clear();
            let b = js_to_idb_key(vm, frames.as_ref(), arg(args, 1), &mut seen)?;
            let res = match a.idb_cmp(&b) {
                Ordering::Less => -1.0,
                Ordering::Equal => 0.0,
                Ordering::Greater => 1.0,
            };
            Ok(JsValue::from_number(res))
        });
    }

    let factory_obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    define_global(vm, "indexedDB", JsValue::from_object(factory_obj));

    install_illegal_ctor(vm, "IDBFactory", proto);
}
