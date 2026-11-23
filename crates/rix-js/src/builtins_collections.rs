//! ES2024 Collections (`Map`, `Set`, `WeakMap`, `WeakSet`, `WeakRef`) and
//! Binary Data (`ArrayBuffer`, `DataView`, and all 9 `TypedArray` constructors).
//!
//! Reference: Spec 05 §6 & Issue #15.

use crate::heap::{DataViewObject, JsObject, ObjectKind, TypedArrayKind, TypedArrayObject};
use crate::shape::PropertyAttrs;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm, NativeFnPtr};

/// Installs `Map`, `Set`, `WeakMap`, `WeakSet`, `WeakRef`, `ArrayBuffer`,
/// `DataView`, and all 9 `TypedArray` constructors on the VM global object.
pub fn install_collection_builtins(vm: &mut JsVm) {
    install_map(vm);
    install_set(vm);
    install_weak_map(vm);
    install_weak_set(vm);
    install_weak_ref(vm);
    install_array_buffer(vm);
    install_data_view(vm);
    install_typed_arrays(vm);
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

fn define_getter(vm: &mut JsVm, target: HeapHandle, name: &str, getter: NativeFnPtr) {
    let key = vm.heap.intern_key(name);
    let g_fn = vm.alloc_native_fn(name, getter);
    vm.heap.object_define_accessor(
        target,
        key,
        Some(g_fn),
        None,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
}

fn define_symbol_iterator(vm: &mut JsVm, target: HeapHandle, f: NativeFnPtr) {
    let sym_key = vm.key_symbol_iterator;
    let func = vm.alloc_native_fn("[Symbol.iterator]", f);
    vm.heap.object_define_data(
        target,
        sym_key,
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

fn bind_ctor_proto(vm: &mut JsVm, ctor: HeapHandle, proto: HeapHandle) {
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
}

#[inline]
fn arg(args: &[JsValue], idx: usize) -> JsValue {
    args.get(idx).copied().unwrap_or(JsValue::UNDEFINED)
}

/// ECMAScript `SameValueZero` comparison (`NaN === NaN`, `+0 === -0`).
#[must_use]
pub fn same_value_zero(vm: &JsVm, a: JsValue, b: JsValue) -> bool {
    if let (Some(fa), Some(fb)) = (a.as_number(), b.as_number()) {
        if fa.is_nan() && fb.is_nan() {
            return true;
        }
        return fa == fb;
    }
    vm.strict_eq(a, b)
}

/// Creates a JS iterable iterator object over a snapshot of `JsValue` items.
pub fn alloc_values_iterator(vm: &mut JsVm, items: Vec<JsValue>) -> HeapHandle {
    let iter_obj = vm.alloc_ordinary_object();
    let arr = vm.alloc_array();
    for item in items {
        vm.heap.object_push_element(arr, item);
    }
    let k_arr = vm.heap.intern_key("__iter_items");
    let k_idx = vm.heap.intern_key("__iter_idx");
    vm.heap.object_define_data(
        iter_obj,
        k_arr,
        JsValue::from_object(arr),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        iter_obj,
        k_idx,
        JsValue::from_i32(0),
        PropertyAttrs::WRITABLE,
        &mut vm.shapes,
    );
    define_method(vm, iter_obj, "next", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Iterator.next requires an object receiver"));
        };
        let k_arr = vm.heap.intern_key("__iter_items");
        let k_idx = vm.heap.intern_key("__iter_idx");
        let arr_h = vm
            .heap
            .object_get_own(h, k_arr, &vm.shapes)
            .and_then(|p| p.value.as_object());
        let idx = vm
            .heap
            .object_get_own(h, k_idx, &vm.shapes)
            .and_then(|p| p.value.as_i32())
            .unwrap_or(0) as u32;
        let res = vm.alloc_ordinary_object();
        if let Some(ah) = arr_h {
            let len = vm.heap.get_object(ah).map_or(0, |o| o.elements.len());
            if idx < len {
                let val = vm
                    .heap
                    .get_object(ah)
                    .and_then(|o| o.elements.get(idx))
                    .unwrap_or(JsValue::UNDEFINED);
                vm.heap.object_define_data(
                    h,
                    k_idx,
                    JsValue::from_i32((idx + 1) as i32),
                    PropertyAttrs::WRITABLE,
                    &mut vm.shapes,
                );
                vm.heap.object_define_data(
                    res,
                    vm.key_value,
                    val,
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
                vm.heap.object_define_data(
                    res,
                    vm.key_done,
                    JsValue::FALSE,
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
                return Ok(JsValue::from_object(res));
            }
        }
        vm.heap.object_define_data(
            res,
            vm.key_value,
            JsValue::UNDEFINED,
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            res,
            vm.key_done,
            JsValue::TRUE,
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        Ok(JsValue::from_object(res))
    });
    define_symbol_iterator(vm, iter_obj, |_vm, this_val, _| Ok(this_val));
    iter_obj
}

// ---------------------------------------------------------------------------
// Map
// ---------------------------------------------------------------------------

fn require_map(vm: &mut JsVm, this_val: JsValue) -> Result<HeapHandle, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Method called on incompatible non-Map receiver"));
    };
    if matches!(
        vm.heap.get_object(h).map(|o| &o.kind),
        Some(ObjectKind::Map(_))
    ) {
        Ok(h)
    } else {
        Err(vm.type_error("Method called on incompatible non-Map receiver"))
    }
}

fn map_set_entry(vm: &mut JsVm, map_h: HeapHandle, key: JsValue, val: JsValue) {
    // Normalize -0.0 to +0.0 per ECMAScript spec.
    let norm_key = if key.as_number() == Some(0.0) {
        JsValue::from_i32(0)
    } else {
        key
    };
    let existing_idx = match vm.heap.get_object(map_h).map(|o| &o.kind) {
        Some(ObjectKind::Map(entries)) => entries
            .iter()
            .position(|(k, _)| same_value_zero(vm, *k, norm_key)),
        _ => None,
    };
    if let Some(ObjectKind::Map(entries)) = vm.heap.get_object_mut(map_h).map(|o| &mut o.kind) {
        if let Some(i) = existing_idx {
            entries[i].1 = val;
        } else {
            entries.push((norm_key, val));
        }
    }
    vm.heap.write_barrier(map_h, norm_key);
    vm.heap.write_barrier(map_h, val);
}

fn install_map(vm: &mut JsVm) {
    let mp = vm.map_proto;
    define_getter(vm, mp, "size", |vm, this_val, _| {
        let h = require_map(vm, this_val)?;
        let len = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::Map(e)) => e.len() as f64,
            _ => 0.0,
        };
        Ok(JsValue::from_f64(len))
    });
    define_method(vm, mp, "get", |vm, this_val, args| {
        let h = require_map(vm, this_val)?;
        let key = arg(args, 0);
        let found = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::Map(e)) => e
                .iter()
                .find(|(k, _)| same_value_zero(vm, *k, key))
                .map(|(_, v)| *v),
            _ => None,
        };
        Ok(found.unwrap_or(JsValue::UNDEFINED))
    });
    define_method(vm, mp, "has", |vm, this_val, args| {
        let h = require_map(vm, this_val)?;
        let key = arg(args, 0);
        let found = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::Map(e)) => e.iter().any(|(k, _)| same_value_zero(vm, *k, key)),
            _ => false,
        };
        Ok(JsValue::from_bool(found))
    });
    define_method(vm, mp, "set", |vm, this_val, args| {
        let h = require_map(vm, this_val)?;
        map_set_entry(vm, h, arg(args, 0), arg(args, 1));
        Ok(this_val)
    });
    define_method(vm, mp, "delete", |vm, this_val, args| {
        let h = require_map(vm, this_val)?;
        let key = arg(args, 0);
        let pos = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::Map(e)) => e.iter().position(|(k, _)| same_value_zero(vm, *k, key)),
            _ => None,
        };
        if let Some(i) = pos {
            if let Some(ObjectKind::Map(e)) = vm.heap.get_object_mut(h).map(|o| &mut o.kind) {
                e.remove(i);
            }
            Ok(JsValue::TRUE)
        } else {
            Ok(JsValue::FALSE)
        }
    });
    define_method(vm, mp, "clear", |vm, this_val, _| {
        let h = require_map(vm, this_val)?;
        if let Some(ObjectKind::Map(e)) = vm.heap.get_object_mut(h).map(|o| &mut o.kind) {
            e.clear();
        }
        Ok(JsValue::UNDEFINED)
    });
    define_method(vm, mp, "forEach", |vm, this_val, args| {
        let h = require_map(vm, this_val)?;
        let cb = arg(args, 0);
        let this_arg = arg(args, 1);
        let snapshot: Vec<(JsValue, JsValue)> = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::Map(e)) => (**e).clone(),
            _ => Vec::new(),
        };
        for (k, v) in snapshot {
            vm.call_value(cb, this_arg, &[v, k, this_val])?;
        }
        Ok(JsValue::UNDEFINED)
    });
    define_method(vm, mp, "keys", |vm, this_val, _| {
        let h = require_map(vm, this_val)?;
        let items: Vec<JsValue> = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::Map(e)) => e.iter().map(|(k, _)| *k).collect(),
            _ => Vec::new(),
        };
        Ok(JsValue::from_object(alloc_values_iterator(vm, items)))
    });
    define_method(vm, mp, "values", |vm, this_val, _| {
        let h = require_map(vm, this_val)?;
        let items: Vec<JsValue> = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::Map(e)) => e.iter().map(|(_, v)| *v).collect(),
            _ => Vec::new(),
        };
        Ok(JsValue::from_object(alloc_values_iterator(vm, items)))
    });
    define_method(vm, mp, "entries", map_entries_fn);
    define_symbol_iterator(vm, mp, map_entries_fn);

    let ctor = vm.alloc_native_fn("Map", |vm, _this, args| {
        let map_h = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Map(Box::default()),
            Some(vm.map_proto),
        ));
        let iterable = arg(args, 0);
        if !iterable.is_undefined() && !iterable.is_null() {
            let items = vm.collect_iterable(iterable)?;
            for item in items {
                if !item.is_object() {
                    return Err(vm.type_error("Iterator value is not an entry object"));
                }
                let k = vm.get_elem(item, JsValue::from_i32(0))?;
                let v = vm.get_elem(item, JsValue::from_i32(1))?;
                map_set_entry(vm, map_h, k, v);
            }
        }
        Ok(JsValue::from_object(map_h))
    });
    define_method(vm, ctor, "groupBy", |vm, _this, args| {
        let items = vm.collect_iterable(arg(args, 0))?;
        let cb = arg(args, 1);
        let map_h = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Map(Box::default()),
            Some(vm.map_proto),
        ));
        for (i, item) in items.into_iter().enumerate() {
            let group_key =
                vm.call_value(cb, JsValue::UNDEFINED, &[item, JsValue::from_f64(i as f64)])?;
            let existing = match vm.heap.get_object(map_h).map(|o| &o.kind) {
                Some(ObjectKind::Map(e)) => e
                    .iter()
                    .find(|(k, _)| same_value_zero(vm, *k, group_key))
                    .and_then(|(_, v)| v.as_object()),
                _ => None,
            };
            let arr = if let Some(a) = existing {
                a
            } else {
                let a = vm.alloc_array();
                map_set_entry(vm, map_h, group_key, JsValue::from_object(a));
                a
            };
            vm.heap.object_push_element(arr, item);
        }
        Ok(JsValue::from_object(map_h))
    });
    bind_ctor_proto(vm, ctor, mp);
    define_global(vm, "Map", JsValue::from_object(ctor));
}

