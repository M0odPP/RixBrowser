//! IndexedDB runtime state tables, transaction lifecycle state machine,
//! metadata rollback, and multi-connection open/delete/transaction scheduler.
//!
//! Reference: W3C Indexed Database API 3.0 §2.7, §4, §5.

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;

use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind};
use rix_storage::{IdbDatabaseSnapshot, IdbStoreSnapshot};

use crate::canvas2d::SharedCanvas2D;
use crate::dom_bridge::SharedHost;
use crate::event_loop::{SharedEventLoop, TaskPriority};
use crate::events::{fire_idb_event, SharedEvents};
use crate::fetch_ws::SharedNetBridge;
use crate::frames::SharedFrames;
use crate::indexeddb_engine::{
    step_index_cursor, step_store_cursor, validate_index_unique_constraint, CursorDirection,
    CursorStepRequest, IdbCursorEntry, KeyRange,
};
use crate::web_apis::{create_dom_exception, SharedWebApis};
use crate::workers::SharedWorkers;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxMode {
    ReadOnly,
    ReadWrite,
    VersionChange,
}

impl TxMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "readonly",
            Self::ReadWrite => "readwrite",
            Self::VersionChange => "versionchange",
        }
    }
}

/// W3C IndexedDB 3.0 §2.7 transaction lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxState {
    Active,
    Inactive,
    Committing,
    Finished,
}

pub(crate) type TxRequestClosure =
    Box<dyn FnOnce(&mut JsVm, &mut IdbTransactionState) -> Result<JsValue, (&'static str, String)>>;

pub(crate) enum TxOp {
    Request {
        req_h: HeapHandle,
        op: TxRequestClosure,
    },
    CursorStep {
        req_h: HeapHandle,
        cursor_h: HeapHandle,
        step: CursorStepRequest,
    },
    ValidateUniqueIndex {
        store_id: u64,
        index_id: u64,
    },
}

pub(crate) struct PendingFactoryOp {
    pub(crate) is_delete: bool,
    pub(crate) req_h: HeapHandle,
    pub(crate) requested_version: Option<u64>,
    pub(crate) vc_sent: bool,
    pub(crate) blocked_sent: bool,
}

#[derive(Default)]
pub(crate) struct DbCoordination {
    pub(crate) open_connections: Vec<HeapHandle>,
    pub(crate) tx_queue: VecDeque<HeapHandle>,
    pub(crate) open_delete_queue: VecDeque<PendingFactoryOp>,
    pub(crate) active_factory_op: bool,
}

pub(crate) struct IdbRequestState {
    pub(crate) result: JsValue,
    pub(crate) error: JsValue,
    pub(crate) ready_state: &'static str,
    pub(crate) source: JsValue,
    pub(crate) transaction: JsValue,
}

pub(crate) struct IdbDatabaseState {
    pub(crate) origin: String,
    pub(crate) name: String,
    pub(crate) version: u64,
    pub(crate) store_names: Vec<String>,
    pub(crate) schema_stores: BTreeMap<String, IdbStoreSnapshot>,
    pub(crate) closed: bool,
    pub(crate) active_versionchange_tx: Option<HeapHandle>,
}

pub(crate) struct IdbTransactionState {
    pub(crate) db_h: HeapHandle,
    pub(crate) origin: String,
    pub(crate) db_name: String,
    pub(crate) mode: TxMode,
    pub(crate) durability: &'static str,
    pub(crate) scope: Vec<String>,
    pub(crate) working: IdbDatabaseSnapshot,
    pub(crate) old_version: u64,
    pub(crate) state: TxState,
    pub(crate) aborted: bool,
    pub(crate) aborting: bool,
    pub(crate) explicit_commit: bool,
    pub(crate) started: bool,
    pub(crate) in_flight_op: bool,
    pub(crate) commit_scheduled: bool,
    pub(crate) abort_scheduled: bool,
    pub(crate) dispatching_depth: usize,
    pub(crate) error: JsValue,
    pub(crate) versionchange_open_req: Option<HeapHandle>,
    pub(crate) queued_ops: VecDeque<TxOp>,
    pub(crate) store_id_by_name: HashMap<String, u64>,
    pub(crate) name_by_store_id: HashMap<u64, String>,
    pub(crate) store_handles: HashMap<u64, HeapHandle>,
    pub(crate) index_id_by_name: HashMap<(u64, String), u64>,
    pub(crate) name_by_index_id: HashMap<(u64, u64), String>,
    pub(crate) index_handles: HashMap<(u64, u64), HeapHandle>,
    pub(crate) initial_versionchange_snapshot: Option<IdbDatabaseSnapshot>,
    pub(crate) initial_store_ids: HashMap<String, u64>,
    pub(crate) initial_index_ids: HashMap<(u64, String), u64>,
    /// Stores deleted mid-upgrade kept alive by `store_id` ONLY for already-queued `TxOp`s.
    pub(crate) deleted_stores: HashMap<u64, IdbStoreSnapshot>,
}

impl IdbTransactionState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        db_h: HeapHandle,
        origin: String,
        db_name: String,
        mode: TxMode,
        durability: &'static str,
        scope: Vec<String>,
        working: IdbDatabaseSnapshot,
        old_version: u64,
        started: bool,
        versionchange_open_req: Option<HeapHandle>,
        initial_versionchange_snapshot: Option<IdbDatabaseSnapshot>,
    ) -> Self {
        Self {
            db_h,
            origin,
            db_name,
            mode,
            durability,
            scope,
            working,
            old_version,
            state: TxState::Active,
            aborted: false,
            aborting: false,
            explicit_commit: false,
            started,
            in_flight_op: false,
            commit_scheduled: false,
            abort_scheduled: false,
            dispatching_depth: 0,
            error: JsValue::NULL,
            versionchange_open_req,
            queued_ops: VecDeque::new(),
            store_id_by_name: HashMap::new(),
            name_by_store_id: HashMap::new(),
            store_handles: HashMap::new(),
            index_id_by_name: HashMap::new(),
            name_by_index_id: HashMap::new(),
            index_handles: HashMap::new(),
            initial_versionchange_snapshot,
            initial_store_ids: HashMap::new(),
            initial_index_ids: HashMap::new(),
            deleted_stores: HashMap::new(),
        }
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.state == TxState::Finished
    }

    pub(crate) fn init_meta_ids(&mut self, next_id: &mut u64) {
        self.store_id_by_name.clear();
        self.name_by_store_id.clear();
        self.index_id_by_name.clear();
        self.name_by_index_id.clear();
        for (sname, store) in &self.working.stores {
            let sid = *next_id;
            *next_id += 1;
            self.store_id_by_name.insert(sname.clone(), sid);
            self.name_by_store_id.insert(sid, sname.clone());
            for iname in store.indexes.keys() {
                let iid = *next_id;
                *next_id += 1;
                self.index_id_by_name.insert((sid, iname.clone()), iid);
                self.name_by_index_id.insert((sid, iid), iname.clone());
            }
        }
    }

    pub(crate) fn store_name(&self, store_id: u64) -> Option<&str> {
        self.name_by_store_id.get(&store_id).map(String::as_str)
    }

    pub(crate) fn index_name(&self, store_id: u64, index_id: u64) -> Option<&str> {
        self.name_by_index_id
            .get(&(store_id, index_id))
            .map(String::as_str)
    }

    pub(crate) fn store_snap(&self, store_id: u64) -> Option<&IdbStoreSnapshot> {
        if let Some(name) = self.store_name(store_id) {
            if let Some(s) = self.working.stores.get(name) {
                return Some(s);
            }
        }
        self.deleted_stores.get(&store_id)
    }

    pub(crate) fn store_snap_mut(&mut self, store_id: u64) -> Option<&mut IdbStoreSnapshot> {
        if let Some(name) = self.name_by_store_id.get(&store_id).cloned() {
            if self.working.stores.contains_key(&name) {
                return self.working.stores.get_mut(&name);
            }
        }
        self.deleted_stores.get_mut(&store_id)
    }
}

