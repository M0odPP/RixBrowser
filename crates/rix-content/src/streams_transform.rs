//! WHATWG Streams Standard §6 (`TransformStream`, `TransformStreamDefaultController`)
//! with exact backpressure synchronization (`backpressureChangePromise`), `start`/`transform`/
//! `flush`/`cancel`, `controller.desiredSize`/`enqueue`/`error`/`terminate`, and full
//! `writableStrategy`/`readableStrategy` support.

use std::rc::Rc;

use rix_js::promise::{
    alloc_promise, promise_resolve_value, promise_then, reject_promise, resolve_promise,
};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind};

use crate::streams::{
    alloc_readable_stream_internal, readable_controller_close, readable_controller_enqueue,
    readable_error_internal, ReadableState, SharedStreams,
};
use crate::streams_common::{
    attach_gc_anchor, call_returning_promise, extract_strategy, get_method_checked,
    promise_mark_handled, promise_reject_value, set_to_string_tag, validate_and_normalize_hwm,
};
use crate::streams_writable::{
    create_writable_stream_with_abort_and_start, writable_start_erroring, WritableState,
};
use crate::web_apis::{
    arg, bind_ctor_proto, define_global, define_host_getter, define_host_method, SharedWebApis,
};

#[derive(Debug, Clone)]
pub(crate) struct TransformStreamState {
    pub(crate) readable: HeapHandle,
    pub(crate) writable: HeapHandle,
    pub(crate) controller: HeapHandle,
    pub(crate) backpressure: bool,
    pub(crate) backpressure_change_promise: Option<HeapHandle>,
    pub(crate) transform_fn: Option<HeapHandle>,
    pub(crate) flush_fn: Option<HeapHandle>,
    pub(crate) cancel_fn: Option<HeapHandle>,
    pub(crate) transformer_obj: Option<HeapHandle>,
    pub(crate) start_promise: Option<HeapHandle>,
    pub(crate) finish_promise: Option<HeapHandle>,
}

impl TransformStreamState {
    pub(crate) fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for h in [
            Some(self.readable),
            Some(self.writable),
            Some(self.controller),
            self.backpressure_change_promise,
            self.transform_fn,
            self.flush_fn,
            self.cancel_fn,
            self.transformer_obj,
            self.start_promise,
            self.finish_promise,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(h));
        }
    }

    fn clear_algorithms(&mut self) {
        self.transform_fn = None;
        self.flush_fn = None;
        self.cancel_fn = None;
    }
}

fn transform_set_backpressure(
    vm: &mut JsVm,
    streams: &SharedStreams,
    ts_h: HeapHandle,
    new_bp: bool,
) {
    let old_p = {
        let mut reg = streams.borrow_mut();
        let Some(ts) = reg.transforms.get_mut(&ts_h) else {
            return;
        };
        let old = ts.backpressure_change_promise.take();
        let fresh = alloc_promise(vm);
        ts.backpressure_change_promise = Some(fresh);
        ts.backpressure = new_bp;
        old
    };
    if let Some(p) = old_p {
        resolve_promise(vm, p, JsValue::UNDEFINED);
    }
}

fn transform_unblock_write(vm: &mut JsVm, streams: &SharedStreams, ts_h: HeapHandle) {
    let is_bp = streams
        .borrow()
        .transforms
        .get(&ts_h)
        .is_some_and(|ts| ts.backpressure);
    if is_bp {
        transform_set_backpressure(vm, streams, ts_h, false);
    }
}

fn transform_error_writable_and_unblock_write(
    vm: &mut JsVm,
    streams: &SharedStreams,
    ts_h: HeapHandle,
    e: JsValue,
) {
    let writable_h = {
        let mut reg = streams.borrow_mut();
        let Some(ts) = reg.transforms.get_mut(&ts_h) else {
            return;
        };
        ts.clear_algorithms();
        ts.writable
    };
    writable_start_erroring(vm, streams, writable_h, e);
    transform_unblock_write(vm, streams, ts_h);
}