fn map_entries_fn(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = require_map(vm, this_val)?;
    let snapshot: Vec<(JsValue, JsValue)> = match vm.heap.get_object(h).map(|o| &o.kind) {
        Some(ObjectKind::Map(e)) => (**e).clone(),
        _ => Vec::new(),
    };
    let mut pairs = Vec::with_capacity(snapshot.len());
    for (k, v) in snapshot {
        let pair = vm.alloc_array();
        vm.heap.object_push_element(pair, k);
        vm.heap.object_push_element(pair, v);
        pairs.push(JsValue::from_object(pair));
    }
    Ok(JsValue::from_object(alloc_values_iterator(vm, pairs)))
}

// ---------------------------------------------------------------------------
// Set (including ES2024 set operations)
// ---------------------------------------------------------------------------

fn require_set(vm: &mut JsVm, this_val: JsValue) -> Result<HeapHandle, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Method called on incompatible non-Set receiver"));
    };
    if matches!(
        vm.heap.get_object(h).map(|o| &o.kind),
        Some(ObjectKind::Set(_))
    ) {
        Ok(h)
    } else {
        Err(vm.type_error("Method called on incompatible non-Set receiver"))
    }
}

fn set_add_entry(vm: &mut JsVm, set_h: HeapHandle, val: JsValue) {
    let norm = if val.as_number() == Some(0.0) {
        JsValue::from_i32(0)
    } else {
        val
    };
    let exists = match vm.heap.get_object(set_h).map(|o| &o.kind) {
        Some(ObjectKind::Set(e)) => e.iter().any(|v| same_value_zero(vm, *v, norm)),
        _ => false,
    };
    if !exists {
        if let Some(ObjectKind::Set(e)) = vm.heap.get_object_mut(set_h).map(|o| &mut o.kind) {
            e.push(norm);
        }
        vm.heap.write_barrier(set_h, norm);
    }
}

fn set_snapshot(vm: &JsVm, h: HeapHandle) -> Vec<JsValue> {
    match vm.heap.get_object(h).map(|o| &o.kind) {
        Some(ObjectKind::Set(e)) => (**e).clone(),
        _ => Vec::new(),
    }
}

fn set_like_items(vm: &mut JsVm, other: JsValue) -> Result<Vec<JsValue>, JsException> {
    if let Some(oh) = other.as_object() {
        if matches!(
            vm.heap.get_object(oh).map(|o| &o.kind),
            Some(ObjectKind::Set(_))
        ) {
            return Ok(set_snapshot(vm, oh));
        }
    }
    vm.collect_iterable(other)
}