pub(crate) struct IdbStoreHandle {
    pub(crate) tx_h: HeapHandle,
    pub(crate) store_id: u64,
    pub(crate) name: String,
    pub(crate) original_name: Option<String>,
    pub(crate) deleted: bool,
    pub(crate) cached_key_path: Option<JsValue>,
    pub(crate) cached_auto_increment: bool,
}

pub(crate) struct IdbIndexHandle {
    pub(crate) tx_h: HeapHandle,
    pub(crate) store_h: HeapHandle,
    pub(crate) store_id: u64,
    pub(crate) index_id: u64,
    pub(crate) index_name: String,
    pub(crate) original_name: Option<String>,
    pub(crate) deleted: bool,
    pub(crate) cached_key_path: Option<JsValue>,
    pub(crate) cached_unique: bool,
    pub(crate) cached_multi_entry: bool,
}

pub(crate) struct IdbCursorState {
    pub(crate) tx_h: HeapHandle,
    pub(crate) req_h: HeapHandle,
    pub(crate) source_h: HeapHandle,
    pub(crate) store_id: u64,
    pub(crate) index_id: Option<u64>,
    pub(crate) range: Option<KeyRange>,
    pub(crate) direction: CursorDirection,
    pub(crate) key_only: bool,
    pub(crate) got_value: bool,
    pub(crate) current: Option<IdbCursorEntry>,
    pub(crate) cached_key_js: Option<JsValue>,
    pub(crate) cached_pk_js: Option<JsValue>,
    pub(crate) cached_val_js: Option<JsValue>,
}

#[derive(Default)]
pub(crate) struct IdbPrototypes {
    pub(crate) factory_proto: Option<HeapHandle>,
    pub(crate) request_proto: Option<HeapHandle>,
    pub(crate) open_request_proto: Option<HeapHandle>,
    pub(crate) database_proto: Option<HeapHandle>,
    pub(crate) transaction_proto: Option<HeapHandle>,
    pub(crate) object_store_proto: Option<HeapHandle>,
    pub(crate) index_proto: Option<HeapHandle>,
    pub(crate) cursor_proto: Option<HeapHandle>,
    pub(crate) cursor_with_value_proto: Option<HeapHandle>,
    pub(crate) key_range_proto: Option<HeapHandle>,
    pub(crate) version_change_event_proto: Option<HeapHandle>,
    pub(crate) dom_string_list_proto: Option<HeapHandle>,
    pub(crate) record_proto: Option<HeapHandle>,
}

#[derive(Clone)]
pub(crate) struct CloneDeps {
    pub(crate) net: SharedNetBridge,
    pub(crate) canvas2d: SharedCanvas2D,
    pub(crate) frames: SharedFrames,
    pub(crate) workers: SharedWorkers,
}

/// Content-runtime registry of active IndexedDB handles, transactions, and prototypes.
#[derive(Default)]
pub struct IdbRuntimeState {
    pub(crate) requests: HashMap<HeapHandle, IdbRequestState>,
    pub(crate) databases: HashMap<HeapHandle, IdbDatabaseState>,
    pub(crate) transactions: HashMap<HeapHandle, IdbTransactionState>,
    pub(crate) stores: HashMap<HeapHandle, IdbStoreHandle>,
    pub(crate) indexes: HashMap<HeapHandle, IdbIndexHandle>,
    pub(crate) cursors: HashMap<HeapHandle, IdbCursorState>,
    pub(crate) key_ranges: HashMap<HeapHandle, KeyRange>,
    pub(crate) coords: HashMap<(String, String), DbCoordination>,
    pub(crate) next_meta_id: u64,
    pub(crate) protos: IdbPrototypes,
    pub(crate) clone_deps: Option<CloneDeps>,
}

/// Shared handle to [`IdbRuntimeState`].
pub type SharedIdbRuntime = Rc<RefCell<IdbRuntimeState>>;

impl IdbRuntimeState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_meta_id: 1,
            ..Self::default()
        }
    }

    pub(crate) fn alloc_meta_id(&mut self) -> u64 {
        let id = self.next_meta_id.max(1);
        self.next_meta_id = id.wrapping_add(1).max(1);
        id
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
        for coord in self.coords.values() {
            for &th in &coord.tx_queue {
                out.push(JsValue::from_object(th));
            }
            for op in &coord.open_delete_queue {
                out.push(JsValue::from_object(op.req_h));
            }
        }
        for (&th, tx) in &self.transactions {
            if !tx.is_finished() {
                out.push(JsValue::from_object(th));
                out.push(JsValue::from_object(tx.db_h));
                out.push(tx.error);
                if let Some(req) = tx.versionchange_open_req {
                    out.push(JsValue::from_object(req));
                }
                for &sh in tx.store_handles.values() {
                    out.push(JsValue::from_object(sh));
                }
                for &ih in tx.index_handles.values() {
                    out.push(JsValue::from_object(ih));
                }
                for op in &tx.queued_ops {
                    match op {
                        TxOp::Request { req_h, .. } => out.push(JsValue::from_object(*req_h)),
                        TxOp::CursorStep {
                            req_h, cursor_h, ..
                        } => {
                            out.push(JsValue::from_object(*req_h));
                            out.push(JsValue::from_object(*cursor_h));
                        }
                        TxOp::ValidateUniqueIndex { .. } => {}
                    }
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
            let active = self
                .transactions
                .get(&c.tx_h)
                .is_some_and(|t| !t.is_finished())
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
            if let Some(v) = c.cached_key_js {
                out.push(v);
            }
            if let Some(v) = c.cached_pk_js {
                out.push(v);
            }
            if let Some(v) = c.cached_val_js {
                out.push(v);
            }
        }
    }

    /// Sweeps dead JS wrapper handles after Major GC.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.requests
            .retain(|h, r| r.ready_state == "pending" || heap.is_alive(*h));
        self.databases.retain(|h, _| heap.is_alive(*h));
        self.transactions
            .retain(|h, tx| !tx.is_finished() || heap.is_alive(*h));
        self.stores.retain(|h, _| heap.is_alive(*h));
        self.indexes.retain(|h, _| heap.is_alive(*h));
        self.cursors.retain(|h, _| heap.is_alive(*h));
        self.key_ranges.retain(|h, _| heap.is_alive(*h));
        for coord in self.coords.values_mut() {
            coord.open_connections.retain(|h| heap.is_alive(*h));
        }
    }
}

