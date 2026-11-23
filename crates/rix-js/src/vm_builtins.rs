//! Minimal core intrinsics (`Proxy`, `Reflect`, `Object`, `Function.prototype`,
//! `Array.prototype`, `Error` constructors) bootstrapped by [`JsVm::new`].
//!
//! Full ECMAScript 2024 standard library built-ins (`Map`, `Set`, `Promise`,
//! `TypedArray`, `Date`, `RegExp`, `JSON`, etc.) build directly on these hooks.

use crate::heap::{BoundFunctionObject, JsObject, ObjectKind, ProxyObject};
use crate::shape::PropertyAttrs;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm, NativeFnPtr};

pub fn install_core_intrinsics(vm: &mut JsVm) {
    // Global constants
    define_global(vm, "undefined", JsValue::UNDEFINED);
    define_global(vm, "NaN", JsValue::NAN);
    define_global(vm, "Infinity", JsValue::from_f64(f64::INFINITY));
    let g = JsValue::from_object(vm.global);
    define_global(vm, "globalThis", g);

    // Function.prototype.{call, apply, bind}
    let fn_proto = vm.function_proto;
    define_method(vm, fn_proto, "call", fn_call);
    define_method(vm, fn_proto, "apply", fn_apply);
    define_method(vm, fn_proto, "bind", fn_bind);

    // Object.prototype.{hasOwnProperty, toString}
    let obj_proto = vm.object_proto;
    define_method(vm, obj_proto, "hasOwnProperty", obj_has_own_property);
    define_method(vm, obj_proto, "toString", obj_to_string);

    // Object constructor + statics
    let object_ctor = vm.alloc_native_fn("Object", object_ctor_fn);
    vm.heap.object_define_data(
        object_ctor,
        vm.key_prototype,
        JsValue::from_object(obj_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    define_method(vm, object_ctor, "create", object_create);
    define_method(vm, object_ctor, "keys", object_keys);
    define_method(vm, object_ctor, "values", object_values);
    define_method(vm, object_ctor, "entries", object_entries);
    define_method(vm, object_ctor, "assign", object_assign);
    define_method(vm, object_ctor, "getPrototypeOf", object_get_prototype_of);
    define_method(vm, object_ctor, "setPrototypeOf", object_set_prototype_of);
    define_method(vm, object_ctor, "hasOwn", object_has_own);
    define_method(vm, object_ctor, "freeze", object_freeze);
    define_method(vm, object_ctor, "is", object_is);
    define_global(vm, "Object", JsValue::from_object(object_ctor));

    // Array.prototype methods + Array constructor
    let arr_proto = vm.array_proto;
    define_method(vm, arr_proto, "push", array_push);
    define_method(vm, arr_proto, "pop", array_pop);
    define_method(vm, arr_proto, "map", array_map);
    define_method(vm, arr_proto, "filter", array_filter);
    define_method(vm, arr_proto, "reduce", array_reduce);
    define_method(vm, arr_proto, "forEach", array_for_each);
    define_method(vm, arr_proto, "join", array_join);
    define_method(vm, arr_proto, "slice", array_slice);
    define_method(vm, arr_proto, "indexOf", array_index_of);
    define_method(vm, arr_proto, "includes", array_includes);

    let array_ctor = vm.alloc_native_fn("Array", array_ctor_fn);
    vm.heap.object_define_data(
        array_ctor,
        vm.key_prototype,
        JsValue::from_object(arr_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    define_method(vm, array_ctor, "isArray", array_is_array);
    define_global(vm, "Array", JsValue::from_object(array_ctor));

    // Proxy & Reflect
    let proxy_ctor = vm.alloc_native_fn("Proxy", proxy_ctor_fn);
    define_method(vm, proxy_ctor, "revocable", proxy_revocable);
    define_global(vm, "Proxy", JsValue::from_object(proxy_ctor));

    let reflect = vm.alloc_ordinary_object();
    define_method(vm, reflect, "get", reflect_get);
    define_method(vm, reflect, "set", reflect_set);
    define_method(vm, reflect, "has", reflect_has);
    define_method(vm, reflect, "deleteProperty", reflect_delete_property);
    define_method(vm, reflect, "apply", reflect_apply);
    define_method(vm, reflect, "construct", reflect_construct);
    define_method(vm, reflect, "getPrototypeOf", reflect_get_prototype_of);
    define_method(vm, reflect, "setPrototypeOf", reflect_set_prototype_of);
    define_method(vm, reflect, "ownKeys", reflect_own_keys);
    define_global(vm, "Reflect", JsValue::from_object(reflect));

    // Error constructors
    for err_name in [
        "Error",
        "TypeError",
        "ReferenceError",
        "RangeError",
        "SyntaxError",
        "URIError",
        "EvalError",
    ] {
        let name_owned = err_name.to_owned();
        let ctor = vm.alloc_host_fn(err_name, move |vm, _this, args| {
            let msg = args
                .first()
                .filter(|v| !v.is_undefined())
                .map(|v| vm.value_to_rust_string(*v))
                .unwrap_or_default();
            Ok(vm.make_error(&name_owned, msg).value)
        });
        let ep = vm.error_proto;
        vm.heap.object_define_data(
            ctor,
            vm.key_prototype,
            JsValue::from_object(ep),
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
        define_global(vm, err_name, JsValue::from_object(ctor));
    }
}

pub fn define_global(vm: &mut JsVm, name: &str, value: JsValue) {
    let key = vm.heap.intern_key(name);
    vm.heap.object_define_data(
        vm.global,
        key,
        value,
        PropertyAttrs::WRITABLE | PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
}

pub fn define_method(vm: &mut JsVm, target: HeapHandle, name: &str, f: NativeFnPtr) {
    let fn_h = vm.alloc_native_fn(name, f);
    let key = vm.heap.intern_key(name);
    vm.heap.object_define_data(
        target,
        key,
        JsValue::from_object(fn_h),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
}

// ---------------------------------------------------------------------------
// Function.prototype
// ---------------------------------------------------------------------------

fn fn_call(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let bound_this = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let rest = if args.len() > 1 { &args[1..] } else { &[] };
    vm.call_value(this_val, bound_this, rest)
}

fn fn_apply(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let bound_this = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let mut call_args = Vec::new();
    if let Some(arr_h) = args.get(1).and_then(|v| v.as_object()) {
        let len = vm.heap.get_object(arr_h).map_or(0, |o| o.elements.len());
        if len > 1_000_000 {
            return Err(vm.range_error("Maximum call stack size exceeded"));
        }
        call_args.reserve(len as usize);
        for i in 0..len {
            let v = vm
                .heap
                .get_object(arr_h)
                .and_then(|o| o.elements.get(i))
                .unwrap_or(JsValue::UNDEFINED);
            call_args.push(v);
        }
    }
    vm.call_value(this_val, bound_this, &call_args)
}

fn fn_bind(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(target) = this_val.as_object() else {
        return Err(vm.type_error("Function.prototype.bind called on non-function"));
    };
    let bound_this = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let bound_args = if args.len() > 1 {
        args[1..].to_vec()
    } else {
        Vec::new()
    };
    let bound = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::BoundFunction(Box::new(BoundFunctionObject {
            target,
            bound_this,
            bound_args,
        })),
        Some(vm.function_proto),
    ));
    Ok(JsValue::from_object(bound))
}

// ---------------------------------------------------------------------------
// Object
// ---------------------------------------------------------------------------

fn object_ctor_fn(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    if let Some(&first) = args.first() {
        if first.is_object() {
            return Ok(first);
        }
    }
    Ok(JsValue::from_object(vm.alloc_ordinary_object()))
}

fn obj_has_own_property(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Ok(JsValue::FALSE);
    };
    let ks = vm.value_to_rust_string(args.first().copied().unwrap_or(JsValue::UNDEFINED));
    let key = vm.heap.intern_key(&ks);
    let has = vm.heap.object_get_own(h, key, &vm.shapes).is_some()
        || ks
            .parse::<u32>()
            .ok()
            .is_some_and(|i| vm.heap.get_object(h).is_some_and(|o| o.elements.has(i)));
    Ok(JsValue::from_bool(has))
}

fn obj_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let tag = if this_val.is_undefined() {
        "[object Undefined]"
    } else if this_val.is_null() {
        "[object Null]"
    } else if let Some(h) = this_val.as_object() {
        match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::Array) => "[object Array]",
            Some(
                ObjectKind::Closure(_)
                | ObjectKind::NativeFunction(_)
                | ObjectKind::BoundFunction(_),
            ) => "[object Function]",
            Some(ObjectKind::RegExp(_)) => "[object RegExp]",
            _ => "[object Object]",
        }
    } else {
        "[object Object]"
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(tag)))
}

