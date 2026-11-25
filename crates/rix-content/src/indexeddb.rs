//! Full async transactional W3C IndexedDB (`IDBFactory`, `IDBOpenDBRequest`,
//! `IDBRequest`, `IDBDatabase`, `IDBTransaction`, `IDBObjectStore`, `IDBIndex`,
//! `IDBCursor`, `IDBKeyRange`) persisted atomically per origin in `rix-storage` (`redb`).
//!
//! Reference: Spec 06 §3, Issue #24.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::rc::Rc;

use rix_js::promise::{alloc_promise, reject_promise, resolve_promise};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_storage::{IdbDatabaseSnapshot, IdbIndexSchema, IdbStoreSnapshot};

use crate::canvas2d::SharedCanvas2D;
use crate::dom_bridge::SharedHost;
use crate::dom_interfaces::define_to_string_tag;
use crate::event_loop::SharedEventLoop;
use crate::events::{
    install_object_event_handler_accessor, register_constructed_event, SharedEvents,
};
use crate::fetch_ws::SharedNetBridge;
use crate::frames::{decode_structured_json, SharedFrames};
use crate::indexeddb_engine::{
    alloc_dom_string_list, alloc_js_array, collect_index_entries, collect_store_entries,
    enforce_range_idb_version, enforce_range_u32, idb_key_to_js, js_to_idb_key, parse_key_path_raw,
    store_put_or_add_payload, validate_key_path_syntax, validate_put_or_add_sync, CursorDirection,
    CursorStepRequest, IdbCursorEntry, KeyRange,
};
use crate::indexeddb_runtime::{
    alloc_request, begin_tx_abort, check_index_for_request, check_store_for_request,
    enqueue_tx_request, idb_err, pump_db_tx_queue, pump_tx_ops, pump_tx_ops_ctx,
    schedule_factory_pump, schedule_tx_commit, CloneDeps, IdbCtx, IdbCursorState, IdbIndexHandle,
    IdbStoreHandle, IdbTransactionState, PendingFactoryOp, TxMode, TxOp, TxState,
};
pub use crate::indexeddb_runtime::{
    IdbRuntimeState as IndexedDbSystem, SharedIdbRuntime as SharedIndexedDb,
};
use crate::structured_clone::{
    decode_storage_payload, deserialize_for_storage, serialize_for_storage, CloneContext,
    StructuredClonePayload,
};
use crate::web_apis::{
    arg, bind_ctor_proto, create_dom_exception, define_global, define_host_accessor,
    define_host_getter, define_host_method, SharedWebApis,
};
use crate::workers::SharedWorkers;

fn idb_frames(idb: &SharedIndexedDb) -> Option<SharedFrames> {
    idb.borrow()
        .clone_deps
        .as_ref()
        .map(|d| Rc::clone(&d.frames))
}