pub(crate) fn metadata_only_stores(
    stores: &BTreeMap<String, IdbStoreSnapshot>,
) -> BTreeMap<String, IdbStoreSnapshot> {
    let mut out = BTreeMap::new();
    for (name, s) in stores {
        let mut copy = IdbStoreSnapshot::new(s.name.clone(), s.key_path.clone(), s.auto_increment);
        copy.key_path_is_array = s.key_path_is_array;
        copy.next_key = s.next_key;
        copy.indexes = s.indexes.clone();
        out.insert(name.clone(), copy);
    }
    out
}

pub(crate) fn alloc_request(
    vm: &mut JsVm,
    events: &SharedEvents,
    idb: &SharedIdbRuntime,
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

/// Synchronously reverts all `VersionChange` schema mutations (store/index creations,
/// deletions, renames, and database version) per W3C IDB §5.5 step 1.
pub(crate) fn revert_versionchange_metadata(st: &mut IdbRuntimeState, tx_h: HeapHandle) {
    let Some(tx) = st.transactions.get_mut(&tx_h) else {
        return;
    };
    if tx.mode != TxMode::VersionChange {
        return;
    }
    let Some(orig_snap) = tx.initial_versionchange_snapshot.take() else {
        return;
    };
    let db_h = tx.db_h;
    let old_version = tx.old_version;
    tx.working = orig_snap;
    tx.scope = tx.working.stores.keys().cloned().collect();
    tx.store_id_by_name = tx.initial_store_ids.clone();
    tx.name_by_store_id = tx
        .initial_store_ids
        .iter()
        .map(|(n, &id)| (id, n.clone()))
        .collect();
    tx.index_id_by_name = tx.initial_index_ids.clone();
    tx.name_by_index_id = tx
        .initial_index_ids
        .iter()
        .map(|((sid, n), &iid)| ((*sid, iid), n.clone()))
        .collect();
    let reverted_names = tx.scope.clone();
    let reverted_schema = metadata_only_stores(&tx.working.stores);
    let store_handles: Vec<HeapHandle> = tx.store_handles.values().copied().collect();
    let index_handles: Vec<HeapHandle> = tx.index_handles.values().copied().collect();

    if let Some(db) = st.databases.get_mut(&db_h) {
        db.version = old_version;
        db.store_names = reverted_names;
        db.schema_stores = reverted_schema;
    }
    for sh in store_handles {
        if let Some(s) = st.stores.get_mut(&sh) {
            if let Some(orig) = s.original_name.clone() {
                s.name = orig;
                s.deleted = false;
            } else {
                s.deleted = true;
            }
        }
    }
    for ih in index_handles {
        if let Some(i) = st.indexes.get_mut(&ih) {
            if let Some(orig) = i.original_name.clone() {
                i.index_name = orig;
                i.deleted = false;
            } else {
                i.deleted = true;
            }
        }
    }
}

pub(crate) fn idb_err(vm: &mut JsVm, name: &str, msg: &str) -> JsException {
    create_dom_exception(vm, name, msg)
}

#[derive(Clone)]
pub(crate) struct IdbCtx {
    pub(crate) host: SharedHost,
    pub(crate) events: SharedEvents,
    pub(crate) event_loop: SharedEventLoop,
    pub(crate) web_apis: SharedWebApis,
    pub(crate) idb: SharedIdbRuntime,
}

impl IdbCtx {
    pub(crate) fn new(
        host: &SharedHost,
        events: &SharedEvents,
        event_loop: &SharedEventLoop,
        web_apis: &SharedWebApis,
        idb: &SharedIdbRuntime,
    ) -> Self {
        Self {
            host: Rc::clone(host),
            events: Rc::clone(events),
            event_loop: Rc::clone(event_loop),
            web_apis: Rc::clone(web_apis),
            idb: Rc::clone(idb),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn fire(
        &self,
        vm: &mut JsVm,
        target: HeapHandle,
        ev_type: &str,
        bubbles: bool,
        cancelable: bool,
        proto: HeapHandle,
        props: &[(&str, JsValue)],
    ) -> (bool, bool) {
        fire_idb_event(
            vm,
            &self.host,
            &self.events,
            &self.event_loop,
            target,
            ev_type,
            bubbles,
            cancelable,
            proto,
            props,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn fire_with_tx(
        &self,
        vm: &mut JsVm,
        tx_h: HeapHandle,
        target: HeapHandle,
        ev_type: &str,
        bubbles: bool,
        cancelable: bool,
        proto: HeapHandle,
        props: &[(&str, JsValue)],
    ) -> (bool, bool) {
        if let Some(tx) = self.idb.borrow_mut().transactions.get_mut(&tx_h) {
            if tx.state == TxState::Inactive && !tx.aborted && !tx.aborting {
                tx.state = TxState::Active;
            }
            tx.dispatching_depth += 1;
        }
        let res = self.fire(vm, target, ev_type, bubbles, cancelable, proto, props);
        if let Some(tx) = self.idb.borrow_mut().transactions.get_mut(&tx_h) {
            tx.dispatching_depth = tx.dispatching_depth.saturating_sub(1);
            if tx.dispatching_depth == 0 && tx.state == TxState::Active {
                tx.state = TxState::Inactive;
            }
        }
        res
    }

    pub(crate) fn abort_on_throw(&self, vm: &mut JsVm, tx_h: HeapHandle, msg: &str) {
        let ignore = self
            .idb
            .borrow()
            .transactions
            .get(&tx_h)
            .is_some_and(|tx| tx.explicit_commit || tx.state == TxState::Committing);
        if !ignore {
            let err = idb_err(vm, "AbortError", msg).value;
            begin_tx_abort_ctx(self, tx_h, err);
        }
    }

    pub(crate) fn set_req_done(
        &self,
        req_h: HeapHandle,
        result: JsValue,
        error: JsValue,
        clear_tx: bool,
    ) {
        if let Some(r) = self.idb.borrow_mut().requests.get_mut(&req_h) {
            r.ready_state = "done";
            r.result = result;
            r.error = error;
            if clear_tx {
                r.transaction = JsValue::NULL;
            }
        }
    }

    pub(crate) fn release_vc_and_pump(&self, mode: TxMode, origin: &str, db_name: &str) {
        if mode == TxMode::VersionChange {
            if let Some(coord) = self
                .idb
                .borrow_mut()
                .coords
                .get_mut(&(origin.to_string(), db_name.to_string()))
            {
                coord.active_factory_op = false;
            }
        }
        pump_db_tx_queue_ctx(self, origin, db_name);
        schedule_factory_pump_ctx(self, origin, db_name);
    }

    pub(crate) fn pop_factory_op(&self, key: &(String, String)) {
        if let Some(coord) = self.idb.borrow_mut().coords.get_mut(key) {
            coord.open_delete_queue.pop_front();
        }
    }
}

pub(crate) fn transactions_conflict(a: &IdbTransactionState, b: &IdbTransactionState) -> bool {
    if a.mode == TxMode::VersionChange || b.mode == TxMode::VersionChange {
        return true;
    }
    !(a.mode == TxMode::ReadOnly && b.mode == TxMode::ReadOnly)
        && a.scope.iter().any(|s| b.scope.contains(s))
}

pub(crate) fn begin_tx_abort_ctx(ctx: &IdbCtx, tx_h: HeapHandle, err_val: JsValue) {
    let should_schedule = {
        let mut st = ctx.idb.borrow_mut();
        if !st
            .transactions
            .get(&tx_h)
            .is_some_and(|tx| !tx.is_finished() && !tx.aborting)
        {
            false
        } else {
            if let Some(tx) = st.transactions.get_mut(&tx_h) {
                tx.aborted = true;
                tx.aborting = true;
                tx.state = TxState::Inactive;
                tx.error = err_val;
            }
            revert_versionchange_metadata(&mut st, tx_h);
            st.transactions.get_mut(&tx_h).is_some_and(|tx| {
                let first = !tx.abort_scheduled;
                tx.abort_scheduled = true;
                first
            })
        }
    };
    if should_schedule {
        schedule_tx_abort_drain_ctx(ctx, tx_h);
    }
}

pub(crate) fn schedule_tx_abort_drain_ctx(ctx: &IdbCtx, tx_h: HeapHandle) {
    let c = ctx.clone();
    ctx.event_loop
        .borrow_mut()
        .post_host_task(TaskPriority::Networking, move |vm| {
            let next_req = {
                let mut st = c.idb.borrow_mut();
                let Some(tx) = st.transactions.get_mut(&tx_h) else {
                    return;
                };
                if tx.is_finished() {
                    return;
                }
                let mut found = None;
                while let Some(op) = tx.queued_ops.pop_front() {
                    match op {
                        TxOp::Request { req_h, .. } | TxOp::CursorStep { req_h, .. } => {
                            found = Some(req_h);
                            break;
                        }
                        TxOp::ValidateUniqueIndex { .. } => {}
                    }
                }
                found
            };

            let ev_proto = c.events.borrow().event_proto.unwrap_or(vm.object_proto);
            if let Some(req_h) = next_req {
                let err = idb_err(vm, "AbortError", "Transaction was aborted").value;
                c.set_req_done(req_h, JsValue::UNDEFINED, err, false);
                let _ = c.fire(vm, req_h, "error", true, true, ev_proto, &[]);
                schedule_tx_abort_drain_ctx(&c, tx_h);
                return;
            }

            let finish_info = {
                let mut st = c.idb.borrow_mut();
                let Some(tx) = st.transactions.get_mut(&tx_h) else {
                    return;
                };
                if tx.is_finished() {
                    return;
                }
                tx.state = TxState::Finished;
                tx.abort_scheduled = false;
                tx.in_flight_op = false;
                let (db_h, origin, db_name, mode, open_req) = (
                    tx.db_h,
                    tx.origin.clone(),
                    tx.db_name.clone(),
                    tx.mode,
                    tx.versionchange_open_req.take(),
                );
                if let Some(db) = st.databases.get_mut(&db_h) {
                    if db.active_versionchange_tx == Some(tx_h) {
                        db.active_versionchange_tx = None;
                    }
                }
                if let Some(coord) = st.coords.get_mut(&(origin.clone(), db_name.clone())) {
                    coord.tx_queue.retain(|&th| th != tx_h);
                    if mode == TxMode::VersionChange {
                        coord.open_connections.retain(|&dh| dh != db_h);
                    }
                }
                Some((origin, db_name, mode, open_req))
            };
            let Some((origin, db_name, mode, open_req)) = finish_info else {
                return;
            };

            let _ = c.fire(vm, tx_h, "abort", true, false, ev_proto, &[]);
            if let Some(req_h) = open_req {
                let err = idb_err(vm, "AbortError", "Version change transaction aborted").value;
                c.set_req_done(req_h, JsValue::UNDEFINED, err, true);
                c.events.borrow_mut().set_object_event_parent(req_h, None);
                let _ = c.fire(vm, req_h, "error", true, true, ev_proto, &[]);
            }
            c.release_vc_and_pump(mode, &origin, &db_name);
        });
}

pub(crate) fn schedule_tx_commit_ctx(ctx: &IdbCtx, tx_h: HeapHandle) {
    let should_schedule = ctx
        .idb
        .borrow_mut()
        .transactions
        .get_mut(&tx_h)
        .is_some_and(|tx| {
            let ok = !tx.is_finished()
                && !tx.aborted
                && !tx.aborting
                && !tx.commit_scheduled
                && tx.started
                && !tx.in_flight_op
                && tx.queued_ops.is_empty();
            if ok {
                tx.state = TxState::Committing;
                tx.commit_scheduled = true;
            }
            ok
        });
    if !should_schedule {
        return;
    }
    let c = ctx.clone();
    ctx.event_loop
        .borrow_mut()
        .post_host_task(TaskPriority::Networking, move |vm| {
            let commit_info = {
                let mut st = c.idb.borrow_mut();
                let Some(tx) = st.transactions.get_mut(&tx_h) else {
                    return;
                };
                tx.commit_scheduled = false;
                if tx.is_finished()
                    || tx.aborted
                    || tx.aborting
                    || tx.in_flight_op
                    || !tx.queued_ops.is_empty()
                {
                    return;
                }
                tx.state = TxState::Finished;
                let (db_h, mode, origin, db_name, working, open_req) = (
                    tx.db_h,
                    tx.mode,
                    tx.origin.clone(),
                    tx.db_name.clone(),
                    tx.working.clone(),
                    tx.versionchange_open_req.take(),
                );
                let mut db_closed = false;
                if let Some(db) = st.databases.get_mut(&db_h) {
                    db_closed = db.closed;
                    if db.active_versionchange_tx == Some(tx_h) {
                        db.active_versionchange_tx = None;
                    }
                    if mode == TxMode::VersionChange {
                        db.version = working.version;
                        db.store_names = working.stores.keys().cloned().collect();
                        db.schema_stores = metadata_only_stores(&working.stores);
                    }
                }
                if let Some(coord) = st.coords.get_mut(&(origin.clone(), db_name.clone())) {
                    coord.tx_queue.retain(|&th| th != tx_h);
                }
                Some((db_h, db_closed, mode, origin, db_name, working, open_req))
            };
            let Some((db_h, db_closed, mode, origin, db_name, working, open_req)) = commit_info
            else {
                return;
            };

            let ev_proto = c.events.borrow().event_proto.unwrap_or(vm.object_proto);
            if mode != TxMode::ReadOnly {
                let storage = c.web_apis.borrow().storage.clone();
                if let Err(e) = storage.idb_commit_database(&origin, &working) {
                    let dom_err = idb_err(vm, "AbortError", &e.to_string()).value;
                    if let Some(tx) = c.idb.borrow_mut().transactions.get_mut(&tx_h) {
                        tx.aborted = true;
                        tx.error = dom_err;
                    }
                    let _ = c.fire(vm, tx_h, "abort", true, false, ev_proto, &[]);
                    c.release_vc_and_pump(mode, &origin, &db_name);
                    return;
                }
            }

            let _ = c.fire(vm, tx_h, "complete", false, false, ev_proto, &[]);
            if let Some(req_h) = open_req {
                let closed_now = db_closed
                    || c.idb
                        .borrow()
                        .databases
                        .get(&db_h)
                        .is_some_and(|d| d.closed);
                c.events.borrow_mut().set_object_event_parent(req_h, None);
                if closed_now {
                    let err = idb_err(
                        vm,
                        "AbortError",
                        "Database connection was closed during upgrade",
                    )
                    .value;
                    c.set_req_done(req_h, JsValue::UNDEFINED, err, true);
                    let _ = c.fire(vm, req_h, "error", true, true, ev_proto, &[]);
                } else {
                    c.set_req_done(req_h, JsValue::from_object(db_h), JsValue::NULL, true);
                    let _ = c.fire(vm, req_h, "success", false, false, ev_proto, &[]);
                }
            }
            c.release_vc_and_pump(mode, &origin, &db_name);
        });
}

pub(crate) fn pump_tx_ops_ctx(ctx: &IdbCtx, tx_h: HeapHandle) {
    let next_action = {
        let mut st = ctx.idb.borrow_mut();
        let Some(tx) = st.transactions.get_mut(&tx_h) else {
            return;
        };
        if tx.is_finished() || tx.in_flight_op || tx.aborted || tx.aborting || !tx.started {
            return;
        }
        tx.queued_ops
            .pop_front()
            .inspect(|_| tx.in_flight_op = true)
    };

    let Some(op) = next_action else {
        let should_commit = ctx.idb.borrow().transactions.get(&tx_h).is_some_and(|tx| {
            tx.started
                && !tx.is_finished()
                && !tx.aborted
                && !tx.aborting
                && !tx.in_flight_op
                && tx.queued_ops.is_empty()
                && tx.dispatching_depth == 0
                && tx.state != TxState::Active
        });
        if should_commit {
            schedule_tx_commit_ctx(ctx, tx_h);
        }
        return;
    };

    let c = ctx.clone();
    ctx.event_loop
        .borrow_mut()
        .post_host_task(TaskPriority::Networking, move |vm| {
            let ev_proto = c.events.borrow().event_proto.unwrap_or(vm.object_proto);
            let req_outcome = match op {
                TxOp::Request { req_h, op: f } => {
                    let mut aborted_early = false;
                    let res = {
                        let mut st = c.idb.borrow_mut();
                        let Some(tx) = st.transactions.get_mut(&tx_h) else {
                            return;
                        };
                        if tx.aborted || tx.aborting || tx.is_finished() {
                            aborted_early = true;
                            Err(("AbortError", "Transaction was aborted".to_string()))
                        } else {
                            f(vm, tx)
                        }
                    };
                    Some((req_h, res, aborted_early, false))
                }
                TxOp::CursorStep {
                    req_h,
                    cursor_h,
                    step,
                } => {
                    let mut aborted_early = false;
                    let res: Result<JsValue, (&'static str, String)> = {
                        let mut st = c.idb.borrow_mut();
                        if st
                            .transactions
                            .get(&tx_h)
                            .is_none_or(|tx| tx.aborted || tx.aborting || tx.is_finished())
                        {
                            aborted_early = true;
                            Err(("AbortError", "Transaction was aborted".to_string()))
                        } else {
                            let next_entry = st.cursors.get(&cursor_h).and_then(|cur| {
                                let tx = st.transactions.get(&tx_h)?;
                                let store = tx.store_snap(cur.store_id)?;
                                if let Some(iid) = cur.index_id {
                                    let idx =
                                        store.indexes.get(tx.index_name(cur.store_id, iid)?)?;
                                    let pos =
                                        cur.current.as_ref().map(|e| (&e.key, &e.primary_key));
                                    step_index_cursor(
                                        store,
                                        idx,
                                        cur.range.as_ref(),
                                        cur.direction,
                                        pos,
                                        &step,
                                        cur.key_only,
                                    )
                                } else {
                                    let ck = cur.current.as_ref().map(|e| &e.key);
                                    step_store_cursor(
                                        store,
                                        cur.range.as_ref(),
                                        cur.direction,
                                        ck,
                                        &step,
                                        cur.key_only,
                                    )
                                }
                            });
                            if let Some(cur) = st.cursors.get_mut(&cursor_h) {
                                cur.current = next_entry;
                                cur.cached_key_js = None;
                                cur.cached_pk_js = None;
                                cur.cached_val_js = None;
                                cur.got_value = cur.current.is_some();
                                Ok(if cur.got_value {
                                    JsValue::from_object(cursor_h)
                                } else {
                                    JsValue::NULL
                                })
                            } else {
                                Ok(JsValue::NULL)
                            }
                        }
                    };
                    Some((req_h, res, aborted_early, true))
                }
                TxOp::ValidateUniqueIndex { store_id, index_id } => {
                    let check_res = c
                        .idb
                        .borrow_mut()
                        .transactions
                        .get_mut(&tx_h)
                        .filter(|tx| !tx.aborted && !tx.aborting && !tx.is_finished())
                        .and_then(|tx| {
                            let iname = tx.index_name(store_id, index_id)?.to_string();
                            let store = tx.store_snap_mut(store_id)?;
                            if let Some(schema) = store.indexes.get_mut(&iname) {
                                schema.unique = true;
                            }
                            Some(validate_index_unique_constraint(
                                store,
                                store.indexes.get(&iname)?,
                            ))
                        });
                    if let Some(Err((en, em))) = check_res {
                        let dom_err = idb_err(vm, en, &em).value;
                        begin_tx_abort_ctx(&c, tx_h, dom_err);
                    }
                    None
                }
            };

            if let Some((req_h, res, aborted_early, is_cursor)) = req_outcome {
                match res {
                    Ok(val) => {
                        c.set_req_done(req_h, val, JsValue::NULL, false);
                        let (_, threw) =
                            c.fire_with_tx(vm, tx_h, req_h, "success", false, false, ev_proto, &[]);
                        if threw {
                            c.abort_on_throw(vm, tx_h, "Uncaught exception in success handler");
                        }
                    }
                    Err((en, em)) => {
                        let dom_err = idb_err(vm, en, &em).value;
                        c.set_req_done(req_h, JsValue::UNDEFINED, dom_err, false);
                        let (prevented, threw) = if is_cursor {
                            c.fire(vm, req_h, "error", true, true, ev_proto, &[])
                        } else {
                            c.fire_with_tx(vm, tx_h, req_h, "error", true, true, ev_proto, &[])
                        };
                        if !aborted_early && !is_cursor {
                            if threw {
                                c.abort_on_throw(vm, tx_h, "Uncaught exception in error handler");
                            } else if !prevented {
                                begin_tx_abort_ctx(&c, tx_h, dom_err);
                            }
                        }
                    }
                }
            }

            let aborted_now = c
                .idb
                .borrow_mut()
                .transactions
                .get_mut(&tx_h)
                .is_some_and(|tx| {
                    tx.in_flight_op = false;
                    tx.aborted || tx.aborting
                });
            if aborted_now {
                let need = c
                    .idb
                    .borrow_mut()
                    .transactions
                    .get_mut(&tx_h)
                    .is_some_and(|tx| {
                        let ok = !tx.is_finished() && !tx.abort_scheduled;
                        if ok {
                            tx.abort_scheduled = true;
                        }
                        ok
                    });
                if need {
                    schedule_tx_abort_drain_ctx(&c, tx_h);
                }
            } else {
                pump_tx_ops_ctx(&c, tx_h);
            }
        });
}

pub(crate) fn pump_db_tx_queue_ctx(ctx: &IdbCtx, origin: &str, db_name: &str) {
    let to_start: Vec<HeapHandle> = {
        let st = ctx.idb.borrow();
        let Some(coord) = st.coords.get(&(origin.to_string(), db_name.to_string())) else {
            return;
        };
        let queue: Vec<HeapHandle> = coord.tx_queue.iter().copied().collect();
        queue
            .iter()
            .enumerate()
            .filter_map(|(idx, &th)| {
                let tx = st.transactions.get(&th)?;
                if tx.started || tx.is_finished() || tx.aborted || tx.aborting {
                    return None;
                }
                let blocked = queue[..idx].iter().any(|&eh| {
                    st.transactions
                        .get(&eh)
                        .is_some_and(|e| !e.is_finished() && transactions_conflict(e, tx))
                });
                (!blocked).then_some(th)
            })
            .collect()
    };
    if to_start.is_empty() {
        return;
    }
    let storage = ctx.web_apis.borrow().storage.clone();
    for th in to_start {
        let fresh = storage
            .idb_load_database(origin, db_name)
            .ok()
            .flatten()
            .unwrap_or_else(|| IdbDatabaseSnapshot::new(db_name.to_string(), 1));
        if let Some(tx) = ctx.idb.borrow_mut().transactions.get_mut(&th) {
            if !tx.started && !tx.is_finished() {
                tx.working = fresh;
                tx.started = true;
            }
        }
        pump_tx_ops_ctx(ctx, th);
    }
}

pub(crate) fn schedule_factory_pump_ctx(ctx: &IdbCtx, origin: &str, db_name: &str) {
    let has_pending = ctx
        .idb
        .borrow()
        .coords
        .get(&(origin.to_string(), db_name.to_string()))
        .is_some_and(|c| !c.open_delete_queue.is_empty() && !c.active_factory_op);
    if !has_pending {
        return;
    }
    let (c, orig, name) = (ctx.clone(), origin.to_string(), db_name.to_string());
    ctx.event_loop
        .borrow_mut()
        .post_host_task(TaskPriority::Networking, move |vm| {
            pump_factory_queue_now(vm, &c, &orig, &name);
        });
}

#[allow(clippy::too_many_arguments)]
fn notify_blocking_connections(
    vm: &mut JsVm,
    ctx: &IdbCtx,
    key: &(String, String),
    req_h: HeapHandle,
    open_conns: Vec<HeapHandle>,
    vc_sent: bool,
    blocked_sent: bool,
    vc_proto: HeapHandle,
    old_version: u64,
    new_version: JsValue,
) {
    let props = [
        (
            "__rix_idb_old_ver__",
            JsValue::from_number(old_version as f64),
        ),
        ("__rix_idb_new_ver__", new_version),
    ];
    if !vc_sent {
        if let Some(f) = ctx
            .idb
            .borrow_mut()
            .coords
            .get_mut(key)
            .and_then(|c| c.open_delete_queue.front_mut())
        {
            f.vc_sent = true;
        }
        for conn_h in open_conns {
            let _ = ctx.fire(vm, conn_h, "versionchange", false, false, vc_proto, &props);
        }
        schedule_factory_pump_ctx(ctx, &key.0, &key.1);
    } else if !blocked_sent {
        if let Some(f) = ctx
            .idb
            .borrow_mut()
            .coords
            .get_mut(key)
            .and_then(|c| c.open_delete_queue.front_mut())
        {
            f.blocked_sent = true;
        }
        let _ = ctx.fire(vm, req_h, "blocked", false, false, vc_proto, &props);
    }
}

fn pump_factory_queue_now(vm: &mut JsVm, ctx: &IdbCtx, origin: &str, db_name: &str) {
    let key = (origin.to_string(), db_name.to_string());
    let ev_proto = ctx.events.borrow().event_proto.unwrap_or(vm.object_proto);
    let vc_proto = ctx
        .idb
        .borrow()
        .protos
        .version_change_event_proto
        .unwrap_or(ev_proto);

    loop {
        let Some((is_delete, req_h, requested_version, vc_sent, blocked_sent)) = ({
            let st = ctx.idb.borrow();
            st.coords
                .get(&key)
                .filter(|c| !c.active_factory_op)
                .and_then(|c| {
                    c.open_delete_queue.front().map(|f| {
                        (
                            f.is_delete,
                            f.req_h,
                            f.requested_version,
                            f.vc_sent,
                            f.blocked_sent,
                        )
                    })
                })
        }) else {
            return;
        };

        let storage = ctx.web_apis.borrow().storage.clone();
        let existing = match storage.idb_load_database(origin, db_name) {
            Ok(opt) => opt,
            Err(e) => {
                ctx.pop_factory_op(&key);
                let err = idb_err(vm, "UnknownError", &e.to_string()).value;
                ctx.set_req_done(req_h, JsValue::UNDEFINED, err, false);
                let _ = ctx.fire(vm, req_h, "error", true, true, ev_proto, &[]);
                continue;
            }
        };
        let old_version = existing.as_ref().map_or(0, |d| d.version);

        let (open_conns, has_unfinished_tx) = {
            let mut st = ctx.idb.borrow_mut();
            let live_conns: Vec<HeapHandle> = st
                .coords
                .get(&key)
                .map(|c| {
                    c.open_connections
                        .iter()
                        .copied()
                        .filter(|dh| st.databases.get(dh).is_some_and(|d| !d.closed))
                        .collect()
                })
                .unwrap_or_default();
            let tx_busy = st.coords.get(&key).is_some_and(|c| {
                c.tx_queue
                    .iter()
                    .any(|th| st.transactions.get(th).is_some_and(|tx| !tx.is_finished()))
            });
            if let Some(c) = st.coords.get_mut(&key) {
                c.open_connections = live_conns.clone();
            }
            (live_conns, tx_busy)
        };

        if is_delete {
            if !open_conns.is_empty() {
                notify_blocking_connections(
                    vm,
                    ctx,
                    &key,
                    req_h,
                    open_conns,
                    vc_sent,
                    blocked_sent,
                    vc_proto,
                    old_version,
                    JsValue::NULL,
                );
                return;
            }
            if has_unfinished_tx {
                return;
            }
            ctx.pop_factory_op(&key);
            let _ = storage.idb_delete_database(origin, db_name);
            ctx.set_req_done(req_h, JsValue::UNDEFINED, JsValue::NULL, false);
            let _ = ctx.fire(
                vm,
                req_h,
                "success",
                false,
                false,
                vc_proto,
                &[
                    (
                        "__rix_idb_old_ver__",
                        JsValue::from_number(old_version as f64),
                    ),
                    ("__rix_idb_new_ver__", JsValue::NULL),
                ],
            );
            continue;
        }

        let target_version = requested_version.unwrap_or_else(|| old_version.max(1));
        if old_version > 0 && target_version < old_version {
            ctx.pop_factory_op(&key);
            let msg = format!("Requested version ({target_version}) < existing ({old_version})");
            let err = idb_err(vm, "VersionError", &msg).value;
            ctx.set_req_done(req_h, JsValue::UNDEFINED, err, false);
            let _ = ctx.fire(vm, req_h, "error", true, true, ev_proto, &[]);
            continue;
        }

        let needs_upgrade = old_version == 0 || target_version > old_version;
        if needs_upgrade {
            if !open_conns.is_empty() {
                notify_blocking_connections(
                    vm,
                    ctx,
                    &key,
                    req_h,
                    open_conns,
                    vc_sent,
                    blocked_sent,
                    vc_proto,
                    old_version,
                    JsValue::from_number(target_version as f64),
                );
                return;
            }
            if has_unfinished_tx {
                return;
            }
        }

        ctx.pop_factory_op(&key);
        let mut snap = existing
            .unwrap_or_else(|| IdbDatabaseSnapshot::new(db_name.to_string(), target_version));
        let orig_snap = snap.clone();
        snap.version = target_version;

        let (db_proto, tx_proto) = {
            let st = ctx.idb.borrow();
            (
                st.protos.database_proto.unwrap_or(vm.object_proto),
                st.protos.transaction_proto.unwrap_or(vm.object_proto),
            )
        };
        let db_h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(db_proto)));
        let store_names: Vec<String> = snap.stores.keys().cloned().collect();
        let schema_stores = metadata_only_stores(&snap.stores);

        if !needs_upgrade {
            {
                let mut st = ctx.idb.borrow_mut();
                st.databases.insert(
                    db_h,
                    IdbDatabaseState {
                        origin: origin.to_string(),
                        name: db_name.to_string(),
                        version: snap.version,
                        store_names,
                        schema_stores,
                        closed: false,
                        active_versionchange_tx: None,
                    },
                );
                st.coords
                    .entry(key.clone())
                    .or_default()
                    .open_connections
                    .push(db_h);
            }
            ctx.set_req_done(req_h, JsValue::from_object(db_h), JsValue::NULL, false);
            let _ = ctx.fire(vm, req_h, "success", false, false, ev_proto, &[]);
            continue;
        }

        let tx_h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(tx_proto)));
        ctx.events
            .borrow_mut()
            .set_object_event_parent(tx_h, Some(db_h));
        ctx.events
            .borrow_mut()
            .set_object_event_parent(req_h, Some(tx_h));
        {
            let mut st = ctx.idb.borrow_mut();
            let mut tx_state = IdbTransactionState::new(
                db_h,
                origin.to_string(),
                db_name.to_string(),
                TxMode::VersionChange,
                "default",
                store_names.clone(),
                snap,
                old_version,
                true,
                Some(req_h),
                Some(orig_snap),
            );
            let mut next_id = st.next_meta_id.max(1);
            tx_state.init_meta_ids(&mut next_id);
            st.next_meta_id = next_id;
            tx_state.initial_store_ids = tx_state.store_id_by_name.clone();
            tx_state.initial_index_ids = tx_state.index_id_by_name.clone();
            st.databases.insert(
                db_h,
                IdbDatabaseState {
                    origin: origin.to_string(),
                    name: db_name.to_string(),
                    version: target_version,
                    store_names,
                    schema_stores,
                    closed: false,
                    active_versionchange_tx: Some(tx_h),
                },
            );
            st.transactions.insert(tx_h, tx_state);
            let coord = st.coords.entry(key.clone()).or_default();
            coord.open_connections.push(db_h);
            coord.tx_queue.push_back(tx_h);
            coord.active_factory_op = true;
            if let Some(r) = st.requests.get_mut(&req_h) {
                r.ready_state = "done";
                r.result = JsValue::from_object(db_h);
                r.transaction = JsValue::from_object(tx_h);
            }
        }

        let (_, threw) = ctx.fire_with_tx(
            vm,
            tx_h,
            req_h,
            "upgradeneeded",
            false,
            false,
            vc_proto,
            &[
                (
                    "__rix_idb_old_ver__",
                    JsValue::from_number(old_version as f64),
                ),
                (
                    "__rix_idb_new_ver__",
                    JsValue::from_number(target_version as f64),
                ),
            ],
        );
        if threw {
            ctx.abort_on_throw(vm, tx_h, "Uncaught exception in upgradeneeded handler");
        }
        pump_tx_ops_ctx(ctx, tx_h);
        return;
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn begin_tx_abort(
    h: &SharedHost,
    ev: &SharedEvents,
    el: &SharedEventLoop,
    wa: &SharedWebApis,
    db: &SharedIdbRuntime,
    tx_h: HeapHandle,
    err: JsValue,
) {
    begin_tx_abort_ctx(&IdbCtx::new(h, ev, el, wa, db), tx_h, err);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn schedule_tx_commit(
    h: &SharedHost,
    ev: &SharedEvents,
    el: &SharedEventLoop,
    wa: &SharedWebApis,
    db: &SharedIdbRuntime,
    tx_h: HeapHandle,
) {
    schedule_tx_commit_ctx(&IdbCtx::new(h, ev, el, wa, db), tx_h);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn pump_tx_ops(
    h: &SharedHost,
    ev: &SharedEvents,
    el: &SharedEventLoop,
    wa: &SharedWebApis,
    db: &SharedIdbRuntime,
    tx_h: HeapHandle,
) {
    pump_tx_ops_ctx(&IdbCtx::new(h, ev, el, wa, db), tx_h);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn pump_db_tx_queue(
    h: &SharedHost,
    ev: &SharedEvents,
    el: &SharedEventLoop,
    wa: &SharedWebApis,
    db: &SharedIdbRuntime,
    origin: &str,
    db_name: &str,
) {
    pump_db_tx_queue_ctx(&IdbCtx::new(h, ev, el, wa, db), origin, db_name);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn schedule_factory_pump(
    h: &SharedHost,
    ev: &SharedEvents,
    el: &SharedEventLoop,
    wa: &SharedWebApis,
    db: &SharedIdbRuntime,
    origin: &str,
    db_name: &str,
) {
    schedule_factory_pump_ctx(&IdbCtx::new(h, ev, el, wa, db), origin, db_name);
}

pub(crate) fn check_store_for_request(
    vm: &mut JsVm,
    idb: &SharedIdbRuntime,
    sh: HeapHandle,
    require_writable: bool,
) -> Result<(HeapHandle, u64), JsException> {
    let st = idb.borrow();
    let s = st
        .stores
        .get(&sh)
        .ok_or_else(|| vm.type_error("Illegal invocation"))?;
    if s.deleted {
        return Err(idb_err(
            vm,
            "InvalidStateError",
            "Object store has been deleted",
        ));
    }
    let tx = st
        .transactions
        .get(&s.tx_h)
        .ok_or_else(|| idb_err(vm, "TransactionInactiveError", "Transaction inactive"))?;
    if tx.state != TxState::Active || tx.aborted || tx.aborting {
        return Err(idb_err(
            vm,
            "TransactionInactiveError",
            "Transaction is not active",
        ));
    }
    if require_writable && tx.mode == TxMode::ReadOnly {
        return Err(idb_err(
            vm,
            "ReadOnlyError",
            "Cannot mutate object store in a readonly transaction",
        ));
    }
    Ok((s.tx_h, s.store_id))
}

pub(crate) fn check_index_for_request(
    vm: &mut JsVm,
    idb: &SharedIdbRuntime,
    ih: HeapHandle,
) -> Result<(HeapHandle, u64, u64), JsException> {
    let st = idb.borrow();
    let idx = st
        .indexes
        .get(&ih)
        .ok_or_else(|| vm.type_error("Illegal invocation"))?;
    if idx.deleted || st.stores.get(&idx.store_h).is_some_and(|s| s.deleted) {
        return Err(idb_err(
            vm,
            "InvalidStateError",
            "Index or its object store has been deleted",
        ));
    }
    let tx = st
        .transactions
        .get(&idx.tx_h)
        .ok_or_else(|| idb_err(vm, "TransactionInactiveError", "Transaction inactive"))?;
    if tx.state != TxState::Active || tx.aborted || tx.aborting {
        return Err(idb_err(
            vm,
            "TransactionInactiveError",
            "Transaction is not active",
        ));
    }
    Ok((idx.tx_h, idx.store_id, idx.index_id))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn enqueue_tx_request<F>(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    idb: &SharedIdbRuntime,
    tx_h: HeapHandle,
    source_h: HeapHandle,
    op: F,
) -> Result<HeapHandle, JsException>
where
    F: FnOnce(&mut JsVm, &mut IdbTransactionState) -> Result<JsValue, (&'static str, String)>
        + 'static,
{
    let active = idb
        .borrow()
        .transactions
        .get(&tx_h)
        .is_some_and(|tx| tx.state == TxState::Active && !tx.aborted && !tx.aborting);
    if !active {
        return Err(idb_err(
            vm,
            "TransactionInactiveError",
            "Transaction is not active",
        ));
    }

    let req_h = alloc_request(
        vm,
        events,
        idb,
        false,
        JsValue::from_object(source_h),
        JsValue::from_object(tx_h),
    );
    if let Some(tx) = idb.borrow_mut().transactions.get_mut(&tx_h) {
        tx.queued_ops.push_back(TxOp::Request {
            req_h,
            op: Box::new(op),
        });
    }
    pump_tx_ops(host, events, event_loop, web_apis, idb, tx_h);
    Ok(req_h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_tx(mode: TxMode, scope: &[&str]) -> IdbTransactionState {
        IdbTransactionState::new(
            HeapHandle::new(1, 0),
            "https://example.test".to_string(),
            "db".to_string(),
            mode,
            "default",
            scope.iter().map(|s| (*s).to_string()).collect(),
            IdbDatabaseSnapshot::new("db".to_string(), 1),
            0,
            false,
            None,
            None,
        )
    }

    #[test]
    fn scope_conflict_matrix_matches_w3c_idb() {
        let ro_a = dummy_tx(TxMode::ReadOnly, &["a"]);
        let ro_ab = dummy_tx(TxMode::ReadOnly, &["a", "b"]);
        let rw_a = dummy_tx(TxMode::ReadWrite, &["a"]);
        let rw_b = dummy_tx(TxMode::ReadWrite, &["b"]);
        let vc = dummy_tx(TxMode::VersionChange, &[]);

        assert!(!transactions_conflict(&ro_a, &ro_ab));
        assert!(transactions_conflict(&ro_a, &rw_a));
        assert!(!transactions_conflict(&ro_a, &rw_b));
        assert!(!transactions_conflict(&rw_a, &rw_b));
        assert!(transactions_conflict(&rw_a, &rw_a));
        assert!(transactions_conflict(&vc, &ro_a));
        assert!(transactions_conflict(&rw_b, &vc));
    }

    #[test]
    fn metadata_rollback_restores_stores_and_version() {
        let mut orig = IdbDatabaseSnapshot::new("db".to_string(), 1);
        orig.stores.insert(
            "s1".to_string(),
            IdbStoreSnapshot::new("s1".to_string(), None, false),
        );
        let mut working = orig.clone();
        working.version = 2;
        working.stores.insert(
            "s2".to_string(),
            IdbStoreSnapshot::new("s2".to_string(), None, false),
        );

        let db_h = HeapHandle::new(10, 0);
        let tx_h = HeapHandle::new(11, 0);
        let mut tx = IdbTransactionState::new(
            db_h,
            "https://example.test".to_string(),
            "db".to_string(),
            TxMode::VersionChange,
            "default",
            vec!["s1".to_string(), "s2".to_string()],
            working,
            1,
            true,
            None,
            Some(orig),
        );
        let mut next_id = 1;
        tx.init_meta_ids(&mut next_id);
        tx.initial_store_ids.insert("s1".to_string(), 1);

        let mut st = IdbRuntimeState::new();
        st.databases.insert(
            db_h,
            IdbDatabaseState {
                origin: "https://example.test".to_string(),
                name: "db".to_string(),
                version: 2,
                store_names: vec!["s1".to_string(), "s2".to_string()],
                schema_stores: BTreeMap::new(),
                closed: false,
                active_versionchange_tx: Some(tx_h),
            },
        );
        st.transactions.insert(tx_h, tx);

        revert_versionchange_metadata(&mut st, tx_h);
        let db = st.databases.get(&db_h).unwrap();
        assert_eq!(db.version, 1);
        assert_eq!(db.store_names, vec!["s1".to_string()]);
    }
}
