//! ECMA-262 §10.5 Proxy Object Internal Methods and Invariant Enforcement.
//!
//! Provides unified MOP operations (`mop_*`) that dispatch seamlessly across
//! ordinary objects, exotic objects, and nested `Proxy` chains while checking
//! every §10.5 invariant and preserving Symbol property keys.

use std::collections::HashSet;

use crate::heap::{ObjectKind, ProxyObject};
use crate::shape::PropertyAttrs;
use crate::value::{HeapHandle, JsValue, SymbolId};
use crate::vm::{JsException, JsVm};
use crate::PropertyKey;

/// Full or partial ECMAScript Property Descriptor (ECMA-262 §6.2.5).
#[derive(Debug, Clone, Copy, Default)]
pub struct PropDesc {
    pub value: Option<JsValue>,
    pub writable: Option<bool>,
    pub get: Option<JsValue>,
    pub set: Option<JsValue>,
    pub enumerable: Option<bool>,
    pub configurable: Option<bool>,
}

impl PropDesc {
    pub fn is_accessor(&self) -> bool {
        self.get.is_some() || self.set.is_some()
    }

    pub fn is_data(&self) -> bool {
        self.value.is_some() || self.writable.is_some()
    }

    pub fn complete_into_full(mut self) -> Self {
        if !self.is_accessor() {
            if self.value.is_none() {
                self.value = Some(JsValue::UNDEFINED);
            }
            if self.writable.is_none() {
                self.writable = Some(false);
            }
        } else {
            if self.get.is_none() {
                self.get = Some(JsValue::UNDEFINED);
            }
            if self.set.is_none() {
                self.set = Some(JsValue::UNDEFINED);
            }
        }
        if self.enumerable.is_none() {
            self.enumerable = Some(false);
        }
        if self.configurable.is_none() {
            self.configurable = Some(false);
        }
        self
    }
}

/// Converts an interned [`PropertyKey`] back into its true JS String or Symbol [`JsValue`].
pub fn key_to_js_value(vm: &mut JsVm, key: PropertyKey) -> JsValue {
    if let Some(name) = vm.heap.resolve_key(key).map(str::to_string) {
        if let Some(id) = name
            .strip_prefix("Symbol(")
            .and_then(|s| s.strip_suffix(')'))
            .and_then(|n| n.parse::<u32>().ok())
        {
            return JsValue::from_symbol(SymbolId(id));
        }
        return JsValue::from_string(vm.heap.alloc_string(name));
    }
    JsValue::from_string(vm.heap.alloc_string(""))
}

/// ECMA-262 §7.2.10 `SameValue(x, y)`.
pub fn same_value(vm: &JsVm, x: JsValue, y: JsValue) -> bool {
    if let (Some(a), Some(b)) = (x.as_number(), y.as_number()) {
        if a.is_nan() && b.is_nan() {
            return true;
        }
        if a == 0.0 && b == 0.0 {
            return a.is_sign_positive() == b.is_sign_positive();
        }
        return a == b;
    }
    vm.strict_eq(x, y)
}

fn get_proxy(vm: &JsVm, obj: HeapHandle) -> Option<ProxyObject> {
    match vm.heap.get_object(obj).map(|o| &o.kind) {
        Some(ObjectKind::Proxy(p)) => Some(*p),
        _ => None,
    }
}

fn get_proxy_trap(
    vm: &mut JsVm,
    p: &ProxyObject,
    trap_name: &str,
) -> Result<Option<JsValue>, JsException> {
    if p.revoked {
        return Err(vm.type_error(format!("Cannot perform '{trap_name}' on a revoked Proxy")));
    }
    let k = vm.heap.intern_key(trap_name);
    let trap = vm.get_property_by_key(p.handler, k)?;
    if trap.is_undefined() || trap.is_null() {
        return Ok(None);
    }
    if !trap.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
        return Err(vm.type_error(format!("Proxy trap '{trap_name}' is not a function")));
    }
    Ok(Some(trap))
}

