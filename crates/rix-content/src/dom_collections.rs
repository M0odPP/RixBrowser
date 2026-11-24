//! WHATWG DOM Standard `Attr`, `NamedNodeMap`, live `DOMTokenList`, live `HTMLCollection`,
//! live & static `NodeList`, and `TreeWalker`/`NodeIterator`/`NodeFilter` traversal APIs.

use std::rc::Rc;

use rix_dom::{Document, NodeData, NodeId};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::dom_bridge::{
    arg, define_global, define_host_accessor, define_host_method, get_attr_string, require_node,
    set_element_attr, wrap_node, DomMutation, SharedHost,
};
use crate::dom_interfaces::{
    define_to_string_tag, is_ascii_ws, to_webidl_u32, AttrEntry, HtmlCollectionKind,
    TokenListEntry, TraversalEntry, HTML_NS, MATHML_NS,
};
use crate::web_apis::create_dom_exception;

/// Installs all `Attr`/`NamedNodeMap`, `DOMTokenList`, `HTMLCollection`/`NodeList`,
/// and `TreeWalker`/`NodeIterator`/`NodeFilter` prototypes and element accessors.
pub fn install_collections_and_traversal(vm: &mut JsVm, host: &SharedHost) {
    install_attr_and_named_node_map_prototypes(vm, host);
    install_dom_token_list_prototype(vm, host);
    install_collection_prototypes(vm, host);
    install_traversal_apis(vm, host);
    install_element_token_list_attributes(vm, host);
}

// ---------------------------------------------------------------------------
// Attr & NamedNodeMap (WHATWG DOM §4.9)
// ---------------------------------------------------------------------------

pub fn alloc_attr_object(vm: &mut JsVm, host: &SharedHost, entry: AttrEntry) -> HeapHandle {
    let proto = host.borrow().prototypes.attr_proto;
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    host.borrow_mut().attrs.insert(obj, entry);
    obj
}

pub fn get_or_create_element_attr_handle(
    vm: &mut JsVm,
    host: &SharedHost,
    elem: NodeId,
    ns: &str,
    prefix: Option<String>,
    local: &str,
) -> HeapHandle {
    let key = (elem, ns.to_string(), local.to_string());
    let existing = host.borrow().elem_attr_handles.get(&key).copied();
    if let Some(h) = existing {
        if vm.heap.get_object(h).is_some() {
            if let Some(e) = host.borrow_mut().attrs.get_mut(&h) {
                e.prefix = prefix;
                e.owner_element = Some(elem);
            }
            return h;
        }
    }
    let owner_doc = host.borrow().owner_doc_of(elem);
    let h = alloc_attr_object(
        vm,
        host,
        AttrEntry {
            owner_element: Some(elem),
            owner_doc,
            namespace: ns.to_string(),
            prefix,
            local_name: local.to_string(),
            detached_value: String::new(),
        },
    );
    host.borrow_mut().elem_attr_handles.insert(key, h);
    h
}

fn read_attr_value(host: &SharedHost, entry: &AttrEntry) -> String {
    if let Some(elem) = entry.owner_element {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        if let Some(NodeData::Element(e)) = doc.get(elem) {
            if let Some(v) = e.get_attribute_ns(&entry.namespace, &entry.local_name) {
                return v.to_string();
            }
        }
    }
    entry.detached_value.clone()
}

fn write_attr_value(vm: &mut JsVm, host: &SharedHost, attr_h: HeapHandle, new_val: &str) {
    let info = host.borrow().attrs.get(&attr_h).cloned();
    let Some(entry) = info else {
        return;
    };
    if let Some(elem) = entry.owner_element {
        let old_val = {
            let doc_rc = Rc::clone(&host.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            match doc.get_mut(elem) {
                Some(NodeData::Element(e)) => e
                    .set_attribute_ns(
                        &entry.namespace,
                        entry.prefix.as_deref(),
                        &entry.local_name,
                        new_val,
                    )
                    .map(|(old, _)| old),
                _ => None,
            }
        };
        host.borrow_mut().invalidate_attr_mutation(
            elem,
            &entry.local_name,
            old_val.as_deref(),
            Some(new_val),
        );
        host.borrow().emit_mutation(DomMutation::Attributes {
            target: elem,
            attribute_name: entry.local_name.clone(),
            attribute_namespace: if entry.namespace.is_empty() {
                None
            } else {
                Some(entry.namespace.clone())
            },
            old_value: old_val,
        });
        let _ = vm;
    } else if let Some(e) = host.borrow_mut().attrs.get_mut(&attr_h) {
        e.detached_value = new_val.to_string();
    }
}

/// Creates a live `NamedNodeMap` Proxy for `elem.attributes`.
pub fn create_named_node_map_proxy(vm: &mut JsVm, host: &SharedHost, elem: NodeId) -> JsValue {
    if let Some(&existing) = host.borrow().named_node_maps.get(&elem) {
        if vm.heap.get_object(existing).is_some() {
            return JsValue::from_object(existing);
        }
    }
    let proto = host.borrow().prototypes.named_node_map_proto;
    let target = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let handler = vm.alloc_ordinary_object();

    let h_get = Rc::clone(host);
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let target_h = arg(args, 0).as_object();
        let prop_val = arg(args, 1);
        let receiver = arg(args, 2);
        if prop_val.is_symbol() {
            if let Some(th) = target_h {
                let ks = vm.to_property_key(prop_val)?;
                let k = vm.heap.intern_key(&ks);
                return vm.get_property_with_receiver(th, k, receiver);
            }
            return Ok(JsValue::UNDEFINED);
        }
        let key_str = vm.value_to_rust_string(prop_val);
        if let Ok(idx) = key_str.parse::<usize>() {
            if idx.to_string() == key_str {
                if let Some(attr) = named_node_map_item(vm, &h_get, elem, idx) {
                    return Ok(attr);
                }
            }
        }
        if let Some(th) = target_h {
            let k = vm.heap.intern_key(&key_str);
            if vm.has_property_by_key(th, k)? {
                return vm.get_property_with_receiver(th, k, receiver);
            }
        }
        if let Some(attr) = named_node_map_get_named(vm, &h_get, elem, &key_str) {
            return Ok(attr);
        }
        Ok(JsValue::UNDEFINED)
    });

    let h_has = Rc::clone(host);
    define_host_method(vm, handler, "has", move |vm, _, args| {
        let target_h = arg(args, 0).as_object();
        let prop_val = arg(args, 1);
        if prop_val.is_symbol() {
            if let Some(th) = target_h {
                let ks = vm.to_property_key(prop_val)?;
                let k = vm.heap.intern_key(&ks);
                return Ok(JsValue::from_bool(vm.has_property_by_key(th, k)?));
            }
            return Ok(JsValue::FALSE);
        }
        let key_str = vm.value_to_rust_string(prop_val);
        if let Ok(idx) = key_str.parse::<usize>() {
            if idx.to_string() == key_str && idx < elem_attr_count(&h_has, elem) {
                return Ok(JsValue::TRUE);
            }
        }
        if let Some(th) = target_h {
            let k = vm.heap.intern_key(&key_str);
            if vm.has_property_by_key(th, k)? {
                return Ok(JsValue::TRUE);
            }
        }
        Ok(JsValue::from_bool(
            elem_find_attr_by_qname(&h_has, elem, &key_str).is_some(),
        ))
    });

    let h_keys = Rc::clone(host);
    define_host_method(vm, handler, "ownKeys", move |vm, _, _| {
        let (count, names) = {
            let doc_rc = Rc::clone(&h_keys.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(elem) {
                Some(NodeData::Element(e)) => {
                    let mut names = Vec::new();
                    for (q, _) in &e.attributes {
                        let qn = match &q.prefix {
                            Some(p) if !p.is_empty() => format!("{p}:{}", q.local),
                            _ => q.local.to_string(),
                        };
                        if !names.contains(&qn) {
                            names.push(qn);
                        }
                    }
                    (e.attributes.len(), names)
                }
                _ => (0, Vec::new()),
            }
        };
        let arr = vm.alloc_array();
        for i in 0..count {
            let s = JsValue::from_string(vm.heap.alloc_string(i.to_string()));
            vm.heap.object_push_element(arr, s);
        }
        for name in names {
            let s = JsValue::from_string(vm.heap.alloc_string(name));
            vm.heap.object_push_element(arr, s);
        }
        Ok(JsValue::from_object(arr))
    });

    let h_desc = Rc::clone(host);
    define_host_method(
        vm,
        handler,
        "getOwnPropertyDescriptor",
        move |vm, _, args| {
            let prop_val = arg(args, 1);
            if prop_val.is_symbol() {
                return Ok(JsValue::UNDEFINED);
            }
            let key_str = vm.value_to_rust_string(prop_val);
            let found = if let Ok(idx) = key_str.parse::<usize>() {
                if idx.to_string() == key_str {
                    named_node_map_item(vm, &h_desc, elem, idx).map(|v| (v, true))
                } else {
                    None
                }
            } else {
                named_node_map_get_named(vm, &h_desc, elem, &key_str).map(|v| (v, false))
            };
            let Some((val, enumerable)) = found else {
                return Ok(JsValue::UNDEFINED);
            };
            let desc = vm.alloc_ordinary_object();
            for (k_s, v) in [
                ("value", val),
                ("writable", JsValue::FALSE),
                ("enumerable", JsValue::from_bool(enumerable)),
                ("configurable", JsValue::TRUE),
            ] {
                let k = vm.heap.intern_key(k_s);
                vm.heap
                    .object_define_data(desc, k, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
            }
            Ok(JsValue::from_object(desc))
        },
    );

    let proxy = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(rix_js::ProxyObject {
            target,
            handler,
            revoked: false,
        }),
        Some(proto),
    ));
    host.borrow_mut().named_node_maps.insert(elem, proxy);
    host.borrow_mut().named_node_map_to_elem.insert(proxy, elem);
    host.borrow_mut()
        .named_node_map_to_elem
        .insert(target, elem);
    JsValue::from_object(proxy)
}

fn elem_attr_count(host: &SharedHost, elem: NodeId) -> usize {
    let doc_rc = Rc::clone(&host.borrow().document);
    let doc = doc_rc.borrow();
    match doc.get(elem) {
        Some(NodeData::Element(e)) => e.attributes.len(),
        _ => 0,
    }
}

fn elem_find_attr_by_qname(
    host: &SharedHost,
    elem: NodeId,
    qname: &str,
) -> Option<(String, Option<String>, String)> {
    let is_html_doc = !host.borrow().is_xml_doc_for_node(elem);
    let doc_rc = Rc::clone(&host.borrow().document);
    let doc = doc_rc.borrow();
    let Some(NodeData::Element(e)) = doc.get(elem) else {
        return None;
    };
    let fold = is_html_doc && &*e.name.ns == HTML_NS;
    let query = if fold {
        qname.to_ascii_lowercase()
    } else {
        qname.to_string()
    };
    let (idx, _) = e.get_attribute_by_qname(&query)?;
    let (q, _) = &e.attributes[idx];
    Some((
        q.ns.to_string(),
        q.prefix.as_ref().map(ToString::to_string),
        q.local.to_string(),
    ))
}

