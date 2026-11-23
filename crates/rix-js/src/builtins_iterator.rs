//! ECMA-262 §27.1 `Iterator`, `%Iterator.prototype%`, `%IteratorHelperPrototype%`,
//! `%WrapForValidIteratorPrototype%`, `%AsyncIterator.prototype%`, `Iterator.from`,
//! and ES2025 `Iterator.concat`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::heap::{JsObject, ObjectKind};
use crate::promise::{alloc_promise, make_iter_result, reject_promise, resolve_promise};
use crate::shape::PropertyAttrs;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm, NativeFnPtr};
use crate::vm_builtins::{define_global, define_method_len};

#[inline]
fn arg(args: &[JsValue], i: usize) -> JsValue {
    args.get(i).copied().unwrap_or(JsValue::UNDEFINED)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelperLifecycle {
    SuspendedStart,
    SuspendedYield,
    Executing,
    Completed,
}

#[derive(Debug, Clone)]
enum HelperKind {
    Map {
        iter: HeapHandle,
        next_method: JsValue,
        mapper: JsValue,
        counter: u64,
    },
    Filter {
        iter: HeapHandle,
        next_method: JsValue,
        pred: JsValue,
        counter: u64,
    },
    Take {
        iter: HeapHandle,
        next_method: JsValue,
        remaining: u64,
    },
    Drop {
        iter: HeapHandle,
        next_method: JsValue,
        remaining: u64,
    },
    FlatMap {
        iter: HeapHandle,
        next_method: JsValue,
        mapper: JsValue,
        counter: u64,
        inner: Option<(HeapHandle, JsValue)>,
    },
    Concat {
        iterables: Vec<(HeapHandle, JsValue)>,
        index: usize,
        inner: Option<(HeapHandle, JsValue)>,
    },
}

struct HelperRecord {
    state: HelperLifecycle,
    kind: HelperKind,
}

type HelperTable = Rc<RefCell<HashMap<HeapHandle, HelperRecord>>>;
type WrapTable = Rc<RefCell<HashMap<HeapHandle, (HeapHandle, JsValue)>>>;

fn iter_done(vm: &mut JsVm) -> JsValue {
    make_iter_result(vm, JsValue::UNDEFINED, true)
}

fn iter_val(vm: &mut JsVm, val: JsValue) -> JsValue {
    make_iter_result(vm, val, false)
}

fn check_iter_budget(vm: &mut JsVm) -> Result<(), JsException> {
    vm.loop_iterations = vm.loop_iterations.saturating_add(1);
    if let Some(limit) = vm.max_loop_iterations {
        if vm.loop_iterations > limit {
            return Err(vm.range_error("Script execution iteration limit exceeded"));
        }
    }
    Ok(())
}

/// ECMA-262 §7.4.3 `IteratorStepValue` using a cached `(iter, next_method)` record.
fn step_record(
    vm: &mut JsVm,
    iter: HeapHandle,
    next_method: JsValue,
) -> Result<Option<JsValue>, JsException> {
    check_iter_budget(vm)?;
    let res = vm.call_value(next_method, JsValue::from_object(iter), &[])?;
    let Some(res_h) = res.as_object() else {
        return Err(vm.type_error("Iterator result is not an object"));
    };
    let done_val = vm.get_property_by_key(res_h, vm.key_done)?;
    if vm.to_boolean(done_val) {
        return Ok(None);
    }
    let val = vm.get_property_by_key(res_h, vm.key_value)?;
    Ok(Some(val))
}

fn get_iterator_direct(
    vm: &mut JsVm,
    obj: HeapHandle,
) -> Result<(HeapHandle, JsValue), JsException> {
    let next_method = vm.get_property_by_key(obj, vm.key_next)?;
    Ok((obj, next_method))
}

/// ECMA-262 §7.4.2 `GetIteratorFlattenable(obj, primitiveHandling)`.
fn get_iterator_flattenable(
    vm: &mut JsVm,
    obj: JsValue,
    iterate_strings: bool,
) -> Result<(HeapHandle, JsValue), JsException> {
    if !obj.is_object() && !(iterate_strings && obj.is_string()) {
        return Err(vm.type_error("Value is not an object or iterable"));
    }
    let method = if let Some(h) = obj.as_object() {
        vm.get_property_by_key(h, vm.key_symbol_iterator)?
    } else {
        vm.get_property_by_key(vm.string_proto, vm.key_symbol_iterator)?
    };
    let iterator = if method.is_undefined() || method.is_null() {
        obj
    } else {
        if !method.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
            return Err(vm.type_error("Symbol.iterator is not callable"));
        }
        vm.call_value(method, obj, &[])?
    };
    let Some(iter_h) = iterator.as_object() else {
        return Err(vm.type_error("Iterator must be an object"));
    };
    let next_method = vm.get_property_by_key(iter_h, vm.key_next)?;
    Ok((iter_h, next_method))
}

