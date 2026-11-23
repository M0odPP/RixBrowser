//! ES2024 Collections (`Map`, `Set`, `WeakMap`, `WeakSet`, `WeakRef`) and
//! Binary Data (`ArrayBuffer`, `DataView`, and all 9 `TypedArray` constructors).
//!
//! Reference: Spec 05 §6 & Issue #15.

use crate::bigint::JsBigInt;
use crate::builtins_atomics::{
    ab_byte_len, ab_copy_slice, ab_is_detached, ab_is_immutable, ab_is_shared, ab_max_byte_length,
    ab_read_bytes, ab_shared_backing, ab_write_bytes, dv_effective_byte_len, install_atomics,
    ta_effective_len, to_bigint_i64, to_index,
};
use crate::heap::{
    DataViewObject, JsObject, ObjectKind, SharedBufferBacking, TypedArrayKind, TypedArrayObject,
};
use crate::shape::PropertyAttrs;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm, NativeFnPtr};
use crate::vm_builtins::define_method_len;

/// Installs `Map`, `Set`, `WeakMap`, `WeakSet`, `WeakRef`, `ArrayBuffer`, `SharedArrayBuffer`,
/// `DataView`, all 11 `TypedArray` constructors, `Atomics`, and ES2025 `Iterator` helpers.
pub fn install_collection_builtins(vm: &mut JsVm) {
    install_iterator(vm);
    install_map(vm);
    install_set(vm);
    install_weak_map(vm);
    install_weak_set(vm);
    install_weak_ref(vm);
    install_array_buffer(vm);
    install_shared_array_buffer(vm);
    install_data_view(vm);
    install_typed_arrays(vm);
    install_atomics(vm);
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
    let g_fn = vm.alloc_native_fn(&format!("get {name}"), getter);
    vm.set_fn_length(g_fn, 0);
    vm.heap.object_define_accessor(
        target,
        key,
        Some(g_fn),
        None,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
}

fn define_species_getter(vm: &mut JsVm, ctor: HeapHandle) {
    let sp = vm.alloc_native_fn("get [Symbol.species]", |_vm, t, _| Ok(t));
    vm.set_fn_length(sp, 0);
    let k = vm.key_symbol_species;
    vm.heap.object_define_accessor(
        ctor,
        k,
        Some(sp),
        None,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
}

fn define_to_string_tag(vm: &mut JsVm, proto: HeapHandle, tag: &str) {
    let v = JsValue::from_string(vm.heap.alloc_string(tag));
    let k = vm.key_symbol_to_string_tag;
    vm.heap
        .object_define_data(proto, k, v, PropertyAttrs::CONFIGURABLE, &mut vm.shapes);
}

fn species_ctor_for(
    vm: &mut JsVm,
    obj: HeapHandle,
    default_ctor: HeapHandle,
) -> Result<JsValue, JsException> {
    let c = vm.get_property_by_key(obj, vm.key_constructor)?;
    if c.is_undefined() {
        return Ok(JsValue::from_object(default_ctor));
    }
    let Some(ch) = c.as_object() else {
        return Err(vm.type_error("constructor property is not an object"));
    };
    let s = vm.get_property_by_key(ch, vm.key_symbol_species)?;
    if s.is_undefined() || s.is_null() {
        return Ok(JsValue::from_object(default_ctor));
    }
    if s.as_object().is_some_and(|h| vm.is_constructor_handle(h)) {
        return Ok(s);
    }
    Err(vm.type_error("Species is not a constructor"))
}

fn parse_max_byte_length_opt(
    vm: &mut JsVm,
    options: JsValue,
    byte_len: usize,
) -> Result<Option<usize>, JsException> {
    let Some(opt_h) = options.as_object() else {
        return Ok(None);
    };
    let k = vm.heap.intern_key("maxByteLength");
    let mv = vm.get_property_by_key(opt_h, k)?;
    if mv.is_undefined() {
        return Ok(None);
    }
    let max_len = to_index(vm, mv)?;
    if byte_len > max_len || max_len > 64 * 1024 * 1024 {
        return Err(vm.range_error("Invalid maxByteLength"));
    }
    Ok(Some(max_len))
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
    vm.mark_constructor(ctor);
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
    let iter_obj = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.iterator_proto),
    ));
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
// ArrayBuffer (ECMA-262 §25.1, incl. Resizable ArrayBuffer & transfer)
// ---------------------------------------------------------------------------

fn require_any_buffer(vm: &mut JsVm, val: JsValue) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Expected ArrayBuffer or SharedArrayBuffer"));
    };
    if matches!(
        vm.heap.get_object(h).map(|o| &o.kind),
        Some(ObjectKind::ArrayBuffer(_))
    ) {
        Ok(h)
    } else {
        Err(vm.type_error("Expected ArrayBuffer or SharedArrayBuffer"))
    }
}

fn require_unshared_ab(vm: &mut JsVm, val: JsValue) -> Result<HeapHandle, JsException> {
    let h = require_any_buffer(vm, val)?;
    if ab_is_shared(vm, h) {
        return Err(vm.type_error("Method called on incompatible SharedArrayBuffer"));
    }
    Ok(h)
}

pub fn alloc_array_buffer(vm: &mut JsVm, bytes: Vec<u8>) -> HeapHandle {
    alloc_array_buffer_ex(vm, bytes, None)
}

pub fn alloc_array_buffer_ex(
    vm: &mut JsVm,
    bytes: Vec<u8>,
    max_byte_length: Option<usize>,
) -> HeapHandle {
    let h = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::ArrayBuffer(Box::new(bytes)),
        Some(vm.array_buffer_proto),
    ));
    if let Some(max) = max_byte_length {
        if let Some(o) = vm.heap.get_object_mut(h) {
            o.rare_mut().ab_max_byte_length = Some(max);
        }
    }
    h
}

fn clamp_rel_usize(rel: f64, len: usize) -> usize {
    if rel == f64::NEG_INFINITY {
        0
    } else if rel < 0.0 {
        (len as f64 + rel).max(0.0) as usize
    } else if rel >= len as f64 {
        len
    } else {
        rel as usize
    }
}

fn ab_transfer_impl(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
    preserve_resizable: bool,
    make_immutable: bool,
) -> Result<JsValue, JsException> {
    let h = require_unshared_ab(vm, this_val)?;
    let new_len_arg = arg(args, 0);
    let new_len = if new_len_arg.is_undefined() {
        ab_byte_len(vm, h)
    } else {
        to_index(vm, new_len_arg)?
    };
    if ab_is_detached(vm, h) || ab_is_immutable(vm, h) {
        return Err(vm.type_error("Cannot transfer a detached or immutable ArrayBuffer"));
    }
    let old_max = ab_max_byte_length(vm, h);
    let new_max = if preserve_resizable && !make_immutable {
        old_max
    } else {
        None
    };
    if let Some(max) = new_max {
        if new_len > max {
            return Err(vm.range_error("Transfer byteLength exceeds maxByteLength"));
        }
    }
    if new_len > 64 * 1024 * 1024 {
        return Err(vm.range_error("Invalid ArrayBuffer byteLength"));
    }
    let cur_len = ab_byte_len(vm, h);
    let copy_len = cur_len.min(new_len);
    let mut new_bytes = vec![0u8; new_len];
    if copy_len > 0 {
        let src = ab_copy_slice(vm, h, 0, copy_len);
        new_bytes[..src.len()].copy_from_slice(&src);
    }
    vm.detach_array_buffer(h)?;
    let out_h = alloc_array_buffer_ex(vm, new_bytes, new_max);
    if make_immutable {
        if let Some(o) = vm.heap.get_object_mut(out_h) {
            o.rare_mut().ab_immutable = true;
        }
    }
    Ok(JsValue::from_object(out_h))
}

fn install_array_buffer(vm: &mut JsVm) {
    let abp = vm.array_buffer_proto;
    define_getter(vm, abp, "byteLength", |vm, this_val, _| {
        let h = require_unshared_ab(vm, this_val)?;
        Ok(JsValue::from_f64(ab_byte_len(vm, h) as f64))
    });
    define_getter(vm, abp, "maxByteLength", |vm, this_val, _| {
        let h = require_unshared_ab(vm, this_val)?;
        if ab_is_detached(vm, h) {
            return Ok(JsValue::from_i32(0));
        }
        let max = ab_max_byte_length(vm, h).unwrap_or_else(|| ab_byte_len(vm, h));
        Ok(JsValue::from_f64(max as f64))
    });
    define_getter(vm, abp, "resizable", |vm, this_val, _| {
        let h = require_unshared_ab(vm, this_val)?;
        Ok(JsValue::from_bool(ab_max_byte_length(vm, h).is_some()))
    });
    define_getter(vm, abp, "detached", |vm, this_val, _| {
        let h = require_unshared_ab(vm, this_val)?;
        Ok(JsValue::from_bool(ab_is_detached(vm, h)))
    });
    define_getter(vm, abp, "immutable", |vm, this_val, _| {
        let h = require_unshared_ab(vm, this_val)?;
        Ok(JsValue::from_bool(ab_is_immutable(vm, h)))
    });
    define_method_len(vm, abp, "resize", 1, |vm, this_val, args| {
        let h = require_unshared_ab(vm, this_val)?;
        let Some(max) = ab_max_byte_length(vm, h) else {
            return Err(vm.type_error("ArrayBuffer is not resizable"));
        };
        let new_len = to_index(vm, arg(args, 0))?;
        if ab_is_detached(vm, h) || ab_is_immutable(vm, h) {
            return Err(vm.type_error("Cannot resize a detached or immutable ArrayBuffer"));
        }
        if new_len > max {
            return Err(vm.range_error("New length exceeds maxByteLength"));
        }
        if let Some(ObjectKind::ArrayBuffer(buf)) = vm.heap.get_object_mut(h).map(|o| &mut o.kind) {
            buf.resize(new_len, 0);
        }
        Ok(JsValue::UNDEFINED)
    });
    define_method_len(vm, abp, "transfer", 0, |vm, t, args| {
        ab_transfer_impl(vm, t, args, true, false)
    });
    define_method_len(vm, abp, "transferToFixedLength", 0, |vm, t, args| {
        ab_transfer_impl(vm, t, args, false, false)
    });
    define_method_len(vm, abp, "transferToImmutable", 0, |vm, t, args| {
        ab_transfer_impl(vm, t, args, false, true)
    });
    define_method_len(vm, abp, "sliceToImmutable", 2, |vm, this_val, args| {
        let h = require_unshared_ab(vm, this_val)?;
        if ab_is_detached(vm, h) {
            return Err(vm.type_error("Cannot slice a detached ArrayBuffer"));
        }
        let len = ab_byte_len(vm, h);
        let first = clamp_rel_usize(vm.to_integer_or_infinity(arg(args, 0))?, len);
        let fin = if arg(args, 1).is_undefined() {
            len
        } else {
            clamp_rel_usize(vm.to_integer_or_infinity(arg(args, 1))?, len)
        };
        let new_len = fin.saturating_sub(first);
        if ab_is_detached(vm, h) {
            return Err(vm.type_error("Source ArrayBuffer was detached"));
        }
        let mut bytes = vec![0u8; new_len];
        let avail = ab_byte_len(vm, h).saturating_sub(first).min(new_len);
        if avail > 0 {
            let copied = ab_copy_slice(vm, h, first, first + avail);
            bytes[..copied.len()].copy_from_slice(&copied);
        }
        let out_h = alloc_array_buffer_ex(vm, bytes, None);
        if let Some(o) = vm.heap.get_object_mut(out_h) {
            o.rare_mut().ab_immutable = true;
        }
        Ok(JsValue::from_object(out_h))
    });
    define_method_len(vm, abp, "slice", 2, |vm, this_val, args| {
        let h = require_unshared_ab(vm, this_val)?;
        if ab_is_detached(vm, h) {
            return Err(vm.type_error("Cannot slice a detached ArrayBuffer"));
        }
        let len = ab_byte_len(vm, h);
        let rel_start = vm.to_integer_or_infinity(arg(args, 0))?;
        let first = clamp_rel_usize(rel_start, len);
        let fin = if arg(args, 1).is_undefined() {
            len
        } else {
            clamp_rel_usize(vm.to_integer_or_infinity(arg(args, 1))?, len)
        };
        let new_len = fin.saturating_sub(first);
        let k_ab = vm.heap.intern_key("ArrayBuffer");
        let default_ctor = vm
            .get_property_by_key(vm.global, k_ab)
            .ok()
            .and_then(|v| v.as_object())
            .unwrap_or(vm.array_buffer_proto);
        let ctor = species_ctor_for(vm, h, default_ctor)?;
        let new_val = vm.construct_value(ctor, &[JsValue::from_f64(new_len as f64)], ctor)?;
        let new_h = require_unshared_ab(vm, new_val)?;
        if ab_is_detached(vm, new_h) {
            return Err(vm.type_error("Species constructor returned a detached ArrayBuffer"));
        }
        if new_h == h {
            return Err(vm.type_error("Species constructor returned the same ArrayBuffer"));
        }
        if ab_byte_len(vm, new_h) < new_len {
            return Err(vm.type_error("Species constructor returned too-small ArrayBuffer"));
        }
        if ab_is_detached(vm, h) {
            return Err(vm.type_error("Source ArrayBuffer was detached during slice"));
        }
        let available = ab_byte_len(vm, h).saturating_sub(first);
        let copy_n = new_len.min(available);
        if copy_n > 0 {
            let copied = ab_copy_slice(vm, h, first, first + copy_n);
            if let Some(ObjectKind::ArrayBuffer(dst)) =
                vm.heap.get_object_mut(new_h).map(|o| &mut o.kind)
            {
                dst[..copied.len()].copy_from_slice(&copied);
            }
        }
        Ok(new_val)
    });
    define_to_string_tag(vm, abp, "ArrayBuffer");

    let ctor = vm.alloc_native_fn("ArrayBuffer", |vm, _this, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("Constructor ArrayBuffer requires 'new'"));
        }
        let len = to_index(vm, arg(args, 0))?;
        let max_len = parse_max_byte_length_opt(vm, arg(args, 1), len)?;
        if len > 64 * 1024 * 1024 {
            return Err(vm.range_error("Invalid ArrayBuffer byteLength"));
        }
        Ok(JsValue::from_object(alloc_array_buffer_ex(
            vm,
            vec![0u8; len],
            max_len,
        )))
    });
    vm.set_fn_length(ctor, 1);
    define_method_len(vm, ctor, "isView", 1, |vm, _this, args| {
        let is_view = arg(args, 0).as_object().is_some_and(|h| {
            matches!(
                vm.heap.get_object(h).map(|o| &o.kind),
                Some(ObjectKind::DataView(_) | ObjectKind::TypedArray(_))
            )
        });
        Ok(JsValue::from_bool(is_view))
    });
    define_species_getter(vm, ctor);
    bind_ctor_proto(vm, ctor, abp);
    define_global(vm, "ArrayBuffer", JsValue::from_object(ctor));
}

