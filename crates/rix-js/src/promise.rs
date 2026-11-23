//! ES2024 `Promise`, FIFO microtask job queue, `async`/`await` frame suspension,
//! and `function*` `Generator` execution (`yield` / `yield*`).
//!
//! Reference: Spec 05 §6 & Issue #15.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use crate::heap::{
    AsyncGeneratorRequest, AsyncGeneratorRequestKind, GeneratorObject, GeneratorState, JsObject,
    ObjectKind, PromiseReaction, PromiseState, SuspendedFrame,
};
use crate::shape::PropertyAttrs;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm, NativeFnPtr, RunOutcome};
use crate::vm_builtins::define_method_len;

/// Queued ECMAScript microtask job executed FIFO at every microtask checkpoint.
#[derive(Debug, Clone)]
pub enum MicrotaskJob {
    Callback {
        func: HeapHandle,
        this_val: JsValue,
        args: Vec<JsValue>,
    },
    ResolveThenable {
        thenable: JsValue,
        then_fn: HeapHandle,
        resolve_fn: HeapHandle,
        reject_fn: HeapHandle,
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
            Self::ResolveThenable {
                thenable,
                then_fn,
                resolve_fn,
                reject_fn,
            } => {
                out.push(*thenable);
                out.push(JsValue::from_object(*then_fn));
                out.push(JsValue::from_object(*resolve_fn));
                out.push(JsValue::from_object(*reject_fn));
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
        let proto = if let Some(nth) = vm.native_new_target.as_object() {
            vm.get_property_by_key(nth, vm.key_prototype)?
                .as_object()
                .unwrap_or(agg_proto)
        } else {
            agg_proto
        };
        let err = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        if !arg(args, 1).is_undefined() {
            let msg = vm.to_js_string(arg(args, 1))?;
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
/// Per ECMA-262 §27.2.1.3 both functions are anonymous (`name === ""`) with `length === 1`.
pub fn create_resolving_functions(vm: &mut JsVm, promise: HeapHandle) -> (HeapHandle, HeapHandle) {
    let settled = Rc::new(Cell::new(false));
    let s1 = Rc::clone(&settled);
    let resolve_fn = vm.alloc_host_fn("", move |vm, _this, args| {
        if s1.replace(true) {
            return Ok(JsValue::UNDEFINED);
        }
        resolve_promise(vm, promise, arg(args, 0));
        Ok(JsValue::UNDEFINED)
    });
    vm.set_fn_length(resolve_fn, 1);
    let s2 = settled;
    let reject_fn = vm.alloc_host_fn("", move |vm, _this, args| {
        if s2.replace(true) {
            return Ok(JsValue::UNDEFINED);
        }
        reject_promise(vm, promise, arg(args, 0));
        Ok(JsValue::UNDEFINED)
    });
    vm.set_fn_length(reject_fn, 1);
    (resolve_fn, reject_fn)
}

/// Resolves `promise` with `value` per ECMA-262 §27.2.1.3.2 (chaining via `.then`).
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
        let k_then = vm.heap.intern_key("then");
        match vm.get_property_by_key(vh, k_then) {
            Ok(then_val) => {
                if let Some(then_h) = then_val.as_object() {
                    if vm.is_callable_handle(then_h) {
                        let (res_fn, rej_fn) = create_resolving_functions(vm, promise);
                        vm.enqueue_microtask(MicrotaskJob::ResolveThenable {
                            thenable: value,
                            then_fn: then_h,
                            resolve_fn: res_fn,
                            reject_fn: rej_fn,
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
            MicrotaskJob::ResolveThenable {
                thenable,
                then_fn,
                resolve_fn,
                reject_fn,
            } => {
                if let Err(e) = vm.call_value(
                    JsValue::from_object(then_fn),
                    thenable,
                    &[
                        JsValue::from_object(resolve_fn),
                        JsValue::from_object(reject_fn),
                    ],
                ) {
                    let _ = vm.call_value(
                        JsValue::from_object(reject_fn),
                        JsValue::UNDEFINED,
                        &[e.value],
                    );
                }
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

struct PromiseCapability {
    promise: JsValue,
    resolve: HeapHandle,
    reject: HeapHandle,
}

fn new_promise_capability(vm: &mut JsVm, ctor: JsValue) -> Result<PromiseCapability, JsException> {
    if !ctor
        .as_object()
        .is_some_and(|h| vm.is_constructor_handle(h))
    {
        return Err(vm.type_error("Promise constructor is not a constructor"));
    }
    if ctor.as_object() == vm.promise_ctor {
        let p = alloc_promise(vm);
        let (resolve, reject) = create_resolving_functions(vm, p);
        return Ok(PromiseCapability {
            promise: JsValue::from_object(p),
            resolve,
            reject,
        });
    }
    let res_cell = Rc::new(Cell::new(JsValue::UNDEFINED));
    let rej_cell = Rc::new(Cell::new(JsValue::UNDEFINED));
    let r1 = Rc::clone(&res_cell);
    let r2 = Rc::clone(&rej_cell);
    let exec = vm.alloc_host_fn("", move |vm, _this, args| {
        if !r1.get().is_undefined() || !r2.get().is_undefined() {
            return Err(vm.type_error("Promise executor already called"));
        }
        r1.set(arg(args, 0));
        r2.set(arg(args, 1));
        Ok(JsValue::UNDEFINED)
    });
    vm.set_fn_length(exec, 2);
    let promise = vm.construct_value(ctor, &[JsValue::from_object(exec)], ctor)?;
    let Some(resolve) = res_cell
        .get()
        .as_object()
        .filter(|&h| vm.is_callable_handle(h))
    else {
        return Err(vm.type_error("Promise resolve function is not callable"));
    };
    let Some(reject) = rej_cell
        .get()
        .as_object()
        .filter(|&h| vm.is_callable_handle(h))
    else {
        return Err(vm.type_error("Promise reject function is not callable"));
    };
    Ok(PromiseCapability {
        promise,
        resolve,
        reject,
    })
}

fn species_constructor(
    vm: &mut JsVm,
    obj: HeapHandle,
    default_ctor: JsValue,
) -> Result<JsValue, JsException> {
    let c = vm.get_property_by_key(obj, vm.key_constructor)?;
    if c.is_undefined() {
        return Ok(default_ctor);
    }
    let Some(ch) = c.as_object() else {
        return Err(vm.type_error("Constructor property is not an object"));
    };
    let s = vm.get_property_by_key(ch, vm.key_symbol_species)?;
    if s.is_undefined() || s.is_null() {
        return Ok(default_ctor);
    }
    if s.as_object().is_some_and(|sh| vm.is_constructor_handle(sh)) {
        return Ok(s);
    }
    Err(vm.type_error("Species is not a constructor"))
}

fn promise_resolve_with_ctor(
    vm: &mut JsVm,
    ctor: JsValue,
    val: JsValue,
) -> Result<JsValue, JsException> {
    if !ctor.is_object() {
        return Err(vm.type_error("Promise.resolve called on non-object"));
    }
    if let Some(vh) = val.as_object() {
        if matches!(
            vm.heap.get_object(vh).map(|o| &o.kind),
            Some(ObjectKind::Promise(_))
        ) {
            let xc = vm.get_property_by_key(vh, vm.key_constructor)?;
            if xc.is_object() && xc.as_object() == ctor.as_object() {
                return Ok(val);
            }
        }
    }
    let cap = new_promise_capability(vm, ctor)?;
    vm.call_value(
        JsValue::from_object(cap.resolve),
        JsValue::UNDEFINED,
        &[val],
    )?;
    Ok(cap.promise)
}

fn get_promise_resolve(vm: &mut JsVm, ctor: JsValue) -> Result<JsValue, JsException> {
    let Some(ch) = ctor.as_object() else {
        return Err(vm.type_error("Promise combinator called on non-object"));
    };
    let k_resolve = vm.heap.intern_key("resolve");
    let r = vm.get_property_by_key(ch, k_resolve)?;
    if !r.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
        return Err(vm.type_error("Promise resolve property is not callable"));
    }
    Ok(r)
}

fn perform_promise_then_cap(
    vm: &mut JsVm,
    promise: HeapHandle,
    on_ful: Option<HeapHandle>,
    on_rej: Option<HeapHandle>,
    cap: PromiseCapability,
) -> JsValue {
    if let Some(rp) = cap.promise.as_object() {
        if matches!(
            vm.heap.get_object(rp).map(|o| &o.kind),
            Some(ObjectKind::Promise(_))
        ) && vm.promise_ctor.is_some()
        {
            // Check whether cap came from native Promise by wrapping with cap.resolve/cap.reject
            // only if needed; using wrapper guarantees custom subclass resolve/reject hooks fire.
        }
    }
    let cap_res = cap.resolve;
    let cap_rej = cap.reject;
    let wrapped_ful = vm.alloc_host_fn("", move |vm, _this, args| {
        let v = arg(args, 0);
        if let Some(fh) = on_ful {
            match vm.call_value(JsValue::from_object(fh), JsValue::UNDEFINED, &[v]) {
                Ok(r) => {
                    let _ = vm.call_value(JsValue::from_object(cap_res), JsValue::UNDEFINED, &[r]);
                }
                Err(e) => {
                    let _ = vm.call_value(
                        JsValue::from_object(cap_rej),
                        JsValue::UNDEFINED,
                        &[e.value],
                    );
                }
            }
        } else {
            let _ = vm.call_value(JsValue::from_object(cap_res), JsValue::UNDEFINED, &[v]);
        }
        Ok(JsValue::UNDEFINED)
    });
    let wrapped_rej = vm.alloc_host_fn("", move |vm, _this, args| {
        let r = arg(args, 0);
        if let Some(rh) = on_rej {
            match vm.call_value(JsValue::from_object(rh), JsValue::UNDEFINED, &[r]) {
                Ok(res) => {
                    let _ =
                        vm.call_value(JsValue::from_object(cap_res), JsValue::UNDEFINED, &[res]);
                }
                Err(e) => {
                    let _ = vm.call_value(
                        JsValue::from_object(cap_rej),
                        JsValue::UNDEFINED,
                        &[e.value],
                    );
                }
            }
        } else {
            let _ = vm.call_value(JsValue::from_object(cap_rej), JsValue::UNDEFINED, &[r]);
        }
        Ok(JsValue::UNDEFINED)
    });
    attach_promise_reaction(
        vm,
        promise,
        PromiseReaction {
            on_fulfilled: Some(wrapped_ful),
            on_rejected: Some(wrapped_rej),
            result_promise: None,
        },
    );
    cap.promise
}

fn invoke_then(
    vm: &mut JsVm,
    promise_val: JsValue,
    on_ful: JsValue,
    on_rej: JsValue,
) -> Result<JsValue, JsException> {
    let h = vm.to_object(promise_val)?;
    let k_then = vm.heap.intern_key("then");
    let then_fn = vm.get_property_by_key(h, k_then)?;
    vm.call_value(then_fn, promise_val, &[on_ful, on_rej])
}

fn install_promise(vm: &mut JsVm) {
    let pp = vm.promise_proto;
    define_method_len(vm, pp, "then", 2, |vm, this_val, args| {
        let Some(ph) = this_val.as_object() else {
            return Err(vm.type_error("Promise.prototype.then called on non-Promise"));
        };
        if !matches!(
            vm.heap.get_object(ph).map(|o| &o.kind),
            Some(ObjectKind::Promise(_))
        ) {
            return Err(vm.type_error("Promise.prototype.then called on non-Promise"));
        }
        let default_ctor = vm
            .promise_ctor
            .map(JsValue::from_object)
            .unwrap_or(JsValue::UNDEFINED);
        let c = species_constructor(vm, ph, default_ctor)?;
        let on_ful = arg(args, 0)
            .as_object()
            .filter(|&h| vm.is_callable_handle(h));
        let on_rej = arg(args, 1)
            .as_object()
            .filter(|&h| vm.is_callable_handle(h));
        if c.as_object() == vm.promise_ctor {
            return Ok(JsValue::from_object(promise_then(vm, ph, on_ful, on_rej)));
        }
        let cap = new_promise_capability(vm, c)?;
        Ok(perform_promise_then_cap(vm, ph, on_ful, on_rej, cap))
    });
    define_method_len(vm, pp, "catch", 1, |vm, this_val, args| {
        invoke_then(vm, this_val, JsValue::UNDEFINED, arg(args, 0))
    });
    define_method_len(vm, pp, "finally", 1, |vm, this_val, args| {
        let Some(ph) = this_val.as_object() else {
            return Err(vm.type_error("Promise.prototype.finally called on non-object"));
        };
        let default_ctor = vm
            .promise_ctor
            .map(JsValue::from_object)
            .unwrap_or(JsValue::UNDEFINED);
        let c = species_constructor(vm, ph, default_ctor)?;
        let on_finally = arg(args, 0);
        let Some(fin_h) = on_finally.as_object().filter(|&h| vm.is_callable_handle(h)) else {
            return invoke_then(vm, this_val, on_finally, on_finally);
        };
        let on_ful = vm.alloc_host_fn("", move |vm, _this, fargs| {
            let val = arg(fargs, 0);
            let res = vm.call_value(JsValue::from_object(fin_h), JsValue::UNDEFINED, &[])?;
            let p = promise_resolve_with_ctor(vm, c, res)?;
            let ret_val = vm.alloc_host_fn("", move |_vm, _, _| Ok(val));
            vm.set_fn_length(ret_val, 0);
            invoke_then(vm, p, JsValue::from_object(ret_val), JsValue::UNDEFINED)
        });
        vm.set_fn_length(on_ful, 1);
        let on_rej = vm.alloc_host_fn("", move |vm, _this, fargs| {
            let reason = arg(fargs, 0);
            let res = vm.call_value(JsValue::from_object(fin_h), JsValue::UNDEFINED, &[])?;
            let p = promise_resolve_with_ctor(vm, c, res)?;
            let rethrow = vm.alloc_host_fn("", move |vm, _, _| {
                let msg = vm.format_exception_public(reason);
                Err(JsException {
                    value: reason,
                    message: msg,
                })
            });
            vm.set_fn_length(rethrow, 0);
            invoke_then(vm, p, JsValue::from_object(rethrow), JsValue::UNDEFINED)
        });
        vm.set_fn_length(on_rej, 1);
        invoke_then(
            vm,
            this_val,
            JsValue::from_object(on_ful),
            JsValue::from_object(on_rej),
        )
    });

    let ptag = JsValue::from_string(vm.heap.alloc_string("Promise"));
    let k_tag = vm.key_symbol_to_string_tag;
    vm.heap
        .object_define_data(pp, k_tag, ptag, PropertyAttrs::CONFIGURABLE, &mut vm.shapes);

    let ctor = vm.alloc_native_fn("Promise", |vm, _this, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("Constructor Promise requires 'new'"));
        }
        let Some(exec) = arg(args, 0).as_object() else {
            return Err(vm.type_error("Promise resolver is not a function"));
        };
        if !vm.is_callable_handle(exec) {
            return Err(vm.type_error("Promise resolver is not a function"));
        }
        let proto = if let Some(nth) = vm.native_new_target.as_object() {
            vm.get_property_by_key(nth, vm.key_prototype)?
                .as_object()
                .unwrap_or(vm.promise_proto)
        } else {
            vm.promise_proto
        };
        let p = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Promise(Box::default()),
            Some(proto),
        ));
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
    vm.promise_ctor = Some(ctor);

    let species_get = vm.alloc_native_fn("get [Symbol.species]", |_vm, this_val, _| Ok(this_val));
    vm.set_fn_length(species_get, 0);
    let k_species = vm.key_symbol_species;
    vm.heap.object_define_accessor(
        ctor,
        k_species,
        Some(species_get),
        None,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    define_method_len(vm, ctor, "resolve", 1, |vm, this_val, args| {
        promise_resolve_with_ctor(vm, this_val, arg(args, 0))
    });
    define_method_len(vm, ctor, "reject", 1, |vm, this_val, args| {
        let cap = new_promise_capability(vm, this_val)?;
        vm.call_value(
            JsValue::from_object(cap.reject),
            JsValue::UNDEFINED,
            &[arg(args, 0)],
        )?;
        Ok(cap.promise)
    });
    define_method_len(vm, ctor, "withResolvers", 0, |vm, this_val, _| {
        let cap = new_promise_capability(vm, this_val)?;
        let obj = vm.alloc_ordinary_object();
        let kp = vm.heap.intern_key("promise");
        let kres = vm.heap.intern_key("resolve");
        let krej = vm.heap.intern_key("reject");
        vm.heap
            .object_define_data(obj, kp, cap.promise, PropertyAttrs::DEFAULT, &mut vm.shapes);
        vm.heap.object_define_data(
            obj,
            kres,
            JsValue::from_object(cap.resolve),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            obj,
            krej,
            JsValue::from_object(cap.reject),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        Ok(JsValue::from_object(obj))
    });
    define_method_len(vm, ctor, "try", 1, |vm, this_val, args| {
        if !this_val.is_object() {
            return Err(vm.type_error("Promise.try called on non-object"));
        }
        let cap = new_promise_capability(vm, this_val)?;
        let cb = arg(args, 0);
        let rest = if args.len() > 1 { &args[1..] } else { &[] };
        match vm.call_value(cb, JsValue::UNDEFINED, rest) {
            Ok(v) => {
                vm.call_value(JsValue::from_object(cap.resolve), JsValue::UNDEFINED, &[v])?;
            }
            Err(e) => {
                vm.call_value(
                    JsValue::from_object(cap.reject),
                    JsValue::UNDEFINED,
                    &[e.value],
                )?;
            }
        }
        Ok(cap.promise)
    });
    define_method_len(vm, ctor, "all", 1, promise_all_fn);
    define_method_len(vm, ctor, "race", 1, promise_race_fn);
    define_method_len(vm, ctor, "allSettled", 1, promise_all_settled_fn);
    define_method_len(vm, ctor, "any", 1, promise_any_fn);
    define_method_len(vm, ctor, "allKeyed", 1, |vm, t, a| {
        promise_all_keyed_impl(vm, t, a, false)
    });
    define_method_len(vm, ctor, "allSettledKeyed", 1, |vm, t, a| {
        promise_all_keyed_impl(vm, t, a, true)
    });

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

fn reject_cap_and_return(
    vm: &mut JsVm,
    cap: &PromiseCapability,
    err: JsValue,
) -> Result<JsValue, JsException> {
    vm.call_value(JsValue::from_object(cap.reject), JsValue::UNDEFINED, &[err])?;
    Ok(cap.promise)
}

fn promise_all_fn(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let cap = new_promise_capability(vm, this_val)?;
    let promise_resolve = match get_promise_resolve(vm, this_val) {
        Ok(r) => r,
        Err(e) => return reject_cap_and_return(vm, &cap, e.value),
    };
    let iter = match vm.create_iterator_public(arg(args, 0)) {
        Ok(it) => it,
        Err(e) => return reject_cap_and_return(vm, &cap, e.value),
    };
    let values = Rc::new(RefCell::new(Vec::<JsValue>::new()));
    let remaining = Rc::new(Cell::new(1usize));
    let mut index = 0usize;
    loop {
        let (next_val, done) = match vm.step_iterator_public(JsValue::from_object(iter)) {
            Ok(pair) => pair,
            Err(e) => return reject_cap_and_return(vm, &cap, e.value),
        };
        if done {
            let left = remaining.get().saturating_sub(1);
            remaining.set(left);
            if left == 0 {
                let arr = vm.alloc_array();
                for &v in values.borrow().iter() {
                    vm.heap.object_push_element(arr, v);
                }
                vm.call_value(
                    JsValue::from_object(cap.resolve),
                    JsValue::UNDEFINED,
                    &[JsValue::from_object(arr)],
                )?;
            }
            return Ok(cap.promise);
        }
        values.borrow_mut().push(JsValue::UNDEFINED);
        let next_p = match vm.call_value(promise_resolve, this_val, &[next_val]) {
            Ok(p) => p,
            Err(e) => {
                let _ = vm.iterator_close_handle(iter, true);
                return reject_cap_and_return(vm, &cap, e.value);
            }
        };
        let already_called = Rc::new(Cell::new(false));
        let vals_c = Rc::clone(&values);
        let rem_c = Rc::clone(&remaining);
        let cap_res = cap.resolve;
        let idx = index;
        let on_ful = vm.alloc_host_fn("", move |vm, _this, fargs| {
            if already_called.replace(true) {
                return Ok(JsValue::UNDEFINED);
            }
            vals_c.borrow_mut()[idx] = arg(fargs, 0);
            let left = rem_c.get().saturating_sub(1);
            rem_c.set(left);
            if left == 0 {
                let arr = vm.alloc_array();
                for &v in vals_c.borrow().iter() {
                    vm.heap.object_push_element(arr, v);
                }
                vm.call_value(
                    JsValue::from_object(cap_res),
                    JsValue::UNDEFINED,
                    &[JsValue::from_object(arr)],
                )?;
            }
            Ok(JsValue::UNDEFINED)
        });
        vm.set_fn_length(on_ful, 1);
        remaining.set(remaining.get() + 1);
        if let Err(e) = invoke_then(
            vm,
            next_p,
            JsValue::from_object(on_ful),
            JsValue::from_object(cap.reject),
        ) {
            let _ = vm.iterator_close_handle(iter, true);
            return reject_cap_and_return(vm, &cap, e.value);
        }
        index += 1;
    }
}

fn promise_race_fn(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let cap = new_promise_capability(vm, this_val)?;
    let promise_resolve = match get_promise_resolve(vm, this_val) {
        Ok(r) => r,
        Err(e) => return reject_cap_and_return(vm, &cap, e.value),
    };
    let iter = match vm.create_iterator_public(arg(args, 0)) {
        Ok(it) => it,
        Err(e) => return reject_cap_and_return(vm, &cap, e.value),
    };
    loop {
        let (next_val, done) = match vm.step_iterator_public(JsValue::from_object(iter)) {
            Ok(pair) => pair,
            Err(e) => return reject_cap_and_return(vm, &cap, e.value),
        };
        if done {
            return Ok(cap.promise);
        }
        let next_p = match vm.call_value(promise_resolve, this_val, &[next_val]) {
            Ok(p) => p,
            Err(e) => {
                let _ = vm.iterator_close_handle(iter, true);
                return reject_cap_and_return(vm, &cap, e.value);
            }
        };
        if let Err(e) = invoke_then(
            vm,
            next_p,
            JsValue::from_object(cap.resolve),
            JsValue::from_object(cap.reject),
        ) {
            let _ = vm.iterator_close_handle(iter, true);
            return reject_cap_and_return(vm, &cap, e.value);
        }
    }
}

fn promise_all_settled_fn(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let cap = new_promise_capability(vm, this_val)?;
    let promise_resolve = match get_promise_resolve(vm, this_val) {
        Ok(r) => r,
        Err(e) => return reject_cap_and_return(vm, &cap, e.value),
    };
    let iter = match vm.create_iterator_public(arg(args, 0)) {
        Ok(it) => it,
        Err(e) => return reject_cap_and_return(vm, &cap, e.value),
    };
    let values = Rc::new(RefCell::new(Vec::<JsValue>::new()));
    let remaining = Rc::new(Cell::new(1usize));
    let mut index = 0usize;
    loop {
        let (next_val, done) = match vm.step_iterator_public(JsValue::from_object(iter)) {
            Ok(pair) => pair,
            Err(e) => return reject_cap_and_return(vm, &cap, e.value),
        };
        if done {
            let left = remaining.get().saturating_sub(1);
            remaining.set(left);
            if left == 0 {
                let arr = vm.alloc_array();
                for &v in values.borrow().iter() {
                    vm.heap.object_push_element(arr, v);
                }
                vm.call_value(
                    JsValue::from_object(cap.resolve),
                    JsValue::UNDEFINED,
                    &[JsValue::from_object(arr)],
                )?;
            }
            return Ok(cap.promise);
        }
        values.borrow_mut().push(JsValue::UNDEFINED);
        let next_p = match vm.call_value(promise_resolve, this_val, &[next_val]) {
            Ok(p) => p,
            Err(e) => {
                let _ = vm.iterator_close_handle(iter, true);
                return reject_cap_and_return(vm, &cap, e.value);
            }
        };
        let already_called = Rc::new(Cell::new(false));
        let ac1 = Rc::clone(&already_called);
        let vals1 = Rc::clone(&values);
        let rem1 = Rc::clone(&remaining);
        let cap_res1 = cap.resolve;
        let idx = index;
        let on_ful = vm.alloc_host_fn("", move |vm, _this, fargs| {
            if ac1.replace(true) {
                return Ok(JsValue::UNDEFINED);
            }
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
            vals1.borrow_mut()[idx] = JsValue::from_object(entry);
            let left = rem1.get().saturating_sub(1);
            rem1.set(left);
            if left == 0 {
                let arr = vm.alloc_array();
                for &v in vals1.borrow().iter() {
                    vm.heap.object_push_element(arr, v);
                }
                vm.call_value(
                    JsValue::from_object(cap_res1),
                    JsValue::UNDEFINED,
                    &[JsValue::from_object(arr)],
                )?;
            }
            Ok(JsValue::UNDEFINED)
        });
        vm.set_fn_length(on_ful, 1);

        let ac2 = already_called;
        let vals2 = Rc::clone(&values);
        let rem2 = Rc::clone(&remaining);
        let cap_res2 = cap.resolve;
        let on_rej = vm.alloc_host_fn("", move |vm, _this, fargs| {
            if ac2.replace(true) {
                return Ok(JsValue::UNDEFINED);
            }
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
            vals2.borrow_mut()[idx] = JsValue::from_object(entry);
            let left = rem2.get().saturating_sub(1);
            rem2.set(left);
            if left == 0 {
                let arr = vm.alloc_array();
                for &v in vals2.borrow().iter() {
                    vm.heap.object_push_element(arr, v);
                }
                vm.call_value(
                    JsValue::from_object(cap_res2),
                    JsValue::UNDEFINED,
                    &[JsValue::from_object(arr)],
                )?;
            }
            Ok(JsValue::UNDEFINED)
        });
        vm.set_fn_length(on_rej, 1);

        remaining.set(remaining.get() + 1);
        if let Err(e) = invoke_then(
            vm,
            next_p,
            JsValue::from_object(on_ful),
            JsValue::from_object(on_rej),
        ) {
            let _ = vm.iterator_close_handle(iter, true);
            return reject_cap_and_return(vm, &cap, e.value);
        }
        index += 1;
    }
}

fn make_aggregate_error_val(vm: &mut JsVm, errs: &[JsValue]) -> JsValue {
    let arr = vm.alloc_array();
    for &e in errs {
        vm.heap.object_push_element(arr, e);
    }
    let obj = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.aggregate_error_proto),
    ));
    let k_errors = vm.heap.intern_key("errors");
    vm.heap.object_define_data(
        obj,
        k_errors,
        JsValue::from_object(arr),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let msg = JsValue::from_string(vm.heap.alloc_string("All promises were rejected"));
    vm.heap.object_define_data(
        obj,
        vm.key_message,
        msg,
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    JsValue::from_object(obj)
}

fn promise_any_fn(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let cap = new_promise_capability(vm, this_val)?;
    let promise_resolve = match get_promise_resolve(vm, this_val) {
        Ok(r) => r,
        Err(e) => return reject_cap_and_return(vm, &cap, e.value),
    };
    let iter = match vm.create_iterator_public(arg(args, 0)) {
        Ok(it) => it,
        Err(e) => return reject_cap_and_return(vm, &cap, e.value),
    };
    let errors = Rc::new(RefCell::new(Vec::<JsValue>::new()));
    let remaining = Rc::new(Cell::new(1usize));
    let mut index = 0usize;
    loop {
        let (next_val, done) = match vm.step_iterator_public(JsValue::from_object(iter)) {
            Ok(pair) => pair,
            Err(e) => return reject_cap_and_return(vm, &cap, e.value),
        };
        if done {
            let left = remaining.get().saturating_sub(1);
            remaining.set(left);
            if left == 0 {
                let agg = make_aggregate_error_val(vm, &errors.borrow());
                vm.call_value(JsValue::from_object(cap.reject), JsValue::UNDEFINED, &[agg])?;
            }
            return Ok(cap.promise);
        }
        errors.borrow_mut().push(JsValue::UNDEFINED);
        let next_p = match vm.call_value(promise_resolve, this_val, &[next_val]) {
            Ok(p) => p,
            Err(e) => {
                let _ = vm.iterator_close_handle(iter, true);
                return reject_cap_and_return(vm, &cap, e.value);
            }
        };
        let already_called = Rc::new(Cell::new(false));
        let errs_c = Rc::clone(&errors);
        let rem_c = Rc::clone(&remaining);
        let cap_rej = cap.reject;
        let idx = index;
        let on_rej = vm.alloc_host_fn("", move |vm, _this, fargs| {
            if already_called.replace(true) {
                return Ok(JsValue::UNDEFINED);
            }
            errs_c.borrow_mut()[idx] = arg(fargs, 0);
            let left = rem_c.get().saturating_sub(1);
            rem_c.set(left);
            if left == 0 {
                let agg = make_aggregate_error_val(vm, &errs_c.borrow());
                vm.call_value(JsValue::from_object(cap_rej), JsValue::UNDEFINED, &[agg])?;
            }
            Ok(JsValue::UNDEFINED)
        });
        vm.set_fn_length(on_rej, 1);
        remaining.set(remaining.get() + 1);
        if let Err(e) = invoke_then(
            vm,
            next_p,
            JsValue::from_object(cap.resolve),
            JsValue::from_object(on_rej),
        ) {
            let _ = vm.iterator_close_handle(iter, true);
            return reject_cap_and_return(vm, &cap, e.value);
        }
        index += 1;
    }
}

/// Returns `obj.[[OwnPropertyKeys]]()` as `(JsValue key, PropertyKey)` honoring Proxy `ownKeys`.
pub(crate) fn own_property_keys_spec(
    vm: &mut JsVm,
    obj: HeapHandle,
) -> Result<Vec<(JsValue, crate::PropertyKey)>, JsException> {
    if let Some(ObjectKind::Proxy(p)) = vm.heap.get_object(obj).map(|o| o.kind.clone()) {
        if p.revoked {
            return Err(vm.type_error("Proxy has been revoked"));
        }
        let trap = vm.get_property_by_key(p.handler, vm.key_own_keys)?;
        if !trap.is_undefined() && !trap.is_null() {
            let list_val = vm.call_value(
                trap,
                JsValue::from_object(p.handler),
                &[JsValue::from_object(p.target)],
            )?;
            let list_h = list_val
                .as_object()
                .ok_or_else(|| vm.type_error("Proxy ownKeys trap must return an object"))?;
            let len = vm.array_get_len(list_h)?;
            let mut out = Vec::with_capacity(len.min(65_536) as usize);
            for i in 0..len {
                let kv = vm.array_get_index(list_h, i)?;
                if !kv.is_string() && !kv.is_symbol() {
                    return Err(vm.type_error("Proxy ownKeys trap returned non-string/symbol key"));
                }
                let ks = vm.to_property_key(kv)?;
                let pk = vm.heap.intern_key(&ks);
                out.push((kv, pk));
            }
            return Ok(out);
        }
        return own_property_keys_spec(vm, p.target);
    }

    let mut indices: Vec<u32> = Vec::new();
    let mut str_keys: Vec<(String, crate::PropertyKey)> = Vec::new();
    let mut sym_keys: Vec<(crate::SymbolId, crate::PropertyKey)> = Vec::new();

    if let Some(o) = vm.heap.get_object(obj) {
        let elen = o.elements.len();
        for i in 0..elen {
            if o.elements.has(i) {
                indices.push(i);
            }
        }
        let raw_keys: Vec<crate::PropertyKey> = if o.is_dictionary() {
            o.rare
                .as_ref()
                .map(|r| r.dict_order.clone())
                .unwrap_or_default()
        } else {
            vm.shapes
                .get(o.shape())
                .map(|s| s.keys().to_vec())
                .unwrap_or_default()
        };
        for pk in raw_keys {
            if let Some(name) = vm.heap.resolve_key(pk).map(str::to_string) {
                if let Some(sym_num) = name
                    .strip_prefix("Symbol(")
                    .and_then(|s| s.strip_suffix(')'))
                    .and_then(|n| n.parse::<u32>().ok())
                {
                    sym_keys.push((crate::SymbolId(sym_num), pk));
                } else if let Ok(idx) = name.parse::<u32>() {
                    if idx < u32::MAX && idx.to_string() == name {
                        if !indices.contains(&idx) {
                            indices.push(idx);
                        }
                    } else {
                        str_keys.push((name, pk));
                    }
                } else {
                    str_keys.push((name, pk));
                }
            }
        }
    }
    indices.sort_unstable();
    let mut out = Vec::with_capacity(indices.len() + str_keys.len() + sym_keys.len());
    for i in indices {
        let s = i.to_string();
        let pk = vm.heap.intern_key(&s);
        let kv = JsValue::from_string(vm.heap.alloc_string(s));
        out.push((kv, pk));
    }
    for (s, pk) in str_keys {
        let kv = JsValue::from_string(vm.heap.alloc_string(s));
        out.push((kv, pk));
    }
    for (sid, pk) in sym_keys {
        out.push((JsValue::from_symbol(sid), pk));
    }
    Ok(out)
}

/// Checks `GetOwnProperty(obj, key)` and returns `Ok(true)` iff it exists and `[[Enumerable]]` is true.
pub(crate) fn is_own_prop_enumerable(
    vm: &mut JsVm,
    obj: HeapHandle,
    key_val: JsValue,
    pk: crate::PropertyKey,
) -> Result<bool, JsException> {
    if let Some(ObjectKind::Proxy(p)) = vm.heap.get_object(obj).map(|o| o.kind.clone()) {
        if p.revoked {
            return Err(vm.type_error("Proxy has been revoked"));
        }
        let k_gopd = vm.heap.intern_key("getOwnPropertyDescriptor");
        let trap = vm.get_property_by_key(p.handler, k_gopd)?;
        if !trap.is_undefined() && !trap.is_null() {
            let desc = vm.call_value(
                trap,
                JsValue::from_object(p.handler),
                &[JsValue::from_object(p.target), key_val],
            )?;
            if desc.is_undefined() {
                return Ok(false);
            }
            let dh = desc.as_object().ok_or_else(|| {
                vm.type_error("Proxy getOwnPropertyDescriptor trap must return object or undefined")
            })?;
            let k_enum = vm.heap.intern_key("enumerable");
            let ev = vm.get_property_by_key(dh, k_enum)?;
            return Ok(vm.to_boolean(ev));
        }
        return is_own_prop_enumerable(vm, p.target, key_val, pk);
    }
    if let Some(prop) = vm.heap.object_get_own(obj, pk, &vm.shapes) {
        return Ok(prop.attrs.contains(PropertyAttrs::ENUMERABLE));
    }
    if let Some(idx) = vm
        .heap
        .resolve_key(pk)
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&i| i < u32::MAX)
    {
        if vm.heap.get_object(obj).is_some_and(|o| o.elements.has(idx)) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn build_keyed_result_object(
    vm: &mut JsVm,
    keys: &[crate::PropertyKey],
    values: &[JsValue],
) -> JsValue {
    let out = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, None));
    for (&pk, &val) in keys.iter().zip(values.iter()) {
        vm.heap
            .object_define_data(out, pk, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
        vm.heap.write_barrier(out, val);
    }
    JsValue::from_object(out)
}

fn promise_all_keyed_impl(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
    all_settled: bool,
) -> Result<JsValue, JsException> {
    let cap = new_promise_capability(vm, this_val)?;
    let promise_resolve = match get_promise_resolve(vm, this_val) {
        Ok(r) => r,
        Err(e) => return reject_cap_and_return(vm, &cap, e.value),
    };
    let promises_val = arg(args, 0);
    let Some(promises_h) = promises_val.as_object() else {
        let err = vm.type_error("Promise.allKeyed requires an object argument");
        return reject_cap_and_return(vm, &cap, err.value);
    };
    let all_keys = match own_property_keys_spec(vm, promises_h) {
        Ok(k) => k,
        Err(e) => return reject_cap_and_return(vm, &cap, e.value),
    };

    let keys = Rc::new(RefCell::new(Vec::<crate::PropertyKey>::new()));
    let values = Rc::new(RefCell::new(Vec::<JsValue>::new()));
    let remaining = Rc::new(Cell::new(1usize));

    for (key_val, pk) in all_keys {
        let is_enum = match is_own_prop_enumerable(vm, promises_h, key_val, pk) {
            Ok(b) => b,
            Err(e) => return reject_cap_and_return(vm, &cap, e.value),
        };
        if !is_enum {
            continue;
        }
        let prop_val = match vm.get_elem(promises_val, key_val) {
            Ok(v) => v,
            Err(e) => return reject_cap_and_return(vm, &cap, e.value),
        };
        let next_p = match vm.call_value(promise_resolve, this_val, &[prop_val]) {
            Ok(p) => p,
            Err(e) => return reject_cap_and_return(vm, &cap, e.value),
        };
        let idx = keys.borrow().len();
        keys.borrow_mut().push(pk);
        values.borrow_mut().push(JsValue::UNDEFINED);

        let already_called = Rc::new(Cell::new(false));
        let ac1 = Rc::clone(&already_called);
        let keys1 = Rc::clone(&keys);
        let vals1 = Rc::clone(&values);
        let rem1 = Rc::clone(&remaining);
        let cap_res1 = cap.resolve;
        let on_ful = vm.alloc_host_fn("", move |vm, _this, fargs| {
            if ac1.replace(true) {
                return Ok(JsValue::UNDEFINED);
            }
            let stored = if all_settled {
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
                JsValue::from_object(entry)
            } else {
                arg(fargs, 0)
            };
            vals1.borrow_mut()[idx] = stored;
            let left = rem1.get().saturating_sub(1);
            rem1.set(left);
            if left == 0 {
                let obj = build_keyed_result_object(vm, &keys1.borrow(), &vals1.borrow());
                vm.call_value(JsValue::from_object(cap_res1), JsValue::UNDEFINED, &[obj])?;
            }
            Ok(JsValue::UNDEFINED)
        });
        vm.set_fn_length(on_ful, 1);

        let rej_handler = if all_settled {
            let ac2 = already_called;
            let keys2 = Rc::clone(&keys);
            let vals2 = Rc::clone(&values);
            let rem2 = Rc::clone(&remaining);
            let cap_res2 = cap.resolve;
            let on_rej = vm.alloc_host_fn("", move |vm, _this, fargs| {
                if ac2.replace(true) {
                    return Ok(JsValue::UNDEFINED);
                }
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
                vals2.borrow_mut()[idx] = JsValue::from_object(entry);
                let left = rem2.get().saturating_sub(1);
                rem2.set(left);
                if left == 0 {
                    let obj = build_keyed_result_object(vm, &keys2.borrow(), &vals2.borrow());
                    vm.call_value(JsValue::from_object(cap_res2), JsValue::UNDEFINED, &[obj])?;
                }
                Ok(JsValue::UNDEFINED)
            });
            vm.set_fn_length(on_rej, 1);
            JsValue::from_object(on_rej)
        } else {
            JsValue::from_object(cap.reject)
        };

        remaining.set(remaining.get() + 1);
        if let Err(e) = invoke_then(vm, next_p, JsValue::from_object(on_ful), rej_handler) {
            return reject_cap_and_return(vm, &cap, e.value);
        }
    }

    let left = remaining.get().saturating_sub(1);
    remaining.set(left);
    if left == 0 {
        let obj = build_keyed_result_object(vm, &keys.borrow(), &values.borrow());
        vm.call_value(
            JsValue::from_object(cap.resolve),
            JsValue::UNDEFINED,
            &[obj],
        )?;
    }
    Ok(cap.promise)
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

// ---------------------------------------------------------------------------
// ES2024 `Array.fromAsync(asyncItems [, mapfn [, thisArg]])` (ECMA-262 §23.1.2.2)
// ---------------------------------------------------------------------------

fn close_from_async_iter_and_reject(
    vm: &mut JsVm,
    iter_obj: HeapHandle,
    is_async_iter: bool,
    err_val: JsValue,
    out_p: HeapHandle,
) {
    if !is_async_iter {
        let _ = vm.iterator_close_obj(iter_obj, true);
        reject_promise(vm, out_p, err_val);
        return;
    }
    let ret_fn = match vm.get_property_by_key(iter_obj, vm.key_return) {
        Ok(v) if !v.is_undefined() && !v.is_null() => v,
        _ => {
            reject_promise(vm, out_p, err_val);
            return;
        }
    };
    if !ret_fn.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
        reject_promise(vm, out_p, err_val);
        return;
    }
    match vm.call_value(ret_fn, JsValue::from_object(iter_obj), &[]) {
        Ok(res) => {
            let p = promise_resolve_value(vm, res);
            let done = vm.alloc_host_fn("", move |vm, _, _| {
                reject_promise(vm, out_p, err_val);
                Ok(JsValue::UNDEFINED)
            });
            attach_promise_reaction(
                vm,
                p,
                PromiseReaction {
                    on_fulfilled: Some(done),
                    on_rejected: Some(done),
                    result_promise: None,
                },
            );
        }
        Err(_) => reject_promise(vm, out_p, err_val),
    }
}

#[derive(Clone, Copy)]
struct FromAsyncIterCtx {
    iter_obj: HeapHandle,
    next_method: JsValue,
    is_async_iter: bool,
    out_arr: HeapHandle,
    map_fn: Option<JsValue>,
    this_arg: JsValue,
    out_p: HeapHandle,
}

fn drive_from_async_iter(vm: &mut JsVm, ctx: FromAsyncIterCtx, k: u64) {
    if k >= 9_007_199_254_740_991 {
        let err = vm.type_error("Array.fromAsync length exceeds 2^53 - 1");
        close_from_async_iter_and_reject(vm, ctx.iter_obj, ctx.is_async_iter, err.value, ctx.out_p);
        return;
    }
    let step_res = match vm.call_value(ctx.next_method, JsValue::from_object(ctx.iter_obj), &[]) {
        Ok(v) => v,
        Err(e) => {
            reject_promise(vm, ctx.out_p, e.value);
            return;
        }
    };

    let process_value = move |vm: &mut JsVm, val: JsValue| {
        if let Some(mf) = ctx.map_fn {
            match vm.call_value(mf, ctx.this_arg, &[val, JsValue::from_f64(k as f64)]) {
                Ok(mapped) => {
                    let mp = promise_resolve_value(vm, mapped);
                    let on_ok = vm.alloc_host_fn("", move |vm, _, args| {
                        if let Err(e) = vm.array_create_data_prop(ctx.out_arr, k, arg(args, 0)) {
                            close_from_async_iter_and_reject(
                                vm,
                                ctx.iter_obj,
                                ctx.is_async_iter,
                                e.value,
                                ctx.out_p,
                            );
                            return Ok(JsValue::UNDEFINED);
                        }
                        drive_from_async_iter(vm, ctx, k + 1);
                        Ok(JsValue::UNDEFINED)
                    });
                    let on_err = vm.alloc_host_fn("", move |vm, _, args| {
                        close_from_async_iter_and_reject(
                            vm,
                            ctx.iter_obj,
                            ctx.is_async_iter,
                            arg(args, 0),
                            ctx.out_p,
                        );
                        Ok(JsValue::UNDEFINED)
                    });
                    attach_promise_reaction(
                        vm,
                        mp,
                        PromiseReaction {
                            on_fulfilled: Some(on_ok),
                            on_rejected: Some(on_err),
                            result_promise: None,
                        },
                    );
                }
                Err(e) => {
                    close_from_async_iter_and_reject(
                        vm,
                        ctx.iter_obj,
                        ctx.is_async_iter,
                        e.value,
                        ctx.out_p,
                    );
                }
            }
        } else {
            if let Err(e) = vm.array_create_data_prop(ctx.out_arr, k, val) {
                close_from_async_iter_and_reject(
                    vm,
                    ctx.iter_obj,
                    ctx.is_async_iter,
                    e.value,
                    ctx.out_p,
                );
                return;
            }
            drive_from_async_iter(vm, ctx, k + 1);
        }
    };

    if ctx.is_async_iter {
        let p = promise_resolve_value(vm, step_res);
        let on_ok = vm.alloc_host_fn("", move |vm, _, args| {
            let res_val = arg(args, 0);
            let Some(res_h) = res_val.as_object() else {
                let err = vm.type_error("Async iterator result is not an object");
                reject_promise(vm, ctx.out_p, err.value);
                return Ok(JsValue::UNDEFINED);
            };
            let done_v = match vm.get_property_by_key(res_h, vm.key_done) {
                Ok(v) => v,
                Err(e) => {
                    reject_promise(vm, ctx.out_p, e.value);
                    return Ok(JsValue::UNDEFINED);
                }
            };
            if vm.to_boolean(done_v) {
                if let Err(e) = vm.array_set_length(ctx.out_arr, k) {
                    reject_promise(vm, ctx.out_p, e.value);
                } else {
                    resolve_promise(vm, ctx.out_p, JsValue::from_object(ctx.out_arr));
                }
                return Ok(JsValue::UNDEFINED);
            }
            let val = match vm.get_property_by_key(res_h, vm.key_value) {
                Ok(v) => v,
                Err(e) => {
                    reject_promise(vm, ctx.out_p, e.value);
                    return Ok(JsValue::UNDEFINED);
                }
            };
            process_value(vm, val);
            Ok(JsValue::UNDEFINED)
        });
        let on_err = vm.alloc_host_fn("", move |vm, _, args| {
            reject_promise(vm, ctx.out_p, arg(args, 0));
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
    } else {
        // Sync iterator wrapped with Async-from-Sync awaiting each yielded value.
        let Some(res_h) = step_res.as_object() else {
            let err = vm.type_error("Iterator result is not an object");
            reject_promise(vm, ctx.out_p, err.value);
            return;
        };
        let done_v = match vm.get_property_by_key(res_h, vm.key_done) {
            Ok(v) => v,
            Err(e) => {
                reject_promise(vm, ctx.out_p, e.value);
                return;
            }
        };
        if vm.to_boolean(done_v) {
            if let Err(e) = vm.array_set_length(ctx.out_arr, k) {
                reject_promise(vm, ctx.out_p, e.value);
            } else {
                resolve_promise(vm, ctx.out_p, JsValue::from_object(ctx.out_arr));
            }
            return;
        }
        let val = match vm.get_property_by_key(res_h, vm.key_value) {
            Ok(v) => v,
            Err(e) => {
                reject_promise(vm, ctx.out_p, e.value);
                return;
            }
        };
        let vp = promise_resolve_value(vm, val);
        let on_ok = vm.alloc_host_fn("", move |vm, _, args| {
            process_value(vm, arg(args, 0));
            Ok(JsValue::UNDEFINED)
        });
        let on_err = vm.alloc_host_fn("", move |vm, _, args| {
            close_from_async_iter_and_reject(vm, ctx.iter_obj, false, arg(args, 0), ctx.out_p);
            Ok(JsValue::UNDEFINED)
        });
        attach_promise_reaction(
            vm,
            vp,
            PromiseReaction {
                on_fulfilled: Some(on_ok),
                on_rejected: Some(on_err),
                result_promise: None,
            },
        );
    }
}

#[derive(Clone, Copy)]
struct FromAsyncArrayLikeCtx {
    src_h: HeapHandle,
    len: u64,
    out_arr: HeapHandle,
    map_fn: Option<JsValue>,
    this_arg: JsValue,
    out_p: HeapHandle,
}

fn drive_from_async_array_like(vm: &mut JsVm, ctx: FromAsyncArrayLikeCtx, k: u64) {
    if k >= ctx.len {
        if let Err(e) = vm.array_set_length(ctx.out_arr, ctx.len) {
            reject_promise(vm, ctx.out_p, e.value);
        } else {
            resolve_promise(vm, ctx.out_p, JsValue::from_object(ctx.out_arr));
        }
        return;
    }
    let val = match vm.array_get_index(ctx.src_h, k) {
        Ok(v) => v,
        Err(e) => {
            reject_promise(vm, ctx.out_p, e.value);
            return;
        }
    };
    let vp = promise_resolve_value(vm, val);
    let on_ok = vm.alloc_host_fn("", move |vm, _, args| {
        let awaited = arg(args, 0);
        if let Some(mf) = ctx.map_fn {
            match vm.call_value(mf, ctx.this_arg, &[awaited, JsValue::from_f64(k as f64)]) {
                Ok(mapped) => {
                    let mp = promise_resolve_value(vm, mapped);
                    let on_m_ok = vm.alloc_host_fn("", move |vm, _, margs| {
                        if let Err(e) = vm.array_create_data_prop(ctx.out_arr, k, arg(margs, 0)) {
                            reject_promise(vm, ctx.out_p, e.value);
                            return Ok(JsValue::UNDEFINED);
                        }
                        drive_from_async_array_like(vm, ctx, k + 1);
                        Ok(JsValue::UNDEFINED)
                    });
                    let on_m_err = vm.alloc_host_fn("", move |vm, _, margs| {
                        reject_promise(vm, ctx.out_p, arg(margs, 0));
                        Ok(JsValue::UNDEFINED)
                    });
                    attach_promise_reaction(
                        vm,
                        mp,
                        PromiseReaction {
                            on_fulfilled: Some(on_m_ok),
                            on_rejected: Some(on_m_err),
                            result_promise: None,
                        },
                    );
                }
                Err(e) => reject_promise(vm, ctx.out_p, e.value),
            }
        } else {
            if let Err(e) = vm.array_create_data_prop(ctx.out_arr, k, awaited) {
                reject_promise(vm, ctx.out_p, e.value);
                return Ok(JsValue::UNDEFINED);
            }
            drive_from_async_array_like(vm, ctx, k + 1);
        }
        Ok(JsValue::UNDEFINED)
    });
    let on_err = vm.alloc_host_fn("", move |vm, _, args| {
        reject_promise(vm, ctx.out_p, arg(args, 0));
        Ok(JsValue::UNDEFINED)
    });
    attach_promise_reaction(
        vm,
        vp,
        PromiseReaction {
            on_fulfilled: Some(on_ok),
            on_rejected: Some(on_err),
            result_promise: None,
        },
    );
}

fn array_from_async_inner(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
    out_p: HeapHandle,
) -> Result<(), JsException> {
    let async_items = arg(args, 0);
    let map_arg = arg(args, 1);
    let this_arg = arg(args, 2);
    let map_fn = if map_arg.is_undefined() {
        None
    } else {
        if !map_arg
            .as_object()
            .is_some_and(|h| vm.is_callable_handle(h))
        {
            return Err(vm.type_error("Array.fromAsync mapfn is not a function"));
        }
        Some(map_arg)
    };
    if async_items.is_null() || async_items.is_undefined() {
        return Err(vm.type_error("Cannot convert undefined or null to object"));
    }

    let items_obj = vm.to_object(async_items)?;
    let async_m = vm.get_property_by_key(items_obj, vm.key_symbol_async_iterator)?;
    let (iter_method, is_async_iter) = if !async_m.is_undefined() && !async_m.is_null() {
        if !async_m
            .as_object()
            .is_some_and(|h| vm.is_callable_handle(h))
        {
            return Err(vm.type_error("Symbol.asyncIterator is not callable"));
        }
        (Some(async_m), true)
    } else {
        let sync_m = vm.get_property_by_key(items_obj, vm.key_symbol_iterator)?;
        if !sync_m.is_undefined() && !sync_m.is_null() {
            if !sync_m.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
                return Err(vm.type_error("Symbol.iterator is not callable"));
            }
            (Some(sync_m), false)
        } else {
            (None, false)
        }
    };

    if let Some(im) = iter_method {
        let iter_res = vm.call_value(im, async_items, &[])?;
        let Some(iter_obj) = iter_res.as_object() else {
            return Err(vm.type_error("Iterator method returned non-object"));
        };
        let next_method = vm.get_property_by_key(iter_obj, vm.key_next)?;
        let out_arr = if this_val
            .as_object()
            .is_some_and(|h| vm.is_constructor_handle(h))
        {
            vm.construct_value(this_val, &[], this_val)?
                .as_object()
                .ok_or_else(|| vm.type_error("Constructor returned non-object"))?
        } else {
            vm.alloc_array()
        };
        drive_from_async_iter(
            vm,
            FromAsyncIterCtx {
                iter_obj,
                next_method,
                is_async_iter,
                out_arr,
                map_fn,
                this_arg,
                out_p,
            },
            0,
        );
        return Ok(());
    }

    let len = vm.array_get_len(items_obj)?;
    let out_arr = if this_val
        .as_object()
        .is_some_and(|h| vm.is_constructor_handle(h))
    {
        vm.construct_value(this_val, &[JsValue::from_f64(len as f64)], this_val)?
            .as_object()
            .ok_or_else(|| vm.type_error("Constructor returned non-object"))?
    } else {
        if len > u64::from(u32::MAX) {
            return Err(vm.range_error("Invalid array length"));
        }
        let a = vm.alloc_array();
        if let Some(o) = vm.heap.get_object_mut(a) {
            o.elements.set_len(len as u32);
        }
        a
    };
    drive_from_async_array_like(
        vm,
        FromAsyncArrayLikeCtx {
            src_h: items_obj,
            len,
            out_arr,
            map_fn,
            this_arg,
            out_p,
        },
        0,
    );
    Ok(())
}

pub fn array_from_async(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let out_p = alloc_promise(vm);
    if let Err(e) = array_from_async_inner(vm, this_val, args, out_p) {
        reject_promise(vm, out_p, e.value);
    }
    Ok(JsValue::from_object(out_p))
}