fn install_set(vm: &mut JsVm) {
    let sp = vm.set_proto;
    define_getter(vm, sp, "size", |vm, this_val, _| {
        let h = require_set(vm, this_val)?;
        let len = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::Set(e)) => e.len() as f64,
            _ => 0.0,
        };
        Ok(JsValue::from_f64(len))
    });
    define_method(vm, sp, "add", |vm, this_val, args| {
        let h = require_set(vm, this_val)?;
        set_add_entry(vm, h, arg(args, 0));
        Ok(this_val)
    });
    define_method(vm, sp, "has", |vm, this_val, args| {
        let h = require_set(vm, this_val)?;
        let val = arg(args, 0);
        let found = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::Set(e)) => e.iter().any(|v| same_value_zero(vm, *v, val)),
            _ => false,
        };
        Ok(JsValue::from_bool(found))
    });
    define_method(vm, sp, "delete", |vm, this_val, args| {
        let h = require_set(vm, this_val)?;
        let val = arg(args, 0);
        let pos = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::Set(e)) => e.iter().position(|v| same_value_zero(vm, *v, val)),
            _ => None,
        };
        if let Some(i) = pos {
            if let Some(ObjectKind::Set(e)) = vm.heap.get_object_mut(h).map(|o| &mut o.kind) {
                e.remove(i);
            }
            Ok(JsValue::TRUE)
        } else {
            Ok(JsValue::FALSE)
        }
    });
    define_method(vm, sp, "clear", |vm, this_val, _| {
        let h = require_set(vm, this_val)?;
        if let Some(ObjectKind::Set(e)) = vm.heap.get_object_mut(h).map(|o| &mut o.kind) {
            e.clear();
        }
        Ok(JsValue::UNDEFINED)
    });
    define_method(vm, sp, "forEach", |vm, this_val, args| {
        let h = require_set(vm, this_val)?;
        let cb = arg(args, 0);
        let this_arg = arg(args, 1);
        for v in set_snapshot(vm, h) {
            vm.call_value(cb, this_arg, &[v, v, this_val])?;
        }
        Ok(JsValue::UNDEFINED)
    });
    define_method(vm, sp, "values", set_values_fn);
    define_method(vm, sp, "keys", set_values_fn);
    define_symbol_iterator(vm, sp, set_values_fn);
    define_method(vm, sp, "entries", |vm, this_val, _| {
        let h = require_set(vm, this_val)?;
        let mut pairs = Vec::new();
        for v in set_snapshot(vm, h) {
            let p = vm.alloc_array();
            vm.heap.object_push_element(p, v);
            vm.heap.object_push_element(p, v);
            pairs.push(JsValue::from_object(p));
        }
        Ok(JsValue::from_object(alloc_values_iterator(vm, pairs)))
    });
    // ES2024 Set mathematical methods.
    define_method(vm, sp, "union", |vm, this_val, args| {
        let h = require_set(vm, this_val)?;
        let a = set_snapshot(vm, h);
        let b = set_like_items(vm, arg(args, 0))?;
        let out = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Set(Box::default()),
            Some(vm.set_proto),
        ));
        for v in a.into_iter().chain(b) {
            set_add_entry(vm, out, v);
        }
        Ok(JsValue::from_object(out))
    });
    define_method(vm, sp, "intersection", |vm, this_val, args| {
        let h = require_set(vm, this_val)?;
        let a = set_snapshot(vm, h);
        let b = set_like_items(vm, arg(args, 0))?;
        let out = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Set(Box::default()),
            Some(vm.set_proto),
        ));
        for v in a {
            if b.iter().any(|bv| same_value_zero(vm, v, *bv)) {
                set_add_entry(vm, out, v);
            }
        }
        Ok(JsValue::from_object(out))
    });
    define_method(vm, sp, "difference", |vm, this_val, args| {
        let h = require_set(vm, this_val)?;
        let a = set_snapshot(vm, h);
        let b = set_like_items(vm, arg(args, 0))?;
        let out = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Set(Box::default()),
            Some(vm.set_proto),
        ));
        for v in a {
            if !b.iter().any(|bv| same_value_zero(vm, v, *bv)) {
                set_add_entry(vm, out, v);
            }
        }
        Ok(JsValue::from_object(out))
    });
    define_method(vm, sp, "symmetricDifference", |vm, this_val, args| {
        let h = require_set(vm, this_val)?;
        let a = set_snapshot(vm, h);
        let b = set_like_items(vm, arg(args, 0))?;
        let out = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Set(Box::default()),
            Some(vm.set_proto),
        ));
        for &v in &a {
            if !b.iter().any(|bv| same_value_zero(vm, v, *bv)) {
                set_add_entry(vm, out, v);
            }
        }
        for &v in &b {
            if !a.iter().any(|av| same_value_zero(vm, v, *av)) {
                set_add_entry(vm, out, v);
            }
        }
        Ok(JsValue::from_object(out))
    });
    define_method(vm, sp, "isSubsetOf", |vm, this_val, args| {
        let h = require_set(vm, this_val)?;
        let a = set_snapshot(vm, h);
        let b = set_like_items(vm, arg(args, 0))?;
        Ok(JsValue::from_bool(a.iter().all(|av| {
            b.iter().any(|bv| same_value_zero(vm, *av, *bv))
        })))
    });
    define_method(vm, sp, "isSupersetOf", |vm, this_val, args| {
        let h = require_set(vm, this_val)?;
        let a = set_snapshot(vm, h);
        let b = set_like_items(vm, arg(args, 0))?;
        Ok(JsValue::from_bool(b.iter().all(|bv| {
            a.iter().any(|av| same_value_zero(vm, *av, *bv))
        })))
    });
    define_method(vm, sp, "isDisjointFrom", |vm, this_val, args| {
        let h = require_set(vm, this_val)?;
        let a = set_snapshot(vm, h);
        let b = set_like_items(vm, arg(args, 0))?;
        Ok(JsValue::from_bool(!a.iter().any(|av| {
            b.iter().any(|bv| same_value_zero(vm, *av, *bv))
        })))
    });

    let ctor = vm.alloc_native_fn("Set", |vm, _this, args| {
        let set_h = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Set(Box::default()),
            Some(vm.set_proto),
        ));
        let iterable = arg(args, 0);
        if !iterable.is_undefined() && !iterable.is_null() {
            for item in vm.collect_iterable(iterable)? {
                set_add_entry(vm, set_h, item);
            }
        }
        Ok(JsValue::from_object(set_h))
    });
    bind_ctor_proto(vm, ctor, sp);
    define_global(vm, "Set", JsValue::from_object(ctor));
}

fn set_values_fn(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = require_set(vm, this_val)?;
    let items = set_snapshot(vm, h);
    Ok(JsValue::from_object(alloc_values_iterator(vm, items)))
}

// ---------------------------------------------------------------------------
// WeakMap, WeakSet, WeakRef (true GC-integrated weak references)
// ---------------------------------------------------------------------------

fn require_weakmap(vm: &mut JsVm, this_val: JsValue) -> Result<HeapHandle, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Incompatible WeakMap receiver"));
    };
    if matches!(
        vm.heap.get_object(h).map(|o| &o.kind),
        Some(ObjectKind::WeakMap(_))
    ) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible WeakMap receiver"))
    }
}

fn install_weak_map(vm: &mut JsVm) {
    let wp = vm.weakmap_proto;
    define_method(vm, wp, "get", |vm, this_val, args| {
        let h = require_weakmap(vm, this_val)?;
        let Some(k) = arg(args, 0).as_object() else {
            return Ok(JsValue::UNDEFINED);
        };
        if !vm.heap.is_alive(k) {
            return Ok(JsValue::UNDEFINED);
        }
        let res = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::WeakMap(m)) => m.get(&k).copied(),
            _ => None,
        };
        Ok(res.unwrap_or(JsValue::UNDEFINED))
    });
    define_method(vm, wp, "has", |vm, this_val, args| {
        let h = require_weakmap(vm, this_val)?;
        let Some(k) = arg(args, 0).as_object() else {
            return Ok(JsValue::FALSE);
        };
        let found = vm.heap.is_alive(k)
            && match vm.heap.get_object(h).map(|o| &o.kind) {
                Some(ObjectKind::WeakMap(m)) => m.contains_key(&k),
                _ => false,
            };
        Ok(JsValue::from_bool(found))
    });
    define_method(vm, wp, "set", |vm, this_val, args| {
        let h = require_weakmap(vm, this_val)?;
        let Some(k) = arg(args, 0).as_object() else {
            return Err(vm.type_error("Invalid value used as WeakMap key"));
        };
        let v = arg(args, 1);
        if let Some(ObjectKind::WeakMap(m)) = vm.heap.get_object_mut(h).map(|o| &mut o.kind) {
            m.insert(k, v);
        }
        vm.heap.write_barrier(h, v);
        Ok(this_val)
    });
    define_method(vm, wp, "delete", |vm, this_val, args| {
        let h = require_weakmap(vm, this_val)?;
        let Some(k) = arg(args, 0).as_object() else {
            return Ok(JsValue::FALSE);
        };
        let removed = match vm.heap.get_object_mut(h).map(|o| &mut o.kind) {
            Some(ObjectKind::WeakMap(m)) => m.remove(&k).is_some(),
            _ => false,
        };
        Ok(JsValue::from_bool(removed))
    });

    let ctor = vm.alloc_native_fn("WeakMap", |vm, _this, args| {
        let wm = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::WeakMap(Box::default()),
            Some(vm.weakmap_proto),
        ));
        let iterable = arg(args, 0);
        if !iterable.is_undefined() && !iterable.is_null() {
            for item in vm.collect_iterable(iterable)? {
                let k = vm.get_elem(item, JsValue::from_i32(0))?;
                let Some(kh) = k.as_object() else {
                    return Err(vm.type_error("Invalid value used as WeakMap key"));
                };
                let v = vm.get_elem(item, JsValue::from_i32(1))?;
                if let Some(ObjectKind::WeakMap(m)) =
                    vm.heap.get_object_mut(wm).map(|o| &mut o.kind)
                {
                    m.insert(kh, v);
                }
                vm.heap.write_barrier(wm, v);
            }
        }
        Ok(JsValue::from_object(wm))
    });
    bind_ctor_proto(vm, ctor, wp);
    define_global(vm, "WeakMap", JsValue::from_object(ctor));
}

fn require_weakset(vm: &mut JsVm, this_val: JsValue) -> Result<HeapHandle, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Incompatible WeakSet receiver"));
    };
    if matches!(
        vm.heap.get_object(h).map(|o| &o.kind),
        Some(ObjectKind::WeakSet(_))
    ) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible WeakSet receiver"))
    }
}