// ---------------------------------------------------------------------------
// DataView (ECMA-262 §25.3, all 11 element types + auto-length tracking)
// ---------------------------------------------------------------------------

fn require_data_view(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<(HeapHandle, DataViewObject), JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible DataView receiver"));
    };
    match vm.heap.get_object(h).map(|o| &o.kind) {
        Some(ObjectKind::DataView(dv)) => Ok((h, *dv)),
        _ => Err(vm.type_error("Incompatible DataView receiver")),
    }
}

fn dv_get_bytes<const N: usize>(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<([u8; N], bool), JsException> {
    let (dv_h, dv) = require_data_view(vm, this_val)?;
    let off = to_index(vm, arg(args, 0))?;
    let le = vm.to_boolean(arg(args, 1));
    let Some(view_len) = dv_effective_byte_len(vm, dv_h, dv) else {
        return Err(vm.type_error("DataView buffer is detached or out of bounds"));
    };
    if off.checked_add(N).is_none_or(|end| end > view_len as usize) {
        return Err(vm.range_error("Offset is outside the bounds of the DataView"));
    }
    let abs = dv.byte_offset as usize + off;
    let bytes = ab_read_bytes::<N>(vm, dv.buffer, abs)
        .ok_or_else(|| vm.range_error("Offset is outside the bounds of the DataView"))?;
    Ok((bytes, le))
}

fn dv_set_num<const N: usize>(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
    encode: impl FnOnce(f64, bool) -> [u8; N],
) -> Result<JsValue, JsException> {
    let (dv_h, dv) = require_data_view(vm, this_val)?;
    let off = to_index(vm, arg(args, 0))?;
    let num = vm.to_number(arg(args, 1))?;
    let le = vm.to_boolean(arg(args, 2));
    let Some(view_len) = dv_effective_byte_len(vm, dv_h, dv) else {
        return Err(vm.type_error("DataView buffer is detached or out of bounds"));
    };
    if off.checked_add(N).is_none_or(|end| end > view_len as usize) {
        return Err(vm.range_error("Offset is outside the bounds of the DataView"));
    }
    let bytes = encode(num, le);
    if !ab_write_bytes(vm, dv.buffer, dv.byte_offset as usize + off, bytes) {
        return Err(vm.type_error("DataView buffer is detached or out of bounds"));
    }
    Ok(JsValue::UNDEFINED)
}

fn dv_set_bigint(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let (dv_h, dv) = require_data_view(vm, this_val)?;
    let off = to_index(vm, arg(args, 0))?;
    let bits = to_bigint_i64(vm, arg(args, 1))?;
    let le = vm.to_boolean(arg(args, 2));
    let Some(view_len) = dv_effective_byte_len(vm, dv_h, dv) else {
        return Err(vm.type_error("DataView buffer is detached or out of bounds"));
    };
    if off.checked_add(8).is_none_or(|end| end > view_len as usize) {
        return Err(vm.range_error("Offset is outside the bounds of the DataView"));
    }
    let bytes = if le {
        bits.to_le_bytes()
    } else {
        bits.to_be_bytes()
    };
    if !ab_write_bytes(vm, dv.buffer, dv.byte_offset as usize + off, bytes) {
        return Err(vm.type_error("DataView buffer is detached or out of bounds"));
    }
    Ok(JsValue::UNDEFINED)
}

fn install_data_view(vm: &mut JsVm) {
    let dvp = vm.data_view_proto;
    define_getter(vm, dvp, "buffer", |vm, this_val, _| {
        let (_, dv) = require_data_view(vm, this_val)?;
        Ok(JsValue::from_object(dv.buffer))
    });
    define_getter(vm, dvp, "byteOffset", |vm, this_val, _| {
        let (dv_h, dv) = require_data_view(vm, this_val)?;
        if dv_effective_byte_len(vm, dv_h, dv).is_none() {
            return Err(vm.type_error("DataView buffer is detached or out of bounds"));
        }
        Ok(JsValue::from_f64(f64::from(dv.byte_offset)))
    });
    define_getter(vm, dvp, "byteLength", |vm, this_val, _| {
        let (dv_h, dv) = require_data_view(vm, this_val)?;
        let Some(len) = dv_effective_byte_len(vm, dv_h, dv) else {
            return Err(vm.type_error("DataView buffer is detached or out of bounds"));
        };
        Ok(JsValue::from_f64(f64::from(len)))
    });
    define_method_len(vm, dvp, "getInt8", 1, |vm, t, args| {
        let ([b], _) = dv_get_bytes::<1>(vm, t, args)?;
        Ok(JsValue::from_i32(i32::from(b as i8)))
    });
    define_method_len(vm, dvp, "setInt8", 2, |vm, t, args| {
        dv_set_num::<1>(vm, t, args, |n, _| [to_int32_bits(n) as i8 as u8])
    });
    define_method_len(vm, dvp, "getUint8", 1, |vm, t, args| {
        let ([b], _) = dv_get_bytes::<1>(vm, t, args)?;
        Ok(JsValue::from_i32(i32::from(b)))
    });
    define_method_len(vm, dvp, "setUint8", 2, |vm, t, args| {
        dv_set_num::<1>(vm, t, args, |n, _| [to_int32_bits(n) as u8])
    });
    define_method_len(vm, dvp, "getInt16", 1, |vm, t, args| {
        let (b, le) = dv_get_bytes::<2>(vm, t, args)?;
        Ok(JsValue::from_i32(i32::from(if le {
            i16::from_le_bytes(b)
        } else {
            i16::from_be_bytes(b)
        })))
    });
    define_method_len(vm, dvp, "setInt16", 2, |vm, t, args| {
        dv_set_num::<2>(vm, t, args, |n, le| {
            let v = to_int32_bits(n) as i16;
            if le {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        })
    });
    define_method_len(vm, dvp, "getUint16", 1, |vm, t, args| {
        let (b, le) = dv_get_bytes::<2>(vm, t, args)?;
        Ok(JsValue::from_i32(i32::from(if le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })))
    });
    define_method_len(vm, dvp, "setUint16", 2, |vm, t, args| {
        dv_set_num::<2>(vm, t, args, |n, le| {
            let v = to_int32_bits(n) as u16;
            if le {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        })
    });
    define_method_len(vm, dvp, "getInt32", 1, |vm, t, args| {
        let (b, le) = dv_get_bytes::<4>(vm, t, args)?;
        Ok(JsValue::from_i32(if le {
            i32::from_le_bytes(b)
        } else {
            i32::from_be_bytes(b)
        }))
    });
    define_method_len(vm, dvp, "setInt32", 2, |vm, t, args| {
        dv_set_num::<4>(vm, t, args, |n, le| {
            let v = to_int32_bits(n);
            if le {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        })
    });
    define_method_len(vm, dvp, "getUint32", 1, |vm, t, args| {
        let (b, le) = dv_get_bytes::<4>(vm, t, args)?;
        Ok(JsValue::from_f64(f64::from(if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })))
    });
    define_method_len(vm, dvp, "setUint32", 2, |vm, t, args| {
        dv_set_num::<4>(vm, t, args, |n, le| {
            let v = to_int32_bits(n) as u32;
            if le {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        })
    });
    define_method_len(vm, dvp, "getFloat16", 1, |vm, t, args| {
        let (b, le) = dv_get_bytes::<2>(vm, t, args)?;
        let bits = if le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        };
        Ok(JsValue::from_f64(f16_to_f64(bits)))
    });
    define_method_len(vm, dvp, "setFloat16", 2, |vm, t, args| {
        dv_set_num::<2>(vm, t, args, |n, le| {
            let bits = f64_to_f16(n);
            if le {
                bits.to_le_bytes()
            } else {
                bits.to_be_bytes()
            }
        })
    });
    define_method_len(vm, dvp, "getFloat32", 1, |vm, t, args| {
        let (b, le) = dv_get_bytes::<4>(vm, t, args)?;
        Ok(JsValue::from_f64(f64::from(if le {
            f32::from_le_bytes(b)
        } else {
            f32::from_be_bytes(b)
        })))
    });
    define_method_len(vm, dvp, "setFloat32", 2, |vm, t, args| {
        dv_set_num::<4>(vm, t, args, |n, le| {
            let v = n as f32;
            if le {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        })
    });
    define_method_len(vm, dvp, "getFloat64", 1, |vm, t, args| {
        let (b, le) = dv_get_bytes::<8>(vm, t, args)?;
        Ok(JsValue::from_f64(if le {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        }))
    });
    define_method_len(vm, dvp, "setFloat64", 2, |vm, t, args| {
        dv_set_num::<8>(vm, t, args, |n, le| {
            if le {
                n.to_le_bytes()
            } else {
                n.to_be_bytes()
            }
        })
    });
    define_method_len(vm, dvp, "getBigInt64", 1, |vm, t, args| {
        let (b, le) = dv_get_bytes::<8>(vm, t, args)?;
        let v = if le {
            i64::from_le_bytes(b)
        } else {
            i64::from_be_bytes(b)
        };
        Ok(JsValue::from_bigint(
            vm.heap.alloc_bigint(JsBigInt::from_i64(v)),
        ))
    });
    define_method_len(vm, dvp, "setBigInt64", 2, dv_set_bigint);
    define_method_len(vm, dvp, "getBigUint64", 1, |vm, t, args| {
        let (b, le) = dv_get_bytes::<8>(vm, t, args)?;
        let v = if le {
            u64::from_le_bytes(b)
        } else {
            u64::from_be_bytes(b)
        };
        Ok(JsValue::from_bigint(
            vm.heap.alloc_bigint(JsBigInt::from_i128(i128::from(v))),
        ))
    });
    define_method_len(vm, dvp, "setBigUint64", 2, dv_set_bigint);
    define_to_string_tag(vm, dvp, "DataView");

    let ctor = vm.alloc_native_fn("DataView", |vm, _this, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("Constructor DataView requires 'new'"));
        }
        let buf_h = require_any_buffer(vm, arg(args, 0))?;
        let byte_offset = to_index(vm, arg(args, 1))?;
        if ab_is_detached(vm, buf_h) {
            return Err(vm.type_error("Cannot construct DataView on detached ArrayBuffer"));
        }
        let buf_len = ab_byte_len(vm, buf_h);
        if byte_offset > buf_len {
            return Err(vm.range_error("Start offset is outside the bounds of the buffer"));
        }
        let (byte_length, auto_len) = if arg(args, 2).is_undefined() {
            (
                buf_len - byte_offset,
                ab_max_byte_length(vm, buf_h).is_some(),
            )
        } else {
            let l = to_index(vm, arg(args, 2))?;
            if byte_offset.checked_add(l).is_none_or(|end| end > buf_len) {
                return Err(vm.range_error("Invalid DataView byteLength"));
            }
            (l, false)
        };
        if ab_is_detached(vm, buf_h) {
            return Err(vm.type_error("Cannot construct DataView on detached ArrayBuffer"));
        }
        let cur_len = ab_byte_len(vm, buf_h);
        if byte_offset > cur_len || (!auto_len && byte_offset + byte_length > cur_len) {
            return Err(vm.range_error("Start offset is outside the bounds of the buffer"));
        }
        let dv = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::DataView(DataViewObject {
                buffer: buf_h,
                byte_offset: byte_offset as u32,
                byte_length: byte_length as u32,
            }),
            Some(vm.data_view_proto),
        ));
        if auto_len {
            if let Some(o) = vm.heap.get_object_mut(dv) {
                o.rare_mut().view_auto_length = true;
            }
        }
        Ok(JsValue::from_object(dv))
    });
    vm.set_fn_length(ctor, 1);
    bind_ctor_proto(vm, ctor, dvp);
    define_global(vm, "DataView", JsValue::from_object(ctor));
}