fn object_create(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let proto_arg = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let proto = if proto_arg.is_null() {
        None
    } else if let Some(h) = proto_arg.as_object() {
        Some(h)
    } else {
        return Err(vm.type_error("Object prototype may only be an Object or null"));
    };
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, None));
    vm.heap.object_set_prototype(obj, proto, &mut vm.shapes);
    Ok(JsValue::from_object(obj))
}

fn object_keys(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let arr = vm.alloc_array();
    if let Some(h) = args.first().and_then(|v| v.as_object()) {
        for k in vm.own_enumerable_keys_including_elements(h) {
            let s = JsValue::from_string(vm.heap.alloc_string(k));
            vm.heap.object_push_element(arr, s);
        }
    }
    Ok(JsValue::from_object(arr))
}

fn object_values(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let arr = vm.alloc_array();
    if let Some(h) = args.first().and_then(|v| v.as_object()) {
        let keys = vm.own_enumerable_keys_including_elements(h);
        for ks in keys {
            let kv = JsValue::from_string(vm.heap.alloc_string(ks));
            let val = vm.get_elem(JsValue::from_object(h), kv)?;
            vm.heap.object_push_element(arr, val);
        }
    }
    Ok(JsValue::from_object(arr))
}

fn object_entries(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let arr = vm.alloc_array();
    if let Some(h) = args.first().and_then(|v| v.as_object()) {
        let keys = vm.own_enumerable_keys_including_elements(h);
        for ks in keys {
            let kv = JsValue::from_string(vm.heap.alloc_string(ks));
            let val = vm.get_elem(JsValue::from_object(h), kv)?;
            let pair = vm.alloc_array();
            vm.heap.object_push_element(pair, kv);
            vm.heap.object_push_element(pair, val);
            vm.heap.object_push_element(arr, JsValue::from_object(pair));
        }
    }
    Ok(JsValue::from_object(arr))
}