fn install_weak_set(vm: &mut JsVm) {
    let wp = vm.weakset_proto;
    define_method(vm, wp, "add", |vm, this_val, args| {
        let h = require_weakset(vm, this_val)?;
        let Some(k) = arg(args, 0).as_object() else {
            return Err(vm.type_error("Invalid value used in WeakSet"));
        };
        if let Some(ObjectKind::WeakSet(s)) = vm.heap.get_object_mut(h).map(|o| &mut o.kind) {
            s.insert(k);
        }
        Ok(this_val)
    });
    define_method(vm, wp, "has", |vm, this_val, args| {
        let h = require_weakset(vm, this_val)?;
        let Some(k) = arg(args, 0).as_object() else {
            return Ok(JsValue::FALSE);
        };
        let found = vm.heap.is_alive(k)
            && match vm.heap.get_object(h).map(|o| &o.kind) {
                Some(ObjectKind::WeakSet(s)) => s.contains(&k),
                _ => false,
            };
        Ok(JsValue::from_bool(found))
    });
    define_method(vm, wp, "delete", |vm, this_val, args| {
        let h = require_weakset(vm, this_val)?;
        let Some(k) = arg(args, 0).as_object() else {
            return Ok(JsValue::FALSE);
        };
        let removed = match vm.heap.get_object_mut(h).map(|o| &mut o.kind) {
            Some(ObjectKind::WeakSet(s)) => s.remove(&k),
            _ => false,
        };
        Ok(JsValue::from_bool(removed))
    });

    let ctor = vm.alloc_native_fn("WeakSet", |vm, _this, args| {
        let ws = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::WeakSet(Box::default()),
            Some(vm.weakset_proto),
        ));
        let iterable = arg(args, 0);
        if !iterable.is_undefined() && !iterable.is_null() {
            for item in vm.collect_iterable(iterable)? {
                let Some(kh) = item.as_object() else {
                    return Err(vm.type_error("Invalid value used in WeakSet"));
                };
                if let Some(ObjectKind::WeakSet(s)) =
                    vm.heap.get_object_mut(ws).map(|o| &mut o.kind)
                {
                    s.insert(kh);
                }
            }
        }
        Ok(JsValue::from_object(ws))
    });
    bind_ctor_proto(vm, ctor, wp);
    define_global(vm, "WeakSet", JsValue::from_object(ctor));
}

fn install_weak_ref(vm: &mut JsVm) {
    let rp = vm.weakref_proto;
    define_method(vm, rp, "deref", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Incompatible WeakRef receiver"));
        };
        let target = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::WeakRef(t)) => *t,
            _ => return Err(vm.type_error("Incompatible WeakRef receiver")),
        };
        if vm.heap.is_alive(target) {
            Ok(JsValue::from_object(target))
        } else {
            Ok(JsValue::UNDEFINED)
        }
    });

    let ctor = vm.alloc_native_fn("WeakRef", |vm, _this, args| {
        let Some(target) = arg(args, 0).as_object() else {
            return Err(vm.type_error("WeakRef target must be an object"));
        };
        let wr = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::WeakRef(target),
            Some(vm.weakref_proto),
        ));
        Ok(JsValue::from_object(wr))
    });
    bind_ctor_proto(vm, ctor, rp);
    define_global(vm, "WeakRef", JsValue::from_object(ctor));
}

// ---------------------------------------------------------------------------
// ArrayBuffer
// ---------------------------------------------------------------------------

fn require_array_buffer(vm: &mut JsVm, val: JsValue) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Expected ArrayBuffer"));
    };
    if matches!(
        vm.heap.get_object(h).map(|o| &o.kind),
        Some(ObjectKind::ArrayBuffer(_))
    ) {
        Ok(h)
    } else {
        Err(vm.type_error("Expected ArrayBuffer"))
    }
}

pub fn alloc_array_buffer(vm: &mut JsVm, bytes: Vec<u8>) -> HeapHandle {
    vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::ArrayBuffer(Box::new(bytes)),
        Some(vm.array_buffer_proto),
    ))
}

fn clamp_slice_index(idx: i64, len: usize) -> usize {
    let len_i = len as i64;
    if idx < 0 {
        (len_i + idx).max(0) as usize
    } else {
        idx.min(len_i) as usize
    }
}

fn install_array_buffer(vm: &mut JsVm) {
    let abp = vm.array_buffer_proto;
    define_getter(vm, abp, "byteLength", |vm, this_val, _| {
        let h = require_array_buffer(vm, this_val)?;
        let len = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::ArrayBuffer(b)) => b.len() as f64,
            _ => 0.0,
        };
        Ok(JsValue::from_f64(len))
    });
    define_method(vm, abp, "slice", |vm, this_val, args| {
        let h = require_array_buffer(vm, this_val)?;
        let len = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::ArrayBuffer(b)) => b.len(),
            _ => 0,
        };
        let start_n = if arg(args, 0).is_undefined() {
            0
        } else {
            vm.to_number(arg(args, 0))?.trunc() as i64
        };
        let end_n = if arg(args, 1).is_undefined() {
            len as i64
        } else {
            vm.to_number(arg(args, 1))?.trunc() as i64
        };
        let start = clamp_slice_index(start_n, len);
        let end = clamp_slice_index(end_n, len).max(start);
        let sliced = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::ArrayBuffer(b)) => b[start..end].to_vec(),
            _ => Vec::new(),
        };
        Ok(JsValue::from_object(alloc_array_buffer(vm, sliced)))
    });

    let ctor = vm.alloc_native_fn("ArrayBuffer", |vm, _this, args| {
        let raw_len = if arg(args, 0).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 0))?
        };
        if !raw_len.is_finite() || raw_len < 0.0 || raw_len > 64.0 * 1024.0 * 1024.0 {
            return Err(vm.range_error("Invalid ArrayBuffer byteLength"));
        }
        let len = raw_len.trunc() as usize;
        Ok(JsValue::from_object(alloc_array_buffer(vm, vec![0u8; len])))
    });
    define_method(vm, ctor, "isView", |vm, _this, args| {
        let is_view = arg(args, 0).as_object().is_some_and(|h| {
            matches!(
                vm.heap.get_object(h).map(|o| &o.kind),
                Some(ObjectKind::DataView(_) | ObjectKind::TypedArray(_))
            )
        });
        Ok(JsValue::from_bool(is_view))
    });
    bind_ctor_proto(vm, ctor, abp);
    define_global(vm, "ArrayBuffer", JsValue::from_object(ctor));
}

// ---------------------------------------------------------------------------
// DataView
// ---------------------------------------------------------------------------

fn require_data_view(vm: &mut JsVm, val: JsValue) -> Result<DataViewObject, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible DataView receiver"));
    };
    match vm.heap.get_object(h).map(|o| &o.kind) {
        Some(ObjectKind::DataView(dv)) => Ok(*dv),
        _ => Err(vm.type_error("Incompatible DataView receiver")),
    }
}

fn dv_read_bytes<const N: usize>(
    vm: &mut JsVm,
    dv: DataViewObject,
    offset_val: JsValue,
) -> Result<[u8; N], JsException> {
    let off_f = vm.to_number(offset_val)?;
    if !off_f.is_finite() || off_f < 0.0 {
        return Err(vm.range_error("Offset is outside the bounds of the DataView"));
    }
    let off = off_f.trunc() as usize;
    if off.saturating_add(N) > dv.byte_length as usize {
        return Err(vm.range_error("Offset is outside the bounds of the DataView"));
    }
    let abs = dv.byte_offset as usize + off;
    let Some(ObjectKind::ArrayBuffer(buf)) = vm.heap.get_object(dv.buffer).map(|o| &o.kind) else {
        return Err(vm.type_error("Detached ArrayBuffer"));
    };
    if abs + N > buf.len() {
        return Err(vm.range_error("Offset is outside the bounds of the DataView"));
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&buf[abs..abs + N]);
    Ok(out)
}

fn dv_write_bytes<const N: usize>(
    vm: &mut JsVm,
    dv: DataViewObject,
    offset_val: JsValue,
    bytes: [u8; N],
) -> Result<(), JsException> {
    let off_f = vm.to_number(offset_val)?;
    if !off_f.is_finite() || off_f < 0.0 {
        return Err(vm.range_error("Offset is outside the bounds of the DataView"));
    }
    let off = off_f.trunc() as usize;
    if off.saturating_add(N) > dv.byte_length as usize {
        return Err(vm.range_error("Offset is outside the bounds of the DataView"));
    }
    let abs = dv.byte_offset as usize + off;
    let Some(ObjectKind::ArrayBuffer(buf)) = vm.heap.get_object_mut(dv.buffer).map(|o| &mut o.kind)
    else {
        return Err(vm.type_error("Detached ArrayBuffer"));
    };
    if abs + N > buf.len() {
        return Err(vm.range_error("Offset is outside the bounds of the DataView"));
    }
    buf[abs..abs + N].copy_from_slice(&bytes);
    Ok(())
}