// ---------------------------------------------------------------------------
// TypedArray (all 9 numeric views)
// ---------------------------------------------------------------------------

fn f16_to_f64(bits: u16) -> f64 {
    let (s, e, m) = (
        u32::from(bits >> 15) << 31,
        u32::from((bits >> 10) & 0x1F),
        u32::from(bits & 0x3FF),
    );
    let f32_bits = match e {
        0 if m == 0 => s,
        0 => {
            let shift = m.leading_zeros() - 21;
            s | ((113 - shift) << 23) | (((m << shift) & 0x3FF) << 13)
        }
        0x1F => s | (0xFF << 23) | (m << 13),
        _ => s | ((e + 112) << 23) | (m << 13),
    };
    f64::from(f32::from_bits(f32_bits))
}

/// Exact IEEE-754 binary64 -> binary16 (round-to-nearest-even without f32 double-rounding).
fn f64_to_f16(val: f64) -> u16 {
    let bits = val.to_bits();
    let sign = ((bits >> 48) & 0x8000) as u16;
    let exp = ((bits >> 52) & 0x7FF) as i32;
    let frac = bits & 0x000F_FFFF_FFFF_FFFF;
    if exp == 0x7FF {
        return sign | if frac != 0 { 0x7E00 } else { 0x7C00 };
    }
    let unbiased = exp - 1023;
    if unbiased > 15 {
        return sign | 0x7C00;
    }
    if unbiased < -25 {
        return sign;
    }
    if unbiased < -14 {
        let mant = 0x0010_0000_0000_0000u64 | frac;
        let shift = (-14 - unbiased) as u32 + 42;
        let mut r = mant >> shift;
        let half = 1u64 << (shift - 1);
        let rem = mant & ((1u64 << shift) - 1);
        if rem > half || (rem == half && (r & 1) == 1) {
            r += 1;
        }
        return sign | (r as u16);
    }
    let h_exp = (unbiased + 15) as u16;
    let mut h_frac = (frac >> 42) as u16;
    let rem = frac & ((1u64 << 42) - 1);
    let half = 1u64 << 41;
    if rem > half || (rem == half && (h_frac & 1) == 1) {
        h_frac += 1;
        if h_frac == 0x400 {
            return if h_exp + 1 >= 0x1F {
                sign | 0x7C00
            } else {
                sign | ((h_exp + 1) << 10)
            };
        }
    }
    sign | (h_exp << 10) | h_frac
}

fn typed_array_bytes(vm: &JsVm, ta: TypedArrayObject, index: u32) -> Option<[u8; 8]> {
    let bpe = ta.kind.bytes_per_element();
    let off = (ta.byte_offset as usize).checked_add((index as usize).checked_mul(bpe)?)?;
    let mut tmp = [0u8; 8];
    match bpe {
        1 => tmp[..1].copy_from_slice(&ab_read_bytes::<1>(vm, ta.buffer, off)?),
        2 => tmp[..2].copy_from_slice(&ab_read_bytes::<2>(vm, ta.buffer, off)?),
        4 => tmp[..4].copy_from_slice(&ab_read_bytes::<4>(vm, ta.buffer, off)?),
        8 => tmp.copy_from_slice(&ab_read_bytes::<8>(vm, ta.buffer, off)?),
        _ => return None,
    }
    Some(tmp)
}

/// Formats element `index` of a `TypedArray` as a Rust string without mutating the heap.
#[must_use]
pub fn typed_array_read_element_string(vm: &JsVm, ta: TypedArrayObject, index: u32) -> String {
    let Some(b) = typed_array_bytes(vm, ta, index) else {
        return String::new();
    };
    match ta.kind {
        TypedArrayKind::BigInt64 => i64::from_ne_bytes(b).to_string(),
        TypedArrayKind::BigUint64 => u64::from_ne_bytes(b).to_string(),
        TypedArrayKind::Float16 => {
            vm.value_to_rust_string(JsValue::from_f64(f16_to_f64(u16::from_ne_bytes([
                b[0], b[1],
            ]))))
        }
        TypedArrayKind::Int8 => (b[0] as i8).to_string(),
        TypedArrayKind::Uint8 | TypedArrayKind::Uint8Clamped => b[0].to_string(),
        TypedArrayKind::Int16 => i16::from_ne_bytes([b[0], b[1]]).to_string(),
        TypedArrayKind::Uint16 => u16::from_ne_bytes([b[0], b[1]]).to_string(),
        TypedArrayKind::Int32 => i32::from_ne_bytes([b[0], b[1], b[2], b[3]]).to_string(),
        TypedArrayKind::Uint32 => u32::from_ne_bytes([b[0], b[1], b[2], b[3]]).to_string(),
        TypedArrayKind::Float32 => {
            vm.value_to_rust_string(JsValue::from_f64(f64::from(f32::from_ne_bytes([
                b[0], b[1], b[2], b[3],
            ]))))
        }
        TypedArrayKind::Float64 => {
            vm.value_to_rust_string(JsValue::from_f64(f64::from_ne_bytes(b)))
        }
    }
}

/// Reads element `index` from a `TypedArray` backing `ArrayBuffer` / `SharedArrayBuffer`.
#[must_use]
pub fn typed_array_read_element(vm: &mut JsVm, ta: TypedArrayObject, index: u32) -> JsValue {
    let Some(b) = typed_array_bytes(vm, ta, index) else {
        return JsValue::UNDEFINED;
    };
    match ta.kind {
        TypedArrayKind::Int8 => JsValue::from_i32(i32::from(b[0] as i8)),
        TypedArrayKind::Uint8 | TypedArrayKind::Uint8Clamped => JsValue::from_i32(i32::from(b[0])),
        TypedArrayKind::Int16 => JsValue::from_i32(i32::from(i16::from_ne_bytes([b[0], b[1]]))),
        TypedArrayKind::Uint16 => JsValue::from_i32(i32::from(u16::from_ne_bytes([b[0], b[1]]))),
        TypedArrayKind::Float16 => JsValue::from_f64(f16_to_f64(u16::from_ne_bytes([b[0], b[1]]))),
        TypedArrayKind::Int32 => JsValue::from_i32(i32::from_ne_bytes([b[0], b[1], b[2], b[3]])),
        TypedArrayKind::Uint32 => {
            JsValue::from_f64(f64::from(u32::from_ne_bytes([b[0], b[1], b[2], b[3]])))
        }
        TypedArrayKind::Float32 => {
            JsValue::from_f64(f64::from(f32::from_ne_bytes([b[0], b[1], b[2], b[3]])))
        }
        TypedArrayKind::Float64 => JsValue::from_f64(f64::from_ne_bytes(b)),
        TypedArrayKind::BigInt64 => JsValue::from_bigint(
            vm.heap
                .alloc_bigint(JsBigInt::from_i64(i64::from_ne_bytes(b))),
        ),
        TypedArrayKind::BigUint64 => JsValue::from_bigint(
            vm.heap
                .alloc_bigint(JsBigInt::from_i128(i128::from(u64::from_ne_bytes(b)))),
        ),
    }
}