fn named_node_map_item(
    vm: &mut JsVm,
    host: &SharedHost,
    elem: NodeId,
    idx: usize,
) -> Option<JsValue> {
    let spec = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        match doc.get(elem) {
            Some(NodeData::Element(e)) => e.attributes.get(idx).map(|(q, _)| {
                (
                    q.ns.to_string(),
                    q.prefix.as_ref().map(ToString::to_string),
                    q.local.to_string(),
                )
            }),
            _ => None,
        }
    }?;
    let h = get_or_create_element_attr_handle(vm, host, elem, &spec.0, spec.1, &spec.2);
    Some(JsValue::from_object(h))
}

fn named_node_map_get_named(
    vm: &mut JsVm,
    host: &SharedHost,
    elem: NodeId,
    qname: &str,
) -> Option<JsValue> {
    let (ns, prefix, local) = elem_find_attr_by_qname(host, elem, qname)?;
    let h = get_or_create_element_attr_handle(vm, host, elem, &ns, prefix, &local);
    Some(JsValue::from_object(h))
}

/// Shared implementation of `Element.setAttributeNode` / `NamedNodeMap.setNamedItem`.
pub fn set_attribute_node_on_element(
    vm: &mut JsVm,
    host: &SharedHost,
    elem: NodeId,
    attr_val: JsValue,
) -> Result<JsValue, JsException> {
    let Some(attr_h) = attr_val.as_object() else {
        return Err(vm.type_error("Argument must be an Attr"));
    };
    let Some(entry) = host.borrow().attrs.get(&attr_h).cloned() else {
        return Err(vm.type_error("Argument must be an Attr"));
    };
    if let Some(cur_owner) = entry.owner_element {
        if cur_owner == elem {
            return Ok(attr_val);
        }
        return Err(create_dom_exception(
            vm,
            "InUseAttributeError",
            "Attribute is already in use by another element",
        ));
    }
    let val = entry.detached_value.clone();
    let old_attr_handle = {
        let key = (elem, entry.namespace.clone(), entry.local_name.clone());
        let existing_handle = host.borrow().elem_attr_handles.get(&key).copied();
        let existing_in_dom = {
            let doc_rc = Rc::clone(&host.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(elem) {
                Some(NodeData::Element(e)) => e
                    .attributes
                    .iter()
                    .find(|(q, _)| *q.ns == entry.namespace && *q.local == entry.local_name)
                    .map(|(q, v)| {
                        (
                            q.ns.to_string(),
                            q.prefix.as_ref().map(ToString::to_string),
                            q.local.to_string(),
                            v.clone(),
                        )
                    }),
                _ => None,
            }
        };
        if let Some((old_ns, old_pref, old_local, old_val_s)) = existing_in_dom {
            let old_h = existing_handle
                .filter(|&oh| vm.heap.get_object(oh).is_some())
                .unwrap_or_else(|| {
                    let owner_doc = host.borrow().owner_doc_of(elem);
                    alloc_attr_object(
                        vm,
                        host,
                        AttrEntry {
                            owner_element: None,
                            owner_doc,
                            namespace: old_ns,
                            prefix: old_pref,
                            local_name: old_local,
                            detached_value: old_val_s.clone(),
                        },
                    )
                });
            if let Some(old_entry) = host.borrow_mut().attrs.get_mut(&old_h) {
                old_entry.owner_element = None;
                old_entry.detached_value = old_val_s;
            }
            Some(old_h)
        } else {
            None
        }
    };

    let old_val = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = doc_rc.borrow_mut();
        match doc.get_mut(elem) {
            Some(NodeData::Element(e)) => e
                .set_attribute_ns(
                    &entry.namespace,
                    entry.prefix.as_deref(),
                    &entry.local_name,
                    &val,
                )
                .map(|(old, _)| old),
            _ => None,
        }
    };
    let elem_doc = host.borrow().owner_doc_of(elem);
    {
        let mut hb = host.borrow_mut();
        if let Some(e) = hb.attrs.get_mut(&attr_h) {
            e.owner_element = Some(elem);
            e.owner_doc = elem_doc;
        }
        hb.elem_attr_handles.insert(
            (elem, entry.namespace.clone(), entry.local_name.clone()),
            attr_h,
        );
        hb.invalidate_attr_mutation(elem, &entry.local_name, old_val.as_deref(), Some(&val));
    }
    host.borrow().emit_mutation(DomMutation::Attributes {
        target: elem,
        attribute_name: entry.local_name,
        attribute_namespace: if entry.namespace.is_empty() {
            None
        } else {
            Some(entry.namespace)
        },
        old_value: old_val,
    });
    Ok(old_attr_handle.map_or(JsValue::NULL, JsValue::from_object))
}

/// Shared implementation of `Element.removeAttributeNode` / `NamedNodeMap.removeNamedItem`.
pub fn remove_attribute_node_from_element(
    vm: &mut JsVm,
    host: &SharedHost,
    elem: NodeId,
    ns: &str,
    local: &str,
) -> Result<JsValue, JsException> {
    let existing = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        match doc.get(elem) {
            Some(NodeData::Element(e)) => e
                .attributes
                .iter()
                .find(|(q, _)| &*q.ns == ns && &*q.local == local)
                .map(|(q, v)| (q.prefix.as_ref().map(ToString::to_string), v.clone())),
            _ => None,
        }
    };
    let Some((prefix, old_val)) = existing else {
        return Err(create_dom_exception(
            vm,
            "NotFoundError",
            "The attribute was not found on this element",
        ));
    };
    let attr_h = get_or_create_element_attr_handle(vm, host, elem, ns, prefix, local);
    {
        let doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = doc_rc.borrow_mut();
        if let Some(NodeData::Element(e)) = doc.get_mut(elem) {
            e.remove_attribute_ns(ns, local);
        }
    }
    {
        let mut hb = host.borrow_mut();
        hb.elem_attr_handles
            .remove(&(elem, ns.to_string(), local.to_string()));
        if let Some(e) = hb.attrs.get_mut(&attr_h) {
            e.owner_element = None;
            e.detached_value = old_val.clone();
        }
        hb.invalidate_attr_mutation(elem, local, Some(&old_val), None);
    }
    host.borrow().emit_mutation(DomMutation::Attributes {
        target: elem,
        attribute_name: local.to_string(),
        attribute_namespace: if ns.is_empty() {
            None
        } else {
            Some(ns.to_string())
        },
        old_value: Some(old_val),
    });
    Ok(JsValue::from_object(attr_h))
}