fn install_data_view(vm: &mut JsVm) {
    let dvp = vm.data_view_proto;
    define_getter(vm, dvp, "buffer", |vm, this_val, _| {
        let dv = require_data_view(vm, this_val)?;
        Ok(JsValue::from_object(dv.buffer))
    });
    define_getter(vm, dvp, "byteOffset", |vm, this_val, _| {
        let dv = require_data_view(vm, this_val)?;
        Ok(JsValue::from_f64(f64::from(dv.byte_offset)))
    });
    define_getter(vm, dvp, "byteLength", |vm, this_val, _| {
        let dv = require_data_view(vm, this_val)?;
        Ok(JsValue::from_f64(f64::from(dv.byte_length)))
    });
    define_method(vm, dvp, "getInt8", |vm, t, args| {
        let dv = require_data_view(vm, t)?;
        let [b] = dv_read_bytes::<1>(vm, dv, arg(args, 0))?;
        Ok(JsValue::from_i32(i32::from(b as i8)))
    });
    define_method(vm, dvp, "setInt8", |vm, t, args| {
        let dv = require_data_view(vm, t)?;
        let v = vm.to_i32(arg(args, 1))? as i8 as u8;
        dv_write_bytes(vm, dv, arg(args, 0), [v])?;
        Ok(JsValue::UNDEFINED)
    });
    define_method(vm, dvp, "getUint8", |vm, t, args| {
        let dv = require_data_view(vm, t)?;
        let [b] = dv_read_bytes::<1>(vm, dv, arg(args, 0))?;
        Ok(JsValue::from_i32(i32::from(b)))
    });
    define_method(vm, dvp, "setUint8", |vm, t, args| {
        let dv = require_data_view(vm, t)?;
        let v = vm.to_u32(arg(args, 1))? as u8;
        dv_write_bytes(vm, dv, arg(args, 0), [v])?;
        Ok(JsValue::UNDEFINED)
    });
    define_method(vm, dvp, "getInt16", |vm, t, args| {
        let dv = require_data_view(vm, t)?;
        let b = dv_read_bytes::<2>(vm, dv, arg(args, 0))?;
        let le = vm.to_boolean(arg(args, 1));
        let v = if le {
            i16::from_le_bytes(b)
        } else {
            i16::from_be_bytes(b)
        };
        Ok(JsValue::from_i32(i32::from(v)))
    });
    define_method(vm, dvp, "setInt16", |vm, t, args| {
        let dv = require_data_view(vm, t)?;
        let v = vm.to_i32(arg(args, 1))? as i16;
        let le = vm.to_boolean(arg(args, 2));
        let b = if le { v.to_le_bytes() } else { v.to_be_bytes() };
        dv_write_bytes(vm, dv, arg(args, 0), b)?;
        Ok(JsValue::UNDEFINED)
    });
    define_method(vm, dvp, "getUint16", |vm, t, args| {
        let dv = require_data_view(vm, t)?;
        let b = dv_read_bytes::<2>(vm, dv, arg(args, 0))?;
        let le = vm.to_boolean(arg(args, 1));
        let v = if le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        };
        Ok(JsValue::from_i32(i32::from(v)))
    });
    define_method(vm, dvp, "setUint16", |vm, t, args| {
        let dv = require_data_view(vm, t)?;
        let v = vm.to_u32(arg(args, 1))? as u16;
        let le = vm.to_boolean(arg(args, 2));
        let b = if le { v.to_le_bytes() } else { v.to_be_bytes() };
        dv_write_bytes(vm, dv, arg(args, 0), b)?;
        Ok(JsValue::UNDEFINED)
    });
    define_method(vm, dvp, "getInt32", |vm, t, args| {
        let dv = require_data_view(vm, t)?;
        let b = dv_read_bytes::<4>(vm, dv, arg(args, 0))?;
        let le = vm.to_boolean(arg(args, 1));
        let v = if le {
            i32::from_le_bytes(b)
        } else {
            i32::from_be_bytes(b)
        };
        Ok(JsValue::from_i32(v))
    });
    define_method(vm, dvp, "setInt32", |vm, t, args| {
        let dv = require_data_view(vm, t)?;
        let v = vm.to_i32(arg(args, 1))?;
        let le = vm.to_boolean(arg(args, 2));
        let b = if le { v.to_le_bytes() } else { v.to_be_bytes() };
        dv_write_bytes(vm, dv, arg(args, 0), b)?;
        Ok(JsValue::UNDEFINED)
    });
    define_method(vm, dvp, "getUint32", |vm, t, args| {
        let dv = require_data_view(vm, t)?;
        let b = dv_read_bytes::<4>(vm, dv, arg(args, 0))?;
        let le = vm.to_boolean(arg(args, 1));
        let v = if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        };
        Ok(JsValue::from_f64(f64::from(v)))
    });
    define_method(vm, dvp, "setUint32", |vm, t, args| {
        let dv = require_data_view(vm, t)?;
        let v = vm.to_u32(arg(args, 1))?;
        let le = vm.to_boolean(arg(args, 2));
        let b = if le { v.to_le_bytes() } else { v.to_be_bytes() };
        dv_write_bytes(vm, dv, arg(args, 0), b)?;
        Ok(JsValue::UNDEFINED)
    });
    define_method(vm, dvp, "getFloat32", |vm, t, args| {
        let dv = require_data_view(vm, t)?;
        let b = dv_read_bytes::<4>(vm, dv, arg(args, 0))?;
        let le = vm.to_boolean(arg(args, 1));
        let v = if le {
            f32::from_le_bytes(b)
        } else {
            f32::from_be_bytes(b)
        };
        Ok(JsValue::from_f64(f64::from(v)))
    });
    define_method(vm, dvp, "setFloat32", |vm, t, args| {
        let dv = require_data_view(vm, t)?;
        let v = vm.to_number(arg(args, 1))? as f32;
        let le = vm.to_boolean(arg(args, 2));
        let b = if le { v.to_le_bytes() } else { v.to_be_bytes() };
        dv_write_bytes(vm, dv, arg(args, 0), b)?;
        Ok(JsValue::UNDEFINED)
    });
    define_method(vm, dvp, "getFloat64", |vm, t, args| {
        let dv = require_data_view(vm, t)?;
        let b = dv_read_bytes::<8>(vm, dv, arg(args, 0))?;
        let le = vm.to_boolean(arg(args, 1));
        let v = if le {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        };
        Ok(JsValue::from_f64(v))
    });
    define_method(vm, dvp, "setFloat64", |vm, t, args| {
        let dv = require_data_view(vm, t)?;
        let v = vm.to_number(arg(args, 1))?;
        let le = vm.to_boolean(arg(args, 2));
        let b = if le { v.to_le_bytes() } else { v.to_be_bytes() };
        dv_write_bytes(vm, dv, arg(args, 0), b)?;
        Ok(JsValue::UNDEFINED)
    });

    let ctor = vm.alloc_native_fn("DataView", |vm, _this, args| {
        let buf_h = require_array_buffer(vm, arg(args, 0))?;
        let buf_len = match vm.heap.get_object(buf_h).map(|o| &o.kind) {
            Some(ObjectKind::ArrayBuffer(b)) => b.len(),
            _ => 0,
        };
        let off_f = if arg(args, 1).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 1))?
        };
        if !off_f.is_finite() || off_f < 0.0 || (off_f.trunc() as usize) > buf_len {
            return Err(vm.range_error("Start offset is outside the bounds of the buffer"));
        }
        let byte_offset = off_f.trunc() as usize;
        let byte_length = if arg(args, 2).is_undefined() {
            buf_len - byte_offset
        } else {
            let l = vm.to_number(arg(args, 2))?;
            if !l.is_finite() || l < 0.0 || byte_offset + (l.trunc() as usize) > buf_len {
                return Err(vm.range_error("Invalid DataView byteLength"));
            }
            l.trunc() as usize
        };
        let dv = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::DataView(DataViewObject {
                buffer: buf_h,
                byte_offset: byte_offset as u32,
                byte_length: byte_length as u32,
            }),
            Some(vm.data_view_proto),
        ));
        Ok(JsValue::from_object(dv))
    });
    bind_ctor_proto(vm, ctor, dvp);
    define_global(vm, "DataView", JsValue::from_object(ctor));
}

// ---------------------------------------------------------------------------
// TypedArray (all 9 numeric views)
// ---------------------------------------------------------------------------