fn idb_effective_origin(vm: &mut JsVm, web_apis: &SharedWebApis, idb: &SharedIndexedDb) -> String {
    // Sandboxed iframes run inside a local global whose own `origin` is `"null"`.
    let k_orig = vm.heap.intern_key("origin");
    if let Some(p) = vm.heap.object_get_own(vm.global, k_orig, &vm.shapes) {
        if p.value.is_string() {
            let s = vm.value_to_rust_string(p.value);
            if s == "null" {
                return s;
            }
        }
    }
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

#[allow(clippy::too_many_arguments)]
fn serialize_while_tx_inactive(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    web_apis: &SharedWebApis,
    idb: &SharedIndexedDb,
    tx_h: HeapHandle,
    val: JsValue,
) -> Result<StructuredClonePayload, JsException> {
    let prev_state = idb.borrow_mut().transactions.get_mut(&tx_h).map(|t| {
        let prev = t.state;
        t.state = TxState::Inactive;
        prev
    });
    let res = idb_serialize_value(vm, host, events, web_apis, idb, val);
    if let Some(prev) = prev_state {
        if let Some(t) = idb.borrow_mut().transactions.get_mut(&tx_h) {
            if t.state == TxState::Inactive && !t.aborted && !t.aborting {
                t.state = prev;
            }
        }
    }
    res
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

    // W3C IDB §5.4: deactivate Active transactions (not currently inside event dispatch) at the
    // end of each WHATWG microtask checkpoint, and trigger auto-commit when idle with 0 ops.
    // Capture `Weak` handles so `event_loop -> checkpoint_hooks -> event_loop` never forms an Rc
    // cycle that keeps `WebApiState` / `StorageEngine` (`redb`) alive after `ContentRuntime` drops.
    let h_ck = Rc::downgrade(host);
    let ev_ck = Rc::downgrade(events);
    let el_ck = Rc::downgrade(event_loop);
    let wa_ck = Rc::downgrade(web_apis);
    let idb_ck = Rc::downgrade(&idb);
    event_loop
        .borrow_mut()
        .add_checkpoint_hook(Rc::new(RefCell::new(Box::new(move |_vm| {
            let (Some(h_r), Some(ev_r), Some(el_r), Some(wa_r), Some(idb_r)) = (
                h_ck.upgrade(),
                ev_ck.upgrade(),
                el_ck.upgrade(),
                wa_ck.upgrade(),
                idb_ck.upgrade(),
            ) else {
                return;
            };
            let to_commit: Vec<HeapHandle> = {
                let mut st = idb_r.borrow_mut();
                let mut ready = Vec::new();
                for (&th, tx) in &mut st.transactions {
                    if tx.state == TxState::Active && tx.dispatching_depth == 0 {
                        tx.state = TxState::Inactive;
                        if tx.started
                            && !tx.aborted
                            && !tx.aborting
                            && !tx.in_flight_op
                            && tx.queued_ops.is_empty()
                        {
                            ready.push(th);
                        }
                    }
                }
                ready
            };
            for th in to_commit {
                schedule_tx_commit(&h_r, &ev_r, &el_r, &wa_r, &idb_r, th);
            }
        }))));

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
            let (cached, key) = {
                let st = db.borrow();
                let Some(c) = st.cursors.get(&h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                (c.cached_key_js, c.current.as_ref().map(|e| e.key.clone()))
            };
            if let Some(v) = cached {
                return Ok(v);
            }
            let Some(k) = key else {
                return Ok(JsValue::UNDEFINED);
            };
            let v = idb_key_to_js(vm, &k);
            if let Some(c) = db.borrow_mut().cursors.get_mut(&h) {
                c.cached_key_js = Some(v);
            }
            Ok(v)
        });
    }
    {
        let db = Rc::clone(idb);
        define_host_getter(vm, proto, "primaryKey", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let (cached, pk) = {
                let st = db.borrow();
                let Some(c) = st.cursors.get(&h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                (
                    c.cached_pk_js,
                    c.current.as_ref().map(|e| e.primary_key.clone()),
                )
            };
            if let Some(v) = cached {
                return Ok(v);
            }
            let Some(k) = pk else {
                return Ok(JsValue::UNDEFINED);
            };
            let v = idb_key_to_js(vm, &k);
            if let Some(c) = db.borrow_mut().cursors.get_mut(&h) {
                c.cached_pk_js = Some(v);
            }
            Ok(v)
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
            let (cached, val_data, deps) = {
                let st = db.borrow();
                let Some(c) = st.cursors.get(&h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if c.key_only {
                    return Err(vm.type_error("Illegal invocation"));
                }
                (
                    c.cached_val_js,
                    c.current
                        .as_ref()
                        .map(|e| (e.value_bytes.clone(), e.value_json.clone())),
                    st.clone_deps.clone(),
                )
            };
            if let Some(v) = cached {
                return Ok(v);
            }
            let Some((vb, vj)) = val_data else {
                return Ok(JsValue::UNDEFINED);
            };
            let v = idb_deserialize_entry(vm, &h_r, &ev_r, &wa_r, deps.as_ref(), &vb, &vj);
            if let Some(c) = db.borrow_mut().cursors.get_mut(&h) {
                c.cached_val_js = Some(v);
            }
            Ok(v)
        });
    }

    // Shared cursor validation helpers.
    fn cursor_source_deleted(st: &IndexedDbSystem, c: &IdbCursorState) -> bool {
        if let Some(s) = st.stores.get(&c.source_h) {
            return s.deleted;
        }
        if let Some(i) = st.indexes.get(&c.source_h) {
            return i.deleted || st.stores.get(&i.store_h).is_some_and(|s| s.deleted);
        }
        let Some(tx) = st.transactions.get(&c.tx_h) else {
            return true;
        };
        let Some(store) = tx.store_snap(c.store_id) else {
            return true;
        };
        c.index_id.is_some_and(|iid| {
            tx.index_name(c.store_id, iid)
                .and_then(|n| store.indexes.get(n))
                .is_none()
        })
    }

    fn validate_cursor_step<'a>(
        vm: &mut JsVm,
        st: &'a IndexedDbSystem,
        ch: HeapHandle,
        require_non_unique_index: bool,
    ) -> Result<&'a IdbCursorState, JsException> {
        let c = st
            .cursors
            .get(&ch)
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        if !st
            .transactions
            .get(&c.tx_h)
            .is_some_and(|t| t.state == TxState::Active && !t.aborted && !t.aborting)
        {
            return Err(idb_err(
                vm,
                "TransactionInactiveError",
                "Transaction is not active",
            ));
        }
        if cursor_source_deleted(st, c) {
            return Err(idb_err(
                vm,
                "InvalidStateError",
                "Cursor source or object store has been deleted",
            ));
        }
        if require_non_unique_index && (c.index_id.is_none() || c.direction.is_unique()) {
            return Err(idb_err(
                vm,
                "InvalidAccessError",
                "continuePrimaryKey() requires a non-unique index cursor",
            ));
        }
        if !c.got_value {
            return Err(idb_err(
                vm,
                "InvalidStateError",
                "Cursor has already been iterated",
            ));
        }
        Ok(c)
    }

    fn validate_cursor_mutate(
        vm: &mut JsVm,
        st: &IndexedDbSystem,
        ch: HeapHandle,
    ) -> Result<(HeapHandle, u64, rix_storage::IdbKey), JsException> {
        let c = st
            .cursors
            .get(&ch)
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let tx = st
            .transactions
            .get(&c.tx_h)
            .ok_or_else(|| idb_err(vm, "TransactionInactiveError", "Transaction is not active"))?;
        if tx.state != TxState::Active || tx.aborted || tx.aborting {
            return Err(idb_err(
                vm,
                "TransactionInactiveError",
                "Transaction is not active",
            ));
        }
        if tx.mode == TxMode::ReadOnly {
            return Err(idb_err(
                vm,
                "ReadOnlyError",
                "Cannot mutate cursor inside a readonly transaction",
            ));
        }
        if cursor_source_deleted(st, c) {
            return Err(idb_err(
                vm,
                "InvalidStateError",
                "Cursor source or object store has been deleted",
            ));
        }
        if !c.got_value || c.key_only {
            return Err(idb_err(
                vm,
                "InvalidStateError",
                "Cannot mutate a key cursor or cursor without a value",
            ));
        }
        let entry = c
            .current
            .as_ref()
            .ok_or_else(|| idb_err(vm, "InvalidStateError", "Cursor has no current value"))?;
        Ok((c.tx_h, c.store_id, entry.primary_key.clone()))
    }

    fn queue_cursor_step(
        db: &SharedIndexedDb,
        ch: HeapHandle,
        tx_h: HeapHandle,
        req_h: HeapHandle,
        step: CursorStepRequest,
    ) {
        let mut st = db.borrow_mut();
        if let Some(c) = st.cursors.get_mut(&ch) {
            c.got_value = false;
        }
        if let Some(r) = st.requests.get_mut(&req_h) {
            r.ready_state = "pending";
        }
        if let Some(tx) = st.transactions.get_mut(&tx_h) {
            tx.queued_ops.push_back(TxOp::CursorStep {
                req_h,
                cursor_h: ch,
                step,
            });
        }
    }

    // cursor.continue(key?)
    {
        let ctx = IdbCtx::new(host, events, event_loop, web_apis, idb);
        define_host_method(vm, proto, "continue", move |vm, this_val, args| {
            let Some(ch) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let (tx_h, req_h, dir, cur_key) = {
                let st = ctx.idb.borrow();
                let c = validate_cursor_step(vm, &st, ch, false)?;
                (
                    c.tx_h,
                    c.req_h,
                    c.direction,
                    c.current.as_ref().map(|e| e.key.clone()),
                )
            };
            let target_key = if arg(args, 0).is_undefined() {
                None
            } else {
                let frames = idb_frames(&ctx.idb);
                let mut seen = HashSet::new();
                let tk = js_to_idb_key(vm, frames.as_ref(), arg(args, 0), &mut seen)?;
                if let Some(ref ck) = cur_key {
                    let invalid = if dir.is_reverse() {
                        tk.idb_cmp(ck) != Ordering::Less
                    } else {
                        tk.idb_cmp(ck) != Ordering::Greater
                    };
                    if invalid {
                        return Err(idb_err(
                            vm,
                            "DataError",
                            "Cursor continue() key does not advance in cursor direction",
                        ));
                    }
                }
                Some(tk)
            };
            queue_cursor_step(
                &ctx.idb,
                ch,
                tx_h,
                req_h,
                CursorStepRequest::Continue(target_key),
            );
            pump_tx_ops_ctx(&ctx, tx_h);
            Ok(JsValue::UNDEFINED)
        });
    }

    // cursor.continuePrimaryKey(key, primaryKey)
    {
        let ctx = IdbCtx::new(host, events, event_loop, web_apis, idb);
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
                let (tx_h, req_h, dir, cur_key, cur_pk) = {
                    let st = ctx.idb.borrow();
                    let c = validate_cursor_step(vm, &st, ch, true)?;
                    (
                        c.tx_h,
                        c.req_h,
                        c.direction,
                        c.current.as_ref().map(|e| e.key.clone()),
                        c.current.as_ref().map(|e| e.primary_key.clone()),
                    )
                };
                let frames = idb_frames(&ctx.idb);
                let mut seen = HashSet::new();
                let target_key = js_to_idb_key(vm, frames.as_ref(), arg(args, 0), &mut seen)?;
                seen.clear();
                let target_pk = js_to_idb_key(vm, frames.as_ref(), arg(args, 1), &mut seen)?;
                if let (Some(ref ck), Some(ref cpk)) = (cur_key, cur_pk) {
                    let (cmp_k, cmp_pk) = (target_key.idb_cmp(ck), target_pk.idb_cmp(cpk));
                    let invalid = if dir.is_reverse() {
                        cmp_k == Ordering::Greater
                            || (cmp_k == Ordering::Equal && cmp_pk != Ordering::Less)
                    } else {
                        cmp_k == Ordering::Less
                            || (cmp_k == Ordering::Equal && cmp_pk != Ordering::Greater)
                    };
                    if invalid {
                        return Err(idb_err(
                            vm,
                            "DataError",
                            "continuePrimaryKey() key/primaryKey does not advance",
                        ));
                    }
                }
                queue_cursor_step(
                    &ctx.idb,
                    ch,
                    tx_h,
                    req_h,
                    CursorStepRequest::ContinuePrimaryKey(target_key, target_pk),
                );
                pump_tx_ops_ctx(&ctx, tx_h);
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // cursor.advance(count)
    {
        let ctx = IdbCtx::new(host, events, event_loop, web_apis, idb);
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
            let (tx_h, req_h) = {
                let st = ctx.idb.borrow();
                let c = validate_cursor_step(vm, &st, ch, false)?;
                (c.tx_h, c.req_h)
            };
            queue_cursor_step(
                &ctx.idb,
                ch,
                tx_h,
                req_h,
                CursorStepRequest::Advance(count as usize),
            );
            pump_tx_ops_ctx(&ctx, tx_h);
            Ok(JsValue::UNDEFINED)
        });
    }

    // cursor.update(value)
    {
        let ctx = IdbCtx::new(host, events, event_loop, web_apis, idb);
        define_host_method(vm, proto, "update", move |vm, this_val, args| {
            let Some(ch) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if args.is_empty() {
                return Err(vm.type_error("IDBCursor.update requires 1 argument"));
            }
            let (tx_h, store_id, pk) = validate_cursor_mutate(vm, &ctx.idb.borrow(), ch)?;
            let payload = serialize_while_tx_inactive(
                vm,
                &ctx.host,
                &ctx.events,
                &ctx.web_apis,
                &ctx.idb,
                tx_h,
                arg(args, 0),
            )?;
            if let Some(store) = ctx
                .idb
                .borrow()
                .transactions
                .get(&tx_h)
                .and_then(|tx| tx.store_snap(store_id))
            {
                if store.key_path.is_some() {
                    match validate_put_or_add_sync(store, &payload, None) {
                        Ok(Some(extracted)) if extracted.idb_cmp(&pk) == Ordering::Equal => {}
                        Ok(Some(_)) => {
                            return Err(idb_err(
                                vm,
                                "DataError",
                                "Updated key does not match cursor primaryKey",
                            ))
                        }
                        Ok(None) => {
                            return Err(idb_err(
                                vm,
                                "DataError",
                                "Updated value is missing inline key",
                            ))
                        }
                        Err((en, em)) => return Err(idb_err(vm, en, &em)),
                    }
                }
            }
            let req_h = enqueue_tx_request(
                vm,
                &ctx.host,
                &ctx.events,
                &ctx.event_loop,
                &ctx.web_apis,
                &ctx.idb,
                tx_h,
                ch,
                move |vm, tx| {
                    let store = tx
                        .store_snap_mut(store_id)
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
        let ctx = IdbCtx::new(host, events, event_loop, web_apis, idb);
        define_host_method(vm, proto, "delete", move |vm, this_val, _| {
            let Some(ch) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let (tx_h, store_id, pk) = validate_cursor_mutate(vm, &ctx.idb.borrow(), ch)?;
            let req_h = enqueue_tx_request(
                vm,
                &ctx.host,
                &ctx.events,
                &ctx.event_loop,
                &ctx.web_apis,
                &ctx.idb,
                tx_h,
                ch,
                move |_, tx| {
                    let store = tx
                        .store_snap_mut(store_id)
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
        let db_g = Rc::clone(idb);
        let db_s = Rc::clone(idb);
        define_host_accessor(
            vm,
            proto,
            "name",
            move |vm, this_val, _| {
                let Some(s) = this_val
                    .as_object()
                    .and_then(|h| db_g.borrow().indexes.get(&h).map(|i| i.index_name.clone()))
                else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
            },
            move |vm, this_val, args| {
                let Some(ih) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if !db_s.borrow().indexes.contains_key(&ih) {
                    return Err(vm.type_error("Illegal invocation"));
                }
                let new_name = vm.to_js_string(arg(args, 0))?;
                let mut st = db_s.borrow_mut();
                let (tx_h, store_h, store_id, index_id, old_name, idx_deleted) = {
                    let i = st
                        .indexes
                        .get(&ih)
                        .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                    (
                        i.tx_h,
                        i.store_h,
                        i.store_id,
                        i.index_id,
                        i.index_name.clone(),
                        i.deleted,
                    )
                };
                let store_deleted = st.stores.get(&store_h).is_some_and(|s| s.deleted);
                let tx = st.transactions.get_mut(&tx_h).ok_or_else(|| {
                    idb_err(vm, "TransactionInactiveError", "Transaction is not active")
                })?;
                if tx.mode != TxMode::VersionChange {
                    return Err(idb_err(
                        vm,
                        "InvalidStateError",
                        "Index can only be renamed during a versionchange transaction",
                    ));
                }
                if idx_deleted || store_deleted {
                    return Err(idb_err(
                        vm,
                        "InvalidStateError",
                        "Index or its object store has been deleted",
                    ));
                }
                if tx.state != TxState::Active || tx.aborted || tx.aborting {
                    return Err(idb_err(
                        vm,
                        "TransactionInactiveError",
                        "Transaction is not active",
                    ));
                }
                if new_name == old_name {
                    return Ok(JsValue::UNDEFINED);
                }
                let Some(store_name) = tx.name_by_store_id.get(&store_id).cloned() else {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Object store has been deleted",
                    ));
                };
                let Some(store) = tx.working.stores.get_mut(&store_name) else {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Object store has been deleted",
                    ));
                };
                if store.indexes.contains_key(&new_name) {
                    return Err(create_dom_exception(
                        vm,
                        "ConstraintError",
                        &format!("Index '{new_name}' already exists"),
                    ));
                }
                if let Some(mut schema) = store.indexes.remove(&old_name) {
                    schema.name = new_name.clone();
                    store.indexes.insert(new_name.clone(), schema);
                }
                tx.index_id_by_name.remove(&(store_id, old_name));
                tx.index_id_by_name
                    .insert((store_id, new_name.clone()), index_id);
                tx.name_by_index_id
                    .insert((store_id, index_id), new_name.clone());
                if let Some(i) = st.indexes.get_mut(&ih) {
                    i.index_name = new_name;
                }
                Ok(JsValue::UNDEFINED)
            },
        );
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
                    let store = tx.store_snap(idx.store_id)?;
                    let iname = tx.index_name(idx.store_id, idx.index_id)?;
                    store
                        .indexes
                        .get(iname)
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
                if is_unique {
                    idx.cached_unique
                } else {
                    idx.cached_multi_entry
                }
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
            let (tx_h, store_id, index_id) = check_index_for_request(vm, &db_r, ih)?;
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
                        .store_snap(store_id)
                        .ok_or(("NotFoundError", "Object store not found".to_string()))?;
                    let iname = tx
                        .index_name(store_id, index_id)
                        .ok_or(("NotFoundError", "Index not found".to_string()))?;
                    let idx_schema = store
                        .indexes
                        .get(iname)
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
            let (tx_h, store_id, index_id) = check_index_for_request(vm, &db_r, ih)?;
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
                store_id,
                Some(index_id),
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
    store_id: u64,
    index_id: Option<u64>,
    range: Option<KeyRange>,
    direction: CursorDirection,
    key_only: bool,
) -> Result<HeapHandle, JsException> {
    {
        let st = idb.borrow();
        let Some(tx) = st.transactions.get(&tx_h) else {
            return Err(create_dom_exception(
                vm,
                "TransactionInactiveError",
                "Transaction is not active",
            ));
        };
        if tx.state != TxState::Active || tx.aborted || tx.aborting {
            return Err(create_dom_exception(
                vm,
                "TransactionInactiveError",
                "Transaction is not active",
            ));
        }
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
    {
        let mut st = idb.borrow_mut();
        st.cursors.insert(
            cursor_h,
            IdbCursorState {
                tx_h,
                req_h,
                source_h,
                store_id,
                index_id,
                range,
                direction,
                key_only,
                got_value: false,
                current: None,
                cached_key_js: None,
                cached_pk_js: None,
                cached_val_js: None,
            },
        );
        if let Some(tx) = st.transactions.get_mut(&tx_h) {
            tx.queued_ops.push_back(TxOp::CursorStep {
                req_h,
                cursor_h,
                step: CursorStepRequest::Initial,
            });
        }
    }
    pump_tx_ops(host, events, event_loop, web_apis, idb, tx_h);
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
        let db_g = Rc::clone(idb);
        let db_s = Rc::clone(idb);
        define_host_accessor(
            vm,
            proto,
            "name",
            move |vm, this_val, _| {
                let Some(s) = this_val
                    .as_object()
                    .and_then(|h| db_g.borrow().stores.get(&h).map(|s| s.name.clone()))
                else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
            },
            move |vm, this_val, args| {
                let Some(sh) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if !db_s.borrow().stores.contains_key(&sh) {
                    return Err(vm.type_error("Illegal invocation"));
                }
                let new_name = vm.to_js_string(arg(args, 0))?;
                let mut st = db_s.borrow_mut();
                let (tx_h, store_id, old_name, deleted) = {
                    let s = st
                        .stores
                        .get(&sh)
                        .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                    (s.tx_h, s.store_id, s.name.clone(), s.deleted)
                };
                let tx = st.transactions.get_mut(&tx_h).ok_or_else(|| {
                    idb_err(vm, "TransactionInactiveError", "Transaction is not active")
                })?;
                if tx.mode != TxMode::VersionChange {
                    return Err(idb_err(
                        vm,
                        "InvalidStateError",
                        "Object store can only be renamed during a versionchange transaction",
                    ));
                }
                if deleted {
                    return Err(idb_err(
                        vm,
                        "InvalidStateError",
                        "Object store has been deleted",
                    ));
                }
                if tx.state != TxState::Active || tx.aborted || tx.aborting {
                    return Err(idb_err(
                        vm,
                        "TransactionInactiveError",
                        "Transaction is not active",
                    ));
                }
                if new_name == old_name {
                    return Ok(JsValue::UNDEFINED);
                }
                if tx.working.stores.contains_key(&new_name) {
                    return Err(idb_err(
                        vm,
                        "ConstraintError",
                        &format!("Object store '{new_name}' already exists"),
                    ));
                }
                if let Some(mut snap) = tx.working.stores.remove(&old_name) {
                    snap.name = new_name.clone();
                    tx.working.stores.insert(new_name.clone(), snap);
                }
                for sname in &mut tx.scope {
                    if *sname == old_name {
                        *sname = new_name.clone();
                    }
                }
                tx.store_id_by_name.remove(&old_name);
                tx.store_id_by_name.insert(new_name.clone(), store_id);
                tx.name_by_store_id.insert(store_id, new_name.clone());
                if let Some(s) = st.stores.get_mut(&sh) {
                    s.name = new_name;
                }
                Ok(JsValue::UNDEFINED)
            },
        );
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
                    let s = tx.store_snap(sh.store_id)?;
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
                sh.cached_auto_increment
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
                let n = if sh.deleted {
                    Vec::new()
                } else {
                    st.transactions
                        .get(&sh.tx_h)
                        .and_then(|tx| tx.store_snap(sh.store_id))
                        .map(|s| s.indexes.keys().cloned().collect::<Vec<_>>())
                        .unwrap_or_default()
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
            let (tx_h, store_id) = check_store_for_request(vm, &db_r, sh, true)?;
            let payload =
                serialize_while_tx_inactive(vm, &h_r, &ev_r, &wa_r, &db_r, tx_h, arg(args, 0))?;
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
                let s = st
                    .stores
                    .get(&sh)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                if s.deleted {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Object store has been deleted",
                    ));
                }
                let tx = st.transactions.get(&tx_h).ok_or_else(|| {
                    create_dom_exception(vm, "TransactionInactiveError", "Transaction inactive")
                })?;
                if tx.state != TxState::Active || tx.aborted || tx.aborting {
                    return Err(create_dom_exception(
                        vm,
                        "TransactionInactiveError",
                        "Transaction is not active",
                    ));
                }
                let store = tx.store_snap(store_id).ok_or_else(|| {
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
                        .store_snap_mut(store_id)
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
            let (tx_h, store_id) = check_store_for_request(vm, &db_r, sh, false)?;
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
                        .store_snap(store_id)
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
            let (tx_h, store_id) = check_store_for_request(vm, &db_r, sh, true)?;
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
                        .store_snap_mut(store_id)
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
            let (tx_h, store_id) = check_store_for_request(vm, &db_r, sh, true)?;
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
                        .store_snap_mut(store_id)
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
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, "createIndex", move |vm, this_val, args| {
            let Some(sh) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if !db_r.borrow().stores.contains_key(&sh) {
                return Err(vm.type_error("Illegal invocation"));
            }
            if args.len() < 2 {
                return Err(vm.type_error("IDBObjectStore.createIndex requires 2 arguments"));
            }
            let idx_name = vm.to_js_string(arg(args, 0))?;
            let Some((key_path, key_path_is_array)) = parse_key_path_raw(vm, arg(args, 1), false)?
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

            // W3C IDB §4.5 exact exception ordering:
            // VersionChange -> deleted -> TransactionInactiveError -> ConstraintError -> SyntaxError -> InvalidAccessError.
            let (tx_h, store_id) = {
                let st = db_r.borrow();
                let s = st
                    .stores
                    .get(&sh)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                let tx = st.transactions.get(&s.tx_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Transaction inactive")
                })?;
                if tx.mode != TxMode::VersionChange {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "createIndex() is only allowed inside a versionchange transaction",
                    ));
                }
                if s.deleted {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Object store has been deleted",
                    ));
                }
                if tx.state != TxState::Active || tx.aborted || tx.aborting {
                    return Err(create_dom_exception(
                        vm,
                        "TransactionInactiveError",
                        "Transaction is not active",
                    ));
                }
                let store = tx.store_snap(s.store_id).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Object store has been deleted")
                })?;
                if store.indexes.contains_key(&idx_name) {
                    return Err(create_dom_exception(
                        vm,
                        "ConstraintError",
                        &format!("Index '{idx_name}' already exists"),
                    ));
                }
                (s.tx_h, s.store_id)
            };
            validate_key_path_syntax(vm, &key_path, key_path_is_array)?;
            if multi_entry && (key_path_is_array || key_path.len() > 1) {
                return Err(create_dom_exception(
                    vm,
                    "InvalidAccessError",
                    "multiEntry cannot be true for array keyPath",
                ));
            }
            let (index_id, index_proto) = {
                let mut st = db_r.borrow_mut();
                let iid = st.alloc_meta_id();
                let proto_h = st.protos.index_proto.unwrap_or(vm.object_proto);
                let tx = st.transactions.get_mut(&tx_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Transaction inactive")
                })?;
                let store = tx.store_snap_mut(store_id).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Object store has been deleted")
                })?;
                store.indexes.insert(
                    idx_name.clone(),
                    IdbIndexSchema {
                        name: idx_name.clone(),
                        key_path,
                        key_path_is_array,
                        // Activated when `TxOp::ValidateUniqueIndex` executes in FIFO order after
                        // any earlier queued `put`/`add` requests on this transaction.
                        unique: false,
                        multi_entry,
                    },
                );
                tx.index_id_by_name
                    .insert((store_id, idx_name.clone()), iid);
                tx.name_by_index_id
                    .insert((store_id, iid), idx_name.clone());
                if unique {
                    tx.queued_ops.push_back(TxOp::ValidateUniqueIndex {
                        store_id,
                        index_id: iid,
                    });
                }
                (iid, proto_h)
            };
            let idx_h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(index_proto)));
            {
                let mut st = db_r.borrow_mut();
                st.indexes.insert(
                    idx_h,
                    IdbIndexHandle {
                        tx_h,
                        store_h: sh,
                        store_id,
                        index_id,
                        index_name: idx_name,
                        original_name: None,
                        deleted: false,
                        cached_key_path: None,
                        cached_unique: unique,
                        cached_multi_entry: multi_entry,
                    },
                );
                if let Some(tx) = st.transactions.get_mut(&tx_h) {
                    tx.index_handles.insert((store_id, index_id), idx_h);
                }
            }
            if unique {
                pump_tx_ops(&h_r, &ev_r, &el_r, &wa_r, &db_r, tx_h);
            }
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
            let (tx_h, store_id, deleted) = {
                let s = st
                    .stores
                    .get(&sh)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                (s.tx_h, s.store_id, s.deleted)
            };
            let (removed_ih,) = {
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
                if deleted {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Object store has been deleted",
                    ));
                }
                if tx.state != TxState::Active || tx.aborted || tx.aborting {
                    return Err(create_dom_exception(
                        vm,
                        "TransactionInactiveError",
                        "Transaction is not active",
                    ));
                }
                let store = tx.store_snap_mut(store_id).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Object store has been deleted")
                })?;
                if store.indexes.remove(&idx_name).is_none() {
                    return Err(create_dom_exception(
                        vm,
                        "NotFoundError",
                        &format!("Index '{idx_name}' not found"),
                    ));
                }
                let iid = tx.index_id_by_name.remove(&(store_id, idx_name));
                let ih = iid.and_then(|id| {
                    tx.name_by_index_id.remove(&(store_id, id));
                    tx.index_handles.get(&(store_id, id)).copied()
                });
                (ih,)
            };
            if let Some(ih) = removed_ih {
                if let Some(i) = st.indexes.get_mut(&ih) {
                    i.deleted = true;
                }
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
            let lookup = {
                let st = db_r.borrow();
                let s = st
                    .stores
                    .get(&sh)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                if s.deleted {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Object store has been deleted",
                    ));
                }
                let tx = st.transactions.get(&s.tx_h).ok_or_else(|| {
                    create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Transaction has already finished",
                    )
                })?;
                if tx.is_finished() || tx.aborting {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Transaction has already finished",
                    ));
                }
                let store = tx.store_snap(s.store_id).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Object store has been deleted")
                })?;
                let schema = store.indexes.get(&idx_name).ok_or_else(|| {
                    create_dom_exception(
                        vm,
                        "NotFoundError",
                        &format!("Index '{idx_name}' not found"),
                    )
                })?;
                let Some(&iid) = tx.index_id_by_name.get(&(s.store_id, idx_name.clone())) else {
                    return Err(create_dom_exception(
                        vm,
                        "NotFoundError",
                        &format!("Index '{idx_name}' not found"),
                    ));
                };
                if let Some(&existing_ih) = tx.index_handles.get(&(s.store_id, iid)) {
                    return Ok(JsValue::from_object(existing_ih));
                }
                let orig_name = if tx.initial_index_ids.values().any(|&id| id == iid) {
                    Some(
                        tx.initial_index_ids
                            .iter()
                            .find_map(|((sid, n), &id)| {
                                (*sid == s.store_id && id == iid).then(|| n.clone())
                            })
                            .unwrap_or_else(|| idx_name.clone()),
                    )
                } else if tx.mode != TxMode::VersionChange {
                    Some(idx_name.clone())
                } else {
                    None
                };
                (
                    s.tx_h,
                    s.store_id,
                    iid,
                    orig_name,
                    schema.unique,
                    schema.multi_entry,
                    st.protos.index_proto.unwrap_or(vm.object_proto),
                )
            };
            let (tx_h, store_id, index_id, original_name, unique, multi_entry, index_proto) =
                lookup;
            let idx_h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(index_proto)));
            {
                let mut st = db_r.borrow_mut();
                st.indexes.insert(
                    idx_h,
                    IdbIndexHandle {
                        tx_h,
                        store_h: sh,
                        store_id,
                        index_id,
                        index_name: idx_name,
                        original_name,
                        deleted: false,
                        cached_key_path: None,
                        cached_unique: unique,
                        cached_multi_entry: multi_entry,
                    },
                );
                if let Some(tx) = st.transactions.get_mut(&tx_h) {
                    tx.index_handles.insert((store_id, index_id), idx_h);
                }
            }
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
            let (tx_h, store_id) = check_store_for_request(vm, &db_r, sh, false)?;
            let range = query_to_key_range(vm, &db_r, arg(args, 0))?;
            let dir = parse_cursor_direction(vm, arg(args, 1))?;
            let req_h = open_cursor_common(
                vm, &h_r, &ev_r, &el_r, &wa_r, &db_r, tx_h, sh, store_id, None, range, dir,
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
                let n = if t.mode == TxMode::VersionChange {
                    t.working.stores.keys().cloned().collect()
                } else {
                    t.scope.clone()
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
            let lookup = {
                let st = db.borrow();
                let tx = st
                    .transactions
                    .get(&tx_h)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                if tx.is_finished() || tx.aborting {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Transaction has already finished",
                    ));
                }
                let in_scope =
                    tx.mode == TxMode::VersionChange || tx.scope.iter().any(|s| s == &name);
                if !in_scope {
                    return Err(create_dom_exception(
                        vm,
                        "NotFoundError",
                        &format!("Object store '{name}' not in transaction scope"),
                    ));
                }
                let Some(&store_id) = tx.store_id_by_name.get(&name) else {
                    return Err(create_dom_exception(
                        vm,
                        "NotFoundError",
                        &format!("Object store '{name}' not in transaction scope"),
                    ));
                };
                if let Some(&existing_sh) = tx.store_handles.get(&store_id) {
                    return Ok(JsValue::from_object(existing_sh));
                }
                let auto_inc = tx
                    .working
                    .stores
                    .get(&name)
                    .is_some_and(|s| s.auto_increment);
                let orig_name = if tx.mode != TxMode::VersionChange {
                    Some(name.clone())
                } else {
                    tx.initial_store_ids
                        .iter()
                        .find_map(|(n, &id)| (id == store_id).then(|| n.clone()))
                };
                (
                    store_id,
                    orig_name,
                    auto_inc,
                    st.protos.object_store_proto.unwrap_or(vm.object_proto),
                )
            };
            let (store_id, original_name, auto_inc, store_proto) = lookup;
            let store_h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(store_proto)));
            {
                let mut st = db.borrow_mut();
                st.stores.insert(
                    store_h,
                    IdbStoreHandle {
                        tx_h,
                        store_id,
                        name,
                        original_name,
                        deleted: false,
                        cached_key_path: None,
                        cached_auto_increment: auto_inc,
                    },
                );
                if let Some(tx) = st.transactions.get_mut(&tx_h) {
                    tx.store_handles.insert(store_id, store_h);
                }
            }
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
                let st = db.borrow();
                let Some(tx) = st.transactions.get(&tx_h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if tx.is_finished() || tx.state == TxState::Committing || tx.aborted || tx.aborting
                {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Cannot abort a committing, finished, or already aborted transaction",
                    ));
                }
            }
            begin_tx_abort(&h_r, &ev_r, &el_r, &wa_r, &db, tx_h, JsValue::NULL);
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
            let should_kick_commit = {
                let mut st = db.borrow_mut();
                let Some(tx) = st.transactions.get_mut(&tx_h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if tx.state != TxState::Active || tx.aborted || tx.aborting {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Cannot commit an inactive, finished, or aborted transaction",
                    ));
                }
                tx.state = TxState::Committing;
                tx.explicit_commit = true;
                tx.started && !tx.in_flight_op && tx.queued_ops.is_empty()
            };
            if should_kick_commit {
                schedule_tx_commit(&h_r, &ev_r, &el_r, &wa_r, &db, tx_h);
            }
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
            if !db.borrow().databases.contains_key(&db_h) {
                return Err(vm.type_error("Illegal invocation"));
            }
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
                let kp = parse_key_path_raw(vm, kp_val, true)?;
                (kp, ai)
            } else {
                (None, false)
            };

            // W3C IDB §4.3 exact exception order:
            // VersionChange (`InvalidStateError`) -> Active (`TransactionInactiveError`)
            // -> keyPath syntax (`SyntaxError`) -> duplicate (`ConstraintError`) -> autoIncrement (`InvalidAccessError`).
            let vc_tx = {
                let st = db.borrow();
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
                let tx = st.transactions.get(&vc_tx).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Transaction inactive")
                })?;
                if tx.state != TxState::Active || tx.aborted || tx.aborting {
                    return Err(create_dom_exception(
                        vm,
                        "TransactionInactiveError",
                        "Version change transaction is not active",
                    ));
                }
                vc_tx
            };

            let (key_path, key_path_is_array) = match key_path_pair {
                Some((v, is_arr)) => {
                    validate_key_path_syntax(vm, &v, is_arr)?;
                    (Some(v), is_arr)
                }
                None => (None, false),
            };

            let (store_id, store_proto) = {
                let mut st = db.borrow_mut();
                let sid = st.alloc_meta_id();
                let proto_h = st.protos.object_store_proto.unwrap_or(vm.object_proto);
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
                let mut snap_store = IdbStoreSnapshot::new(name.clone(), key_path, auto_increment);
                snap_store.key_path_is_array = key_path_is_array;
                tx.working.stores.insert(name.clone(), snap_store);
                if !tx.scope.contains(&name) {
                    tx.scope.push(name.clone());
                }
                tx.store_id_by_name.insert(name.clone(), sid);
                tx.name_by_store_id.insert(sid, name.clone());
                (sid, proto_h)
            };
            let store_h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(store_proto)));
            {
                let mut st = db.borrow_mut();
                st.stores.insert(
                    store_h,
                    IdbStoreHandle {
                        tx_h: vc_tx,
                        store_id,
                        name,
                        original_name: None,
                        deleted: false,
                        cached_key_path: None,
                        cached_auto_increment: auto_increment,
                    },
                );
                if let Some(tx) = st.transactions.get_mut(&vc_tx) {
                    tx.store_handles.insert(store_id, store_h);
                }
            }
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
            if !db.borrow().databases.contains_key(&db_h) {
                return Err(vm.type_error("Illegal invocation"));
            }
            if args.is_empty() {
                return Err(vm.type_error("IDBDatabase.deleteObjectStore requires 1 argument"));
            }
            let name = vm.to_js_string(arg(args, 0))?;
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
            let (removed_sh, removed_ihs) = {
                let tx = st.transactions.get_mut(&vc_tx).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "Transaction inactive")
                })?;
                if tx.state != TxState::Active || tx.aborted || tx.aborting {
                    return Err(create_dom_exception(
                        vm,
                        "TransactionInactiveError",
                        "Version change transaction is not active",
                    ));
                }
                let Some(removed_store) = tx.working.stores.remove(&name) else {
                    return Err(create_dom_exception(
                        vm,
                        "NotFoundError",
                        &format!("Object store '{name}' does not exist"),
                    ));
                };
                tx.scope.retain(|s| s != &name);
                let sid = tx.store_id_by_name.remove(&name);
                if let Some(id) = sid {
                    tx.deleted_stores.insert(id, removed_store);
                }
                let sh = sid.and_then(|id| {
                    tx.name_by_store_id.remove(&id);
                    tx.store_handles.get(&id).copied()
                });
                let ihs: Vec<HeapHandle> = sid
                    .map(|id| {
                        tx.index_handles
                            .iter()
                            .filter_map(|(&(s, _), &ih)| (s == id).then_some(ih))
                            .collect()
                    })
                    .unwrap_or_default();
                (sh, ihs)
            };
            if let Some(sh) = removed_sh {
                if let Some(s) = st.stores.get_mut(&sh) {
                    s.deleted = true;
                }
            }
            for ih in removed_ihs {
                if let Some(i) = st.indexes.get_mut(&ih) {
                    i.deleted = true;
                }
            }
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
            let mode_str = if arg(args, 1).is_undefined() {
                "readonly".to_string()
            } else {
                let ms = vm.to_js_string(arg(args, 1))?;
                // WebIDL enum `IDBTransactionMode` accepts `"readonly" | "readwrite" | "versionchange"`.
                if !matches!(ms.as_str(), "readonly" | "readwrite" | "versionchange") {
                    return Err(vm.type_error(format!("Invalid transaction mode '{ms}'")));
                }
                ms
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

            let (origin, db_name, version, schema_stores, tx_proto) = {
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
                for sname in &scope {
                    if !d.schema_stores.contains_key(sname) {
                        return Err(create_dom_exception(
                            vm,
                            "NotFoundError",
                            &format!("Object store '{sname}' does not exist"),
                        ));
                    }
                }
                if scope.is_empty() {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidAccessError",
                        "Transaction storeNames must not be empty",
                    ));
                }
                (
                    d.origin.clone(),
                    d.name.clone(),
                    d.version,
                    d.schema_stores.clone(),
                    st.protos.transaction_proto.unwrap_or(vm.object_proto),
                )
            };

            let mode = match mode_str.as_str() {
                "readonly" => TxMode::ReadOnly,
                "readwrite" => TxMode::ReadWrite,
                other => {
                    return Err(vm.type_error(format!("Invalid transaction mode '{other}'")));
                }
            };

            let mut working = IdbDatabaseSnapshot::new(db_name.clone(), version);
            working.stores = schema_stores;

            let tx_h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(tx_proto)));
            ev_r.borrow_mut().set_object_event_parent(tx_h, Some(db_h));
            {
                let mut st = db.borrow_mut();
                let mut tx_state = IdbTransactionState::new(
                    db_h,
                    origin.clone(),
                    db_name.clone(),
                    mode,
                    durability,
                    scope,
                    working,
                    version,
                    false,
                    None,
                    None,
                );
                let mut next_id = st.next_meta_id.max(1);
                tx_state.init_meta_ids(&mut next_id);
                st.next_meta_id = next_id;
                st.transactions.insert(tx_h, tx_state);
                st.coords
                    .entry((origin.clone(), db_name.clone()))
                    .or_default()
                    .tx_queue
                    .push_back(tx_h);
            }
            pump_db_tx_queue(&h_r, &ev_r, &el_r, &wa_r, &db, &origin, &db_name);
            Ok(JsValue::from_object(tx_h))
        });
    }

    // db.close()
    {
        let h_r = Rc::clone(host);
        let ev_r = Rc::clone(events);
        let el_r = Rc::clone(event_loop);
        let wa_r = Rc::clone(web_apis);
        let db = Rc::clone(idb);
        define_host_method(vm, proto, "close", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let db_key = {
                let mut st = db.borrow_mut();
                let Some(d) = st.databases.get_mut(&h) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                d.closed = true;
                let key = (d.origin.clone(), d.name.clone());
                if let Some(coord) = st.coords.get_mut(&key) {
                    coord.open_connections.retain(|&dh| dh != h);
                }
                key
            };
            schedule_factory_pump(&h_r, &ev_r, &el_r, &wa_r, &db, &db_key.0, &db_key.1);
            Ok(JsValue::UNDEFINED)
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
            let origin = idb_effective_origin(vm, &wa_r, &db_r);
            if origin == "null" {
                return Err(idb_err(
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
            db_r.borrow_mut()
                .coords
                .entry((origin.clone(), db_name.clone()))
                .or_default()
                .open_delete_queue
                .push_back(PendingFactoryOp {
                    is_delete: false,
                    req_h,
                    requested_version,
                    vc_sent: false,
                    blocked_sent: false,
                });
            schedule_factory_pump(&h_r, &ev_r, &el_r, &wa_r, &db_r, &origin, &db_name);
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
            let origin = idb_effective_origin(vm, &wa_r, &db_r);
            if origin == "null" {
                return Err(idb_err(
                    vm,
                    "SecurityError",
                    "IndexedDB access is denied in an opaque origin",
                ));
            }
            let db_name = vm.to_js_string(arg(args, 0))?;
            let req_h = alloc_request(vm, &ev_r, &db_r, true, JsValue::NULL, JsValue::NULL);
            db_r.borrow_mut()
                .coords
                .entry((origin.clone(), db_name.clone()))
                .or_default()
                .open_delete_queue
                .push_back(PendingFactoryOp {
                    is_delete: true,
                    req_h,
                    requested_version: None,
                    vc_sent: false,
                    blocked_sent: false,
                });
            schedule_factory_pump(&h_r, &ev_r, &el_r, &wa_r, &db_r, &origin, &db_name);
            Ok(JsValue::from_object(req_h))
        });
    }

    // indexedDB.databases() -> Promise<Array<{name, version}>>
    {
        let wa_r = Rc::clone(web_apis);
        let db_r = Rc::clone(idb);
        define_host_method(vm, proto, "databases", move |vm, _, _| {
            let p = alloc_promise(vm);
            let origin = idb_effective_origin(vm, &wa_r, &db_r);
            if origin == "null" {
                let err = idb_err(
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