fn install_attr_and_named_node_map_prototypes(vm: &mut JsVm, host: &SharedHost) {
    let ap = host.borrow().prototypes.attr_proto;
    let nnmp = host.borrow().prototypes.named_node_map_proto;

    // Attr.prototype getters/setters
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get namespaceURI", move |vm, t, _| {
        let Some(ah) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation: receiver is not Attr"));
        };
        let ns = h
            .borrow()
            .attrs
            .get(&ah)
            .map(|e| e.namespace.clone())
            .ok_or_else(|| vm.type_error("Illegal invocation: receiver is not Attr"))?;
        Ok(if ns.is_empty() {
            JsValue::NULL
        } else {
            JsValue::from_string(vm.heap.alloc_string(ns))
        })
    });
    define_host_accessor(vm, ap, "namespaceURI", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get prefix", move |vm, t, _| {
        let Some(ah) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation: receiver is not Attr"));
        };
        let pref = h
            .borrow()
            .attrs
            .get(&ah)
            .map(|e| e.prefix.clone())
            .ok_or_else(|| vm.type_error("Illegal invocation: receiver is not Attr"))?;
        Ok(pref.map_or(JsValue::NULL, |s| {
            JsValue::from_string(vm.heap.alloc_string(s))
        }))
    });
    define_host_accessor(vm, ap, "prefix", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get localName", move |vm, t, _| {
        let Some(ah) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation: receiver is not Attr"));
        };
        let local = h
            .borrow()
            .attrs
            .get(&ah)
            .map(|e| e.local_name.clone())
            .ok_or_else(|| vm.type_error("Illegal invocation: receiver is not Attr"))?;
        Ok(JsValue::from_string(vm.heap.alloc_string(local)))
    });
    define_host_accessor(vm, ap, "localName", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get name", move |vm, t, _| {
        let Some(ah) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation: receiver is not Attr"));
        };
        let qn = h
            .borrow()
            .attrs
            .get(&ah)
            .map(AttrEntry::qualified_name)
            .ok_or_else(|| vm.type_error("Illegal invocation: receiver is not Attr"))?;
        Ok(JsValue::from_string(vm.heap.alloc_string(qn)))
    });
    define_host_accessor(vm, ap, "name", Some(g), None);

    let h1 = Rc::clone(host);
    let g_val = vm.alloc_host_fn("get value", move |vm, t, _| {
        let Some(ah) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation: receiver is not Attr"));
        };
        let entry = h1
            .borrow()
            .attrs
            .get(&ah)
            .cloned()
            .ok_or_else(|| vm.type_error("Illegal invocation: receiver is not Attr"))?;
        let val = read_attr_value(&h1, &entry);
        Ok(JsValue::from_string(vm.heap.alloc_string(val)))
    });
    let h2 = Rc::clone(host);
    let s_val = vm.alloc_host_fn("set value", move |vm, t, args| {
        let Some(ah) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation: receiver is not Attr"));
        };
        if !h2.borrow().attrs.contains_key(&ah) {
            return Err(vm.type_error("Illegal invocation: receiver is not Attr"));
        }
        let v = vm.value_to_rust_string(arg(args, 0));
        write_attr_value(vm, &h2, ah, &v);
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, ap, "value", Some(g_val), Some(s_val));

    let h = Rc::clone(host);
    let g_oe = vm.alloc_host_fn("get ownerElement", move |vm, t, _| {
        let Some(ah) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation: receiver is not Attr"));
        };
        let oe = h
            .borrow()
            .attrs
            .get(&ah)
            .map(|e| e.owner_element)
            .ok_or_else(|| vm.type_error("Illegal invocation: receiver is not Attr"))?;
        Ok(oe.map_or(JsValue::NULL, |nid| wrap_node(vm, &h, nid)))
    });
    define_host_accessor(vm, ap, "ownerElement", Some(g_oe), None);

    let g_spec = vm.alloc_host_fn("get specified", move |_vm, _, _| Ok(JsValue::TRUE));
    define_host_accessor(vm, ap, "specified", Some(g_spec), None);

    // Override Node getters on Attr.prototype for Attr nodes (`nodeType=2`, `nodeName`, `nodeValue`, `textContent`, `ownerDocument`, `parentNode`, `childNodes`, `cloneNode`)
    let g_nt = vm.alloc_host_fn("get nodeType", move |_vm, _, _| Ok(JsValue::from_i32(2)));
    define_host_accessor(vm, ap, "nodeType", Some(g_nt), None);

    let h = Rc::clone(host);
    let g_nn = vm.alloc_host_fn("get nodeName", move |vm, t, _| {
        let Some(ah) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let qn = h
            .borrow()
            .attrs
            .get(&ah)
            .map(AttrEntry::qualified_name)
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(JsValue::from_string(vm.heap.alloc_string(qn)))
    });
    define_host_accessor(vm, ap, "nodeName", Some(g_nn), None);

    let h1 = Rc::clone(host);
    let g_nv = vm.alloc_host_fn("get nodeValue", move |vm, t, _| {
        let Some(ah) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let entry = h1
            .borrow()
            .attrs
            .get(&ah)
            .cloned()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let val = read_attr_value(&h1, &entry);
        Ok(JsValue::from_string(vm.heap.alloc_string(val)))
    });
    let h2 = Rc::clone(host);
    let s_nv = vm.alloc_host_fn("set nodeValue", move |vm, t, args| {
        let Some(ah) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let v = if arg(args, 0).is_null() {
            String::new()
        } else {
            vm.value_to_rust_string(arg(args, 0))
        };
        write_attr_value(vm, &h2, ah, &v);
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, ap, "nodeValue", Some(g_nv), Some(s_nv));
    define_host_accessor(vm, ap, "textContent", Some(g_nv), Some(s_nv));

    let h = Rc::clone(host);
    let g_od = vm.alloc_host_fn("get ownerDocument", move |vm, t, _| {
        let Some(ah) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let od = h
            .borrow()
            .attrs
            .get(&ah)
            .map(|e| e.owner_doc)
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(wrap_node(vm, &h, od))
    });
    define_host_accessor(vm, ap, "ownerDocument", Some(g_od), None);

    let g_null = vm.alloc_host_fn("get null", move |_vm, _, _| Ok(JsValue::NULL));
    for prop in [
        "parentNode",
        "parentElement",
        "firstChild",
        "lastChild",
        "previousSibling",
        "nextSibling",
    ] {
        define_host_accessor(vm, ap, prop, Some(g_null), None);
    }

    let h = Rc::clone(host);
    define_host_method(vm, ap, "cloneNode", move |vm, t, _| {
        let Some(ah) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let entry = h
            .borrow()
            .attrs
            .get(&ah)
            .cloned()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let cur_val = read_attr_value(&h, &entry);
        let cloned = alloc_attr_object(
            vm,
            &h,
            AttrEntry {
                owner_element: None,
                owner_doc: entry.owner_doc,
                namespace: entry.namespace,
                prefix: entry.prefix,
                local_name: entry.local_name,
                detached_value: cur_val,
            },
        );
        Ok(JsValue::from_object(cloned))
    });

    // NamedNodeMap.prototype methods
    let require_nnm_elem = |host: &SharedHost, this_val: JsValue| -> Option<NodeId> {
        let h = this_val.as_object()?;
        host.borrow().named_node_map_to_elem.get(&h).copied()
    };

    let h = Rc::clone(host);
    let g_len = vm.alloc_host_fn("get length", move |vm, t, _| {
        let Some(elem) = require_nnm_elem(&h, t) else {
            return Err(vm.type_error("Illegal invocation: receiver is not NamedNodeMap"));
        };
        Ok(JsValue::from_f64(elem_attr_count(&h, elem) as f64))
    });
    define_host_accessor(vm, nnmp, "length", Some(g_len), None);

    let h = Rc::clone(host);
    define_host_method(vm, nnmp, "item", move |vm, t, args| {
        let Some(elem) = require_nnm_elem(&h, t) else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let idx = to_webidl_u32(vm, arg(args, 0))?;
        Ok(named_node_map_item(vm, &h, elem, idx).unwrap_or(JsValue::NULL))
    });

    let h = Rc::clone(host);
    define_host_method(vm, nnmp, "getNamedItem", move |vm, t, args| {
        let Some(elem) = require_nnm_elem(&h, t) else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let qn = vm.value_to_rust_string(arg(args, 0));
        Ok(named_node_map_get_named(vm, &h, elem, &qn).unwrap_or(JsValue::NULL))
    });

    let h = Rc::clone(host);
    define_host_method(vm, nnmp, "getNamedItemNS", move |vm, t, args| {
        let Some(elem) = require_nnm_elem(&h, t) else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let ns_arg = arg(args, 0);
        let ns = if ns_arg.is_null() || ns_arg.is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(ns_arg)
        };
        let local = vm.value_to_rust_string(arg(args, 1));
        let found = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(elem) {
                Some(NodeData::Element(e)) => e
                    .attributes
                    .iter()
                    .find(|(q, _)| *q.ns == ns && *q.local == local)
                    .map(|(q, _)| q.prefix.as_ref().map(ToString::to_string)),
                _ => None,
            }
        };
        Ok(match found {
            Some(pref) => JsValue::from_object(get_or_create_element_attr_handle(
                vm, &h, elem, &ns, pref, &local,
            )),
            None => JsValue::NULL,
        })
    });

    let h = Rc::clone(host);
    define_host_method(vm, nnmp, "setNamedItem", move |vm, t, args| {
        let Some(elem) = require_nnm_elem(&h, t) else {
            return Err(vm.type_error("Illegal invocation"));
        };
        set_attribute_node_on_element(vm, &h, elem, arg(args, 0))
    });
    let h = Rc::clone(host);
    define_host_method(vm, nnmp, "setNamedItemNS", move |vm, t, args| {
        let Some(elem) = require_nnm_elem(&h, t) else {
            return Err(vm.type_error("Illegal invocation"));
        };
        set_attribute_node_on_element(vm, &h, elem, arg(args, 0))
    });

    let h = Rc::clone(host);
    define_host_method(vm, nnmp, "removeNamedItem", move |vm, t, args| {
        let Some(elem) = require_nnm_elem(&h, t) else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let qn = vm.value_to_rust_string(arg(args, 0));
        let Some((ns, _, local)) = elem_find_attr_by_qname(&h, elem, &qn) else {
            return Err(create_dom_exception(
                vm,
                "NotFoundError",
                "Named item not found",
            ));
        };
        remove_attribute_node_from_element(vm, &h, elem, &ns, &local)
    });

    let h = Rc::clone(host);
    define_host_method(vm, nnmp, "removeNamedItemNS", move |vm, t, args| {
        let Some(elem) = require_nnm_elem(&h, t) else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let ns_arg = arg(args, 0);
        let ns = if ns_arg.is_null() || ns_arg.is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(ns_arg)
        };
        let local = vm.value_to_rust_string(arg(args, 1));
        remove_attribute_node_from_element(vm, &h, elem, &ns, &local)
    });

    // NamedNodeMap.prototype[Symbol.iterator]
    let h = Rc::clone(host);
    let iter_fn = vm.alloc_host_fn("values", move |vm, t, _| {
        let Some(elem) = require_nnm_elem(&h, t) else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let count = elem_attr_count(&h, elem);
        let arr = vm.alloc_array();
        for i in 0..count {
            if let Some(v) = named_node_map_item(vm, &h, elem, i) {
                vm.heap.object_push_element(arr, v);
            }
        }
        let iter_k = vm.key_symbol_iterator;
        let iter_m = vm.get_property_by_key(arr, iter_k)?;
        vm.call_value(iter_m, JsValue::from_object(arr), &[])
    });
    let k_sym_iter = vm.key_symbol_iterator;
    vm.heap.object_define_data(
        nnmp,
        k_sym_iter,
        JsValue::from_object(iter_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
}

// ---------------------------------------------------------------------------
// DOMTokenList (WHATWG DOM §7.1)
// ---------------------------------------------------------------------------

fn validate_dom_token(vm: &mut JsVm, token: &str) -> Result<(), JsException> {
    if token.is_empty() {
        return Err(create_dom_exception(
            vm,
            "SyntaxError",
            "The token provided must not be empty",
        ));
    }
    if token.bytes().any(is_ascii_ws) {
        return Err(create_dom_exception(
            vm,
            "InvalidCharacterError",
            "The token provided contains HTML space characters",
        ));
    }
    Ok(())
}

/// Parses an attribute value into a deduplicated ordered set of tokens per WHATWG DOM §7.1.
fn parse_ordered_token_set(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tok in raw.split(['\t', '\n', '\x0C', '\r', ' ']) {
        if !tok.is_empty() && !out.iter().any(|existing: &String| existing == tok) {
            out.push(tok.to_string());
        }
    }
    out
}

/// Returns a cached live `DOMTokenList` Proxy for `(nid, attr_name)`.
pub fn get_or_create_token_list(
    vm: &mut JsVm,
    host: &SharedHost,
    nid: NodeId,
    attr_name: &'static str,
    supported_tokens: Option<&'static [&'static str]>,
) -> HeapHandle {
    let cache_key = (nid, attr_name);
    if let Some(&existing) = host.borrow().token_list_cache.get(&cache_key) {
        if vm.heap.get_object(existing).is_some() {
            return existing;
        }
    }
    let proto = host.borrow().prototypes.dom_token_list_proto;
    let target = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let handler = vm.alloc_ordinary_object();

    let h_get = Rc::clone(host);
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let target_h = arg(args, 0).as_object();
        let prop_val = arg(args, 1);
        let receiver = arg(args, 2);
        if prop_val.is_symbol() {
            if let Some(th) = target_h {
                let ks = vm.to_property_key(prop_val)?;
                let k = vm.heap.intern_key(&ks);
                return vm.get_property_with_receiver(th, k, receiver);
            }
            return Ok(JsValue::UNDEFINED);
        }
        let key_str = vm.value_to_rust_string(prop_val);
        if let Ok(idx) = key_str.parse::<usize>() {
            if idx.to_string() == key_str {
                let raw = get_attr_string(&h_get, nid, attr_name).unwrap_or_default();
                let tokens = parse_ordered_token_set(&raw);
                if let Some(tok) = tokens.get(idx) {
                    return Ok(JsValue::from_string(vm.heap.alloc_string(tok.clone())));
                }
                return Ok(JsValue::UNDEFINED);
            }
        }
        if let Some(th) = target_h {
            let k = vm.heap.intern_key(&key_str);
            return vm.get_property_with_receiver(th, k, receiver);
        }
        Ok(JsValue::UNDEFINED)
    });

    let h_has = Rc::clone(host);
    define_host_method(vm, handler, "has", move |vm, _, args| {
        let target_h = arg(args, 0).as_object();
        let prop_val = arg(args, 1);
        if prop_val.is_symbol() {
            if let Some(th) = target_h {
                let ks = vm.to_property_key(prop_val)?;
                let k = vm.heap.intern_key(&ks);
                return Ok(JsValue::from_bool(vm.has_property_by_key(th, k)?));
            }
            return Ok(JsValue::FALSE);
        }
        let key_str = vm.value_to_rust_string(prop_val);
        if let Ok(idx) = key_str.parse::<usize>() {
            if idx.to_string() == key_str {
                let raw = get_attr_string(&h_has, nid, attr_name).unwrap_or_default();
                let tokens = parse_ordered_token_set(&raw);
                return Ok(JsValue::from_bool(idx < tokens.len()));
            }
        }
        if let Some(th) = target_h {
            let k = vm.heap.intern_key(&key_str);
            return Ok(JsValue::from_bool(vm.has_property_by_key(th, k)?));
        }
        Ok(JsValue::FALSE)
    });

    let h_keys = Rc::clone(host);
    define_host_method(vm, handler, "ownKeys", move |vm, _, _| {
        let raw = get_attr_string(&h_keys, nid, attr_name).unwrap_or_default();
        let len = parse_ordered_token_set(&raw).len();
        let arr = vm.alloc_array();
        for i in 0..len {
            let s = JsValue::from_string(vm.heap.alloc_string(i.to_string()));
            vm.heap.object_push_element(arr, s);
        }
        Ok(JsValue::from_object(arr))
    });

    let h_desc = Rc::clone(host);
    define_host_method(
        vm,
        handler,
        "getOwnPropertyDescriptor",
        move |vm, _, args| {
            let prop_val = arg(args, 1);
            if prop_val.is_symbol() {
                return Ok(JsValue::UNDEFINED);
            }
            let key_str = vm.value_to_rust_string(prop_val);
            if let Ok(idx) = key_str.parse::<usize>() {
                if idx.to_string() == key_str {
                    let raw = get_attr_string(&h_desc, nid, attr_name).unwrap_or_default();
                    let tokens = parse_ordered_token_set(&raw);
                    if let Some(tok) = tokens.get(idx) {
                        let desc = vm.alloc_ordinary_object();
                        let v = JsValue::from_string(vm.heap.alloc_string(tok.clone()));
                        for (k_s, val) in [
                            ("value", v),
                            ("writable", JsValue::FALSE),
                            ("enumerable", JsValue::TRUE),
                            ("configurable", JsValue::TRUE),
                        ] {
                            let k = vm.heap.intern_key(k_s);
                            vm.heap.object_define_data(
                                desc,
                                k,
                                val,
                                PropertyAttrs::DEFAULT,
                                &mut vm.shapes,
                            );
                        }
                        return Ok(JsValue::from_object(desc));
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let proxy = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(rix_js::ProxyObject {
            target,
            handler,
            revoked: false,
        }),
        Some(proto),
    ));
    let entry = TokenListEntry {
        node: nid,
        attr_name,
        supported_tokens,
    };
    let mut hb = host.borrow_mut();
    hb.token_list_cache.insert(cache_key, proxy);
    hb.token_lists.insert(proxy, entry.clone());
    hb.token_lists.insert(target, entry);
    proxy
}

fn require_token_list(
    vm: &mut JsVm,
    host: &SharedHost,
    this_val: JsValue,
) -> Result<TokenListEntry, JsException> {
    this_val
        .as_object()
        .and_then(|h| host.borrow().token_lists.get(&h).cloned())
        .ok_or_else(|| vm.type_error("Illegal invocation: receiver is not a DOMTokenList"))
}

fn install_dom_token_list_prototype(vm: &mut JsVm, host: &SharedHost) {
    let p = host.borrow().prototypes.dom_token_list_proto;

    let h = Rc::clone(host);
    let g_len = vm.alloc_host_fn("get length", move |vm, t, _| {
        let entry = require_token_list(vm, &h, t)?;
        let raw = get_attr_string(&h, entry.node, entry.attr_name).unwrap_or_default();
        let len = parse_ordered_token_set(&raw).len();
        Ok(JsValue::from_f64(len as f64))
    });
    define_host_accessor(vm, p, "length", Some(g_len), None);

    let h1 = Rc::clone(host);
    let g_val = vm.alloc_host_fn("get value", move |vm, t, _| {
        let entry = require_token_list(vm, &h1, t)?;
        let raw = get_attr_string(&h1, entry.node, entry.attr_name).unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(raw)))
    });
    let h2 = Rc::clone(host);
    let s_val = vm.alloc_host_fn("set value", move |vm, t, args| {
        let entry = require_token_list(vm, &h2, t)?;
        let v = vm.value_to_rust_string(arg(args, 0));
        set_element_attr(vm, &h2, entry.node, entry.attr_name, &v);
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, p, "value", Some(g_val), Some(s_val));

    let h = Rc::clone(host);
    define_host_method(vm, p, "toString", move |vm, t, _| {
        let entry = require_token_list(vm, &h, t)?;
        let raw = get_attr_string(&h, entry.node, entry.attr_name).unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(raw)))
    });

    let h = Rc::clone(host);
    define_host_method(vm, p, "item", move |vm, t, args| {
        let entry = require_token_list(vm, &h, t)?;
        let idx = to_webidl_u32(vm, arg(args, 0))?;
        let raw = get_attr_string(&h, entry.node, entry.attr_name).unwrap_or_default();
        let tokens = parse_ordered_token_set(&raw);
        Ok(match tokens.get(idx) {
            Some(tok) => JsValue::from_string(vm.heap.alloc_string(tok.clone())),
            None => JsValue::NULL,
        })
    });

    let h = Rc::clone(host);
    define_host_method(vm, p, "contains", move |vm, t, args| {
        let entry = require_token_list(vm, &h, t)?;
        let tok = vm.value_to_rust_string(arg(args, 0));
        let raw = get_attr_string(&h, entry.node, entry.attr_name).unwrap_or_default();
        let has = parse_ordered_token_set(&raw).iter().any(|x| x == &tok);
        Ok(JsValue::from_bool(has))
    });

    let h = Rc::clone(host);
    define_host_method(vm, p, "add", move |vm, t, args| {
        let entry = require_token_list(vm, &h, t)?;
        let mut to_add = Vec::with_capacity(args.len());
        for &a in args {
            let s = vm.value_to_rust_string(a);
            validate_dom_token(vm, &s)?;
            to_add.push(s);
        }
        let existing_attr = get_attr_string(&h, entry.node, entry.attr_name);
        let mut tokens = parse_ordered_token_set(existing_attr.as_deref().unwrap_or(""));
        for s in to_add {
            if !tokens.iter().any(|x| x == &s) {
                tokens.push(s);
            }
        }
        // WHATWG DOM §7.1 update steps: if attr was null and set is still empty, do nothing!
        if existing_attr.is_some() || !tokens.is_empty() {
            set_element_attr(vm, &h, entry.node, entry.attr_name, &tokens.join(" "));
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, p, "remove", move |vm, t, args| {
        let entry = require_token_list(vm, &h, t)?;
        let mut to_rm = Vec::with_capacity(args.len());
        for &a in args {
            let s = vm.value_to_rust_string(a);
            validate_dom_token(vm, &s)?;
            to_rm.push(s);
        }
        let existing_attr = get_attr_string(&h, entry.node, entry.attr_name);
        let mut tokens = parse_ordered_token_set(existing_attr.as_deref().unwrap_or(""));
        tokens.retain(|x| !to_rm.iter().any(|r| r == x));
        if existing_attr.is_some() || !tokens.is_empty() {
            set_element_attr(vm, &h, entry.node, entry.attr_name, &tokens.join(" "));
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, p, "toggle", move |vm, t, args| {
        let entry = require_token_list(vm, &h, t)?;
        let tok = vm.value_to_rust_string(arg(args, 0));
        validate_dom_token(vm, &tok)?;
        let force = args
            .get(1)
            .copied()
            .filter(|v| !v.is_undefined())
            .map(|v| vm.to_boolean(v));
        let existing_attr = get_attr_string(&h, entry.node, entry.attr_name);
        let mut tokens = parse_ordered_token_set(existing_attr.as_deref().unwrap_or(""));
        let present = tokens.iter().any(|x| x == &tok);
        if present {
            if force == Some(true) {
                return Ok(JsValue::TRUE);
            }
            tokens.retain(|x| x != &tok);
            set_element_attr(vm, &h, entry.node, entry.attr_name, &tokens.join(" "));
            Ok(JsValue::FALSE)
        } else {
            if force == Some(false) {
                return Ok(JsValue::FALSE);
            }
            tokens.push(tok);
            set_element_attr(vm, &h, entry.node, entry.attr_name, &tokens.join(" "));
            Ok(JsValue::TRUE)
        }
    });

    let h = Rc::clone(host);
    define_host_method(vm, p, "replace", move |vm, t, args| {
        let entry = require_token_list(vm, &h, t)?;
        let old_tok = vm.value_to_rust_string(arg(args, 0));
        let new_tok = vm.value_to_rust_string(arg(args, 1));
        if old_tok.is_empty() || new_tok.is_empty() {
            return Err(create_dom_exception(
                vm,
                "SyntaxError",
                "DOMTokenList token must not be empty",
            ));
        }
        if old_tok.bytes().any(is_ascii_ws) || new_tok.bytes().any(is_ascii_ws) {
            return Err(create_dom_exception(
                vm,
                "InvalidCharacterError",
                "DOMTokenList token must not contain ASCII whitespace",
            ));
        }
        let existing_attr = get_attr_string(&h, entry.node, entry.attr_name);
        let mut tokens = parse_ordered_token_set(existing_attr.as_deref().unwrap_or(""));
        let Some(pos) = tokens.iter().position(|x| x == &old_tok) else {
            return Ok(JsValue::FALSE);
        };
        if let Some(new_pos) = tokens.iter().position(|x| x == &new_tok) {
            if new_pos < pos {
                tokens.remove(pos);
            } else if new_pos > pos {
                tokens.remove(new_pos);
                tokens[pos] = new_tok;
            }
        } else {
            tokens[pos] = new_tok;
        }
        set_element_attr(vm, &h, entry.node, entry.attr_name, &tokens.join(" "));
        Ok(JsValue::TRUE)
    });

    let h = Rc::clone(host);
    define_host_method(vm, p, "supports", move |vm, t, args| {
        let entry = require_token_list(vm, &h, t)?;
        let Some(supported) = entry.supported_tokens else {
            return Err(vm.type_error(format!(
                "DOMTokenList for '{}' has no supported tokens",
                entry.attr_name
            )));
        };
        let tok = vm.value_to_rust_string(arg(args, 0)).to_ascii_lowercase();
        Ok(JsValue::from_bool(supported.contains(&tok.as_str())))
    });

    // WebIDL value-iterable methods borrowed from Array.prototype (DOMTokenList-iteration.html).
    copy_array_iterable_methods(vm, p, true);
}

fn copy_array_iterable_methods(vm: &mut JsVm, target_proto: HeapHandle, full_value_iterable: bool) {
    let arr_proto = vm.array_proto;
    let k_sym_iter = vm.key_symbol_iterator;
    if let Ok(iter_fn) = vm.get_property_by_key(arr_proto, k_sym_iter) {
        vm.heap.object_define_data(
            target_proto,
            k_sym_iter,
            iter_fn,
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }
    if full_value_iterable {
        for name in ["values", "keys", "entries", "forEach"] {
            let k = vm.heap.intern_key(name);
            if let Ok(method_val) = vm.get_property_by_key(arr_proto, k) {
                vm.heap.object_define_data(
                    target_proto,
                    k,
                    method_val,
                    PropertyAttrs::METHOD,
                    &mut vm.shapes,
                );
            }
        }
    }
}

fn install_element_token_list_attributes(vm: &mut JsVm, host: &SharedHost) {
    const LINK_REL_TOKENS: &[&str] = &[
        "alternate",
        "canonical",
        "dns-prefetch",
        "icon",
        "manifest",
        "modulepreload",
        "next",
        "pingback",
        "preconnect",
        "prefetch",
        "preload",
        "prev",
        "search",
        "stylesheet",
    ];
    const ANCHOR_REL_TOKENS: &[&str] = &[
        "alternate",
        "author",
        "bookmark",
        "external",
        "help",
        "license",
        "next",
        "nofollow",
        "noopener",
        "noreferrer",
        "opener",
        "prev",
        "privacy-policy",
        "search",
        "tag",
        "terms-of-service",
    ];
    const SANDBOX_TOKENS: &[&str] = &[
        "allow-downloads",
        "allow-forms",
        "allow-modals",
        "allow-orientation-lock",
        "allow-pointer-lock",
        "allow-popups",
        "allow-popups-to-escape-sandbox",
        "allow-presentation",
        "allow-same-origin",
        "allow-scripts",
        "allow-top-navigation",
        "allow-top-navigation-by-user-activation",
        "allow-top-navigation-to-custom-protocols",
    ];
    const BLOCKING_TOKENS: &[&str] = &["render"];

    let install_tl = |vm: &mut JsVm,
                      proto: HeapHandle,
                      prop: &'static str,
                      attr: &'static str,
                      supported: Option<&'static [&'static str]>| {
        let h1 = Rc::clone(host);
        let g = vm.alloc_host_fn(prop, move |vm, t, _| {
            let nid = require_node(vm, &h1, t)?;
            Ok(JsValue::from_object(get_or_create_token_list(
                vm, &h1, nid, attr, supported,
            )))
        });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn(prop, move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            let val = vm.value_to_rust_string(arg(args, 0));
            set_element_attr(vm, &h2, nid, attr, &val);
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, proto, prop, Some(g), Some(s));
    };

    let protos = host.borrow().interface_protos.clone();
    if let Some(&p) = protos.get("HTMLAnchorElement") {
        install_tl(vm, p, "relList", "rel", Some(ANCHOR_REL_TOKENS));
    }
    if let Some(&p) = protos.get("SVGAElement") {
        install_tl(vm, p, "relList", "rel", Some(ANCHOR_REL_TOKENS));
    }
    if let Some(&p) = protos.get("HTMLAreaElement") {
        install_tl(vm, p, "relList", "rel", Some(ANCHOR_REL_TOKENS));
    }
    if let Some(&p) = protos.get("HTMLLinkElement") {
        install_tl(vm, p, "relList", "rel", Some(LINK_REL_TOKENS));
        install_tl(vm, p, "sizes", "sizes", None);
        install_tl(vm, p, "blocking", "blocking", Some(BLOCKING_TOKENS));
    }
    if let Some(&p) = protos.get("HTMLFormElement") {
        install_tl(vm, p, "relList", "rel", Some(ANCHOR_REL_TOKENS));
    }
    if let Some(&p) = protos.get("HTMLIFrameElement") {
        install_tl(vm, p, "sandbox", "sandbox", Some(SANDBOX_TOKENS));
    }
    if let Some(&p) = protos.get("HTMLOutputElement") {
        install_tl(vm, p, "htmlFor", "for", None);
    }
    if let Some(&p) = protos.get("HTMLTableCellElement") {
        install_tl(vm, p, "headers", "headers", None);
    }
    if let Some(&p) = protos.get("HTMLScriptElement") {
        install_tl(vm, p, "blocking", "blocking", Some(BLOCKING_TOKENS));
    }
    if let Some(&p) = protos.get("HTMLStyleElement") {
        install_tl(vm, p, "blocking", "blocking", Some(BLOCKING_TOKENS));
    }
    // MathML `<a>.relList` (WPT DOMTokenList-coverage-for-attributes.html)
    let mathml_p = host.borrow().prototypes.mathml_element_proto;
    let h1 = Rc::clone(host);
    let g_mrel = vm.alloc_host_fn("get relList", move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        let is_a = matches!(
            h1.borrow().document.borrow().get(nid),
            Some(NodeData::Element(e)) if &*e.name.local == "a" && &*e.name.ns == MATHML_NS
        );
        if is_a {
            Ok(JsValue::from_object(get_or_create_token_list(
                vm,
                &h1,
                nid,
                "rel",
                Some(ANCHOR_REL_TOKENS),
            )))
        } else {
            Ok(JsValue::UNDEFINED)
        }
    });
    let h2 = Rc::clone(host);
    let s_mrel = vm.alloc_host_fn("set relList", move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let is_a = matches!(
            h2.borrow().document.borrow().get(nid),
            Some(NodeData::Element(e)) if &*e.name.local == "a" && &*e.name.ns == MATHML_NS
        );
        if is_a {
            let val = vm.value_to_rust_string(arg(args, 0));
            set_element_attr(vm, &h2, nid, "rel", &val);
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, mathml_p, "relList", Some(g_mrel), Some(s_mrel));
    // Element.prototype.part (CSS Shadow Parts)
    let ep = host.borrow().prototypes.element_proto;
    install_tl(vm, ep, "part", "part", None);
}

// ---------------------------------------------------------------------------
// Live HTMLCollection & NodeList Proxies
// ---------------------------------------------------------------------------

fn evaluate_html_collection(host: &SharedHost, kind: &HtmlCollectionKind) -> Vec<NodeId> {
    let doc_rc = Rc::clone(&host.borrow().document);
    let doc = doc_rc.borrow();
    match kind {
        HtmlCollectionKind::Children(parent) => doc
            .children(*parent)
            .filter(|&c| matches!(doc.get(c), Some(NodeData::Element(_))))
            .collect(),
        HtmlCollectionKind::ByTagName {
            root,
            qname,
            is_html_doc,
        } => {
            if qname == "*" {
                return doc
                    .descendants(*root)
                    .filter(|&n| n != *root && matches!(doc.get(n), Some(NodeData::Element(_))))
                    .collect();
            }
            let lower = qname.to_ascii_lowercase();
            doc.descendants(*root)
                .filter(|&n| {
                    n != *root
                        && matches!(doc.get(n), Some(NodeData::Element(e)) if {
                            let elem_qn = match &e.name.prefix {
                                Some(p) if !p.is_empty() => format!("{p}:{}", e.name.local),
                                _ => e.name.local.to_string(),
                            };
                            if *is_html_doc && &*e.name.ns == HTML_NS {
                                elem_qn == lower
                            } else {
                                elem_qn == *qname
                            }
                        })
                })
                .collect()
        }
        HtmlCollectionKind::ByTagNameNS { root, ns, local } => doc
            .descendants(*root)
            .filter(|&n| {
                n != *root
                    && matches!(doc.get(n), Some(NodeData::Element(e)) if {
                        (ns == "*" || &*e.name.ns == ns.as_str())
                            && (local == "*" || &*e.name.local == local.as_str())
                    })
            })
            .collect(),
        HtmlCollectionKind::ByClassName {
            root,
            classes,
            quirks,
        } => {
            if classes.is_empty() {
                return Vec::new();
            }
            doc.descendants(*root)
                .filter(|&n| {
                    n != *root
                        && matches!(doc.get(n), Some(NodeData::Element(e)) if {
                            let raw = e.get_attribute("class").unwrap_or("");
                            let actual: Vec<&str> = raw
                                .split(['\t', '\n', '\x0C', '\r', ' '])
                                .filter(|s| !s.is_empty())
                                .collect();
                            classes.iter().all(|w| {
                                if *quirks {
                                    actual.iter().any(|a| a.eq_ignore_ascii_case(w))
                                } else {
                                    actual.iter().any(|a| a == w)
                                }
                            })
                        })
                })
                .collect()
        }
    }
}

fn html_collection_named_item(host: &SharedHost, items: &[NodeId], name: &str) -> Option<NodeId> {
    if name.is_empty() {
        return None;
    }
    let doc_rc = Rc::clone(&host.borrow().document);
    let doc = doc_rc.borrow();
    for &nid in items {
        if let Some(NodeData::Element(e)) = doc.get(nid) {
            if e.get_attribute("id") == Some(name) {
                return Some(nid);
            }
            if &*e.name.ns == HTML_NS && e.get_attribute("name") == Some(name) {
                return Some(nid);
            }
        }
    }
    None
}

fn html_collection_supported_names(host: &SharedHost, items: &[NodeId]) -> Vec<String> {
    let doc_rc = Rc::clone(&host.borrow().document);
    let doc = doc_rc.borrow();
    let mut names = Vec::new();
    for &nid in items {
        if let Some(NodeData::Element(e)) = doc.get(nid) {
            if let Some(id) = e.get_attribute("id").filter(|s| !s.is_empty()) {
                if !names.iter().any(|x| x == id) {
                    names.push(id.to_string());
                }
            }
            if &*e.name.ns == HTML_NS {
                if let Some(nm) = e.get_attribute("name").filter(|s| !s.is_empty()) {
                    if !names.iter().any(|x| x == nm) {
                        names.push(nm.to_string());
                    }
                }
            }
        }
    }
    names
}

/// Creates a live `HTMLCollection` Proxy (indexed + named property access, live updates).
pub fn create_live_html_collection(
    vm: &mut JsVm,
    host: &SharedHost,
    kind: HtmlCollectionKind,
) -> JsValue {
    let proto = host.borrow().prototypes.html_collection_proto;
    let target = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let handler = vm.alloc_ordinary_object();

    let h_get = Rc::clone(host);
    let k_get = kind.clone();
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let target_h = arg(args, 0).as_object();
        let prop_val = arg(args, 1);
        let receiver = arg(args, 2);
        if prop_val.is_symbol() {
            if let Some(th) = target_h {
                let ks = vm.to_property_key(prop_val)?;
                let k = vm.heap.intern_key(&ks);
                return vm.get_property_with_receiver(th, k, receiver);
            }
            return Ok(JsValue::UNDEFINED);
        }
        let key_str = vm.value_to_rust_string(prop_val);
        // Check own properties on target first (for expando properties), then indices, then proto, then named items.
        if let Some(th) = target_h {
            let k = vm.heap.intern_key(&key_str);
            if vm.heap.object_get_own(th, k, &vm.shapes).is_some() {
                return vm.get_property_with_receiver(th, k, receiver);
            }
        }
        if let Ok(idx) = key_str.parse::<u32>() {
            if idx != u32::MAX && idx.to_string() == key_str {
                let items = evaluate_html_collection(&h_get, &k_get);
                if let Some(&nid) = items.get(idx as usize) {
                    return Ok(wrap_node(vm, &h_get, nid));
                }
                return Ok(JsValue::UNDEFINED);
            }
        }
        if let Some(th) = target_h {
            let k = vm.heap.intern_key(&key_str);
            if vm.has_property_by_key(th, k)? {
                return vm.get_property_with_receiver(th, k, receiver);
            }
        }
        let items = evaluate_html_collection(&h_get, &k_get);
        if let Some(nid) = html_collection_named_item(&h_get, &items, &key_str) {
            return Ok(wrap_node(vm, &h_get, nid));
        }
        Ok(JsValue::UNDEFINED)
    });

    let h_has = Rc::clone(host);
    let k_has = kind.clone();
    define_host_method(vm, handler, "has", move |vm, _, args| {
        let target_h = arg(args, 0).as_object();
        let prop_val = arg(args, 1);
        if prop_val.is_symbol() {
            if let Some(th) = target_h {
                let ks = vm.to_property_key(prop_val)?;
                let k = vm.heap.intern_key(&ks);
                return Ok(JsValue::from_bool(vm.has_property_by_key(th, k)?));
            }
            return Ok(JsValue::FALSE);
        }
        let key_str = vm.value_to_rust_string(prop_val);
        if let Some(th) = target_h {
            let k = vm.heap.intern_key(&key_str);
            if vm.has_property_by_key(th, k)? {
                return Ok(JsValue::TRUE);
            }
        }
        let items = evaluate_html_collection(&h_has, &k_has);
        if let Ok(idx) = key_str.parse::<u32>() {
            if idx != u32::MAX && idx.to_string() == key_str && (idx as usize) < items.len() {
                return Ok(JsValue::TRUE);
            }
        }
        Ok(JsValue::from_bool(
            html_collection_named_item(&h_has, &items, &key_str).is_some(),
        ))
    });

    let h_keys = Rc::clone(host);
    let k_keys = kind.clone();
    define_host_method(vm, handler, "ownKeys", move |vm, _, args| {
        let target_h = arg(args, 0).as_object();
        let items = evaluate_html_collection(&h_keys, &k_keys);
        let names = html_collection_supported_names(&h_keys, &items);
        let arr = vm.alloc_array();
        let mut seen = Vec::new();
        for i in 0..items.len() {
            let s = i.to_string();
            seen.push(s.clone());
            let v = JsValue::from_string(vm.heap.alloc_string(s));
            vm.heap.object_push_element(arr, v);
        }
        for name in names {
            if !seen.contains(&name) {
                seen.push(name.clone());
                let v = JsValue::from_string(vm.heap.alloc_string(name));
                vm.heap.object_push_element(arr, v);
            }
        }
        if let Some(th) = target_h {
            if let Ok(own) = rix_js::proxy::mop_own_property_keys(vm, th) {
                for (kv, _) in own {
                    if kv.is_symbol() {
                        vm.heap.object_push_element(arr, kv);
                    } else {
                        let s = vm.value_to_rust_string(kv);
                        if !seen.contains(&s) {
                            seen.push(s.clone());
                            let v = JsValue::from_string(vm.heap.alloc_string(s));
                            vm.heap.object_push_element(arr, v);
                        }
                    }
                }
            }
        }
        Ok(JsValue::from_object(arr))
    });

    let h_desc = Rc::clone(host);
    let k_desc = kind.clone();
    define_host_method(
        vm,
        handler,
        "getOwnPropertyDescriptor",
        move |vm, _, args| {
            let target_h = arg(args, 0).as_object();
            let prop_val = arg(args, 1);
            if prop_val.is_symbol() {
                return Ok(JsValue::UNDEFINED);
            }
            let key_str = vm.value_to_rust_string(prop_val);
            if let Some(th) = target_h {
                let k = vm.heap.intern_key(&key_str);
                if let Some(prop) = vm.heap.object_get_own(th, k, &vm.shapes) {
                    let desc = vm.alloc_ordinary_object();
                    for (ks, v) in [
                        ("value", prop.value),
                        (
                            "writable",
                            JsValue::from_bool(prop.attrs.contains(PropertyAttrs::WRITABLE)),
                        ),
                        (
                            "enumerable",
                            JsValue::from_bool(prop.attrs.contains(PropertyAttrs::ENUMERABLE)),
                        ),
                        (
                            "configurable",
                            JsValue::from_bool(prop.attrs.contains(PropertyAttrs::CONFIGURABLE)),
                        ),
                    ] {
                        let dk = vm.heap.intern_key(ks);
                        vm.heap.object_define_data(
                            desc,
                            dk,
                            v,
                            PropertyAttrs::DEFAULT,
                            &mut vm.shapes,
                        );
                    }
                    return Ok(JsValue::from_object(desc));
                }
            }
            let items = evaluate_html_collection(&h_desc, &k_desc);
            let found = if let Ok(idx) = key_str.parse::<u32>() {
                if idx != u32::MAX && idx.to_string() == key_str {
                    items
                        .get(idx as usize)
                        .map(|&nid| (wrap_node(vm, &h_desc, nid), true))
                } else {
                    None
                }
            } else {
                html_collection_named_item(&h_desc, &items, &key_str)
                    .map(|nid| (wrap_node(vm, &h_desc, nid), false))
            };
            let Some((val, enumerable)) = found else {
                return Ok(JsValue::UNDEFINED);
            };
            let desc = vm.alloc_ordinary_object();
            for (ks, v) in [
                ("value", val),
                ("writable", JsValue::FALSE),
                ("enumerable", JsValue::from_bool(enumerable)),
                ("configurable", JsValue::TRUE),
            ] {
                let dk = vm.heap.intern_key(ks);
                vm.heap
                    .object_define_data(desc, dk, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
            }
            Ok(JsValue::from_object(desc))
        },
    );

    // WebIDL LegacyPlatformObject [[Set]] & [[Delete]] traps for HTMLCollection:
    // reject writes to array indices or shadowing an existing named property,
    // and reject deletes of supported indexed or named properties.
    let h_set = Rc::clone(host);
    let k_set = kind.clone();
    define_host_method(vm, handler, "set", move |vm, _, args| {
        let target_h = arg(args, 0).as_object();
        let prop_val = arg(args, 1);
        let value = arg(args, 2);
        if prop_val.is_symbol() {
            if let Some(th) = target_h {
                let ks = vm.to_property_key(prop_val)?;
                let k = vm.heap.intern_key(&ks);
                vm.heap
                    .object_define_data(th, k, value, PropertyAttrs::DEFAULT, &mut vm.shapes);
            }
            return Ok(JsValue::TRUE);
        }
        let key_str = vm.value_to_rust_string(prop_val);
        if let Ok(idx) = key_str.parse::<u32>() {
            if idx != u32::MAX && idx.to_string() == key_str {
                return Ok(JsValue::FALSE);
            }
        }
        let items = evaluate_html_collection(&h_set, &k_set);
        if html_collection_named_item(&h_set, &items, &key_str).is_some() {
            return Ok(JsValue::FALSE);
        }
        if let Some(th) = target_h {
            let k = vm.heap.intern_key(&key_str);
            vm.heap
                .object_define_data(th, k, value, PropertyAttrs::DEFAULT, &mut vm.shapes);
        }
        Ok(JsValue::TRUE)
    });

    let h_del = Rc::clone(host);
    let k_del = kind.clone();
    define_host_method(vm, handler, "deleteProperty", move |vm, _, args| {
        let target_h = arg(args, 0).as_object();
        let prop_val = arg(args, 1);
        if prop_val.is_symbol() {
            if let Some(th) = target_h {
                let ks = vm.to_property_key(prop_val)?;
                let k = vm.heap.intern_key(&ks);
                let _ = vm.heap.object_delete_own(th, k, &mut vm.shapes);
            }
            return Ok(JsValue::TRUE);
        }
        let key_str = vm.value_to_rust_string(prop_val);
        let items = evaluate_html_collection(&h_del, &k_del);
        if let Ok(idx) = key_str.parse::<u32>() {
            if idx != u32::MAX && idx.to_string() == key_str {
                return Ok(JsValue::from_bool((idx as usize) >= items.len()));
            }
        }
        if html_collection_named_item(&h_del, &items, &key_str).is_some() {
            return Ok(JsValue::FALSE);
        }
        if let Some(th) = target_h {
            let k = vm.heap.intern_key(&key_str);
            let _ = vm.heap.object_delete_own(th, k, &mut vm.shapes);
        }
        Ok(JsValue::TRUE)
    });

    let proxy = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(rix_js::ProxyObject {
            target,
            handler,
            revoked: false,
        }),
        Some(proto),
    ));
    let mut hb = host.borrow_mut();
    hb.html_collections.insert(proxy, kind.clone());
    hb.html_collections.insert(target, kind);
    JsValue::from_object(proxy)
}