/// Reads element `index` from a `TypedArray` backing `ArrayBuffer`.
#[must_use]
pub fn typed_array_read_element(vm: &JsVm, ta: TypedArrayObject, index: u32) -> JsValue {
    if index >= ta.length {
        return JsValue::UNDEFINED;
    }
    let Some(ObjectKind::ArrayBuffer(buf)) = vm.heap.get_object(ta.buffer).map(|o| &o.kind) else {
        return JsValue::UNDEFINED;
    };
    let bpe = ta.kind.bytes_per_element();
    let off = ta.byte_offset as usize + (index as usize) * bpe;
    if off + bpe > buf.len() {
        return JsValue::UNDEFINED;
    }
    match ta.kind {
        TypedArrayKind::Int8 => JsValue::from_i32(i32::from(buf[off] as i8)),
        TypedArrayKind::Uint8 | TypedArrayKind::Uint8Clamped => {
            JsValue::from_i32(i32::from(buf[off]))
        }
        TypedArrayKind::Int16 => {
            JsValue::from_i32(i32::from(i16::from_ne_bytes([buf[off], buf[off + 1]])))
        }
        TypedArrayKind::Uint16 => {
            JsValue::from_i32(i32::from(u16::from_ne_bytes([buf[off], buf[off + 1]])))
        }
        TypedArrayKind::Int32 => JsValue::from_i32(i32::from_ne_bytes([
            buf[off],
            buf[off + 1],
            buf[off + 2],
            buf[off + 3],
        ])),
        TypedArrayKind::Uint32 => JsValue::from_f64(f64::from(u32::from_ne_bytes([
            buf[off],
            buf[off + 1],
            buf[off + 2],
            buf[off + 3],
        ]))),
        TypedArrayKind::Float32 => JsValue::from_f64(f64::from(f32::from_ne_bytes([
            buf[off],
            buf[off + 1],
            buf[off + 2],
            buf[off + 3],
        ]))),
        TypedArrayKind::Float64 => JsValue::from_f64(f64::from_ne_bytes([
            buf[off],
            buf[off + 1],
            buf[off + 2],
            buf[off + 3],
            buf[off + 4],
            buf[off + 5],
            buf[off + 6],
            buf[off + 7],
        ])),
    }
}

/// Writes `val` coerced to `ta.kind` at element `index` in the backing `ArrayBuffer`.
pub fn typed_array_write_element(
    vm: &mut JsVm,
    ta: TypedArrayObject,
    index: u32,
    val: JsValue,
) -> Result<(), JsException> {
    if index >= ta.length {
        return Ok(());
    }
    let num = vm.to_number(val)?;
    let bpe = ta.kind.bytes_per_element();
    let off = ta.byte_offset as usize + (index as usize) * bpe;
    let Some(ObjectKind::ArrayBuffer(buf)) = vm.heap.get_object_mut(ta.buffer).map(|o| &mut o.kind)
    else {
        return Ok(());
    };
    if off + bpe > buf.len() {
        return Ok(());
    }
    match ta.kind {
        TypedArrayKind::Int8 => {
            buf[off] = to_int32_bits(num) as i8 as u8;
        }
        TypedArrayKind::Uint8 => {
            buf[off] = to_int32_bits(num) as u8;
        }
        TypedArrayKind::Uint8Clamped => {
            buf[off] = clamp_u8_round_even(num);
        }
        TypedArrayKind::Int16 => {
            buf[off..off + 2].copy_from_slice(&(to_int32_bits(num) as i16).to_ne_bytes());
        }
        TypedArrayKind::Uint16 => {
            buf[off..off + 2].copy_from_slice(&(to_int32_bits(num) as u16).to_ne_bytes());
        }
        TypedArrayKind::Int32 => {
            buf[off..off + 4].copy_from_slice(&to_int32_bits(num).to_ne_bytes());
        }
        TypedArrayKind::Uint32 => {
            buf[off..off + 4].copy_from_slice(&(to_int32_bits(num) as u32).to_ne_bytes());
        }
        TypedArrayKind::Float32 => {
            buf[off..off + 4].copy_from_slice(&(num as f32).to_ne_bytes());
        }
        TypedArrayKind::Float64 => {
            buf[off..off + 8].copy_from_slice(&num.to_ne_bytes());
        }
    }
    Ok(())
}

#[inline]
fn to_int32_bits(n: f64) -> i32 {
    if !n.is_finite() || n == 0.0 {
        0
    } else {
        (n.trunc().rem_euclid(4_294_967_296.0) as u32) as i32
    }
}

/// ECMAScript `ToUint8Clamp` (ties to even).
#[inline]
fn clamp_u8_round_even(n: f64) -> u8 {
    if n.is_nan() || n <= 0.0 {
        return 0;
    }
    if n >= 255.0 {
        return 255;
    }
    let f = n.floor();
    let frac = n - f;
    let rounded = if frac < 0.5 {
        f
    } else if frac > 0.5 || (f as u32) % 2 == 1 {
        f + 1.0
    } else {
        f
    };
    rounded as u8
}

fn require_typed_array(vm: &mut JsVm, val: JsValue) -> Result<TypedArrayObject, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible TypedArray receiver"));
    };
    match vm.heap.get_object(h).map(|o| &o.kind) {
        Some(ObjectKind::TypedArray(ta)) => Ok(*ta),
        _ => Err(vm.type_error("Incompatible TypedArray receiver")),
    }
}

pub fn alloc_typed_array(vm: &mut JsVm, kind: TypedArrayKind, length: u32) -> HeapHandle {
    let byte_len = (length as usize) * kind.bytes_per_element();
    let buf = alloc_array_buffer(vm, vec![0u8; byte_len]);
    let proto = vm.typed_array_protos[kind as usize];
    vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::TypedArray(TypedArrayObject {
            kind,
            buffer: buf,
            byte_offset: 0,
            length,
        }),
        Some(proto),
    ))
}

fn construct_typed_array(
    vm: &mut JsVm,
    kind: TypedArrayKind,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let bpe = kind.bytes_per_element();
    let first = arg(args, 0);
    if first.is_undefined() {
        return Ok(JsValue::from_object(alloc_typed_array(vm, kind, 0)));
    }
    // 1. Over an existing ArrayBuffer (shared backing memory!).
    if let Some(buf_h) = first.as_object() {
        if let Some(ObjectKind::ArrayBuffer(buf)) = vm.heap.get_object(buf_h).map(|o| &o.kind) {
            let buf_len = buf.len();
            let off_f = if arg(args, 1).is_undefined() {
                0.0
            } else {
                vm.to_number(arg(args, 1))?
            };
            if !off_f.is_finite() || off_f < 0.0 || (off_f.trunc() as usize) > buf_len {
                return Err(vm.range_error("TypedArray byteOffset out of range"));
            }
            let byte_offset = off_f.trunc() as usize;
            if !byte_offset.is_multiple_of(bpe) {
                return Err(
                    vm.range_error("TypedArray byteOffset must be a multiple of element size")
                );
            }
            let length = if arg(args, 2).is_undefined() {
                let rem = buf_len - byte_offset;
                if !rem.is_multiple_of(bpe) {
                    return Err(
                        vm.range_error("ArrayBuffer byteLength not aligned to element size")
                    );
                }
                rem / bpe
            } else {
                let l = vm.to_number(arg(args, 2))?;
                if !l.is_finite() || l < 0.0 {
                    return Err(vm.range_error("Invalid TypedArray length"));
                }
                let count = l.trunc() as usize;
                if byte_offset + count * bpe > buf_len {
                    return Err(vm.range_error("TypedArray length exceeds ArrayBuffer bounds"));
                }
                count
            };
            let proto = vm.typed_array_protos[kind as usize];
            let ta = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::TypedArray(TypedArrayObject {
                    kind,
                    buffer: buf_h,
                    byte_offset: byte_offset as u32,
                    length: length as u32,
                }),
                Some(proto),
            ));
            return Ok(JsValue::from_object(ta));
        }

        // 2. Copy from another TypedArray or iterable/array-like.
        let src_ta = match vm.heap.get_object(buf_h).map(|o| &o.kind) {
            Some(ObjectKind::TypedArray(src)) => Some(*src),
            _ => None,
        };
        let src_vals = if let Some(src) = src_ta {
            (0..src.length)
                .map(|i| typed_array_read_element(vm, src, i))
                .collect::<Vec<_>>()
        } else {
            vm.collect_iterable(first)?
        };
        let handle = alloc_typed_array(vm, kind, src_vals.len() as u32);
        let ta = match vm.heap.get_object(handle).map(|o| &o.kind) {
            Some(ObjectKind::TypedArray(t)) => *t,
            _ => unreachable!(),
        };
        for (i, v) in src_vals.into_iter().enumerate() {
            typed_array_write_element(vm, ta, i as u32, v)?;
        }
        return Ok(JsValue::from_object(handle));
    }

    // 3. Length constructor.
    let len_f = vm.to_number(first)?;
    let max_elems = (64 * 1024 * 1024) / bpe;
    if !len_f.is_finite() || len_f < 0.0 || len_f > max_elems as f64 {
        return Err(vm.range_error("Invalid TypedArray length"));
    }
    Ok(JsValue::from_object(alloc_typed_array(
        vm,
        kind,
        len_f.trunc() as u32,
    )))
}