fn object_assign(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let target_val = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let Some(target) = target_val.as_object() else {
        return Err(vm.type_error("Cannot convert undefined or null to object"));
    };
    for &src_val in args.iter().skip(1) {
        if let Some(src) = src_val.as_object() {
            for k in vm.own_enumerable_keys(src) {
                let v = vm.get_property_by_key(src, k)?;
                vm.set_property_by_key(target, k, v)?;
            }
        }
    }
    Ok(target_val)
}

fn object_get_prototype_of(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::NULL);
    };
    let proto = vm.heap.get_object(h).and_then(|o| o.prototype);
    Ok(proto.map_or(JsValue::NULL, JsValue::from_object))
}

/// ECMA-262 §10.1.2.1 `OrdinarySetPrototypeOf`: rejects non-extensible changes and prototype cycles.
fn ordinary_set_prototype_of(vm: &mut JsVm, h: HeapHandle, proto: Option<HeapHandle>) -> bool {
    let Some(obj) = vm.heap.get_object(h) else {
        return false;
    };
    if obj.prototype == proto {
        return true;
    }
    if !obj.extensible {
        return false;
    }
    // Walk candidate prototype chain to prevent any cycle (including `p == h`).
    let mut cur = proto;
    let mut hops = 0usize;
    while let Some(p) = cur {
        if p == h || hops > 4096 {
            return false;
        }
        cur = vm.heap.get_object(p).and_then(|o| o.prototype);
        hops += 1;
    }
    vm.heap.object_set_prototype(h, proto, &mut vm.shapes);
    vm.mega_ic.invalidate_prototypes();
    true
}