fn write_ta_raw_num(vm: &mut JsVm, ta: TypedArrayObject, off: usize, num: f64) {
    match ta.kind {
        TypedArrayKind::Int8 => {
            ab_write_bytes(vm, ta.buffer, off, [to_int32_bits(num) as i8 as u8]);
        }
        TypedArrayKind::Uint8 => {
            ab_write_bytes(vm, ta.buffer, off, [to_int32_bits(num) as u8]);
        }
        TypedArrayKind::Uint8Clamped => {
            ab_write_bytes(vm, ta.buffer, off, [clamp_u8_round_even(num)]);
        }
        TypedArrayKind::Int16 => {
            ab_write_bytes(
                vm,
                ta.buffer,
                off,
                (to_int32_bits(num) as i16).to_ne_bytes(),
            );
        }
        TypedArrayKind::Uint16 => {
            ab_write_bytes(
                vm,
                ta.buffer,
                off,
                (to_int32_bits(num) as u16).to_ne_bytes(),
            );
        }
        TypedArrayKind::Float16 => {
            ab_write_bytes(vm, ta.buffer, off, f64_to_f16(num).to_ne_bytes());
        }
        TypedArrayKind::Int32 => {
            ab_write_bytes(vm, ta.buffer, off, to_int32_bits(num).to_ne_bytes());
        }
        TypedArrayKind::Uint32 => {
            ab_write_bytes(
                vm,
                ta.buffer,
                off,
                (to_int32_bits(num) as u32).to_ne_bytes(),
            );
        }
        TypedArrayKind::Float32 => {
            ab_write_bytes(vm, ta.buffer, off, (num as f32).to_ne_bytes());
        }
        TypedArrayKind::Float64 => {
            ab_write_bytes(vm, ta.buffer, off, num.to_ne_bytes());
        }
        TypedArrayKind::BigInt64 | TypedArrayKind::BigUint64 => unreachable!(),
    }
}

/// ECMA-262 §10.4.5.9 `IntegerIndexedElementSet` (coerces `val` BEFORE detached/OOB check).
pub fn typed_array_write_element_checked(
    vm: &mut JsVm,
    ta_h: HeapHandle,
    ta: TypedArrayObject,
    index: u32,
    val: JsValue,
) -> Result<(), JsException> {
    let bpe = ta.kind.bytes_per_element();
    let off = ta.byte_offset as usize + (index as usize) * bpe;
    if ta.kind.is_bigint() {
        let bits = to_bigint_i64(vm, val)?;
        if ta_effective_len(vm, ta_h, ta).is_some_and(|len| index < len) {
            ab_write_bytes(vm, ta.buffer, off, bits.to_ne_bytes());
        }
        return Ok(());
    }
    let num = vm.to_number(val)?;
    if ta_effective_len(vm, ta_h, ta).is_some_and(|len| index < len) {
        write_ta_raw_num(vm, ta, off, num);
    }
    Ok(())
}

/// Writes `val` coerced to `ta.kind` at element `index` in the backing `ArrayBuffer`.
pub fn typed_array_write_element(
    vm: &mut JsVm,
    ta: TypedArrayObject,
    index: u32,
    val: JsValue,
) -> Result<(), JsException> {
    let bpe = ta.kind.bytes_per_element();
    let off = ta.byte_offset as usize + (index as usize) * bpe;
    if ta.kind.is_bigint() {
        let bits = to_bigint_i64(vm, val)?;
        ab_write_bytes(vm, ta.buffer, off, bits.to_ne_bytes());
        return Ok(());
    }
    let num = vm.to_number(val)?;
    write_ta_raw_num(vm, ta, off, num);
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

fn require_typed_array_raw(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<(HeapHandle, TypedArrayObject), JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible TypedArray receiver"));
    };
    match vm.heap.get_object(h).map(|o| &o.kind) {
        Some(ObjectKind::TypedArray(ta)) => Ok((h, *ta)),
        _ => Err(vm.type_error("Incompatible TypedArray receiver")),
    }
}

fn require_valid_ta(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<(HeapHandle, TypedArrayObject, u32), JsException> {
    let (h, ta) = require_typed_array_raw(vm, val)?;
    let Some(len) = ta_effective_len(vm, h, ta) else {
        return Err(vm.type_error("TypedArray buffer is detached or out of bounds"));
    };
    Ok((h, ta, len))
}

fn require_callable(vm: &mut JsVm, cb: JsValue) -> Result<(), JsException> {
    if cb.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
        Ok(())
    } else {
        Err(vm.type_error("Callback is not a function"))
    }
}

fn ta_default_ctor(vm: &mut JsVm, kind: TypedArrayKind) -> HeapHandle {
    let k = vm.heap.intern_key(kind.name());
    vm.get_property_by_key(vm.global, k)
        .ok()
        .and_then(|v| v.as_object())
        .unwrap_or(vm.typed_array_proto)
}

