//! ES2024 `Promise`, FIFO microtask job queue, `async`/`await` frame suspension,
//! and `function*` `Generator` execution (`yield` / `yield*`).
//!
//! Reference: Spec 05 §6 & Issue #15.

use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::heap::{
    AsyncGeneratorRequest, AsyncGeneratorRequestKind, GeneratorObject, GeneratorState, JsObject,
    ObjectKind, PromiseReaction, PromiseState, SuspendedFrame,
};
use crate::shape::PropertyAttrs;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm, NativeFnPtr, RunOutcome};

/// Queued ECMAScript microtask job executed FIFO at every microtask checkpoint.
#[derive(Debug, Clone)]
pub enum MicrotaskJob {
    Callback {
        func: HeapHandle,
        this_val: JsValue,
        args: Vec<JsValue>,
    },
    PromiseReaction {
        reaction: PromiseReaction,
        is_rejected: bool,
        value: JsValue,
    },
    AsyncResume {
        frame: SuspendedFrame,
        promise: HeapHandle,
        value: JsValue,
        is_throw: bool,
    },
}

impl MicrotaskJob {
    pub fn trace_roots(&self, out: &mut Vec<JsValue>) {
        match self {
            Self::Callback {
                func,
                this_val,
                args,
            } => {
                out.push(JsValue::from_object(*func));
                out.push(*this_val);
                out.extend_from_slice(args);
            }
            Self::PromiseReaction {
                reaction, value, ..
            } => {
                if let Some(h) = reaction.on_fulfilled {
                    out.push(JsValue::from_object(h));
                }
                if let Some(h) = reaction.on_rejected {
                    out.push(JsValue::from_object(h));
                }
                if let Some(h) = reaction.result_promise {
                    out.push(JsValue::from_object(h));
                }
                out.push(*value);
            }
            Self::AsyncResume {
                frame,
                promise,
                value,
                ..
            } => {
                frame.trace_children(|h| out.push(JsValue::from_object(h)));
                out.push(JsValue::from_object(*promise));
                out.push(*value);
            }
        }
    }
}

/// Action dispatched into a `Generator` (`next`, `return`, or `throw`).
#[derive(Debug, Clone, Copy)]
pub enum GeneratorAction {
    Next(JsValue),
    Return(JsValue),
    Throw(JsValue),
}