fn object_set_prototype_of(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let obj_val = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    if obj_val.is_undefined() || obj_val.is_null() {
        return Err(vm.type_error("Object.setPrototypeOf called on null or undefined"));
    }
    let proto_arg = args.get(1).copied().unwrap_or(JsValue::NULL);
    let proto = if proto_arg.is_null() {
        None
    } else if let Some(p) = proto_arg.as_object() {
        Some(p)
    } else {
        return Err(vm.type_error("Object prototype may only be an Object or null"));
    };
    let Some(h) = obj_val.as_object() else {
        // Per ES spec §20.1.2.21 step 4: primitive targets return O unchanged after validating proto.
        return Ok(obj_val);
    };
    if !ordinary_set_prototype_of(vm, h, proto) {
        return Err(vm.type_error("Cyclic __proto__ value or non-extensible object"));
    }
    Ok(obj_val)
}

fn object_has_own(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let obj = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let prop = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    obj_has_own_property(vm, obj, &[prop])
}

fn object_freeze(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let target = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    if let Some(h) = target.as_object() {
        if let Some(o) = vm.heap.get_object_mut(h) {
            o.extensible = false;
        }
    }
    Ok(target)
}

fn object_is(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let a = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let b = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let eq = if a.is_string() || b.is_string() || a.is_bigint() || b.is_bigint() {
        vm.strict_eq(a, b)
    } else {
        a.same_value_fast(b)
    };
    Ok(JsValue::from_bool(eq))
}

// ---------------------------------------------------------------------------
// Array
// ---------------------------------------------------------------------------

fn array_ctor_fn(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let arr = vm.alloc_array();
    if args.len() == 1 && args[0].is_number() {
        let len = vm.to_u32(args[0])?;
        if let Some(o) = vm.heap.get_object_mut(arr) {
            o.elements.set_len(len);
        }
    } else {
        for &a in args {
            vm.heap.object_push_element(arr, a);
        }
    }
    Ok(JsValue::from_object(arr))
}

fn array_is_array(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let is_arr = args
        .first()
        .and_then(|v| v.as_object())
        .and_then(|h| vm.heap.get_object(h))
        .is_some_and(|o| matches!(o.kind, ObjectKind::Array));
    Ok(JsValue::from_bool(is_arr))
}

fn array_push(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Array.prototype.push called on non-object"));
    };
    for &a in args {
        vm.heap.object_push_element(h, a);
    }
    let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len());
    Ok(JsValue::from_f64(f64::from(len)))
}

fn array_pop(vm: &mut JsVm, this_val: JsValue, _args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Array.prototype.pop called on non-object"));
    };
    let val = vm
        .heap
        .get_object_mut(h)
        .and_then(|o| o.elements.pop())
        .unwrap_or(JsValue::UNDEFINED);
    Ok(val)
}

fn array_present_or_dense_indices(vm: &JsVm, h: HeapHandle) -> (u32, Option<Vec<u32>>) {
    let Some(o) = vm.heap.get_object(h) else {
        return (0, None);
    };
    let len = o.elements.len();
    if len > 65_536 {
        (len, Some(o.elements.present_indices()))
    } else {
        (len, None)
    }
}