/// ECMA-262 §6.2.5.5 `ToPropertyDescriptor(Obj)`.
pub fn to_property_descriptor(vm: &mut JsVm, obj_val: JsValue) -> Result<PropDesc, JsException> {
    let Some(oh) = obj_val.as_object() else {
        return Err(vm.type_error("Property description must be an object"));
    };
    let mut desc = PropDesc::default();
    let k_enum = vm.heap.intern_key("enumerable");
    if mop_has_property(vm, oh, k_enum)? {
        let v = vm.get_property_by_key(oh, k_enum)?;
        desc.enumerable = Some(vm.to_boolean(v));
    }
    let k_conf = vm.heap.intern_key("configurable");
    if mop_has_property(vm, oh, k_conf)? {
        let v = vm.get_property_by_key(oh, k_conf)?;
        desc.configurable = Some(vm.to_boolean(v));
    }
    let k_val = vm.key_value;
    if mop_has_property(vm, oh, k_val)? {
        desc.value = Some(vm.get_property_by_key(oh, k_val)?);
    }
    let k_writ = vm.heap.intern_key("writable");
    if mop_has_property(vm, oh, k_writ)? {
        let v = vm.get_property_by_key(oh, k_writ)?;
        desc.writable = Some(vm.to_boolean(v));
    }
    let k_get = vm.key_get;
    if mop_has_property(vm, oh, k_get)? {
        let g = vm.get_property_by_key(oh, k_get)?;
        if !g.is_undefined() && !g.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
            return Err(vm.type_error("Getter must be a function or undefined"));
        }
        desc.get = Some(g);
    }
    let k_set = vm.key_set;
    if mop_has_property(vm, oh, k_set)? {
        let s = vm.get_property_by_key(oh, k_set)?;
        if !s.is_undefined() && !s.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
            return Err(vm.type_error("Setter must be a function or undefined"));
        }
        desc.set = Some(s);
    }
    if desc.is_accessor() && desc.is_data() {
        return Err(vm.type_error(
            "Invalid property descriptor: cannot both specify accessors and a value or writable attribute",
        ));
    }
    Ok(desc)
}