fn transform_error(vm: &mut JsVm, streams: &SharedStreams, ts_h: HeapHandle, e: JsValue) {
    let readable_h = match streams.borrow().transforms.get(&ts_h) {
        Some(ts) => ts.readable,
        None => return,
    };
    readable_error_internal(vm, streams, readable_h, e);
    transform_error_writable_and_unblock_write(vm, streams, ts_h, e);
}

fn transform_controller_enqueue(
    vm: &mut JsVm,
    streams: &SharedStreams,
    ts_h: HeapHandle,
    chunk: JsValue,
) -> Result<(), JsException> {
    let (readable_h, can_enqueue) = {
        let reg = streams.borrow();
        let rh = reg
            .transforms
            .get(&ts_h)
            .map(|ts| ts.readable)
            .ok_or_else(|| vm.type_error("Invalid TransformStreamDefaultController"))?;
        let ok = reg
            .readable
            .get(&rh)
            .is_some_and(|rs| rs.can_close_or_enqueue());
        (rh, ok)
    };
    // WHATWG §6.4.4 step 3: if readable cannot close or enqueue, throw TypeError
    // WITHOUT erroring the writable side.
    if !can_enqueue {
        return Err(vm.type_error("Cannot enqueue into a closed or errored TransformStream"));
    }
    if let Err(e) = readable_controller_enqueue(vm, streams, readable_h, chunk) {
        transform_error_writable_and_unblock_write(vm, streams, ts_h, e.value);
        // WHATWG §6.4.4 step 5b: rethrow `stream.[[readable]].[[storedError]]` (first error).
        let stored = streams
            .borrow()
            .readable
            .get(&readable_h)
            .map_or(e.value, |rs| rs.stored_error);
        let msg = vm.value_to_rust_string(stored);
        return Err(JsException {
            value: stored,
            message: msg,
        });
    }
    let (want_bp, cur_bp) = {
        let reg = streams.borrow();
        let ds = reg
            .readable
            .get(&readable_h)
            .and_then(|rs| rs.desired_size());
        let bp_now = reg.transforms.get(&ts_h).is_some_and(|ts| ts.backpressure);
        (!ds.is_some_and(|d| d > 0.0), bp_now)
    };
    if want_bp != cur_bp {
        transform_set_backpressure(vm, streams, ts_h, true);
    }
    Ok(())
}

fn transform_controller_terminate(vm: &mut JsVm, streams: &SharedStreams, ts_h: HeapHandle) {
    let readable_h = match streams.borrow().transforms.get(&ts_h) {
        Some(ts) => ts.readable,
        None => return,
    };
    let _ = readable_controller_close(vm, streams, readable_h);
    let err = vm.type_error("TransformStream has been terminated").value;
    transform_error_writable_and_unblock_write(vm, streams, ts_h, err);
}

fn transform_perform_transform(
    vm: &mut JsVm,
    streams: &SharedStreams,
    ts_h: HeapHandle,
    chunk: JsValue,
) -> HeapHandle {
    let info = {
        let reg = streams.borrow();
        let Some(ts) = reg.transforms.get(&ts_h) else {
            return promise_resolve_value(vm, JsValue::UNDEFINED);
        };
        (ts.transform_fn, ts.transformer_obj, ts.controller)
    };
    let p = if let Some(tf) = info.0 {
        let this_v = info.1.map_or(JsValue::UNDEFINED, JsValue::from_object);
        call_returning_promise(vm, tf, this_v, &[chunk, JsValue::from_object(info.2)])
    } else {
        match transform_controller_enqueue(vm, streams, ts_h, chunk) {
            Ok(()) => promise_resolve_value(vm, JsValue::UNDEFINED),
            Err(e) => promise_reject_value(vm, e.value),
        }
    };
    let ss_err = Rc::clone(streams);
    let on_err = vm.alloc_host_fn("ts_transform_err", move |vm, _, a| {
        let r = arg(a, 0);
        transform_error(vm, &ss_err, ts_h, r);
        let msg = vm.value_to_rust_string(r);
        Err(JsException {
            value: r,
            message: msg,
        })
    });
    promise_then(vm, p, None, Some(on_err))
}