fn array_map(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Array.prototype.map called on non-object"));
    };
    let cb = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let this_arg = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let (len, sparse_indices) = array_present_or_dense_indices(vm, h);
    let out = vm.alloc_array();
    if let Some(indices) = sparse_indices {
        if let Some(o) = vm.heap.get_object_mut(out) {
            o.elements.set_len(len);
        }
        for i in indices {
            if let Some(elem) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
                let mapped = vm.call_value(
                    cb,
                    this_arg,
                    &[elem, JsValue::from_f64(f64::from(i)), this_val],
                )?;
                if let Some(o) = vm.heap.get_object_mut(out) {
                    o.elements.set(i, mapped);
                }
            }
        }
    } else {
        for i in 0..len {
            if let Some(elem) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
                let mapped =
                    vm.call_value(cb, this_arg, &[elem, JsValue::from_i32(i as i32), this_val])?;
                vm.heap.object_push_element(out, mapped);
            } else if let Some(o) = vm.heap.get_object_mut(out) {
                o.elements.push_hole();
            }
        }
    }
    Ok(JsValue::from_object(out))
}

fn array_filter(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Array.prototype.filter called on non-object"));
    };
    let cb = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let this_arg = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let (len, sparse_indices) = array_present_or_dense_indices(vm, h);
    let out = vm.alloc_array();
    if let Some(indices) = sparse_indices {
        for i in indices {
            if let Some(elem) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
                let keep = vm.call_value(
                    cb,
                    this_arg,
                    &[elem, JsValue::from_f64(f64::from(i)), this_val],
                )?;
                if vm.to_boolean(keep) {
                    vm.heap.object_push_element(out, elem);
                }
            }
        }
    } else {
        for i in 0..len {
            if let Some(elem) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
                let keep =
                    vm.call_value(cb, this_arg, &[elem, JsValue::from_i32(i as i32), this_val])?;
                if vm.to_boolean(keep) {
                    vm.heap.object_push_element(out, elem);
                }
            }
        }
    }
    Ok(JsValue::from_object(out))
}

fn array_reduce(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Array.prototype.reduce called on non-object"));
    };
    let cb = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let (len, sparse_indices) = array_present_or_dense_indices(vm, h);
    if let Some(indices) = sparse_indices {
        let mut iter = indices.into_iter();
        let mut acc = if args.len() >= 2 {
            args[1]
        } else {
            let mut first = None;
            for i in iter.by_ref() {
                if let Some(v) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
                    first = Some(v);
                    break;
                }
            }
            first.ok_or_else(|| vm.type_error("Reduce of empty array with no initial value"))?
        };
        for i in iter {
            if let Some(elem) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
                acc = vm.call_value(
                    cb,
                    JsValue::UNDEFINED,
                    &[acc, elem, JsValue::from_f64(f64::from(i)), this_val],
                )?;
            }
        }
        return Ok(acc);
    }
    let mut start = 0u32;
    let mut acc = if args.len() >= 2 {
        args[1]
    } else {
        let mut first = None;
        while start < len {
            if let Some(v) = vm.heap.get_object(h).and_then(|o| o.elements.get(start)) {
                first = Some(v);
                start += 1;
                break;
            }
            start += 1;
        }
        first.ok_or_else(|| vm.type_error("Reduce of empty array with no initial value"))?
    };
    for i in start..len {
        if let Some(elem) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
            acc = vm.call_value(
                cb,
                JsValue::UNDEFINED,
                &[acc, elem, JsValue::from_i32(i as i32), this_val],
            )?;
        }
    }
    Ok(acc)
}

fn array_for_each(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Array.prototype.forEach called on non-object"));
    };
    let cb = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let this_arg = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let (len, sparse_indices) = array_present_or_dense_indices(vm, h);
    if let Some(indices) = sparse_indices {
        for i in indices {
            if let Some(elem) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
                vm.call_value(
                    cb,
                    this_arg,
                    &[elem, JsValue::from_f64(f64::from(i)), this_val],
                )?;
            }
        }
    } else {
        for i in 0..len {
            if let Some(elem) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
                vm.call_value(cb, this_arg, &[elem, JsValue::from_i32(i as i32), this_val])?;
            }
        }
    }
    Ok(JsValue::UNDEFINED)
}

