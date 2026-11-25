//! WHATWG HTML §4.13.7 `ElementInternals`, `CustomStateSet` (`:state(...)`),
//! W3C ARIA 1.3 `ARIAMixin` (string + element reflection), Form-Associated
//! Custom Elements, and standard HTML reflecting IDL properties with `[CEReactions]`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use rix_core::intern_str;
use rix_dom::{is_valid_custom_element_name, Document, ElementState, NodeData, NodeId};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::custom_elements::{
    lookup_element_def, ElementInternalsData, ValidityFlags, ARIA_ELEMENT_ATTRS, ARIA_STRING_ATTRS,
};
use crate::dom_bridge::{
    arg, define_host_accessor, define_host_method, get_attr_string, remove_element_attr,
    require_node, set_element_attr, wrap_node, SharedHost,
};
use crate::dom_interfaces::{make_proto, register_dom_interface, HTML_NS};
use crate::web_apis::create_dom_exception;

/// Installs `CustomStateSet`, `ElementInternals`, `HTMLElement.prototype.attachInternals()`,
/// `ARIAMixin` on `Element.prototype`, and reflecting HTML IDL attributes with `[CEReactions]`.
pub fn install_element_internals_and_aria(vm: &mut JsVm, host: &SharedHost) {
    install_custom_state_set_and_internals(vm, host);
    install_aria_mixin_on_element(vm, host);
    install_html_reflecting_idl_properties(vm, host);
}

fn freeze_js_value(vm: &mut JsVm, val: JsValue) {
    let k_obj = vm.heap.intern_key("Object");
    let k_freeze = vm.heap.intern_key("freeze");
    if let Ok(obj_ctor) = vm.get_property_by_key(vm.global, k_obj) {
        if let Some(oh) = obj_ctor.as_object() {
            if let Ok(fr) = vm.get_property_by_key(oh, k_freeze) {
                let _ = vm.call_value(fr, obj_ctor, &[val]);
            }
        }
    }
}

fn require_internals_node(
    vm: &mut JsVm,
    host: &SharedHost,
    this_val: JsValue,
) -> Result<NodeId, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Illegal invocation: expected ElementInternals"));
    };
    host.borrow()
        .custom_elements_state
        .borrow()
        .internals_handle_to_node
        .get(&h)
        .copied()
        .ok_or_else(|| vm.type_error("Illegal invocation: expected ElementInternals"))
}

fn require_form_associated(
    vm: &mut JsVm,
    host: &SharedHost,
    nid: NodeId,
) -> Result<(), JsException> {
    let is_fa = lookup_element_def(host, nid).is_some_and(|d| d.form_associated);
    if !is_fa {
        return Err(create_dom_exception(
            vm,
            "NotSupportedError",
            "Target element is not a form-associated custom element",
        ));
    }
    Ok(())
}

fn require_state_set_node(
    vm: &mut JsVm,
    host: &SharedHost,
    this_val: JsValue,
) -> Result<NodeId, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Illegal invocation: expected CustomStateSet"));
    };
    host.borrow()
        .custom_elements_state
        .borrow()
        .state_set_handle_to_node
        .get(&h)
        .copied()
        .ok_or_else(|| vm.type_error("Illegal invocation: expected CustomStateSet"))
}

fn sync_element_custom_states(host: &SharedHost, nid: NodeId) {
    let states: Vec<rix_core::InternedStr> = {
        let hb = host.borrow();
        let ce = hb.custom_elements_state.borrow();
        ce.internals_data
            .get(&nid)
            .map(|d| d.state_entries.iter().map(|(_, s)| intern_str(s)).collect())
            .unwrap_or_default()
    };
    {
        let doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = doc_rc.borrow_mut();
        if let Some(NodeData::Element(e)) = doc.get_mut(nid) {
            e.custom_states = states;
        }
    }
    host.borrow_mut().invalidate_style_for_node(nid);
}