fn define_method(vm: &mut JsVm, target: HeapHandle, name: &str, f: NativeFnPtr) {
    let key = vm.heap.intern_key(name);
    let func = vm.alloc_native_fn(name, f);
    vm.heap.object_define_data(
        target,
        key,
        JsValue::from_object(func),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
}

fn define_global(vm: &mut JsVm, name: &str, val: JsValue) {
    let key = vm.heap.intern_key(name);
    let g = vm.global;
    vm.heap
        .object_define_data(g, key, val, PropertyAttrs::METHOD, &mut vm.shapes);
}

#[inline]
fn arg(args: &[JsValue], i: usize) -> JsValue {
    args.get(i).copied().unwrap_or(JsValue::UNDEFINED)
}

/// Installs `Promise`, `AggregateError`, `queueMicrotask`, and `Generator.prototype`.
pub fn install_promise_and_async(vm: &mut JsVm) {
    install_aggregate_error(vm);
    install_promise(vm);
    install_generator_proto(vm);
    install_async_generator_proto(vm);

    let qm = vm.alloc_native_fn("queueMicrotask", |vm, _this, args| {
        let Some(func) = arg(args, 0).as_object() else {
            return Err(vm.type_error("queueMicrotask requires a callback function"));
        };
        if !vm.is_callable_handle(func) {
            return Err(vm.type_error("queueMicrotask requires a callback function"));
        }
        vm.enqueue_microtask(MicrotaskJob::Callback {
            func,
            this_val: JsValue::UNDEFINED,
            args: Vec::new(),
        });
        Ok(JsValue::UNDEFINED)
    });
    define_global(vm, "queueMicrotask", JsValue::from_object(qm));
}

fn install_aggregate_error(vm: &mut JsVm) {
    let agg_proto = vm.aggregate_error_proto;
    let nv = JsValue::from_string(vm.heap.alloc_string("AggregateError"));
    let empty_m = JsValue::from_string(vm.heap.alloc_string(""));
    vm.heap.object_define_data(
        agg_proto,
        vm.key_name,
        nv,
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        agg_proto,
        vm.key_message,
        empty_m,
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    let ctor = vm.alloc_host_fn("AggregateError", move |vm, _this, args| {
        let err = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(agg_proto)));
        let errors_arr = vm.alloc_array();
        let iterable = arg(args, 0);
        for item in vm.collect_iterable(iterable)? {
            vm.heap.object_push_element(errors_arr, item);
        }
        let k_errors = vm.heap.intern_key("errors");
        vm.heap.object_define_data(
            err,
            k_errors,
            JsValue::from_object(errors_arr),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
        if !arg(args, 1).is_undefined() {
            let msg = vm.value_to_rust_string(arg(args, 1));
            let mv = JsValue::from_string(vm.heap.alloc_string(msg));
            vm.heap.object_define_data(
                err,
                vm.key_message,
                mv,
                PropertyAttrs::METHOD,
                &mut vm.shapes,
            );
        }
        if let Some(opts) = args.get(2).and_then(|v| v.as_object()) {
            let k_cause = vm.heap.intern_key("cause");
            if vm.has_property_by_key(opts, k_cause)? {
                let cause = vm.get_property_by_key(opts, k_cause)?;
                vm.heap.object_define_data(
                    err,
                    k_cause,
                    cause,
                    PropertyAttrs::METHOD,
                    &mut vm.shapes,
                );
            }
        }
        Ok(JsValue::from_object(err))
    });
    vm.mark_constructor(ctor);
    vm.set_fn_length(ctor, 2);
    let k_error = vm.heap.intern_key("Error");
    if let Ok(err_ctor) = vm.get_property_by_key(vm.global, k_error) {
        if let Some(ec) = err_ctor.as_object() {
            vm.heap.object_set_prototype(ctor, Some(ec), &mut vm.shapes);
        }
    }
    vm.heap.object_define_data(
        agg_proto,
        vm.key_constructor,
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(agg_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    define_global(vm, "AggregateError", JsValue::from_object(ctor));
}

/// Allocates a new pending `Promise` object.
pub fn alloc_promise(vm: &mut JsVm) -> HeapHandle {
    vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Promise(Box::default()),
        Some(vm.promise_proto),
    ))
}

/// Returns `(resolve_fn, reject_fn)` paired to settle `promise` at most once.
pub fn create_resolving_functions(vm: &mut JsVm, promise: HeapHandle) -> (HeapHandle, HeapHandle) {
    let settled = Rc::new(Cell::new(false));
    let s1 = Rc::clone(&settled);
    let resolve_fn = vm.alloc_host_fn("resolve", move |vm, _this, args| {
        if s1.replace(true) {
            return Ok(JsValue::UNDEFINED);
        }
        resolve_promise(vm, promise, arg(args, 0));
        Ok(JsValue::UNDEFINED)
    });
    let s2 = settled;
    let reject_fn = vm.alloc_host_fn("reject", move |vm, _this, args| {
        if s2.replace(true) {
            return Ok(JsValue::UNDEFINED);
        }
        reject_promise(vm, promise, arg(args, 0));
        Ok(JsValue::UNDEFINED)
    });
    (resolve_fn, reject_fn)
}

/// Resolves `promise` with `value` (chaining if `value` is a `Promise` or thenable).
pub fn resolve_promise(vm: &mut JsVm, promise: HeapHandle, value: JsValue) {
    let is_pending = matches!(
        vm.heap.get_object(promise).map(|o| &o.kind),
        Some(ObjectKind::Promise(p)) if matches!(p.state, PromiseState::Pending)
    );
    if !is_pending {
        return;
    }
    if value.as_object() == Some(promise) {
        let err = vm.type_error("Chaining cycle detected for promise");
        reject_promise(vm, promise, err.value);
        return;
    }
    if let Some(vh) = value.as_object() {
        // Fast path: resolving with another native Promise.
        if matches!(
            vm.heap.get_object(vh).map(|o| &o.kind),
            Some(ObjectKind::Promise(_))
        ) {
            let (res_fn, rej_fn) = create_resolving_functions(vm, promise);
            attach_promise_reaction(
                vm,
                vh,
                PromiseReaction {
                    on_fulfilled: Some(res_fn),
                    on_rejected: Some(rej_fn),
                    result_promise: None,
                },
            );
            return;
        }
        // Generic thenable check (`typeof value.then === "function"`).
        let k_then = vm.heap.intern_key("then");
        match vm.get_property_by_key(vh, k_then) {
            Ok(then_val) => {
                if let Some(then_h) = then_val.as_object() {
                    if vm.is_callable_handle(then_h) {
                        let (res_fn, rej_fn) = create_resolving_functions(vm, promise);
                        vm.enqueue_microtask(MicrotaskJob::Callback {
                            func: then_h,
                            this_val: value,
                            args: vec![JsValue::from_object(res_fn), JsValue::from_object(rej_fn)],
                        });
                        return;
                    }
                }
            }
            Err(e) => {
                reject_promise(vm, promise, e.value);
                return;
            }
        }
    }
    let reactions = {
        let Some(ObjectKind::Promise(p)) = vm.heap.get_object_mut(promise).map(|o| &mut o.kind)
        else {
            return;
        };
        if !matches!(p.state, PromiseState::Pending) {
            return;
        }
        p.state = PromiseState::Fulfilled(value);
        std::mem::take(&mut p.reactions)
    };
    vm.heap.write_barrier(promise, value);
    for reaction in reactions {
        vm.enqueue_microtask(MicrotaskJob::PromiseReaction {
            reaction,
            is_rejected: false,
            value,
        });
    }
}

/// Rejects `promise` with `reason` and enqueues all pending reactions.
pub fn reject_promise(vm: &mut JsVm, promise: HeapHandle, reason: JsValue) {
    let reactions = {
        let Some(ObjectKind::Promise(p)) = vm.heap.get_object_mut(promise).map(|o| &mut o.kind)
        else {
            return;
        };
        if !matches!(p.state, PromiseState::Pending) {
            return;
        }
        p.state = PromiseState::Rejected(reason);
        std::mem::take(&mut p.reactions)
    };
    vm.heap.write_barrier(promise, reason);
    for reaction in reactions {
        vm.enqueue_microtask(MicrotaskJob::PromiseReaction {
            reaction,
            is_rejected: true,
            value: reason,
        });
    }
}

/// Attaches a reaction to `promise` (enqueuing immediately if already settled).
pub fn attach_promise_reaction(vm: &mut JsVm, promise: HeapHandle, reaction: PromiseReaction) {
    let settled = {
        let Some(ObjectKind::Promise(p)) = vm.heap.get_object_mut(promise).map(|o| &mut o.kind)
        else {
            return;
        };
        p.handled = true;
        match p.state {
            PromiseState::Pending => {
                p.reactions.push(reaction);
                None
            }
            PromiseState::Fulfilled(v) => Some((false, v)),
            PromiseState::Rejected(r) => Some((true, r)),
        }
    };
    if let Some((is_rejected, value)) = settled {
        vm.enqueue_microtask(MicrotaskJob::PromiseReaction {
            reaction,
            is_rejected,
            value,
        });
    } else {
        if let Some(h) = reaction.on_fulfilled {
            vm.heap.write_barrier(promise, JsValue::from_object(h));
        }
        if let Some(h) = reaction.on_rejected {
            vm.heap.write_barrier(promise, JsValue::from_object(h));
        }
        if let Some(h) = reaction.result_promise {
            vm.heap.write_barrier(promise, JsValue::from_object(h));
        }
    }
}

/// Implements `Promise.prototype.then(onFulfilled, onRejected)` returning a chained `Promise`.
pub fn promise_then(
    vm: &mut JsVm,
    promise: HeapHandle,
    on_fulfilled: Option<HeapHandle>,
    on_rejected: Option<HeapHandle>,
) -> HeapHandle {
    let result_promise = alloc_promise(vm);
    attach_promise_reaction(
        vm,
        promise,
        PromiseReaction {
            on_fulfilled,
            on_rejected,
            result_promise: Some(result_promise),
        },
    );
    result_promise
}

/// Coerces any JS value to a `Promise` (`Promise.resolve(val)`).
pub fn promise_resolve_value(vm: &mut JsVm, val: JsValue) -> HeapHandle {
    if let Some(h) = val.as_object() {
        if matches!(
            vm.heap.get_object(h).map(|o| &o.kind),
            Some(ObjectKind::Promise(_))
        ) {
            return h;
        }
    }
    let p = alloc_promise(vm);
    resolve_promise(vm, p, val);
    p
}

/// Drains all queued microtasks strictly FIFO until the microtask queue is empty.
pub fn drain_microtasks(vm: &mut JsVm) {
    let mut ran = 0u64;
    let max_jobs = vm.max_loop_iterations.unwrap_or(u64::MAX);
    while let Some(job) = vm.pop_microtask() {
        ran = ran.saturating_add(1);
        if ran > max_jobs {
            break;
        }
        match job {
            MicrotaskJob::Callback {
                func,
                this_val,
                args,
            } => {
                let _ = vm.call_value(JsValue::from_object(func), this_val, &args);
            }
            MicrotaskJob::PromiseReaction {
                reaction,
                is_rejected,
                value,
            } => {
                let handler = if is_rejected {
                    reaction.on_rejected
                } else {
                    reaction.on_fulfilled
                };
                if let Some(h) = handler {
                    match vm.call_value(JsValue::from_object(h), JsValue::UNDEFINED, &[value]) {
                        Ok(res) => {
                            if let Some(rp) = reaction.result_promise {
                                resolve_promise(vm, rp, res);
                            }
                        }
                        Err(e) => {
                            if let Some(rp) = reaction.result_promise {
                                reject_promise(vm, rp, e.value);
                            }
                        }
                    }
                } else if let Some(rp) = reaction.result_promise {
                    if is_rejected {
                        reject_promise(vm, rp, value);
                    } else {
                        resolve_promise(vm, rp, value);
                    }
                }
            }
            MicrotaskJob::AsyncResume {
                frame,
                promise,
                value,
                is_throw,
            } => {
                resume_async_frame(vm, frame, promise, value, is_throw);
            }
        }
    }
}

/// Starts an `async` closure call, returning its backing `Promise`.
pub fn start_async_call(
    vm: &mut JsVm,
    callee: HeapHandle,
    program_id: u32,
    func_idx: u32,
    this_val: JsValue,
    args: &[JsValue],
) -> HeapHandle {
    let promise = alloc_promise(vm);
    let entry_depth = vm.frame_depth();
    if let Err(e) = vm.push_closure_frame(
        callee,
        program_id,
        func_idx,
        this_val,
        args,
        None,
        None,
        JsValue::UNDEFINED,
    ) {
        reject_promise(vm, promise, e.value);
        return promise;
    }
    drive_async_execution(vm, entry_depth, promise);
    promise
}

/// Resumes a suspended `async` frame after an awaited promise settles.
pub fn resume_async_frame(
    vm: &mut JsVm,
    frame: SuspendedFrame,
    promise: HeapHandle,
    value: JsValue,
    is_throw: bool,
) {
    let entry_depth = vm.frame_depth();
    let base = vm.restore_suspended_frame(&frame);
    if is_throw {
        let msg = vm.format_exception_public(value);
        let exc = JsException {
            value,
            message: msg,
        };
        if !vm.unwind_into_frame(entry_depth, exc) {
            reject_promise(vm, promise, value);
            return;
        }
    } else {
        vm.write_register(base + usize::from(frame.resume_dst), value);
    }
    drive_async_execution(vm, entry_depth, promise);
}

fn drive_async_execution(vm: &mut JsVm, entry_depth: usize, promise: HeapHandle) {
    match vm.run_suspendable(entry_depth) {
        Ok(RunOutcome::Completed(val)) => resolve_promise(vm, promise, val),
        Ok(RunOutcome::SuspendedAwait { frame, awaited }) => {
            let awaited_p = promise_resolve_value(vm, awaited);
            let frame_rc = Rc::new(std::cell::RefCell::new(Some(frame)));
            let f_ok = Rc::clone(&frame_rc);
            let on_ok = vm.alloc_host_fn("async_resume_ok", move |vm, _this, args| {
                if let Some(f) = f_ok.borrow_mut().take() {
                    resume_async_frame(vm, f, promise, arg(args, 0), false);
                }
                Ok(JsValue::UNDEFINED)
            });
            let f_err = frame_rc;
            let on_err = vm.alloc_host_fn("async_resume_err", move |vm, _this, args| {
                if let Some(f) = f_err.borrow_mut().take() {
                    resume_async_frame(vm, f, promise, arg(args, 0), true);
                }
                Ok(JsValue::UNDEFINED)
            });
            attach_promise_reaction(
                vm,
                awaited_p,
                PromiseReaction {
                    on_fulfilled: Some(on_ok),
                    on_rejected: Some(on_err),
                    result_promise: None,
                },
            );
        }
        Ok(
            RunOutcome::SuspendedYield { .. }
            | RunOutcome::SuspendedYieldStar { .. }
            | RunOutcome::SuspendedInitial { .. },
        ) => {
            let err = vm.type_error("Unexpected yield inside async function");
            reject_promise(vm, promise, err.value);
        }
        Err(e) => reject_promise(vm, promise, e.value),
    }
}

fn install_promise(vm: &mut JsVm) {
    let pp = vm.promise_proto;
    define_method(vm, pp, "then", |vm, this_val, args| {
        let Some(ph) = this_val.as_object() else {
            return Err(vm.type_error("Promise.prototype.then called on non-Promise"));
        };
        if !matches!(
            vm.heap.get_object(ph).map(|o| &o.kind),
            Some(ObjectKind::Promise(_))
        ) {
            return Err(vm.type_error("Promise.prototype.then called on non-Promise"));
        }
        let on_ful = arg(args, 0)
            .as_object()
            .filter(|&h| vm.is_callable_handle(h));
        let on_rej = arg(args, 1)
            .as_object()
            .filter(|&h| vm.is_callable_handle(h));
        Ok(JsValue::from_object(promise_then(vm, ph, on_ful, on_rej)))
    });
    define_method(vm, pp, "catch", |vm, this_val, args| {
        let k_then = vm.heap.intern_key("then");
        let Some(ph) = this_val.as_object() else {
            return Err(vm.type_error("Promise.prototype.catch called on non-object"));
        };
        let then_fn = vm.get_property_by_key(ph, k_then)?;
        vm.call_value(then_fn, this_val, &[JsValue::UNDEFINED, arg(args, 0)])
    });
    define_method(vm, pp, "finally", |vm, this_val, args| {
        let Some(ph) = this_val.as_object() else {
            return Err(vm.type_error("Promise.prototype.finally called on non-object"));
        };
        let on_finally = arg(args, 0);
        let Some(fin_h) = on_finally.as_object().filter(|&h| vm.is_callable_handle(h)) else {
            return Ok(JsValue::from_object(promise_then(vm, ph, None, None)));
        };
        let on_ful = vm.alloc_host_fn("finally_ful", move |vm, _this, fargs| {
            let val = arg(fargs, 0);
            let res = vm.call_value(JsValue::from_object(fin_h), JsValue::UNDEFINED, &[])?;
            let p = promise_resolve_value(vm, res);
            let ret_val = vm.alloc_host_fn("finally_ret", move |_vm, _, _| Ok(val));
            Ok(JsValue::from_object(promise_then(
                vm,
                p,
                Some(ret_val),
                None,
            )))
        });
        let on_rej = vm.alloc_host_fn("finally_rej", move |vm, _this, fargs| {
            let reason = arg(fargs, 0);
            let res = vm.call_value(JsValue::from_object(fin_h), JsValue::UNDEFINED, &[])?;
            let p = promise_resolve_value(vm, res);
            let rethrow = vm.alloc_host_fn("finally_throw", move |vm, _, _| {
                let msg = vm.format_exception_public(reason);
                Err(JsException {
                    value: reason,
                    message: msg,
                })
            });
            Ok(JsValue::from_object(promise_then(
                vm,
                p,
                Some(rethrow),
                None,
            )))
        });
        Ok(JsValue::from_object(promise_then(
            vm,
            ph,
            Some(on_ful),
            Some(on_rej),
        )))
    });

    let ctor = vm.alloc_native_fn("Promise", |vm, _this, args| {
        let Some(exec) = arg(args, 0).as_object() else {
            return Err(vm.type_error("Promise resolver is not a function"));
        };
        if !vm.is_callable_handle(exec) {
            return Err(vm.type_error("Promise resolver is not a function"));
        }
        let p = alloc_promise(vm);
        let (res_fn, rej_fn) = create_resolving_functions(vm, p);
        if let Err(e) = vm.call_value(
            JsValue::from_object(exec),
            JsValue::UNDEFINED,
            &[JsValue::from_object(res_fn), JsValue::from_object(rej_fn)],
        ) {
            let _ = vm.call_value(JsValue::from_object(rej_fn), JsValue::UNDEFINED, &[e.value]);
        }
        Ok(JsValue::from_object(p))
    });
    vm.mark_constructor(ctor);
    vm.set_fn_length(ctor, 1);
    define_method(vm, ctor, "resolve", |vm, _this, args| {
        Ok(JsValue::from_object(promise_resolve_value(
            vm,
            arg(args, 0),
        )))
    });
    define_method(vm, ctor, "reject", |vm, _this, args| {
        let p = alloc_promise(vm);
        reject_promise(vm, p, arg(args, 0));
        Ok(JsValue::from_object(p))
    });
    define_method(vm, ctor, "withResolvers", |vm, _this, _| {
        let p = alloc_promise(vm);
        let (res_fn, rej_fn) = create_resolving_functions(vm, p);
        let obj = vm.alloc_ordinary_object();
        let kp = vm.heap.intern_key("promise");
        let kres = vm.heap.intern_key("resolve");
        let krej = vm.heap.intern_key("reject");
        vm.heap.object_define_data(
            obj,
            kp,
            JsValue::from_object(p),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            obj,
            kres,
            JsValue::from_object(res_fn),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            obj,
            krej,
            JsValue::from_object(rej_fn),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        Ok(JsValue::from_object(obj))
    });
    define_method(vm, ctor, "all", promise_all_fn);
    define_method(vm, ctor, "race", promise_race_fn);
    define_method(vm, ctor, "allSettled", promise_all_settled_fn);
    define_method(vm, ctor, "any", promise_any_fn);

    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(pp),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        pp,
        vm.key_constructor,
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    define_global(vm, "Promise", JsValue::from_object(ctor));
}

fn promise_all_fn(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let out_p = alloc_promise(vm);
    let items = match vm.collect_iterable(arg(args, 0)) {
        Ok(v) => v,
        Err(e) => {
            reject_promise(vm, out_p, e.value);
            return Ok(JsValue::from_object(out_p));
        }
    };
    if items.is_empty() {
        let empty = vm.alloc_array();
        resolve_promise(vm, out_p, JsValue::from_object(empty));
        return Ok(JsValue::from_object(out_p));
    }
    let total = items.len();
    let results = vm.alloc_array();
    if let Some(o) = vm.heap.get_object_mut(results) {
        o.elements.set_len(total as u32);
    }
    let remaining = Rc::new(Cell::new(total));
    let (res_out, rej_out) = create_resolving_functions(vm, out_p);
    for (idx, item) in items.into_iter().enumerate() {
        let p = promise_resolve_value(vm, item);
        let rem = Rc::clone(&remaining);
        let on_ful = vm.alloc_host_fn("all_ful", move |vm, _this, fargs| {
            let v = arg(fargs, 0);
            if let Some(o) = vm.heap.get_object_mut(results) {
                o.elements.set(idx as u32, v);
            }
            vm.heap.write_barrier(results, v);
            let left = rem.get().saturating_sub(1);
            rem.set(left);
            if left == 0 {
                let _ = vm.call_value(
                    JsValue::from_object(res_out),
                    JsValue::UNDEFINED,
                    &[JsValue::from_object(results)],
                );
            }
            Ok(JsValue::UNDEFINED)
        });
        attach_promise_reaction(
            vm,
            p,
            PromiseReaction {
                on_fulfilled: Some(on_ful),
                on_rejected: Some(rej_out),
                result_promise: None,
            },
        );
    }
    Ok(JsValue::from_object(out_p))
}

fn promise_race_fn(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let out_p = alloc_promise(vm);
    let items = match vm.collect_iterable(arg(args, 0)) {
        Ok(v) => v,
        Err(e) => {
            reject_promise(vm, out_p, e.value);
            return Ok(JsValue::from_object(out_p));
        }
    };
    let (res_out, rej_out) = create_resolving_functions(vm, out_p);
    for item in items {
        let p = promise_resolve_value(vm, item);
        attach_promise_reaction(
            vm,
            p,
            PromiseReaction {
                on_fulfilled: Some(res_out),
                on_rejected: Some(rej_out),
                result_promise: None,
            },
        );
    }
    Ok(JsValue::from_object(out_p))
}

fn promise_all_settled_fn(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let out_p = alloc_promise(vm);
    let items = match vm.collect_iterable(arg(args, 0)) {
        Ok(v) => v,
        Err(e) => {
            reject_promise(vm, out_p, e.value);
            return Ok(JsValue::from_object(out_p));
        }
    };
    if items.is_empty() {
        let empty = vm.alloc_array();
        resolve_promise(vm, out_p, JsValue::from_object(empty));
        return Ok(JsValue::from_object(out_p));
    }
    let total = items.len();
    let results = vm.alloc_array();
    if let Some(o) = vm.heap.get_object_mut(results) {
        o.elements.set_len(total as u32);
    }
    let remaining = Rc::new(Cell::new(total));
    for (idx, item) in items.into_iter().enumerate() {
        let p = promise_resolve_value(vm, item);
        let rem1 = Rc::clone(&remaining);
        let on_ful = vm.alloc_host_fn("settled_ful", move |vm, _this, fargs| {
            let entry = vm.alloc_ordinary_object();
            let k_status = vm.heap.intern_key("status");
            let s_val = JsValue::from_string(vm.heap.alloc_string("fulfilled"));
            vm.heap.object_define_data(
                entry,
                k_status,
                s_val,
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
            vm.heap.object_define_data(
                entry,
                vm.key_value,
                arg(fargs, 0),
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
            let ev = JsValue::from_object(entry);
            if let Some(o) = vm.heap.get_object_mut(results) {
                o.elements.set(idx as u32, ev);
            }
            vm.heap.write_barrier(results, ev);
            let left = rem1.get().saturating_sub(1);
            rem1.set(left);
            if left == 0 {
                resolve_promise(vm, out_p, JsValue::from_object(results));
            }
            Ok(JsValue::UNDEFINED)
        });
        let rem2 = Rc::clone(&remaining);
        let on_rej = vm.alloc_host_fn("settled_rej", move |vm, _this, fargs| {
            let entry = vm.alloc_ordinary_object();
            let k_status = vm.heap.intern_key("status");
            let k_reason = vm.heap.intern_key("reason");
            let s_val = JsValue::from_string(vm.heap.alloc_string("rejected"));
            vm.heap.object_define_data(
                entry,
                k_status,
                s_val,
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
            vm.heap.object_define_data(
                entry,
                k_reason,
                arg(fargs, 0),
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
            let ev = JsValue::from_object(entry);
            if let Some(o) = vm.heap.get_object_mut(results) {
                o.elements.set(idx as u32, ev);
            }
            vm.heap.write_barrier(results, ev);
            let left = rem2.get().saturating_sub(1);
            rem2.set(left);
            if left == 0 {
                resolve_promise(vm, out_p, JsValue::from_object(results));
            }
            Ok(JsValue::UNDEFINED)
        });
        attach_promise_reaction(
            vm,
            p,
            PromiseReaction {
                on_fulfilled: Some(on_ful),
                on_rejected: Some(on_rej),
                result_promise: None,
            },
        );
    }
    Ok(JsValue::from_object(out_p))
}

fn promise_any_fn(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let out_p = alloc_promise(vm);
    let items = match vm.collect_iterable(arg(args, 0)) {
        Ok(v) => v,
        Err(e) => {
            reject_promise(vm, out_p, e.value);
            return Ok(JsValue::from_object(out_p));
        }
    };
    let reject_aggregate = |vm: &mut JsVm, errs: HeapHandle| {
        let k_agg = vm.heap.intern_key("AggregateError");
        let agg = vm
            .get_property_by_key(vm.global, k_agg)
            .ok()
            .and_then(|ctor| {
                let msg = JsValue::from_string(vm.heap.alloc_string("All promises were rejected"));
                vm.construct_value(ctor, &[JsValue::from_object(errs), msg], ctor)
                    .ok()
            })
            .unwrap_or_else(|| {
                vm.make_error("AggregateError", "All promises were rejected")
                    .value
            });
        reject_promise(vm, out_p, agg);
    };
    if items.is_empty() {
        let empty = vm.alloc_array();
        reject_aggregate(vm, empty);
        return Ok(JsValue::from_object(out_p));
    }
    let total = items.len();
    let errors = vm.alloc_array();
    if let Some(o) = vm.heap.get_object_mut(errors) {
        o.elements.set_len(total as u32);
    }
    let remaining = Rc::new(Cell::new(total));
    let (res_out, _rej_unused) = create_resolving_functions(vm, out_p);
    for (idx, item) in items.into_iter().enumerate() {
        let p = promise_resolve_value(vm, item);
        let rem = Rc::clone(&remaining);
        let on_rej = vm.alloc_host_fn("any_rej", move |vm, _this, fargs| {
            let reason = arg(fargs, 0);
            if let Some(o) = vm.heap.get_object_mut(errors) {
                o.elements.set(idx as u32, reason);
            }
            vm.heap.write_barrier(errors, reason);
            let left = rem.get().saturating_sub(1);
            rem.set(left);
            if left == 0 {
                let k_agg = vm.heap.intern_key("AggregateError");
                let agg = vm
                    .get_property_by_key(vm.global, k_agg)
                    .ok()
                    .and_then(|ctor| {
                        let msg = JsValue::from_string(
                            vm.heap.alloc_string("All promises were rejected"),
                        );
                        vm.construct_value(ctor, &[JsValue::from_object(errors), msg], ctor)
                            .ok()
                    })
                    .unwrap_or_else(|| {
                        vm.make_error("AggregateError", "All promises were rejected")
                            .value
                    });
                reject_promise(vm, out_p, agg);
            }
            Ok(JsValue::UNDEFINED)
        });
        attach_promise_reaction(
            vm,
            p,
            PromiseReaction {
                on_fulfilled: Some(res_out),
                on_rejected: Some(on_rej),
                result_promise: None,
            },
        );
    }
    Ok(JsValue::from_object(out_p))
}

// ---------------------------------------------------------------------------
// Generator (`function*`, `yield`, `yield*`)
// ---------------------------------------------------------------------------

/// Creates a new suspended `Generator` object for a `function*` invocation.
/// Runs parameter initialization synchronously up to `Opcode::GeneratorStart` per ECMA-262 §15.5.5.
pub fn create_generator(
    vm: &mut JsVm,
    callee: HeapHandle,
    program_id: u32,
    func_idx: u32,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<HeapHandle, JsException> {
    create_generator_inner(vm, callee, program_id, func_idx, this_val, args, false)
}

/// Creates a new suspended `AsyncGenerator` object for an `async function*` invocation.
/// Runs parameter initialization synchronously up to `Opcode::GeneratorStart` per ECMA-262 §15.6.5.
pub fn create_async_generator(
    vm: &mut JsVm,
    callee: HeapHandle,
    program_id: u32,
    func_idx: u32,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<HeapHandle, JsException> {
    create_generator_inner(vm, callee, program_id, func_idx, this_val, args, true)
}

fn create_generator_inner(
    vm: &mut JsVm,
    callee: HeapHandle,
    program_id: u32,
    func_idx: u32,
    this_val: JsValue,
    args: &[JsValue],
    is_async: bool,
) -> Result<HeapHandle, JsException> {
    let starts_with_gen_start = vm
        .function_bytecode(program_id, func_idx)
        .and_then(|f| f.code.first())
        .and_then(|i| i.opcode())
        == Some(crate::bytecode::Opcode::GeneratorStart);
    let has_gen_start = vm.function_bytecode(program_id, func_idx).is_some_and(|f| {
        f.code
            .iter()
            .any(|i| i.opcode() == Some(crate::bytecode::Opcode::GeneratorStart))
    });

    let frame = if !has_gen_start || starts_with_gen_start {
        let (reg_count, param_count) = {
            let f = vm.function_bytecode(program_id, func_idx).unwrap();
            (f.register_count as usize, f.param_count as usize)
        };
        let mut saved_regs = vec![JsValue::UNDEFINED; reg_count.max(param_count + 1)];
        for (i, &a) in args.iter().take(param_count).enumerate() {
            saved_regs[i] = a;
        }
        if args.len() > param_count {
            saved_regs.extend_from_slice(&args[param_count..]);
        }
        SuspendedFrame {
            program_id,
            func_idx,
            pc: usize::from(starts_with_gen_start),
            this_val,
            callee,
            saved_regs,
            call_args: args.to_vec(),
            saved_upvalues: Vec::new(),
            resume_dst: 0,
        }
    } else {
        let stop_depth = vm.frame_depth();
        vm.push_closure_frame(
            callee,
            program_id,
            func_idx,
            this_val,
            args,
            None,
            None,
            JsValue::UNDEFINED,
        )?;
        match vm.run_suspendable(stop_depth) {
            Ok(RunOutcome::SuspendedInitial { frame }) => frame,
            Ok(_) => {
                vm.discard_frames_to(stop_depth);
                return Err(vm.type_error("Generator initialization failed unexpectedly"));
            }
            Err(exc) => {
                vm.discard_frames_to(stop_depth);
                return Err(exc);
            }
        }
    };
    let proto = if is_async {
        vm.async_generator_proto
    } else {
        vm.generator_proto
    };
    Ok(vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Generator(Box::new(GeneratorObject {
            state: GeneratorState::SuspendedStart,
            frame: Some(frame),
            delegate_iter: None,
            is_async,
            pending_requests: VecDeque::new(),
        })),
        Some(proto),
    )))
}

pub fn make_iter_result(vm: &mut JsVm, value: JsValue, done: bool) -> JsValue {
    let obj = vm.alloc_ordinary_object();
    vm.heap.object_define_data(
        obj,
        vm.key_value,
        value,
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        obj,
        vm.key_done,
        JsValue::from_bool(done),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    JsValue::from_object(obj)
}

pub fn step_generator(
    vm: &mut JsVm,
    gen_h: HeapHandle,
    action: GeneratorAction,
) -> Result<JsValue, JsException> {
    let (state, mut frame_opt, delegate_opt) =
        match vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind) {
            Some(ObjectKind::Generator(g)) => (g.state, g.frame.take(), g.delegate_iter),
            _ => return Err(vm.type_error("Incompatible Generator receiver")),
        };
    if state == GeneratorState::Executing {
        return Err(vm.type_error("Generator is already executing"));
    }
    if state == GeneratorState::Completed {
        return match action {
            GeneratorAction::Next(_) => Ok(make_iter_result(vm, JsValue::UNDEFINED, true)),
            GeneratorAction::Return(v) => Ok(make_iter_result(vm, v, true)),
            GeneratorAction::Throw(err) => {
                let msg = vm.format_exception_public(err);
                Err(JsException {
                    value: err,
                    message: msg,
                })
            }
        };
    }

    // Handle active `yield*` delegation iterator before resuming generator bytecode.
    if let Some(iter_h) = delegate_opt {
        match action {
            GeneratorAction::Next(_) => {
                let (v, done) = vm.step_iterator_public(JsValue::from_object(iter_h))?;
                if !done {
                    if let Some(ObjectKind::Generator(g)) =
                        vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                    {
                        g.frame = frame_opt;
                    }
                    return Ok(make_iter_result(vm, v, false));
                }
                if let Some(ObjectKind::Generator(g)) =
                    vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                {
                    g.delegate_iter = None;
                }
                // Delegate exhausted; pass its final value into the generator's `yield*` result reg.
                return resume_generator_frame(
                    vm,
                    gen_h,
                    frame_opt,
                    GeneratorAction::Next(v),
                    false,
                );
            }
            GeneratorAction::Return(v) => {
                if let Some(ObjectKind::Generator(g)) =
                    vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                {
                    g.delegate_iter = None;
                    g.state = GeneratorState::Completed;
                    g.frame = None;
                }
                return Ok(make_iter_result(vm, v, true));
            }
            GeneratorAction::Throw(err) => {
                if let Some(ObjectKind::Generator(g)) =
                    vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                {
                    g.delegate_iter = None;
                }
                return resume_generator_frame(
                    vm,
                    gen_h,
                    frame_opt,
                    GeneratorAction::Throw(err),
                    false,
                );
            }
        }
    }

    if state == GeneratorState::SuspendedStart {
        match action {
            GeneratorAction::Return(v) => {
                if let Some(ObjectKind::Generator(g)) =
                    vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                {
                    g.state = GeneratorState::Completed;
                }
                return Ok(make_iter_result(vm, v, true));
            }
            GeneratorAction::Throw(err) => {
                if let Some(ObjectKind::Generator(g)) =
                    vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                {
                    g.state = GeneratorState::Completed;
                }
                let msg = vm.format_exception_public(err);
                return Err(JsException {
                    value: err,
                    message: msg,
                });
            }
            GeneratorAction::Next(_) => {
                // First `.next()` ignores passed argument and resumes right after `GeneratorStart`.
                return resume_generator_frame(
                    vm,
                    gen_h,
                    frame_opt.take(),
                    GeneratorAction::Next(JsValue::UNDEFINED),
                    true,
                );
            }
        }
    }

    resume_generator_frame(vm, gen_h, frame_opt, action, false)
}

fn resume_generator_frame(
    vm: &mut JsVm,
    gen_h: HeapHandle,
    frame_opt: Option<SuspendedFrame>,
    action: GeneratorAction,
    is_initial_start: bool,
) -> Result<JsValue, JsException> {
    let Some(frame) = frame_opt else {
        return Ok(make_iter_result(vm, JsValue::UNDEFINED, true));
    };
    if let Some(ObjectKind::Generator(g)) = vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind) {
        g.state = GeneratorState::Executing;
    }
    let entry_depth = vm.frame_depth();
    let base = vm.restore_suspended_frame(&frame);
    match action {
        GeneratorAction::Next(v) => {
            if !is_initial_start {
                vm.write_register(base + usize::from(frame.resume_dst), v);
            }
        }
        GeneratorAction::Return(v) => {
            vm.discard_frames_to(entry_depth);
            if let Some(ObjectKind::Generator(g)) =
                vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
            {
                g.state = GeneratorState::Completed;
                g.frame = None;
            }
            return Ok(make_iter_result(vm, v, true));
        }
        GeneratorAction::Throw(err) => {
            let msg = vm.format_exception_public(err);
            let exc = JsException {
                value: err,
                message: msg,
            };
            if !vm.unwind_into_frame(entry_depth, exc.clone()) {
                if let Some(ObjectKind::Generator(g)) =
                    vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                {
                    g.state = GeneratorState::Completed;
                    g.frame = None;
                }
                return Err(exc);
            }
        }
    }

    loop {
        match vm.run_suspendable(entry_depth) {
            Ok(RunOutcome::Completed(val)) => {
                if let Some(ObjectKind::Generator(g)) =
                    vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                {
                    g.state = GeneratorState::Completed;
                    g.frame = None;
                }
                return Ok(make_iter_result(vm, val, true));
            }
            Ok(RunOutcome::SuspendedYield { frame, yielded }) => {
                if let Some(ObjectKind::Generator(g)) =
                    vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                {
                    g.state = GeneratorState::SuspendedYield;
                    g.frame = Some(frame);
                }
                return Ok(make_iter_result(vm, yielded, false));
            }
            Ok(RunOutcome::SuspendedYieldStar { frame, iterable }) => {
                let iter_h = vm.create_iterator_public(iterable)?;
                let (v, done) = vm.step_iterator_public(JsValue::from_object(iter_h))?;
                if !done {
                    if let Some(ObjectKind::Generator(g)) =
                        vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                    {
                        g.state = GeneratorState::SuspendedYield;
                        g.frame = Some(frame);
                        g.delegate_iter = Some(iter_h);
                    }
                    vm.heap.write_barrier(gen_h, JsValue::from_object(iter_h));
                    return Ok(make_iter_result(vm, v, false));
                }
                // Empty iterable in `yield*`: immediately resume generator with `v`!
                let b = vm.restore_suspended_frame(&frame);
                vm.write_register(b + usize::from(frame.resume_dst), v);
            }
            Ok(RunOutcome::SuspendedAwait { .. } | RunOutcome::SuspendedInitial { .. }) => {
                if let Some(ObjectKind::Generator(g)) =
                    vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                {
                    g.state = GeneratorState::Completed;
                }
                return Err(vm.type_error("Unexpected await inside synchronous generator"));
            }
            Err(e) => {
                if let Some(ObjectKind::Generator(g)) =
                    vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                {
                    g.state = GeneratorState::Completed;
                    g.frame = None;
                }
                return Err(e);
            }
        }
    }
}

fn install_generator_proto(vm: &mut JsVm) {
    let gp = vm.generator_proto;
    define_method(vm, gp, "next", |vm, this_val, args| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Generator.prototype.next called on non-object"));
        };
        step_generator(vm, h, GeneratorAction::Next(arg(args, 0)))
    });
    define_method(vm, gp, "return", |vm, this_val, args| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Generator.prototype.return called on non-object"));
        };
        step_generator(vm, h, GeneratorAction::Return(arg(args, 0)))
    });
    define_method(vm, gp, "throw", |vm, this_val, args| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Generator.prototype.throw called on non-object"));
        };
        step_generator(vm, h, GeneratorAction::Throw(arg(args, 0)))
    });
    let sym_key = vm.key_symbol_iterator;
    let sym_fn = vm.alloc_native_fn("[Symbol.iterator]", |_vm, this_val, _| Ok(this_val));
    vm.heap.object_define_data(
        gp,
        sym_key,
        JsValue::from_object(sym_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
}

// ---------------------------------------------------------------------------
// AsyncGenerator (`async function*`, ECMA-262 §27.6)
// ---------------------------------------------------------------------------

fn enqueue_async_generator_request(
    vm: &mut JsVm,
    this_val: JsValue,
    kind: AsyncGeneratorRequestKind,
    value: JsValue,
) -> HeapHandle {
    let out_p = alloc_promise(vm);
    let Some(gen_h) = this_val.as_object() else {
        let err = vm.type_error("Incompatible AsyncGenerator receiver");
        reject_promise(vm, out_p, err.value);
        return out_p;
    };
    let (is_valid_async_gen, state) = match vm.heap.get_object(gen_h).map(|o| &o.kind) {
        Some(ObjectKind::Generator(g)) if g.is_async => (true, g.state),
        _ => (false, GeneratorState::Completed),
    };
    if !is_valid_async_gen {
        let err = vm.type_error("Incompatible AsyncGenerator receiver");
        reject_promise(vm, out_p, err.value);
        return out_p;
    }
    let req = AsyncGeneratorRequest {
        kind,
        value,
        promise: out_p,
    };
    if state == GeneratorState::Executing {
        if let Some(ObjectKind::Generator(g)) = vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind) {
            g.pending_requests.push_back(req);
        }
        vm.heap.write_barrier(gen_h, value);
        vm.heap.write_barrier(gen_h, JsValue::from_object(out_p));
        return out_p;
    }
    start_async_generator_request(vm, gen_h, req);
    out_p
}