fn has_iterator_proto_in_chain(vm: &JsVm, mut obj: HeapHandle) -> bool {
    let target = vm.iterator_proto;
    for _ in 0..1024 {
        let Some(proto) = vm.heap.get_object(obj).and_then(|o| o.prototype) else {
            return false;
        };
        if proto == target {
            return true;
        }
        obj = proto;
    }
    false
}

fn alloc_helper(
    vm: &mut JsVm,
    table: &HelperTable,
    kind: HelperKind,
    extra_roots: &[JsValue],
) -> HeapHandle {
    let h = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.iterator_helper_proto),
    ));
    if let Some(o) = vm.heap.get_object_mut(h) {
        for &v in extra_roots {
            if let Some(rh) = v.as_heap_handle() {
                o.extra_refs.push(rh);
            }
        }
    }
    table.borrow_mut().insert(
        h,
        HelperRecord {
            state: HelperLifecycle::SuspendedStart,
            kind,
        },
    );
    h
}

fn helper_next(
    vm: &mut JsVm,
    table: &HelperTable,
    this_val: JsValue,
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Iterator Helper.next called on incompatible receiver"));
    };
    let mut rec = {
        let mut map = table.borrow_mut();
        let Some(r) = map.get_mut(&h) else {
            return Err(vm.type_error("Iterator Helper.next called on incompatible receiver"));
        };
        match r.state {
            HelperLifecycle::Executing => {
                return Err(vm.type_error("Iterator Helper is already executing"));
            }
            HelperLifecycle::Completed => return Ok(iter_done(vm)),
            HelperLifecycle::SuspendedStart | HelperLifecycle::SuspendedYield => {
                r.state = HelperLifecycle::Executing;
                r.kind.clone()
            }
        }
    };

    let step_res: Result<Option<JsValue>, JsException> = match &mut rec {
        HelperKind::Map {
            iter,
            next_method,
            mapper,
            counter,
        } => match step_record(vm, *iter, *next_method) {
            Ok(Some(val)) => {
                let idx = *counter;
                *counter += 1;
                match vm.call_value(
                    *mapper,
                    JsValue::UNDEFINED,
                    &[val, JsValue::from_f64(idx as f64)],
                ) {
                    Ok(mapped) => Ok(Some(mapped)),
                    Err(e) => {
                        let _ = vm.iterator_close_obj(*iter, true);
                        Err(e)
                    }
                }
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        },
        HelperKind::Filter {
            iter,
            next_method,
            pred,
            counter,
        } => loop {
            match step_record(vm, *iter, *next_method) {
                Ok(Some(val)) => {
                    let idx = *counter;
                    *counter += 1;
                    match vm.call_value(
                        *pred,
                        JsValue::UNDEFINED,
                        &[val, JsValue::from_f64(idx as f64)],
                    ) {
                        Ok(keep) => {
                            if vm.to_boolean(keep) {
                                break Ok(Some(val));
                            }
                        }
                        Err(e) => {
                            let _ = vm.iterator_close_obj(*iter, true);
                            break Err(e);
                        }
                    }
                }
                Ok(None) => break Ok(None),
                Err(e) => break Err(e),
            }
        },
        HelperKind::Take {
            iter,
            next_method,
            remaining,
        } => {
            if *remaining == 0 {
                match vm.iterator_close_obj(*iter, false) {
                    Ok(()) => Ok(None),
                    Err(e) => Err(e),
                }
            } else {
                if *remaining != u64::MAX {
                    *remaining -= 1;
                }
                step_record(vm, *iter, *next_method)
            }
        }
        HelperKind::Drop {
            iter,
            next_method,
            remaining,
        } => {
            let mut err_or_done: Option<Result<Option<JsValue>, JsException>> = None;
            while *remaining > 0 {
                if *remaining != u64::MAX {
                    *remaining -= 1;
                }
                match step_record(vm, *iter, *next_method) {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        *remaining = 0;
                        err_or_done = Some(Ok(None));
                        break;
                    }
                    Err(e) => {
                        err_or_done = Some(Err(e));
                        break;
                    }
                }
            }
            match err_or_done {
                Some(r) => r,
                None => step_record(vm, *iter, *next_method),
            }
        }
        HelperKind::FlatMap {
            iter,
            next_method,
            mapper,
            counter,
            inner,
        } => loop {
            if let Some((in_h, in_next)) = *inner {
                match step_record(vm, in_h, in_next) {
                    Ok(Some(v)) => break Ok(Some(v)),
                    Ok(None) => {
                        *inner = None;
                    }
                    Err(e) => {
                        *inner = None;
                        let _ = vm.iterator_close_obj(*iter, true);
                        break Err(e);
                    }
                }
            }
            match step_record(vm, *iter, *next_method) {
                Ok(Some(val)) => {
                    let idx = *counter;
                    *counter += 1;
                    let mapped = match vm.call_value(
                        *mapper,
                        JsValue::UNDEFINED,
                        &[val, JsValue::from_f64(idx as f64)],
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = vm.iterator_close_obj(*iter, true);
                            break Err(e);
                        }
                    };
                    match get_iterator_flattenable(vm, mapped, false) {
                        Ok((in_h, in_next)) => {
                            if let Some(o) = vm.heap.get_object_mut(h) {
                                o.extra_refs.push(in_h);
                                if let Some(nh) = in_next.as_heap_handle() {
                                    o.extra_refs.push(nh);
                                }
                            }
                            *inner = Some((in_h, in_next));
                        }
                        Err(e) => {
                            let _ = vm.iterator_close_obj(*iter, true);
                            break Err(e);
                        }
                    }
                }
                Ok(None) => break Ok(None),
                Err(e) => break Err(e),
            }
        },
        HelperKind::Concat {
            iterables,
            index,
            inner,
        } => loop {
            if let Some((in_h, in_next)) = *inner {
                match step_record(vm, in_h, in_next) {
                    Ok(Some(v)) => break Ok(Some(v)),
                    Ok(None) => {
                        *inner = None;
                    }
                    Err(e) => {
                        *inner = None;
                        break Err(e);
                    }
                }
            }
            if *index >= iterables.len() {
                break Ok(None);
            }
            let (iterable_h, method) = iterables[*index];
            *index += 1;
            let iter_res = match vm.call_value(method, JsValue::from_object(iterable_h), &[]) {
                Ok(v) => v,
                Err(e) => break Err(e),
            };
            let Some(in_h) = iter_res.as_object() else {
                break Err(vm.type_error("Iterator.concat iterable returned non-object"));
            };
            let in_next = match vm.get_property_by_key(in_h, vm.key_next) {
                Ok(v) => v,
                Err(e) => break Err(e),
            };
            if let Some(o) = vm.heap.get_object_mut(h) {
                o.extra_refs.push(in_h);
                if let Some(nh) = in_next.as_heap_handle() {
                    o.extra_refs.push(nh);
                }
            }
            *inner = Some((in_h, in_next));
        },
    };

    let mut map = table.borrow_mut();
    if let Some(r) = map.get_mut(&h) {
        r.kind = rec;
        match &step_res {
            Ok(Some(_)) => r.state = HelperLifecycle::SuspendedYield,
            Ok(None) | Err(_) => r.state = HelperLifecycle::Completed,
        }
    }
    match step_res {
        Ok(Some(v)) => Ok(iter_val(vm, v)),
        Ok(None) => Ok(iter_done(vm)),
        Err(e) => Err(e),
    }
}