fn require_transform_stream(
    vm: &mut JsVm,
    streams: &SharedStreams,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible TransformStream receiver"));
    };
    if streams.borrow().transforms.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible TransformStream receiver"))
    }
}

fn require_transform_controller(
    vm: &mut JsVm,
    streams: &SharedStreams,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible TransformStreamDefaultController receiver"));
    };
    streams
        .borrow()
        .transform_controllers
        .get(&h)
        .copied()
        .ok_or_else(|| vm.type_error("Incompatible TransformStreamDefaultController receiver"))
}

/// Installs `TransformStream` and `TransformStreamDefaultController`.
pub(crate) fn install_transform_stream_types(
    vm: &mut JsVm,
    streams: &SharedStreams,
    web_apis: &SharedWebApis,
) {
    // --- TransformStreamDefaultController ---
    let tcp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(tcp).unwrap().is_prototype = true;
    set_to_string_tag(vm, tcp, "TransformStreamDefaultController");
    streams.borrow_mut().transform_controller_proto = Some(tcp);

    let ss = Rc::clone(streams);
    define_host_getter(vm, tcp, "desiredSize", move |vm, this_val, _| {
        let ts_h = require_transform_controller(vm, &ss, this_val)?;
        let reg = ss.borrow();
        let readable_h = reg
            .transforms
            .get(&ts_h)
            .map(|ts| ts.readable)
            .ok_or_else(|| vm.type_error("Invalid TransformStreamDefaultController"))?;
        let ds = reg.readable.get(&readable_h).and_then(|r| r.desired_size());
        Ok(ds.map_or(JsValue::NULL, JsValue::from_f64))
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, tcp, "enqueue", move |vm, this_val, args| {
        let ts_h = require_transform_controller(vm, &ss, this_val)?;
        transform_controller_enqueue(vm, &ss, ts_h, arg(args, 0))?;
        Ok(JsValue::UNDEFINED)
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, tcp, "error", move |vm, this_val, args| {
        let ts_h = require_transform_controller(vm, &ss, this_val)?;
        transform_error(vm, &ss, ts_h, arg(args, 0));
        Ok(JsValue::UNDEFINED)
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, tcp, "terminate", move |vm, this_val, _| {
        let ts_h = require_transform_controller(vm, &ss, this_val)?;
        transform_controller_terminate(vm, &ss, ts_h);
        Ok(JsValue::UNDEFINED)
    });
    let tc_ctor = vm.alloc_host_fn("TransformStreamDefaultController", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: TransformStreamDefaultController"))
    });
    bind_ctor_proto(vm, tc_ctor, tcp);
    define_global(
        vm,
        "TransformStreamDefaultController",
        JsValue::from_object(tc_ctor),
    );

    // --- TransformStream ---
    let tsp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(tsp).unwrap().is_prototype = true;
    set_to_string_tag(vm, tsp, "TransformStream");
    streams.borrow_mut().transform_proto = Some(tsp);

    let ss = Rc::clone(streams);
    define_host_getter(vm, tsp, "readable", move |vm, this_val, _| {
        let ts_h = require_transform_stream(vm, &ss, this_val)?;
        let r = ss
            .borrow()
            .transforms
            .get(&ts_h)
            .map(|ts| ts.readable)
            .ok_or_else(|| vm.type_error("Incompatible TransformStream receiver"))?;
        Ok(JsValue::from_object(r))
    });
    let ss = Rc::clone(streams);
    define_host_getter(vm, tsp, "writable", move |vm, this_val, _| {
        let ts_h = require_transform_stream(vm, &ss, this_val)?;
        let w = ss
            .borrow()
            .transforms
            .get(&ts_h)
            .map(|ts| ts.writable)
            .ok_or_else(|| vm.type_error("Incompatible TransformStream receiver"))?;
        Ok(JsValue::from_object(w))
    });

    let ss = Rc::clone(streams);
    let wa = Rc::clone(web_apis);
    let ts_ctor =
        vm.alloc_host_fn("TransformStream", move |vm, _, args| {
            if vm.native_new_target.is_undefined() {
                return Err(vm.type_error("TransformStream constructor requires 'new'"));
            }
            let transformer_val = arg(args, 0);
            if !transformer_val.is_undefined() && !transformer_val.is_object() {
                return Err(vm.type_error("TransformStream transformer must be an object"));
            }
            // WebIDL converts writableStrategy & readableStrategy BEFORE transformer dict.
            let (ws_hwm_v, ws_size) = extract_strategy(vm, arg(args, 1))?;
            let (rs_hwm_v, rs_size) = extract_strategy(vm, arg(args, 2))?;

            let mut cancel_fn = None;
            let mut flush_fn = None;
            let mut start_fn = None;
            let mut transform_fn = None;
            let transformer_obj = transformer_val.as_object();
            if let Some(th) = transformer_obj {
                // Alphabetical WebIDL Transformer dictionary order:
                // `cancel`, `flush`, `readableType`, `start`, `transform`, `writableType`.
                cancel_fn = get_method_checked(vm, th, "cancel")?;
                flush_fn = get_method_checked(vm, th, "flush")?;
                let k_rt = vm.heap.intern_key("readableType");
                if !vm.get_property_by_key(th, k_rt)?.is_undefined() {
                    return Err(vm.range_error("TransformStream readableType is not supported"));
                }
                start_fn = get_method_checked(vm, th, "start")?;
                transform_fn = get_method_checked(vm, th, "transform")?;
                let k_wt = vm.heap.intern_key("writableType");
                if !vm.get_property_by_key(th, k_wt)?.is_undefined() {
                    return Err(vm.range_error("TransformStream writableType is not supported"));
                }
            }
            let ws_hwm = match ws_hwm_v {
                Some(v) => validate_and_normalize_hwm(vm, v)?,
                None => 1.0,
            };
            let rs_hwm = match rs_hwm_v {
                Some(v) => validate_and_normalize_hwm(vm, v)?,
                None => 0.0,
            };

            let ts_obj = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(tsp)));
            let ctrl = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(tcp)));
            let start_p = alloc_promise(vm);

            // Readable side pull & cancel callbacks
            let ss_rp = Rc::clone(&ss);
            let r_pull = vm.alloc_host_fn("ts_r_pull", move |vm, _, _| {
                transform_set_backpressure(vm, &ss_rp, ts_obj, false);
                let bp_p = ss_rp
                    .borrow()
                    .transforms
                    .get(&ts_obj)
                    .and_then(|ts| ts.backpressure_change_promise)
                    .unwrap_or_else(|| promise_resolve_value(vm, JsValue::UNDEFINED));
                Ok(JsValue::from_object(bp_p))
            });
            let ss_rc = Rc::clone(&ss);
            let r_cancel = vm.alloc_host_fn("ts_r_cancel", move |vm, _, a| {
                let reason = arg(a, 0);
                let existing = ss_rc
                    .borrow()
                    .transforms
                    .get(&ts_obj)
                    .and_then(|ts| ts.finish_promise);
                if let Some(fp) = existing {
                    return Ok(JsValue::from_object(fp));
                }
                let fp = alloc_promise(vm);
                let (cf, tobj, writable_h) = {
                    let mut reg = ss_rc.borrow_mut();
                    let Some(ts) = reg.transforms.get_mut(&ts_obj) else {
                        resolve_promise(vm, fp, JsValue::UNDEFINED);
                        return Ok(JsValue::from_object(fp));
                    };
                    ts.finish_promise = Some(fp);
                    let cf = ts.cancel_fn;
                    let tobj = ts.transformer_obj;
                    let wh = ts.writable;
                    ts.clear_algorithms();
                    (cf, tobj, wh)
                };
                let cancel_p = if let Some(cf_h) = cf {
                    let this_v = tobj.map_or(JsValue::UNDEFINED, JsValue::from_object);
                    match vm.call_value(JsValue::from_object(cf_h), this_v, &[reason]) {
                        Ok(v) => {
                            let p = alloc_promise(vm);
                            resolve_promise(vm, p, v);
                            p
                        }
                        Err(e) => promise_reject_value(vm, e.value),
                    }
                } else {
                    promise_resolve_value(vm, JsValue::UNDEFINED)
                };
                let ss_ok = Rc::clone(&ss_rc);
                let on_ok = vm.alloc_host_fn("ts_rc_ok", move |vm, _, _| {
                    let err_state = ss_ok.borrow().writable.get(&writable_h).and_then(|ws| {
                        (ws.state == WritableState::Errored).then_some(ws.stored_error)
                    });
                    if let Some(e) = err_state {
                        reject_promise(vm, fp, e);
                    } else {
                        transform_error_writable_and_unblock_write(vm, &ss_ok, ts_obj, reason);
                        resolve_promise(vm, fp, JsValue::UNDEFINED);
                    }
                    Ok(JsValue::UNDEFINED)
                });
                let ss_err = Rc::clone(&ss_rc);
                let on_err = vm.alloc_host_fn("ts_rc_err", move |vm, _, ea| {
                    let r = arg(ea, 0);
                    transform_error_writable_and_unblock_write(vm, &ss_err, ts_obj, r);
                    reject_promise(vm, fp, r);
                    Ok(JsValue::UNDEFINED)
                });
                let _ = promise_then(vm, cancel_p, Some(on_ok), Some(on_err));
                Ok(JsValue::from_object(fp))
            });

            let (readable, _) = alloc_readable_stream_internal(
                vm,
                &ss,
                rs_hwm,
                rs_size,
                Some(r_pull),
                Some(r_cancel),
                None,
                false,
            );

            // Writable side write, close, abort callbacks
            let ss_ww = Rc::clone(&ss);
            let w_write = vm.alloc_host_fn("ts_w_write", move |vm, _, a| {
                let chunk = arg(a, 0);
                let (is_bp, bp_p_opt, writable_h) = {
                    let reg = ss_ww.borrow();
                    let Some(ts) = reg.transforms.get(&ts_obj) else {
                        return Ok(JsValue::UNDEFINED);
                    };
                    (ts.backpressure, ts.backpressure_change_promise, ts.writable)
                };
                if is_bp {
                    if let Some(bp_p) = bp_p_opt {
                        let ss_cont = Rc::clone(&ss_ww);
                        let on_unblock = vm.alloc_host_fn("ts_w_unblock", move |vm, _, _| {
                            let erroring =
                                ss_cont.borrow().writable.get(&writable_h).and_then(|ws| {
                                    (ws.state == WritableState::Erroring).then_some(ws.stored_error)
                                });
                            if let Some(e) = erroring {
                                return Ok(JsValue::from_object(promise_reject_value(vm, e)));
                            }
                            Ok(JsValue::from_object(transform_perform_transform(
                                vm, &ss_cont, ts_obj, chunk,
                            )))
                        });
                        return Ok(JsValue::from_object(promise_then(
                            vm,
                            bp_p,
                            Some(on_unblock),
                            None,
                        )));
                    }
                }
                Ok(JsValue::from_object(transform_perform_transform(
                    vm, &ss_ww, ts_obj, chunk,
                )))
            });

            let ss_wcl = Rc::clone(&ss);
            let w_close = vm.alloc_host_fn("ts_w_close", move |vm, _, _| {
                let existing = ss_wcl
                    .borrow()
                    .transforms
                    .get(&ts_obj)
                    .and_then(|ts| ts.finish_promise);
                if let Some(fp) = existing {
                    return Ok(JsValue::from_object(fp));
                }
                let fp = alloc_promise(vm);
                let (ff, tobj, readable_h) = {
                    let mut reg = ss_wcl.borrow_mut();
                    let Some(ts) = reg.transforms.get_mut(&ts_obj) else {
                        resolve_promise(vm, fp, JsValue::UNDEFINED);
                        return Ok(JsValue::from_object(fp));
                    };
                    ts.finish_promise = Some(fp);
                    let ff = ts.flush_fn;
                    let tobj = ts.transformer_obj;
                    let rh = ts.readable;
                    ts.clear_algorithms();
                    (ff, tobj, rh)
                };
                let flush_p = if let Some(ff_h) = ff {
                    let this_v = tobj.map_or(JsValue::UNDEFINED, JsValue::from_object);
                    call_returning_promise(vm, ff_h, this_v, &[JsValue::from_object(ctrl)])
                } else {
                    promise_resolve_value(vm, JsValue::UNDEFINED)
                };
                let ss_ok = Rc::clone(&ss_wcl);
                let on_ok = vm.alloc_host_fn("ts_flush_ok", move |vm, _, _| {
                    let r_err = ss_ok.borrow().readable.get(&readable_h).and_then(|rs| {
                        (rs.state == ReadableState::Errored).then_some(rs.stored_error)
                    });
                    if let Some(e) = r_err {
                        reject_promise(vm, fp, e);
                    } else {
                        let _ = readable_controller_close(vm, &ss_ok, readable_h);
                        resolve_promise(vm, fp, JsValue::UNDEFINED);
                    }
                    Ok(JsValue::UNDEFINED)
                });
                let ss_err = Rc::clone(&ss_wcl);
                let on_err = vm.alloc_host_fn("ts_flush_err", move |vm, _, a| {
                    let r = arg(a, 0);
                    transform_error(vm, &ss_err, ts_obj, r);
                    reject_promise(vm, fp, r);
                    Ok(JsValue::UNDEFINED)
                });
                let _ = promise_then(vm, flush_p, Some(on_ok), Some(on_err));
                Ok(JsValue::from_object(fp))
            });

            let ss_wab = Rc::clone(&ss);
            let w_abort = vm.alloc_host_fn("ts_w_abort", move |vm, _, a| {
                let reason = arg(a, 0);
                let existing = ss_wab
                    .borrow()
                    .transforms
                    .get(&ts_obj)
                    .and_then(|ts| ts.finish_promise);
                if let Some(fp) = existing {
                    return Ok(JsValue::from_object(fp));
                }
                let fp = alloc_promise(vm);
                let (cf, tobj, readable_h) = {
                    let mut reg = ss_wab.borrow_mut();
                    let Some(ts) = reg.transforms.get_mut(&ts_obj) else {
                        resolve_promise(vm, fp, JsValue::UNDEFINED);
                        return Ok(JsValue::from_object(fp));
                    };
                    ts.finish_promise = Some(fp);
                    let cf = ts.cancel_fn;
                    let tobj = ts.transformer_obj;
                    let rh = ts.readable;
                    ts.clear_algorithms();
                    (cf, tobj, rh)
                };
                let cancel_p = if let Some(cf_h) = cf {
                    let this_v = tobj.map_or(JsValue::UNDEFINED, JsValue::from_object);
                    match vm.call_value(JsValue::from_object(cf_h), this_v, &[reason]) {
                        Ok(v) => {
                            let p = alloc_promise(vm);
                            resolve_promise(vm, p, v);
                            p
                        }
                        Err(e) => promise_reject_value(vm, e.value),
                    }
                } else {
                    promise_resolve_value(vm, JsValue::UNDEFINED)
                };
                let ss_ok = Rc::clone(&ss_wab);
                let on_ok = vm.alloc_host_fn("ts_wab_ok", move |vm, _, _| {
                    let r_err = ss_ok.borrow().readable.get(&readable_h).and_then(|rs| {
                        (rs.state == ReadableState::Errored).then_some(rs.stored_error)
                    });
                    if let Some(e) = r_err {
                        reject_promise(vm, fp, e);
                    } else {
                        transform_error(vm, &ss_ok, ts_obj, reason);
                        resolve_promise(vm, fp, JsValue::UNDEFINED);
                    }
                    Ok(JsValue::UNDEFINED)
                });
                let ss_err = Rc::clone(&ss_wab);
                let on_err = vm.alloc_host_fn("ts_wab_err", move |vm, _, ea| {
                    let r = arg(ea, 0);
                    transform_error(vm, &ss_err, ts_obj, r);
                    reject_promise(vm, fp, r);
                    Ok(JsValue::UNDEFINED)
                });
                let _ = promise_then(vm, cancel_p, Some(on_ok), Some(on_err));
                Ok(JsValue::from_object(fp))
            });

            let writable = create_writable_stream_with_abort_and_start(
                vm, &ss, &wa, w_write, w_close, w_abort, ws_hwm, ws_size, start_p,
            );

            // Hook readable start to `start_p`.
            {
                let ss_rs1 = Rc::clone(&ss);
                let on_rs_ok = vm.alloc_host_fn("ts_rs_start_ok", move |vm, _, _| {
                    if let Some(rs) = ss_rs1.borrow_mut().readable.get_mut(&readable) {
                        rs.started = true;
                    }
                    crate::streams::readable_call_pull_if_needed(vm, &ss_rs1, readable);
                    Ok(JsValue::UNDEFINED)
                });
                let ss_rs2 = Rc::clone(&ss);
                let on_rs_err = vm.alloc_host_fn("ts_rs_start_err", move |vm, _, a| {
                    readable_error_internal(vm, &ss_rs2, readable, arg(a, 0));
                    Ok(JsValue::UNDEFINED)
                });
                let _ = promise_then(vm, start_p, Some(on_rs_ok), Some(on_rs_err));
            }

            // Register TransformStream state and initial backpressure = true (per WHATWG §6.3.1).
            let ts_state = TransformStreamState {
                readable,
                writable,
                controller: ctrl,
                backpressure: false,
                backpressure_change_promise: None,
                transform_fn,
                flush_fn,
                cancel_fn,
                transformer_obj,
                start_promise: Some(start_p),
                finish_promise: None,
            };
            attach_gc_anchor(vm, ctrl, ts_obj);
            attach_gc_anchor(vm, readable, ts_obj);
            attach_gc_anchor(vm, writable, ts_obj);
            {
                let mut reg = ss.borrow_mut();
                reg.transforms.insert(ts_obj, ts_state);
                reg.transform_controllers.insert(ctrl, ts_obj);
            }
            transform_set_backpressure(vm, &ss, ts_obj, true);

            // Invoke `transformer.start(controller)` synchronously and settle `start_p`!
            let start_res = if let Some(sf) = start_fn {
                match vm.call_value(
                    JsValue::from_object(sf),
                    transformer_val,
                    &[JsValue::from_object(ctrl)],
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        reject_promise(vm, start_p, e.value);
                        promise_mark_handled(vm, start_p);
                        return Err(e);
                    }
                }
            } else {
                JsValue::UNDEFINED
            };
            let start_res_p = promise_resolve_value(vm, start_res);
            resolve_promise(vm, start_p, JsValue::from_object(start_res_p));
            Ok(JsValue::from_object(ts_obj))
        });
    bind_ctor_proto(vm, ts_ctor, tsp);
    define_global(vm, "TransformStream", JsValue::from_object(ts_ctor));
}