fn array_join(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Array.prototype.join called on non-object"));
    };
    let sep = match args.first().copied() {
        Some(v) if !v.is_undefined() => vm.value_to_rust_string(v),
        _ => ",".to_owned(),
    };
    let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len());
    if len > 4_000_000 {
        return Err(vm.range_error("Invalid string length"));
    }
    let mut parts = Vec::with_capacity((len as usize).min(16_384));
    for i in 0..len {
        match vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
            Some(v) if !v.is_null() && !v.is_undefined() => parts.push(vm.value_to_rust_string(v)),
            _ => parts.push(String::new()),
        }
    }
    Ok(JsValue::from_string(vm.heap.alloc_string(parts.join(&sep))))
}

fn array_slice(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Array.prototype.slice called on non-object"));
    };
    let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len()) as i64;
    let rel_start = match args.first().copied() {
        Some(v) if !v.is_undefined() => vm.to_i32(v)? as i64,
        _ => 0,
    };
    let rel_end = match args.get(1).copied() {
        Some(v) if !v.is_undefined() => vm.to_i32(v)? as i64,
        _ => len,
    };
    let start = if rel_start < 0 {
        (len + rel_start).max(0)
    } else {
        rel_start.min(len)
    } as u32;
    let end = if rel_end < 0 {
        (len + rel_end).max(0)
    } else {
        rel_end.min(len)
    } as u32;
    let out = vm.alloc_array();
    if end.saturating_sub(start) > 65_536 {
        let indices = vm
            .heap
            .get_object(h)
            .map(|o| o.elements.present_indices())
            .unwrap_or_default();
        if let Some(o) = vm.heap.get_object_mut(out) {
            o.elements.set_len(end.saturating_sub(start));
        }
        for i in indices {
            if i >= start && i < end {
                if let Some(v) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
                    if let Some(o) = vm.heap.get_object_mut(out) {
                        o.elements.set(i - start, v);
                    }
                }
            }
        }
    } else {
        for i in start..end {
            if let Some(v) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
                vm.heap.object_push_element(out, v);
            } else if let Some(o) = vm.heap.get_object_mut(out) {
                o.elements.push_hole();
            }
        }
    }
    Ok(JsValue::from_object(out))
}

fn array_index_of(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Ok(JsValue::from_i32(-1));
    };
    let target = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let (len, sparse_indices) = array_present_or_dense_indices(vm, h);
    if let Some(indices) = sparse_indices {
        for i in indices {
            if let Some(v) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
                if vm.strict_eq(v, target) {
                    return Ok(JsValue::from_f64(f64::from(i)));
                }
            }
        }
    } else {
        for i in 0..len {
            if let Some(v) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
                if vm.strict_eq(v, target) {
                    return Ok(JsValue::from_i32(i as i32));
                }
            }
        }
    }
    Ok(JsValue::from_i32(-1))
}

fn array_includes(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let idx = array_index_of(vm, this_val, args)?;
    Ok(JsValue::from_bool(idx.as_i32() != Some(-1)))
}

// ---------------------------------------------------------------------------
// Proxy & Reflect
// ---------------------------------------------------------------------------

fn proxy_ctor_fn(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let (Some(target), Some(handler)) = (
        args.first().and_then(|v| v.as_object()),
        args.get(1).and_then(|v| v.as_object()),
    ) else {
        return Err(vm.type_error("Cannot create proxy with a non-object as target or handler"));
    };
    let proxy = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(ProxyObject {
            target,
            handler,
            revoked: false,
        }),
        None,
    ));
    Ok(JsValue::from_object(proxy))
}