fn helper_return(
    vm: &mut JsVm,
    table: &HelperTable,
    this_val: JsValue,
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Iterator Helper.return called on incompatible receiver"));
    };
    let (prev_state, kind) = {
        let mut map = table.borrow_mut();
        let Some(r) = map.get_mut(&h) else {
            return Err(vm.type_error("Iterator Helper.return called on incompatible receiver"));
        };
        match r.state {
            HelperLifecycle::Executing => {
                return Err(vm.type_error("Iterator Helper is already executing"));
            }
            HelperLifecycle::Completed => return Ok(iter_done(vm)),
            s => {
                r.state = HelperLifecycle::Executing;
                (s, r.kind.clone())
            }
        }
    };

    let close_res: Result<(), JsException> = match kind {
        HelperKind::Map { iter, .. }
        | HelperKind::Filter { iter, .. }
        | HelperKind::Take { iter, .. }
        | HelperKind::Drop { iter, .. } => vm.iterator_close_obj(iter, false),
        HelperKind::FlatMap { iter, inner, .. } => {
            if let Some((in_h, _)) = inner {
                match vm.iterator_close_obj(in_h, false) {
                    Ok(()) => vm.iterator_close_obj(iter, false),
                    Err(e) => {
                        let _ = vm.iterator_close_obj(iter, true);
                        Err(e)
                    }
                }
            } else {
                vm.iterator_close_obj(iter, false)
            }
        }
        HelperKind::Concat { inner, .. } => {
            if prev_state == HelperLifecycle::SuspendedStart {
                Ok(())
            } else if let Some((in_h, _)) = inner {
                vm.iterator_close_obj(in_h, false)
            } else {
                Ok(())
            }
        }
    };

    if let Some(r) = table.borrow_mut().get_mut(&h) {
        r.state = HelperLifecycle::Completed;
    }
    close_res?;
    Ok(iter_done(vm))
}