/// ECMA-262 §6.2.5.4 `FromPropertyDescriptor(Desc)`.
pub fn from_property_descriptor(vm: &mut JsVm, desc: Option<PropDesc>) -> JsValue {
    let Some(d) = desc else {
        return JsValue::UNDEFINED;
    };
    let obj = vm.alloc_ordinary_object();
    if let Some(v) = d.value {
        vm.heap
            .object_define_data(obj, vm.key_value, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }
    if let Some(w) = d.writable {
        let k = vm.heap.intern_key("writable");
        vm.heap.object_define_data(
            obj,
            k,
            JsValue::from_bool(w),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
    }
    if let Some(g) = d.get {
        vm.heap
            .object_define_data(obj, vm.key_get, g, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }
    if let Some(s) = d.set {
        vm.heap
            .object_define_data(obj, vm.key_set, s, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }
    if let Some(e) = d.enumerable {
        let k = vm.heap.intern_key("enumerable");
        vm.heap.object_define_data(
            obj,
            k,
            JsValue::from_bool(e),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
    }
    if let Some(c) = d.configurable {
        let k = vm.heap.intern_key("configurable");
        vm.heap.object_define_data(
            obj,
            k,
            JsValue::from_bool(c),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
    }
    JsValue::from_object(obj)
}

/// ECMA-262 §10.1.6.3 `ValidateAndApplyPropertyDescriptor` check-only (`O = undefined`).
pub fn is_compatible_property_descriptor(
    vm: &JsVm,
    extensible: bool,
    desc: PropDesc,
    current: Option<PropDesc>,
) -> bool {
    let Some(cur) = current else {
        return extensible;
    };
    if desc.value.is_none()
        && desc.writable.is_none()
        && desc.get.is_none()
        && desc.set.is_none()
        && desc.enumerable.is_none()
        && desc.configurable.is_none()
    {
        return true;
    }
    let cur_conf = cur.configurable.unwrap_or(false);
    let cur_enum = cur.enumerable.unwrap_or(false);
    if !cur_conf {
        if desc.configurable == Some(true) {
            return false;
        }
        if let Some(de) = desc.enumerable {
            if de != cur_enum {
                return false;
            }
        }
    }
    if !desc.is_data() && !desc.is_accessor() {
        return true;
    }
    if cur.is_data() != desc.is_data() {
        return cur_conf;
    }
    if cur.is_data() && desc.is_data() {
        if !cur_conf && cur.writable == Some(false) {
            if desc.writable == Some(true) {
                return false;
            }
            if let Some(dv) = desc.value {
                if !same_value(vm, dv, cur.value.unwrap_or(JsValue::UNDEFINED)) {
                    return false;
                }
            }
        }
        return true;
    }
    if !cur_conf {
        if let Some(ds) = desc.set {
            if !same_value(vm, ds, cur.set.unwrap_or(JsValue::UNDEFINED)) {
                return false;
            }
        }
        if let Some(dg) = desc.get {
            if !same_value(vm, dg, cur.get.unwrap_or(JsValue::UNDEFINED)) {
                return false;
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// 1. [[GetPrototypeOf]] (§10.5.1)
// ---------------------------------------------------------------------------

pub fn mop_get_prototype_of(
    vm: &mut JsVm,
    obj: HeapHandle,
) -> Result<Option<HeapHandle>, JsException> {
    if let Some(p) = get_proxy(vm, obj) {
        let Some(trap) = get_proxy_trap(vm, &p, "getPrototypeOf")? else {
            return mop_get_prototype_of(vm, p.target);
        };
        let res = vm.call_value(
            trap,
            JsValue::from_object(p.handler),
            &[JsValue::from_object(p.target)],
        )?;
        let handler_proto = if res.is_null() {
            None
        } else if let Some(ph) = res.as_object() {
            Some(ph)
        } else {
            return Err(
                vm.type_error("'getPrototypeOf' on proxy: trap returned neither object nor null")
            );
        };
        if mop_is_extensible(vm, p.target)? {
            return Ok(handler_proto);
        }
        let target_proto = mop_get_prototype_of(vm, p.target)?;
        if handler_proto != target_proto {
            return Err(vm.type_error(
                "'getPrototypeOf' on proxy: proxy target is non-extensible but trap did not return its actual prototype",
            ));
        }
        return Ok(handler_proto);
    }
    Ok(vm.heap.get_object(obj).and_then(|o| o.prototype))
}

// ---------------------------------------------------------------------------
// 2. [[SetPrototypeOf]] (§10.5.2)
// ---------------------------------------------------------------------------

pub fn mop_set_prototype_of(
    vm: &mut JsVm,
    obj: HeapHandle,
    proto: Option<HeapHandle>,
) -> Result<bool, JsException> {
    if let Some(p) = get_proxy(vm, obj) {
        let Some(trap) = get_proxy_trap(vm, &p, "setPrototypeOf")? else {
            return mop_set_prototype_of(vm, p.target, proto);
        };
        let pv = proto.map_or(JsValue::NULL, JsValue::from_object);
        let r = vm.call_value(
            trap,
            JsValue::from_object(p.handler),
            &[JsValue::from_object(p.target), pv],
        )?;
        if !vm.to_boolean(r) {
            return Ok(false);
        }
        if mop_is_extensible(vm, p.target)? {
            return Ok(true);
        }
        let target_proto = mop_get_prototype_of(vm, p.target)?;
        if proto != target_proto {
            return Err(vm.type_error(
                "'setPrototypeOf' on proxy: trap returned truish for setting a new prototype on the non-extensible proxy target",
            ));
        }
        return Ok(true);
    }
    let current = vm.heap.get_object(obj).and_then(|o| o.prototype);
    if current == proto {
        return Ok(true);
    }
    if !vm.heap.get_object(obj).is_some_and(|o| o.extensible) {
        return Ok(false);
    }
    // Cycle check walking through ordinary & proxy prototypes
    let mut cur = proto;
    for _ in 0..1024 {
        let Some(h) = cur else { break };
        if h == obj {
            return Ok(false);
        }
        if matches!(
            vm.heap.get_object(h).map(|o| &o.kind),
            Some(ObjectKind::Proxy(_))
        ) {
            break;
        }
        cur = vm.heap.get_object(h).and_then(|o| o.prototype);
    }
    vm.heap.object_set_prototype(obj, proto, &mut vm.shapes);
    vm.mega_ic.invalidate_prototypes();
    Ok(true)
}

// ---------------------------------------------------------------------------
// 3. [[IsExtensible]] (§10.5.3)
// ---------------------------------------------------------------------------

pub fn mop_is_extensible(vm: &mut JsVm, obj: HeapHandle) -> Result<bool, JsException> {
    if let Some(p) = get_proxy(vm, obj) {
        let Some(trap) = get_proxy_trap(vm, &p, "isExtensible")? else {
            return mop_is_extensible(vm, p.target);
        };
        let r = vm.call_value(
            trap,
            JsValue::from_object(p.handler),
            &[JsValue::from_object(p.target)],
        )?;
        let res = vm.to_boolean(r);
        let target_ext = mop_is_extensible(vm, p.target)?;
        if res != target_ext {
            return Err(vm.type_error(
                "'isExtensible' on proxy: trap result does not reflect extensibility of proxy target",
            ));
        }
        return Ok(res);
    }
    Ok(vm.heap.get_object(obj).is_some_and(|o| o.extensible))
}

// ---------------------------------------------------------------------------
// 4. [[PreventExtensions]] (§10.5.4)
// ---------------------------------------------------------------------------

pub fn mop_prevent_extensions(vm: &mut JsVm, obj: HeapHandle) -> Result<bool, JsException> {
    if let Some(p) = get_proxy(vm, obj) {
        let Some(trap) = get_proxy_trap(vm, &p, "preventExtensions")? else {
            return mop_prevent_extensions(vm, p.target);
        };
        let r = vm.call_value(
            trap,
            JsValue::from_object(p.handler),
            &[JsValue::from_object(p.target)],
        )?;
        let res = vm.to_boolean(r);
        if res && mop_is_extensible(vm, p.target)? {
            return Err(vm.type_error(
                "'preventExtensions' on proxy: trap returned truish but the proxy target is extensible",
            ));
        }
        return Ok(res);
    }
    if let Some(o) = vm.heap.get_object_mut(obj) {
        o.extensible = false;
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// 5. [[GetOwnProperty]] (§10.5.5)
// ---------------------------------------------------------------------------

pub fn mop_get_own_property(
    vm: &mut JsVm,
    obj: HeapHandle,
    key: PropertyKey,
) -> Result<Option<PropDesc>, JsException> {
    if let Some(p) = get_proxy(vm, obj) {
        let Some(trap) = get_proxy_trap(vm, &p, "getOwnPropertyDescriptor")? else {
            return mop_get_own_property(vm, p.target, key);
        };
        let kv = key_to_js_value(vm, key);
        let trap_res = vm.call_value(
            trap,
            JsValue::from_object(p.handler),
            &[JsValue::from_object(p.target), kv],
        )?;
        if !trap_res.is_undefined() && !trap_res.is_object() {
            return Err(vm.type_error(
                "'getOwnPropertyDescriptor' on proxy: trap returned neither object nor undefined",
            ));
        }
        let target_desc = mop_get_own_property(vm, p.target, key)?;
        if trap_res.is_undefined() {
            if let Some(td) = target_desc {
                if td.configurable != Some(true) {
                    return Err(vm.type_error(
                        "'getOwnPropertyDescriptor' on proxy: trap returned undefined for non-configurable property",
                    ));
                }
                if !mop_is_extensible(vm, p.target)? {
                    return Err(vm.type_error(
                        "'getOwnPropertyDescriptor' on proxy: trap returned undefined for property of non-extensible target",
                    ));
                }
            }
            return Ok(None);
        }
        let extensible_target = mop_is_extensible(vm, p.target)?;
        let result_desc = to_property_descriptor(vm, trap_res)?.complete_into_full();
        if !is_compatible_property_descriptor(vm, extensible_target, result_desc, target_desc) {
            return Err(vm.type_error(
                "'getOwnPropertyDescriptor' on proxy: trap returned incompatible descriptor",
            ));
        }
        if result_desc.configurable == Some(false) {
            match target_desc {
                None => {
                    return Err(vm.type_error(
                        "'getOwnPropertyDescriptor' on proxy: trap reported non-configurability for nonexistent property",
                    ));
                }
                Some(td) if td.configurable == Some(true) => {
                    return Err(vm.type_error(
                        "'getOwnPropertyDescriptor' on proxy: trap reported non-configurability for configurable property",
                    ));
                }
                Some(td)
                    if result_desc.is_data()
                        && result_desc.writable == Some(false)
                        && td.is_data()
                        && td.writable == Some(true) =>
                {
                    return Err(vm.type_error(
                        "'getOwnPropertyDescriptor' on proxy: trap reported non-writable for writable property",
                    ));
                }
                _ => {}
            }
        }
        return Ok(Some(result_desc));
    }

    // Ordinary / Array / TypedArray / String object lookup
    if let Some(prop) = vm.heap.object_get_own(obj, key, &vm.shapes) {
        let d = if prop.attrs.contains(PropertyAttrs::ACCESSOR) {
            PropDesc {
                get: Some(prop.getter.map_or(JsValue::UNDEFINED, JsValue::from_object)),
                set: Some(prop.setter.map_or(JsValue::UNDEFINED, JsValue::from_object)),
                enumerable: Some(prop.attrs.contains(PropertyAttrs::ENUMERABLE)),
                configurable: Some(prop.attrs.contains(PropertyAttrs::CONFIGURABLE)),
                ..PropDesc::default()
            }
        } else {
            PropDesc {
                value: Some(prop.value),
                writable: Some(prop.attrs.contains(PropertyAttrs::WRITABLE)),
                enumerable: Some(prop.attrs.contains(PropertyAttrs::ENUMERABLE)),
                configurable: Some(prop.attrs.contains(PropertyAttrs::CONFIGURABLE)),
                ..PropDesc::default()
            }
        };
        return Ok(Some(d));
    }
    if key == vm.key_length {
        if let Some(o) = vm.heap.get_object(obj) {
            if matches!(o.kind, ObjectKind::Array) {
                return Ok(Some(PropDesc {
                    value: Some(JsValue::from_f64(f64::from(o.elements.len()))),
                    writable: Some(true),
                    enumerable: Some(false),
                    configurable: Some(false),
                    ..PropDesc::default()
                }));
            }
        }
    }
    if let Some(name) = vm.heap.resolve_key(key) {
        if let Ok(idx) = name.parse::<u32>() {
            if idx < u32::MAX && idx.to_string() == name {
                if let Some(o) = vm.heap.get_object(obj) {
                    if let ObjectKind::TypedArray(ta) = o.kind {
                        if let Some(eff_len) =
                            crate::builtins_atomics::ta_effective_len(vm, obj, ta)
                        {
                            if idx < eff_len {
                                let v = vm.get_elem(
                                    JsValue::from_object(obj),
                                    JsValue::from_f64(f64::from(idx)),
                                )?;
                                return Ok(Some(PropDesc {
                                    value: Some(v),
                                    writable: Some(true),
                                    enumerable: Some(true),
                                    configurable: Some(true),
                                    ..PropDesc::default()
                                }));
                            }
                        }
                    } else if let Some(sh) = o
                        .rare
                        .as_ref()
                        .and_then(|r| r.boxed_value)
                        .and_then(|v| v.as_string())
                    {
                        if let Some(s) = vm.heap.materialize_string(sh) {
                            let units: Vec<u16> = s.encode_utf16().collect();
                            if (idx as usize) < units.len() {
                                let ch =
                                    String::from_utf16_lossy(&units[idx as usize..=idx as usize]);
                                let v = JsValue::from_string(vm.heap.alloc_string(ch));
                                return Ok(Some(PropDesc {
                                    value: Some(v),
                                    writable: Some(false),
                                    enumerable: Some(true),
                                    configurable: Some(false),
                                    ..PropDesc::default()
                                }));
                            }
                        }
                    } else if o.elements.has(idx) {
                        let v = o.elements.get(idx).unwrap_or(JsValue::UNDEFINED);
                        return Ok(Some(PropDesc {
                            value: Some(v),
                            writable: Some(true),
                            enumerable: Some(true),
                            configurable: Some(true),
                            ..PropDesc::default()
                        }));
                    }
                }
            }
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// 6. [[DefineOwnProperty]] (§10.5.6)
// ---------------------------------------------------------------------------

pub fn mop_define_own_property(
    vm: &mut JsVm,
    obj: HeapHandle,
    key: PropertyKey,
    desc: PropDesc,
) -> Result<bool, JsException> {
    if let Some(p) = get_proxy(vm, obj) {
        let Some(trap) = get_proxy_trap(vm, &p, "defineProperty")? else {
            return mop_define_own_property(vm, p.target, key, desc);
        };
        let kv = key_to_js_value(vm, key);
        let desc_obj = from_property_descriptor(vm, Some(desc));
        let r = vm.call_value(
            trap,
            JsValue::from_object(p.handler),
            &[JsValue::from_object(p.target), kv, desc_obj],
        )?;
        if !vm.to_boolean(r) {
            return Ok(false);
        }
        let target_desc = mop_get_own_property(vm, p.target, key)?;
        let extensible_target = mop_is_extensible(vm, p.target)?;
        let setting_config_false = desc.configurable == Some(false);
        match target_desc {
            None => {
                if !extensible_target {
                    return Err(vm.type_error(
                        "'defineProperty' on proxy: trap returned truish for defining new property on non-extensible target",
                    ));
                }
                if setting_config_false {
                    return Err(vm.type_error(
                        "'defineProperty' on proxy: trap returned truish for defining non-configurable property not on target",
                    ));
                }
            }
            Some(td) => {
                if !is_compatible_property_descriptor(vm, extensible_target, desc, Some(td)) {
                    return Err(vm.type_error(
                        "'defineProperty' on proxy: trap returned truish for defining incompatible property",
                    ));
                }
                if setting_config_false && td.configurable == Some(true) {
                    return Err(vm.type_error(
                        "'defineProperty' on proxy: trap returned truish for making configurable property non-configurable",
                    ));
                }
                if td.is_data()
                    && td.configurable == Some(false)
                    && td.writable == Some(true)
                    && desc.writable == Some(false)
                {
                    return Err(vm.type_error(
                        "'defineProperty' on proxy: trap returned truish for making non-configurable writable property non-writable",
                    ));
                }
            }
        }
        return Ok(true);
    }

    // Ordinary define property
    let current = mop_get_own_property(vm, obj, key)?;
    let extensible = vm.heap.get_object(obj).is_some_and(|o| o.extensible);
    if !is_compatible_property_descriptor(vm, extensible, desc, current) {
        return Ok(false);
    }
    // Array `length` special handling
    if key == vm.key_length
        && matches!(
            vm.heap.get_object(obj).map(|o| &o.kind),
            Some(ObjectKind::Array)
        )
    {
        if desc.is_accessor() || desc.enumerable == Some(true) || desc.configurable == Some(true) {
            return Ok(false);
        }
        let cur_w = vm
            .heap
            .object_get_own(obj, vm.key_length, &vm.shapes)
            .is_none_or(|p| p.attrs.contains(PropertyAttrs::WRITABLE));
        if !cur_w && desc.writable == Some(true) {
            return Ok(false);
        }
        let mut ok = true;
        if let Some(v) = desc.value {
            ok = vm.set_array_length_checked(obj, v)?;
        }
        if desc.writable == Some(false) {
            let cur_len = vm.heap.get_object(obj).map_or(0, |o| o.elements.len());
            vm.heap.object_define_data(
                obj,
                vm.key_length,
                JsValue::from_f64(f64::from(cur_len)),
                PropertyAttrs::empty(),
                &mut vm.shapes,
            );
        }
        return Ok(ok);
    }

    // If defining an indexed element descriptor, migrate out of dense elements.
    if let Some(name) = vm.heap.resolve_key(key).map(str::to_string) {
        if let Ok(idx) = name.parse::<u32>() {
            if idx < u32::MAX && idx.to_string() == name {
                let is_arr = matches!(
                    vm.heap.get_object(obj).map(|o| &o.kind),
                    Some(ObjectKind::Array)
                );
                if is_arr {
                    let cur_len = vm.heap.get_object(obj).map_or(0, |o| o.elements.len());
                    let len_writable = vm
                        .heap
                        .object_get_own(obj, vm.key_length, &vm.shapes)
                        .is_none_or(|p| p.attrs.contains(PropertyAttrs::WRITABLE));
                    if idx >= cur_len && !len_writable {
                        return Ok(false);
                    }
                    if let Some(o) = vm.heap.get_object_mut(obj) {
                        if idx >= o.elements.len() {
                            o.elements.set_len(idx + 1);
                        }
                    }
                }
                if let Some(o) = vm.heap.get_object_mut(obj) {
                    if o.elements.has(idx) {
                        o.elements.delete(idx);
                    }
                }
            }
        }
    }

    let merged = match current {
        None => desc.complete_into_full(),
        Some(cur) => {
            let is_acc = if desc.is_accessor() {
                true
            } else if desc.is_data() {
                false
            } else {
                cur.is_accessor()
            };
            if is_acc {
                PropDesc {
                    get: desc.get.or(cur.get).or(Some(JsValue::UNDEFINED)),
                    set: desc.set.or(cur.set).or(Some(JsValue::UNDEFINED)),
                    enumerable: desc.enumerable.or(cur.enumerable).or(Some(false)),
                    configurable: desc.configurable.or(cur.configurable).or(Some(false)),
                    ..PropDesc::default()
                }
            } else {
                PropDesc {
                    value: desc.value.or(cur.value).or(Some(JsValue::UNDEFINED)),
                    writable: desc.writable.or(cur.writable).or(Some(false)),
                    enumerable: desc.enumerable.or(cur.enumerable).or(Some(false)),
                    configurable: desc.configurable.or(cur.configurable).or(Some(false)),
                    ..PropDesc::default()
                }
            }
        }
    };

    let mut attrs = PropertyAttrs::empty();
    if merged.enumerable == Some(true) {
        attrs |= PropertyAttrs::ENUMERABLE;
    }
    if merged.configurable == Some(true) {
        attrs |= PropertyAttrs::CONFIGURABLE;
    }
    if merged.is_accessor() {
        let g = merged.get.and_then(|v| v.as_object());
        let s = merged.set.and_then(|v| v.as_object());
        vm.heap
            .object_replace_accessor(obj, key, g, s, attrs, &mut vm.shapes);
    } else {
        if merged.writable == Some(true) {
            attrs |= PropertyAttrs::WRITABLE;
        }
        let v = merged.value.unwrap_or(JsValue::UNDEFINED);
        vm.heap
            .object_define_data(obj, key, v, attrs, &mut vm.shapes);
        vm.heap.write_barrier(obj, v);
    }
    if vm.heap.get_object(obj).is_some_and(|o| o.is_prototype) {
        vm.mega_ic.invalidate_prototypes();
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// 7–10. [[HasProperty]], [[Get]], [[Set]], [[Delete]] Invariant Helpers (§10.5.7–10)
// ---------------------------------------------------------------------------

pub fn mop_has_property(
    vm: &mut JsVm,
    obj: HeapHandle,
    key: PropertyKey,
) -> Result<bool, JsException> {
    if let Some(p) = get_proxy(vm, obj) {
        return proxy_has_trap(vm, p, key);
    }
    vm.has_property_by_key(obj, key)
}

pub fn proxy_has_trap(
    vm: &mut JsVm,
    p: ProxyObject,
    key: PropertyKey,
) -> Result<bool, JsException> {
    let Some(trap) = get_proxy_trap(vm, &p, "has")? else {
        return vm.has_property_by_key(p.target, key);
    };
    let kv = key_to_js_value(vm, key);
    let r = vm.call_value(
        trap,
        JsValue::from_object(p.handler),
        &[JsValue::from_object(p.target), kv],
    )?;
    let res = vm.to_boolean(r);
    if !res {
        if let Some(td) = mop_get_own_property(vm, p.target, key)? {
            if td.configurable == Some(false) {
                return Err(vm.type_error(
                    "'has' on proxy: trap returned falsish for non-configurable property",
                ));
            }
            if !mop_is_extensible(vm, p.target)? {
                return Err(vm.type_error(
                    "'has' on proxy: trap returned falsish for property of non-extensible target",
                ));
            }
        }
    }
    Ok(res)
}

pub fn proxy_get_trap(
    vm: &mut JsVm,
    p: ProxyObject,
    key: PropertyKey,
    receiver: JsValue,
) -> Result<JsValue, JsException> {
    let Some(trap) = get_proxy_trap(vm, &p, "get")? else {
        return vm.get_property_with_receiver(p.target, key, receiver);
    };
    let kv = key_to_js_value(vm, key);
    let res = vm.call_value(
        trap,
        JsValue::from_object(p.handler),
        &[JsValue::from_object(p.target), kv, receiver],
    )?;
    if let Some(td) = mop_get_own_property(vm, p.target, key)? {
        if td.configurable == Some(false) {
            if td.is_data()
                && td.writable == Some(false)
                && !same_value(vm, res, td.value.unwrap_or(JsValue::UNDEFINED))
            {
                return Err(vm.type_error(
                    "'get' on proxy: property is a read-only and non-configurable data property on the proxy target",
                ));
            }
            if td.is_accessor() && td.get.is_none_or(|g| g.is_undefined()) && !res.is_undefined() {
                return Err(vm.type_error(
                    "'get' on proxy: property is a non-configurable accessor without a getter",
                ));
            }
        }
    }
    Ok(res)
}

pub fn proxy_set_trap(
    vm: &mut JsVm,
    p: ProxyObject,
    key: PropertyKey,
    val: JsValue,
    receiver: JsValue,
) -> Result<bool, JsException> {
    let Some(trap) = get_proxy_trap(vm, &p, "set")? else {
        return vm.set_property_with_receiver(p.target, key, val, receiver);
    };
    let kv = key_to_js_value(vm, key);
    let r = vm.call_value(
        trap,
        JsValue::from_object(p.handler),
        &[JsValue::from_object(p.target), kv, val, receiver],
    )?;
    if !vm.to_boolean(r) {
        return Ok(false);
    }
    if let Some(td) = mop_get_own_property(vm, p.target, key)? {
        if td.configurable == Some(false) {
            if td.is_data()
                && td.writable == Some(false)
                && !same_value(vm, val, td.value.unwrap_or(JsValue::UNDEFINED))
            {
                return Err(vm.type_error(
                    "'set' on proxy: trap returned truish for read-only non-configurable property",
                ));
            }
            if td.is_accessor() && td.set.is_none_or(|s| s.is_undefined()) {
                return Err(vm.type_error(
                    "'set' on proxy: trap returned truish for non-configurable accessor without a setter",
                ));
            }
        }
    }
    Ok(true)
}

pub fn proxy_delete_trap(
    vm: &mut JsVm,
    p: ProxyObject,
    key: PropertyKey,
) -> Result<bool, JsException> {
    let Some(trap) = get_proxy_trap(vm, &p, "deleteProperty")? else {
        return vm.delete_property_by_key(p.target, key);
    };
    let kv = key_to_js_value(vm, key);
    let r = vm.call_value(
        trap,
        JsValue::from_object(p.handler),
        &[JsValue::from_object(p.target), kv],
    )?;
    if !vm.to_boolean(r) {
        return Ok(false);
    }
    if let Some(td) = mop_get_own_property(vm, p.target, key)? {
        if td.configurable == Some(false) {
            return Err(vm.type_error(
                "'deleteProperty' on proxy: trap returned truish for non-configurable property",
            ));
        }
        if !mop_is_extensible(vm, p.target)? {
            return Err(vm.type_error(
                "'deleteProperty' on proxy: trap returned truish for property of non-extensible target",
            ));
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// 11. [[OwnPropertyKeys]] (§10.5.11)
// ---------------------------------------------------------------------------

pub fn mop_own_property_keys(
    vm: &mut JsVm,
    obj: HeapHandle,
) -> Result<Vec<(JsValue, PropertyKey)>, JsException> {
    if let Some(p) = get_proxy(vm, obj) {
        let Some(trap) = get_proxy_trap(vm, &p, "ownKeys")? else {
            return mop_own_property_keys(vm, p.target);
        };
        let trap_res_arr = vm.call_value(
            trap,
            JsValue::from_object(p.handler),
            &[JsValue::from_object(p.target)],
        )?;
        let list_h = trap_res_arr
            .as_object()
            .ok_or_else(|| vm.type_error("'ownKeys' on proxy: trap returned non-object"))?;
        let len = vm.array_get_len(list_h)?;
        let mut trap_result: Vec<(JsValue, PropertyKey)> =
            Vec::with_capacity(len.min(65_536) as usize);
        let mut seen: HashSet<PropertyKey> = HashSet::new();
        for i in 0..len {
            let kv = vm.array_get_index(list_h, i)?;
            if !kv.is_string() && !kv.is_symbol() {
                return Err(vm.type_error(
                    "'ownKeys' on proxy: trap result contained non-string/non-symbol key",
                ));
            }
            let ks = vm.to_property_key(kv)?;
            let pk = vm.heap.intern_key(&ks);
            if !seen.insert(pk) {
                return Err(vm.type_error("'ownKeys' on proxy: trap returned duplicate key"));
            }
            trap_result.push((kv, pk));
        }

        let extensible_target = mop_is_extensible(vm, p.target)?;
        let target_keys = mop_own_property_keys(vm, p.target)?;
        let mut target_non_conf = Vec::new();
        let mut target_conf = Vec::new();
        for (_, pk) in target_keys {
            if let Some(td) = mop_get_own_property(vm, p.target, pk)? {
                if td.configurable == Some(false) {
                    target_non_conf.push(pk);
                } else {
                    target_conf.push(pk);
                }
            }
        }
        if extensible_target && target_non_conf.is_empty() {
            return Ok(trap_result);
        }
        let mut unchecked: HashSet<PropertyKey> = seen;
        for pk in target_non_conf {
            if !unchecked.remove(&pk) {
                return Err(vm.type_error(
                    "'ownKeys' on proxy: trap result did not include non-configurable property",
                ));
            }
        }
        if extensible_target {
            return Ok(trap_result);
        }
        for pk in target_conf {
            if !unchecked.remove(&pk) {
                return Err(vm.type_error(
                    "'ownKeys' on proxy: trap result did not include property of non-extensible target",
                ));
            }
        }
        if !unchecked.is_empty() {
            return Err(vm.type_error(
                "'ownKeys' on proxy: trap returned extra keys for non-extensible target",
            ));
        }
        return Ok(trap_result);
    }

    // Ordinary / Array / TypedArray / String object own keys in §10.1.11.1 order
    let mut indices: Vec<u32> = Vec::new();
    let mut str_keys: Vec<(String, PropertyKey)> = Vec::new();
    let mut sym_keys: Vec<(SymbolId, PropertyKey)> = Vec::new();
    let mut seen_str: HashSet<PropertyKey> = HashSet::new();

    if let Some(o) = vm.heap.get_object(obj) {
        if let ObjectKind::TypedArray(ta) = o.kind {
            if let Some(eff_len) = crate::builtins_atomics::ta_effective_len(vm, obj, ta) {
                for i in 0..eff_len {
                    indices.push(i);
                }
            }
        } else {
            if let Some(sh) = o
                .rare
                .as_ref()
                .and_then(|r| r.boxed_value)
                .and_then(|v| v.as_string())
            {
                if let Some(s) = vm.heap.materialize_string(sh) {
                    let slen = s.encode_utf16().count().min(u32::MAX as usize) as u32;
                    for i in 0..slen {
                        indices.push(i);
                    }
                }
            }
            let elen = o.elements.len();
            for i in 0..elen {
                if o.elements.has(i) && !indices.contains(&i) {
                    indices.push(i);
                }
            }
        }
        let raw_keys: Vec<PropertyKey> = if o.is_dictionary() {
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
        let is_array = matches!(o.kind, ObjectKind::Array);
        for pk in raw_keys {
            if let Some(name) = vm.heap.resolve_key(pk).map(str::to_string) {
                if let Some(sym_num) = name
                    .strip_prefix("Symbol(")
                    .and_then(|s| s.strip_suffix(')'))
                    .and_then(|n| n.parse::<u32>().ok())
                {
                    sym_keys.push((SymbolId(sym_num), pk));
                } else if let Ok(idx) = name.parse::<u32>() {
                    if idx < u32::MAX && idx.to_string() == name {
                        if !indices.contains(&idx) {
                            indices.push(idx);
                        }
                    } else if seen_str.insert(pk) {
                        str_keys.push((name, pk));
                    }
                } else if seen_str.insert(pk) {
                    str_keys.push((name, pk));
                }
            }
        }
        if is_array && seen_str.insert(vm.key_length) {
            str_keys.push(("length".to_string(), vm.key_length));
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