fn drain_next_async_generator_request(vm: &mut JsVm, gen_h: HeapHandle) {
    let next_req = match vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind) {
        Some(ObjectKind::Generator(g)) => g.pending_requests.pop_front(),
        _ => None,
    };
    if let Some(req) = next_req {
        start_async_generator_request(vm, gen_h, req);
    }
}

fn finish_async_generator_step_resolve(
    vm: &mut JsVm,
    gen_h: HeapHandle,
    step_p: HeapHandle,
    value: JsValue,
    done: bool,
) {
    let res = make_iter_result(vm, value, done);
    resolve_promise(vm, step_p, res);
    drain_next_async_generator_request(vm, gen_h);
}

fn finish_async_generator_step_reject(
    vm: &mut JsVm,
    gen_h: HeapHandle,
    step_p: HeapHandle,
    reason: JsValue,
) {
    reject_promise(vm, step_p, reason);
    drain_next_async_generator_request(vm, gen_h);
}

fn start_async_generator_request(vm: &mut JsVm, gen_h: HeapHandle, req: AsyncGeneratorRequest) {
    let (state, mut frame_opt, delegate_opt) =
        match vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind) {
            Some(ObjectKind::Generator(g)) => (g.state, g.frame.take(), g.delegate_iter),
            _ => {
                let err = vm.type_error("Incompatible AsyncGenerator receiver");
                reject_promise(vm, req.promise, err.value);
                return;
            }
        };

    if state == GeneratorState::Completed {
        match req.kind {
            AsyncGeneratorRequestKind::Next => {
                finish_async_generator_step_resolve(
                    vm,
                    gen_h,
                    req.promise,
                    JsValue::UNDEFINED,
                    true,
                );
            }
            AsyncGeneratorRequestKind::Return => {
                let p = promise_resolve_value(vm, req.value);
                let step_p = req.promise;
                let on_ok = vm.alloc_host_fn("agen_ret_ok", move |vm, _this, args| {
                    finish_async_generator_step_resolve(vm, gen_h, step_p, arg(args, 0), true);
                    Ok(JsValue::UNDEFINED)
                });
                let on_err = vm.alloc_host_fn("agen_ret_err", move |vm, _this, args| {
                    finish_async_generator_step_reject(vm, gen_h, step_p, arg(args, 0));
                    Ok(JsValue::UNDEFINED)
                });
                attach_promise_reaction(
                    vm,
                    p,
                    PromiseReaction {
                        on_fulfilled: Some(on_ok),
                        on_rejected: Some(on_err),
                        result_promise: None,
                    },
                );
            }
            AsyncGeneratorRequestKind::Throw => {
                finish_async_generator_step_reject(vm, gen_h, req.promise, req.value);
            }
        }
        return;
    }

    // Active `yield*` delegation inside an async generator.
    if let Some(iter_h) = delegate_opt {
        if req.kind == AsyncGeneratorRequestKind::Next {
            let next_res_p = match vm.step_async_iterator_public(JsValue::from_object(iter_h)) {
                Ok(v) => promise_resolve_value(vm, v),
                Err(e) => {
                    if let Some(ObjectKind::Generator(g)) =
                        vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                    {
                        g.delegate_iter = None;
                        g.state = GeneratorState::Completed;
                    }
                    finish_async_generator_step_reject(vm, gen_h, req.promise, e.value);
                    return;
                }
            };
            if let Some(ObjectKind::Generator(g)) =
                vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
            {
                g.state = GeneratorState::Executing;
                g.frame = frame_opt;
            }
            let step_p = req.promise;
            let on_ok = vm.alloc_host_fn("agen_del_ok", move |vm, _this, args| {
                let res_val = arg(args, 0);
                let Some(res_h) = res_val.as_object() else {
                    if let Some(ObjectKind::Generator(g)) =
                        vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                    {
                        g.delegate_iter = None;
                        g.state = GeneratorState::Completed;
                        g.frame = None;
                    }
                    let err = vm.type_error("Async iterator result is not an object");
                    finish_async_generator_step_reject(vm, gen_h, step_p, err.value);
                    return Ok(JsValue::UNDEFINED);
                };
                let done_val = match vm.get_property_by_key(res_h, vm.key_done) {
                    Ok(v) => v,
                    Err(e) => {
                        finish_async_generator_step_reject(vm, gen_h, step_p, e.value);
                        return Ok(JsValue::UNDEFINED);
                    }
                };
                let done = vm.to_boolean(done_val);
                let val = match vm.get_property_by_key(res_h, vm.key_value) {
                    Ok(v) => v,
                    Err(e) => {
                        finish_async_generator_step_reject(vm, gen_h, step_p, e.value);
                        return Ok(JsValue::UNDEFINED);
                    }
                };
                if !done {
                    if let Some(ObjectKind::Generator(g)) =
                        vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                    {
                        g.state = GeneratorState::SuspendedYield;
                    }
                    finish_async_generator_step_resolve(vm, gen_h, step_p, val, false);
                } else {
                    let frame = match vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind) {
                        Some(ObjectKind::Generator(g)) => {
                            g.delegate_iter = None;
                            g.frame.take()
                        }
                        _ => None,
                    };
                    resume_async_generator_frame(
                        vm,
                        gen_h,
                        frame,
                        AsyncGeneratorRequest {
                            kind: AsyncGeneratorRequestKind::Next,
                            value: val,
                            promise: step_p,
                        },
                        false,
                    );
                }
                Ok(JsValue::UNDEFINED)
            });
            let on_err = vm.alloc_host_fn("agen_del_err", move |vm, _this, args| {
                let frame = match vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind) {
                    Some(ObjectKind::Generator(g)) => {
                        g.delegate_iter = None;
                        g.frame.take()
                    }
                    _ => None,
                };
                resume_async_generator_frame(
                    vm,
                    gen_h,
                    frame,
                    AsyncGeneratorRequest {
                        kind: AsyncGeneratorRequestKind::Throw,
                        value: arg(args, 0),
                        promise: step_p,
                    },
                    false,
                );
                Ok(JsValue::UNDEFINED)
            });
            attach_promise_reaction(
                vm,
                next_res_p,
                PromiseReaction {
                    on_fulfilled: Some(on_ok),
                    on_rejected: Some(on_err),
                    result_promise: None,
                },
            );
            return;
        }
        if let Some(ObjectKind::Generator(g)) = vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind) {
            g.delegate_iter = None;
        }
    }

    if state == GeneratorState::SuspendedStart {
        match req.kind {
            AsyncGeneratorRequestKind::Return => {
                if let Some(ObjectKind::Generator(g)) =
                    vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                {
                    g.state = GeneratorState::Completed;
                }
                let p = promise_resolve_value(vm, req.value);
                let step_p = req.promise;
                let on_ok = vm.alloc_host_fn("agen_start_ret_ok", move |vm, _this, args| {
                    finish_async_generator_step_resolve(vm, gen_h, step_p, arg(args, 0), true);
                    Ok(JsValue::UNDEFINED)
                });
                let on_err = vm.alloc_host_fn("agen_start_ret_err", move |vm, _this, args| {
                    finish_async_generator_step_reject(vm, gen_h, step_p, arg(args, 0));
                    Ok(JsValue::UNDEFINED)
                });
                attach_promise_reaction(
                    vm,
                    p,
                    PromiseReaction {
                        on_fulfilled: Some(on_ok),
                        on_rejected: Some(on_err),
                        result_promise: None,
                    },
                );
                return;
            }
            AsyncGeneratorRequestKind::Throw => {
                if let Some(ObjectKind::Generator(g)) =
                    vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                {
                    g.state = GeneratorState::Completed;
                }
                finish_async_generator_step_reject(vm, gen_h, req.promise, req.value);
                return;
            }
            AsyncGeneratorRequestKind::Next => {
                resume_async_generator_frame(
                    vm,
                    gen_h,
                    frame_opt.take(),
                    AsyncGeneratorRequest {
                        kind: AsyncGeneratorRequestKind::Next,
                        value: JsValue::UNDEFINED,
                        promise: req.promise,
                    },
                    true,
                );
                return;
            }
        }
    }

    resume_async_generator_frame(vm, gen_h, frame_opt, req, false);
}