/// ECMA-262 §7.3.37 `SetterThatIgnoresPrototypeProperties`.
fn setter_ignoring_proto_props(
    vm: &mut JsVm,
    this_val: JsValue,
    home: HeapHandle,
    key: crate::PropertyKey,
    val: JsValue,
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Cannot set property on non-object"));
    };
    if h == home {
        return Err(vm.type_error("Cannot set property directly on Iterator.prototype"));
    }
    if let Some(own) = vm.heap.object_get_own(h, key, &vm.shapes) {
        if own.attrs.contains(PropertyAttrs::ACCESSOR) {
            if let Some(sh) = own.setter {
                vm.call_value(JsValue::from_object(sh), this_val, &[val])?;
            } else {
                return Err(vm.type_error("Cannot set property which has only a getter"));
            }
        } else if !own.attrs.contains(PropertyAttrs::WRITABLE) {
            return Err(vm.type_error("Cannot assign to read-only property"));
        } else {
            vm.set_property_by_key(h, key, val)?;
        }
    } else if !vm.heap.get_object(h).is_some_and(|o| o.extensible) {
        return Err(vm.type_error("Cannot define property on non-extensible object"));
    } else {
        vm.heap
            .object_define_data(h, key, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
        vm.heap.write_barrier(h, val);
    }
    Ok(JsValue::UNDEFINED)
}

fn require_obj_and_cb(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
    name: &str,
) -> Result<(HeapHandle, JsValue, JsValue), JsException> {
    let Some(obj) = this_val.as_object() else {
        return Err(vm.type_error(format!("Iterator.prototype.{name} called on non-object")));
    };
    let cb = arg(args, 0);
    if !cb.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
        let _ = vm.iterator_close_obj(obj, true);
        return Err(vm.type_error(format!("Iterator.prototype.{name} requires a function")));
    }
    let (_, next_method) = get_iterator_direct(vm, obj)?;
    Ok((obj, next_method, cb))
}

fn require_obj_and_limit(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
    name: &str,
) -> Result<(HeapHandle, JsValue, u64), JsException> {
    let Some(obj) = this_val.as_object() else {
        return Err(vm.type_error(format!("Iterator.prototype.{name} called on non-object")));
    };
    let num = match vm.to_number(arg(args, 0)) {
        Ok(n) => n,
        Err(e) => {
            let _ = vm.iterator_close_obj(obj, true);
            return Err(e);
        }
    };
    if num.is_nan() || num < 0.0 {
        let _ = vm.iterator_close_obj(obj, true);
        return Err(vm.range_error(format!(
            "Iterator.prototype.{name} limit must be non-negative"
        )));
    }
    let lim = if num.is_infinite() {
        u64::MAX
    } else {
        num.trunc() as u64
    };
    let (_, next_method) = get_iterator_direct(vm, obj)?;
    Ok((obj, next_method, lim))
}

fn iter_predicate_scan(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
    mode: u8,
) -> Result<JsValue, JsException> {
    let name = match mode {
        0 => "some",
        1 => "every",
        _ => "find",
    };
    let (obj, next_method, cb) = require_obj_and_cb(vm, this_val, args, name)?;
    let mut idx = 0u64;
    while let Some(v) = step_record(vm, obj, next_method)? {
        let res = match vm.call_value(cb, JsValue::UNDEFINED, &[v, JsValue::from_f64(idx as f64)]) {
            Ok(r) => r,
            Err(e) => {
                let _ = vm.iterator_close_obj(obj, true);
                return Err(e);
            }
        };
        let hit = vm.to_boolean(res);
        match mode {
            0 if hit => {
                vm.iterator_close_obj(obj, false)?;
                return Ok(JsValue::TRUE);
            }
            1 if !hit => {
                vm.iterator_close_obj(obj, false)?;
                return Ok(JsValue::FALSE);
            }
            2 if hit => {
                vm.iterator_close_obj(obj, false)?;
                return Ok(v);
            }
            _ => {}
        }
        idx += 1;
    }
    Ok(match mode {
        0 => JsValue::FALSE,
        1 => JsValue::TRUE,
        _ => JsValue::UNDEFINED,
    })
}

