//! ES2025 Explicit Resource Management: `SuppressedError`, `DisposableStack`,
//! and `AsyncDisposableStack`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::heap::{DisposableEntry, DisposableStackData, JsObject, ObjectKind, PromiseReaction};
use crate::promise::{alloc_promise, promise_resolve_value, reject_promise, resolve_promise};
use crate::shape::PropertyAttrs;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm};
use crate::vm_builtins::{define_global, define_method_len};

#[inline]
fn arg(args: &[JsValue], i: usize) -> JsValue {
    args.get(i).copied().unwrap_or(JsValue::UNDEFINED)
}

pub fn install_disposable(vm: &mut JsVm) {
    install_suppressed_error(vm);
    install_disposable_stack(vm);
    install_async_disposable_stack(vm);
}

fn proto_from_new_target(
    vm: &mut JsVm,
    default_proto: HeapHandle,
) -> Result<HeapHandle, JsException> {
    if let Some(nt_h) = vm.native_new_target.as_object() {
        let p = vm.get_property_by_key(nt_h, vm.key_prototype)?;
        if let Some(ph) = p.as_object() {
            return Ok(ph);
        }
    }
    Ok(default_proto)
}

fn get_method_opt(
    vm: &mut JsVm,
    obj: HeapHandle,
    key: crate::PropertyKey,
) -> Result<Option<HeapHandle>, JsException> {
    let func = vm.get_property_by_key(obj, key)?;
    if func.is_undefined() || func.is_null() {
        return Ok(None);
    }
    let Some(fh) = func.as_object().filter(|&h| vm.is_callable_handle(h)) else {
        return Err(vm.type_error("Dispose method is not callable"));
    };
    Ok(Some(fh))
}