fn install_custom_state_set_and_internals(vm: &mut JsVm, host: &SharedHost) {
    // 1. `CustomStateSet`
    let ss_proto = make_proto(vm, vm.object_proto);
    let _ = register_dom_interface(vm, "CustomStateSet", ss_proto, None, false);
    host.borrow()
        .custom_elements_state
        .borrow_mut()
        .state_set_proto = Some(ss_proto);

    let h = Rc::clone(host);
    let g_size = vm.alloc_host_fn("get size", move |vm, t, _| {
        let nid = require_state_set_node(vm, &h, t)?;
        let len = h
            .borrow()
            .custom_elements_state
            .borrow()
            .internals_data
            .get(&nid)
            .map_or(0, |d| d.state_entries.len());
        Ok(JsValue::from_f64(len as f64))
    });
    define_host_accessor(vm, ss_proto, "size", Some(g_size), None);

    let h = Rc::clone(host);
    define_host_method(vm, ss_proto, "has", move |vm, t, args| {
        let nid = require_state_set_node(vm, &h, t)?;
        let val = crate::dom_parsing::to_dom_string(vm, arg(args, 0))?;
        let found = h
            .borrow()
            .custom_elements_state
            .borrow()
            .internals_data
            .get(&nid)
            .is_some_and(|d| d.state_entries.iter().any(|(_, s)| s == &val));
        Ok(JsValue::from_bool(found))
    });

    let h = Rc::clone(host);
    define_host_method(vm, ss_proto, "add", move |vm, t, args| {
        let nid = require_state_set_node(vm, &h, t)?;
        let val = crate::dom_parsing::to_dom_string(vm, arg(args, 0))?;
        let mut changed = false;
        {
            let hb = h.borrow();
            let mut ce = hb.custom_elements_state.borrow_mut();
            if let Some(d) = ce.internals_data.get_mut(&nid) {
                if !d.state_entries.iter().any(|(_, s)| s == &val) {
                    d.next_state_seq += 1;
                    let seq = d.next_state_seq;
                    d.state_entries.push((seq, val));
                    changed = true;
                }
            }
        }
        if changed {
            sync_element_custom_states(&h, nid);
        }
        Ok(t)
    });

    let h = Rc::clone(host);
    define_host_method(vm, ss_proto, "delete", move |vm, t, args| {
        let nid = require_state_set_node(vm, &h, t)?;
        let val = crate::dom_parsing::to_dom_string(vm, arg(args, 0))?;
        let removed = {
            let hb = h.borrow();
            let mut ce = hb.custom_elements_state.borrow_mut();
            if let Some(d) = ce.internals_data.get_mut(&nid) {
                let before = d.state_entries.len();
                d.state_entries.retain(|(_, s)| s != &val);
                d.state_entries.len() != before
            } else {
                false
            }
        };
        if removed {
            sync_element_custom_states(&h, nid);
        }
        Ok(JsValue::from_bool(removed))
    });

    let h = Rc::clone(host);
    define_host_method(vm, ss_proto, "clear", move |vm, t, _| {
        let nid = require_state_set_node(vm, &h, t)?;
        let had_any = {
            let hb = h.borrow();
            let mut ce = hb.custom_elements_state.borrow_mut();
            if let Some(d) = ce.internals_data.get_mut(&nid) {
                let any = !d.state_entries.is_empty();
                d.state_entries.clear();
                any
            } else {
                false
            }
        };
        if had_any {
            sync_element_custom_states(&h, nid);
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, ss_proto, "forEach", move |vm, t, args| {
        let nid = require_state_set_node(vm, &h, t)?;
        let cb = arg(args, 0);
        if !cb.as_object().is_some_and(|ch| vm.is_callable_handle(ch)) {
            return Err(vm.type_error("CustomStateSet.forEach requires a callback function"));
        }
        let this_arg = arg(args, 1);
        let mut last_seq = 0u64;
        loop {
            let next_item = {
                let hb = h.borrow();
                let ce = hb.custom_elements_state.borrow();
                ce.internals_data.get(&nid).and_then(|d| {
                    d.state_entries
                        .iter()
                        .find(|(seq, _)| *seq > last_seq)
                        .cloned()
                })
            };
            let Some((seq, s)) = next_item else {
                break;
            };
            last_seq = seq;
            let sv = JsValue::from_string(vm.heap.alloc_string(s));
            vm.call_value(cb, this_arg, &[sv, sv, t])?;
        }
        Ok(JsValue::UNDEFINED)
    });

    for (mname, is_entries) in [("values", false), ("keys", false), ("entries", true)] {
        let h = Rc::clone(host);
        let fn_h = vm.alloc_host_fn(mname, move |vm, t, _| {
            let nid = require_state_set_node(vm, &h, t)?;
            let cursor = Rc::new(RefCell::new((0u64, false)));
            let iter = vm.alloc_ordinary_object();
            let h_next = Rc::clone(&h);
            let cur_next = Rc::clone(&cursor);
            define_host_method(vm, iter, "next", move |vm, _, _| {
                let mut st = cur_next.borrow_mut();
                let res = vm.alloc_ordinary_object();
                let kv = vm.heap.intern_key("value");
                let kd = vm.heap.intern_key("done");
                if st.1 {
                    vm.heap.object_define_data(
                        res,
                        kv,
                        JsValue::UNDEFINED,
                        PropertyAttrs::DEFAULT,
                        &mut vm.shapes,
                    );
                    vm.heap.object_define_data(
                        res,
                        kd,
                        JsValue::TRUE,
                        PropertyAttrs::DEFAULT,
                        &mut vm.shapes,
                    );
                    return Ok(JsValue::from_object(res));
                }
                let next_item = {
                    let hb = h_next.borrow();
                    let ce = hb.custom_elements_state.borrow();
                    ce.internals_data
                        .get(&nid)
                        .and_then(|d| d.state_entries.iter().find(|(seq, _)| *seq > st.0).cloned())
                };
                if let Some((seq, s)) = next_item {
                    st.0 = seq;
                    let sv = JsValue::from_string(vm.heap.alloc_string(s));
                    let out_val = if is_entries {
                        let pair = vm.alloc_array();
                        vm.heap.object_push_element(pair, sv);
                        vm.heap.object_push_element(pair, sv);
                        JsValue::from_object(pair)
                    } else {
                        sv
                    };
                    vm.heap.object_define_data(
                        res,
                        kv,
                        out_val,
                        PropertyAttrs::DEFAULT,
                        &mut vm.shapes,
                    );
                    vm.heap.object_define_data(
                        res,
                        kd,
                        JsValue::FALSE,
                        PropertyAttrs::DEFAULT,
                        &mut vm.shapes,
                    );
                } else {
                    st.1 = true;
                    vm.heap.object_define_data(
                        res,
                        kv,
                        JsValue::UNDEFINED,
                        PropertyAttrs::DEFAULT,
                        &mut vm.shapes,
                    );
                    vm.heap.object_define_data(
                        res,
                        kd,
                        JsValue::TRUE,
                        PropertyAttrs::DEFAULT,
                        &mut vm.shapes,
                    );
                }
                Ok(JsValue::from_object(res))
            });
            let self_iter = vm.alloc_host_fn("[Symbol.iterator]", |_vm, t, _| Ok(t));
            vm.heap.object_define_data(
                iter,
                vm.key_symbol_iterator,
                JsValue::from_object(self_iter),
                PropertyAttrs::METHOD,
                &mut vm.shapes,
            );
            Ok(JsValue::from_object(iter))
        });
        let k = vm.heap.intern_key(mname);
        vm.heap.object_define_data(
            ss_proto,
            k,
            JsValue::from_object(fn_h),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
        if mname == "values" {
            vm.heap.object_define_data(
                ss_proto,
                vm.key_symbol_iterator,
                JsValue::from_object(fn_h),
                PropertyAttrs::METHOD,
                &mut vm.shapes,
            );
        }
    }

    // 2. `ElementInternals`
    let ei_proto = make_proto(vm, vm.object_proto);
    let _ = register_dom_interface(vm, "ElementInternals", ei_proto, None, false);
    host.borrow()
        .custom_elements_state
        .borrow_mut()
        .internals_proto = Some(ei_proto);

    let h = Rc::clone(host);
    let g_sr = vm.alloc_host_fn("get shadowRoot", move |vm, t, _| {
        let nid = require_internals_node(vm, &h, t)?;
        let sr_opt = {
            let hb = h.borrow();
            let doc = hb.document.borrow();
            match doc.get(nid) {
                Some(NodeData::Element(e)) => e.shadow_root,
                _ => None,
            }
        };
        Ok(sr_opt.map_or(JsValue::NULL, |sr| wrap_node(vm, &h, sr)))
    });
    define_host_accessor(vm, ei_proto, "shadowRoot", Some(g_sr), None);

    let h = Rc::clone(host);
    let g_st = vm.alloc_host_fn("get states", move |vm, t, _| {
        let nid = require_internals_node(vm, &h, t)?;
        let sh = h
            .borrow()
            .custom_elements_state
            .borrow()
            .internals_data
            .get(&nid)
            .map(|d| d.states_handle);
        Ok(sh.map_or(JsValue::NULL, JsValue::from_object))
    });
    define_host_accessor(vm, ei_proto, "states", Some(g_st), None);

    for &(idl_name, _) in ARIA_STRING_ATTRS {
        let prop_key = idl_name.to_string();
        let h1 = Rc::clone(host);
        let pk1 = prop_key.clone();
        let g = vm.alloc_host_fn(&format!("get {idl_name}"), move |vm, t, _| {
            let nid = require_internals_node(vm, &h1, t)?;
            let hb = h1.borrow();
            let ce = hb.custom_elements_state.borrow();
            let val = ce.internals_data.get(&nid).and_then(|d| {
                if pk1 == "role" {
                    d.role.clone()
                } else {
                    d.aria_strings.get(&pk1).cloned().flatten()
                }
            });
            Ok(val.map_or(JsValue::NULL, |s| {
                JsValue::from_string(vm.heap.alloc_string(s))
            }))
        });
        let h2 = Rc::clone(host);
        let pk2 = prop_key;
        let s = vm.alloc_host_fn(&format!("set {idl_name}"), move |vm, t, args| {
            let nid = require_internals_node(vm, &h2, t)?;
            let v = arg(args, 0);
            let new_val = if v.is_null() {
                None
            } else {
                Some(crate::dom_parsing::to_dom_string(vm, v)?)
            };
            let hb = h2.borrow();
            let mut ce = hb.custom_elements_state.borrow_mut();
            if let Some(d) = ce.internals_data.get_mut(&nid) {
                if pk2 == "role" {
                    d.role = new_val;
                } else {
                    d.aria_strings.insert(pk2.clone(), new_val);
                }
            }
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, ei_proto, idl_name, Some(g), Some(s));
    }

    for &(idl_name, _, is_list) in ARIA_ELEMENT_ATTRS {
        let prop_key = idl_name.to_string();
        let h1 = Rc::clone(host);
        let pk1 = prop_key.clone();
        let g = vm.alloc_host_fn(&format!("get {idl_name}"), move |vm, t, _| {
            let nid = require_internals_node(vm, &h1, t)?;
            let stored = h1
                .borrow()
                .custom_elements_state
                .borrow()
                .internals_data
                .get(&nid)
                .and_then(|d| d.aria_elements.get(&pk1).cloned())
                .flatten();
            let Some(nodes) = stored else {
                return Ok(JsValue::NULL);
            };
            let valid: Vec<NodeId> = {
                let hb = h1.borrow();
                let doc = hb.document.borrow();
                let host_si_root = shadow_including_root(&doc, nid);
                nodes
                    .into_iter()
                    .filter(|&target| {
                        shadow_including_root(&doc, target) == host_si_root
                            && crate::shadow_dom::is_shadow_including_inclusive_ancestor(
                                &doc,
                                crate::shadow_dom::tree_root(&doc, target),
                                nid,
                            )
                    })
                    .collect()
            };
            if is_list {
                let cached = h1
                    .borrow()
                    .custom_elements_state
                    .borrow()
                    .internals_data
                    .get(&nid)
                    .and_then(|d| d.cached_aria_arrays.get(&pk1).cloned());
                if let Some((ch, cached_nodes)) = cached {
                    if cached_nodes == valid {
                        return Ok(JsValue::from_object(ch));
                    }
                }
                let arr = vm.alloc_array();
                for &n in &valid {
                    let w = wrap_node(vm, &h1, n);
                    vm.heap.object_push_element(arr, w);
                }
                let arr_v = JsValue::from_object(arr);
                freeze_js_value(vm, arr_v);
                if let Some(d) = h1
                    .borrow()
                    .custom_elements_state
                    .borrow_mut()
                    .internals_data
                    .get_mut(&nid)
                {
                    d.cached_aria_arrays.insert(pk1.clone(), (arr, valid));
                }
                Ok(arr_v)
            } else {
                Ok(valid
                    .first()
                    .map_or(JsValue::NULL, |&n| wrap_node(vm, &h1, n)))
            }
        });
        let h2 = Rc::clone(host);
        let pk2 = prop_key;
        let s = vm.alloc_host_fn(&format!("set {idl_name}"), move |vm, t, args| {
            let nid = require_internals_node(vm, &h2, t)?;
            let v = arg(args, 0);
            let stored = if v.is_null() {
                None
            } else if is_list {
                let items = vm.collect_iterable(v)?;
                let mut list = Vec::with_capacity(items.len());
                for item in items {
                    let el_id = require_node(vm, &h2, item)?;
                    if !matches!(
                        h2.borrow().document.borrow().get(el_id),
                        Some(NodeData::Element(_))
                    ) {
                        return Err(vm.type_error("Expected Element in ARIA elements array"));
                    }
                    list.push(el_id);
                }
                Some(list)
            } else {
                let el_id = require_node(vm, &h2, v)?;
                if !matches!(
                    h2.borrow().document.borrow().get(el_id),
                    Some(NodeData::Element(_))
                ) {
                    return Err(vm.type_error("Expected Element for ARIA element reflection"));
                }
                Some(vec![el_id])
            };
            let hb = h2.borrow();
            let mut ce = hb.custom_elements_state.borrow_mut();
            if let Some(d) = ce.internals_data.get_mut(&nid) {
                d.aria_elements.insert(pk2.clone(), stored);
                d.cached_aria_arrays.remove(&pk2);
            }
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, ei_proto, idl_name, Some(g), Some(s));
    }

    // Form-associated `ElementInternals` methods & getters
    let h = Rc::clone(host);
    define_host_method(vm, ei_proto, "setFormValue", move |vm, t, args| {
        let nid = require_internals_node(vm, &h, t)?;
        require_form_associated(vm, &h, nid)?;
        let val = arg(args, 0);
        let state = if args.len() >= 2 { arg(args, 1) } else { val };
        let hb = h.borrow();
        let mut ce = hb.custom_elements_state.borrow_mut();
        if let Some(d) = ce.internals_data.get_mut(&nid) {
            d.form_value = if val.is_null() || val.is_undefined() {
                None
            } else {
                Some(val)
            };
            d.form_state = if state.is_null() || state.is_undefined() {
                None
            } else {
                Some(state)
            };
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    let g_form = vm.alloc_host_fn("get form", move |vm, t, _| {
        let nid = require_internals_node(vm, &h, t)?;
        require_form_associated(vm, &h, nid)?;
        let form_node = find_form_owner(&h, nid);
        Ok(form_node.map_or(JsValue::NULL, |f| wrap_node(vm, &h, f)))
    });
    define_host_accessor(vm, ei_proto, "form", Some(g_form), None);

    let h = Rc::clone(host);
    define_host_method(vm, ei_proto, "setValidity", move |vm, t, args| {
        let nid = require_internals_node(vm, &h, t)?;
        require_form_associated(vm, &h, nid)?;
        let mut flags = ValidityFlags::default();
        let f_arg = arg(args, 0);
        if let Some(fh) = f_arg.as_object() {
            let get_b = |vm: &mut JsVm, key: &str| -> Result<bool, JsException> {
                let k = vm.heap.intern_key(key);
                let v = vm.get_property_by_key(fh, k)?;
                Ok(vm.to_boolean(v))
            };
            flags.value_missing = get_b(vm, "valueMissing")?;
            flags.type_mismatch = get_b(vm, "typeMismatch")?;
            flags.pattern_mismatch = get_b(vm, "patternMismatch")?;
            flags.too_long = get_b(vm, "tooLong")?;
            flags.too_short = get_b(vm, "tooShort")?;
            flags.range_underflow = get_b(vm, "rangeUnderflow")?;
            flags.range_overflow = get_b(vm, "rangeOverflow")?;
            flags.step_mismatch = get_b(vm, "stepMismatch")?;
            flags.bad_input = get_b(vm, "badInput")?;
            flags.custom_error = get_b(vm, "customError")?;
        }
        let msg = if args.get(1).is_some_and(|v| !v.is_undefined()) {
            crate::dom_parsing::to_dom_string(vm, args[1])?
        } else {
            String::new()
        };
        if !flags.is_valid() && msg.is_empty() {
            return Err(vm.type_error(
                "setValidity requires a non-empty validation message when a validity flag is true",
            ));
        }
        let anchor = if args
            .get(2)
            .is_some_and(|v| !v.is_undefined() && !v.is_null())
        {
            let aid = require_node(vm, &h, args[2])?;
            let valid_anchor = {
                let hb = h.borrow();
                let doc = hb.document.borrow();
                crate::shadow_dom::is_shadow_including_inclusive_ancestor(&doc, nid, aid)
                    && aid != nid
            };
            if !valid_anchor {
                return Err(create_dom_exception(
                    vm,
                    "NotFoundError",
                    "Validation anchor must be a shadow-including descendant of the element",
                ));
            }
            Some(aid)
        } else {
            None
        };
        let is_valid = flags.is_valid();
        {
            let hb = h.borrow();
            let mut ce = hb.custom_elements_state.borrow_mut();
            if let Some(d) = ce.internals_data.get_mut(&nid) {
                d.validity = flags;
                d.validation_message = if is_valid { String::new() } else { msg };
                d.validation_anchor = anchor;
            }
        }
        {
            let doc_rc = Rc::clone(&h.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            if let Some(NodeData::Element(e)) = doc.get_mut(nid) {
                e.pseudo_state.set(ElementState::INVALID, !is_valid);
            }
        }
        h.borrow_mut()
            .invalidate_element_state(nid, ElementState::INVALID);
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    let g_wv = vm.alloc_host_fn("get willValidate", move |vm, t, _| {
        let nid = require_internals_node(vm, &h, t)?;
        require_form_associated(vm, &h, nid)?;
        let wv = !is_element_barred_from_validation(&h, nid);
        Ok(JsValue::from_bool(wv))
    });
    define_host_accessor(vm, ei_proto, "willValidate", Some(g_wv), None);

    let h = Rc::clone(host);
    let g_val = vm.alloc_host_fn("get validity", move |vm, t, _| {
        let nid = require_internals_node(vm, &h, t)?;
        require_form_associated(vm, &h, nid)?;
        let flags = h
            .borrow()
            .custom_elements_state
            .borrow()
            .internals_data
            .get(&nid)
            .map(|d| d.validity.clone())
            .unwrap_or_default();
        let obj = vm.alloc_ordinary_object();
        for (kname, bval) in [
            ("valueMissing", flags.value_missing),
            ("typeMismatch", flags.type_mismatch),
            ("patternMismatch", flags.pattern_mismatch),
            ("tooLong", flags.too_long),
            ("tooShort", flags.too_short),
            ("rangeUnderflow", flags.range_underflow),
            ("rangeOverflow", flags.range_overflow),
            ("stepMismatch", flags.step_mismatch),
            ("badInput", flags.bad_input),
            ("customError", flags.custom_error),
            ("valid", flags.is_valid()),
        ] {
            let k = vm.heap.intern_key(kname);
            vm.heap.object_define_data(
                obj,
                k,
                JsValue::from_bool(bval),
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
        }
        Ok(JsValue::from_object(obj))
    });
    define_host_accessor(vm, ei_proto, "validity", Some(g_val), None);

    let h = Rc::clone(host);
    let g_vm = vm.alloc_host_fn("get validationMessage", move |vm, t, _| {
        let nid = require_internals_node(vm, &h, t)?;
        require_form_associated(vm, &h, nid)?;
        let msg = if is_element_barred_from_validation(&h, nid) {
            String::new()
        } else {
            h.borrow()
                .custom_elements_state
                .borrow()
                .internals_data
                .get(&nid)
                .map(|d| d.validation_message.clone())
                .unwrap_or_default()
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(msg)))
    });
    define_host_accessor(vm, ei_proto, "validationMessage", Some(g_vm), None);

    for mname in ["checkValidity", "reportValidity"] {
        let h = Rc::clone(host);
        define_host_method(vm, ei_proto, mname, move |vm, t, _| {
            let nid = require_internals_node(vm, &h, t)?;
            require_form_associated(vm, &h, nid)?;
            if is_element_barred_from_validation(&h, nid) {
                return Ok(JsValue::TRUE);
            }
            let valid = h
                .borrow()
                .custom_elements_state
                .borrow()
                .internals_data
                .get(&nid)
                .is_none_or(|d| d.validity.is_valid());
            Ok(JsValue::from_bool(valid))
        });
    }

    let h = Rc::clone(host);
    let g_labels = vm.alloc_host_fn("get labels", move |vm, t, _| {
        let nid = require_internals_node(vm, &h, t)?;
        require_form_associated(vm, &h, nid)?;
        let arr = vm.alloc_array();
        let labels = find_labels_for_element(&h, nid);
        for l in labels {
            let w = wrap_node(vm, &h, l);
            vm.heap.object_push_element(arr, w);
        }
        Ok(JsValue::from_object(arr))
    });
    define_host_accessor(vm, ei_proto, "labels", Some(g_labels), None);

    // 3. `HTMLElement.prototype.attachInternals()`
    let hep = host.borrow().prototypes.html_element_proto;
    let h = Rc::clone(host);
    define_host_method(vm, hep, "attachInternals", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let (is_customized_builtin, already_attached, disabled_internals) = {
            let hb = h.borrow();
            let doc = hb.document.borrow();
            let is_cbi = match doc.get(nid) {
                Some(NodeData::Element(e)) => e.is_value.is_some(),
                _ => {
                    return Err(create_dom_exception(
                        vm,
                        "NotSupportedError",
                        "Not an HTMLElement",
                    ))
                }
            };
            let ce = hb.custom_elements_state.borrow();
            let attached = ce.internals_attached.contains(&nid);
            let disabled = ce
                .element_definitions
                .get(&nid)
                .and_then(|(rid, name)| ce.registries.get(rid)?.by_name.get(name))
                .is_some_and(|d| d.disable_internals);
            (is_cbi, attached, disabled)
        };
        if is_customized_builtin {
            return Err(create_dom_exception(
                vm,
                "NotSupportedError",
                "Unable to attach ElementInternals to a customized built-in element",
            ));
        }
        if disabled_internals {
            return Err(create_dom_exception(
                vm,
                "NotSupportedError",
                "ElementInternals is disabled for this custom element",
            ));
        }
        if already_attached {
            return Err(create_dom_exception(
                vm,
                "NotSupportedError",
                "ElementInternals for the specified element was already attached",
            ));
        }
        let valid_ce = lookup_element_def(&h, nid).is_some()
            && matches!(
                h.borrow().document.borrow().get(nid),
                Some(NodeData::Element(e))
                    if &*e.name.ns == HTML_NS && is_valid_custom_element_name(&e.name.local)
            );
        if !valid_ce {
            return Err(create_dom_exception(
                vm,
                "NotSupportedError",
                "ElementInternals can only be attached to a defined autonomous custom element",
            ));
        }

        let ei_obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(ei_proto)));
        let ss_obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(ss_proto)));
        {
            let hb = h.borrow();
            let mut ce = hb.custom_elements_state.borrow_mut();
            ce.internals_attached.insert(nid);
            ce.internals_handle_to_node.insert(ei_obj, nid);
            ce.state_set_handle_to_node.insert(ss_obj, nid);
            ce.internals_data.insert(
                nid,
                ElementInternalsData {
                    js_handle: ei_obj,
                    states_handle: ss_obj,
                    state_entries: Vec::new(),
                    next_state_seq: 0,
                    role: None,
                    aria_strings: HashMap::new(),
                    aria_elements: HashMap::new(),
                    cached_aria_arrays: HashMap::new(),
                    form_value: None,
                    form_state: None,
                    validity: ValidityFlags::default(),
                    validation_message: String::new(),
                    validation_anchor: None,
                },
            );
        }
        Ok(JsValue::from_object(ei_obj))
    });
}

fn shadow_including_root(doc: &Document, mut nid: NodeId) -> NodeId {
    for _ in 0..128 {
        nid = crate::shadow_dom::tree_root(doc, nid);
        if let Some(NodeData::ShadowRoot { host, .. }) = doc.get(nid) {
            nid = *host;
        } else {
            return nid;
        }
    }
    nid
}

pub(crate) fn find_form_owner(host: &SharedHost, nid: NodeId) -> Option<NodeId> {
    let hb = host.borrow();
    let doc = hb.document.borrow();
    if let Some(NodeData::Element(e)) = doc.get(nid) {
        if let Some(form_id) = e.get_attribute("form") {
            let root = crate::shadow_dom::tree_root(&doc, nid);
            for cand in doc.descendants(root) {
                if matches!(
                    doc.get(cand),
                    Some(NodeData::Element(fe))
                        if fe.name.local.as_str().eq_ignore_ascii_case("form")
                            && fe.get_attribute("id") == Some(form_id)
                ) {
                    return Some(cand);
                }
            }
            return None;
        }
    }
    let mut cur = doc.parent(nid);
    while let Some(p) = cur {
        if matches!(
            doc.get(p),
            Some(NodeData::Element(fe)) if fe.name.local.as_str().eq_ignore_ascii_case("form")
        ) {
            return Some(p);
        }
        cur = doc.parent(p);
    }
    None
}

pub(crate) fn is_element_disabled_for_form(host: &SharedHost, nid: NodeId) -> bool {
    let hb = host.borrow();
    let doc = hb.document.borrow();
    if let Some(NodeData::Element(e)) = doc.get(nid) {
        if e.has_attribute("disabled") {
            return true;
        }
    }
    let mut cur = doc.parent(nid);
    while let Some(p) = cur {
        if let Some(NodeData::Element(pe)) = doc.get(p) {
            if pe.name.local.as_str().eq_ignore_ascii_case("fieldset")
                && pe.has_attribute("disabled")
            {
                // HTML §4.10.16: elements inside the fieldset's first `<legend>` child are NOT disabled!
                let first_legend = doc.children(p).find(|&c| {
                    matches!(
                        doc.get(c),
                        Some(NodeData::Element(le)) if le.name.local.as_str().eq_ignore_ascii_case("legend")
                    )
                });
                let in_first_legend = first_legend.is_some_and(|leg| {
                    let mut c = Some(nid);
                    while let Some(x) = c {
                        if x == leg {
                            return true;
                        }
                        if x == p {
                            return false;
                        }
                        c = doc.parent(x);
                    }
                    false
                });
                if !in_first_legend {
                    return true;
                }
            }
        }
        cur = doc.parent(p);
    }
    false
}

fn is_element_barred_from_validation(host: &SharedHost, nid: NodeId) -> bool {
    if is_element_disabled_for_form(host, nid) {
        return true;
    }
    let hb = host.borrow();
    let doc = hb.document.borrow();
    if let Some(NodeData::Element(e)) = doc.get(nid) {
        if e.has_attribute("readonly") {
            return true;
        }
    }
    let mut cur = doc.parent(nid);
    while let Some(p) = cur {
        if let Some(NodeData::Element(pe)) = doc.get(p) {
            if pe.name.local.as_str().eq_ignore_ascii_case("datalist") {
                return true;
            }
        }
        cur = doc.parent(p);
    }
    false
}

fn find_labels_for_element(host: &SharedHost, nid: NodeId) -> Vec<NodeId> {
    let hb = host.borrow();
    let doc = hb.document.borrow();
    let root = crate::shadow_dom::tree_root(&doc, nid);
    let my_id = match doc.get(nid) {
        Some(NodeData::Element(e)) => e.get_attribute("id").map(str::to_string),
        _ => None,
    };
    let mut out = Vec::new();
    for cand in doc.descendants(root) {
        if let Some(NodeData::Element(le)) = doc.get(cand) {
            if le.name.local.as_str().eq_ignore_ascii_case("label") {
                if let Some(for_attr) = le.get_attribute("for") {
                    if my_id.as_deref() == Some(for_attr) {
                        out.push(cand);
                    }
                } else {
                    let mut cur = doc.parent(nid);
                    while let Some(p) = cur {
                        if p == cand {
                            out.push(cand);
                            break;
                        }
                        cur = doc.parent(p);
                    }
                }
            }
        }
    }
    out
}

fn install_aria_mixin_on_element(vm: &mut JsVm, host: &SharedHost) {
    let ep = host.borrow().prototypes.element_proto;
    for &(idl_name, attr_name) in ARIA_STRING_ATTRS {
        let h1 = Rc::clone(host);
        let g = vm.alloc_host_fn(&format!("get {idl_name}"), move |vm, t, _| {
            let nid = require_node(vm, &h1, t)?;
            Ok(
                get_attr_string(&h1, nid, attr_name).map_or(JsValue::NULL, |s| {
                    JsValue::from_string(vm.heap.alloc_string(s))
                }),
            )
        });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn(&format!("set {idl_name}"), move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            let v = arg(args, 0);
            if v.is_null() {
                remove_element_attr(vm, &h2, nid, attr_name);
            } else {
                let sv = crate::dom_parsing::to_dom_string(vm, v)?;
                set_element_attr(vm, &h2, nid, attr_name, &sv);
            }
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, ep, idl_name, Some(g), Some(s));
    }

    for &(idl_name, attr_name, is_list) in ARIA_ELEMENT_ATTRS {
        let h1 = Rc::clone(host);
        let g = vm.alloc_host_fn(&format!("get {idl_name}"), move |vm, t, _| {
            let nid = require_node(vm, &h1, t)?;
            let explicit = h1
                .borrow()
                .custom_elements_state
                .borrow()
                .element_aria_elements
                .get(&(nid, attr_name.to_string()))
                .cloned();
            let found: Vec<NodeId> = if let Some(nodes) = explicit {
                let hb = h1.borrow();
                let doc = hb.document.borrow();
                let host_si_root = shadow_including_root(&doc, nid);
                nodes
                    .into_iter()
                    .filter(|&target| {
                        shadow_including_root(&doc, target) == host_si_root
                            && crate::shadow_dom::is_shadow_including_inclusive_ancestor(
                                &doc,
                                crate::shadow_dom::tree_root(&doc, target),
                                nid,
                            )
                    })
                    .collect()
            } else {
                let Some(attr_val) = get_attr_string(&h1, nid, attr_name) else {
                    return Ok(JsValue::NULL);
                };
                let ids: Vec<&str> = attr_val.split_ascii_whitespace().collect();
                let hb = h1.borrow();
                let doc = hb.document.borrow();
                let root = crate::shadow_dom::tree_root(&doc, nid);
                ids.iter()
                    .filter_map(|id_str| {
                        doc.descendants(root).find(|&c| {
                            matches!(
                                doc.get(c),
                                Some(NodeData::Element(e)) if e.get_attribute("id") == Some(*id_str)
                            )
                        })
                    })
                    .collect()
            };
            if is_list {
                let arr = vm.alloc_array();
                for n in found {
                    let w = wrap_node(vm, &h1, n);
                    vm.heap.object_push_element(arr, w);
                }
                let arr_v = JsValue::from_object(arr);
                freeze_js_value(vm, arr_v);
                Ok(arr_v)
            } else {
                Ok(found
                    .first()
                    .map_or(JsValue::NULL, |&n| wrap_node(vm, &h1, n)))
            }
        });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn(&format!("set {idl_name}"), move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            let v = arg(args, 0);
            if v.is_null() {
                h2.borrow()
                    .custom_elements_state
                    .borrow_mut()
                    .element_aria_elements
                    .remove(&(nid, attr_name.to_string()));
                remove_element_attr(vm, &h2, nid, attr_name);
                return Ok(JsValue::UNDEFINED);
            }
            let list: Vec<NodeId> = if is_list {
                let items = vm.collect_iterable(v)?;
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    let el_id = require_node(vm, &h2, item)?;
                    if !matches!(
                        h2.borrow().document.borrow().get(el_id),
                        Some(NodeData::Element(_))
                    ) {
                        return Err(vm.type_error("Expected Element in ARIA elements array"));
                    }
                    out.push(el_id);
                }
                out
            } else {
                let el_id = require_node(vm, &h2, v)?;
                if !matches!(
                    h2.borrow().document.borrow().get(el_id),
                    Some(NodeData::Element(_))
                ) {
                    return Err(vm.type_error("Expected Element for ARIA element reflection"));
                }
                vec![el_id]
            };
            // WHATWG HTML §2.6.10 Element Reflection: store explicitly set elements AND
            // set the reflected content attribute to `""` (triggers `[CEReactions]`).
            {
                let hb = h2.borrow();
                let mut ce = hb.custom_elements_state.borrow_mut();
                ce.element_aria_elements
                    .insert((nid, attr_name.to_string()), list);
                ce.suppressing_aria_clear = true;
            }
            set_element_attr(vm, &h2, nid, attr_name, "");
            h2.borrow()
                .custom_elements_state
                .borrow_mut()
                .suppressing_aria_clear = false;
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, ep, idl_name, Some(g), Some(s));
    }
}

/// Installs standard HTML reflecting IDL properties (string, nullable string, boolean,
/// signed/unsigned integer, floating-point) on built-in `HTML*Element` prototypes so
/// customized built-ins trigger `[CEReactions]` (`attributeChangedCallback`).
fn install_html_reflecting_idl_properties(vm: &mut JsVm, host: &SharedHost) {
    let install_str = |vm: &mut JsVm, proto: HeapHandle, idl: &'static str, attr: &'static str| {
        let h1 = Rc::clone(host);
        let g = vm.alloc_host_fn(&format!("get {idl}"), move |vm, t, _| {
            let nid = require_node(vm, &h1, t)?;
            let s = get_attr_string(&h1, nid, attr).unwrap_or_default();
            Ok(JsValue::from_string(vm.heap.alloc_string(s)))
        });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn(&format!("set {idl}"), move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            let val = crate::dom_parsing::to_dom_string(vm, arg(args, 0))?;
            set_element_attr(vm, &h2, nid, attr, &val);
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, proto, idl, Some(g), Some(s));
    };

    let install_nullable_str =
        |vm: &mut JsVm, proto: HeapHandle, idl: &'static str, attr: &'static str| {
            let h1 = Rc::clone(host);
            let g = vm.alloc_host_fn(&format!("get {idl}"), move |vm, t, _| {
                let nid = require_node(vm, &h1, t)?;
                Ok(get_attr_string(&h1, nid, attr).map_or(JsValue::NULL, |s| {
                    JsValue::from_string(vm.heap.alloc_string(s))
                }))
            });
            let h2 = Rc::clone(host);
            let s = vm.alloc_host_fn(&format!("set {idl}"), move |vm, t, args| {
                let nid = require_node(vm, &h2, t)?;
                let v = arg(args, 0);
                if v.is_null() {
                    remove_element_attr(vm, &h2, nid, attr);
                } else {
                    let val = crate::dom_parsing::to_dom_string(vm, v)?;
                    set_element_attr(vm, &h2, nid, attr, &val);
                }
                Ok(JsValue::UNDEFINED)
            });
            define_host_accessor(vm, proto, idl, Some(g), Some(s));
        };

    let install_bool = |vm: &mut JsVm, proto: HeapHandle, idl: &'static str, attr: &'static str| {
        let h1 = Rc::clone(host);
        let g =
            vm.alloc_host_fn(&format!("get {idl}"), move |vm, t, _| {
                let nid = require_node(vm, &h1, t)?;
                let present =
                    h1.borrow().document.borrow().get(nid).is_some_and(
                        |d| matches!(d, NodeData::Element(e) if e.has_attribute(attr)),
                    );
                Ok(JsValue::from_bool(present))
            });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn(&format!("set {idl}"), move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            if vm.to_boolean(arg(args, 0)) {
                set_element_attr(vm, &h2, nid, attr, "");
            } else {
                remove_element_attr(vm, &h2, nid, attr);
            }
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, proto, idl, Some(g), Some(s));
    };

    let install_uint = |vm: &mut JsVm,
                        proto: HeapHandle,
                        idl: &'static str,
                        attr: &'static str,
                        default_v: u32| {
        let h1 = Rc::clone(host);
        let g = vm.alloc_host_fn(&format!("get {idl}"), move |vm, t, _| {
            let nid = require_node(vm, &h1, t)?;
            let v = get_attr_string(&h1, nid, attr)
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(default_v);
            Ok(JsValue::from_f64(f64::from(v)))
        });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn(&format!("set {idl}"), move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            let n = vm.to_number(arg(args, 0))?;
            let v = if n.is_finite() {
                (n.trunc().rem_euclid(4_294_967_296.0)) as u32
            } else {
                0
            };
            set_element_attr(vm, &h2, nid, attr, &v.to_string());
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, proto, idl, Some(g), Some(s));
    };

    let install_float =
        |vm: &mut JsVm, proto: HeapHandle, idl: &'static str, attr: &'static str| {
            let h1 = Rc::clone(host);
            let g = vm.alloc_host_fn(&format!("get {idl}"), move |vm, t, _| {
                let nid = require_node(vm, &h1, t)?;
                let v = get_attr_string(&h1, nid, attr)
                    .and_then(|s| s.trim().parse::<f64>().ok())
                    .unwrap_or(0.0);
                Ok(JsValue::from_f64(v))
            });
            let h2 = Rc::clone(host);
            let s = vm.alloc_host_fn(&format!("set {idl}"), move |vm, t, args| {
                let nid = require_node(vm, &h2, t)?;
                let n = vm.to_number(arg(args, 0))?;
                if !n.is_finite() {
                    return Err(vm.type_error("Non-finite floating-point value"));
                }
                set_element_attr(vm, &h2, nid, attr, &n.to_string());
                Ok(JsValue::UNDEFINED)
            });
            define_host_accessor(vm, proto, idl, Some(g), Some(s));
        };

    let proto_of =
        |name: &str| -> Option<HeapHandle> { host.borrow().interface_protos.get(name).copied() };

    // `HTMLElement.prototype`
    let hep = host.borrow().prototypes.html_element_proto;
    for &(idl, attr) in &[
        ("accessKey", "accesskey"),
        ("autocapitalize", "autocapitalize"),
        ("enterKeyHint", "enterkeyhint"),
        ("inputMode", "inputmode"),
        ("nonce", "nonce"),
        ("writingSuggestions", "writingsuggestions"),
    ] {
        install_str(vm, hep, idl, attr);
    }
    install_nullable_str(vm, hep, "popover", "popover");
    for &(idl, attr) in &[("autofocus", "autofocus"), ("inert", "inert")] {
        install_bool(vm, hep, idl, attr);
    }
    // `HTMLElement.prototype.translate` (`yes` / `no`)
    {
        let h1 = Rc::clone(host);
        let g = vm.alloc_host_fn("get translate", move |vm, t, _| {
            let nid = require_node(vm, &h1, t)?;
            let v = !get_attr_string(&h1, nid, "translate")
                .is_some_and(|s| s.eq_ignore_ascii_case("no"));
            Ok(JsValue::from_bool(v))
        });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn("set translate", move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            let val = if vm.to_boolean(arg(args, 0)) {
                "yes"
            } else {
                "no"
            };
            set_element_attr(vm, &h2, nid, "translate", val);
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, hep, "translate", Some(g), Some(s));
    }
    // `HTMLElement.prototype.draggable` (`true` / `false`)
    {
        let h1 = Rc::clone(host);
        let g = vm.alloc_host_fn("get draggable", move |vm, t, _| {
            let nid = require_node(vm, &h1, t)?;
            let v = get_attr_string(&h1, nid, "draggable")
                .is_some_and(|s| s.eq_ignore_ascii_case("true"));
            Ok(JsValue::from_bool(v))
        });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn("set draggable", move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            let val = if vm.to_boolean(arg(args, 0)) {
                "true"
            } else {
                "false"
            };
            set_element_attr(vm, &h2, nid, "draggable", val);
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, hep, "draggable", Some(g), Some(s));
    }
    // `HTMLElement.prototype.spellcheck` (`true` / `false`)
    {
        let h1 = Rc::clone(host);
        let g = vm.alloc_host_fn("get spellcheck", move |vm, t, _| {
            let nid = require_node(vm, &h1, t)?;
            let v = !get_attr_string(&h1, nid, "spellcheck")
                .is_some_and(|s| s.eq_ignore_ascii_case("false"));
            Ok(JsValue::from_bool(v))
        });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn("set spellcheck", move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            let val = if vm.to_boolean(arg(args, 0)) {
                "true"
            } else {
                "false"
            };
            set_element_attr(vm, &h2, nid, "spellcheck", val);
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, hep, "spellcheck", Some(g), Some(s));
    }
    // `HTMLElement.prototype.contentEditable`
    {
        let h1 = Rc::clone(host);
        let g = vm.alloc_host_fn("get contentEditable", move |vm, t, _| {
            let nid = require_node(vm, &h1, t)?;
            let v = match get_attr_string(&h1, nid, "contenteditable")
                .map(|s| s.to_ascii_lowercase())
                .as_deref()
            {
                Some("" | "true") => "true",
                Some("false") => "false",
                Some("plaintext-only") => "plaintext-only",
                _ => "inherit",
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(v)))
        });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn("set contentEditable", move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            let v = crate::dom_parsing::to_dom_string(vm, arg(args, 0))?;
            if v.eq_ignore_ascii_case("inherit") {
                remove_element_attr(vm, &h2, nid, "contenteditable");
            } else if v.eq_ignore_ascii_case("true") {
                set_element_attr(vm, &h2, nid, "contenteditable", "true");
            } else if v.eq_ignore_ascii_case("false") {
                set_element_attr(vm, &h2, nid, "contenteditable", "false");
            } else if v.eq_ignore_ascii_case("plaintext-only") {
                set_element_attr(vm, &h2, nid, "contenteditable", "plaintext-only");
            } else {
                return Err(create_dom_exception(
                    vm,
                    "SyntaxError",
                    "Invalid contentEditable value",
                ));
            }
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, hep, "contentEditable", Some(g), Some(s));
    }

    // `HTMLTemplateElement.prototype.shadowRootCustomElementRegistry`
    if let Some(tp) = proto_of("HTMLTemplateElement") {
        install_str(
            vm,
            tp,
            "shadowRootCustomElementRegistry",
            "shadowrootcustomelementregistry",
        );
    }

    // `HTMLMediaElement.prototype`
    let mp = host.borrow().prototypes.html_media_element_proto;
    for &(idl, attr) in &[("src", "src"), ("preload", "preload")] {
        install_str(vm, mp, idl, attr);
    }
    install_nullable_str(vm, mp, "crossOrigin", "crossorigin");
    for &(idl, attr) in &[
        ("autoplay", "autoplay"),
        ("loop", "loop"),
        ("controls", "controls"),
        ("defaultMuted", "muted"),
    ] {
        install_bool(vm, mp, idl, attr);
    }

    // `HTMLImageElement.prototype`
    if let Some(p) = proto_of("HTMLImageElement") {
        for &(idl, attr) in &[
            ("alt", "alt"),
            ("src", "src"),
            ("srcset", "srcset"),
            ("sizes", "sizes"),
            ("useMap", "usemap"),
            ("referrerPolicy", "referrerpolicy"),
            ("decoding", "decoding"),
            ("loading", "loading"),
            ("fetchPriority", "fetchpriority"),
            ("align", "align"),
            ("longDesc", "longdesc"),
            ("border", "border"),
            ("lowsrc", "lowsrc"),
            ("name", "name"),
        ] {
            install_str(vm, p, idl, attr);
        }
        install_nullable_str(vm, p, "crossOrigin", "crossorigin");
        install_bool(vm, p, "isMap", "ismap");
        install_uint(vm, p, "width", "width", 0);
        install_uint(vm, p, "height", "height", 0);
        install_uint(vm, p, "hspace", "hspace", 0);
        install_uint(vm, p, "vspace", "vspace", 0);
        // `HTMLImageElement.prototype.decode()` (HTML §4.8.4.3.7)
        define_host_method(vm, p, "decode", move |vm, _, _| {
            let p = rix_js::promise::promise_resolve_value(vm, JsValue::UNDEFINED);
            Ok(JsValue::from_object(p))
        });
    }

    // `HTMLAreaElement.prototype`
    if let Some(p) = proto_of("HTMLAreaElement") {
        for &(idl, attr) in &[
            ("alt", "alt"),
            ("coords", "coords"),
            ("shape", "shape"),
            ("target", "target"),
            ("download", "download"),
            ("ping", "ping"),
            ("rel", "rel"),
            ("referrerPolicy", "referrerpolicy"),
        ] {
            install_str(vm, p, idl, attr);
        }
        install_bool(vm, p, "noHref", "nohref");
    }

    // `HTMLButtonElement.prototype`
    if let Some(p) = proto_of("HTMLButtonElement") {
        for &(idl, attr) in &[
            ("formAction", "formaction"),
            ("formEnctype", "formenctype"),
            ("formMethod", "formmethod"),
            ("formTarget", "formtarget"),
            ("name", "name"),
            ("type", "type"),
            ("value", "value"),
            ("popoverTargetAction", "popovertargetaction"),
        ] {
            install_str(vm, p, idl, attr);
        }
        for &(idl, attr) in &[
            ("autofocus", "autofocus"),
            ("disabled", "disabled"),
            ("formNoValidate", "formnovalidate"),
        ] {
            install_bool(vm, p, idl, attr);
        }
    }

    // `HTMLTableCellElement.prototype`
    if let Some(p) = proto_of("HTMLTableCellElement") {
        for &(idl, attr) in &[
            ("headers", "headers"),
            ("scope", "scope"),
            ("abbr", "abbr"),
            ("align", "align"),
            ("axis", "axis"),
            ("ch", "char"),
            ("chOff", "charoff"),
            ("vAlign", "valign"),
            ("bgColor", "bgcolor"),
            ("height", "height"),
            ("width", "width"),
        ] {
            install_str(vm, p, idl, attr);
        }
        install_bool(vm, p, "noWrap", "nowrap");
        install_uint(vm, p, "colSpan", "colspan", 1);
        install_uint(vm, p, "rowSpan", "rowspan", 1);
    }

    // `HTMLMeterElement.prototype`
    if let Some(p) = proto_of("HTMLMeterElement") {
        for &(idl, attr) in &[
            ("value", "value"),
            ("min", "min"),
            ("max", "max"),
            ("low", "low"),
            ("high", "high"),
            ("optimum", "optimum"),
        ] {
            install_float(vm, p, idl, attr);
        }
    }

    // `HTMLModElement.prototype`
    if let Some(p) = proto_of("HTMLModElement") {
        install_str(vm, p, "cite", "cite");
        install_str(vm, p, "dateTime", "datetime");
    }

    // `HTMLSourceElement.prototype`
    if let Some(p) = proto_of("HTMLSourceElement") {
        for &(idl, attr) in &[
            ("src", "src"),
            ("type", "type"),
            ("srcset", "srcset"),
            ("sizes", "sizes"),
            ("media", "media"),
        ] {
            install_str(vm, p, idl, attr);
        }
        install_uint(vm, p, "width", "width", 0);
        install_uint(vm, p, "height", "height", 0);
    }

    // `HTMLFieldSetElement.prototype.{disabled, name, elements}`
    if let Some(p) = proto_of("HTMLFieldSetElement") {
        install_bool(vm, p, "disabled", "disabled");
        install_str(vm, p, "name", "name");
        let h1 = Rc::clone(host);
        let g_elems = vm.alloc_host_fn("get elements", move |vm, t, _| {
            let nid = require_node(vm, &h1, t)?;
            let controls: Vec<NodeId> = {
                let hb = h1.borrow();
                let doc = hb.document.borrow();
                let ce = hb.custom_elements_state.borrow();
                doc.descendants(nid)
                    .filter(|&c| {
                        if c == nid {
                            return false;
                        }
                        if let Some(NodeData::Element(e)) = doc.get(c) {
                            let tag = e.name.local.as_str();
                            let is_builtin = matches!(
                                tag,
                                "button"
                                    | "fieldset"
                                    | "input"
                                    | "object"
                                    | "output"
                                    | "select"
                                    | "textarea"
                            );
                            let is_fa = ce
                                .element_definitions
                                .get(&c)
                                .and_then(|(rid, n)| ce.registries.get(rid)?.by_name.get(n))
                                .is_some_and(|d| d.form_associated);
                            is_builtin || is_fa
                        } else {
                            false
                        }
                    })
                    .collect()
            };
            let arr = vm.alloc_array();
            for c in controls {
                let w = wrap_node(vm, &h1, c);
                vm.heap.object_push_element(arr, w);
            }
            Ok(JsValue::from_object(arr))
        });
        define_host_accessor(vm, p, "elements", Some(g_elems), None);
    }

    // `HTMLFormElement.prototype.reset()` (resets built-in controls then fires `formResetCallback`)
    if let Some(p) = proto_of("HTMLFormElement") {
        let h1 = Rc::clone(host);
        define_host_method(vm, p, "reset", move |vm, t, _| {
            let form_id = require_node(vm, &h1, t)?;
            let candidates: Vec<(NodeId, bool)> = {
                let hb = h1.borrow();
                let doc = hb.document.borrow();
                let ce = hb.custom_elements_state.borrow();
                let root = crate::shadow_dom::tree_root(&doc, form_id);
                doc.descendants(root)
                    .filter_map(|cand| {
                        let NodeData::Element(e) = doc.get(cand)? else {
                            return None;
                        };
                        let is_fa = ce
                            .element_definitions
                            .get(&cand)
                            .and_then(|(rid, n)| ce.registries.get(rid)?.by_name.get(n))
                            .is_some_and(|d| d.form_associated);
                        if is_fa {
                            return Some((cand, true));
                        }
                        let tag = e.name.local.as_str();
                        if matches!(tag, "input" | "textarea" | "select" | "output") {
                            return Some((cand, false));
                        }
                        None
                    })
                    .collect()
            };
            let mut face_nodes = Vec::new();
            for (cand, is_fa) in candidates {
                if find_form_owner(&h1, cand) != Some(form_id) {
                    continue;
                }
                if is_fa {
                    face_nodes.push(cand);
                } else {
                    // Reset built-in form control before custom element `formResetCallback` runs.
                    let default_val: Option<String> = {
                        let hb = h1.borrow();
                        let doc = hb.document.borrow();
                        match doc.get(cand) {
                            Some(NodeData::Element(e))
                                if e.name.local.as_str().eq_ignore_ascii_case("output") =>
                            {
                                Some(doc.inner_text(cand))
                            }
                            Some(NodeData::Element(e)) => {
                                e.get_attribute("value").map(str::to_string)
                            }
                            _ => None,
                        }
                    };
                    if let Some(dv) = default_val {
                        let w = wrap_node(vm, &h1, cand);
                        if let Some(wh) = w.as_object() {
                            let kv = vm.heap.intern_key("value");
                            let v = JsValue::from_string(vm.heap.alloc_string(dv));
                            let _ = vm.set_property_by_key(wh, kv, v);
                        }
                    }
                }
            }
            crate::custom_elements::with_ce_reactions(vm, &h1, |_vm| {
                for fa in face_nodes {
                    crate::custom_elements::enqueue_reaction(
                        &h1,
                        crate::custom_elements::CeReaction::FormReset(fa),
                    );
                }
                Ok(())
            })?;
            Ok(JsValue::UNDEFINED)
        });
    }
}