pub fn install_iterator(vm: &mut JsVm) {
    let helpers: HelperTable = Rc::new(RefCell::new(HashMap::new()));
    let wraps: WrapTable = Rc::new(RefCell::new(HashMap::new()));

    // 1. %IteratorHelperPrototype%
    let ihp = vm.iterator_helper_proto;
    {
        let h_next = Rc::clone(&helpers);
        let nf = vm.alloc_host_fn("next", move |vm, this_val, _| {
            helper_next(vm, &h_next, this_val)
        });
        vm.set_fn_length(nf, 0);
        vm.heap.object_define_data(
            ihp,
            vm.key_next,
            JsValue::from_object(nf),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }
    {
        let h_ret = Rc::clone(&helpers);
        let rf = vm.alloc_host_fn("return", move |vm, this_val, _| {
            helper_return(vm, &h_ret, this_val)
        });
        vm.set_fn_length(rf, 0);
        vm.heap.object_define_data(
            ihp,
            vm.key_return,
            JsValue::from_object(rf),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }
    let ih_tag = JsValue::from_string(vm.heap.alloc_string("Iterator Helper"));
    let k_tag = vm.key_symbol_to_string_tag;
    vm.heap.object_define_data(
        ihp,
        k_tag,
        ih_tag,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    // 2. %WrapForValidIteratorPrototype%
    let wip = vm.wrap_iterator_proto;
    {
        let w_next = Rc::clone(&wraps);
        let nf = vm.alloc_host_fn("next", move |vm, this_val, _| {
            let pair = this_val
                .as_object()
                .and_then(|h| w_next.borrow().get(&h).copied());
            let Some((iter_h, next_method)) = pair else {
                return Err(
                    vm.type_error("WrapForValidIterator.next called on incompatible receiver")
                );
            };
            vm.call_value(next_method, JsValue::from_object(iter_h), &[])
        });
        vm.set_fn_length(nf, 0);
        vm.heap.object_define_data(
            wip,
            vm.key_next,
            JsValue::from_object(nf),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }
    {
        let w_ret = Rc::clone(&wraps);
        let rf = vm.alloc_host_fn("return", move |vm, this_val, _| {
            let pair = this_val
                .as_object()
                .and_then(|h| w_ret.borrow().get(&h).copied());
            let Some((iter_h, _)) = pair else {
                return Err(
                    vm.type_error("WrapForValidIterator.return called on incompatible receiver")
                );
            };
            let ret_fn = vm.get_property_by_key(iter_h, vm.key_return)?;
            if ret_fn.is_undefined() || ret_fn.is_null() {
                return Ok(iter_done(vm));
            }
            if !ret_fn
                .as_object()
                .is_some_and(|fh| vm.is_callable_handle(fh))
            {
                return Err(vm.type_error("Iterator return property is not callable"));
            }
            vm.call_value(ret_fn, JsValue::from_object(iter_h), &[])
        });
        vm.set_fn_length(rf, 0);
        vm.heap.object_define_data(
            wip,
            vm.key_return,
            JsValue::from_object(rf),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }

    // 3. %AsyncIterator.prototype% ([Symbol.asyncIterator], [Symbol.asyncDispose])
    let aip = vm.async_iterator_proto;
    let sym_ai = vm.alloc_native_fn("[Symbol.asyncIterator]", |_vm, this_val, _| Ok(this_val));
    vm.set_fn_length(sym_ai, 0);
    let k_sym_ai = vm.key_symbol_async_iterator;
    vm.heap.object_define_data(
        aip,
        k_sym_ai,
        JsValue::from_object(sym_ai),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let sym_ad = vm.alloc_native_fn("[Symbol.asyncDispose]", |vm, this_val, _| {
        let out_p = alloc_promise(vm);
        let obj = match vm.to_object(this_val) {
            Ok(h) => h,
            Err(e) => {
                reject_promise(vm, out_p, e.value);
                return Ok(JsValue::from_object(out_p));
            }
        };
        let ret_fn = match vm.get_property_by_key(obj, vm.key_return) {
            Ok(v) => v,
            Err(e) => {
                reject_promise(vm, out_p, e.value);
                return Ok(JsValue::from_object(out_p));
            }
        };
        if ret_fn.is_undefined() || ret_fn.is_null() {
            resolve_promise(vm, out_p, JsValue::UNDEFINED);
            return Ok(JsValue::from_object(out_p));
        }
        if !ret_fn.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
            let err = vm.type_error("AsyncIterator return property is not callable");
            reject_promise(vm, out_p, err.value);
            return Ok(JsValue::from_object(out_p));
        }
        match vm.call_value(ret_fn, this_val, &[]) {
            Ok(res) => {
                let p = crate::promise::promise_resolve_value(vm, res);
                let on_ful = vm.alloc_host_fn("", move |vm, _, _| {
                    resolve_promise(vm, out_p, JsValue::UNDEFINED);
                    Ok(JsValue::UNDEFINED)
                });
                let on_rej = vm.alloc_host_fn("", move |vm, _, args| {
                    reject_promise(vm, out_p, arg(args, 0));
                    Ok(JsValue::UNDEFINED)
                });
                crate::promise::attach_promise_reaction(
                    vm,
                    p,
                    crate::heap::PromiseReaction {
                        on_fulfilled: Some(on_ful),
                        on_rejected: Some(on_rej),
                        result_promise: None,
                    },
                );
            }
            Err(e) => reject_promise(vm, out_p, e.value),
        }
        Ok(JsValue::from_object(out_p))
    });
    vm.set_fn_length(sym_ad, 0);
    let k_sym_ad = vm.key_symbol_async_dispose;
    vm.heap.object_define_data(
        aip,
        k_sym_ad,
        JsValue::from_object(sym_ad),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    // 4. %Iterator.prototype%
    let ip = vm.iterator_proto;
    let sym_it = vm.alloc_native_fn("[Symbol.iterator]", |_vm, this_val, _| Ok(this_val));
    vm.set_fn_length(sym_it, 0);
    let k_sym_it = vm.key_symbol_iterator;
    vm.heap.object_define_data(
        ip,
        k_sym_it,
        JsValue::from_object(sym_it),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    let sym_disp = vm.alloc_native_fn("[Symbol.dispose]", |vm, this_val, _| {
        let obj = vm.to_object(this_val)?;
        let ret_fn = vm.get_property_by_key(obj, vm.key_return)?;
        if !ret_fn.is_undefined() && !ret_fn.is_null() {
            if !ret_fn.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
                return Err(vm.type_error("Iterator return property is not callable"));
            }
            vm.call_value(ret_fn, this_val, &[])?;
        }
        Ok(JsValue::UNDEFINED)
    });
    vm.set_fn_length(sym_disp, 0);
    let k_sym_d = vm.key_symbol_dispose;
    vm.heap.object_define_data(
        ip,
        k_sym_d,
        JsValue::from_object(sym_disp),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    // Helper-producing methods on %Iterator.prototype%
    fn def_helper(
        vm: &mut JsVm,
        ip: HeapHandle,
        name: &str,
        len: u32,
        f: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
    ) {
        let fh = vm.alloc_host_fn(name, f);
        vm.set_fn_length(fh, len);
        let k = vm.heap.intern_key(name);
        vm.heap.object_define_data(
            ip,
            k,
            JsValue::from_object(fh),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }

    {
        let ht = Rc::clone(&helpers);
        def_helper(vm, ip, "map", 1, move |vm, this_val, args| {
            let (iter, next_method, mapper) = require_obj_and_cb(vm, this_val, args, "map")?;
            let h = alloc_helper(
                vm,
                &ht,
                HelperKind::Map {
                    iter,
                    next_method,
                    mapper,
                    counter: 0,
                },
                &[JsValue::from_object(iter), next_method, mapper],
            );
            Ok(JsValue::from_object(h))
        });
    }
    {
        let ht = Rc::clone(&helpers);
        def_helper(vm, ip, "filter", 1, move |vm, this_val, args| {
            let (iter, next_method, pred) = require_obj_and_cb(vm, this_val, args, "filter")?;
            let h = alloc_helper(
                vm,
                &ht,
                HelperKind::Filter {
                    iter,
                    next_method,
                    pred,
                    counter: 0,
                },
                &[JsValue::from_object(iter), next_method, pred],
            );
            Ok(JsValue::from_object(h))
        });
    }
    {
        let ht = Rc::clone(&helpers);
        def_helper(vm, ip, "take", 1, move |vm, this_val, args| {
            let (iter, next_method, remaining) = require_obj_and_limit(vm, this_val, args, "take")?;
            let h = alloc_helper(
                vm,
                &ht,
                HelperKind::Take {
                    iter,
                    next_method,
                    remaining,
                },
                &[JsValue::from_object(iter), next_method],
            );
            Ok(JsValue::from_object(h))
        });
    }
    {
        let ht = Rc::clone(&helpers);
        def_helper(vm, ip, "drop", 1, move |vm, this_val, args| {
            let (iter, next_method, remaining) = require_obj_and_limit(vm, this_val, args, "drop")?;
            let h = alloc_helper(
                vm,
                &ht,
                HelperKind::Drop {
                    iter,
                    next_method,
                    remaining,
                },
                &[JsValue::from_object(iter), next_method],
            );
            Ok(JsValue::from_object(h))
        });
    }
    {
        let ht = Rc::clone(&helpers);
        def_helper(vm, ip, "flatMap", 1, move |vm, this_val, args| {
            let (iter, next_method, mapper) = require_obj_and_cb(vm, this_val, args, "flatMap")?;
            let h = alloc_helper(
                vm,
                &ht,
                HelperKind::FlatMap {
                    iter,
                    next_method,
                    mapper,
                    counter: 0,
                    inner: None,
                },
                &[JsValue::from_object(iter), next_method, mapper],
            );
            Ok(JsValue::from_object(h))
        });
    }

    // Consuming methods on %Iterator.prototype%
    let consumers: &[(&str, u32, NativeFnPtr)] = &[
        ("toArray", 0, |vm, this_val, _| {
            let Some(obj) = this_val.as_object() else {
                return Err(vm.type_error("Iterator.prototype.toArray called on non-object"));
            };
            let (_, next_method) = get_iterator_direct(vm, obj)?;
            let out = vm.alloc_array();
            while let Some(v) = step_record(vm, obj, next_method)? {
                vm.heap.object_push_element(out, v);
            }
            Ok(JsValue::from_object(out))
        }),
        ("forEach", 1, |vm, this_val, args| {
            let (obj, next_method, cb) = require_obj_and_cb(vm, this_val, args, "forEach")?;
            let mut idx = 0u64;
            while let Some(v) = step_record(vm, obj, next_method)? {
                if let Err(e) =
                    vm.call_value(cb, JsValue::UNDEFINED, &[v, JsValue::from_f64(idx as f64)])
                {
                    let _ = vm.iterator_close_obj(obj, true);
                    return Err(e);
                }
                idx += 1;
            }
            Ok(JsValue::UNDEFINED)
        }),
        ("reduce", 1, |vm, this_val, args| {
            let (obj, next_method, cb) = require_obj_and_cb(vm, this_val, args, "reduce")?;
            let mut idx = 0u64;
            let mut acc = if args.len() > 1 {
                arg(args, 1)
            } else {
                let Some(first) = step_record(vm, obj, next_method)? else {
                    return Err(vm.type_error("Reduce of empty iterator with no initial value"));
                };
                idx = 1;
                first
            };
            while let Some(v) = step_record(vm, obj, next_method)? {
                acc = match vm.call_value(
                    cb,
                    JsValue::UNDEFINED,
                    &[acc, v, JsValue::from_f64(idx as f64)],
                ) {
                    Ok(a) => a,
                    Err(e) => {
                        let _ = vm.iterator_close_obj(obj, true);
                        return Err(e);
                    }
                };
                idx += 1;
            }
            Ok(acc)
        }),
        ("some", 1, |vm, t, a| iter_predicate_scan(vm, t, a, 0)),
        ("every", 1, |vm, t, a| iter_predicate_scan(vm, t, a, 1)),
        ("find", 1, |vm, t, a| iter_predicate_scan(vm, t, a, 2)),
    ];
    for &(name, len, f) in consumers {
        define_method_len(vm, ip, name, len, f);
    }

    // 5. `Iterator` constructor, `Iterator.from`, `Iterator.concat`
    let ctor_cell: Rc<RefCell<Option<HeapHandle>>> = Rc::new(RefCell::new(None));
    let ctor_check = Rc::clone(&ctor_cell);
    let ctor = vm.alloc_host_fn("Iterator", move |vm, _this, _| {
        let nt = vm.native_new_target;
        if nt.is_undefined() || nt.as_object() == *ctor_check.borrow() {
            return Err(vm.type_error("Abstract class Iterator not directly constructable"));
        }
        let proto = if let Some(nth) = nt.as_object() {
            let p = vm.get_property_by_key(nth, vm.key_prototype)?;
            p.as_object().unwrap_or(vm.iterator_proto)
        } else {
            vm.iterator_proto
        };
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        Ok(JsValue::from_object(obj))
    });
    *ctor_cell.borrow_mut() = Some(ctor);
    vm.mark_constructor(ctor);
    vm.set_fn_length(ctor, 0);
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(ip),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );

    // `Iterator.prototype.constructor` accessor (ECMA-262 §27.1.4.2)
    let ctor_getter = vm.alloc_host_fn("get constructor", move |_vm, _, _| {
        Ok(JsValue::from_object(ctor))
    });
    vm.set_fn_length(ctor_getter, 0);
    let ctor_setter = vm.alloc_native_fn("set constructor", |vm, this_val, args| {
        setter_ignoring_proto_props(
            vm,
            this_val,
            vm.iterator_proto,
            vm.key_constructor,
            arg(args, 0),
        )
    });
    vm.set_fn_length(ctor_setter, 1);
    vm.heap.object_define_accessor(
        ip,
        vm.key_constructor,
        Some(ctor_getter),
        Some(ctor_setter),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    // `Iterator.prototype[Symbol.toStringTag]` accessor (ECMA-262 §27.1.4.14)
    let tag_getter = vm.alloc_native_fn("get [Symbol.toStringTag]", |vm, _, _| {
        Ok(JsValue::from_string(vm.heap.alloc_string("Iterator")))
    });
    vm.set_fn_length(tag_getter, 0);
    let tag_setter = vm.alloc_native_fn("set [Symbol.toStringTag]", |vm, this_val, args| {
        setter_ignoring_proto_props(
            vm,
            this_val,
            vm.iterator_proto,
            vm.key_symbol_to_string_tag,
            arg(args, 0),
        )
    });
    vm.set_fn_length(tag_setter, 1);
    vm.heap.object_define_accessor(
        ip,
        vm.key_symbol_to_string_tag,
        Some(tag_getter),
        Some(tag_setter),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    // `Iterator.from(O)`
    {
        let wt = Rc::clone(&wraps);
        let from_fn = vm.alloc_host_fn("from", move |vm, _this, args| {
            let (iter_h, next_method) = get_iterator_flattenable(vm, arg(args, 0), true)?;
            if has_iterator_proto_in_chain(vm, iter_h) {
                return Ok(JsValue::from_object(iter_h));
            }
            let wrapper = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Ordinary,
                Some(vm.wrap_iterator_proto),
            ));
            if let Some(o) = vm.heap.get_object_mut(wrapper) {
                o.extra_refs.push(iter_h);
                if let Some(nh) = next_method.as_heap_handle() {
                    o.extra_refs.push(nh);
                }
            }
            wt.borrow_mut().insert(wrapper, (iter_h, next_method));
            Ok(JsValue::from_object(wrapper))
        });
        vm.set_fn_length(from_fn, 1);
        let k_from = vm.heap.intern_key("from");
        vm.heap.object_define_data(
            ctor,
            k_from,
            JsValue::from_object(from_fn),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }

    // `Iterator.concat(...items)` (ES2025)
    {
        let ht = Rc::clone(&helpers);
        let concat_fn = vm.alloc_host_fn("concat", move |vm, _this, args| {
            let mut iterables = Vec::with_capacity(args.len());
            let mut roots = Vec::with_capacity(args.len() * 2);
            for &item in args {
                let Some(ih) = item.as_object() else {
                    return Err(vm.type_error("Iterator.concat requires object arguments"));
                };
                let method = vm.get_property_by_key(ih, vm.key_symbol_iterator)?;
                if !method
                    .as_object()
                    .is_some_and(|mh| vm.is_callable_handle(mh))
                {
                    return Err(vm.type_error("Iterator.concat argument is not iterable"));
                }
                iterables.push((ih, method));
                roots.push(item);
                roots.push(method);
            }
            let h = alloc_helper(
                vm,
                &ht,
                HelperKind::Concat {
                    iterables,
                    index: 0,
                    inner: None,
                },
                &roots,
            );
            Ok(JsValue::from_object(h))
        });
        vm.set_fn_length(concat_fn, 0);
        let k_concat = vm.heap.intern_key("concat");
        vm.heap.object_define_data(
            ctor,
            k_concat,
            JsValue::from_object(concat_fn),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }

    define_global(vm, "Iterator", JsValue::from_object(ctor));
}
