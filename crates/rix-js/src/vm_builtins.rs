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
    // Global constants & core global functions
    define_global(vm, "undefined", JsValue::UNDEFINED);
    define_global(vm, "NaN", JsValue::NAN);
    define_global(vm, "Infinity", JsValue::from_f64(f64::INFINITY));
    let g = JsValue::from_object(vm.global);
    define_global(vm, "globalThis", g);
    let eval_fn = vm.alloc_native_fn("eval", global_eval_fn);
    vm.set_fn_length(eval_fn, 1);
    vm.eval_fn = Some(eval_fn);
    define_global(vm, "eval", JsValue::from_object(eval_fn));

    // Function.prototype.{call, apply, bind, toString} + Function constructor
    let fn_proto = vm.function_proto;
    vm.heap.object_define_data(
        fn_proto,
        vm.key_length,
        JsValue::from_i32(0),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    let empty_s = JsValue::from_string(vm.heap.alloc_string(""));
    vm.heap.object_define_data(
        fn_proto,
        vm.key_name,
        empty_s,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    define_method_len(vm, fn_proto, "call", 1, fn_call);
    define_method_len(vm, fn_proto, "apply", 2, fn_apply);
    define_method_len(vm, fn_proto, "bind", 1, fn_bind);
    define_method_len(vm, fn_proto, "toString", 0, fn_to_string);
    // Function.prototype[Symbol.hasInstance] (ECMA-262 §20.2.3.6: non-writable/enum/config)
    let has_inst_fn = vm.alloc_native_fn("[Symbol.hasInstance]", fn_has_instance);
    vm.set_fn_length(has_inst_fn, 1);
    let k_has_inst = vm.key_symbol_has_instance;
    vm.heap.object_define_data(
        fn_proto,
        k_has_inst,
        JsValue::from_object(has_inst_fn),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    // Restricted Function.prototype.{caller, arguments} thrower (ECMA-262 §10.2.4)
    let thrower = vm.alloc_native_fn("", |vm, _this, _args| {
        Err(vm.type_error("'caller', 'callee', and 'arguments' properties may not be accessed on strict mode functions"))
    });
    for restricted in ["caller", "arguments"] {
        let rk = vm.heap.intern_key(restricted);
        vm.heap.object_define_accessor(
            fn_proto,
            rk,
            Some(thrower),
            Some(thrower),
            PropertyAttrs::CONFIGURABLE,
            &mut vm.shapes,
        );
    }
    let function_ctor = vm.alloc_native_fn("Function", function_ctor_fn);
    vm.mark_constructor(function_ctor);
    vm.set_fn_length(function_ctor, 1);
    vm.heap.object_define_data(
        function_ctor,
        vm.key_prototype,
        JsValue::from_object(fn_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        fn_proto,
        vm.key_constructor,
        JsValue::from_object(function_ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    define_global(vm, "Function", JsValue::from_object(function_ctor));

    // Object.prototype.{hasOwnProperty, toString, valueOf, isPrototypeOf, propertyIsEnumerable}
    let obj_proto = vm.object_proto;
    define_method_len(vm, obj_proto, "hasOwnProperty", 1, obj_has_own_property);
    define_method_len(vm, obj_proto, "toString", 0, obj_to_string);
    define_method_len(vm, obj_proto, "valueOf", 0, obj_value_of);
    define_method_len(vm, obj_proto, "isPrototypeOf", 1, obj_is_prototype_of);
    define_method_len(
        vm,
        obj_proto,
        "propertyIsEnumerable",
        1,
        obj_property_is_enumerable,
    );
    define_method_len(vm, obj_proto, "toLocaleString", 0, obj_to_locale_string);
    // Annex B §B.2.2 legacy Object.prototype accessors & __proto__.
    define_method_len(vm, obj_proto, "__defineGetter__", 2, obj_define_getter);
    define_method_len(vm, obj_proto, "__defineSetter__", 2, obj_define_setter);
    define_method_len(vm, obj_proto, "__lookupGetter__", 1, obj_lookup_getter);
    define_method_len(vm, obj_proto, "__lookupSetter__", 1, obj_lookup_setter);
    let get_proto_fn = vm.alloc_native_fn("get __proto__", obj_get_proto_accessor);
    vm.set_fn_length(get_proto_fn, 0);
    let set_proto_fn = vm.alloc_native_fn("set __proto__", obj_set_proto_accessor);
    vm.set_fn_length(set_proto_fn, 1);
    let k_dunder_proto = vm.heap.intern_key("__proto__");
    vm.heap.object_define_accessor(
        obj_proto,
        k_dunder_proto,
        Some(get_proto_fn),
        Some(set_proto_fn),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    // Object constructor + statics
    let object_ctor = vm.alloc_native_fn("Object", object_ctor_fn);
    vm.mark_constructor(object_ctor);
    vm.set_fn_length(object_ctor, 1);
    vm.heap.object_define_data(
        object_ctor,
        vm.key_prototype,
        JsValue::from_object(obj_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        obj_proto,
        vm.key_constructor,
        JsValue::from_object(object_ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    define_method_len(vm, object_ctor, "create", 2, object_create);
    define_method_len(vm, object_ctor, "keys", 1, object_keys);
    define_method_len(vm, object_ctor, "values", 1, object_values);
    define_method_len(vm, object_ctor, "entries", 1, object_entries);
    define_method_len(vm, object_ctor, "assign", 2, object_assign);
    define_method_len(
        vm,
        object_ctor,
        "getPrototypeOf",
        1,
        object_get_prototype_of,
    );
    define_method_len(
        vm,
        object_ctor,
        "setPrototypeOf",
        2,
        object_set_prototype_of,
    );
    define_method_len(vm, object_ctor, "hasOwn", 2, object_has_own);
    define_method_len(vm, object_ctor, "freeze", 1, object_freeze);
    define_method_len(vm, object_ctor, "is", 2, object_is);
    define_global(vm, "Object", JsValue::from_object(object_ctor));

    // Array.prototype methods + Array constructor
    let arr_proto = vm.array_proto;
    define_method_len(vm, arr_proto, "push", 1, array_push);
    define_method_len(vm, arr_proto, "pop", 0, array_pop);
    define_method_len(vm, arr_proto, "map", 1, array_map);
    define_method_len(vm, arr_proto, "filter", 1, array_filter);
    define_method_len(vm, arr_proto, "reduce", 1, array_reduce);
    define_method_len(vm, arr_proto, "forEach", 1, array_for_each);
    define_method_len(vm, arr_proto, "join", 1, array_join);
    define_method_len(vm, arr_proto, "slice", 2, array_slice);
    define_method_len(vm, arr_proto, "indexOf", 1, array_index_of);
    define_method_len(vm, arr_proto, "includes", 1, array_includes);
    define_method_len(vm, arr_proto, "toString", 0, array_to_string);
    let array_values_fn = vm.alloc_native_fn("values", array_values);
    let k_values = vm.heap.intern_key("values");
    vm.heap.object_define_data(
        arr_proto,
        k_values,
        JsValue::from_object(array_values_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let sym_iter_key = vm.key_symbol_iterator;
    vm.heap.object_define_data(
        arr_proto,
        sym_iter_key,
        JsValue::from_object(array_values_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.default_array_iter_fn = Some(array_values_fn);
    define_method_len(vm, arr_proto, "keys", 0, array_keys);
    define_method_len(vm, arr_proto, "entries", 0, array_entries);
    // Array.prototype[Symbol.unscopables] (ECMA-262 §23.1.3.37)
    let unscopables = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, None));
    for name in [
        "at",
        "copyWithin",
        "entries",
        "fill",
        "find",
        "findIndex",
        "findLast",
        "findLastIndex",
        "flat",
        "flatMap",
        "includes",
        "keys",
        "values",
    ] {
        let k = vm.heap.intern_key(name);
        vm.heap.object_define_data(
            unscopables,
            k,
            JsValue::TRUE,
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
    }
    let k_unscop = vm.key_symbol_unscopables;
    vm.heap.object_define_data(
        arr_proto,
        k_unscop,
        JsValue::from_object(unscopables),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    let array_ctor = vm.alloc_native_fn("Array", array_ctor_fn);
    vm.mark_constructor(array_ctor);
    vm.set_fn_length(array_ctor, 1);
    let species_getter =
        vm.alloc_native_fn("get [Symbol.species]", |_vm, this_val, _| Ok(this_val));
    let k_species = vm.key_symbol_species;
    vm.heap.object_define_accessor(
        array_ctor,
        k_species,
        Some(species_getter),
        None,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        array_ctor,
        vm.key_prototype,
        JsValue::from_object(arr_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        arr_proto,
        vm.key_constructor,
        JsValue::from_object(array_ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    define_method_len(vm, array_ctor, "isArray", 1, array_is_array);
    define_global(vm, "Array", JsValue::from_object(array_ctor));

    // Proxy & Reflect
    let proxy_ctor = vm.alloc_native_fn("Proxy", proxy_ctor_fn);
    vm.mark_constructor(proxy_ctor);
    vm.set_fn_length(proxy_ctor, 2);
    define_method_len(vm, proxy_ctor, "revocable", 2, proxy_revocable);
    define_global(vm, "Proxy", JsValue::from_object(proxy_ctor));

    let reflect = vm.alloc_ordinary_object();
    define_method_len(vm, reflect, "get", 2, reflect_get);
    define_method_len(vm, reflect, "set", 3, reflect_set);
    define_method_len(vm, reflect, "has", 2, reflect_has);
    define_method_len(vm, reflect, "deleteProperty", 2, reflect_delete_property);
    define_method_len(vm, reflect, "apply", 3, reflect_apply);
    define_method_len(vm, reflect, "construct", 2, reflect_construct);
    define_method_len(vm, reflect, "getPrototypeOf", 1, reflect_get_prototype_of);
    define_method_len(vm, reflect, "setPrototypeOf", 2, reflect_set_prototype_of);
    define_method_len(vm, reflect, "ownKeys", 1, reflect_own_keys);
    define_method_len(vm, reflect, "isExtensible", 1, reflect_is_extensible);
    define_method_len(
        vm,
        reflect,
        "preventExtensions",
        1,
        reflect_prevent_extensions,
    );
    let reflect_tag = JsValue::from_string(vm.heap.alloc_string("Reflect"));
    let k_tag = vm.key_symbol_to_string_tag;
    vm.heap.object_define_data(
        reflect,
        k_tag,
        reflect_tag,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    define_global(vm, "Reflect", JsValue::from_object(reflect));

    // Error & NativeError constructors with distinct prototypes (ECMA-262 §20.5)
    define_method_len(vm, vm.error_proto, "toString", 0, error_proto_to_string);
    let stack_get = vm.alloc_native_fn("get stack", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Error.prototype.stack getter requires an object receiver"));
        };
        let is_err = vm
            .heap
            .get_object(h)
            .and_then(|o| o.rare.as_ref())
            .is_some_and(|r| r.is_error_data);
        if !is_err {
            return Ok(JsValue::UNDEFINED);
        }
        let name = vm
            .get_property_by_key(h, vm.key_name)
            .ok()
            .filter(|v| !v.is_undefined())
            .map(|v| vm.value_to_rust_string(v))
            .unwrap_or_else(|| "Error".to_string());
        let msg = vm
            .get_property_by_key(h, vm.key_message)
            .ok()
            .filter(|v| !v.is_undefined())
            .map(|v| vm.value_to_rust_string(v))
            .unwrap_or_default();
        let line = if msg.is_empty() {
            name
        } else {
            format!("{name}: {msg}")
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(line)))
    });
    vm.set_fn_length(stack_get, 0);
    let stack_set = vm.alloc_native_fn("set stack", |vm, this_val, args| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Error.prototype.stack setter requires an object receiver"));
        };
        let val = args.first().copied().unwrap_or(JsValue::UNDEFINED);
        if !val.is_string() {
            return Err(vm.type_error("Error.prototype.stack value must be a string"));
        }
        if h == vm.error_proto {
            return Err(vm.type_error("Cannot set stack directly on Error.prototype"));
        }
        let k_stack = vm.heap.intern_key("stack");
        if let Some(own) = vm.heap.object_get_own(h, k_stack, &vm.shapes) {
            if own.attrs.contains(PropertyAttrs::ACCESSOR) {
                if let Some(sh) = own.setter {
                    vm.call_value(JsValue::from_object(sh), this_val, &[val])?;
                } else {
                    return Err(vm.type_error("Cannot set property which has only a getter"));
                }
            } else if !own.attrs.contains(PropertyAttrs::WRITABLE) {
                return Err(vm.type_error("Cannot assign to read-only property 'stack'"));
            } else {
                vm.set_property_by_key(h, k_stack, val)?;
            }
        } else if !vm.heap.get_object(h).is_some_and(|o| o.extensible) {
            return Err(vm.type_error("Cannot define property 'stack' on non-extensible object"));
        } else {
            vm.heap
                .object_define_data(h, k_stack, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
            vm.heap.write_barrier(h, val);
        }
        Ok(JsValue::UNDEFINED)
    });
    vm.set_fn_length(stack_set, 1);
    let k_stack = vm.heap.intern_key("stack");
    vm.heap.object_define_accessor(
        vm.error_proto,
        k_stack,
        Some(stack_get),
        Some(stack_set),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    let mut error_root_ctor = None;
    for (err_name, proto) in [
        ("Error", vm.error_proto),
        ("TypeError", vm.type_error_proto),
        ("ReferenceError", vm.reference_error_proto),
        ("RangeError", vm.range_error_proto),
        ("SyntaxError", vm.syntax_error_proto),
        ("URIError", vm.uri_error_proto),
        ("EvalError", vm.eval_error_proto),
    ] {
        let ctor = vm.alloc_host_fn(err_name, move |vm, _this, args| {
            let obj_proto = if let Some(nth) = vm.native_new_target.as_object() {
                vm.get_property_by_key(nth, vm.key_prototype)?
                    .as_object()
                    .unwrap_or(proto)
            } else {
                proto
            };
            let err_obj = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(obj_proto)));
            if let Some(o) = vm.heap.get_object_mut(err_obj) {
                o.rare_mut().is_error_data = true;
            }
            if let Some(&msg_arg) = args.first().filter(|v| !v.is_undefined()) {
                let msg = vm.to_js_string(msg_arg)?;
                let mv = JsValue::from_string(vm.heap.alloc_string(msg));
                vm.heap.object_define_data(
                    err_obj,
                    vm.key_message,
                    mv,
                    PropertyAttrs::METHOD,
                    &mut vm.shapes,
                );
            }
            if let Some(opts) = args.get(1).and_then(|v| v.as_object()) {
                let k_cause = vm.heap.intern_key("cause");
                if vm.has_property_by_key(opts, k_cause)? {
                    let cause = vm.get_property_by_key(opts, k_cause)?;
                    vm.heap.object_define_data(
                        err_obj,
                        k_cause,
                        cause,
                        PropertyAttrs::METHOD,
                        &mut vm.shapes,
                    );
                }
            }
            Ok(JsValue::from_object(err_obj))
        });
        vm.mark_constructor(ctor);
        vm.set_fn_length(ctor, 1);
        if let Some(root_ctor) = error_root_ctor {
            vm.heap
                .object_set_prototype(ctor, Some(root_ctor), &mut vm.shapes);
        } else {
            define_method_len(vm, ctor, "isError", 1, |vm, _this, args| {
                let is_err = args
                    .first()
                    .and_then(|v| v.as_object())
                    .and_then(|h| vm.heap.get_object(h))
                    .and_then(|o| o.rare.as_ref())
                    .is_some_and(|r| r.is_error_data);
                Ok(JsValue::from_bool(is_err))
            });
            error_root_ctor = Some(ctor);
        }
        let pn = JsValue::from_string(vm.heap.alloc_string(err_name));
        let pm = JsValue::from_string(vm.heap.alloc_string(""));
        vm.heap.object_define_data(
            proto,
            vm.key_name,
            pn,
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            proto,
            vm.key_message,
            pm,
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            proto,
            vm.key_constructor,
            JsValue::from_object(ctor),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            ctor,
            vm.key_prototype,
            JsValue::from_object(proto),
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
        define_global(vm, err_name, JsValue::from_object(ctor));
    }
}

pub fn define_method_len(vm: &mut JsVm, target: HeapHandle, name: &str, len: u32, f: NativeFnPtr) {
    let fn_h = vm.alloc_native_fn(name, f);
    vm.set_fn_length(fn_h, len);
    let key = vm.heap.intern_key(name);
    vm.heap.object_define_data(
        target,
        key,
        JsValue::from_object(fn_h),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
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
// Global eval & Function constructor
// ---------------------------------------------------------------------------

fn eval_error_to_exception(vm: &mut JsVm, err: crate::vm::EvalError) -> JsException {
    match err {
        crate::vm::EvalError::Compile(ce) => vm.make_error("SyntaxError", ce.to_string()),
        crate::vm::EvalError::Runtime(exc) => exc,
    }
}

fn global_eval_fn(
    vm: &mut JsVm,
    _this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let first = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let Some(str_h) = first.as_string() else {
        // ECMA-262 §19.2.1.1 step 2: if x is not a String, return x unchanged.
        return Ok(first);
    };
    let src = vm.heap.materialize_string(str_h).unwrap_or_default();
    let prog = crate::parser::parse(&src, crate::ast::SourceType::Script)
        .map_err(|e| vm.make_error("SyntaxError", e.to_string()))?;
    let mode = if prog.strict {
        crate::compiler::EvalScopeMode::LocalAll
    } else {
        crate::compiler::EvalScopeMode::GlobalVars
    };
    let compiled = crate::compiler::compile_eval_program(&prog, mode)
        .map_err(|e| vm.make_error("SyntaxError", e.to_string()))?;
    vm.execute_program(compiled)
}

fn function_ctor_fn(
    vm: &mut JsVm,
    _this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let (params_str, body_str) = if args.is_empty() {
        (String::new(), String::new())
    } else if args.len() == 1 {
        (String::new(), vm.to_js_string(args[0])?)
    } else {
        let mut params = Vec::with_capacity(args.len() - 1);
        for &a in &args[..args.len() - 1] {
            params.push(vm.to_js_string(a)?);
        }
        let body = vm.to_js_string(args[args.len() - 1])?;
        (params.join(","), body)
    };
    // Validate parameter list independently so syntax like `Function("a) { } (", "")` cannot escape.
    let param_check = format!("(function anonymous({params_str}\n) {{\n}})");
    if let Err(e) = crate::parser::parse(&param_check, crate::ast::SourceType::Script) {
        return Err(vm.make_error("SyntaxError", format!("Invalid Function parameters: {e}")));
    }
    let full_src = format!("(function anonymous({params_str}\n) {{\n{body_str}\n}})");
    vm.eval_script(&full_src)
        .map_err(|e| eval_error_to_exception(vm, e))
}

// ---------------------------------------------------------------------------
// Function.prototype
// ---------------------------------------------------------------------------

fn fn_call(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let bound_this = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let rest = if args.len() > 1 { &args[1..] } else { &[] };
    vm.call_value(this_val, bound_this, rest)
}

fn create_list_from_array_like(
    vm: &mut JsVm,
    arg_array: JsValue,
) -> Result<Vec<JsValue>, JsException> {
    let Some(arr_h) = arg_array.as_object() else {
        return Err(vm.type_error("CreateListFromArrayLike called on non-object"));
    };
    let is_dense = matches!(
        vm.heap.get_object(arr_h).map(|o| &o.kind),
        Some(ObjectKind::Array)
    );
    let len: u32 = if is_dense {
        vm.heap.get_object(arr_h).map_or(0, |o| o.elements.len())
    } else {
        let lv = vm.get_property_by_key(arr_h, vm.key_length)?;
        let n = vm.to_number(lv)?;
        if !n.is_finite() || n <= 0.0 {
            0
        } else if n > 1_000_000.0 {
            return Err(vm.range_error("Maximum call stack size exceeded"));
        } else {
            n.trunc() as u32
        }
    };
    if len > 1_000_000 {
        return Err(vm.range_error("Maximum call stack size exceeded"));
    }
    let mut call_args = Vec::with_capacity(len as usize);
    for i in 0..len {
        let v = if is_dense {
            vm.heap
                .get_object(arr_h)
                .and_then(|o| o.elements.get(i))
                .unwrap_or(JsValue::UNDEFINED)
        } else {
            vm.get_elem(arg_array, JsValue::from_i32(i as i32))?
        };
        call_args.push(v);
    }
    Ok(call_args)
}

fn fn_apply(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let bound_this = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let arg_array = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let call_args = if arg_array.is_undefined() || arg_array.is_null() {
        Vec::new()
    } else {
        create_list_from_array_like(vm, arg_array)?
    };
    vm.call_value(this_val, bound_this, &call_args)
}

fn fn_bind(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(target) = this_val.as_object().filter(|&h| vm.is_callable_handle(h)) else {
        return Err(vm.type_error("Function.prototype.bind called on non-function"));
    };
    let bound_this = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let bound_args = if args.len() > 1 {
        args[1..].to_vec()
    } else {
        Vec::new()
    };
    let target_proto = vm
        .heap
        .get_object(target)
        .and_then(|o| o.prototype)
        .or(Some(vm.function_proto));
    let bound = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::BoundFunction(Box::new(BoundFunctionObject {
            target,
            bound_this,
            bound_args,
        })),
        target_proto,
    ));
    if vm.is_constructor_handle(target) {
        vm.mark_constructor(bound);
    }
    let has_own_len = vm
        .heap
        .object_get_own(target, vm.key_length, &vm.shapes)
        .is_some();
    let new_len = if has_own_len {
        let lv = vm.get_property_by_key(target, vm.key_length)?;
        if let Some(n) = lv.as_number() {
            if n.is_nan() || n == f64::NEG_INFINITY {
                0.0
            } else if n == f64::INFINITY {
                f64::INFINITY
            } else {
                (n.trunc() - args.len().saturating_sub(1) as f64).max(0.0)
            }
        } else {
            0.0
        }
    } else {
        0.0
    };
    vm.heap.object_define_data(
        bound,
        vm.key_length,
        JsValue::from_f64(new_len),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    let target_name = vm
        .get_property_by_key(target, vm.key_name)?
        .as_string()
        .and_then(|sh| vm.heap.materialize_string(sh))
        .unwrap_or_default();
    let bound_name = JsValue::from_string(vm.heap.alloc_string(format!("bound {target_name}")));
    vm.heap.object_define_data(
        bound,
        vm.key_name,
        bound_name,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    Ok(JsValue::from_object(bound))
}

fn fn_has_instance(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let v = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    Ok(JsValue::from_bool(vm.ordinary_has_instance(this_val, v)?))
}

fn is_valid_native_fn_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let ok_start = first == '_' || first == '$' || first.is_alphabetic();
    ok_start && chars.all(|c| c == '_' || c == '$' || c.is_alphanumeric())
}

fn fn_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Function.prototype.toString requires a Function"));
    };
    if !vm.is_callable_handle(h) {
        return Err(vm.type_error("Function.prototype.toString requires a Function"));
    }
    let name = vm
        .heap
        .object_get_own(h, vm.key_name, &vm.shapes)
        .and_then(|p| p.value.as_string())
        .and_then(|sh| vm.heap.materialize_string(sh))
        .unwrap_or_default();
    let s = if is_valid_native_fn_ident(&name) {
        format!("function {name}() {{ [native code] }}")
    } else {
        "function () { [native code] }".to_owned()
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn error_proto_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Error.prototype.toString called on non-object"));
    };
    let name_val = vm.get_property_by_key(h, vm.key_name)?;
    let name = if name_val.is_undefined() {
        "Error".to_owned()
    } else {
        vm.value_to_rust_string(name_val)
    };
    let msg_val = vm.get_property_by_key(h, vm.key_message)?;
    let msg = if msg_val.is_undefined() {
        String::new()
    } else {
        vm.value_to_rust_string(msg_val)
    };
    let out = if name.is_empty() {
        msg
    } else if msg.is_empty() {
        name
    } else {
        format!("{name}: {msg}")
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

fn obj_value_of(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_object(vm.to_object(this_val)?))
}

fn obj_is_prototype_of(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(mut cur) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::FALSE);
    };
    let target = vm.to_object(this_val)?;
    for _ in 0..1024 {
        let Some(proto) = crate::proxy::mop_get_prototype_of(vm, cur)? else {
            return Ok(JsValue::FALSE);
        };
        if proto == target {
            return Ok(JsValue::TRUE);
        }
        cur = proto;
    }
    Ok(JsValue::FALSE)
}

fn obj_property_is_enumerable(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let prop = vm.to_property_key(args.first().copied().unwrap_or(JsValue::UNDEFINED))?;
    let h = vm.to_object(this_val)?;
    let key = vm.heap.intern_key(&prop);
    let desc = crate::proxy::mop_get_own_property(vm, h, key)?;
    Ok(JsValue::from_bool(
        desc.is_some_and(|d| d.enumerable == Some(true)),
    ))
}

// ---------------------------------------------------------------------------
// Object
// ---------------------------------------------------------------------------

fn object_ctor_fn(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let first = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    if first.is_null() || first.is_undefined() {
        return Ok(JsValue::from_object(vm.alloc_ordinary_object()));
    }
    Ok(JsValue::from_object(vm.to_object(first)?))
}

fn obj_has_own_property(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let ks = vm.to_property_key(args.first().copied().unwrap_or(JsValue::UNDEFINED))?;
    let h = vm.to_object(this_val)?;
    let key = vm.heap.intern_key(&ks);
    let has = crate::proxy::mop_get_own_property(vm, h, key)?.is_some();
    Ok(JsValue::from_bool(has))
}

fn obj_to_locale_string(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let k_to_str = vm.heap.intern_key("toString");
    let to_str = vm.get_property_by_key(h, k_to_str)?;
    vm.call_value(to_str, this_val, &[])
}

fn obj_get_proto_accessor(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    Ok(match crate::proxy::mop_get_prototype_of(vm, h)? {
        Some(p) => JsValue::from_object(p),
        None => JsValue::NULL,
    })
}

fn obj_set_proto_accessor(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    if this_val.is_null() || this_val.is_undefined() {
        return Err(vm.type_error("Cannot convert undefined or null to object"));
    }
    let proto_val = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    if !proto_val.is_object() && !proto_val.is_null() {
        return Ok(JsValue::UNDEFINED);
    }
    let Some(h) = this_val.as_object() else {
        return Ok(JsValue::UNDEFINED);
    };
    let new_proto = proto_val.as_object();
    if !crate::proxy::mop_set_prototype_of(vm, h, new_proto)? {
        return Err(vm.type_error("Cyclic __proto__ value or non-extensible object"));
    }
    Ok(JsValue::UNDEFINED)
}

fn obj_define_accessor_legacy(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
    is_getter: bool,
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let fn_val = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let Some(fh) = fn_val.as_object().filter(|&f| vm.is_callable_handle(f)) else {
        return Err(vm.type_error(if is_getter {
            "__defineGetter__: getter must be a function"
        } else {
            "__defineSetter__: setter must be a function"
        }));
    };
    let key_s = vm.to_property_key(args.first().copied().unwrap_or(JsValue::UNDEFINED))?;
    let key = vm.heap.intern_key(&key_s);
    let existing = vm.heap.object_get_own(h, key, &vm.shapes);
    if let Some(cur) = &existing {
        if !cur.attrs.contains(PropertyAttrs::CONFIGURABLE) {
            return Err(vm.type_error("Cannot redefine non-configurable property"));
        }
    } else if !vm.heap.get_object(h).is_some_and(|o| o.extensible) {
        return Err(vm.type_error("Cannot define property on non-extensible object"));
    }
    let (g, s) = if is_getter {
        (Some(fh), existing.and_then(|p| p.setter))
    } else {
        (existing.and_then(|p| p.getter), Some(fh))
    };
    vm.heap.object_define_accessor(
        h,
        key,
        g,
        s,
        PropertyAttrs::ENUMERABLE | PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    Ok(JsValue::UNDEFINED)
}

fn obj_define_getter(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    obj_define_accessor_legacy(vm, this_val, args, true)
}

fn obj_define_setter(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    obj_define_accessor_legacy(vm, this_val, args, false)
}

fn obj_lookup_accessor_legacy(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
    is_getter: bool,
) -> Result<JsValue, JsException> {
    let mut cur = Some(vm.to_object(this_val)?);
    let key_s = vm.to_property_key(args.first().copied().unwrap_or(JsValue::UNDEFINED))?;
    let key = vm.heap.intern_key(&key_s);
    for _ in 0..64 {
        let Some(h) = cur else { break };
        if let Some(prop) = vm.heap.object_get_own(h, key, &vm.shapes) {
            if prop.attrs.contains(PropertyAttrs::ACCESSOR) {
                let acc = if is_getter { prop.getter } else { prop.setter };
                return Ok(acc.map_or(JsValue::UNDEFINED, JsValue::from_object));
            }
            return Ok(JsValue::UNDEFINED);
        }
        cur = vm.heap.get_object(h).and_then(|o| o.prototype);
    }
    Ok(JsValue::UNDEFINED)
}

fn obj_lookup_getter(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    obj_lookup_accessor_legacy(vm, this_val, args, true)
}

fn obj_lookup_setter(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    obj_lookup_accessor_legacy(vm, this_val, args, false)
}

fn obj_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let builtin_tag = if this_val.is_undefined() {
        return Ok(JsValue::from_string(
            vm.heap.alloc_string("[object Undefined]"),
        ));
    } else if this_val.is_null() {
        return Ok(JsValue::from_string(vm.heap.alloc_string("[object Null]")));
    } else if this_val.is_bool() {
        "Boolean"
    } else if this_val.is_number() {
        "Number"
    } else if this_val.is_string() {
        "String"
    } else if this_val.is_bigint() {
        "BigInt"
    } else if this_val.is_symbol() {
        "Symbol"
    } else if let Some(h) = this_val.as_object() {
        // Step 6: IsArray(O) MUST run before @@toStringTag (throws on revoked Proxy).
        let is_arr = vm
            .is_array_value(this_val)
            .map_err(|_| vm.type_error("Cannot perform 'IsArray' on a revoked Proxy"))?;
        // Check `Symbol.toStringTag` on objects (ECMA-262 §20.1.3.6).
        let tag_key = vm.key_symbol_to_string_tag;
        let tag_val = vm.get_property_by_key(h, tag_key)?;
        if let Some(sh) = tag_val.as_string() {
            let s = vm.heap.materialize_string(sh).unwrap_or_default();
            return Ok(JsValue::from_string(
                vm.heap.alloc_string(format!("[object {s}]")),
            ));
        }
        if is_arr {
            "Array"
        } else if let Some(bv) = vm
            .heap
            .get_object(h)
            .and_then(|o| o.rare.as_ref())
            .and_then(|r| r.boxed_value)
        {
            if bv.is_string() {
                "String"
            } else if bv.is_number() {
                "Number"
            } else if bv.is_bool() {
                "Boolean"
            } else if bv.is_bigint() {
                "BigInt"
            } else if bv.is_symbol() {
                "Symbol"
            } else {
                "Object"
            }
        } else if vm
            .heap
            .get_object(h)
            .and_then(|o| o.rare.as_ref())
            .is_some_and(|r| r.is_error_data)
        {
            "Error"
        } else if vm.is_callable_handle(h) {
            "Function"
        } else {
            match vm.heap.get_object(h).map(|o| &o.kind) {
                Some(ObjectKind::RegExp(_)) => "RegExp",
                Some(ObjectKind::Date(_)) => "Date",
                Some(ObjectKind::Map(_)) => "Map",
                Some(ObjectKind::Set(_)) => "Set",
                Some(ObjectKind::Promise(_)) => "Promise",
                Some(ObjectKind::ArrayBuffer(_)) => "ArrayBuffer",
                Some(ObjectKind::DataView(_)) => "DataView",
                Some(ObjectKind::TypedArray(ta)) => ta.kind.name(),
                _ => "Object",
            }
        }
    } else {
        "Object"
    };
    Ok(JsValue::from_string(
        vm.heap.alloc_string(format!("[object {builtin_tag}]")),
    ))
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
    if let Some(&props_arg) = args.get(1) {
        if !props_arg.is_undefined() {
            crate::builtins_primitives::define_properties_on_object(vm, obj, props_arg)?;
        }
    }
    Ok(JsValue::from_object(obj))
}

fn object_keys(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let h = vm.to_object(args.first().copied().unwrap_or(JsValue::UNDEFINED))?;
    let arr = vm.alloc_array();
    let own = crate::proxy::mop_own_property_keys(vm, h)?;
    for (kv, pk) in own {
        if kv.is_string()
            && crate::proxy::mop_get_own_property(vm, h, pk)?
                .is_some_and(|d| d.enumerable == Some(true))
        {
            vm.heap.object_push_element(arr, kv);
        }
    }
    Ok(JsValue::from_object(arr))
}

fn object_values(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let h = vm.to_object(args.first().copied().unwrap_or(JsValue::UNDEFINED))?;
    let arr = vm.alloc_array();
    let own = crate::proxy::mop_own_property_keys(vm, h)?;
    for (kv, pk) in own {
        if kv.is_string()
            && crate::proxy::mop_get_own_property(vm, h, pk)?
                .is_some_and(|d| d.enumerable == Some(true))
        {
            let val = vm.get_property_by_key(h, pk)?;
            vm.heap.object_push_element(arr, val);
        }
    }
    Ok(JsValue::from_object(arr))
}

fn object_entries(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let h = vm.to_object(args.first().copied().unwrap_or(JsValue::UNDEFINED))?;
    let arr = vm.alloc_array();
    let own = crate::proxy::mop_own_property_keys(vm, h)?;
    for (kv, pk) in own {
        if kv.is_string()
            && crate::proxy::mop_get_own_property(vm, h, pk)?
                .is_some_and(|d| d.enumerable == Some(true))
        {
            let val = vm.get_property_by_key(h, pk)?;
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
    let target = vm.to_object(target_val)?;
    for &src_val in args.iter().skip(1) {
        if src_val.is_null() || src_val.is_undefined() {
            continue;
        }
        let src = vm.to_object(src_val)?;
        let own = crate::proxy::mop_own_property_keys(vm, src)?;
        for (kv, pk) in own {
            if crate::proxy::mop_get_own_property(vm, src, pk)?
                .is_some_and(|d| d.enumerable == Some(true))
            {
                let v = vm.get_property_by_key(src, pk)?;
                let ok = if kv.is_symbol() {
                    vm.set_property_by_key(target, pk, v)?
                } else {
                    vm.set_elem_checked(JsValue::from_object(target), kv, v)?
                };
                if !ok {
                    return Err(vm.type_error("Cannot assign to read only property"));
                }
            }
        }
    }
    Ok(JsValue::from_object(target))
}

fn object_get_prototype_of(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(args.first().copied().unwrap_or(JsValue::UNDEFINED))?;
    let proto = crate::proxy::mop_get_prototype_of(vm, h)?;
    Ok(proto.map_or(JsValue::NULL, JsValue::from_object))
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
    if !crate::proxy::mop_set_prototype_of(vm, h, proto)? {
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
        if !crate::builtins_primitives::set_object_integrity_level(vm, h, true)? {
            return Err(vm.type_error("Cannot freeze object"));
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
        let n = vm.to_number(args[0])?;
        let len = if n.is_finite() && n >= 0.0 && n <= f64::from(u32::MAX) && n.trunc() == n {
            n as u32
        } else {
            return Err(vm.range_error("Invalid array length"));
        };
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
    let is_arr = vm
        .is_array_value(args.first().copied().unwrap_or(JsValue::UNDEFINED))
        .map_err(|_| vm.type_error("Cannot perform 'IsArray' on a revoked Proxy"))?;
    Ok(JsValue::from_bool(is_arr))
}

fn array_push(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let mut len = vm.array_get_len(h)?;
    if len + (args.len() as u64) > 9_007_199_254_740_991 {
        return Err(vm.type_error("Array length exceeds 2^53 - 1"));
    }
    for &a in args {
        vm.array_set_index_strict(h, len, a)?;
        len += 1;
    }
    vm.array_set_length(h, len)?;
    Ok(JsValue::from_f64(len as f64))
}

fn array_pop(vm: &mut JsVm, this_val: JsValue, _args: &[JsValue]) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    if len == 0 {
        vm.array_set_length(h, 0)?;
        return Ok(JsValue::UNDEFINED);
    }
    let new_len = len - 1;
    let val = vm.array_get_index(h, new_len)?;
    vm.array_delete_index_strict(h, new_len)?;
    vm.array_set_length(h, new_len)?;
    Ok(val)
}

fn require_callable(vm: &mut JsVm, cb: JsValue) -> Result<(), JsException> {
    if cb.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
        Ok(())
    } else {
        Err(vm.type_error("Callback is not a function"))
    }
}

fn for_each_present_idx(
    vm: &mut JsVm,
    h: HeapHandle,
    len: u64,
    mut f: impl FnMut(&mut JsVm, u64, JsValue) -> Result<(), JsException>,
) -> Result<(), JsException> {
    let sparse = vm.sparse_present_indices_if_safe(h, len);
    let count = sparse.as_ref().map_or(len, |v| v.len() as u64);
    let limit = vm.max_loop_iterations.unwrap_or(1_000_000).min(1_000_000);
    for step in 0..count {
        if step >= limit {
            return Err(vm.range_error("Script execution iteration limit exceeded"));
        }
        let i = sparse.as_ref().map_or(step, |v| v[step as usize]);
        if vm.array_has_index(h, i)? {
            let elem = vm.array_get_index(h, i)?;
            f(vm, i, elem)?;
        }
    }
    Ok(())
}

fn array_map(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let cb = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    require_callable(vm, cb)?;
    let this_arg = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let mark = vm.temp_roots.len();
    let res = (|| {
        vm.temp_roots.push(JsValue::from_object(h));
        let out = vm.array_species_create(h, len)?;
        vm.temp_roots.push(JsValue::from_object(out));
        let obj_val = JsValue::from_object(h);
        for_each_present_idx(vm, h, len, |vm, i, elem| {
            let mapped =
                vm.call_value(cb, this_arg, &[elem, JsValue::from_f64(i as f64), obj_val])?;
            vm.array_create_data_prop(out, i, mapped)
        })?;
        Ok(JsValue::from_object(out))
    })();
    vm.temp_roots.truncate(mark);
    res
}

fn array_filter(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let cb = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    require_callable(vm, cb)?;
    let this_arg = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let mark = vm.temp_roots.len();
    let res = (|| {
        vm.temp_roots.push(JsValue::from_object(h));
        let out = vm.array_species_create(h, 0)?;
        vm.temp_roots.push(JsValue::from_object(out));
        let obj_val = JsValue::from_object(h);
        let mut to = 0u64;
        for_each_present_idx(vm, h, len, |vm, i, elem| {
            let keep =
                vm.call_value(cb, this_arg, &[elem, JsValue::from_f64(i as f64), obj_val])?;
            if vm.to_boolean(keep) {
                vm.array_create_data_prop(out, to, elem)?;
                to += 1;
            }
            Ok(())
        })?;
        Ok(JsValue::from_object(out))
    })();
    vm.temp_roots.truncate(mark);
    res
}

fn array_reduce(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let cb = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    require_callable(vm, cb)?;
    let obj_val = JsValue::from_object(h);
    let mut acc = if args.len() >= 2 { Some(args[1]) } else { None };
    for_each_present_idx(vm, h, len, |vm, i, elem| {
        acc = Some(if let Some(cur) = acc {
            vm.call_value(
                cb,
                JsValue::UNDEFINED,
                &[cur, elem, JsValue::from_f64(i as f64), obj_val],
            )?
        } else {
            elem
        });
        Ok(())
    })?;
    acc.ok_or_else(|| vm.type_error("Reduce of empty array with no initial value"))
}

fn array_for_each(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let cb = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    require_callable(vm, cb)?;
    let this_arg = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let obj_val = JsValue::from_object(h);
    for_each_present_idx(vm, h, len, |vm, i, elem| {
        vm.call_value(cb, this_arg, &[elem, JsValue::from_f64(i as f64), obj_val])?;
        Ok(())
    })?;
    Ok(JsValue::UNDEFINED)
}

fn array_join(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let sep = match args.first().copied() {
        Some(v) if !v.is_undefined() => vm.to_js_string(v)?,
        _ => ",".to_owned(),
    };
    if len == 0 {
        return Ok(JsValue::from_string(vm.heap.alloc_string("")));
    }
    if len > 4_000_000 {
        return Err(vm.range_error("Invalid string length"));
    }
    let mut parts = Vec::with_capacity((len as usize).min(16_384));
    for i in 0..len {
        let v = vm.array_get_index(h, i)?;
        if v.is_null() || v.is_undefined() {
            parts.push(String::new());
        } else {
            parts.push(vm.to_js_string(v)?);
        }
    }
    Ok(JsValue::from_string(vm.heap.alloc_string(parts.join(&sep))))
}

fn array_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let k_join = vm.heap.intern_key("join");
    let join_fn = vm.get_property_by_key(h, k_join)?;
    if join_fn
        .as_object()
        .is_some_and(|fh| vm.is_callable_handle(fh))
    {
        vm.call_value(join_fn, JsValue::from_object(h), &[])
    } else {
        obj_to_string(vm, JsValue::from_object(h), &[])
    }
}

fn clamp_rel_index(rel: f64, len: u64) -> u64 {
    if rel == f64::NEG_INFINITY {
        0
    } else if rel < 0.0 {
        (len as f64 + rel).max(0.0) as u64
    } else if rel >= len as f64 {
        len
    } else {
        rel as u64
    }
}

fn array_slice(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let rel_start = match args.first().copied() {
        Some(v) if !v.is_undefined() => vm.to_integer_or_infinity(v)?,
        _ => 0.0,
    };
    let start = clamp_rel_index(rel_start, len);
    let end = match args.get(1).copied() {
        Some(v) if !v.is_undefined() => clamp_rel_index(vm.to_integer_or_infinity(v)?, len),
        _ => len,
    };
    let count = end.saturating_sub(start);
    let out = vm.array_species_create(h, count)?;
    for_each_present_idx(vm, h, end, |vm, i, v| {
        if i >= start {
            vm.array_create_data_prop(out, i - start, v)?;
        }
        Ok(())
    })?;
    vm.array_set_length(out, count)?;
    Ok(JsValue::from_object(out))
}

fn array_index_of(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    if len == 0 {
        return Ok(JsValue::from_i32(-1));
    }
    let target = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let n = match args.get(1).copied() {
        Some(v) if !v.is_undefined() => vm.to_integer_or_infinity(v)?,
        _ => 0.0,
    };
    if n == f64::INFINITY || n >= len as f64 {
        return Ok(JsValue::from_i32(-1));
    }
    let start = clamp_rel_index(n, len);
    let sparse = vm.sparse_present_indices_if_safe(h, len);
    let total = sparse.as_ref().map_or(len - start, |v| v.len() as u64);
    let limit = vm.max_loop_iterations.unwrap_or(1_000_000).min(1_000_000);
    for step in 0..total {
        if step >= limit {
            return Err(vm.range_error("Script execution iteration limit exceeded"));
        }
        let i = sparse.as_ref().map_or(start + step, |v| v[step as usize]);
        if i >= start && vm.array_has_index(h, i)? {
            let v = vm.array_get_index(h, i)?;
            if vm.strict_eq(v, target) {
                return Ok(JsValue::from_f64(i as f64));
            }
        }
    }
    Ok(JsValue::from_i32(-1))
}

fn same_value_zero(vm: &JsVm, a: JsValue, b: JsValue) -> bool {
    if let (Some(na), Some(nb)) = (a.as_number(), b.as_number()) {
        (na.is_nan() && nb.is_nan()) || na == nb
    } else {
        vm.strict_eq(a, b)
    }
}

fn array_includes(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    if len == 0 {
        return Ok(JsValue::FALSE);
    }
    let target = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let n = match args.get(1).copied() {
        Some(v) if !v.is_undefined() => vm.to_integer_or_infinity(v)?,
        _ => 0.0,
    };
    if n == f64::INFINITY || n >= len as f64 {
        return Ok(JsValue::FALSE);
    }
    let start = clamp_rel_index(n, len);
    if !target.is_undefined() {
        if let Some(indices) = vm.sparse_present_indices_if_safe(h, len) {
            for i in indices {
                if i >= start {
                    let v = vm.array_get_index(h, i)?;
                    if same_value_zero(vm, v, target) {
                        return Ok(JsValue::TRUE);
                    }
                }
            }
            return Ok(JsValue::FALSE);
        }
    }
    let limit = vm.max_loop_iterations.unwrap_or(1_000_000).min(1_000_000);
    for i in start..len {
        if i - start >= limit {
            return Err(vm.range_error("Script execution iteration limit exceeded"));
        }
        let v = vm.array_get_index(h, i)?;
        if same_value_zero(vm, v, target) {
            return Ok(JsValue::TRUE);
        }
    }
    Ok(JsValue::FALSE)
}

#[derive(Clone, Copy)]
enum ArrayIterKind {
    Values,
    Keys,
    Entries,
}

fn alloc_array_iterator(
    vm: &mut JsVm,
    this_val: JsValue,
    kind: ArrayIterKind,
) -> Result<JsValue, JsException> {
    if this_val.is_null() || this_val.is_undefined() {
        return Err(vm.type_error("Cannot convert undefined or null to object"));
    }
    let Some(target) = this_val.as_object() else {
        return Err(vm.type_error("Array iterator requires an object"));
    };
    let iter_obj = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.array_iter_proto),
    ));
    let idx_cell = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let done_cell = std::rc::Rc::new(std::cell::Cell::new(false));
    let next_fn = vm.alloc_host_fn("next", move |vm, _this, _args| {
        if done_cell.get() {
            return Ok(crate::promise::make_iter_result(
                vm,
                JsValue::UNDEFINED,
                true,
            ));
        }
        let len: u32 = if matches!(
            vm.heap.get_object(target).map(|o| &o.kind),
            Some(ObjectKind::Array)
        ) {
            vm.heap.get_object(target).map_or(0, |o| o.elements.len())
        } else {
            let lv = vm.get_property_by_key(target, vm.key_length)?;
            let n = vm.to_number(lv)?;
            if !n.is_finite() || n <= 0.0 {
                0
            } else {
                n.trunc().min(f64::from(u32::MAX)) as u32
            }
        };
        let i = idx_cell.get();
        if i >= len {
            done_cell.set(true);
            return Ok(crate::promise::make_iter_result(
                vm,
                JsValue::UNDEFINED,
                true,
            ));
        }
        idx_cell.set(i + 1);
        let item = match kind {
            ArrayIterKind::Keys => JsValue::from_f64(f64::from(i)),
            ArrayIterKind::Values => vm.get_elem(this_val, JsValue::from_f64(f64::from(i)))?,
            ArrayIterKind::Entries => {
                let val = vm.get_elem(this_val, JsValue::from_f64(f64::from(i)))?;
                let pair = vm.alloc_array();
                vm.heap
                    .object_push_element(pair, JsValue::from_f64(f64::from(i)));
                vm.heap.object_push_element(pair, val);
                JsValue::from_object(pair)
            }
        };
        Ok(crate::promise::make_iter_result(vm, item, false))
    });
    vm.heap.object_define_data(
        iter_obj,
        vm.key_next,
        JsValue::from_object(next_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let self_iter = vm.alloc_native_fn("[Symbol.iterator]", |_vm, t, _| Ok(t));
    let sym_key = vm.key_symbol_iterator;
    vm.heap.object_define_data(
        iter_obj,
        sym_key,
        JsValue::from_object(self_iter),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    if let Some(o) = vm.heap.get_object_mut(iter_obj) {
        o.extra_refs.push(target);
    }
    Ok(JsValue::from_object(iter_obj))
}

fn array_values(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    alloc_array_iterator(vm, this_val, ArrayIterKind::Values)
}

fn array_keys(vm: &mut JsVm, this_val: JsValue, _args: &[JsValue]) -> Result<JsValue, JsException> {
    alloc_array_iterator(vm, this_val, ArrayIterKind::Keys)
}

fn array_entries(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    alloc_array_iterator(vm, this_val, ArrayIterKind::Entries)
}

// ---------------------------------------------------------------------------
// Proxy & Reflect
// ---------------------------------------------------------------------------

fn alloc_proxy_obj(vm: &mut JsVm, args: &[JsValue]) -> Result<HeapHandle, JsException> {
    let (Some(target), Some(handler)) = (
        args.first().and_then(|v| v.as_object()),
        args.get(1).and_then(|v| v.as_object()),
    ) else {
        return Err(vm.type_error("Cannot create proxy with a non-object as target or handler"));
    };
    Ok(vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(ProxyObject {
            target,
            handler,
            revoked: false,
        }),
        None,
    )))
}

fn proxy_ctor_fn(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    if vm.native_new_target.is_undefined() {
        return Err(vm.type_error("Constructor Proxy requires 'new'"));
    }
    let proxy = alloc_proxy_obj(vm, args)?;
    Ok(JsValue::from_object(proxy))
}

fn proxy_revocable(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let proxy_h = alloc_proxy_obj(vm, args)?;
    let proxy_val = JsValue::from_object(proxy_h);
    let revoke_fn = vm.alloc_host_fn("", move |vm, _this, _args| {
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
    let prop_str = vm.to_property_key(args.get(1).copied().unwrap_or(JsValue::UNDEFINED))?;
    let key = vm.heap.intern_key(&prop_str);
    let receiver = if args.len() >= 3 {
        args[2]
    } else {
        JsValue::from_object(target)
    };
    vm.get_property_with_receiver(target, key, receiver)
}

fn reflect_set(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(target) = args.first().and_then(|v| v.as_object()) else {
        return Err(vm.type_error("Reflect.set called on non-object"));
    };
    let prop_str = vm.to_property_key(args.get(1).copied().unwrap_or(JsValue::UNDEFINED))?;
    let key = vm.heap.intern_key(&prop_str);
    let val = args.get(2).copied().unwrap_or(JsValue::UNDEFINED);
    let receiver = if args.len() >= 4 {
        args[3]
    } else {
        JsValue::from_object(target)
    };
    let ok = vm.set_property_with_receiver(target, key, val, receiver)?;
    Ok(JsValue::from_bool(ok))
}

fn reflect_has(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(target) = args.first().and_then(|v| v.as_object()) else {
        return Err(vm.type_error("Reflect.has called on non-object"));
    };
    let prop_str = vm.to_property_key(args.get(1).copied().unwrap_or(JsValue::UNDEFINED))?;
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
    let prop_str = vm.to_property_key(args.get(1).copied().unwrap_or(JsValue::UNDEFINED))?;
    let key = vm.heap.intern_key(&prop_str);
    let del = vm.delete_property_by_key(target, key)?;
    Ok(JsValue::from_bool(del))
}

fn reflect_apply(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let target = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    if !target.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
        return Err(vm.type_error("Reflect.apply target is not callable"));
    }
    let this_arg = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let arg_list = args.get(2).copied().unwrap_or(JsValue::UNDEFINED);
    let call_args = create_list_from_array_like(vm, arg_list)?;
    vm.call_value(target, this_arg, &call_args)
}

fn reflect_construct(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let target = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let Some(target_h) = target.as_object() else {
        return Err(vm.type_error("Reflect.construct target is not a constructor"));
    };
    if !vm.is_constructor_handle(target_h) {
        return Err(vm.type_error("Reflect.construct target is not a constructor"));
    }
    let new_target = if args.len() >= 3 { args[2] } else { target };
    let Some(nt_h) = new_target.as_object() else {
        return Err(vm.type_error("Reflect.construct newTarget is not a constructor"));
    };
    if !vm.is_constructor_handle(nt_h) {
        return Err(vm.type_error("Reflect.construct newTarget is not a constructor"));
    }
    let arg_list = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let call_args = create_list_from_array_like(vm, arg_list)?;
    vm.construct_value(target, &call_args, new_target)
}

fn reflect_is_extensible(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = args.first().and_then(|v| v.as_object()) else {
        return Err(vm.type_error("Reflect.isExtensible called on non-object"));
    };
    Ok(JsValue::from_bool(crate::proxy::mop_is_extensible(vm, h)?))
}

fn reflect_prevent_extensions(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = args.first().and_then(|v| v.as_object()) else {
        return Err(vm.type_error("Reflect.preventExtensions called on non-object"));
    };
    Ok(JsValue::from_bool(crate::proxy::mop_prevent_extensions(
        vm, h,
    )?))
}

fn reflect_get_prototype_of(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = args.first().and_then(|v| v.as_object()) else {
        return Err(vm.type_error("Reflect.getPrototypeOf called on non-object"));
    };
    let proto = crate::proxy::mop_get_prototype_of(vm, h)?;
    Ok(proto.map_or(JsValue::NULL, JsValue::from_object))
}

fn reflect_set_prototype_of(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = args.first().and_then(|v| v.as_object()) else {
        return Err(vm.type_error("Reflect.setPrototypeOf called on non-object"));
    };
    let proto_arg = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let proto = if proto_arg.is_null() {
        None
    } else if let Some(p) = proto_arg.as_object() {
        Some(p)
    } else {
        return Err(vm.type_error("Object prototype may only be an Object or null"));
    };
    Ok(JsValue::from_bool(crate::proxy::mop_set_prototype_of(
        vm, h, proto,
    )?))
}

fn reflect_own_keys(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = args.first().and_then(|v| v.as_object()) else {
        return Err(vm.type_error("Reflect.ownKeys called on non-object"));
    };
    let keys = crate::proxy::mop_own_property_keys(vm, h)?;
    let arr = vm.alloc_array();
    for (kv, _) in keys {
        vm.heap.object_push_element(arr, kv);
    }
    Ok(JsValue::from_object(arr))
}