fn resume_async_generator_frame(
    vm: &mut JsVm,
    gen_h: HeapHandle,
    frame_opt: Option<SuspendedFrame>,
    req: AsyncGeneratorRequest,
    is_initial_start: bool,
) {
    let Some(frame) = frame_opt else {
        if let Some(ObjectKind::Generator(g)) = vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind) {
            g.state = GeneratorState::Completed;
        }
        finish_async_generator_step_resolve(vm, gen_h, req.promise, JsValue::UNDEFINED, true);
        return;
    };
    if let Some(ObjectKind::Generator(g)) = vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind) {
        g.state = GeneratorState::Executing;
    }
    let entry_depth = vm.frame_depth();
    let base = vm.restore_suspended_frame(&frame);
    match req.kind {
        AsyncGeneratorRequestKind::Next => {
            if !is_initial_start {
                vm.write_register(base + usize::from(frame.resume_dst), req.value);
            }
        }
        AsyncGeneratorRequestKind::Return => {
            vm.discard_frames_to(entry_depth);
            if let Some(ObjectKind::Generator(g)) =
                vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
            {
                g.state = GeneratorState::Completed;
                g.frame = None;
            }
            let p = promise_resolve_value(vm, req.value);
            let step_p = req.promise;
            let on_ok = vm.alloc_host_fn("agen_mid_ret_ok", move |vm, _this, args| {
                finish_async_generator_step_resolve(vm, gen_h, step_p, arg(args, 0), true);
                Ok(JsValue::UNDEFINED)
            });
            let on_err = vm.alloc_host_fn("agen_mid_ret_err", move |vm, _this, args| {
                finish_async_generator_step_reject(vm, gen_h, step_p, arg(args, 0));
                Ok(JsValue::UNDEFINED)
            });
            attach_promise_reaction(
                vm,
                p,
                PromiseReaction {
                    on_fulfilled: Some(on_ok),
                    on_rejected: Some(on_err),
                    result_promise: None,
                },
            );
            return;
        }
        AsyncGeneratorRequestKind::Throw => {
            let msg = vm.format_exception_public(req.value);
            let exc = JsException {
                value: req.value,
                message: msg,
            };
            if !vm.unwind_into_frame(entry_depth, exc) {
                if let Some(ObjectKind::Generator(g)) =
                    vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                {
                    g.state = GeneratorState::Completed;
                    g.frame = None;
                }
                finish_async_generator_step_reject(vm, gen_h, req.promise, req.value);
                return;
            }
        }
    }
    drive_async_generator_exec(vm, gen_h, entry_depth, req.promise);
}

