//! Full async transactional W3C IndexedDB (`IDBFactory`, `IDBOpenDBRequest`,
//! `IDBRequest`, `IDBDatabase`, `IDBTransaction`, `IDBObjectStore`, `IDBIndex`,
//! `IDBCursor`, `IDBKeyRange`) persisted atomically per origin in `rix-storage` (`redb`).
//!
//! Reference: Spec 06 §3, Issue #24.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use rix_js::promise::{alloc_promise, resolve_promise};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_storage::{IdbDatabaseSnapshot, IdbIndexSchema, IdbStoreSnapshot};

use crate::dom_bridge::SharedHost;
use crate::event_loop::{SharedEventLoop, TaskPriority};
use crate::events::{
    fire_cancelable_object_event, fire_object_event, fire_object_event_with_proto, SharedEvents,
};
use crate::frames::{decode_structured_json, encode_structured_json};
use crate::indexeddb_engine::{
    alloc_dom_string_list, alloc_js_array, collect_index_entries, collect_store_entries,
    idb_key_to_js, js_to_idb_key, parse_key_path_arg, store_put_or_add, CursorDirection,
    IdbCursorEntry, KeyRange,
};
use crate::web_apis::{
    arg, bind_ctor_proto, create_dom_exception, define_global, define_host_getter,
    define_host_method, SharedWebApis,
};

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
    let mut seen = HashSet::new();
    let key = js_to_idb_key(vm, val, &mut seen)?;
    Ok(Some(KeyRange::only(key)))
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
    scope: Vec<String>,
    working: IdbDatabaseSnapshot,
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
}