fn install_typed_arrays(vm: &mut JsVm) {
    let tap = vm.typed_array_proto;
    // Shared `%TypedArray%` intrinsic constructor (ECMA-262 §23.2.1):
    // `Object.getPrototypeOf(Int8Array) === %TypedArray%` and `%TypedArray%.prototype === tap`.
    let typed_array_ctor = vm.alloc_native_fn("TypedArray", |vm, _, _| {
        Err(vm.type_error("Abstract class TypedArray not directly constructable"))
    });
    bind_ctor_proto(vm, typed_array_ctor, tap);

    define_getter(vm, tap, "buffer", |vm, t, _| {
        let ta = require_typed_array(vm, t)?;
        Ok(JsValue::from_object(ta.buffer))
    });
    define_getter(vm, tap, "byteOffset", |vm, t, _| {
        let ta = require_typed_array(vm, t)?;
        Ok(JsValue::from_f64(f64::from(ta.byte_offset)))
    });
    define_getter(vm, tap, "byteLength", |vm, t, _| {
        let ta = require_typed_array(vm, t)?;
        Ok(JsValue::from_f64(
            (ta.length as usize * ta.kind.bytes_per_element()) as f64,
        ))
    });
    define_getter(vm, tap, "length", |vm, t, _| {
        let ta = require_typed_array(vm, t)?;
        Ok(JsValue::from_f64(f64::from(ta.length)))
    });
    define_method(vm, tap, "subarray", |vm, t, args| {
        let ta = require_typed_array(vm, t)?;
        let len = ta.length as usize;
        let begin_n = if arg(args, 0).is_undefined() {
            0
        } else {
            vm.to_number(arg(args, 0))?.trunc() as i64
        };
        let end_n = if arg(args, 1).is_undefined() {
            len as i64
        } else {
            vm.to_number(arg(args, 1))?.trunc() as i64
        };
        let begin = clamp_slice_index(begin_n, len);
        let end = clamp_slice_index(end_n, len).max(begin);
        let new_off = ta.byte_offset as usize + begin * ta.kind.bytes_per_element();
        let proto = vm.typed_array_protos[ta.kind as usize];
        let sub = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::TypedArray(TypedArrayObject {
                kind: ta.kind,
                buffer: ta.buffer,
                byte_offset: new_off as u32,
                length: (end - begin) as u32,
            }),
            Some(proto),
        ));
        Ok(JsValue::from_object(sub))
    });
    define_method(vm, tap, "slice", |vm, t, args| {
        let ta = require_typed_array(vm, t)?;
        let len = ta.length as usize;
        let begin_n = if arg(args, 0).is_undefined() {
            0
        } else {
            vm.to_number(arg(args, 0))?.trunc() as i64
        };
        let end_n = if arg(args, 1).is_undefined() {
            len as i64
        } else {
            vm.to_number(arg(args, 1))?.trunc() as i64
        };
        let begin = clamp_slice_index(begin_n, len);
        let end = clamp_slice_index(end_n, len).max(begin);
        let out_h = alloc_typed_array(vm, ta.kind, (end - begin) as u32);
        let out_ta = match vm.heap.get_object(out_h).map(|o| &o.kind) {
            Some(ObjectKind::TypedArray(x)) => *x,
            _ => unreachable!(),
        };
        for i in begin..end {
            let v = typed_array_read_element(vm, ta, i as u32);
            typed_array_write_element(vm, out_ta, (i - begin) as u32, v)?;
        }
        Ok(JsValue::from_object(out_h))
    });
    define_method(vm, tap, "set", |vm, t, args| {
        let ta = require_typed_array(vm, t)?;
        let src = arg(args, 0);
        let off_f = if arg(args, 1).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 1))?
        };
        if !off_f.is_finite() || off_f < 0.0 {
            return Err(vm.range_error("Offset is out of bounds"));
        }
        let off = off_f.trunc() as usize;
        let vals = if let Some(sh) = src.as_object() {
            let sta_opt = match vm.heap.get_object(sh).map(|o| &o.kind) {
                Some(ObjectKind::TypedArray(sta)) => Some(*sta),
                _ => None,
            };
            if let Some(sta) = sta_opt {
                (0..sta.length)
                    .map(|i| typed_array_read_element(vm, sta, i))
                    .collect::<Vec<_>>()
            } else {
                vm.collect_iterable(src)?
            }
        } else {
            return Err(vm.type_error("TypedArray.prototype.set source must be an object"));
        };
        if off + vals.len() > ta.length as usize {
            return Err(vm.range_error("Source is too large for TypedArray"));
        }
        for (i, v) in vals.into_iter().enumerate() {
            typed_array_write_element(vm, ta, (off + i) as u32, v)?;
        }
        Ok(JsValue::UNDEFINED)
    });
    define_method(vm, tap, "fill", |vm, t, args| {
        let ta = require_typed_array(vm, t)?;
        let val = arg(args, 0);
        let len = ta.length as usize;
        let start = if arg(args, 1).is_undefined() {
            0
        } else {
            clamp_slice_index(vm.to_number(arg(args, 1))?.trunc() as i64, len)
        };
        let end = if arg(args, 2).is_undefined() {
            len
        } else {
            clamp_slice_index(vm.to_number(arg(args, 2))?.trunc() as i64, len)
        };
        for i in start..end {
            typed_array_write_element(vm, ta, i as u32, val)?;
        }
        Ok(t)
    });
    define_method(vm, tap, "at", |vm, t, args| {
        let ta = require_typed_array(vm, t)?;
        let idx_f = vm.to_number(arg(args, 0))?.trunc() as i64;
        let len = ta.length as i64;
        let actual = if idx_f < 0 { len + idx_f } else { idx_f };
        if actual < 0 || actual >= len {
            Ok(JsValue::UNDEFINED)
        } else {
            Ok(typed_array_read_element(vm, ta, actual as u32))
        }
    });
    define_method(vm, tap, "indexOf", |vm, t, args| {
        let ta = require_typed_array(vm, t)?;
        let target = arg(args, 0);
        let from = if arg(args, 1).is_undefined() {
            0
        } else {
            clamp_slice_index(
                vm.to_number(arg(args, 1))?.trunc() as i64,
                ta.length as usize,
            )
        };
        for i in from..ta.length as usize {
            let v = typed_array_read_element(vm, ta, i as u32);
            if vm.strict_eq(v, target) {
                return Ok(JsValue::from_i32(i as i32));
            }
        }
        Ok(JsValue::from_i32(-1))
    });
    define_method(vm, tap, "includes", |vm, t, args| {
        let ta = require_typed_array(vm, t)?;
        let target = arg(args, 0);
        let from = if arg(args, 1).is_undefined() {
            0
        } else {
            clamp_slice_index(
                vm.to_number(arg(args, 1))?.trunc() as i64,
                ta.length as usize,
            )
        };
        for i in from..ta.length as usize {
            let v = typed_array_read_element(vm, ta, i as u32);
            if same_value_zero(vm, v, target) {
                return Ok(JsValue::TRUE);
            }
        }
        Ok(JsValue::FALSE)
    });
    define_method(vm, tap, "forEach", |vm, t, args| {
        let ta = require_typed_array(vm, t)?;
        let cb = arg(args, 0);
        let this_arg = arg(args, 1);
        for i in 0..ta.length {
            let v = typed_array_read_element(vm, ta, i);
            vm.call_value(cb, this_arg, &[v, JsValue::from_f64(f64::from(i)), t])?;
        }
        Ok(JsValue::UNDEFINED)
    });
    define_method(vm, tap, "map", |vm, t, args| {
        let ta = require_typed_array(vm, t)?;
        let cb = arg(args, 0);
        let this_arg = arg(args, 1);
        let out_h = alloc_typed_array(vm, ta.kind, ta.length);
        let out_ta = match vm.heap.get_object(out_h).map(|o| &o.kind) {
            Some(ObjectKind::TypedArray(x)) => *x,
            _ => unreachable!(),
        };
        for i in 0..ta.length {
            let v = typed_array_read_element(vm, ta, i);
            let mapped = vm.call_value(cb, this_arg, &[v, JsValue::from_f64(f64::from(i)), t])?;
            typed_array_write_element(vm, out_ta, i, mapped)?;
        }
        Ok(JsValue::from_object(out_h))
    });
    define_method(vm, tap, "filter", |vm, t, args| {
        let ta = require_typed_array(vm, t)?;
        let cb = arg(args, 0);
        let this_arg = arg(args, 1);
        let mut kept = Vec::new();
        for i in 0..ta.length {
            let v = typed_array_read_element(vm, ta, i);
            let keep = vm.call_value(cb, this_arg, &[v, JsValue::from_f64(f64::from(i)), t])?;
            if vm.to_boolean(keep) {
                kept.push(v);
            }
        }
        let out_h = alloc_typed_array(vm, ta.kind, kept.len() as u32);
        let out_ta = match vm.heap.get_object(out_h).map(|o| &o.kind) {
            Some(ObjectKind::TypedArray(x)) => *x,
            _ => unreachable!(),
        };
        for (i, v) in kept.into_iter().enumerate() {
            typed_array_write_element(vm, out_ta, i as u32, v)?;
        }
        Ok(JsValue::from_object(out_h))
    });
    define_method(vm, tap, "reduce", |vm, t, args| {
        let ta = require_typed_array(vm, t)?;
        let cb = arg(args, 0);
        let (mut acc, start) = if args.len() >= 2 {
            (args[1], 0u32)
        } else {
            if ta.length == 0 {
                return Err(vm.type_error("Reduce of empty TypedArray with no initial value"));
            }
            (typed_array_read_element(vm, ta, 0), 1u32)
        };
        for i in start..ta.length {
            let v = typed_array_read_element(vm, ta, i);
            acc = vm.call_value(
                cb,
                JsValue::UNDEFINED,
                &[acc, v, JsValue::from_f64(f64::from(i)), t],
            )?;
        }
        Ok(acc)
    });
    define_method(vm, tap, "reverse", |vm, t, _| {
        let ta = require_typed_array(vm, t)?;
        let len = ta.length;
        for i in 0..(len / 2) {
            let j = len - 1 - i;
            let a = typed_array_read_element(vm, ta, i);
            let b = typed_array_read_element(vm, ta, j);
            typed_array_write_element(vm, ta, i, b)?;
            typed_array_write_element(vm, ta, j, a)?;
        }
        Ok(t)
    });
    define_method(vm, tap, "sort", |vm, t, args| {
        let ta = require_typed_array(vm, t)?;
        let cmp = arg(args, 0);
        let mut vals: Vec<JsValue> = (0..ta.length)
            .map(|i| typed_array_read_element(vm, ta, i))
            .collect();
        if cmp.is_undefined() {
            vals.sort_by(|a, b| {
                let fa = a.as_number().unwrap_or(f64::NAN);
                let fb = b.as_number().unwrap_or(f64::NAN);
                fa.total_cmp(&fb)
            });
        } else {
            for i in 1..vals.len() {
                let mut j = i;
                while j > 0 {
                    let order = vm.call_value(cmp, JsValue::UNDEFINED, &[vals[j - 1], vals[j]])?;
                    if vm.to_number(order)? > 0.0 {
                        vals.swap(j - 1, j);
                        j -= 1;
                    } else {
                        break;
                    }
                }
            }
        }
        for (i, v) in vals.into_iter().enumerate() {
            typed_array_write_element(vm, ta, i as u32, v)?;
        }
        Ok(t)
    });
    define_method(vm, tap, "join", |vm, t, args| {
        let ta = require_typed_array(vm, t)?;
        let sep = if arg(args, 0).is_undefined() {
            ",".to_string()
        } else {
            vm.value_to_rust_string(arg(args, 0))
        };
        let parts: Vec<String> = (0..ta.length)
            .map(|i| vm.value_to_rust_string(typed_array_read_element(vm, ta, i)))
            .collect();
        Ok(JsValue::from_string(vm.heap.alloc_string(parts.join(&sep))))
    });
    define_method(vm, tap, "toString", |vm, t, _| {
        let ta = require_typed_array(vm, t)?;
        let parts: Vec<String> = (0..ta.length)
            .map(|i| vm.value_to_rust_string(typed_array_read_element(vm, ta, i)))
            .collect();
        Ok(JsValue::from_string(vm.heap.alloc_string(parts.join(","))))
    });
    define_method(vm, tap, "values", typed_array_values_fn);
    define_symbol_iterator(vm, tap, typed_array_values_fn);
    define_method(vm, tap, "keys", |vm, t, _| {
        let ta = require_typed_array(vm, t)?;
        let items: Vec<JsValue> = (0..ta.length)
            .map(|i| JsValue::from_f64(f64::from(i)))
            .collect();
        Ok(JsValue::from_object(alloc_values_iterator(vm, items)))
    });
    define_method(vm, tap, "entries", |vm, t, _| {
        let ta = require_typed_array(vm, t)?;
        let mut pairs = Vec::with_capacity(ta.length as usize);
        for i in 0..ta.length {
            let p = vm.alloc_array();
            vm.heap
                .object_push_element(p, JsValue::from_f64(f64::from(i)));
            vm.heap
                .object_push_element(p, typed_array_read_element(vm, ta, i));
            pairs.push(JsValue::from_object(p));
        }
        Ok(JsValue::from_object(alloc_values_iterator(vm, pairs)))
    });

    // Install each concrete TypedArray constructor inheriting from `TypedArray.prototype`.
    let k_bpe = vm.heap.intern_key("BYTES_PER_ELEMENT");
    for kind in TypedArrayKind::ALL {
        let proto = vm.typed_array_protos[kind as usize];
        let bpe_val = JsValue::from_i32(kind.bytes_per_element() as i32);
        vm.heap.object_define_data(
            proto,
            k_bpe,
            bpe_val,
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
        let ctor = vm.alloc_host_fn(kind.name(), move |vm, _this, args| {
            construct_typed_array(vm, kind, args)
        });
        vm.heap
            .object_define_data(ctor, k_bpe, bpe_val, PropertyAttrs::empty(), &mut vm.shapes);
        let from_fn = vm.alloc_host_fn("from", move |vm, _this, args| {
            let src = vm.collect_iterable(arg(args, 0))?;
            let map_fn = arg(args, 1);
            let out_h = alloc_typed_array(vm, kind, src.len() as u32);
            let out_ta = match vm.heap.get_object(out_h).map(|o| &o.kind) {
                Some(ObjectKind::TypedArray(x)) => *x,
                _ => unreachable!(),
            };
            for (i, item) in src.into_iter().enumerate() {
                let v = if !map_fn.is_undefined() {
                    vm.call_value(map_fn, arg(args, 2), &[item, JsValue::from_f64(i as f64)])?
                } else {
                    item
                };
                typed_array_write_element(vm, out_ta, i as u32, v)?;
            }
            Ok(JsValue::from_object(out_h))
        });
        let of_fn = vm.alloc_host_fn("of", move |vm, _this, args| {
            let out_h = alloc_typed_array(vm, kind, args.len() as u32);
            let out_ta = match vm.heap.get_object(out_h).map(|o| &o.kind) {
                Some(ObjectKind::TypedArray(x)) => *x,
                _ => unreachable!(),
            };
            for (i, &v) in args.iter().enumerate() {
                typed_array_write_element(vm, out_ta, i as u32, v)?;
            }
            Ok(JsValue::from_object(out_h))
        });
        let k_from = vm.heap.intern_key("from");
        let k_of = vm.heap.intern_key("of");
        vm.heap.object_define_data(
            ctor,
            k_from,
            JsValue::from_object(from_fn),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            ctor,
            k_of,
            JsValue::from_object(of_fn),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
        bind_ctor_proto(vm, ctor, proto);
        vm.heap
            .object_set_prototype(ctor, Some(typed_array_ctor), &mut vm.shapes);
        define_global(vm, kind.name(), JsValue::from_object(ctor));
    }
}

fn typed_array_values_fn(
    vm: &mut JsVm,
    t: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let ta = require_typed_array(vm, t)?;
    let items: Vec<JsValue> = (0..ta.length)
        .map(|i| typed_array_read_element(vm, ta, i))
        .collect();
    Ok(JsValue::from_object(alloc_values_iterator(vm, items)))
}