/// Creates a live `NodeList` Proxy for `node.childNodes`.
pub fn create_live_child_nodes(vm: &mut JsVm, host: &SharedHost, parent: NodeId) -> JsValue {
    if let Some(&existing) = host.borrow().child_nodes_cache.get(&parent) {
        if vm.heap.get_object(existing).is_some() {
            return JsValue::from_object(existing);
        }
    }
    let proto = host.borrow().prototypes.node_list_proto;
    let target = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let handler = vm.alloc_ordinary_object();

    let h_get = Rc::clone(host);
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let target_h = arg(args, 0).as_object();
        let prop_val = arg(args, 1);
        let receiver = arg(args, 2);
        if prop_val.is_symbol() {
            if let Some(th) = target_h {
                let ks = vm.to_property_key(prop_val)?;
                let k = vm.heap.intern_key(&ks);
                return vm.get_property_with_receiver(th, k, receiver);
            }
            return Ok(JsValue::UNDEFINED);
        }
        let key_str = vm.value_to_rust_string(prop_val);
        if let Ok(idx) = key_str.parse::<u32>() {
            if idx != u32::MAX && idx.to_string() == key_str {
                let kid = h_get
                    .borrow()
                    .document
                    .borrow()
                    .children(parent)
                    .nth(idx as usize);
                if let Some(nid) = kid {
                    return Ok(wrap_node(vm, &h_get, nid));
                }
                return Ok(JsValue::UNDEFINED);
            }
        }
        if let Some(th) = target_h {
            let k = vm.heap.intern_key(&key_str);
            return vm.get_property_with_receiver(th, k, receiver);
        }
        Ok(JsValue::UNDEFINED)
    });

    let h_has = Rc::clone(host);
    define_host_method(vm, handler, "has", move |vm, _, args| {
        let target_h = arg(args, 0).as_object();
        let prop_val = arg(args, 1);
        if prop_val.is_symbol() {
            if let Some(th) = target_h {
                let ks = vm.to_property_key(prop_val)?;
                let k = vm.heap.intern_key(&ks);
                return Ok(JsValue::from_bool(vm.has_property_by_key(th, k)?));
            }
            return Ok(JsValue::FALSE);
        }
        let key_str = vm.value_to_rust_string(prop_val);
        if let Ok(idx) = key_str.parse::<u32>() {
            if idx != u32::MAX && idx.to_string() == key_str {
                let count = h_has.borrow().document.borrow().children(parent).count();
                return Ok(JsValue::from_bool((idx as usize) < count));
            }
        }
        if let Some(th) = target_h {
            let k = vm.heap.intern_key(&key_str);
            return Ok(JsValue::from_bool(vm.has_property_by_key(th, k)?));
        }
        Ok(JsValue::FALSE)
    });

    let h_keys = Rc::clone(host);
    define_host_method(vm, handler, "ownKeys", move |vm, _, _| {
        let count = h_keys.borrow().document.borrow().children(parent).count();
        let arr = vm.alloc_array();
        for i in 0..count {
            let s = JsValue::from_string(vm.heap.alloc_string(i.to_string()));
            vm.heap.object_push_element(arr, s);
        }
        Ok(JsValue::from_object(arr))
    });

    let h_desc = Rc::clone(host);
    define_host_method(
        vm,
        handler,
        "getOwnPropertyDescriptor",
        move |vm, _, args| {
            let prop_val = arg(args, 1);
            if prop_val.is_symbol() {
                return Ok(JsValue::UNDEFINED);
            }
            let key_str = vm.value_to_rust_string(prop_val);
            if let Ok(idx) = key_str.parse::<u32>() {
                if idx != u32::MAX && idx.to_string() == key_str {
                    let kid = h_desc
                        .borrow()
                        .document
                        .borrow()
                        .children(parent)
                        .nth(idx as usize);
                    if let Some(nid) = kid {
                        let v = wrap_node(vm, &h_desc, nid);
                        let desc = vm.alloc_ordinary_object();
                        for (ks, dv) in [
                            ("value", v),
                            ("writable", JsValue::FALSE),
                            ("enumerable", JsValue::TRUE),
                            ("configurable", JsValue::TRUE),
                        ] {
                            let dk = vm.heap.intern_key(ks);
                            vm.heap.object_define_data(
                                desc,
                                dk,
                                dv,
                                PropertyAttrs::DEFAULT,
                                &mut vm.shapes,
                            );
                        }
                        return Ok(JsValue::from_object(desc));
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let proxy = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(rix_js::ProxyObject {
            target,
            handler,
            revoked: false,
        }),
        Some(proto),
    ));
    let mut hb = host.borrow_mut();
    hb.child_nodes_cache.insert(parent, proxy);
    hb.live_node_lists.insert(proxy, parent);
    hb.live_node_lists.insert(target, parent);
    JsValue::from_object(proxy)
}

/// Creates a static `NodeList` (for `querySelectorAll`) using dense JS element storage
/// for O(1) indexed access in hot loops (`Array.prototype.indexOf`, etc.).
pub fn create_static_node_list(vm: &mut JsVm, host: &SharedHost, nodes: Vec<NodeId>) -> JsValue {
    let proto = host.borrow().prototypes.node_list_proto;
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    for &nid in &nodes {
        let v = wrap_node(vm, host, nid);
        vm.heap.object_push_element(obj, v);
    }
    host.borrow_mut().static_node_lists.insert(obj, nodes);
    JsValue::from_object(obj)
}

fn node_list_len(host: &SharedHost, handle: HeapHandle) -> Option<usize> {
    let hb = host.borrow();
    if let Some(&parent) = hb.live_node_lists.get(&handle) {
        return Some(hb.document.borrow().children(parent).count());
    }
    hb.static_node_lists.get(&handle).map(Vec::len)
}

fn node_list_item(host: &SharedHost, handle: HeapHandle, idx: usize) -> Option<Option<NodeId>> {
    let hb = host.borrow();
    if let Some(&parent) = hb.live_node_lists.get(&handle) {
        return Some(hb.document.borrow().children(parent).nth(idx));
    }
    hb.static_node_lists
        .get(&handle)
        .map(|v| v.get(idx).copied())
}

fn install_collection_prototypes(vm: &mut JsVm, host: &SharedHost) {
    let hcp = host.borrow().prototypes.html_collection_proto;
    let nlp = host.borrow().prototypes.node_list_proto;

    // HTMLCollection.prototype: length, item, namedItem, [Symbol.iterator] (live Array iterator!)
    let h = Rc::clone(host);
    let g_len = vm.alloc_host_fn("get length", move |vm, t, _| {
        let kind = t
            .as_object()
            .and_then(|oh| h.borrow().html_collections.get(&oh).cloned())
            .ok_or_else(|| vm.type_error("Illegal invocation: receiver is not HTMLCollection"))?;
        let len = evaluate_html_collection(&h, &kind).len();
        Ok(JsValue::from_f64(len as f64))
    });
    define_host_accessor(vm, hcp, "length", Some(g_len), None);

    let h = Rc::clone(host);
    define_host_method(vm, hcp, "item", move |vm, t, args| {
        let kind = t
            .as_object()
            .and_then(|oh| h.borrow().html_collections.get(&oh).cloned())
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let idx = to_webidl_u32(vm, arg(args, 0))?;
        let items = evaluate_html_collection(&h, &kind);
        Ok(items
            .get(idx)
            .map_or(JsValue::NULL, |&nid| wrap_node(vm, &h, nid)))
    });

    let h = Rc::clone(host);
    define_host_method(vm, hcp, "namedItem", move |vm, t, args| {
        let kind = t
            .as_object()
            .and_then(|oh| h.borrow().html_collections.get(&oh).cloned())
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let name = vm.value_to_rust_string(arg(args, 0));
        let items = evaluate_html_collection(&h, &kind);
        Ok(html_collection_named_item(&h, &items, &name)
            .map_or(JsValue::NULL, |nid| wrap_node(vm, &h, nid)))
    });

    copy_array_iterable_methods(vm, hcp, false);

    // NodeList.prototype: length, item, values, keys, entries, forEach, [Symbol.iterator]
    let h = Rc::clone(host);
    let g_len = vm.alloc_host_fn("get length", move |vm, t, _| {
        let len = t
            .as_object()
            .and_then(|oh| node_list_len(&h, oh))
            .ok_or_else(|| vm.type_error("Illegal invocation: receiver is not NodeList"))?;
        Ok(JsValue::from_f64(len as f64))
    });
    define_host_accessor(vm, nlp, "length", Some(g_len), None);

    let h = Rc::clone(host);
    define_host_method(vm, nlp, "item", move |vm, t, args| {
        let oh = t
            .as_object()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let idx = to_webidl_u32(vm, arg(args, 0))?;
        let found =
            node_list_item(&h, oh, idx).ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(found.map_or(JsValue::NULL, |nid| wrap_node(vm, &h, nid)))
    });

    copy_array_iterable_methods(vm, nlp, true);
}

// ---------------------------------------------------------------------------
// Extra Node.prototype Methods & Pre-Insertion Validity (WHATWG DOM §4.2 / §4.4)
// ---------------------------------------------------------------------------
// TreeWalker, NodeIterator & NodeFilter (WHATWG DOM §6)
// ---------------------------------------------------------------------------

fn node_filter_bit(doc: &Document, nid: NodeId) -> u32 {
    match doc.get(nid) {
        Some(NodeData::Element(_)) => 0x1,
        Some(NodeData::Text(_)) => 0x4,
        Some(NodeData::CDataSection(_)) => 0x8,
        Some(NodeData::ProcessingInstruction { .. }) => 0x40,
        Some(NodeData::Comment(_)) => 0x80,
        Some(NodeData::Document) => 0x100,
        Some(NodeData::DocumentType { .. }) => 0x200,
        Some(NodeData::DocumentFragment | NodeData::ShadowRoot { .. }) => 0x400,
        None => 0,
    }
}

fn filter_node(
    vm: &mut JsVm,
    host: &SharedHost,
    trav_h: HeapHandle,
    nid: NodeId,
) -> Result<i32, JsException> {
    let (what_to_show, filter, already_active) = {
        let hb = host.borrow();
        let Some(t) = hb.traversals.get(&trav_h) else {
            return Err(vm.type_error("Illegal invocation"));
        };
        (t.what_to_show, t.filter, t.active)
    };
    if already_active {
        return Err(create_dom_exception(
            vm,
            "InvalidStateError",
            "Recursive TreeWalker/NodeIterator filter invocation",
        ));
    }
    let bit = node_filter_bit(&host.borrow().document.borrow(), nid);
    if (what_to_show & bit) == 0 {
        return Ok(3); // FILTER_SKIP
    }
    let Some(fh) = filter else {
        return Ok(1); // FILTER_ACCEPT
    };
    if let Some(t) = host.borrow_mut().traversals.get_mut(&trav_h) {
        t.active = true;
    }
    let node_val = wrap_node(vm, host, nid);
    let call_res = if vm.is_callable_handle(fh) {
        vm.call_value(JsValue::from_object(fh), JsValue::UNDEFINED, &[node_val])
    } else {
        let k = vm.heap.intern_key("acceptNode");
        match vm.get_property_by_key(fh, k) {
            Ok(method) => vm.call_value(method, JsValue::from_object(fh), &[node_val]),
            Err(e) => Err(e),
        }
    };
    if let Some(t) = host.borrow_mut().traversals.get_mut(&trav_h) {
        t.active = false;
    }
    let rv = call_res?;
    let n = vm.to_number(rv)?;
    Ok(if n.is_finite() { n as i32 } else { 0 })
}

pub fn create_traversal_object(
    vm: &mut JsVm,
    host: &SharedHost,
    root: NodeId,
    what_to_show: u32,
    filter: Option<HeapHandle>,
    is_iterator: bool,
) -> HeapHandle {
    let proto = if is_iterator {
        host.borrow().prototypes.node_iterator_proto
    } else {
        host.borrow().prototypes.tree_walker_proto
    };
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    host.borrow_mut().traversals.insert(
        obj,
        TraversalEntry {
            root,
            what_to_show,
            filter,
            current_node: root,
            pointer_before: true,
            active: false,
            is_iterator,
        },
    );
    obj
}

fn install_traversal_apis(vm: &mut JsVm, host: &SharedHost) {
    // Global NodeFilter object with constants
    let nf = vm.alloc_ordinary_object();
    define_to_string_tag(vm, nf, "NodeFilter");
    for &(name, val) in &[
        ("FILTER_ACCEPT", 1u32),
        ("FILTER_REJECT", 2),
        ("FILTER_SKIP", 3),
        ("SHOW_ALL", 0xFFFF_FFFF),
        ("SHOW_ELEMENT", 0x1),
        ("SHOW_ATTRIBUTE", 0x2),
        ("SHOW_TEXT", 0x4),
        ("SHOW_CDATA_SECTION", 0x8),
        ("SHOW_ENTITY_REFERENCE", 0x10),
        ("SHOW_ENTITY", 0x20),
        ("SHOW_PROCESSING_INSTRUCTION", 0x40),
        ("SHOW_COMMENT", 0x80),
        ("SHOW_DOCUMENT", 0x100),
        ("SHOW_DOCUMENT_TYPE", 0x200),
        ("SHOW_DOCUMENT_FRAGMENT", 0x400),
        ("SHOW_NOTATION", 0x800),
    ] {
        let k = vm.heap.intern_key(name);
        vm.heap.object_define_data(
            nf,
            k,
            JsValue::from_f64(f64::from(val)),
            PropertyAttrs::ENUMERABLE,
            &mut vm.shapes,
        );
    }
    define_global(vm, "NodeFilter", JsValue::from_object(nf));

    let twp = host.borrow().prototypes.tree_walker_proto;
    let nip = host.borrow().prototypes.node_iterator_proto;

    // Common getters on both TreeWalker.prototype and NodeIterator.prototype
    for &proto in &[twp, nip] {
        let h = Rc::clone(host);
        let g = vm.alloc_host_fn("get root", move |vm, t, _| {
            let root = t
                .as_object()
                .and_then(|oh| h.borrow().traversals.get(&oh).map(|e| e.root))
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            Ok(wrap_node(vm, &h, root))
        });
        define_host_accessor(vm, proto, "root", Some(g), None);

        let h = Rc::clone(host);
        let g = vm.alloc_host_fn("get whatToShow", move |vm, t, _| {
            let w = t
                .as_object()
                .and_then(|oh| h.borrow().traversals.get(&oh).map(|e| e.what_to_show))
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            Ok(JsValue::from_f64(f64::from(w)))
        });
        define_host_accessor(vm, proto, "whatToShow", Some(g), None);

        let h = Rc::clone(host);
        let g = vm.alloc_host_fn("get filter", move |vm, t, _| {
            let f = t
                .as_object()
                .and_then(|oh| h.borrow().traversals.get(&oh).map(|e| e.filter))
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            Ok(f.map_or(JsValue::NULL, JsValue::from_object))
        });
        define_host_accessor(vm, proto, "filter", Some(g), None);
    }

    // TreeWalker.prototype.currentNode (getter + setter)
    let h1 = Rc::clone(host);
    let g_cn = vm.alloc_host_fn("get currentNode", move |vm, t, _| {
        let cn = t
            .as_object()
            .and_then(|oh| h1.borrow().traversals.get(&oh).map(|e| e.current_node))
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(wrap_node(vm, &h1, cn))
    });
    let h2 = Rc::clone(host);
    let s_cn = vm.alloc_host_fn("set currentNode", move |vm, t, args| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let nid = require_node(vm, &h2, arg(args, 0))?;
        if let Some(e) = h2.borrow_mut().traversals.get_mut(&oh) {
            e.current_node = nid;
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, twp, "currentNode", Some(g_cn), Some(s_cn));

    // TreeWalker.prototype.parentNode()
    let h = Rc::clone(host);
    define_host_method(vm, twp, "parentNode", move |vm, t, _| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let (root, mut node) = h
            .borrow()
            .traversals
            .get(&oh)
            .map(|e| (e.root, e.current_node))
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        while node != root {
            let Some(p) = h.borrow().document.borrow().parent(node) else {
                break;
            };
            node = p;
            if filter_node(vm, &h, oh, node)? == 1 {
                if let Some(e) = h.borrow_mut().traversals.get_mut(&oh) {
                    e.current_node = node;
                }
                return Ok(wrap_node(vm, &h, node));
            }
        }
        Ok(JsValue::NULL)
    });

    // TreeWalker.prototype.firstChild() / lastChild()
    for (mname, is_first) in [("firstChild", true), ("lastChild", false)] {
        let h = Rc::clone(host);
        define_host_method(vm, twp, mname, move |vm, t, _| {
            let Some(oh) = t.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let current = h
                .borrow()
                .traversals
                .get(&oh)
                .map(|e| e.current_node)
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            let mut node = {
                let doc_rc = Rc::clone(&h.borrow().document);
                let doc = doc_rc.borrow();
                if is_first {
                    doc.first_child(current)
                } else {
                    doc.last_child(current)
                }
            };
            while let Some(n) = node {
                match filter_node(vm, &h, oh, n)? {
                    1 => {
                        if let Some(e) = h.borrow_mut().traversals.get_mut(&oh) {
                            e.current_node = n;
                        }
                        return Ok(wrap_node(vm, &h, n));
                    }
                    3 => {
                        let child = {
                            let doc_rc = Rc::clone(&h.borrow().document);
                            let doc = doc_rc.borrow();
                            if is_first {
                                doc.first_child(n)
                            } else {
                                doc.last_child(n)
                            }
                        };
                        if child.is_some() {
                            node = child;
                            continue;
                        }
                    }
                    _ => {}
                }
                // Traverse sibling or climb back up toward `current`.
                let mut cur = n;
                node = loop {
                    let sib = {
                        let doc_rc = Rc::clone(&h.borrow().document);
                        let doc = doc_rc.borrow();
                        if is_first {
                            doc.next_sibling(cur)
                        } else {
                            doc.previous_sibling(cur)
                        }
                    };
                    if sib.is_some() {
                        break sib;
                    }
                    let parent = h.borrow().document.borrow().parent(cur);
                    match parent {
                        Some(p) if p != current => cur = p,
                        _ => break None,
                    }
                };
            }
            Ok(JsValue::NULL)
        });
    }

    // TreeWalker.prototype.nextSibling() / previousSibling()
    for (mname, is_next) in [("nextSibling", true), ("previousSibling", false)] {
        let h = Rc::clone(host);
        define_host_method(vm, twp, mname, move |vm, t, _| {
            let Some(oh) = t.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let (root, mut node) = h
                .borrow()
                .traversals
                .get(&oh)
                .map(|e| (e.root, e.current_node))
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            if node == root {
                return Ok(JsValue::NULL);
            }
            loop {
                let mut sib = {
                    let doc_rc = Rc::clone(&h.borrow().document);
                    let doc = doc_rc.borrow();
                    if is_next {
                        doc.next_sibling(node)
                    } else {
                        doc.previous_sibling(node)
                    }
                };
                while let Some(s) = sib {
                    node = s;
                    let res = filter_node(vm, &h, oh, node)?;
                    if res == 1 {
                        if let Some(e) = h.borrow_mut().traversals.get_mut(&oh) {
                            e.current_node = node;
                        }
                        return Ok(wrap_node(vm, &h, node));
                    }
                    sib = if res == 3 {
                        let doc_rc = Rc::clone(&h.borrow().document);
                        let doc = doc_rc.borrow();
                        if is_next {
                            doc.first_child(node)
                        } else {
                            doc.last_child(node)
                        }
                    } else {
                        None
                    };
                    if sib.is_none() {
                        let doc_rc = Rc::clone(&h.borrow().document);
                        let doc = doc_rc.borrow();
                        sib = if is_next {
                            doc.next_sibling(node)
                        } else {
                            doc.previous_sibling(node)
                        };
                    }
                }
                let parent = h.borrow().document.borrow().parent(node);
                let Some(p) = parent else {
                    return Ok(JsValue::NULL);
                };
                if p == root {
                    return Ok(JsValue::NULL);
                }
                if filter_node(vm, &h, oh, p)? == 1 {
                    return Ok(JsValue::NULL);
                }
                node = p;
            }
        });
    }

    // TreeWalker.prototype.nextNode()
    let h = Rc::clone(host);
    define_host_method(vm, twp, "nextNode", move |vm, t, _| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let (root, mut node) = h
            .borrow()
            .traversals
            .get(&oh)
            .map(|e| (e.root, e.current_node))
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let mut result = 1i32; // FILTER_ACCEPT
        loop {
            while result != 2 {
                let fc = h.borrow().document.borrow().first_child(node);
                let Some(c) = fc else { break };
                node = c;
                result = filter_node(vm, &h, oh, node)?;
                if result == 1 {
                    if let Some(e) = h.borrow_mut().traversals.get_mut(&oh) {
                        e.current_node = node;
                    }
                    return Ok(wrap_node(vm, &h, node));
                }
            }
            let mut temp = Some(node);
            let mut found_next = None;
            while let Some(cur) = temp {
                if cur == root {
                    break;
                }
                let ns = h.borrow().document.borrow().next_sibling(cur);
                if let Some(s) = ns {
                    found_next = Some(s);
                    break;
                }
                temp = h.borrow().document.borrow().parent(cur);
            }
            let Some(next_n) = found_next else {
                return Ok(JsValue::NULL);
            };
            node = next_n;
            result = filter_node(vm, &h, oh, node)?;
            if result == 1 {
                if let Some(e) = h.borrow_mut().traversals.get_mut(&oh) {
                    e.current_node = node;
                }
                return Ok(wrap_node(vm, &h, node));
            }
        }
    });

    // TreeWalker.prototype.previousNode()
    let h = Rc::clone(host);
    define_host_method(vm, twp, "previousNode", move |vm, t, _| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let (root, mut node) = h
            .borrow()
            .traversals
            .get(&oh)
            .map(|e| (e.root, e.current_node))
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        while node != root {
            let mut sib = h.borrow().document.borrow().previous_sibling(node);
            while let Some(s) = sib {
                node = s;
                let mut result = filter_node(vm, &h, oh, node)?;
                while result != 2 {
                    let lc = h.borrow().document.borrow().last_child(node);
                    let Some(c) = lc else { break };
                    node = c;
                    result = filter_node(vm, &h, oh, node)?;
                }
                if result == 1 {
                    if let Some(e) = h.borrow_mut().traversals.get_mut(&oh) {
                        e.current_node = node;
                    }
                    return Ok(wrap_node(vm, &h, node));
                }
                sib = h.borrow().document.borrow().previous_sibling(node);
            }
            if node == root {
                return Ok(JsValue::NULL);
            }
            let Some(p) = h.borrow().document.borrow().parent(node) else {
                return Ok(JsValue::NULL);
            };
            node = p;
            if filter_node(vm, &h, oh, node)? == 1 {
                if let Some(e) = h.borrow_mut().traversals.get_mut(&oh) {
                    e.current_node = node;
                }
                return Ok(wrap_node(vm, &h, node));
            }
        }
        Ok(JsValue::NULL)
    });

    // NodeIterator.prototype: referenceNode, pointerBeforeReferenceNode, nextNode, previousNode, detach
    let h = Rc::clone(host);
    let g_rn = vm.alloc_host_fn("get referenceNode", move |vm, t, _| {
        let rn = t
            .as_object()
            .and_then(|oh| h.borrow().traversals.get(&oh).map(|e| e.current_node))
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(wrap_node(vm, &h, rn))
    });
    define_host_accessor(vm, nip, "referenceNode", Some(g_rn), None);

    let h = Rc::clone(host);
    let g_pb = vm.alloc_host_fn("get pointerBeforeReferenceNode", move |vm, t, _| {
        let pb = t
            .as_object()
            .and_then(|oh| h.borrow().traversals.get(&oh).map(|e| e.pointer_before))
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(JsValue::from_bool(pb))
    });
    define_host_accessor(vm, nip, "pointerBeforeReferenceNode", Some(g_pb), None);

    define_host_method(vm, nip, "detach", |_vm, _, _| Ok(JsValue::UNDEFINED));

    for (mname, is_next) in [("nextNode", true), ("previousNode", false)] {
        let h = Rc::clone(host);
        define_host_method(vm, nip, mname, move |vm, t, _| {
            let Some(oh) = t.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let (root, mut node, mut before) = h
                .borrow()
                .traversals
                .get(&oh)
                .map(|e| (e.root, e.current_node, e.pointer_before))
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            loop {
                if is_next {
                    if !before {
                        let next_n = tree_next_in_root(&h.borrow().document.borrow(), root, node);
                        let Some(nn) = next_n else {
                            return Ok(JsValue::NULL);
                        };
                        node = nn;
                    } else {
                        before = false;
                    }
                } else if before {
                    let prev_n = tree_prev_in_root(&h.borrow().document.borrow(), root, node);
                    let Some(pn) = prev_n else {
                        return Ok(JsValue::NULL);
                    };
                    node = pn;
                } else {
                    before = true;
                }
                let res = filter_node(vm, &h, oh, node)?;
                if res == 1 {
                    if let Some(e) = h.borrow_mut().traversals.get_mut(&oh) {
                        e.current_node = node;
                        e.pointer_before = before;
                    }
                    return Ok(wrap_node(vm, &h, node));
                }
            }
        });
    }
}