fn ta_species_create(
    vm: &mut JsVm,
    ta_h: HeapHandle,
    ta: TypedArrayObject,
    ctor_args: &[JsValue],
    min_len: Option<u32>,
) -> Result<(HeapHandle, TypedArrayObject), JsException> {
    let def_ctor = ta_default_ctor(vm, ta.kind);
    let c = species_ctor_for(vm, ta_h, def_ctor)?;
    let created = vm.construct_value(c, ctor_args, c)?;
    let (out_h, out_ta, out_len) = require_valid_ta(vm, created)?;
    if let Some(need) = min_len {
        if out_len < need {
            return Err(vm.type_error("Derived TypedArray constructor created too small array"));
        }
    }
    if out_ta.kind.is_bigint() != ta.kind.is_bigint() {
        return Err(vm.type_error("Cannot mix BigInt and other TypedArray types"));
    }
    Ok((out_h, out_ta))
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

fn alloc_typed_array_pair(
    vm: &mut JsVm,
    kind: TypedArrayKind,
    length: u32,
) -> (HeapHandle, TypedArrayObject) {
    let h = alloc_typed_array(vm, kind, length);
    let ta = match vm.heap.get_object(h).map(|o| &o.kind) {
        Some(ObjectKind::TypedArray(x)) => *x,
        _ => unreachable!(),
    };
    (h, ta)
}

fn ta_find_impl(
    vm: &mut JsVm,
    t: JsValue,
    args: &[JsValue],
    return_index: bool,
    reverse: bool,
) -> Result<JsValue, JsException> {
    let (ta_h, ta, len) = require_valid_ta(vm, t)?;
    let (cb, this_arg) = (arg(args, 0), arg(args, 1));
    require_callable(vm, cb)?;
    for step in 0..len {
        let i = if reverse { len - 1 - step } else { step };
        let v = if ta_effective_len(vm, ta_h, ta).is_some_and(|l| i < l) {
            typed_array_read_element(vm, ta, i)
        } else {
            JsValue::UNDEFINED
        };
        let r = vm.call_value(cb, this_arg, &[v, JsValue::from_f64(f64::from(i)), t])?;
        if vm.to_boolean(r) {
            return Ok(if return_index {
                JsValue::from_f64(f64::from(i))
            } else {
                v
            });
        }
    }
    Ok(if return_index {
        JsValue::from_i32(-1)
    } else {
        JsValue::UNDEFINED
    })
}

fn ta_quantifier(
    vm: &mut JsVm,
    t: JsValue,
    args: &[JsValue],
    stop_on: bool,
) -> Result<JsValue, JsException> {
    let (ta_h, ta, len) = require_valid_ta(vm, t)?;
    let (cb, this_arg) = (arg(args, 0), arg(args, 1));
    require_callable(vm, cb)?;
    for i in 0..len {
        let v = if ta_effective_len(vm, ta_h, ta).is_some_and(|l| i < l) {
            typed_array_read_element(vm, ta, i)
        } else {
            JsValue::UNDEFINED
        };
        let r = vm.call_value(cb, this_arg, &[v, JsValue::from_f64(f64::from(i)), t])?;
        if vm.to_boolean(r) == stop_on {
            return Ok(JsValue::from_bool(stop_on));
        }
    }
    Ok(JsValue::from_bool(!stop_on))
}

fn construct_typed_array(
    vm: &mut JsVm,
    kind: TypedArrayKind,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    if vm.native_new_target.is_undefined() {
        return Err(vm.type_error(format!("Constructor {} requires 'new'", kind.name())));
    }
    let bpe = kind.bytes_per_element();
    let first = arg(args, 0);
    if first.is_undefined() {
        return Ok(JsValue::from_object(alloc_typed_array(vm, kind, 0)));
    }
    // 1. Over an existing ArrayBuffer or SharedArrayBuffer (shared backing memory!).
    if let Some(buf_h) = first.as_object() {
        if matches!(
            vm.heap.get_object(buf_h).map(|o| &o.kind),
            Some(ObjectKind::ArrayBuffer(_))
        ) {
            let byte_offset = to_index(vm, arg(args, 1))?;
            if !byte_offset.is_multiple_of(bpe) {
                return Err(
                    vm.range_error("TypedArray byteOffset must be a multiple of element size")
                );
            }
            let len_arg = arg(args, 2);
            let req_len = if len_arg.is_undefined() {
                None
            } else {
                Some(to_index(vm, len_arg)?)
            };
            if ab_is_detached(vm, buf_h) {
                return Err(vm.type_error("Cannot construct TypedArray on detached ArrayBuffer"));
            }
            let buf_len = ab_byte_len(vm, buf_h);
            let (length, auto_len) = match req_len {
                None => {
                    let resizable = ab_max_byte_length(vm, buf_h).is_some();
                    if !resizable && !buf_len.is_multiple_of(bpe) {
                        return Err(
                            vm.range_error("ArrayBuffer byteLength not aligned to element size")
                        );
                    }
                    if byte_offset > buf_len {
                        return Err(vm.range_error("TypedArray byteOffset out of range"));
                    }
                    ((buf_len - byte_offset) / bpe, resizable)
                }
                Some(count) => {
                    let end = count
                        .checked_mul(bpe)
                        .and_then(|n| byte_offset.checked_add(n));
                    if end.is_none_or(|e| e > buf_len) {
                        return Err(vm.range_error("TypedArray length exceeds ArrayBuffer bounds"));
                    }
                    (count, false)
                }
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
            if auto_len {
                if let Some(o) = vm.heap.get_object_mut(ta) {
                    o.rare_mut().view_auto_length = true;
                }
            }
            return Ok(JsValue::from_object(ta));
        }

        // 2. Copy from another TypedArray or iterable/array-like.
        let src_ta = match vm.heap.get_object(buf_h).map(|o| &o.kind) {
            Some(ObjectKind::TypedArray(src)) => Some(*src),
            _ => None,
        };
        let src_vals = if let Some(src) = src_ta {
            let Some(eff_len) = ta_effective_len(vm, buf_h, src) else {
                return Err(vm.type_error("Source TypedArray buffer is detached or out of bounds"));
            };
            if src.kind.is_bigint() != kind.is_bigint() {
                return Err(vm.type_error("Cannot mix BigInt and other TypedArray types"));
            }
            let mut vals = Vec::with_capacity(eff_len as usize);
            for i in 0..eff_len {
                vals.push(typed_array_read_element(vm, src, i));
            }
            vals
        } else {
            collect_iterable_or_array_like(vm, first)?
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

    // 3. Length constructor (`ToIndex`).
    let len = to_index(vm, first)?;
    let max_elems = (64 * 1024 * 1024) / bpe;
    if len > max_elems {
        return Err(vm.range_error("Invalid TypedArray length"));
    }
    Ok(JsValue::from_object(alloc_typed_array(
        vm, kind, len as u32,
    )))
}

fn install_typed_arrays(vm: &mut JsVm) {
    let tap = vm.typed_array_proto;
    // Shared `%TypedArray%` intrinsic constructor (ECMA-262 §23.2.1):
    let typed_array_ctor = vm.alloc_native_fn("TypedArray", |vm, _, _| {
        Err(vm.type_error("Abstract class TypedArray not directly constructable"))
    });
    vm.set_fn_length(typed_array_ctor, 0);
    define_species_getter(vm, typed_array_ctor);
    define_method_len(vm, typed_array_ctor, "from", 1, |vm, this_val, args| {
        if !this_val
            .as_object()
            .is_some_and(|h| vm.is_constructor_handle(h))
        {
            return Err(vm.type_error("%TypedArray%.from requires a constructor 'this'"));
        }
        let map_arg = arg(args, 1);
        let mapping = !map_arg.is_undefined();
        if mapping {
            require_callable(vm, map_arg)?;
        }
        let this_arg = arg(args, 2);
        let src = collect_iterable_or_array_like(vm, arg(args, 0))?;
        let created =
            vm.construct_value(this_val, &[JsValue::from_f64(src.len() as f64)], this_val)?;
        let (out_h, out_ta, out_len) = require_valid_ta(vm, created)?;
        if (out_len as usize) < src.len() {
            return Err(vm.type_error("Derived TypedArray constructor created too small array"));
        }
        for (i, item) in src.into_iter().enumerate() {
            let v = if mapping {
                vm.call_value(map_arg, this_arg, &[item, JsValue::from_f64(i as f64)])?
            } else {
                item
            };
            typed_array_write_element_checked(vm, out_h, out_ta, i as u32, v)?;
        }
        Ok(created)
    });
    define_method_len(vm, typed_array_ctor, "of", 0, |vm, this_val, args| {
        if !this_val
            .as_object()
            .is_some_and(|h| vm.is_constructor_handle(h))
        {
            return Err(vm.type_error("%TypedArray%.of requires a constructor 'this'"));
        }
        let created =
            vm.construct_value(this_val, &[JsValue::from_f64(args.len() as f64)], this_val)?;
        let (out_h, out_ta, out_len) = require_valid_ta(vm, created)?;
        if (out_len as usize) < args.len() {
            return Err(vm.type_error("Derived TypedArray constructor created too small array"));
        }
        for (i, &v) in args.iter().enumerate() {
            typed_array_write_element_checked(vm, out_h, out_ta, i as u32, v)?;
        }
        Ok(created)
    });
    bind_ctor_proto(vm, typed_array_ctor, tap);

    define_getter(vm, tap, "buffer", |vm, t, _| {
        let (_, ta) = require_typed_array_raw(vm, t)?;
        Ok(JsValue::from_object(ta.buffer))
    });
    define_getter(vm, tap, "byteOffset", |vm, t, _| {
        let (h, ta) = require_typed_array_raw(vm, t)?;
        if ta_effective_len(vm, h, ta).is_none() {
            return Ok(JsValue::from_i32(0));
        }
        Ok(JsValue::from_f64(f64::from(ta.byte_offset)))
    });
    define_getter(vm, tap, "byteLength", |vm, t, _| {
        let (h, ta) = require_typed_array_raw(vm, t)?;
        let len = ta_effective_len(vm, h, ta).unwrap_or(0);
        Ok(JsValue::from_f64(
            (len as usize * ta.kind.bytes_per_element()) as f64,
        ))
    });
    define_getter(vm, tap, "length", |vm, t, _| {
        let (h, ta) = require_typed_array_raw(vm, t)?;
        Ok(JsValue::from_f64(f64::from(
            ta_effective_len(vm, h, ta).unwrap_or(0),
        )))
    });
    let tag_getter = vm.alloc_native_fn("get [Symbol.toStringTag]", |vm, t, _| {
        let Some(h) = t.as_object() else {
            return Ok(JsValue::UNDEFINED);
        };
        let Some(ObjectKind::TypedArray(ta)) = vm.heap.get_object(h).map(|o| &o.kind) else {
            return Ok(JsValue::UNDEFINED);
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(ta.kind.name())))
    });
    vm.set_fn_length(tag_getter, 0);
    let k_tag = vm.key_symbol_to_string_tag;
    vm.heap.object_define_accessor(
        tap,
        k_tag,
        Some(tag_getter),
        None,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    define_method_len(vm, tap, "subarray", 2, |vm, t, args| {
        let (ta_h, ta) = require_typed_array_raw(vm, t)?;
        let src_len = ta_effective_len(vm, ta_h, ta).unwrap_or(0) as usize;
        let begin_f = vm.to_integer_or_infinity(arg(args, 0))?;
        let begin = clamp_rel_usize(begin_f, src_len);
        let new_off = ta.byte_offset as usize + begin * ta.kind.bytes_per_element();
        let auto_src = vm
            .heap
            .get_object(ta_h)
            .and_then(|o| o.rare.as_ref())
            .is_some_and(|r| r.view_auto_length);
        let ctor_args: Vec<JsValue> = if auto_src && arg(args, 1).is_undefined() {
            vec![
                JsValue::from_object(ta.buffer),
                JsValue::from_f64(new_off as f64),
            ]
        } else {
            let end = if arg(args, 1).is_undefined() {
                src_len
            } else {
                clamp_rel_usize(vm.to_integer_or_infinity(arg(args, 1))?, src_len)
            };
            vec![
                JsValue::from_object(ta.buffer),
                JsValue::from_f64(new_off as f64),
                JsValue::from_f64(end.saturating_sub(begin) as f64),
            ]
        };
        let (out_h, _) = ta_species_create(vm, ta_h, ta, &ctor_args, None)?;
        Ok(JsValue::from_object(out_h))
    });
    define_method_len(vm, tap, "slice", 2, |vm, t, args| {
        let (ta_h, ta, src_len_u32) = require_valid_ta(vm, t)?;
        let len = src_len_u32 as usize;
        let begin = clamp_rel_usize(vm.to_integer_or_infinity(arg(args, 0))?, len);
        let end = if arg(args, 1).is_undefined() {
            len
        } else {
            clamp_rel_usize(vm.to_integer_or_infinity(arg(args, 1))?, len)
        };
        let count = end.saturating_sub(begin) as u32;
        let (out_h, out_ta) = ta_species_create(
            vm,
            ta_h,
            ta,
            &[JsValue::from_f64(f64::from(count))],
            Some(count),
        )?;
        if count > 0 {
            let Some(cur_len) = ta_effective_len(vm, ta_h, ta) else {
                return Err(vm.type_error("TypedArray buffer was detached during slice"));
            };
            let avail = (cur_len as usize).saturating_sub(begin).min(count as usize);
            for i in 0..avail {
                let v = typed_array_read_element(vm, ta, (begin + i) as u32);
                typed_array_write_element_checked(vm, out_h, out_ta, i as u32, v)?;
            }
        }
        Ok(JsValue::from_object(out_h))
    });
    define_method_len(vm, tap, "set", 1, |vm, t, args| {
        let (ta_h, ta, target_len) = require_valid_ta(vm, t)?;
        let src = arg(args, 0);
        let off_f = vm.to_integer_or_infinity(arg(args, 1))?;
        if off_f < 0.0 {
            return Err(vm.range_error("Offset is out of bounds"));
        }
        let off = if off_f.is_infinite() {
            usize::MAX
        } else {
            off_f as usize
        };
        let src_ta_opt = src
            .as_object()
            .and_then(|sh| vm.heap.get_object(sh).map(|o| (sh, &o.kind)))
            .and_then(|(sh, k)| match k {
                ObjectKind::TypedArray(sta) => Some((sh, *sta)),
                _ => None,
            });
        if let Some((sh, sta)) = src_ta_opt {
            let Some(cur_target_len) = ta_effective_len(vm, ta_h, ta) else {
                return Err(vm.type_error("Target TypedArray buffer is detached or out of bounds"));
            };
            let Some(src_len) = ta_effective_len(vm, sh, sta) else {
                return Err(vm.type_error("Source TypedArray buffer is detached or out of bounds"));
            };
            if sta.kind.is_bigint() != ta.kind.is_bigint() {
                return Err(vm.type_error("Cannot mix BigInt and other TypedArray types"));
            }
            if off
                .checked_add(src_len as usize)
                .is_none_or(|e| e > cur_target_len as usize)
            {
                return Err(vm.range_error("Source is too large for TypedArray"));
            }
            let mut snapshot = Vec::with_capacity(src_len as usize);
            for i in 0..src_len {
                snapshot.push(typed_array_read_element(vm, sta, i));
            }
            for (i, v) in snapshot.into_iter().enumerate() {
                typed_array_write_element_checked(vm, ta_h, ta, (off + i) as u32, v)?;
            }
            return Ok(JsValue::UNDEFINED);
        }
        let src_h = vm.to_object(src)?;
        let src_len = vm.array_get_len(src_h)?;
        if ta_effective_len(vm, ta_h, ta).is_none() {
            return Err(vm.type_error("Target TypedArray buffer is detached or out of bounds"));
        }
        if (off as u64)
            .checked_add(src_len)
            .is_none_or(|e| e > u64::from(target_len))
        {
            return Err(vm.range_error("Source is too large for TypedArray"));
        }
        for i in 0..src_len {
            let v = vm.array_get_index(src_h, i)?;
            typed_array_write_element_checked(vm, ta_h, ta, (off as u64 + i) as u32, v)?;
        }
        Ok(JsValue::UNDEFINED)
    });
    define_method_len(vm, tap, "fill", 1, |vm, t, args| {
        let (ta_h, ta, len_u32) = require_valid_ta(vm, t)?;
        let len = len_u32 as usize;
        let coerced_val = if ta.kind.is_bigint() {
            let bits = to_bigint_i64(vm, arg(args, 0))?;
            JsValue::from_bigint(vm.heap.alloc_bigint(JsBigInt::from_i64(bits)))
        } else {
            JsValue::from_f64(vm.to_number(arg(args, 0))?)
        };
        let start = clamp_rel_usize(vm.to_integer_or_infinity(arg(args, 1))?, len);
        let end = if arg(args, 2).is_undefined() {
            len
        } else {
            clamp_rel_usize(vm.to_integer_or_infinity(arg(args, 2))?, len)
        };
        let Some(cur_len) = ta_effective_len(vm, ta_h, ta) else {
            return Err(vm.type_error("TypedArray buffer was detached during fill"));
        };
        let end = end.min(cur_len as usize);
        for i in start..end {
            typed_array_write_element_checked(vm, ta_h, ta, i as u32, coerced_val)?;
        }
        Ok(t)
    });
    define_method_len(vm, tap, "at", 1, |vm, t, args| {
        let (ta_h, ta, len_u32) = require_valid_ta(vm, t)?;
        let len = len_u32 as f64;
        let rel = vm.to_integer_or_infinity(arg(args, 0))?;
        let k = if rel >= 0.0 { rel } else { len + rel };
        if k < 0.0 || k >= len {
            return Ok(JsValue::UNDEFINED);
        }
        let idx = k as u32;
        if ta_effective_len(vm, ta_h, ta).is_some_and(|l| idx < l) {
            Ok(typed_array_read_element(vm, ta, idx))
        } else {
            Ok(JsValue::UNDEFINED)
        }
    });
    define_method_len(vm, tap, "indexOf", 1, |vm, t, args| {
        let (ta_h, ta, len_u32) = require_valid_ta(vm, t)?;
        let len = len_u32 as usize;
        if len == 0 {
            return Ok(JsValue::from_i32(-1));
        }
        let target = arg(args, 0);
        let n = vm.to_integer_or_infinity(arg(args, 1))?;
        if n == f64::INFINITY {
            return Ok(JsValue::from_i32(-1));
        }
        let from = clamp_rel_usize(n, len);
        for i in from..len {
            if ta_effective_len(vm, ta_h, ta).is_some_and(|l| (i as u32) < l) {
                let v = typed_array_read_element(vm, ta, i as u32);
                if vm.strict_eq(v, target) {
                    return Ok(JsValue::from_f64(i as f64));
                }
            }
        }
        Ok(JsValue::from_i32(-1))
    });
    define_method_len(vm, tap, "includes", 1, |vm, t, args| {
        let (ta_h, ta, len_u32) = require_valid_ta(vm, t)?;
        let len = len_u32 as usize;
        if len == 0 {
            return Ok(JsValue::FALSE);
        }
        let target = arg(args, 0);
        let n = vm.to_integer_or_infinity(arg(args, 1))?;
        if n == f64::INFINITY {
            return Ok(JsValue::FALSE);
        }
        let from = clamp_rel_usize(n, len);
        for i in from..len {
            let v = if ta_effective_len(vm, ta_h, ta).is_some_and(|l| (i as u32) < l) {
                typed_array_read_element(vm, ta, i as u32)
            } else {
                JsValue::UNDEFINED
            };
            if same_value_zero(vm, v, target) {
                return Ok(JsValue::TRUE);
            }
        }
        Ok(JsValue::FALSE)
    });
    define_method_len(vm, tap, "forEach", 1, |vm, t, args| {
        let (ta_h, ta, len) = require_valid_ta(vm, t)?;
        let (cb, this_arg) = (arg(args, 0), arg(args, 1));
        require_callable(vm, cb)?;
        for i in 0..len {
            let v = if ta_effective_len(vm, ta_h, ta).is_some_and(|l| i < l) {
                typed_array_read_element(vm, ta, i)
            } else {
                JsValue::UNDEFINED
            };
            vm.call_value(cb, this_arg, &[v, JsValue::from_f64(f64::from(i)), t])?;
        }
        Ok(JsValue::UNDEFINED)
    });
    define_method_len(vm, tap, "map", 1, |vm, t, args| {
        let (ta_h, ta, len) = require_valid_ta(vm, t)?;
        let (cb, this_arg) = (arg(args, 0), arg(args, 1));
        require_callable(vm, cb)?;
        let (out_h, out_ta) = ta_species_create(
            vm,
            ta_h,
            ta,
            &[JsValue::from_f64(f64::from(len))],
            Some(len),
        )?;
        for i in 0..len {
            let v = if ta_effective_len(vm, ta_h, ta).is_some_and(|l| i < l) {
                typed_array_read_element(vm, ta, i)
            } else {
                JsValue::UNDEFINED
            };
            let mapped = vm.call_value(cb, this_arg, &[v, JsValue::from_f64(f64::from(i)), t])?;
            typed_array_write_element_checked(vm, out_h, out_ta, i, mapped)?;
        }
        Ok(JsValue::from_object(out_h))
    });
    define_method_len(vm, tap, "filter", 1, |vm, t, args| {
        let (ta_h, ta, len) = require_valid_ta(vm, t)?;
        let (cb, this_arg) = (arg(args, 0), arg(args, 1));
        require_callable(vm, cb)?;
        let mut kept = Vec::new();
        for i in 0..len {
            let v = if ta_effective_len(vm, ta_h, ta).is_some_and(|l| i < l) {
                typed_array_read_element(vm, ta, i)
            } else {
                JsValue::UNDEFINED
            };
            let keep = vm.call_value(cb, this_arg, &[v, JsValue::from_f64(f64::from(i)), t])?;
            if vm.to_boolean(keep) {
                kept.push(v);
            }
        }
        let cnt = kept.len() as u32;
        let (out_h, out_ta) = ta_species_create(
            vm,
            ta_h,
            ta,
            &[JsValue::from_f64(f64::from(cnt))],
            Some(cnt),
        )?;
        for (i, v) in kept.into_iter().enumerate() {
            typed_array_write_element_checked(vm, out_h, out_ta, i as u32, v)?;
        }
        Ok(JsValue::from_object(out_h))
    });
    define_method_len(vm, tap, "reduce", 1, |vm, t, args| {
        let (ta_h, ta, len) = require_valid_ta(vm, t)?;
        let cb = arg(args, 0);
        require_callable(vm, cb)?;
        let (mut acc, start) = if args.len() >= 2 {
            (args[1], 0u32)
        } else {
            if len == 0 {
                return Err(vm.type_error("Reduce of empty TypedArray with no initial value"));
            }
            (typed_array_read_element(vm, ta, 0), 1u32)
        };
        for i in start..len {
            let v = if ta_effective_len(vm, ta_h, ta).is_some_and(|l| i < l) {
                typed_array_read_element(vm, ta, i)
            } else {
                JsValue::UNDEFINED
            };
            acc = vm.call_value(
                cb,
                JsValue::UNDEFINED,
                &[acc, v, JsValue::from_f64(f64::from(i)), t],
            )?;
        }
        Ok(acc)
    });
    define_method_len(vm, tap, "find", 1, |vm, t, args| {
        ta_find_impl(vm, t, args, false, false)
    });
    define_method_len(vm, tap, "findIndex", 1, |vm, t, args| {
        ta_find_impl(vm, t, args, true, false)
    });
    define_method_len(vm, tap, "findLast", 1, |vm, t, args| {
        ta_find_impl(vm, t, args, false, true)
    });
    define_method_len(vm, tap, "findLastIndex", 1, |vm, t, args| {
        ta_find_impl(vm, t, args, true, true)
    });
    define_method_len(vm, tap, "some", 1, |vm, t, args| {
        ta_quantifier(vm, t, args, true)
    });
    define_method_len(vm, tap, "every", 1, |vm, t, args| {
        ta_quantifier(vm, t, args, false)
    });
    define_method_len(vm, tap, "reverse", 0, |vm, t, _| {
        let (ta_h, ta, len) = require_valid_ta(vm, t)?;
        for i in 0..(len / 2) {
            let j = len - 1 - i;
            let a = typed_array_read_element(vm, ta, i);
            let b = typed_array_read_element(vm, ta, j);
            typed_array_write_element_checked(vm, ta_h, ta, i, b)?;
            typed_array_write_element_checked(vm, ta_h, ta, j, a)?;
        }
        Ok(t)
    });
    define_method_len(vm, tap, "sort", 1, |vm, t, args| {
        let cmp = arg(args, 0);
        if !cmp.is_undefined() {
            require_callable(vm, cmp)?;
        }
        let (ta_h, ta, len) = require_valid_ta(vm, t)?;
        let mut vals = Vec::with_capacity(len as usize);
        for i in 0..len {
            vals.push(typed_array_read_element(vm, ta, i));
        }
        if cmp.is_undefined() {
            vals.sort_by(|a, b| {
                if let (Some(ha), Some(hb)) = (a.as_bigint(), b.as_bigint()) {
                    let ba = vm.heap.get_bigint(ha).map_or(0, JsBigInt::to_i64_lossy);
                    let bb = vm.heap.get_bigint(hb).map_or(0, JsBigInt::to_i64_lossy);
                    return if ta.kind == TypedArrayKind::BigUint64 {
                        (ba as u64).cmp(&(bb as u64))
                    } else {
                        ba.cmp(&bb)
                    };
                }
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
            typed_array_write_element_checked(vm, ta_h, ta, i as u32, v)?;
        }
        Ok(t)
    });
    define_method_len(vm, tap, "join", 1, |vm, t, args| {
        let (ta_h, ta, len) = require_valid_ta(vm, t)?;
        let sep = if arg(args, 0).is_undefined() {
            ",".to_string()
        } else {
            vm.to_js_string(arg(args, 0))?
        };
        let parts: Vec<String> = (0..len)
            .map(|i| {
                if ta_effective_len(vm, ta_h, ta).is_some_and(|l| i < l) {
                    typed_array_read_element_string(vm, ta, i)
                } else {
                    String::new()
                }
            })
            .collect();
        Ok(JsValue::from_string(vm.heap.alloc_string(parts.join(&sep))))
    });
    // ECMA-262 §23.2.3.30: `%TypedArray%.prototype.toString === Array.prototype.toString`.
    let k_to_str = vm.heap.intern_key("toString");
    if let Ok(arr_to_str) = vm.get_property_by_key(vm.array_proto, k_to_str) {
        vm.heap.object_define_data(
            tap,
            k_to_str,
            arr_to_str,
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }
    define_method_len(vm, tap, "toLocaleString", 0, |vm, t, _| {
        let (ta_h, ta, len) = require_valid_ta(vm, t)?;
        let mut parts = Vec::with_capacity(len as usize);
        let k_tls = vm.heap.intern_key("toLocaleString");
        for i in 0..len {
            if ta_effective_len(vm, ta_h, ta).is_some_and(|l| i < l) {
                let v = typed_array_read_element(vm, ta, i);
                let vh = vm.to_object(v)?;
                let f = vm.get_property_by_key(vh, k_tls)?;
                let r = vm.call_value(f, v, &[])?;
                parts.push(vm.to_js_string(r)?);
            } else {
                parts.push(String::new());
            }
        }
        Ok(JsValue::from_string(vm.heap.alloc_string(parts.join(","))))
    });
    define_method_len(vm, tap, "values", 0, typed_array_values_fn);
    let k_values = vm.heap.intern_key("values");
    if let Ok(vals_fn) = vm.get_property_by_key(tap, k_values) {
        let k_iter = vm.key_symbol_iterator;
        vm.heap
            .object_define_data(tap, k_iter, vals_fn, PropertyAttrs::METHOD, &mut vm.shapes);
    }
    define_method_len(vm, tap, "keys", 0, |vm, t, _| {
        let (_, _, len) = require_valid_ta(vm, t)?;
        let items: Vec<JsValue> = (0..len).map(|i| JsValue::from_f64(f64::from(i))).collect();
        Ok(JsValue::from_object(alloc_values_iterator(vm, items)))
    });
    define_method_len(vm, tap, "entries", 0, |vm, t, _| {
        let (_, ta, len) = require_valid_ta(vm, t)?;
        let mut pairs = Vec::with_capacity(len as usize);
        for i in 0..len {
            let elem = typed_array_read_element(vm, ta, i);
            let p = vm.alloc_array();
            vm.heap
                .object_push_element(p, JsValue::from_f64(f64::from(i)));
            vm.heap.object_push_element(p, elem);
            pairs.push(JsValue::from_object(p));
        }
        Ok(JsValue::from_object(alloc_values_iterator(vm, pairs)))
    });
    define_method_len(vm, tap, "toReversed", 0, |vm, t, _| {
        let (_, ta, len) = require_valid_ta(vm, t)?;
        let (out_h, out_ta) = alloc_typed_array_pair(vm, ta.kind, len);
        for i in 0..len {
            let v = typed_array_read_element(vm, ta, len - 1 - i);
            typed_array_write_element(vm, out_ta, i, v)?;
        }
        Ok(JsValue::from_object(out_h))
    });
    define_method_len(vm, tap, "toSorted", 1, |vm, t, args| {
        let cmp = arg(args, 0);
        if !cmp.is_undefined() {
            require_callable(vm, cmp)?;
        }
        let (_, ta, len) = require_valid_ta(vm, t)?;
        let (out_h, out_ta) = alloc_typed_array_pair(vm, ta.kind, len);
        for i in 0..len {
            let v = typed_array_read_element(vm, ta, i);
            typed_array_write_element(vm, out_ta, i, v)?;
        }
        let sort_k = vm.heap.intern_key("sort");
        let sort_fn = vm.get_property_by_key(out_h, sort_k)?;
        vm.call_value(sort_fn, JsValue::from_object(out_h), args)
    });
    define_method_len(vm, tap, "with", 2, |vm, t, args| {
        let (ta_h, ta, len_u32) = require_valid_ta(vm, t)?;
        let len = len_u32 as f64;
        let rel = vm.to_integer_or_infinity(arg(args, 0))?;
        let actual_f = if rel >= 0.0 { rel } else { len + rel };
        let new_val = if ta.kind.is_bigint() {
            let bits = to_bigint_i64(vm, arg(args, 1))?;
            JsValue::from_bigint(vm.heap.alloc_bigint(JsBigInt::from_i64(bits)))
        } else {
            JsValue::from_f64(vm.to_number(arg(args, 1))?)
        };
        if actual_f < 0.0 || actual_f >= len {
            return Err(vm.range_error("Invalid index in TypedArray.prototype.with"));
        }
        let Some(cur_len) = ta_effective_len(vm, ta_h, ta) else {
            return Err(vm.type_error("TypedArray buffer was detached"));
        };
        let actual = actual_f as u32;
        if actual >= cur_len {
            return Err(vm.range_error("Invalid index in TypedArray.prototype.with"));
        }
        let (out_h, out_ta) = alloc_typed_array_pair(vm, ta.kind, len_u32);
        for i in 0..len_u32 {
            let v = if i == actual {
                new_val
            } else if i < cur_len {
                typed_array_read_element(vm, ta, i)
            } else {
                JsValue::UNDEFINED
            };
            typed_array_write_element(vm, out_ta, i, v)?;
        }
        Ok(JsValue::from_object(out_h))
    });
    define_method_len(vm, tap, "lastIndexOf", 1, |vm, t, args| {
        let (ta_h, ta, len_u32) = require_valid_ta(vm, t)?;
        if len_u32 == 0 {
            return Ok(JsValue::from_i32(-1));
        }
        let target = arg(args, 0);
        let len = len_u32 as f64;
        let n = if args.len() > 1 {
            vm.to_integer_or_infinity(arg(args, 1))?
        } else {
            len - 1.0
        };
        let k_f = if n >= 0.0 { n.min(len - 1.0) } else { len + n };
        if k_f < 0.0 {
            return Ok(JsValue::from_i32(-1));
        }
        for i in (0..=k_f as u32).rev() {
            if ta_effective_len(vm, ta_h, ta).is_some_and(|l| i < l) {
                let v = typed_array_read_element(vm, ta, i);
                if vm.strict_eq(v, target) {
                    return Ok(JsValue::from_f64(f64::from(i)));
                }
            }
        }
        Ok(JsValue::from_i32(-1))
    });
    define_method_len(vm, tap, "reduceRight", 1, |vm, t, args| {
        let (ta_h, ta, len) = require_valid_ta(vm, t)?;
        let cb = arg(args, 0);
        require_callable(vm, cb)?;
        let mut acc = (args.len() > 1).then(|| arg(args, 1));
        for i in (0..len).rev() {
            let v = if ta_effective_len(vm, ta_h, ta).is_some_and(|l| i < l) {
                typed_array_read_element(vm, ta, i)
            } else {
                JsValue::UNDEFINED
            };
            acc = Some(if let Some(cur) = acc {
                vm.call_value(
                    cb,
                    JsValue::UNDEFINED,
                    &[cur, v, JsValue::from_f64(f64::from(i)), t],
                )?
            } else {
                v
            });
        }
        acc.ok_or_else(|| vm.type_error("Reduce of empty TypedArray with no initial value"))
    });
    define_method_len(vm, tap, "copyWithin", 2, |vm, t, args| {
        let (ta_h, ta, len_u32) = require_valid_ta(vm, t)?;
        let len = len_u32 as usize;
        let to = clamp_rel_usize(vm.to_integer_or_infinity(arg(args, 0))?, len);
        let from = clamp_rel_usize(vm.to_integer_or_infinity(arg(args, 1))?, len);
        let end = if arg(args, 2).is_undefined() {
            len
        } else {
            clamp_rel_usize(vm.to_integer_or_infinity(arg(args, 2))?, len)
        };
        let Some(cur_len) = ta_effective_len(vm, ta_h, ta) else {
            return Err(vm.type_error("TypedArray buffer was detached during copyWithin"));
        };
        let cl = cur_len as usize;
        let count = end.min(cl).saturating_sub(from).min(cl.saturating_sub(to));
        let tmp: Vec<_> = (0..count)
            .map(|i| typed_array_read_element(vm, ta, (from + i) as u32))
            .collect();
        for (i, v) in tmp.into_iter().enumerate() {
            typed_array_write_element_checked(vm, ta_h, ta, (to + i) as u32, v)?;
        }
        Ok(t)
    });

    // Install each concrete TypedArray constructor inheriting from `%TypedArray%`.
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
        vm.set_fn_length(ctor, 3);
        vm.heap
            .object_define_data(ctor, k_bpe, bpe_val, PropertyAttrs::empty(), &mut vm.shapes);
        bind_ctor_proto(vm, ctor, proto);
        vm.heap
            .object_set_prototype(ctor, Some(typed_array_ctor), &mut vm.shapes);
        define_global(vm, kind.name(), JsValue::from_object(ctor));
    }
}

fn collect_array_like_only(vm: &mut JsVm, src: JsValue) -> Result<Vec<JsValue>, JsException> {
    let obj = vm.to_object(src)?;
    let len = vm.array_get_len(obj)?.min(4_000_000) as usize;
    let mut out = Vec::with_capacity(len.min(16_384));
    for i in 0..len {
        out.push(vm.array_get_index(obj, i as u64)?);
    }
    Ok(out)
}

fn collect_iterable_or_array_like(
    vm: &mut JsVm,
    src: JsValue,
) -> Result<Vec<JsValue>, JsException> {
    if src.is_null() || src.is_undefined() {
        return Err(vm.type_error("Cannot convert undefined or null to object"));
    }
    if src.is_string() {
        return vm.collect_iterable(src);
    }
    if let Some(obj) = src.as_object() {
        let iter_m = vm.get_property_by_key(obj, vm.key_symbol_iterator)?;
        if !iter_m.is_undefined() && !iter_m.is_null() {
            return vm.collect_iterable(src);
        }
        return collect_array_like_only(vm, src);
    }
    Ok(Vec::new())
}

fn typed_array_values_fn(
    vm: &mut JsVm,
    t: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let (_, ta, len) = require_valid_ta(vm, t)?;
    let mut items = Vec::with_capacity(len as usize);
    for i in 0..len {
        items.push(typed_array_read_element(vm, ta, i));
    }
    Ok(JsValue::from_object(alloc_values_iterator(vm, items)))
}

// ---------------------------------------------------------------------------
// SharedArrayBuffer (ECMA-262 §25.2, real cross-thread SharedBufferBacking)
// ---------------------------------------------------------------------------

fn require_sab(vm: &mut JsVm, t: JsValue) -> Result<HeapHandle, JsException> {
    let h = require_any_buffer(vm, t)?;
    if !ab_is_shared(vm, h) {
        return Err(vm.type_error("SharedArrayBuffer incompatible receiver"));
    }
    Ok(h)
}

fn install_shared_array_buffer(vm: &mut JsVm) {
    let sab_proto = vm.shared_array_buffer_proto;
    define_getter(vm, sab_proto, "byteLength", |vm, t, _| {
        let h = require_sab(vm, t)?;
        Ok(JsValue::from_f64(ab_byte_len(vm, h) as f64))
    });
    define_getter(vm, sab_proto, "maxByteLength", |vm, t, _| {
        let h = require_sab(vm, t)?;
        let max = ab_max_byte_length(vm, h).unwrap_or_else(|| ab_byte_len(vm, h));
        Ok(JsValue::from_f64(max as f64))
    });
    define_getter(vm, sab_proto, "growable", |vm, t, _| {
        let h = require_sab(vm, t)?;
        Ok(JsValue::from_bool(ab_max_byte_length(vm, h).is_some()))
    });
    define_method_len(vm, sab_proto, "grow", 1, |vm, t, args| {
        let h = require_sab(vm, t)?;
        let Some(shared) = ab_shared_backing(vm, h) else {
            return Err(vm.type_error("SharedArrayBuffer incompatible receiver"));
        };
        if shared.max_byte_length.is_none() {
            return Err(vm.type_error("SharedArrayBuffer is not growable"));
        }
        let new_len = to_index(vm, arg(args, 0))?;
        match shared.grow(new_len) {
            Ok(()) => Ok(JsValue::UNDEFINED),
            Err("not_growable") => Err(vm.type_error("SharedArrayBuffer is not growable")),
            Err(_) => Err(vm.range_error("Invalid SharedArrayBuffer grow byteLength")),
        }
    });
    define_method_len(vm, sab_proto, "slice", 2, |vm, t, args| {
        let h = require_sab(vm, t)?;
        let len = ab_byte_len(vm, h);
        let first = clamp_rel_usize(vm.to_integer_or_infinity(arg(args, 0))?, len);
        let fin = if arg(args, 1).is_undefined() {
            len
        } else {
            clamp_rel_usize(vm.to_integer_or_infinity(arg(args, 1))?, len)
        };
        let new_len = fin.saturating_sub(first);
        let k_sab = vm.heap.intern_key("SharedArrayBuffer");
        let default_ctor = vm
            .get_property_by_key(vm.global, k_sab)
            .ok()
            .and_then(|v| v.as_object())
            .unwrap_or(vm.shared_array_buffer_proto);
        let ctor = species_ctor_for(vm, h, default_ctor)?;
        let new_val = vm.construct_value(ctor, &[JsValue::from_f64(new_len as f64)], ctor)?;
        let new_h = require_sab(vm, new_val)?;
        if new_h == h {
            return Err(vm.type_error("Species constructor returned same SharedArrayBuffer"));
        }
        if ab_byte_len(vm, new_h) < new_len {
            return Err(vm.type_error("Species constructor returned too-small SharedArrayBuffer"));
        }
        if new_len > 0 {
            let copied = ab_copy_slice(vm, h, first, first + new_len);
            if let Some(dst_shared) = ab_shared_backing(vm, new_h) {
                if let Ok(mut g) = dst_shared.data.lock() {
                    let n = copied.len().min(g.len());
                    g[..n].copy_from_slice(&copied[..n]);
                }
            }
        }
        Ok(new_val)
    });
    define_to_string_tag(vm, sab_proto, "SharedArrayBuffer");

    let ctor = vm.alloc_native_fn("SharedArrayBuffer", |vm, _this, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("Constructor SharedArrayBuffer requires 'new'"));
        }
        let len = to_index(vm, arg(args, 0))?;
        let max_len = parse_max_byte_length_opt(vm, arg(args, 1), len)?;
        if len > 64 * 1024 * 1024 {
            return Err(vm.range_error("Invalid SharedArrayBuffer length"));
        }
        let backing = std::sync::Arc::new(SharedBufferBacking::new(vec![0u8; len], max_len));
        Ok(JsValue::from_object(
            vm.alloc_shared_array_buffer_from_backing(backing),
        ))
    });
    vm.set_fn_length(ctor, 1);
    define_species_getter(vm, ctor);
    bind_ctor_proto(vm, ctor, sab_proto);
    define_global(vm, "SharedArrayBuffer", JsValue::from_object(ctor));
}

// ---------------------------------------------------------------------------
// ES2025 Iterator & Iterator Helpers (ECMA-262 §27.1.3)
// ---------------------------------------------------------------------------

fn step_js_iterator(vm: &mut JsVm, iter_h: HeapHandle) -> Result<Option<JsValue>, JsException> {
    vm.loop_iterations = vm.loop_iterations.saturating_add(1);
    if let Some(limit) = vm.max_loop_iterations {
        if vm.loop_iterations > limit {
            return Err(vm.range_error("Script execution iteration limit exceeded"));
        }
    }
    let next_fn = vm.get_property_by_key(iter_h, vm.key_next)?;
    let res = vm.call_value(next_fn, JsValue::from_object(iter_h), &[])?;
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

fn iter_done(vm: &mut JsVm) -> JsValue {
    crate::promise::make_iter_result(vm, JsValue::UNDEFINED, true)
}

fn iter_val(vm: &mut JsVm, val: JsValue) -> JsValue {
    crate::promise::make_iter_result(vm, val, false)
}

fn alloc_helper_iterator(
    vm: &mut JsVm,
    parent: HeapHandle,
    extra: Option<HeapHandle>,
    next_impl: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
) -> HeapHandle {
    let iter_obj = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.iterator_proto),
    ));
    let next_fn = vm.alloc_host_fn("next", next_impl);
    vm.heap.object_define_data(
        iter_obj,
        vm.key_next,
        JsValue::from_object(next_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    if let Some(o) = vm.heap.get_object_mut(iter_obj) {
        o.extra_refs.push(parent);
        if let Some(e) = extra {
            o.extra_refs.push(e);
        }
    }
    iter_obj
}

fn wrap_next_iter(vm: &mut JsVm, ih: HeapHandle) -> HeapHandle {
    alloc_helper_iterator(vm, ih, None, move |vm, _this, _| {
        let next_fn = vm.get_property_by_key(ih, vm.key_next)?;
        vm.call_value(next_fn, JsValue::from_object(ih), &[])
    })
}

fn require_iter_and_cb(
    vm: &mut JsVm,
    t: JsValue,
    args: &[JsValue],
    name: &str,
) -> Result<(HeapHandle, JsValue), JsException> {
    let Some(src_h) = t.as_object() else {
        return Err(vm.type_error(format!("Iterator.prototype.{name} called on non-object")));
    };
    let cb = arg(args, 0);
    if !cb.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
        return Err(vm.type_error(format!("Iterator.prototype.{name} requires a function")));
    }
    Ok((src_h, cb))
}

fn require_iter_limit(
    vm: &mut JsVm,
    t: JsValue,
    args: &[JsValue],
    name: &str,
) -> Result<(HeapHandle, u64), JsException> {
    let Some(src_h) = t.as_object() else {
        return Err(vm.type_error(format!("Iterator.prototype.{name} called on non-object")));
    };
    let n = vm.to_number(arg(args, 0))?;
    if n.is_nan() || n < 0.0 {
        return Err(vm.range_error(format!(
            "Iterator.prototype.{name} limit must be non-negative"
        )));
    }
    Ok((
        src_h,
        if n.is_infinite() {
            u64::MAX
        } else {
            n.trunc() as u64
        },
    ))
}

fn iter_predicate_scan(
    vm: &mut JsVm,
    t: JsValue,
    args: &[JsValue],
    mode: u8,
) -> Result<JsValue, JsException> {
    let name = match mode {
        0 => "some",
        1 => "every",
        _ => "find",
    };
    let (src_h, cb) = require_iter_and_cb(vm, t, args, name)?;
    let mut idx = 0u64;
    while let Some(v) = step_js_iterator(vm, src_h)? {
        let r = vm.call_value(cb, JsValue::UNDEFINED, &[v, JsValue::from_f64(idx as f64)])?;
        let hit = vm.to_boolean(r);
        match mode {
            0 if hit => return Ok(JsValue::TRUE),
            1 if !hit => return Ok(JsValue::FALSE),
            2 if hit => return Ok(v),
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

fn install_iterator(vm: &mut JsVm) {
    let ip = vm.iterator_proto;
    define_symbol_iterator(vm, ip, |_vm, this_val, _| Ok(this_val));
    let def_iter = |vm: &mut JsVm, name: &str, len: u32, f: NativeFnPtr| {
        crate::vm_builtins::define_method_len(vm, ip, name, len, f);
    };
    def_iter(vm, "map", 1, |vm, t, args| {
        let (src_h, mapper) = require_iter_and_cb(vm, t, args, "map")?;
        let counter = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let h = alloc_helper_iterator(vm, src_h, mapper.as_object(), move |vm, _, _| {
            let Some(val) = step_js_iterator(vm, src_h)? else {
                return Ok(iter_done(vm));
            };
            let idx = counter.replace(counter.get() + 1);
            let m = vm.call_value(
                mapper,
                JsValue::UNDEFINED,
                &[val, JsValue::from_f64(idx as f64)],
            )?;
            Ok(iter_val(vm, m))
        });
        Ok(JsValue::from_object(h))
    });
    def_iter(vm, "filter", 1, |vm, t, args| {
        let (src_h, pred) = require_iter_and_cb(vm, t, args, "filter")?;
        let counter = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let h = alloc_helper_iterator(vm, src_h, pred.as_object(), move |vm, _, _| loop {
            let Some(val) = step_js_iterator(vm, src_h)? else {
                return Ok(iter_done(vm));
            };
            let idx = counter.replace(counter.get() + 1);
            let keep = vm.call_value(
                pred,
                JsValue::UNDEFINED,
                &[val, JsValue::from_f64(idx as f64)],
            )?;
            if vm.to_boolean(keep) {
                return Ok(iter_val(vm, val));
            }
        });
        Ok(JsValue::from_object(h))
    });
    def_iter(vm, "take", 1, |vm, t, args| {
        let (src_h, lim) = require_iter_limit(vm, t, args, "take")?;
        let rem = std::rc::Rc::new(std::cell::Cell::new(lim));
        let h = alloc_helper_iterator(vm, src_h, None, move |vm, _, _| {
            if rem.get() == 0 {
                return Ok(iter_done(vm));
            }
            rem.set(rem.get() - 1);
            match step_js_iterator(vm, src_h)? {
                Some(val) => Ok(iter_val(vm, val)),
                None => {
                    rem.set(0);
                    Ok(iter_done(vm))
                }
            }
        });
        Ok(JsValue::from_object(h))
    });
    def_iter(vm, "drop", 1, |vm, t, args| {
        let (src_h, lim) = require_iter_limit(vm, t, args, "drop")?;
        let to_drop = std::rc::Rc::new(std::cell::Cell::new(lim));
        let h = alloc_helper_iterator(vm, src_h, None, move |vm, _, _| {
            while to_drop.get() > 0 {
                to_drop.set(to_drop.get() - 1);
                if step_js_iterator(vm, src_h)?.is_none() {
                    to_drop.set(0);
                    return Ok(iter_done(vm));
                }
            }
            match step_js_iterator(vm, src_h)? {
                Some(val) => Ok(iter_val(vm, val)),
                None => Ok(iter_done(vm)),
            }
        });
        Ok(JsValue::from_object(h))
    });
    def_iter(vm, "flatMap", 1, |vm, t, args| {
        let (src_h, mapper) = require_iter_and_cb(vm, t, args, "flatMap")?;
        let counter = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let buf = std::rc::Rc::new(std::cell::RefCell::new(std::collections::VecDeque::new()));
        let h = alloc_helper_iterator(vm, src_h, mapper.as_object(), move |vm, _, _| loop {
            if let Some(v) = buf.borrow_mut().pop_front() {
                return Ok(iter_val(vm, v));
            }
            let Some(val) = step_js_iterator(vm, src_h)? else {
                return Ok(iter_done(vm));
            };
            let idx = counter.replace(counter.get() + 1);
            let mapped = vm.call_value(
                mapper,
                JsValue::UNDEFINED,
                &[val, JsValue::from_f64(idx as f64)],
            )?;
            buf.borrow_mut().extend(vm.collect_iterable(mapped)?);
        });
        Ok(JsValue::from_object(h))
    });
    def_iter(vm, "toArray", 0, |vm, t, _| {
        let Some(src_h) = t.as_object() else {
            return Err(vm.type_error("Iterator.prototype.toArray called on non-object"));
        };
        let out = vm.alloc_array();
        while let Some(v) = step_js_iterator(vm, src_h)? {
            vm.heap.object_push_element(out, v);
        }
        Ok(JsValue::from_object(out))
    });
    def_iter(vm, "forEach", 1, |vm, t, args| {
        let (src_h, cb) = require_iter_and_cb(vm, t, args, "forEach")?;
        let mut idx = 0u64;
        while let Some(v) = step_js_iterator(vm, src_h)? {
            vm.call_value(cb, JsValue::UNDEFINED, &[v, JsValue::from_f64(idx as f64)])?;
            idx += 1;
        }
        Ok(JsValue::UNDEFINED)
    });
    def_iter(vm, "reduce", 1, |vm, t, args| {
        let (src_h, cb) = require_iter_and_cb(vm, t, args, "reduce")?;
        let mut idx = 0u64;
        let mut acc = if args.len() > 1 {
            arg(args, 1)
        } else {
            let Some(first) = step_js_iterator(vm, src_h)? else {
                return Err(vm.type_error("Reduce of empty iterator with no initial value"));
            };
            idx = 1;
            first
        };
        while let Some(v) = step_js_iterator(vm, src_h)? {
            acc = vm.call_value(
                cb,
                JsValue::UNDEFINED,
                &[acc, v, JsValue::from_f64(idx as f64)],
            )?;
            idx += 1;
        }
        Ok(acc)
    });
    def_iter(vm, "some", 1, |vm, t, args| {
        iter_predicate_scan(vm, t, args, 0)
    });
    def_iter(vm, "every", 1, |vm, t, args| {
        iter_predicate_scan(vm, t, args, 1)
    });
    def_iter(vm, "find", 1, |vm, t, args| {
        iter_predicate_scan(vm, t, args, 2)
    });

    let ctor = vm.alloc_native_fn("Iterator", |vm, this_val, _| {
        // Abstract base constructor: subclasses may invoke `super()`, direct `new Iterator()` throws TypeError.
        if let Some(h) = this_val.as_object() {
            if vm.heap.get_object(h).and_then(|o| o.prototype) == Some(vm.iterator_proto) {
                return Err(vm.type_error("Abstract class Iterator not directly constructable"));
            }
            return Ok(this_val);
        }
        Err(vm.type_error("Iterator constructor requires 'new'"))
    });
    define_method(vm, ctor, "from", |vm, _this, args| {
        let obj = arg(args, 0);
        if let Some(h) = obj.as_object() {
            let iter_m = vm.get_property_by_key(h, vm.key_symbol_iterator)?;
            if !iter_m.is_undefined() && !iter_m.is_null() {
                let it = vm.call_value(iter_m, obj, &[])?;
                if let Some(ih) = it.as_object() {
                    if vm.heap.get_object(ih).and_then(|o| o.prototype) == Some(vm.iterator_proto) {
                        return Ok(it);
                    }
                    return Ok(JsValue::from_object(wrap_next_iter(vm, ih)));
                }
            }
            if vm.has_property_by_key(h, vm.key_next)? {
                return Ok(JsValue::from_object(wrap_next_iter(vm, h)));
            }
        } else if obj.is_string() {
            let chars: Vec<JsValue> = vm
                .value_to_rust_string(obj)
                .chars()
                .map(|c| JsValue::from_string(vm.heap.alloc_string(c.to_string())))
                .collect();
            return Ok(JsValue::from_object(alloc_values_iterator(vm, chars)));
        }
        Err(vm.type_error("Iterator.from called on non-iterable"))
    });
    bind_ctor_proto(vm, ctor, ip);
    define_global(vm, "Iterator", JsValue::from_object(ctor));
}