pub(crate) fn alloc_suppressed_error(
    vm: &mut JsVm,
    error: JsValue,
    suppressed: JsValue,
    message: Option<&str>,
) -> JsValue {
    let obj = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.suppressed_error_proto),
    ));
    if let Some(msg) = message {
        let mv = JsValue::from_string(vm.heap.alloc_string(msg));
        vm.heap.object_define_data(
            obj,
            vm.key_message,
            mv,
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }
    let k_err = vm.heap.intern_key("error");
    let k_sup = vm.heap.intern_key("suppressed");
    vm.heap
        .object_define_data(obj, k_err, error, PropertyAttrs::METHOD, &mut vm.shapes);
    vm.heap.object_define_data(
        obj,
        k_sup,
        suppressed,
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.heap.write_barrier(obj, error);
    vm.heap.write_barrier(obj, suppressed);
    JsValue::from_object(obj)
}

// ---------------------------------------------------------------------------
// SuppressedError
// ---------------------------------------------------------------------------

fn install_suppressed_error(vm: &mut JsVm) {
    let proto = vm.suppressed_error_proto;
    let nv = JsValue::from_string(vm.heap.alloc_string("SuppressedError"));
    let empty = JsValue::from_string(vm.heap.alloc_string(""));
    vm.heap.object_define_data(
        proto,
        vm.key_name,
        nv,
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        proto,
        vm.key_message,
        empty,
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    let ctor = vm.alloc_native_fn("SuppressedError", |vm, _this, args| {
        let obj_proto = proto_from_new_target(vm, vm.suppressed_error_proto)?;
        let err_obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(obj_proto)));
        let error_v = arg(args, 0);
        let suppressed_v = arg(args, 1);
        let msg_v = arg(args, 2);
        if !msg_v.is_undefined() {
            let msg_s = vm.to_js_string(msg_v)?;
            let mv = JsValue::from_string(vm.heap.alloc_string(msg_s));
            vm.heap.object_define_data(
                err_obj,
                vm.key_message,
                mv,
                PropertyAttrs::METHOD,
                &mut vm.shapes,
            );
        }
        let k_err = vm.heap.intern_key("error");
        let k_sup = vm.heap.intern_key("suppressed");
        vm.heap.object_define_data(
            err_obj,
            k_err,
            error_v,
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            err_obj,
            k_sup,
            suppressed_v,
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
        vm.heap.write_barrier(err_obj, error_v);
        vm.heap.write_barrier(err_obj, suppressed_v);
        Ok(JsValue::from_object(err_obj))
    });
    vm.mark_constructor(ctor);
    vm.set_fn_length(ctor, 3);
    let k_error = vm.heap.intern_key("Error");
    if let Ok(err_ctor) = vm.get_property_by_key(vm.global, k_error) {
        if let Some(ec) = err_ctor.as_object() {
            vm.heap.object_set_prototype(ctor, Some(ec), &mut vm.shapes);
        }
    }
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
    define_global(vm, "SuppressedError", JsValue::from_object(ctor));
}

// ---------------------------------------------------------------------------
// Stack slot helpers
// ---------------------------------------------------------------------------

fn require_stack(
    vm: &mut JsVm,
    this_val: JsValue,
    expect_async: bool,
) -> Result<HeapHandle, JsException> {
    if let Some(h) = this_val.as_object() {
        if let Some(ds) = vm
            .heap
            .get_object(h)
            .and_then(|o| o.rare.as_ref())
            .and_then(|r| r.disposable_stack.as_ref())
        {
            if ds.is_async == expect_async {
                return Ok(h);
            }
        }
    }
    let name = if expect_async {
        "AsyncDisposableStack"
    } else {
        "DisposableStack"
    };
    Err(vm.type_error(format!("Incompatible receiver: expected {name}")))
}

fn stack_is_disposed(vm: &JsVm, h: HeapHandle) -> bool {
    vm.heap
        .get_object(h)
        .and_then(|o| o.rare.as_ref())
        .and_then(|r| r.disposable_stack.as_ref())
        .is_none_or(|d| d.disposed)
}

fn require_not_disposed(vm: &mut JsVm, h: HeapHandle) -> Result<(), JsException> {
    if stack_is_disposed(vm, h) {
        return Err(vm.reference_error("DisposableStack has already been disposed"));
    }
    Ok(())
}

fn stack_push(vm: &mut JsVm, h: HeapHandle, entry: DisposableEntry) {
    if let Some(o) = vm.heap.get_object_mut(h) {
        if let Some(ds) = o.rare_mut().disposable_stack.as_mut() {
            ds.entries.push(entry);
        }
    }
    vm.heap.write_barrier(h, entry.value);
    if let Some(m) = entry.method {
        vm.heap.write_barrier(h, JsValue::from_object(m));
    }
}

fn alloc_stack_instance(vm: &mut JsVm, proto: HeapHandle, is_async: bool) -> HeapHandle {
    let h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    if let Some(o) = vm.heap.get_object_mut(h) {
        o.rare_mut().disposable_stack = Some(Box::new(DisposableStackData {
            is_async,
            disposed: false,
            entries: Vec::new(),
        }));
    }
    h
}

// ---------------------------------------------------------------------------
// DisposableStack
// ---------------------------------------------------------------------------

fn install_disposable_stack(vm: &mut JsVm) {
    let proto = vm.disposable_stack_proto;

    let disposed_getter = vm.alloc_native_fn("get disposed", |vm, this_val, _| {
        let h = require_stack(vm, this_val, false)?;
        Ok(JsValue::from_bool(stack_is_disposed(vm, h)))
    });
    vm.set_fn_length(disposed_getter, 0);
    let k_disposed = vm.heap.intern_key("disposed");
    vm.heap.object_define_accessor(
        proto,
        k_disposed,
        Some(disposed_getter),
        None,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    define_method_len(vm, proto, "use", 1, |vm, this_val, args| {
        let h = require_stack(vm, this_val, false)?;
        require_not_disposed(vm, h)?;
        let val = arg(args, 0);
        if val.is_null() || val.is_undefined() {
            return Ok(val);
        }
        let Some(vh) = val.as_object() else {
            return Err(vm.type_error("Disposable resource must be an object"));
        };
        let Some(method) = get_method_opt(vm, vh, vm.key_symbol_dispose)? else {
            return Err(vm.type_error("Object does not implement Symbol.dispose"));
        };
        stack_push(
            vm,
            h,
            DisposableEntry {
                value: val,
                method: Some(method),
                kind: 0,
            },
        );
        Ok(val)
    });

    define_method_len(vm, proto, "adopt", 2, |vm, this_val, args| {
        let h = require_stack(vm, this_val, false)?;
        require_not_disposed(vm, h)?;
        let val = arg(args, 0);
        let on_disp = arg(args, 1);
        let Some(mh) = on_disp.as_object().filter(|&x| vm.is_callable_handle(x)) else {
            return Err(vm.type_error("onDispose callback must be a function"));
        };
        stack_push(
            vm,
            h,
            DisposableEntry {
                value: val,
                method: Some(mh),
                kind: 1,
            },
        );
        Ok(val)
    });

    define_method_len(vm, proto, "defer", 1, |vm, this_val, args| {
        let h = require_stack(vm, this_val, false)?;
        require_not_disposed(vm, h)?;
        let on_disp = arg(args, 0);
        let Some(mh) = on_disp.as_object().filter(|&x| vm.is_callable_handle(x)) else {
            return Err(vm.type_error("onDispose callback must be a function"));
        };
        stack_push(
            vm,
            h,
            DisposableEntry {
                value: JsValue::UNDEFINED,
                method: Some(mh),
                kind: 2,
            },
        );
        Ok(JsValue::UNDEFINED)
    });

    define_method_len(vm, proto, "move", 0, |vm, this_val, _| {
        let h = require_stack(vm, this_val, false)?;
        require_not_disposed(vm, h)?;
        let moved = vm
            .heap
            .get_object_mut(h)
            .and_then(|o| o.rare.as_mut())
            .and_then(|r| r.disposable_stack.as_mut())
            .map(|ds| {
                ds.disposed = true;
                std::mem::take(&mut ds.entries)
            })
            .unwrap_or_default();
        let new_h = alloc_stack_instance(vm, vm.disposable_stack_proto, false);
        for entry in moved {
            stack_push(vm, new_h, entry);
        }
        Ok(JsValue::from_object(new_h))
    });

    let dispose_fn = vm.alloc_native_fn("dispose", |vm, this_val, _| {
        let h = require_stack(vm, this_val, false)?;
        let entries = match vm
            .heap
            .get_object_mut(h)
            .and_then(|o| o.rare.as_mut())
            .and_then(|r| r.disposable_stack.as_mut())
        {
            Some(ds) if !ds.disposed => {
                ds.disposed = true;
                std::mem::take(&mut ds.entries)
            }
            _ => return Ok(JsValue::UNDEFINED),
        };
        let mut pending_err: Option<JsException> = None;
        for entry in entries.into_iter().rev() {
            let Some(mh) = entry.method else {
                continue;
            };
            let res = match entry.kind {
                0 => vm.call_value(JsValue::from_object(mh), entry.value, &[]),
                1 => vm.call_value(JsValue::from_object(mh), JsValue::UNDEFINED, &[entry.value]),
                _ => vm.call_value(JsValue::from_object(mh), JsValue::UNDEFINED, &[]),
            };
            if let Err(err) = res {
                pending_err = Some(match pending_err {
                    None => err,
                    Some(prev) => {
                        let sup = alloc_suppressed_error(
                            vm,
                            err.value,
                            prev.value,
                            Some("An error was suppressed during disposal"),
                        );
                        let msg = vm.format_exception_public(sup);
                        JsException {
                            value: sup,
                            message: msg,
                        }
                    }
                });
            }
        }
        if let Some(err) = pending_err {
            return Err(err);
        }
        Ok(JsValue::UNDEFINED)
    });
    vm.set_fn_length(dispose_fn, 0);
    let k_dispose = vm.heap.intern_key("dispose");
    vm.heap.object_define_data(
        proto,
        k_dispose,
        JsValue::from_object(dispose_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let k_sym_disp = vm.key_symbol_dispose;
    vm.heap.object_define_data(
        proto,
        k_sym_disp,
        JsValue::from_object(dispose_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    let tag = JsValue::from_string(vm.heap.alloc_string("DisposableStack"));
    let k_tag = vm.key_symbol_to_string_tag;
    vm.heap.object_define_data(
        proto,
        k_tag,
        tag,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    let ctor = vm.alloc_native_fn("DisposableStack", |vm, _this, _| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("Constructor DisposableStack requires 'new'"));
        }
        let obj_proto = proto_from_new_target(vm, vm.disposable_stack_proto)?;
        let h = alloc_stack_instance(vm, obj_proto, false);
        Ok(JsValue::from_object(h))
    });
    vm.mark_constructor(ctor);
    vm.set_fn_length(ctor, 0);
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
    define_global(vm, "DisposableStack", JsValue::from_object(ctor));
}

// ---------------------------------------------------------------------------
// AsyncDisposableStack
// ---------------------------------------------------------------------------

fn drive_async_dispose(
    vm: &mut JsVm,
    entries: Rc<RefCell<Vec<DisposableEntry>>>,
    err_slot: Rc<RefCell<Option<JsValue>>>,
    out_p: HeapHandle,
) {
    let record_err = |vm: &mut JsVm, new_err: JsValue| {
        let mut guard = err_slot.borrow_mut();
        let combined = match *guard {
            None => new_err,
            Some(prev) => alloc_suppressed_error(
                vm,
                new_err,
                prev,
                Some("An error was suppressed during disposal"),
            ),
        };
        *guard = Some(combined);
    };

    loop {
        let next_entry = entries.borrow_mut().pop();
        let Some(entry) = next_entry else {
            let final_err = *err_slot.borrow();
            if let Some(e) = final_err {
                reject_promise(vm, out_p, e);
            } else {
                resolve_promise(vm, out_p, JsValue::UNDEFINED);
            }
            return;
        };

        let await_val = match entry.kind {
            4 => JsValue::UNDEFINED,
            3 => {
                let Some(mh) = entry.method else {
                    continue;
                };
                match vm.call_value(JsValue::from_object(mh), entry.value, &[]) {
                    Ok(_) => JsValue::UNDEFINED,
                    Err(e) => {
                        record_err(vm, e.value);
                        continue;
                    }
                }
            }
            kind => {
                let Some(mh) = entry.method else {
                    continue;
                };
                let call_res = match kind {
                    0 => vm.call_value(JsValue::from_object(mh), entry.value, &[]),
                    1 => {
                        vm.call_value(JsValue::from_object(mh), JsValue::UNDEFINED, &[entry.value])
                    }
                    _ => vm.call_value(JsValue::from_object(mh), JsValue::UNDEFINED, &[]),
                };
                match call_res {
                    Ok(v) => v,
                    Err(e) => {
                        record_err(vm, e.value);
                        continue;
                    }
                }
            }
        };

        let p = promise_resolve_value(vm, await_val);
        let e1 = Rc::clone(&entries);
        let s1 = Rc::clone(&err_slot);
        let on_ful = vm.alloc_host_fn("", move |vm, _, _| {
            drive_async_dispose(vm, Rc::clone(&e1), Rc::clone(&s1), out_p);
            Ok(JsValue::UNDEFINED)
        });
        let e2 = Rc::clone(&entries);
        let s2 = Rc::clone(&err_slot);
        let on_rej = vm.alloc_host_fn("", move |vm, _, args| {
            let reason = arg(args, 0);
            {
                let mut guard = s2.borrow_mut();
                let combined = match *guard {
                    None => reason,
                    Some(prev) => alloc_suppressed_error(
                        vm,
                        reason,
                        prev,
                        Some("An error was suppressed during disposal"),
                    ),
                };
                *guard = Some(combined);
            }
            drive_async_dispose(vm, Rc::clone(&e2), Rc::clone(&s2), out_p);
            Ok(JsValue::UNDEFINED)
        });
        crate::promise::attach_promise_reaction(
            vm,
            p,
            PromiseReaction {
                on_fulfilled: Some(on_ful),
                on_rejected: Some(on_rej),
                result_promise: None,
            },
        );
        return;
    }
}

fn install_async_disposable_stack(vm: &mut JsVm) {
    let proto = vm.async_disposable_stack_proto;

    let disposed_getter = vm.alloc_native_fn("get disposed", |vm, this_val, _| {
        let h = require_stack(vm, this_val, true)?;
        Ok(JsValue::from_bool(stack_is_disposed(vm, h)))
    });
    vm.set_fn_length(disposed_getter, 0);
    let k_disposed = vm.heap.intern_key("disposed");
    vm.heap.object_define_accessor(
        proto,
        k_disposed,
        Some(disposed_getter),
        None,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    define_method_len(vm, proto, "use", 1, |vm, this_val, args| {
        let h = require_stack(vm, this_val, true)?;
        require_not_disposed(vm, h)?;
        let val = arg(args, 0);
        if val.is_null() || val.is_undefined() {
            stack_push(
                vm,
                h,
                DisposableEntry {
                    value: JsValue::UNDEFINED,
                    method: None,
                    kind: 4,
                },
            );
            return Ok(val);
        }
        let Some(vh) = val.as_object() else {
            return Err(vm.type_error("Async disposable resource must be an object"));
        };
        if let Some(async_m) = get_method_opt(vm, vh, vm.key_symbol_async_dispose)? {
            stack_push(
                vm,
                h,
                DisposableEntry {
                    value: val,
                    method: Some(async_m),
                    kind: 0,
                },
            );
            return Ok(val);
        }
        if let Some(sync_m) = get_method_opt(vm, vh, vm.key_symbol_dispose)? {
            stack_push(
                vm,
                h,
                DisposableEntry {
                    value: val,
                    method: Some(sync_m),
                    kind: 3,
                },
            );
            return Ok(val);
        }
        Err(vm.type_error("Object does not implement Symbol.asyncDispose or Symbol.dispose"))
    });

    define_method_len(vm, proto, "adopt", 2, |vm, this_val, args| {
        let h = require_stack(vm, this_val, true)?;
        require_not_disposed(vm, h)?;
        let val = arg(args, 0);
        let on_disp = arg(args, 1);
        let Some(mh) = on_disp.as_object().filter(|&x| vm.is_callable_handle(x)) else {
            return Err(vm.type_error("onDisposeAsync callback must be a function"));
        };
        stack_push(
            vm,
            h,
            DisposableEntry {
                value: val,
                method: Some(mh),
                kind: 1,
            },
        );
        Ok(val)
    });

    define_method_len(vm, proto, "defer", 1, |vm, this_val, args| {
        let h = require_stack(vm, this_val, true)?;
        require_not_disposed(vm, h)?;
        let on_disp = arg(args, 0);
        let Some(mh) = on_disp.as_object().filter(|&x| vm.is_callable_handle(x)) else {
            return Err(vm.type_error("onDisposeAsync callback must be a function"));
        };
        stack_push(
            vm,
            h,
            DisposableEntry {
                value: JsValue::UNDEFINED,
                method: Some(mh),
                kind: 2,
            },
        );
        Ok(JsValue::UNDEFINED)
    });

    define_method_len(vm, proto, "move", 0, |vm, this_val, _| {
        let h = require_stack(vm, this_val, true)?;
        require_not_disposed(vm, h)?;
        let moved = vm
            .heap
            .get_object_mut(h)
            .and_then(|o| o.rare.as_mut())
            .and_then(|r| r.disposable_stack.as_mut())
            .map(|ds| {
                ds.disposed = true;
                std::mem::take(&mut ds.entries)
            })
            .unwrap_or_default();
        let new_h = alloc_stack_instance(vm, vm.async_disposable_stack_proto, true);
        for entry in moved {
            stack_push(vm, new_h, entry);
        }
        Ok(JsValue::from_object(new_h))
    });

    let dispose_async_fn = vm.alloc_native_fn("disposeAsync", |vm, this_val, _| {
        let out_p = alloc_promise(vm);
        let h = match require_stack(vm, this_val, true) {
            Ok(h) => h,
            Err(e) => {
                reject_promise(vm, out_p, e.value);
                return Ok(JsValue::from_object(out_p));
            }
        };
        let entries = match vm
            .heap
            .get_object_mut(h)
            .and_then(|o| o.rare.as_mut())
            .and_then(|r| r.disposable_stack.as_mut())
        {
            Some(ds) if !ds.disposed => {
                ds.disposed = true;
                std::mem::take(&mut ds.entries)
            }
            _ => {
                resolve_promise(vm, out_p, JsValue::UNDEFINED);
                return Ok(JsValue::from_object(out_p));
            }
        };
        drive_async_dispose(
            vm,
            Rc::new(RefCell::new(entries)),
            Rc::new(RefCell::new(None)),
            out_p,
        );
        Ok(JsValue::from_object(out_p))
    });
    vm.set_fn_length(dispose_async_fn, 0);
    let k_da = vm.heap.intern_key("disposeAsync");
    vm.heap.object_define_data(
        proto,
        k_da,
        JsValue::from_object(dispose_async_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let k_sym_ad = vm.key_symbol_async_dispose;
    vm.heap.object_define_data(
        proto,
        k_sym_ad,
        JsValue::from_object(dispose_async_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    let tag = JsValue::from_string(vm.heap.alloc_string("AsyncDisposableStack"));
    let k_tag = vm.key_symbol_to_string_tag;
    vm.heap.object_define_data(
        proto,
        k_tag,
        tag,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    let ctor = vm.alloc_native_fn("AsyncDisposableStack", |vm, _this, _| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("Constructor AsyncDisposableStack requires 'new'"));
        }
        let obj_proto = proto_from_new_target(vm, vm.async_disposable_stack_proto)?;
        let h = alloc_stack_instance(vm, obj_proto, true);
        Ok(JsValue::from_object(h))
    });
    vm.mark_constructor(ctor);
    vm.set_fn_length(ctor, 0);
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
    define_global(vm, "AsyncDisposableStack", JsValue::from_object(ctor));
}