fn tree_next_in_root(doc: &Document, root: NodeId, node: NodeId) -> Option<NodeId> {
    if let Some(fc) = doc.first_child(node) {
        return Some(fc);
    }
    let mut cur = node;
    while cur != root {
        if let Some(ns) = doc.next_sibling(cur) {
            return Some(ns);
        }
        cur = doc.parent(cur)?;
    }
    None
}

fn tree_prev_in_root(doc: &Document, root: NodeId, node: NodeId) -> Option<NodeId> {
    if node == root {
        return None;
    }
    if let Some(mut ps) = doc.previous_sibling(node) {
        while let Some(lc) = doc.last_child(ps) {
            ps = lc;
        }
        return Some(ps);
    }
    doc.parent(node)
}

/// Adjusts active `NodeIterator` reference nodes before removing `to_remove` (WHATWG DOM §6.1).
pub fn adjust_node_iterators_for_remove(host: &SharedHost, to_remove: NodeId) {
    let doc_rc = Rc::clone(&host.borrow().document);
    let doc = doc_rc.borrow();
    let mut hb = host.borrow_mut();
    for entry in hb.traversals.values_mut() {
        if !entry.is_iterator {
            continue;
        }
        if entry.root == to_remove {
            continue;
        }
        let is_inclusive_desc = entry.current_node == to_remove
            || doc.ancestors(entry.current_node).any(|a| a == to_remove);
        if !is_inclusive_desc {
            continue;
        }
        if entry.pointer_before {
            let mut next = None;
            let mut cur = to_remove;
            while cur != entry.root {
                if let Some(ns) = doc.next_sibling(cur) {
                    next = Some(ns);
                    break;
                }
                match doc.parent(cur) {
                    Some(p) => cur = p,
                    None => break,
                }
            }
            if let Some(n) = next {
                entry.current_node = n;
                continue;
            }
            entry.pointer_before = false;
        }
        if let Some(mut ps) = doc.previous_sibling(to_remove) {
            while let Some(lc) = doc.last_child(ps) {
                ps = lc;
            }
            entry.current_node = ps;
        } else if let Some(p) = doc.parent(to_remove) {
            entry.current_node = p;
        }
    }
}