struct IdbIndexHandle {
    tx_h: HeapHandle,
    store_h: HeapHandle,
    store_name: String,
    index_name: String,
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
    key_range_proto: Option<HeapHandle>,
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
            self.protos.key_range_proto,
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
                    let working = tx.working.clone();
                    let open_req = tx.versionchange_open_req.take();
                    let err_val = tx.error;
                    if let Some(db) = st.databases.get_mut(&db_h) {
                        if db.active_versionchange_tx == Some(tx_h) {
                            db.active_versionchange_tx = None;
                        }
                        if !aborted && mode == TxMode::VersionChange {
                            db.version = working.version;
                            db.store_names = working.stores.keys().cloned().collect();
                        }
                    }
                    Some((db_h, aborted, mode, origin, working, open_req, err_val))
                }
            };

            let Some((db_h, aborted, mode, origin, working, open_req, err_val)) = outcome else {
                schedule_tx_commit_check(&h, &ev, &el, &wa, &db_sys, tx_h);
                return;
            };

            if aborted {
                // Transaction rolled back: ZERO writes to redb!
                fire_object_event(vm, &h, &ev, &el, tx_h, "abort", &[]);
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
                    fire_object_event(vm, &h, &ev, &el, req_h, "error", &[]);
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
                    fire_object_event(vm, &h, &ev, &el, tx_h, "abort", &[]);
                    return;
                }
            }

            fire_object_event(vm, &h, &ev, &el, tx_h, "complete", &[]);

            if let Some(req_h) = open_req {
                if let Some(r) = db_sys.borrow_mut().requests.get_mut(&req_h) {
                    r.ready_state = "done";
                    r.result = JsValue::from_object(db_h);
                    r.error = JsValue::NULL;
                    r.transaction = JsValue::NULL;
                }
                fire_object_event(vm, &h, &ev, &el, req_h, "success", &[]);
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

            match op_res {
                Ok(result_val) => {
                    if let Some(r) = db_sys.borrow_mut().requests.get_mut(&req_h) {
                        r.ready_state = "done";
                        r.result = result_val;
                        r.error = JsValue::NULL;
                    }
                    fire_object_event(vm, &h, &ev, &el, req_h, "success", &[]);
                }
                Err((err_name, err_msg)) => {
                    let dom_err = create_dom_exception(vm, err_name, &err_msg).value;
                    if let Some(r) = db_sys.borrow_mut().requests.get_mut(&req_h) {
                        r.ready_state = "done";
                        r.result = JsValue::UNDEFINED;
                        r.error = dom_err;
                    }
                    // Fire cancelable `error` event on request; if `preventDefault()` was NOT
                    // called (and not already aborted), abort the transaction per W3C spec.
                    let base_proto = ev.borrow().event_proto.unwrap_or(vm.object_proto);
                    let prevented =
                        fire_cancelable_object_event(vm, &h, &ev, &el, req_h, "error", base_proto);
                    if !aborted_early && !prevented {
                        let tx_prevented = fire_cancelable_object_event(
                            vm, &h, &ev, &el, tx_h, "error", base_proto,
                        );
                        if !tx_prevented {
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
/// and `IDBKeyRange` onto `vm`, backed by `rix-storage` (`redb`).
pub fn install_indexeddb(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
) -> SharedIndexedDb {
    let idb = Rc::new(RefCell::new(IndexedDbSystem::new()));
    let event_target_proto = host.borrow().prototypes.event_target_proto;

    install_idb_key_range(vm, &idb);
    install_idb_request_protos(vm, event_target_proto, &idb);
    install_idb_cursor_proto(vm, host, events, event_loop, web_apis, &idb);
    install_idb_index_proto(vm, host, events, event_loop, web_apis, &idb);
    install_idb_object_store_proto(vm, host, events, event_loop, web_apis, &idb);
    install_idb_transaction_proto(vm, host, events, event_loop, web_apis, &idb);
    install_idb_database_proto(vm, host, events, event_loop, web_apis, &idb);
    install_idb_factory(vm, host, events, event_loop, web_apis, &idb);

    idb
}

fn install_idb_key_range(vm: &mut JsVm, idb: &SharedIndexedDb) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
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
                return Ok(JsValue::UNDEFINED);
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
                return Ok(JsValue::from_bool(false));
            };
            let mut seen = HashSet::new();
            let k = js_to_idb_key(vm, arg(args, 0), &mut seen)?;
            Ok(JsValue::from_bool(kr.matches(&k)))
        });
    }

    let ctor = vm.alloc_host_fn("IDBKeyRange", |vm, _, _| {
        Err(create_dom_exception(vm, "TypeError", "Illegal constructor"))
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
            let mut seen = HashSet::new();
            let k = js_to_idb_key(vm, arg(args, 0), &mut seen)?;
            Ok(alloc_kr(vm, &db, KeyRange::only(k)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_method(vm, ctor, "lowerBound", move |vm, _, args| {
            let mut seen = HashSet::new();
            let lo = js_to_idb_key(vm, arg(args, 0), &mut seen)?;
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
            let mut seen = HashSet::new();
            let hi = js_to_idb_key(vm, arg(args, 0), &mut seen)?;
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
            let mut seen = HashSet::new();
            let lo = js_to_idb_key(vm, arg(args, 0), &mut seen)?;
            seen.clear();
            let hi = js_to_idb_key(vm, arg(args, 1), &mut seen)?;
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
    idb: &SharedIndexedDb,
) {
    let req_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(event_target_proto),
    ));
    let open_proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(req_proto)));
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
                None => Ok(JsValue::UNDEFINED),
            }
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, req_proto, "error", move |_, this_val, _| {
            let v = this_val
                .as_object()
                .and_then(|h| db.borrow().requests.get(&h).map(|r| r.error))
                .unwrap_or(JsValue::NULL);
            Ok(v)
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, req_proto, "readyState", move |vm, this_val, _| {
            let s = this_val
                .as_object()
                .and_then(|h| db.borrow().requests.get(&h).map(|r| r.ready_state))
                .unwrap_or("done");
            Ok(JsValue::from_string(vm.heap.alloc_string(s)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, req_proto, "source", move |_, this_val, _| {
            let v = this_val
                .as_object()
                .and_then(|h| db.borrow().requests.get(&h).map(|r| r.source))
                .unwrap_or(JsValue::NULL);
            Ok(v)
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, req_proto, "transaction", move |_, this_val, _| {
            let v = this_val
                .as_object()
                .and_then(|h| db.borrow().requests.get(&h).map(|r| r.transaction))
                .unwrap_or(JsValue::NULL);
            Ok(v)
        });
    }

    let req_ctor = vm.alloc_host_fn("IDBRequest", |vm, _, _| {
        Err(create_dom_exception(vm, "TypeError", "Illegal constructor"))
    });
    bind_ctor_proto(vm, req_ctor, req_proto);
    define_global(vm, "IDBRequest", JsValue::from_object(req_ctor));

    let open_ctor = vm.alloc_host_fn("IDBOpenDBRequest", |vm, _, _| {
        Err(create_dom_exception(vm, "TypeError", "Illegal constructor"))
    });
    bind_ctor_proto(vm, open_ctor, open_proto);
    define_global(vm, "IDBOpenDBRequest", JsValue::from_object(open_ctor));
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
    idb.borrow_mut().protos.cursor_proto = Some(proto);

    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "source", move |_, this_val, _| {
            Ok(this_val
                .as_object()
                .and_then(|h| {
                    db.borrow()
                        .cursors
                        .get(&h)
                        .map(|c| JsValue::from_object(c.source_h))
                })
                .unwrap_or(JsValue::NULL))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "direction", move |vm, this_val, _| {
            let s = this_val
                .as_object()
                .and_then(|h| db.borrow().cursors.get(&h).map(|c| c.direction.as_str()))
                .unwrap_or("next");
            Ok(JsValue::from_string(vm.heap.alloc_string(s)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "request", move |_, this_val, _| {
            Ok(this_val
                .as_object()
                .and_then(|h| {
                    db.borrow()
                        .cursors
                        .get(&h)
                        .map(|c| JsValue::from_object(c.req_h))
                })
                .unwrap_or(JsValue::NULL))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "key", move |vm, this_val, _| {
            let key = this_val.as_object().and_then(|h| {
                let st = db.borrow();
                let c = st.cursors.get(&h)?;
                c.entries.get(c.index).map(|e| e.key.clone())
            });
            Ok(key
                .as_ref()
                .map_or(JsValue::UNDEFINED, |k| idb_key_to_js(vm, k)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "primaryKey", move |vm, this_val, _| {
            let pk = this_val.as_object().and_then(|h| {
                let st = db.borrow();
                let c = st.cursors.get(&h)?;
                c.entries.get(c.index).map(|e| e.primary_key.clone())
            });
            Ok(pk
                .as_ref()
                .map_or(JsValue::UNDEFINED, |k| idb_key_to_js(vm, k)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "value", move |vm, this_val, _| {
            let val_json = this_val.as_object().and_then(|h| {
                let st = db.borrow();
                let c = st.cursors.get(&h)?;
                if c.key_only {
                    return None;
                }
                c.entries.get(c.index).map(|e| e.value_json.clone())
            });
            let Some(json_str) = val_json else {
                return Ok(JsValue::UNDEFINED);
            };
            Ok(serde_json::from_str::<serde_json::Value>(&json_str)
                .map(|jv| decode_structured_json(vm, &jv))
                .unwrap_or(JsValue::UNDEFINED))
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
                return Ok(JsValue::UNDEFINED);
            };
            let target_key = if arg(args, 0).is_undefined() {
                None
            } else {
                let mut seen = HashSet::new();
                Some(js_to_idb_key(vm, arg(args, 0), &mut seen)?)
            };
            let (tx_h, req_h) = {
                let mut st = db_r.borrow_mut();
                let Some(c) = st.cursors.get_mut(&ch) else {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Invalid cursor",
                    ));
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

    // cursor.advance(count)
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, "advance", move |vm, this_val, args| {
            let Some(ch) = this_val.as_object() else {
                return Ok(JsValue::UNDEFINED);
            };
            let count = arg(args, 0).as_number().unwrap_or(0.0);
            if count < 1.0 || !count.is_finite() {
                return Err(create_dom_exception(
                    vm,
                    "TypeError",
                    "IDBCursor.advance count must be >= 1",
                ));
            }
            let step = count as usize;
            let (tx_h, req_h) = {
                let mut st = db_r.borrow_mut();
                let Some(c) = st.cursors.get_mut(&ch) else {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Invalid cursor",
                    ));
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
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Invalid cursor",
                ));
            };
            let info = {
                let st = db_r.borrow();
                let c = st.cursors.get(&ch).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Invalid cursor")
                })?;
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
            let mut seen = HashSet::new();
            let val_json = encode_structured_json(vm, arg(args, 0), &mut seen)?;
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
                    let explicit = if store.key_path.is_some() {
                        None
                    } else {
                        Some(pk)
                    };
                    let key = store_put_or_add(store, val_json, explicit, false)?;
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
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Invalid cursor",
                ));
            };
            let info = {
                let st = db_r.borrow();
                let c = st.cursors.get(&ch).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Invalid cursor")
                })?;
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

    let ctor = vm.alloc_host_fn("IDBCursor", |vm, _, _| {
        Err(create_dom_exception(vm, "TypeError", "Illegal constructor"))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "IDBCursor", JsValue::from_object(ctor));
    define_global(vm, "IDBCursorWithValue", JsValue::from_object(ctor));
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
                fire_object_event(vm, &h, &ev, &el, req_h, "success", &[]);
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
    idb.borrow_mut().protos.index_proto = Some(proto);

    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "name", move |vm, this_val, _| {
            let s = this_val
                .as_object()
                .and_then(|h| db.borrow().indexes.get(&h).map(|i| i.index_name.clone()))
                .unwrap_or_default();
            Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "objectStore", move |_, this_val, _| {
            Ok(this_val
                .as_object()
                .and_then(|h| {
                    db.borrow()
                        .indexes
                        .get(&h)
                        .map(|i| JsValue::from_object(i.store_h))
                })
                .unwrap_or(JsValue::NULL))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "keyPath", move |vm, this_val, _| {
            let kp = this_val.as_object().and_then(|h| {
                let st = db.borrow();
                let idx = st.indexes.get(&h)?;
                let tx = st.transactions.get(&idx.tx_h)?;
                let store = tx.working.stores.get(&idx.store_name)?;
                store
                    .indexes
                    .get(&idx.index_name)
                    .map(|s| s.key_path.clone())
            });
            Ok(match kp {
                Some(ref v) if v.len() == 1 => JsValue::from_string(vm.heap.alloc_string(&v[0])),
                Some(v) => {
                    let elems: Vec<JsValue> = v
                        .iter()
                        .map(|s| JsValue::from_string(vm.heap.alloc_string(s)))
                        .collect();
                    JsValue::from_object(alloc_js_array(vm, &elems))
                }
                None => JsValue::NULL,
            })
        });
    }
    for (prop, is_unique) in [("unique", true), ("multiEntry", false)] {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, prop, move |_, this_val, _| {
            let b = this_val
                .as_object()
                .and_then(|h| {
                    let st = db.borrow();
                    let idx = st.indexes.get(&h)?;
                    let tx = st.transactions.get(&idx.tx_h)?;
                    let store = tx.working.stores.get(&idx.store_name)?;
                    store.indexes.get(&idx.index_name).map(|s| {
                        if is_unique {
                            s.unique
                        } else {
                            s.multi_entry
                        }
                    })
                })
                .unwrap_or(false);
            Ok(JsValue::from_bool(b))
        });
    }

    // get / getKey / getAll / getAllKeys / count on IDBIndex
    for (method_name, mode_tag) in [
        ("get", 0u8),
        ("getKey", 1),
        ("getAll", 2),
        ("getAllKeys", 3),
        ("count", 4),
    ] {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, method_name, move |vm, this_val, args| {
            let Some(ih) = this_val.as_object() else {
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Invalid index",
                ));
            };
            let (tx_h, store_name, index_name) = {
                let st = db_r.borrow();
                let idx = st.indexes.get(&ih).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Invalid index")
                })?;
                (idx.tx_h, idx.store_name.clone(), idx.index_name.clone())
            };
            let range = query_to_key_range(vm, &db_r, arg(args, 0))?;
            let count_limit = arg(args, 1)
                .as_number()
                .filter(|n| n.is_finite() && *n > 0.0)
                .map(|n| n as usize);
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
                    let entries = collect_index_entries(
                        store,
                        idx_schema,
                        range.as_ref(),
                        CursorDirection::Next,
                    );
                    match mode_tag {
                        0 => Ok(entries.first().map_or(JsValue::UNDEFINED, |e| {
                            serde_json::from_str::<serde_json::Value>(&e.value_json)
                                .map(|jv| decode_structured_json(vm, &jv))
                                .unwrap_or(JsValue::UNDEFINED)
                        })),
                        1 => Ok(entries
                            .first()
                            .map_or(JsValue::UNDEFINED, |e| idb_key_to_js(vm, &e.primary_key))),
                        2 => {
                            let limit = count_limit.unwrap_or(entries.len());
                            let vals: Vec<JsValue> = entries
                                .iter()
                                .take(limit)
                                .map(|e| {
                                    serde_json::from_str::<serde_json::Value>(&e.value_json)
                                        .map(|jv| decode_structured_json(vm, &jv))
                                        .unwrap_or(JsValue::UNDEFINED)
                                })
                                .collect();
                            Ok(JsValue::from_object(alloc_js_array(vm, &vals)))
                        }
                        3 => {
                            let limit = count_limit.unwrap_or(entries.len());
                            let vals: Vec<JsValue> = entries
                                .iter()
                                .take(limit)
                                .map(|e| idb_key_to_js(vm, &e.primary_key))
                                .collect();
                            Ok(JsValue::from_object(alloc_js_array(vm, &vals)))
                        }
                        _ => Ok(JsValue::from_number(entries.len() as f64)),
                    }
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
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Invalid index",
                ));
            };
            let (tx_h, store_name, index_name) = {
                let st = db_r.borrow();
                let idx = st.indexes.get(&ih).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Invalid index")
                })?;
                (idx.tx_h, idx.store_name.clone(), idx.index_name.clone())
            };
            let range = query_to_key_range(vm, &db_r, arg(args, 0))?;
            let dir = if arg(args, 1).is_undefined() {
                CursorDirection::Next
            } else {
                let s = vm.value_to_rust_string(arg(args, 1));
                CursorDirection::parse(&s).ok_or_else(|| {
                    create_dom_exception(vm, "TypeError", "Invalid cursor direction")
                })?
            };
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

    let ctor = vm.alloc_host_fn("IDBIndex", |vm, _, _| {
        Err(create_dom_exception(vm, "TypeError", "Illegal constructor"))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "IDBIndex", JsValue::from_object(ctor));
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
        idb,
        false,
        JsValue::from_object(source_h),
        JsValue::from_object(tx_h),
    );
    let cursor_proto = idb.borrow().protos.cursor_proto.unwrap_or(vm.object_proto);
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
    idb.borrow_mut().protos.object_store_proto = Some(proto);

    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "name", move |vm, this_val, _| {
            let s = this_val
                .as_object()
                .and_then(|h| db.borrow().stores.get(&h).map(|s| s.name.clone()))
                .unwrap_or_default();
            Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "transaction", move |_, this_val, _| {
            Ok(this_val
                .as_object()
                .and_then(|h| {
                    db.borrow()
                        .stores
                        .get(&h)
                        .map(|s| JsValue::from_object(s.tx_h))
                })
                .unwrap_or(JsValue::NULL))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "keyPath", move |vm, this_val, _| {
            let kp = this_val.as_object().and_then(|h| {
                let st = db.borrow();
                let sh = st.stores.get(&h)?;
                let tx = st.transactions.get(&sh.tx_h)?;
                tx.working.stores.get(&sh.name)?.key_path.clone()
            });
            Ok(match kp {
                Some(ref v) if v.len() == 1 => JsValue::from_string(vm.heap.alloc_string(&v[0])),
                Some(v) => {
                    let elems: Vec<JsValue> = v
                        .iter()
                        .map(|s| JsValue::from_string(vm.heap.alloc_string(s)))
                        .collect();
                    JsValue::from_object(alloc_js_array(vm, &elems))
                }
                None => JsValue::NULL,
            })
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "autoIncrement", move |_, this_val, _| {
            let ai = this_val
                .as_object()
                .and_then(|h| {
                    let st = db.borrow();
                    let sh = st.stores.get(&h)?;
                    let tx = st.transactions.get(&sh.tx_h)?;
                    tx.working.stores.get(&sh.name).map(|s| s.auto_increment)
                })
                .unwrap_or(false);
            Ok(JsValue::from_bool(ai))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "indexNames", move |vm, this_val, _| {
            let names = this_val
                .as_object()
                .and_then(|h| {
                    let st = db.borrow();
                    let sh = st.stores.get(&h)?;
                    let tx = st.transactions.get(&sh.tx_h)?;
                    tx.working
                        .stores
                        .get(&sh.name)
                        .map(|s| s.indexes.keys().cloned().collect::<Vec<_>>())
                })
                .unwrap_or_default();
            Ok(JsValue::from_object(alloc_dom_string_list(vm, names)))
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
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Invalid objectStore",
                ));
            };
            let (tx_h, store_name) = {
                let st = db_r.borrow();
                let s = st.stores.get(&sh).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Invalid objectStore")
                })?;
                let tx = st.transactions.get(&s.tx_h).ok_or_else(|| {
                    create_dom_exception(vm, "TransactionInactiveError", "Transaction inactive")
                })?;
                if tx.mode == TxMode::ReadOnly {
                    return Err(create_dom_exception(
                        vm,
                        "ReadOnlyError",
                        "Cannot mutate object store in a readonly transaction",
                    ));
                }
                (s.tx_h, s.name.clone())
            };
            let mut seen = HashSet::new();
            let val_json = encode_structured_json(vm, arg(args, 0), &mut seen)?;
            let explicit_key = if arg(args, 1).is_undefined() {
                None
            } else {
                let mut kseen = HashSet::new();
                Some(js_to_idb_key(vm, arg(args, 1), &mut kseen)?)
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
                    let key = store_put_or_add(store, val_json, explicit_key, no_overwrite)?;
                    Ok(idb_key_to_js(vm, &key))
                },
            )?;
            Ok(JsValue::from_object(req_h))
        });
    }

    // get / getKey / getAll / getAllKeys / count
    for (method_name, mode_tag) in [
        ("get", 0u8),
        ("getKey", 1),
        ("getAll", 2),
        ("getAllKeys", 3),
        ("count", 4),
    ] {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, method_name, move |vm, this_val, args| {
            let Some(sh) = this_val.as_object() else {
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Invalid objectStore",
                ));
            };
            let (tx_h, store_name) = {
                let st = db_r.borrow();
                let s = st.stores.get(&sh).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Invalid objectStore")
                })?;
                (s.tx_h, s.name.clone())
            };
            let range = query_to_key_range(vm, &db_r, arg(args, 0))?;
            let count_limit = arg(args, 1)
                .as_number()
                .filter(|n| n.is_finite() && *n > 0.0)
                .map(|n| n as usize);
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
                    let entries =
                        collect_store_entries(store, range.as_ref(), CursorDirection::Next);
                    match mode_tag {
                        0 => Ok(entries.first().map_or(JsValue::UNDEFINED, |e| {
                            serde_json::from_str::<serde_json::Value>(&e.value_json)
                                .map(|jv| decode_structured_json(vm, &jv))
                                .unwrap_or(JsValue::UNDEFINED)
                        })),
                        1 => Ok(entries
                            .first()
                            .map_or(JsValue::UNDEFINED, |e| idb_key_to_js(vm, &e.primary_key))),
                        2 => {
                            let limit = count_limit.unwrap_or(entries.len());
                            let vals: Vec<JsValue> = entries
                                .iter()
                                .take(limit)
                                .map(|e| {
                                    serde_json::from_str::<serde_json::Value>(&e.value_json)
                                        .map(|jv| decode_structured_json(vm, &jv))
                                        .unwrap_or(JsValue::UNDEFINED)
                                })
                                .collect();
                            Ok(JsValue::from_object(alloc_js_array(vm, &vals)))
                        }
                        3 => {
                            let limit = count_limit.unwrap_or(entries.len());
                            let vals: Vec<JsValue> = entries
                                .iter()
                                .take(limit)
                                .map(|e| idb_key_to_js(vm, &e.primary_key))
                                .collect();
                            Ok(JsValue::from_object(alloc_js_array(vm, &vals)))
                        }
                        _ => Ok(JsValue::from_number(entries.len() as f64)),
                    }
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
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Invalid objectStore",
                ));
            };
            let (tx_h, store_name) = {
                let st = db_r.borrow();
                let s = st.stores.get(&sh).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Invalid objectStore")
                })?;
                let tx = st.transactions.get(&s.tx_h).ok_or_else(|| {
                    create_dom_exception(vm, "TransactionInactiveError", "Transaction inactive")
                })?;
                if tx.mode == TxMode::ReadOnly {
                    return Err(create_dom_exception(
                        vm,
                        "ReadOnlyError",
                        "Cannot delete in a readonly transaction",
                    ));
                }
                (s.tx_h, s.name.clone())
            };
            let range = query_to_key_range(vm, &db_r, arg(args, 0))?;
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
                    if let Some(ref r) = range {
                        store.records.retain(|_, rec| !r.matches(&rec.key));
                    }
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
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Invalid objectStore",
                ));
            };
            let (tx_h, store_name) = {
                let st = db_r.borrow();
                let s = st.stores.get(&sh).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Invalid objectStore")
                })?;
                let tx = st.transactions.get(&s.tx_h).ok_or_else(|| {
                    create_dom_exception(vm, "TransactionInactiveError", "Transaction inactive")
                })?;
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
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Invalid objectStore",
                ));
            };
            let idx_name = vm.value_to_rust_string(arg(args, 0));
            let Some(key_path) = parse_key_path_arg(vm, arg(args, 1))? else {
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
            if multi_entry && key_path.len() > 1 {
                return Err(create_dom_exception(
                    vm,
                    "InvalidAccessError",
                    "multiEntry cannot be true for array keyPath",
                ));
            }
            let (tx_h, store_name, index_proto) = {
                let mut st = db_r.borrow_mut();
                let s = st.stores.get(&sh).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Invalid objectStore")
                })?;
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
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Invalid objectStore",
                ));
            };
            let idx_name = vm.value_to_rust_string(arg(args, 0));
            let mut st = db_r.borrow_mut();
            let s = st.stores.get(&sh).ok_or_else(|| {
                create_dom_exception(vm, "InvalidStateError", "Invalid objectStore")
            })?;
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
            let store = tx
                .working
                .stores
                .get_mut(&store_name)
                .ok_or_else(|| create_dom_exception(vm, "NotFoundError", "Store not found"))?;
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
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Invalid objectStore",
                ));
            };
            let idx_name = vm.value_to_rust_string(arg(args, 0));
            let (tx_h, store_name, index_proto) = {
                let st = db_r.borrow();
                let s = st.stores.get(&sh).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Invalid objectStore")
                })?;
                let tx = st.transactions.get(&s.tx_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Transaction inactive")
                })?;
                let store =
                    tx.working.stores.get(&s.name).ok_or_else(|| {
                        create_dom_exception(vm, "NotFoundError", "Store not found")
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
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Invalid objectStore",
                ));
            };
            let (tx_h, store_name) = {
                let st = db_r.borrow();
                let s = st.stores.get(&sh).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Invalid objectStore")
                })?;
                (s.tx_h, s.name.clone())
            };
            let range = query_to_key_range(vm, &db_r, arg(args, 0))?;
            let dir = if arg(args, 1).is_undefined() {
                CursorDirection::Next
            } else {
                let s = vm.value_to_rust_string(arg(args, 1));
                CursorDirection::parse(&s).ok_or_else(|| {
                    create_dom_exception(vm, "TypeError", "Invalid cursor direction")
                })?
            };
            let req_h = open_cursor_common(
                vm, &h_r, &ev_r, &el_r, &wa_r, &db_r, tx_h, sh, store_name, None, range, dir,
                key_only,
            )?;
            Ok(JsValue::from_object(req_h))
        });
    }

    let ctor = vm.alloc_host_fn("IDBObjectStore", |vm, _, _| {
        Err(create_dom_exception(vm, "TypeError", "Illegal constructor"))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "IDBObjectStore", JsValue::from_object(ctor));
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
    idb.borrow_mut().protos.transaction_proto = Some(proto);

    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "db", move |_, this_val, _| {
            Ok(this_val
                .as_object()
                .and_then(|h| {
                    db.borrow()
                        .transactions
                        .get(&h)
                        .map(|t| JsValue::from_object(t.db_h))
                })
                .unwrap_or(JsValue::NULL))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "mode", move |vm, this_val, _| {
            let m = this_val
                .as_object()
                .and_then(|h| db.borrow().transactions.get(&h).map(|t| t.mode.as_str()))
                .unwrap_or("readonly");
            Ok(JsValue::from_string(vm.heap.alloc_string(m)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "error", move |_, this_val, _| {
            Ok(this_val
                .as_object()
                .and_then(|h| db.borrow().transactions.get(&h).map(|t| t.error))
                .unwrap_or(JsValue::NULL))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "objectStoreNames", move |vm, this_val, _| {
            let names = this_val
                .as_object()
                .and_then(|h| db.borrow().transactions.get(&h).map(|t| t.scope.clone()))
                .unwrap_or_default();
            Ok(JsValue::from_object(alloc_dom_string_list(vm, names)))
        });
    }

    // tx.objectStore(name)
    {
        let db = Rc::clone(idb);
        define_host_method(vm, proto, "objectStore", move |vm, this_val, args| {
            let Some(tx_h) = this_val.as_object() else {
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Invalid transaction",
                ));
            };
            let name = vm.value_to_rust_string(arg(args, 0));
            let store_proto = {
                let st = db.borrow();
                let tx = st.transactions.get(&tx_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Invalid transaction")
                })?;
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
            db.borrow_mut()
                .stores
                .insert(store_h, IdbStoreHandle { tx_h, name });
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
                return Ok(JsValue::UNDEFINED);
            };
            let abort_err = create_dom_exception(vm, "AbortError", "Transaction aborted").value;
            {
                let mut st = db.borrow_mut();
                let Some(tx) = st.transactions.get_mut(&tx_h) else {
                    return Ok(JsValue::UNDEFINED);
                };
                if tx.finished {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Cannot abort a finished transaction",
                    ));
                }
                tx.aborted = true;
                tx.error = abort_err;
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
        define_host_method(vm, proto, "commit", move |_, this_val, _| {
            if let Some(tx_h) = this_val.as_object() {
                schedule_tx_commit_check(&h_r, &ev_r, &el_r, &wa_r, &db, tx_h);
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    let ctor = vm.alloc_host_fn("IDBTransaction", |vm, _, _| {
        Err(create_dom_exception(vm, "TypeError", "Illegal constructor"))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "IDBTransaction", JsValue::from_object(ctor));
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
    idb.borrow_mut().protos.database_proto = Some(proto);

    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "name", move |vm, this_val, _| {
            let n = this_val
                .as_object()
                .and_then(|h| db.borrow().databases.get(&h).map(|d| d.name.clone()))
                .unwrap_or_default();
            Ok(JsValue::from_string(vm.heap.alloc_string(&n)))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "version", move |_, this_val, _| {
            let v = this_val
                .as_object()
                .and_then(|h| db.borrow().databases.get(&h).map(|d| d.version))
                .unwrap_or(0);
            Ok(JsValue::from_number(v as f64))
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "objectStoreNames", move |vm, this_val, _| {
            let names = this_val
                .as_object()
                .and_then(|h| {
                    let st = db.borrow();
                    let d = st.databases.get(&h)?;
                    if let Some(vc_tx) = d.active_versionchange_tx {
                        if let Some(tx) = st.transactions.get(&vc_tx) {
                            return Some(tx.working.stores.keys().cloned().collect::<Vec<_>>());
                        }
                    }
                    Some(d.store_names.clone())
                })
                .unwrap_or_default();
            Ok(JsValue::from_object(alloc_dom_string_list(vm, names)))
        });
    }

    // db.createObjectStore(name, options?)
    {
        let db = Rc::clone(idb);
        define_host_method(vm, proto, "createObjectStore", move |vm, this_val, args| {
            let Some(db_h) = this_val.as_object() else {
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Invalid database",
                ));
            };
            let name = vm.value_to_rust_string(arg(args, 0));
            let (key_path, auto_increment) = if let Some(opts) = arg(args, 1).as_object() {
                let kk = vm.heap.intern_key("keyPath");
                let ak = vm.heap.intern_key("autoIncrement");
                let kp_val = vm
                    .get_property_by_key(opts, kk)
                    .unwrap_or(JsValue::UNDEFINED);
                let kp = parse_key_path_arg(vm, kp_val)?;
                let ai = vm
                    .get_property_by_key(opts, ak)
                    .ok()
                    .is_some_and(|v| vm.to_boolean(v));
                (kp, ai)
            } else {
                (None, false)
            };
            if auto_increment
                && key_path
                    .as_ref()
                    .is_some_and(|kp| kp.len() > 1 || kp[0].is_empty())
            {
                return Err(create_dom_exception(
                    vm,
                    "InvalidAccessError",
                    "autoIncrement cannot be combined with empty or compound keyPath",
                ));
            }

            let (tx_h, store_proto) = {
                let mut st = db.borrow_mut();
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
                tx.working.stores.insert(
                    name.clone(),
                    IdbStoreSnapshot::new(name.clone(), key_path, auto_increment),
                );
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
            db.borrow_mut()
                .stores
                .insert(store_h, IdbStoreHandle { tx_h, name });
            Ok(JsValue::from_object(store_h))
        });
    }

    // db.deleteObjectStore(name)
    {
        let db = Rc::clone(idb);
        define_host_method(vm, proto, "deleteObjectStore", move |vm, this_val, args| {
            let Some(db_h) = this_val.as_object() else {
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Invalid database",
                ));
            };
            let name = vm.value_to_rust_string(arg(args, 0));
            let mut st = db.borrow_mut();
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

    // db.transaction(storeNames, mode?)
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db = Rc::clone(idb);
        define_host_method(vm, proto, "transaction", move |vm, this_val, args| {
            let Some(db_h) = this_val.as_object() else {
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Invalid database",
                ));
            };
            let names_arg = arg(args, 0);
            let scope: Vec<String> = if names_arg.is_string() {
                vec![vm.value_to_rust_string(names_arg)]
            } else if let Some(ah) = names_arg.as_object() {
                let len = vm
                    .heap
                    .get_object(ah)
                    .map_or(0, |o| o.elements.len() as usize);
                let mut v = Vec::with_capacity(len);
                for i in 0..len {
                    let elem = u32::try_from(i)
                        .ok()
                        .and_then(|idx| vm.heap.get_object(ah).and_then(|o| o.elements.get(idx)))
                        .unwrap_or(JsValue::UNDEFINED);
                    let s = vm.value_to_rust_string(elem);
                    if !v.contains(&s) {
                        v.push(s);
                    }
                }
                v
            } else {
                vec![vm.value_to_rust_string(names_arg)]
            };
            if scope.is_empty() {
                return Err(create_dom_exception(
                    vm,
                    "InvalidAccessError",
                    "Transaction storeNames must not be empty",
                ));
            }
            let mode = if arg(args, 1).is_undefined() {
                TxMode::ReadOnly
            } else {
                match vm.value_to_rust_string(arg(args, 1)).as_str() {
                    "readonly" => TxMode::ReadOnly,
                    "readwrite" => TxMode::ReadWrite,
                    other => {
                        return Err(create_dom_exception(
                            vm,
                            "TypeError",
                            &format!("Invalid transaction mode '{other}'"),
                        ));
                    }
                }
            };

            let (origin, db_name, tx_proto) = {
                let st = db.borrow();
                let d = st.databases.get(&db_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Invalid database")
                })?;
                if d.closed {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Database connection is closing",
                    ));
                }
                if d.active_versionchange_tx.is_some() {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "A versionchange transaction is running",
                    ));
                }
                (
                    d.origin.clone(),
                    d.name.clone(),
                    st.protos.transaction_proto.unwrap_or(vm.object_proto),
                )
            };

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
            db.borrow_mut().transactions.insert(
                tx_h,
                IdbTransactionState {
                    db_h,
                    origin,
                    mode,
                    scope,
                    working,
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
        define_host_method(vm, proto, "close", move |_, this_val, _| {
            if let Some(h) = this_val.as_object() {
                if let Some(d) = db.borrow_mut().databases.get_mut(&h) {
                    d.closed = true;
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    let ctor = vm.alloc_host_fn("IDBDatabase", |vm, _, _| {
        Err(create_dom_exception(vm, "TypeError", "Illegal constructor"))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "IDBDatabase", JsValue::from_object(ctor));
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
    idb.borrow_mut().protos.factory_proto = Some(proto);

    // indexedDB.open(name, version?)
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, "open", move |vm, _, args| {
            let db_name = vm.value_to_rust_string(arg(args, 0));
            let requested_version = if arg(args, 1).is_undefined() {
                None
            } else {
                let n = arg(args, 1).as_number().unwrap_or(0.0);
                if !n.is_finite() || n < 1.0 {
                    return Err(create_dom_exception(
                        vm,
                        "TypeError",
                        "IndexedDB version must be a positive integer",
                    ));
                }
                Some(n as u64)
            };

            let req_h = alloc_request(vm, &db_r, true, JsValue::NULL, JsValue::NULL);
            let h = Rc::clone(&h_r);
            let ev = Rc::clone(&ev_r);
            let el = Rc::clone(&el_r);
            let wa = Rc::clone(&wa_r);
            let db_sys = Rc::clone(&db_r);

            el_r.borrow_mut()
                .post_host_task(TaskPriority::Networking, move |vm| {
                    let (origin, storage) = {
                        let s = wa.borrow();
                        (s.current_origin(), s.storage.clone())
                    };
                    let existing = match storage.idb_load_database(&origin, &db_name) {
                        Ok(opt) => opt,
                        Err(e) => {
                            let err = create_dom_exception(vm, "UnknownError", &e.to_string()).value;
                            if let Some(r) = db_sys.borrow_mut().requests.get_mut(&req_h) {
                                r.ready_state = "done";
                                r.error = err;
                            }
                            fire_object_event(vm, &h, &ev, &el, req_h, "error", &[]);
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
                        fire_object_event(vm, &h, &ev, &el, req_h, "error", &[]);
                        return;
                    }

                    let mut snap = existing
                        .unwrap_or_else(|| IdbDatabaseSnapshot::new(db_name.clone(), target_version));
                    let needs_upgrade = old_version == 0 || target_version > old_version;
                    snap.version = target_version;

                    let (db_proto, tx_proto) = {
                        let st = db_sys.borrow();
                        (
                            st.protos.database_proto.unwrap_or(vm.object_proto),
                            st.protos.transaction_proto.unwrap_or(vm.object_proto),
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
                        fire_object_event(vm, &h, &ev, &el, req_h, "success", &[]);
                        return;
                    }

                    // Upgrade needed: create `versionchange` transaction and fire `upgradeneeded`!
                    let tx_h = vm
                        .heap
                        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(tx_proto)));
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
                                scope: store_names,
                                working: snap,
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

                    let base_proto = ev.borrow().event_proto.unwrap_or(vm.object_proto);
                    fire_object_event_with_proto(
                        vm,
                        &h,
                        &ev,
                        &el,
                        req_h,
                        "upgradeneeded",
                        base_proto,
                        &[
                            ("oldVersion", JsValue::from_number(old_version as f64)),
                            ("newVersion", JsValue::from_number(target_version as f64)),
                        ],
                    );
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
            let db_name = vm.value_to_rust_string(arg(args, 0));
            let req_h = alloc_request(vm, &db_r, true, JsValue::NULL, JsValue::NULL);
            let h = Rc::clone(&h_r);
            let ev = Rc::clone(&ev_r);
            let el = Rc::clone(&el_r);
            let wa = Rc::clone(&wa_r);
            let db_sys = Rc::clone(&db_r);
            el_r.borrow_mut()
                .post_host_task(TaskPriority::Networking, move |vm| {
                    let (origin, storage) = {
                        let s = wa.borrow();
                        (s.current_origin(), s.storage.clone())
                    };
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
                    let base_proto = ev.borrow().event_proto.unwrap_or(vm.object_proto);
                    fire_object_event_with_proto(
                        vm,
                        &h,
                        &ev,
                        &el,
                        req_h,
                        "success",
                        base_proto,
                        &[
                            ("oldVersion", JsValue::from_number(old_ver as f64)),
                            ("newVersion", JsValue::NULL),
                        ],
                    );
                });
            Ok(JsValue::from_object(req_h))
        });
    }

    // indexedDB.databases() -> Promise<Array<{name, version}>>
    {
        let wa_r = Rc::clone(web_apis);
        define_host_method(vm, proto, "databases", move |vm, _, _| {
            let p = alloc_promise(vm);
            let (origin, storage) = {
                let s = wa_r.borrow();
                (s.current_origin(), s.storage.clone())
            };
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
    define_host_method(vm, proto, "cmp", |vm, _, args| {
        let mut seen = HashSet::new();
        let a = js_to_idb_key(vm, arg(args, 0), &mut seen)?;
        seen.clear();
        let b = js_to_idb_key(vm, arg(args, 1), &mut seen)?;
        let res = match a.idb_cmp(&b) {
            Ordering::Less => -1.0,
            Ordering::Equal => 0.0,
            Ordering::Greater => 1.0,
        };
        Ok(JsValue::from_number(res))
    });

    let factory_obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    define_global(vm, "indexedDB", JsValue::from_object(factory_obj));

    let ctor = vm.alloc_host_fn("IDBFactory", |vm, _, _| {
        Err(create_dom_exception(vm, "TypeError", "Illegal constructor"))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "IDBFactory", JsValue::from_object(ctor));
}