fn drive_async_generator_exec(
    vm: &mut JsVm,
    gen_h: HeapHandle,
    entry_depth: usize,
    step_p: HeapHandle,
) {
    match vm.run_suspendable(entry_depth) {
        Ok(RunOutcome::Completed(val)) => {
            if let Some(ObjectKind::Generator(g)) =
                vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
            {
                g.state = GeneratorState::Completed;
                g.frame = None;
            }
            // ECMA-262 §27.6.3.3: return value of async generator is awaited before settling.
            let p = promise_resolve_value(vm, val);
            let on_ok = vm.alloc_host_fn("agen_done_ok", move |vm, _this, args| {
                finish_async_generator_step_resolve(vm, gen_h, step_p, arg(args, 0), true);
                Ok(JsValue::UNDEFINED)
            });
            let on_err = vm.alloc_host_fn("agen_done_err", move |vm, _this, args| {
                finish_async_generator_step_reject(vm, gen_h, step_p, arg(args, 0));
                Ok(JsValue::UNDEFINED)
            });
            attach_promise_reaction(
                vm,
                p,
                PromiseReaction {
                    on_fulfilled: Some(on_ok),
                    on_rejected: Some(on_err),
                    result_promise: None,
                },
            );
        }
        Ok(RunOutcome::SuspendedYield { frame, yielded }) => {
            // ECMA-262 §27.6.3.6 AsyncGeneratorYield: await `yielded` before resolving step_p.
            let awaited_p = promise_resolve_value(vm, yielded);
            let frame_rc = Rc::new(std::cell::RefCell::new(Some(frame)));
            let f_ok = Rc::clone(&frame_rc);
            let on_ok = vm.alloc_host_fn("agen_yield_ok", move |vm, _this, args| {
                if let Some(f) = f_ok.borrow_mut().take() {
                    if let Some(ObjectKind::Generator(g)) =
                        vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                    {
                        g.state = GeneratorState::SuspendedYield;
                        g.frame = Some(f);
                    }
                    finish_async_generator_step_resolve(vm, gen_h, step_p, arg(args, 0), false);
                }
                Ok(JsValue::UNDEFINED)
            });
            let f_err = frame_rc;
            let on_err = vm.alloc_host_fn("agen_yield_err", move |vm, _this, args| {
                if let Some(f) = f_err.borrow_mut().take() {
                    resume_async_generator_frame(
                        vm,
                        gen_h,
                        Some(f),
                        AsyncGeneratorRequest {
                            kind: AsyncGeneratorRequestKind::Throw,
                            value: arg(args, 0),
                            promise: step_p,
                        },
                        false,
                    );
                }
                Ok(JsValue::UNDEFINED)
            });
            attach_promise_reaction(
                vm,
                awaited_p,
                PromiseReaction {
                    on_fulfilled: Some(on_ok),
                    on_rejected: Some(on_err),
                    result_promise: None,
                },
            );
        }
        Ok(RunOutcome::SuspendedAwait { frame, awaited }) => {
            let awaited_p = promise_resolve_value(vm, awaited);
            let frame_rc = Rc::new(std::cell::RefCell::new(Some(frame)));
            let f_ok = Rc::clone(&frame_rc);
            let on_ok = vm.alloc_host_fn("agen_await_ok", move |vm, _this, args| {
                if let Some(f) = f_ok.borrow_mut().take() {
                    resume_async_generator_frame(
                        vm,
                        gen_h,
                        Some(f),
                        AsyncGeneratorRequest {
                            kind: AsyncGeneratorRequestKind::Next,
                            value: arg(args, 0),
                            promise: step_p,
                        },
                        false,
                    );
                }
                Ok(JsValue::UNDEFINED)
            });
            let f_err = frame_rc;
            let on_err = vm.alloc_host_fn("agen_await_err", move |vm, _this, args| {
                if let Some(f) = f_err.borrow_mut().take() {
                    resume_async_generator_frame(
                        vm,
                        gen_h,
                        Some(f),
                        AsyncGeneratorRequest {
                            kind: AsyncGeneratorRequestKind::Throw,
                            value: arg(args, 0),
                            promise: step_p,
                        },
                        false,
                    );
                }
                Ok(JsValue::UNDEFINED)
            });
            attach_promise_reaction(
                vm,
                awaited_p,
                PromiseReaction {
                    on_fulfilled: Some(on_ok),
                    on_rejected: Some(on_err),
                    result_promise: None,
                },
            );
        }
        Ok(RunOutcome::SuspendedYieldStar { frame, iterable }) => {
            match vm.create_async_iterator_public(iterable) {
                Ok(iter_h) => {
                    if let Some(ObjectKind::Generator(g)) =
                        vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                    {
                        g.state = GeneratorState::SuspendedYield;
                        g.frame = Some(frame);
                        g.delegate_iter = Some(iter_h);
                    }
                    vm.heap.write_barrier(gen_h, JsValue::from_object(iter_h));
                    start_async_generator_request(
                        vm,
                        gen_h,
                        AsyncGeneratorRequest {
                            kind: AsyncGeneratorRequestKind::Next,
                            value: JsValue::UNDEFINED,
                            promise: step_p,
                        },
                    );
                }
                Err(e) => {
                    if let Some(ObjectKind::Generator(g)) =
                        vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
                    {
                        g.state = GeneratorState::Completed;
                        g.frame = None;
                    }
                    finish_async_generator_step_reject(vm, gen_h, step_p, e.value);
                }
            }
        }
        Ok(RunOutcome::SuspendedInitial { .. }) => {
            if let Some(ObjectKind::Generator(g)) =
                vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
            {
                g.state = GeneratorState::Completed;
                g.frame = None;
            }
            let err = vm.type_error("Unexpected GeneratorStart during execution");
            finish_async_generator_step_reject(vm, gen_h, step_p, err.value);
        }
        Err(e) => {
            if let Some(ObjectKind::Generator(g)) =
                vm.heap.get_object_mut(gen_h).map(|o| &mut o.kind)
            {
                g.state = GeneratorState::Completed;
                g.frame = None;
            }
            finish_async_generator_step_reject(vm, gen_h, step_p, e.value);
        }
    }
}

fn install_async_generator_proto(vm: &mut JsVm) {
    let agp = vm.async_generator_proto;
    define_method(vm, agp, "next", |vm, this_val, args| {
        let p = enqueue_async_generator_request(
            vm,
            this_val,
            AsyncGeneratorRequestKind::Next,
            arg(args, 0),
        );
        Ok(JsValue::from_object(p))
    });
    define_method(vm, agp, "return", |vm, this_val, args| {
        let p = enqueue_async_generator_request(
            vm,
            this_val,
            AsyncGeneratorRequestKind::Return,
            arg(args, 0),
        );
        Ok(JsValue::from_object(p))
    });
    define_method(vm, agp, "throw", |vm, this_val, args| {
        let p = enqueue_async_generator_request(
            vm,
            this_val,
            AsyncGeneratorRequestKind::Throw,
            arg(args, 0),
        );
        Ok(JsValue::from_object(p))
    });
    let sym_key = vm.key_symbol_async_iterator;
    let sym_fn = vm.alloc_native_fn("[Symbol.asyncIterator]", |_vm, this_val, _| Ok(this_val));
    vm.heap.object_define_data(
        agp,
        sym_key,
        JsValue::from_object(sym_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
}