fn proxy_revocable(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let proxy_val = proxy_ctor_fn(vm, JsValue::UNDEFINED, args)?;
    let proxy_h = proxy_val.as_object().unwrap();
    let revoke_fn = vm.alloc_host_fn("revoke", move |vm, _this, _args| {
        if let Some(o) = vm.heap.get_object_mut(proxy_h) {
            if let ObjectKind::Proxy(ref mut p) = o.kind {
                p.revoked = true;
            }
        }
        Ok(JsValue::UNDEFINED)
    });
    let out = vm.alloc_ordinary_object();
    let kp = vm.heap.intern_key("proxy");
    let kr = vm.heap.intern_key("revoke");
    vm.heap
        .object_define_data(out, kp, proxy_val, PropertyAttrs::DEFAULT, &mut vm.shapes);
    vm.heap.object_define_data(
        out,
        kr,
        JsValue::from_object(revoke_fn),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    Ok(JsValue::from_object(out))
}

fn reflect_get(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(target) = args.first().and_then(|v| v.as_object()) else {
        return Err(vm.type_error("Reflect.get called on non-object"));
    };
    let prop_str = vm.value_to_rust_string(args.get(1).copied().unwrap_or(JsValue::UNDEFINED));
    let key = vm.heap.intern_key(&prop_str);
    let receiver = args
        .get(2)
        .copied()
        .unwrap_or_else(|| JsValue::from_object(target));
    vm.get_property_with_receiver(target, key, receiver)
}

fn reflect_set(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(target) = args.first().and_then(|v| v.as_object()) else {
        return Err(vm.type_error("Reflect.set called on non-object"));
    };
    let prop_str = vm.value_to_rust_string(args.get(1).copied().unwrap_or(JsValue::UNDEFINED));
    let key = vm.heap.intern_key(&prop_str);
    let val = args.get(2).copied().unwrap_or(JsValue::UNDEFINED);
    let ok = vm.set_property_by_key(target, key, val)?;
    Ok(JsValue::from_bool(ok))
}

fn reflect_has(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(target) = args.first().and_then(|v| v.as_object()) else {
        return Err(vm.type_error("Reflect.has called on non-object"));
    };
    let prop_str = vm.value_to_rust_string(args.get(1).copied().unwrap_or(JsValue::UNDEFINED));
    let key = vm.heap.intern_key(&prop_str);
    let has = vm.has_property_by_key(target, key)?;
    Ok(JsValue::from_bool(has))
}

fn reflect_delete_property(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(target) = args.first().and_then(|v| v.as_object()) else {
        return Err(vm.type_error("Reflect.deleteProperty called on non-object"));
    };
    let prop_str = vm.value_to_rust_string(args.get(1).copied().unwrap_or(JsValue::UNDEFINED));
    let key = vm.heap.intern_key(&prop_str);
    let del = vm.delete_property_by_key(target, key)?;
    Ok(JsValue::from_bool(del))
}

fn reflect_apply(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let target = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let this_arg = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let arg_list = args.get(2).copied().unwrap_or(JsValue::UNDEFINED);
    fn_apply(vm, target, &[this_arg, arg_list])
}

fn reflect_construct(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let target = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let new_target = args.get(2).copied().unwrap_or(target);
    let mut call_args = Vec::new();
    if let Some(arr_h) = args.get(1).and_then(|v| v.as_object()) {
        let len = vm.heap.get_object(arr_h).map_or(0, |o| o.elements.len());
        if len > 1_000_000 {
            return Err(vm.range_error("Maximum call stack size exceeded"));
        }
        for i in 0..len {
            let v = vm
                .heap
                .get_object(arr_h)
                .and_then(|o| o.elements.get(i))
                .unwrap_or(JsValue::UNDEFINED);
            call_args.push(v);
        }
    }
    vm.construct_value(target, &call_args, new_target)
}

fn reflect_get_prototype_of(
    vm: &mut JsVm,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    object_get_prototype_of(vm, this, args)
}

fn reflect_set_prototype_of(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = args.first().and_then(|v| v.as_object()) else {
        return Err(vm.type_error("Reflect.setPrototypeOf called on non-object"));
    };
    let proto_arg = args.get(1).copied().unwrap_or(JsValue::NULL);
    let proto = if proto_arg.is_null() {
        None
    } else if let Some(p) = proto_arg.as_object() {
        Some(p)
    } else {
        return Err(vm.type_error("Object prototype may only be an Object or null"));
    };
    Ok(JsValue::from_bool(ordinary_set_prototype_of(vm, h, proto)))
}

fn reflect_own_keys(
    vm: &mut JsVm,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    object_keys(vm, this, args)
}
