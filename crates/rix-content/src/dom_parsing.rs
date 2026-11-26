//! W3C DOM Parsing & Serialization + WHATWG Dynamic Markup Insertion:
//! - `DOMParser` & `DOMParser.prototype.parseFromString(str, type)`
//! - `XMLSerializer` & `XMLSerializer.prototype.serializeToString(node)` (W3C DOM-Parsing §3.2)
//! - `Element.prototype.{innerHTML, outerHTML, insertAdjacentHTML, getHTML}`
//! - `ShadowRoot.prototype.{innerHTML, getHTML}`
//! - `Document.parseHTML(html, opts)` & `Document.parseHTMLUnsafe(html, opts)`

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use rix_dom::{Document, ElementData, LocalName, Namespace, NodeData, NodeId, QualName};
use rix_html::{parse_html_fragment_with_options, parse_html_with_options};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::dom_bridge::{
    arg, define_global, define_host_accessor, define_host_method, extract_node,
    notify_custom_element_subtree, require_node, wrap_node, DomMutation, SharedHost,
};
use crate::dom_interfaces::{
    is_valid_xml_char, is_valid_xml_name, parse_xml_fragment_into_node, parse_xml_into_document,
    DocumentMeta, HTML_NS, SVG_NS, XMLNS_NS, XML_NS,
};
use crate::event_loop::SharedEventLoop;
use crate::events::SharedEvents;
use crate::trusted_types::{check_classic_html_sink_options, check_trusted_html_sink};
use crate::web_apis::{create_dom_exception, SharedWebApis};

const PARSERERROR_NS: &str = "http://www.mozilla.org/newlayout/xml/parsererror.xml";

const HTML_VOID_ELEMENTS: &[&str] = &[
    "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "img", "input",
    "keygen", "link", "meta", "param", "source", "track", "wbr",
];

/// Converts a JS argument with WebIDL `[LegacyNullToEmptyString]` semantics
/// (`null` -> `""`, `undefined` -> `"undefined"`, objects via `ToString`).
pub(crate) fn legacy_null_to_empty_string(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<String, JsException> {
    if val.is_null() {
        return Ok(String::new());
    }
    vm.to_js_string(val)
}

/// Converts a JS argument via WebIDL `DOMString` (`ToString`, throwing on `Symbol`).
pub(crate) fn to_dom_string(vm: &mut JsVm, val: JsValue) -> Result<String, JsException> {
    vm.to_js_string(val)
}

/// Installs `DOMParser`, `XMLSerializer`, `Document.parseHTML{Unsafe}`,
/// `Element.prototype.{innerHTML, outerHTML, insertAdjacentHTML, getHTML}`,
/// and `ShadowRoot.prototype.{innerHTML, getHTML}`.
pub fn install_dom_parsing(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
) {
    install_dom_parser_and_xml_serializer(vm, host, web_apis);
    install_html_parsing_and_serialization(vm, host, events, event_loop, web_apis);
    install_document_parse_html_statics(vm, host, web_apis);
    install_document_domain(vm, host, web_apis);
}

fn install_document_domain(vm: &mut JsVm, host: &SharedHost, web_apis: &SharedWebApis) {
    let dp = host.borrow().prototypes.document_proto;
    let h = Rc::clone(host);
    let wa = Rc::clone(web_apis);
    let g = vm.alloc_host_fn("get domain", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let url_str = h.borrow().doc_url_for_node(doc_id);
        let host_str = url::Url::parse(&url_str)
            .ok()
            .and_then(|u| u.host_str().map(ToString::to_string))
            .or_else(|| {
                url::Url::parse(&wa.borrow().current_href())
                    .ok()
                    .and_then(|u| u.host_str().map(ToString::to_string))
            })
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(host_str)))
    });
    let s = vm.alloc_host_fn("set domain", move |_vm, _t, _args| Ok(JsValue::UNDEFINED));
    define_host_accessor(vm, dp, "domain", Some(g), Some(s));

    // SVG 2 `Document.prototype.rootElement`: returns `<svg>` root element or `null`.
    let h_re = Rc::clone(host);
    let g_re = vm.alloc_host_fn("get rootElement", move |vm, t, _| {
        let doc_id = require_node(vm, &h_re, t)?;
        let root_svg = {
            let doc_rc = Rc::clone(&h_re.borrow().document);
            let doc = doc_rc.borrow();
            let found = doc.children(doc_id).find(|&c| {
                matches!(
                    doc.get(c),
                    Some(NodeData::Element(el))
                        if &*el.name.ns == crate::dom_interfaces::SVG_NS
                            && el.name.local.as_str() == "svg"
                )
            });
            found
        };
        Ok(root_svg.map_or(JsValue::NULL, |n| wrap_node(vm, &h_re, n)))
    });
    define_host_accessor(vm, dp, "rootElement", Some(g_re), None);
}

// ---------------------------------------------------------------------------
// DOMParser & XMLSerializer
// ---------------------------------------------------------------------------

fn install_dom_parser_and_xml_serializer(
    vm: &mut JsVm,
    host: &SharedHost,
    web_apis: &SharedWebApis,
) {
    let dom_parser_proto = vm.alloc_ordinary_object();
    vm.heap
        .get_object_mut(dom_parser_proto)
        .unwrap()
        .is_prototype = true;

    let h_dp = Rc::clone(host);
    let wa_dp = Rc::clone(web_apis);
    define_host_method(
        vm,
        dom_parser_proto,
        "parseFromString",
        move |vm, this_val, args| {
            let Some(this_h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let k_url = vm.heap.intern_key("__rix_domparser_url__");
            let url_val = vm.heap.object_get_own(this_h, k_url, &vm.shapes);
            let Some(url_js) = url_val else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let captured_url = vm.value_to_rust_string(url_js.value);
            if args.len() < 2 {
                return Err(vm.type_error("DOMParser.parseFromString requires 2 arguments"));
            }
            let markup = to_dom_string(vm, arg(args, 0))?;
            let mime = to_dom_string(vm, arg(args, 1))?;
            let doc_nid = parse_from_string_impl(vm, &h_dp, &wa_dp, &markup, &mime, &captured_url)?;
            Ok(wrap_node(vm, &h_dp, doc_nid))
        },
    );

    host.borrow_mut().dom_parser_proto = Some(dom_parser_proto);
    let dom_parser_ctor = create_dom_parser_ctor_for_doc(vm, host, web_apis, None);
    define_global(vm, "DOMParser", JsValue::from_object(dom_parser_ctor));

    // XMLSerializer
    let xml_ser_proto = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(xml_ser_proto).unwrap().is_prototype = true;

    let h_xs = Rc::clone(host);
    define_host_method(
        vm,
        xml_ser_proto,
        "serializeToString",
        move |vm, _, args| {
            if args.is_empty() {
                return Err(vm.type_error("XMLSerializer.serializeToString requires 1 argument"));
            }
            let target = arg(args, 0);
            // `Attr` nodes serialize to the empty string per W3C DOM-Parsing.
            if let Some(ah) = target.as_object() {
                if h_xs.borrow().attrs.contains_key(&ah) {
                    return Ok(JsValue::from_string(vm.heap.alloc_string("")));
                }
            }
            let nid = require_node(vm, &h_xs, target)?;
            let serialized = {
                let doc_rc = Rc::clone(&h_xs.borrow().document);
                let doc = doc_rc.borrow();
                xml_serialize_node(&doc, nid, false).unwrap_or_default()
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(serialized)))
        },
    );

    let xml_ser_ctor = vm.alloc_host_fn("XMLSerializer", move |vm, _, _| {
        let inst = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(xml_ser_proto),
        ));
        Ok(JsValue::from_object(inst))
    });
    let k_proto = vm.key_prototype;
    vm.heap.object_define_data(
        xml_ser_ctor,
        k_proto,
        JsValue::from_object(xml_ser_proto),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let k_ctor = vm.heap.intern_key("constructor");
    vm.heap.object_define_data(
        xml_ser_proto,
        k_ctor,
        JsValue::from_object(xml_ser_ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.mark_constructor(xml_ser_ctor);
    define_global(vm, "XMLSerializer", JsValue::from_object(xml_ser_ctor));
}

/// Creates a `DOMParser` constructor bound to `doc_id_opt` (`None` = main window's live URL,
/// `Some(child_doc_id)` = same-origin child iframe's document URL at construction time).
pub(crate) fn create_dom_parser_ctor_for_doc(
    vm: &mut JsVm,
    host: &SharedHost,
    web_apis: &SharedWebApis,
    doc_id_opt: Option<NodeId>,
) -> HeapHandle {
    let dom_parser_proto = host.borrow().dom_parser_proto.unwrap_or(vm.object_proto);
    let h_c = Rc::clone(host);
    let wa_c = Rc::clone(web_apis);
    let ctor = vm.alloc_host_fn("DOMParser", move |vm, _, _| {
        let captured_url = match doc_id_opt {
            Some(did) => h_c.borrow().doc_url_for_node(did),
            None => wa_c.borrow().current_href(),
        };
        let inst = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(dom_parser_proto),
        ));
        let k_url = vm.heap.intern_key("__rix_domparser_url__");
        let url_js = JsValue::from_string(vm.heap.alloc_string(captured_url));
        vm.heap
            .object_define_data(inst, k_url, url_js, PropertyAttrs::empty(), &mut vm.shapes);
        Ok(JsValue::from_object(inst))
    });
    let k_proto = vm.key_prototype;
    vm.heap.object_define_data(
        ctor,
        k_proto,
        JsValue::from_object(dom_parser_proto),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    if doc_id_opt.is_none() {
        let k_ctor = vm.heap.intern_key("constructor");
        vm.heap.object_define_data(
            dom_parser_proto,
            k_ctor,
            JsValue::from_object(ctor),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }
    vm.mark_constructor(ctor);
    ctor
}

fn parse_from_string_impl(
    vm: &mut JsVm,
    host: &SharedHost,
    _web_apis: &SharedWebApis,
    markup: &str,
    mime: &str,
    captured_url: &str,
) -> Result<NodeId, JsException> {
    match mime {
        "text/html" => {
            // WHATWG HTML §8.4: scripting disabled, no declarative shadow roots,
            // characterSet always "UTF-8", location === null, Document prototype (not XMLDocument).
            let parsed = parse_html_with_options(markup, false, false);
            let qm = parsed.quirks_mode();
            let sub_root = {
                let doc_rc = Rc::clone(&host.borrow().document);
                let mut doc = doc_rc.borrow_mut();
                let r = doc.new_node(NodeData::Document);
                let pr = parsed.root();
                for child in parsed.children(pr) {
                    let cloned = doc.clone_subtree_from(&parsed, child);
                    doc.append_child(r, cloned);
                }
                r
            };
            let mut meta = DocumentMeta::html(captured_url);
            meta.use_html_document_proto = false;
            meta.use_xml_document_proto = false;
            meta.character_set = "UTF-8".to_string();
            meta.quirks_mode = qm;
            meta.default_view = None;
            {
                let mut hb = host.borrow_mut();
                hb.doc_meta.insert(sub_root, meta);
                hb.adopt_subtree(sub_root, sub_root);
            }
            Ok(sub_root)
        }
        "text/xml" | "application/xml" | "application/xhtml+xml" | "image/svg+xml" => {
            // WHATWG HTML §8.4: returns `Document` (NOT `XMLDocument`), `characterSet === "UTF-8"`,
            // `location === null`, `<parsererror>` document on any well-formedness error.
            let sub_root = {
                let doc_rc = Rc::clone(&host.borrow().document);
                let mut doc = doc_rc.borrow_mut();
                let r = doc.new_node(NodeData::Document);
                if let Err(err_msg) = parse_xml_into_document(&mut doc, r, markup) {
                    let partial: Vec<NodeId> = doc.children(r).collect();
                    for c in partial {
                        doc.remove_child(c);
                    }
                    let pe = doc.new_node(NodeData::Element(ElementData::new(QualName::new(
                        None,
                        Namespace::from(PARSERERROR_NS),
                        LocalName::from("parsererror"),
                    ))));
                    let st = doc.new_node(NodeData::Element(ElementData::new(QualName::new(
                        None,
                        Namespace::from(PARSERERROR_NS),
                        LocalName::from("sourcetext"),
                    ))));
                    let tn = doc.create_text(&err_msg);
                    doc.append_child(st, tn);
                    doc.append_child(pe, st);
                    doc.append_child(r, pe);
                }
                r
            };
            let mut meta = DocumentMeta::xml_generic(captured_url);
            meta.content_type = mime.to_string();
            meta.use_xml_document_proto = false;
            meta.character_set = "UTF-8".to_string();
            meta.quirks_mode = 0;
            meta.default_view = None;
            {
                let mut hb = host.borrow_mut();
                hb.doc_meta.insert(sub_root, meta);
                hb.adopt_subtree(sub_root, sub_root);
            }
            Ok(sub_root)
        }
        _ => Err(vm.type_error(format!(
            "Failed to execute 'parseFromString' on 'DOMParser': The provided value '{mime}' is not a valid enum value of type SupportedType."
        ))),
    }
}

// ---------------------------------------------------------------------------
// Document.parseHTML / Document.parseHTMLUnsafe
// ---------------------------------------------------------------------------

fn install_document_parse_html_statics(
    vm: &mut JsVm,
    host: &SharedHost,
    _web_apis: &SharedWebApis,
) {
    let k_doc = vm.heap.intern_key("Document");
    let g = vm.global;
    let Some(doc_ctor) = vm
        .heap
        .object_get_own(g, k_doc, &vm.shapes)
        .and_then(|v| v.value.as_object())
    else {
        return;
    };

    for (name, allow_unsafe) in [("parseHTML", false), ("parseHTMLUnsafe", true)] {
        let h = Rc::clone(host);
        define_host_method(vm, doc_ctor, name, move |vm, _, args| {
            let html = to_dom_string(vm, arg(args, 0))?;
            let parsed = parse_html_with_options(&html, false, allow_unsafe);
            let qm = parsed.quirks_mode();
            let sub_root = {
                let doc_rc = Rc::clone(&h.borrow().document);
                let mut doc = doc_rc.borrow_mut();
                let r = doc.new_node(NodeData::Document);
                let pr = parsed.root();
                for child in parsed.children(pr) {
                    let cloned = doc.clone_subtree_from(&parsed, child);
                    doc.append_child(r, cloned);
                }
                if !allow_unsafe {
                    sanitize_default_safe_subtree(&mut doc, r);
                }
                r
            };
            let mut meta = DocumentMeta::html("about:blank");
            meta.use_html_document_proto = false;
            meta.use_xml_document_proto = false;
            meta.character_set = "UTF-8".to_string();
            meta.quirks_mode = qm;
            meta.default_view = None;
            {
                let mut hb = h.borrow_mut();
                hb.doc_meta.insert(sub_root, meta);
                hb.adopt_subtree(sub_root, sub_root);
            }
            Ok(wrap_node(vm, &h, sub_root))
        });
    }
}

/// Minimal safe default sanitizer for `Document.parseHTML` (full configurable
/// WHATWG `Sanitizer` arrives in Phase 6B-5b).
fn sanitize_default_safe_subtree(doc: &mut Document, root: NodeId) {
    let children: Vec<NodeId> = doc.children(root).collect();
    for child in children {
        let remove = match doc.get(child) {
            Some(NodeData::Element(el)) => matches!(
                el.name.local.as_str(),
                "script" | "iframe" | "frame" | "frameset" | "object" | "embed" | "base"
            ),
            Some(NodeData::Comment(_)) => true,
            _ => false,
        };
        if remove {
            doc.remove_child(child);
            continue;
        }
        if let Some(NodeData::Element(el)) = doc.get_mut(child) {
            el.attributes
                .retain(|(q, _)| !q.local.as_str().to_ascii_lowercase().starts_with("on"));
        }
        sanitize_default_safe_subtree(doc, child);
    }
}

// ---------------------------------------------------------------------------
// innerHTML / outerHTML / insertAdjacentHTML / getHTML
// ---------------------------------------------------------------------------

fn install_html_parsing_and_serialization(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    _web_apis: &SharedWebApis,
) {
    let ep = host.borrow().prototypes.element_proto;
    let srp = host.borrow().prototypes.shadow_root_proto;

    // Element.prototype.innerHTML
    {
        let h_g = Rc::clone(host);
        let g = vm.alloc_host_fn("get innerHTML", move |vm, t, _| {
            let nid = require_node(vm, &h_g, t)?;
            serialize_inner_html_for_node(vm, &h_g, nid)
        });
        let h_s = Rc::clone(host);
        let evs_s = Rc::clone(events);
        let el_s = Rc::clone(event_loop);
        let s = vm.alloc_host_fn("set innerHTML", move |vm, t, args| {
            let nid = require_node(vm, &h_s, t)?;
            let val = arg(args, 0);
            let trusted_opts =
                check_classic_html_sink_options(vm, &h_s, &evs_s, &el_s, nid, "Element innerHTML")?;
            let markup = if trusted_opts.is_some() {
                if let Some(s) = h_s.borrow().trusted_types.is_trusted_html(val) {
                    s
                } else {
                    legacy_null_to_empty_string(vm, val)?
                }
            } else {
                check_trusted_html_sink(
                    vm,
                    &h_s,
                    &evs_s,
                    &el_s,
                    nid,
                    "Element innerHTML",
                    val,
                    true,
                )?
            };
            let run_scripts = trusted_opts.is_some_and(|o| o.run_scripts);
            set_inner_html_spec(vm, &h_s, nid, &markup, false, run_scripts)?;
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, ep, "innerHTML", Some(g), Some(s));
    }

    // ShadowRoot.prototype.innerHTML
    {
        let h_g = Rc::clone(host);
        let g = vm.alloc_host_fn("get innerHTML", move |vm, t, _| {
            let nid = require_node(vm, &h_g, t)?;
            serialize_inner_html_for_node(vm, &h_g, nid)
        });
        let h_s = Rc::clone(host);
        let evs_s = Rc::clone(events);
        let el_s = Rc::clone(event_loop);
        let s = vm.alloc_host_fn("set innerHTML", move |vm, t, args| {
            let nid = require_node(vm, &h_s, t)?;
            let val = arg(args, 0);
            let trusted_opts = check_classic_html_sink_options(
                vm,
                &h_s,
                &evs_s,
                &el_s,
                nid,
                "ShadowRoot innerHTML",
            )?;
            let markup = if trusted_opts.is_some() {
                if let Some(s) = h_s.borrow().trusted_types.is_trusted_html(val) {
                    s
                } else {
                    legacy_null_to_empty_string(vm, val)?
                }
            } else {
                check_trusted_html_sink(
                    vm,
                    &h_s,
                    &evs_s,
                    &el_s,
                    nid,
                    "ShadowRoot innerHTML",
                    val,
                    true,
                )?
            };
            let run_scripts = trusted_opts.is_some_and(|o| o.run_scripts);
            set_inner_html_spec(vm, &h_s, nid, &markup, false, run_scripts)?;
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, srp, "innerHTML", Some(g), Some(s));
    }

    // Element.prototype.outerHTML
    {
        let h_g = Rc::clone(host);
        let g = vm.alloc_host_fn("get outerHTML", move |vm, t, _| {
            let nid = require_node(vm, &h_g, t)?;
            let is_xml = h_g.borrow().is_xml_doc(h_g.borrow().owner_doc_of(nid));
            let res = {
                let doc_rc = Rc::clone(&h_g.borrow().document);
                let doc = doc_rc.borrow();
                if is_xml {
                    xml_serialize_node(&doc, nid, true)
                } else {
                    Ok(doc.to_html_string(nid))
                }
            };
            match res {
                Ok(s) => Ok(JsValue::from_string(vm.heap.alloc_string(s))),
                Err(msg) => Err(create_dom_exception(vm, "InvalidStateError", &msg)),
            }
        });
        let h_s = Rc::clone(host);
        let evs_s = Rc::clone(events);
        let el_s = Rc::clone(event_loop);
        let s = vm.alloc_host_fn("set outerHTML", move |vm, t, args| {
            let nid = require_node(vm, &h_s, t)?;
            let val = arg(args, 0);
            let trusted_opts =
                check_classic_html_sink_options(vm, &h_s, &evs_s, &el_s, nid, "Element outerHTML")?;
            let markup = if trusted_opts.is_some() {
                if let Some(s) = h_s.borrow().trusted_types.is_trusted_html(val) {
                    s
                } else {
                    legacy_null_to_empty_string(vm, val)?
                }
            } else {
                check_trusted_html_sink(
                    vm,
                    &h_s,
                    &evs_s,
                    &el_s,
                    nid,
                    "Element outerHTML",
                    val,
                    true,
                )?
            };
            let run_scripts = trusted_opts.is_some_and(|o| o.run_scripts);
            set_outer_html_spec(vm, &h_s, nid, &markup, run_scripts)?;
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, ep, "outerHTML", Some(g), Some(s));
    }

    // Element.prototype.insertAdjacentHTML(position, text)
    {
        let h = Rc::clone(host);
        let evs = Rc::clone(events);
        let el = Rc::clone(event_loop);
        define_host_method(vm, ep, "insertAdjacentHTML", move |vm, t, args| {
            let nid = require_node(vm, &h, t)?;
            if args.len() < 2 {
                return Err(vm.type_error("insertAdjacentHTML requires 2 arguments"));
            }
            let pos = to_dom_string(vm, arg(args, 0))?;
            let val = arg(args, 1);
            let trusted_opts = check_classic_html_sink_options(
                vm,
                &h,
                &evs,
                &el,
                nid,
                "Element insertAdjacentHTML",
            )?;
            let markup = if trusted_opts.is_some() {
                if let Some(s) = h.borrow().trusted_types.is_trusted_html(val) {
                    s
                } else {
                    to_dom_string(vm, val)?
                }
            } else {
                check_trusted_html_sink(
                    vm,
                    &h,
                    &evs,
                    &el,
                    nid,
                    "Element insertAdjacentHTML",
                    val,
                    false,
                )?
            };
            let run_scripts = trusted_opts.is_some_and(|o| o.run_scripts);
            insert_adjacent_html_spec(vm, &h, nid, &pos, &markup, run_scripts)?;
            Ok(JsValue::UNDEFINED)
        });
    }

    // Element.prototype.getHTML(options) & ShadowRoot.prototype.getHTML(options)
    for proto in [ep, srp] {
        let h = Rc::clone(host);
        define_host_method(vm, proto, "getHTML", move |vm, t, args| {
            let nid = require_node(vm, &h, t)?;
            let mut serializable_shadow_roots = false;
            let mut allowed_shadow_roots: HashSet<NodeId> = HashSet::new();
            if let Some(opts_h) = arg(args, 0).as_object() {
                let k_ssr = vm.heap.intern_key("serializableShadowRoots");
                if let Ok(v) = vm.get_property_by_key(opts_h, k_ssr) {
                    serializable_shadow_roots = vm.to_boolean(v);
                }
                let k_sr = vm.heap.intern_key("shadowRoots");
                if let Ok(v) = vm.get_property_by_key(opts_h, k_sr) {
                    if let Some(arr_h) = v.as_object() {
                        let k_len = vm.heap.intern_key("length");
                        let len = vm
                            .get_property_by_key(arr_h, k_len)
                            .ok()
                            .and_then(|l| l.as_f64())
                            .unwrap_or(0.0) as usize;
                        for i in 0..len {
                            let ki = vm.heap.intern_key(&i.to_string());
                            if let Ok(item) = vm.get_property_by_key(arr_h, ki) {
                                if let Some(sr_id) = extract_node(vm, &h, item) {
                                    allowed_shadow_roots.insert(sr_id);
                                }
                            }
                        }
                    }
                }
            }
            let html = {
                let doc_rc = Rc::clone(&h.borrow().document);
                let doc = doc_rc.borrow();
                serialize_html_children_with_shadows(
                    &doc,
                    nid,
                    serializable_shadow_roots,
                    &allowed_shadow_roots,
                )
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(html)))
        });
    }
}

fn serialize_inner_html_for_node(
    vm: &mut JsVm,
    host: &SharedHost,
    nid: NodeId,
) -> Result<JsValue, JsException> {
    let is_xml = host.borrow().is_xml_doc(host.borrow().owner_doc_of(nid));
    let res = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        let container = match doc.get(nid) {
            Some(NodeData::Element(e)) => e.template_contents.unwrap_or(nid),
            _ => nid,
        };
        if is_xml {
            let mut buf = String::new();
            let mut err = None;
            let mut prefix_map = init_xml_prefix_map_for_ancestors(&doc, nid);
            let mut prefix_idx = 1u32;
            let inherited_ns = inherited_default_ns_for_element(&doc, nid);
            for c in doc.children(container) {
                match xml_serialize_node_inner(
                    &doc,
                    c,
                    inherited_ns.as_deref(),
                    &mut prefix_map,
                    &mut prefix_idx,
                    true,
                    &mut buf,
                ) {
                    Ok(()) => {}
                    Err(e) => {
                        err = Some(e);
                        break;
                    }
                }
            }
            match err {
                Some(e) => Err(e),
                None => Ok(buf),
            }
        } else {
            let mut buf = String::new();
            for c in doc.children(container) {
                buf.push_str(&doc.to_html_string(c));
            }
            Ok(buf)
        }
    };
    match res {
        Ok(s) => Ok(JsValue::from_string(vm.heap.alloc_string(s))),
        Err(msg) => Err(create_dom_exception(vm, "InvalidStateError", &msg)),
    }
}

fn maybe_run_fragment_scripts(vm: &mut JsVm, host: &SharedHost, added: &[NodeId], is_conn: bool) {
    let scripts: Vec<NodeId> = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        let mut out = Vec::new();
        for &root in added {
            for id in doc.descendants(root) {
                if matches!(
                    doc.get(id),
                    Some(NodeData::Element(e))
                        if e.name.local.as_str() == "script"
                            && matches!(&*e.name.ns, HTML_NS | SVG_NS)
                ) {
                    out.push(id);
                }
            }
        }
        out
    };
    if scripts.is_empty() {
        return;
    }
    let hook = {
        let mut hb = host.borrow_mut();
        for &sid in &scripts {
            hb.script_force_async.insert(sid);
            if !is_conn {
                hb.contextual_scripts.insert(sid);
            }
        }
        hb.script_connect_hook.clone()
    };
    if is_conn {
        if let Some(ref h) = hook {
            for sid in scripts {
                h(vm, sid);
            }
        }
    }
}

fn set_inner_html_spec(
    vm: &mut JsVm,
    host: &SharedHost,
    nid: NodeId,
    markup: &str,
    allow_declarative_shadow: bool,
    run_scripts: bool,
) -> Result<(), JsException> {
    let owner_doc = host.borrow().owner_doc_of(nid);
    let is_xml = host.borrow().is_xml_doc(owner_doc);
    let (ctx_name, target_container, initial_ns) = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        let ctx = match doc.get(nid) {
            Some(NodeData::Element(e)) => Some(e.name.clone()),
            Some(NodeData::ShadowRoot { host: sh, .. }) => match doc.get(*sh) {
                Some(NodeData::Element(e)) => Some(e.name.clone()),
                _ => None,
            },
            _ => None,
        };
        let target = match doc.get(nid) {
            Some(NodeData::Element(e))
                if &*e.name.ns == HTML_NS && &*e.name.local == "template" =>
            {
                e.template_contents.unwrap_or(nid)
            }
            _ => nid,
        };
        let ns_decls = if is_xml {
            collect_ancestor_xml_ns_decls(&doc, nid)
        } else {
            Vec::new()
        };
        (ctx, target, ns_decls)
    };

    let (frag_doc, frag_roots) = if is_xml {
        let mut tmp_doc = Document::new();
        let frag = tmp_doc.new_node(NodeData::DocumentFragment);
        if let Err(err_msg) = parse_xml_fragment_into_node(&mut tmp_doc, frag, markup, &initial_ns)
        {
            return Err(create_dom_exception(vm, "SyntaxError", &err_msg));
        }
        let roots: Vec<NodeId> = tmp_doc.children(frag).collect();
        (tmp_doc, roots)
    } else {
        parse_html_fragment_with_options(markup, ctx_name.as_ref(), false, allow_declarative_shadow)
    };

    let (removed, added, is_conn) = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = doc_rc.borrow_mut();
        let removed: Vec<NodeId> = doc.children(target_container).collect();
        for &c in &removed {
            doc.remove_child(c);
        }
        let mut added = Vec::with_capacity(frag_roots.len());
        for src_root in frag_roots {
            let cloned = doc.clone_subtree_from(&frag_doc, src_root);
            doc.append_child(target_container, cloned);
            added.push(cloned);
        }
        let is_conn = doc.is_connected(target_container);
        (removed, added, is_conn)
    };
    let is_template_el = target_container != nid;
    let effective_owner_doc = if is_template_el {
        host.borrow_mut().inert_template_doc_for(owner_doc)
    } else {
        owner_doc
    };
    let target_reg = if is_template_el {
        None
    } else {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        match doc.get(target_container) {
            Some(NodeData::ShadowRoot { .. }) => {
                drop(doc);
                crate::custom_elements::registry_for_shadow_root(host, target_container)
            }
            Some(NodeData::Element(_)) => {
                drop(doc);
                crate::custom_elements::effective_registry_for_element(host, target_container)
            }
            _ => {
                drop(doc);
                crate::custom_elements::registry_for_document(host, owner_doc)
            }
        }
    };
    {
        let mut hb = host.borrow_mut();
        if is_template_el {
            hb.adopt_subtree(target_container, effective_owner_doc);
        }
        for &n in &added {
            hb.adopt_subtree(n, effective_owner_doc);
        }
        hb.invalidate_dom_mutation(target_container, None);
    }
    {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        for &n in &added {
            for el in crate::custom_elements::collect_shadow_including_elements(&doc, n, true) {
                let is_null_attr = matches!(
                    doc.get(el),
                    Some(NodeData::Element(e)) if e.null_registry
                );
                if !is_null_attr {
                    crate::custom_elements::set_element_registry(host, el, target_reg);
                }
            }
        }
    }
    host.borrow().emit_mutation(DomMutation::ChildList {
        target: target_container,
        added: added.clone(),
        removed: removed.clone(),
        previous_sibling: None,
        next_sibling: None,
    });
    if is_conn {
        let _ = crate::custom_elements::with_ce_reactions(vm, host, |vm| {
            for &old in &removed {
                notify_custom_element_subtree(vm, host, old, false);
            }
            for &n in &added {
                notify_custom_element_subtree(vm, host, n, true);
            }
            Ok(())
        });
        crate::dom_bridge::notify_connected_subresources(vm, host, &added);
    } else if !is_template_el {
        let _ = crate::custom_elements::with_ce_reactions(vm, host, |vm| {
            for &n in &added {
                crate::custom_elements::upgrade_subtree(vm, host, n, true);
            }
            Ok(())
        });
    }
    if run_scripts {
        maybe_run_fragment_scripts(vm, host, &added, is_conn);
    }
    Ok(())
}

fn set_outer_html_spec(
    vm: &mut JsVm,
    host: &SharedHost,
    nid: NodeId,
    markup: &str,
    run_scripts: bool,
) -> Result<(), JsException> {
    let owner_doc = host.borrow().owner_doc_of(nid);
    let is_xml = host.borrow().is_xml_doc(owner_doc);
    let (parent_id, ctx_name, initial_ns) = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        let Some(p) = doc.parent(nid) else {
            // Detached element: no-op per W3C DOM-Parsing.
            return Ok(());
        };
        if matches!(doc.get(p), Some(NodeData::Document)) {
            return Err(create_dom_exception(
                vm,
                "NoModificationAllowedError",
                "Cannot set outerHTML on an element whose parent is a Document",
            ));
        }
        let ctx = match doc.get(p) {
            Some(NodeData::Element(e)) => Some(e.name.clone()),
            _ => Some(QualName::new(
                None,
                Namespace::from(HTML_NS),
                LocalName::from("body"),
            )),
        };
        let ns_decls = if is_xml {
            collect_ancestor_xml_ns_decls(&doc, p)
        } else {
            Vec::new()
        };
        (p, ctx, ns_decls)
    };

    let (frag_doc, frag_roots) = if is_xml {
        let mut tmp_doc = Document::new();
        let frag = tmp_doc.new_node(NodeData::DocumentFragment);
        if let Err(err_msg) = parse_xml_fragment_into_node(&mut tmp_doc, frag, markup, &initial_ns)
        {
            return Err(create_dom_exception(vm, "SyntaxError", &err_msg));
        }
        let roots: Vec<NodeId> = tmp_doc.children(frag).collect();
        (tmp_doc, roots)
    } else {
        parse_html_fragment_with_options(markup, ctx_name.as_ref(), false, false)
    };

    let (added, prev_sib, next_sib, is_conn) = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = doc_rc.borrow_mut();
        let prev_sib = doc.previous_sibling(nid);
        let next_sib = doc.next_sibling(nid);
        let mut added = Vec::with_capacity(frag_roots.len());
        for src_root in frag_roots {
            let cloned = doc.clone_subtree_from(&frag_doc, src_root);
            doc.insert_before(nid, cloned);
            added.push(cloned);
        }
        doc.remove_child(nid);
        let is_conn = doc.is_connected(parent_id);
        (added, prev_sib, next_sib, is_conn)
    };
    {
        let mut hb = host.borrow_mut();
        for &n in &added {
            hb.adopt_subtree(n, owner_doc);
        }
        hb.invalidate_dom_mutation(parent_id, None);
    }
    host.borrow().emit_mutation(DomMutation::ChildList {
        target: parent_id,
        added: added.clone(),
        removed: vec![nid],
        previous_sibling: prev_sib,
        next_sibling: next_sib,
    });
    if is_conn {
        notify_custom_element_subtree(vm, host, nid, false);
        for &n in &added {
            notify_custom_element_subtree(vm, host, n, true);
        }
        crate::dom_bridge::notify_connected_subresources(vm, host, &added);
    }
    if run_scripts {
        maybe_run_fragment_scripts(vm, host, &added, is_conn);
    }
    Ok(())
}

fn insert_adjacent_html_spec(
    vm: &mut JsVm,
    host: &SharedHost,
    nid: NodeId,
    pos_raw: &str,
    markup: &str,
    run_scripts: bool,
) -> Result<(), JsException> {
    // ASCII case-insensitive match ONLY (non-ASCII Turkish 'İ'/'ı' must throw SyntaxError).
    let pos_kind = if pos_raw.eq_ignore_ascii_case("beforebegin") {
        0u8
    } else if pos_raw.eq_ignore_ascii_case("afterbegin") {
        1u8
    } else if pos_raw.eq_ignore_ascii_case("beforeend") {
        2u8
    } else if pos_raw.eq_ignore_ascii_case("afterend") {
        3u8
    } else {
        return Err(create_dom_exception(
            vm,
            "SyntaxError",
            &format!("Invalid insertAdjacentHTML position: '{pos_raw}'"),
        ));
    };

    let owner_doc = host.borrow().owner_doc_of(nid);
    let is_xml = host.borrow().is_xml_doc(owner_doc);

    let (context_node, parent_target, ctx_name, initial_ns) = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        let (ctx_node, parent_target) = if pos_kind == 0 || pos_kind == 3 {
            let Some(p) = doc.parent(nid) else {
                return Err(create_dom_exception(
                    vm,
                    "NoModificationAllowedError",
                    "Element has no parent for beforebegin/afterend",
                ));
            };
            if matches!(doc.get(p), Some(NodeData::Document)) {
                return Err(create_dom_exception(
                    vm,
                    "NoModificationAllowedError",
                    "Cannot insertAdjacentHTML beforebegin/afterend when parent is a Document",
                ));
            }
            (p, p)
        } else {
            (nid, nid)
        };
        let ctx_qname = match doc.get(ctx_node) {
            Some(NodeData::Element(e))
                if &*e.name.ns == HTML_NS && &*e.name.local == "html" && !is_xml =>
            {
                // W3C DOM-Parsing: when context is HTML <html>, use <body> so implied head/body
                // aren't synthesized as siblings of existing head/body.
                Some(QualName::new(
                    None,
                    Namespace::from(HTML_NS),
                    LocalName::from("body"),
                ))
            }
            Some(NodeData::Element(e)) => Some(e.name.clone()),
            _ => Some(QualName::new(
                None,
                Namespace::from(HTML_NS),
                LocalName::from("body"),
            )),
        };
        let ns_decls = if is_xml {
            collect_ancestor_xml_ns_decls(&doc, ctx_node)
        } else {
            Vec::new()
        };
        (ctx_node, parent_target, ctx_qname, ns_decls)
    };
    let _ = context_node;

    let (frag_doc, frag_roots) = if is_xml {
        let mut tmp_doc = Document::new();
        let frag = tmp_doc.new_node(NodeData::DocumentFragment);
        if let Err(err_msg) = parse_xml_fragment_into_node(&mut tmp_doc, frag, markup, &initial_ns)
        {
            return Err(create_dom_exception(vm, "SyntaxError", &err_msg));
        }
        let roots: Vec<NodeId> = tmp_doc.children(frag).collect();
        (tmp_doc, roots)
    } else {
        parse_html_fragment_with_options(markup, ctx_name.as_ref(), false, false)
    };

    if frag_roots.is_empty() {
        return Ok(());
    }

    let (added, prev_sib, next_sib, is_conn) = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = doc_rc.borrow_mut();
        let ref_node = match pos_kind {
            0 => Some(nid),
            1 => doc.first_child(nid),
            2 => None,
            _ => doc.next_sibling(nid),
        };
        let prev_sib = ref_node
            .and_then(|r| doc.previous_sibling(r))
            .or_else(|| doc.last_child(parent_target));
        let mut added = Vec::with_capacity(frag_roots.len());
        for src_root in frag_roots {
            let cloned = doc.clone_subtree_from(&frag_doc, src_root);
            if let Some(rn) = ref_node {
                doc.insert_before(rn, cloned);
            } else {
                doc.append_child(parent_target, cloned);
            }
            added.push(cloned);
        }
        let is_conn = doc.is_connected(parent_target);
        (added, prev_sib, ref_node, is_conn)
    };
    {
        let mut hb = host.borrow_mut();
        for &n in &added {
            hb.adopt_subtree(n, owner_doc);
        }
        hb.invalidate_dom_mutation(parent_target, added.first().copied());
    }
    host.borrow().emit_mutation(DomMutation::ChildList {
        target: parent_target,
        added: added.clone(),
        removed: Vec::new(),
        previous_sibling: prev_sib,
        next_sibling: next_sib,
    });
    if is_conn {
        for &n in &added {
            notify_custom_element_subtree(vm, host, n, true);
        }
        crate::dom_bridge::notify_connected_subresources(vm, host, &added);
    }
    if run_scripts {
        maybe_run_fragment_scripts(vm, host, &added, is_conn);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// HTML Serialization with Declarative Shadow Roots (`Element.prototype.getHTML`)
// ---------------------------------------------------------------------------

fn serialize_shadow_root_if_allowed(
    doc: &Document,
    sr_id: NodeId,
    serializable_shadow_roots: bool,
    allowed_shadow_roots: &HashSet<NodeId>,
    out: &mut String,
) {
    if let Some(NodeData::ShadowRoot {
        mode,
        delegates_focus,
        clonable,
        serializable,
        ..
    }) = doc.get(sr_id)
    {
        if (*serializable && serializable_shadow_roots) || allowed_shadow_roots.contains(&sr_id) {
            out.push_str("<template shadowrootmode=\"");
            out.push_str(mode);
            out.push('"');
            if *delegates_focus {
                out.push_str(" shadowrootdelegatesfocus=\"\"");
            }
            if *serializable {
                out.push_str(" shadowrootserializable=\"\"");
            }
            if *clonable {
                out.push_str(" shadowrootclonable=\"\"");
            }
            out.push('>');
            for sr_child in doc.children(sr_id) {
                serialize_html_node_with_shadows(
                    doc,
                    sr_child,
                    serializable_shadow_roots,
                    allowed_shadow_roots,
                    out,
                );
            }
            out.push_str("</template>");
        }
    }
}

fn serialize_html_children_with_shadows(
    doc: &Document,
    container: NodeId,
    serializable_shadow_roots: bool,
    allowed_shadow_roots: &HashSet<NodeId>,
) -> String {
    let mut out = String::new();
    let (actual_container, own_shadow) = match doc.get(container) {
        Some(NodeData::Element(el)) if &*el.name.ns == HTML_NS && &*el.name.local == "template" => {
            (el.template_contents.unwrap_or(container), None)
        }
        Some(NodeData::Element(el)) => (container, el.shadow_root),
        _ => (container, None),
    };
    if let Some(sr_id) = own_shadow {
        serialize_shadow_root_if_allowed(
            doc,
            sr_id,
            serializable_shadow_roots,
            allowed_shadow_roots,
            &mut out,
        );
    }
    for child in doc.children(actual_container) {
        serialize_html_node_with_shadows(
            doc,
            child,
            serializable_shadow_roots,
            allowed_shadow_roots,
            &mut out,
        );
    }
    out
}

fn serialize_html_node_with_shadows(
    doc: &Document,
    node: NodeId,
    serializable_shadow_roots: bool,
    allowed_shadow_roots: &HashSet<NodeId>,
    out: &mut String,
) {
    match doc.get(node) {
        Some(NodeData::Element(el)) => {
            out.push('<');
            if !matches!(
                &*el.name.ns,
                HTML_NS | "http://www.w3.org/2000/svg" | "http://www.w3.org/1998/Math/MathML"
            ) {
                if let Some(p) = &el.name.prefix {
                    out.push_str(p);
                    out.push(':');
                }
            }
            out.push_str(&el.name.local);
            for (name, value) in &el.attributes {
                out.push(' ');
                let prefix = match &*name.ns {
                    XML_NS => Some("xml"),
                    XMLNS_NS if &*name.local != "xmlns" => Some("xmlns"),
                    "http://www.w3.org/1999/xlink" => Some("xlink"),
                    _ => name.prefix.as_deref(),
                };
                if let Some(p) = prefix {
                    out.push_str(p);
                    out.push(':');
                }
                out.push_str(&name.local);
                out.push_str("=\"");
                escape_html_str(value, true, out);
                out.push('"');
            }
            out.push('>');
            if &*el.name.ns == HTML_NS && HTML_VOID_ELEMENTS.contains(&&*el.name.local) {
                return;
            }
            // Emit declarative shadow root `<template shadowrootmode="...">` if requested.
            if let Some(sr_id) = el.shadow_root {
                serialize_shadow_root_if_allowed(
                    doc,
                    sr_id,
                    serializable_shadow_roots,
                    allowed_shadow_roots,
                    out,
                );
            }
            let child_container = el.template_contents.unwrap_or(node);
            for child in doc.children(child_container) {
                serialize_html_node_with_shadows(
                    doc,
                    child,
                    serializable_shadow_roots,
                    allowed_shadow_roots,
                    out,
                );
            }
            out.push_str("</");
            if !matches!(
                &*el.name.ns,
                HTML_NS | "http://www.w3.org/2000/svg" | "http://www.w3.org/1998/Math/MathML"
            ) {
                if let Some(p) = &el.name.prefix {
                    out.push_str(p);
                    out.push(':');
                }
            }
            out.push_str(&el.name.local);
            out.push('>');
        }
        Some(NodeData::Text(text) | NodeData::CDataSection(text)) => {
            let raw = doc
                .parent(node)
                .and_then(|p| doc.get(p))
                .is_some_and(|p| {
                    matches!(p, NodeData::Element(el) if &*el.name.ns == HTML_NS && matches!(
                        &*el.name.local,
                        "style" | "script" | "xmp" | "iframe" | "noembed" | "noframes" | "plaintext" | "noscript"
                    ))
                });
            if raw {
                out.push_str(text);
            } else {
                escape_html_str(text, false, out);
            }
        }
        Some(NodeData::Comment(c)) => {
            out.push_str("<!--");
            out.push_str(c);
            out.push_str("-->");
        }
        Some(NodeData::ProcessingInstruction { target, data }) => {
            out.push_str("<?");
            out.push_str(target);
            out.push(' ');
            out.push_str(data);
            out.push_str("?>");
        }
        Some(NodeData::DocumentType { name, .. }) => {
            out.push_str("<!DOCTYPE ");
            out.push_str(name);
            out.push('>');
        }
        _ => {
            for child in doc.children(node) {
                serialize_html_node_with_shadows(
                    doc,
                    child,
                    serializable_shadow_roots,
                    allowed_shadow_roots,
                    out,
                );
            }
        }
    }
}

fn escape_html_str(value: &str, attr: bool, out: &mut String) {
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '\u{00a0}' => out.push_str("&nbsp;"),
            '<' if !attr => out.push_str("&lt;"),
            '>' if !attr => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
}

// ---------------------------------------------------------------------------
// Full W3C DOM-Parsing §3.2 XML Serialization Algorithm
// ---------------------------------------------------------------------------

type PrefixMap = HashMap<String, Vec<Option<String>>>;

/// Serializes any DOM node according to W3C DOM-Parsing §3.2 ("XML serialization").
pub(crate) fn xml_serialize_node(
    doc: &Document,
    node: NodeId,
    require_well_formed: bool,
) -> Result<String, String> {
    let mut prefix_map: PrefixMap = HashMap::new();
    prefix_map.insert(XML_NS.to_string(), vec![Some("xml".to_string())]);
    let mut prefix_index = 1u32;
    let mut out = String::new();
    xml_serialize_node_inner(
        doc,
        node,
        None,
        &mut prefix_map,
        &mut prefix_index,
        require_well_formed,
        &mut out,
    )?;
    Ok(out)
}

fn xml_serialize_node_inner(
    doc: &Document,
    node: NodeId,
    inherited_ns: Option<&str>,
    prefix_map: &mut PrefixMap,
    prefix_index: &mut u32,
    require_well_formed: bool,
    out: &mut String,
) -> Result<(), String> {
    match doc.get(node) {
        Some(NodeData::Element(el)) => {
            let local = el.name.local.as_str();
            if require_well_formed && (local.contains(':') || !is_valid_xml_name(local)) {
                return Err(format!("Invalid XML element localName: '{local}'"));
            }
            let ns: Option<&str> = (!el.name.ns.is_empty()).then_some(el.name.ns.as_str());
            let elem_prefix: Option<&str> = el.name.prefix.as_deref();

            // Copy prefix map for this element scope & record local namespace declarations.
            let mut map = prefix_map.clone();
            let mut local_prefixes_map: HashMap<String, String> = HashMap::new();
            let mut local_default_ns: Option<Option<String>> = None;

            for (aq, aval) in &el.attributes {
                if &*aq.ns == XMLNS_NS {
                    if aq.prefix.is_none() && aq.local.as_str() == "xmlns" {
                        local_default_ns = Some(if aval.is_empty() {
                            None
                        } else {
                            Some(aval.clone())
                        });
                    } else if aq.prefix.as_deref() == Some("xmlns") {
                        let pfx = aq.local.as_str().to_string();
                        local_prefixes_map.insert(pfx.clone(), aval.clone());
                        map.entry(aval.clone()).or_default().push(Some(pfx));
                    }
                }
            }

            let mut ignore_ns_def_attr = false;
            let mut extra_ns_decl: Option<(Option<String>, String)> = None;
            let mut child_default_ns: Option<String> = inherited_ns.map(ToString::to_string);
            let qualified_name: String;

            if inherited_ns == ns {
                if local_default_ns.is_some() {
                    ignore_ns_def_attr = true;
                }
                if ns == Some(XML_NS) {
                    qualified_name = format!("xml:{local}");
                } else {
                    qualified_name = local.to_string();
                }
            } else {
                // `inherited_ns != ns`
                let preferred_prefix: Option<String> = if ns == Some(XML_NS) {
                    Some("xml".to_string())
                } else if let Some(uri) = ns {
                    map.get(uri)
                        .and_then(|list| list.iter().rev().find_map(|p| p.clone()))
                } else {
                    None
                };

                if let Some(pref_pfx) = preferred_prefix {
                    qualified_name = format!("{pref_pfx}:{local}");
                    if let Some(ref ldn) = local_default_ns {
                        child_default_ns = ldn.clone();
                    }
                } else if let Some(uri) = ns {
                    if let Some(orig_pfx) = elem_prefix {
                        // Use original prefix unless locally bound to a different URI.
                        let chosen = if local_prefixes_map.get(orig_pfx).is_some_and(|u| u != uri) {
                            let g = format!("ns{}", *prefix_index);
                            *prefix_index += 1;
                            g
                        } else {
                            orig_pfx.to_string()
                        };
                        map.entry(uri.to_string())
                            .or_default()
                            .push(Some(chosen.clone()));
                        qualified_name = format!("{chosen}:{local}");
                        extra_ns_decl = Some((Some(chosen), uri.to_string()));
                        if let Some(ref ldn) = local_default_ns {
                            child_default_ns = ldn.clone();
                        }
                    } else {
                        // No prefix: declare default namespace `xmlns="uri"`.
                        qualified_name = local.to_string();
                        child_default_ns = Some(uri.to_string());
                        if local_default_ns.as_ref() != Some(&Some(uri.to_string())) {
                            ignore_ns_def_attr = true;
                            extra_ns_decl = Some((None, uri.to_string()));
                        }
                    }
                } else {
                    // `ns == None` while `inherited_ns.is_some()`: reset default namespace `xmlns=""`.
                    qualified_name = local.to_string();
                    child_default_ns = None;
                    if local_default_ns != Some(None) {
                        ignore_ns_def_attr = true;
                        extra_ns_decl = Some((None, String::new()));
                    }
                }
            }

            out.push('<');
            out.push_str(&qualified_name);

            if let Some((decl_pfx, decl_uri)) = extra_ns_decl {
                if let Some(p) = decl_pfx {
                    out.push_str(" xmlns:");
                    out.push_str(&p);
                } else {
                    out.push_str(" xmlns");
                }
                out.push_str("=\"");
                escape_xml_attr(&decl_uri, out);
                out.push('"');
            }

            // Serialize attributes (W3C DOM-Parsing §3.2.1.1)
            let mut seen_attr_keys: HashSet<(String, String)> = HashSet::new();
            for (aq, aval) in &el.attributes {
                let alocal = aq.local.as_str();
                let ans = aq.ns.as_str();
                if require_well_formed {
                    if alocal.contains(':') || !is_valid_xml_name(alocal) {
                        return Err(format!("Invalid XML attribute localName: '{alocal}'"));
                    }
                    if aval.chars().any(|c| !is_valid_xml_char(c)) {
                        return Err("Invalid XML character in attribute value".to_string());
                    }
                    if !seen_attr_keys.insert((ans.to_string(), alocal.to_string())) {
                        return Err(format!("Duplicate XML attribute '{alocal}'"));
                    }
                }
                // Drop redundant or inconsistent `xmlns="..."` (including non-XMLNS local name `xmlns`).
                if aq.prefix.is_none() && alocal == "xmlns" {
                    if ignore_ns_def_attr {
                        continue;
                    }
                    if ans != XMLNS_NS && aval != ns.unwrap_or("") {
                        continue;
                    }
                    if ans != XMLNS_NS && ns.is_none() {
                        continue;
                    }
                    if ans == XMLNS_NS && aval.as_str() != ns.unwrap_or("") {
                        continue;
                    }
                    if ans == XMLNS_NS && ns.is_none() && inherited_ns.is_none() {
                        continue;
                    }
                    out.push_str(" xmlns=\"");
                    escape_xml_attr(aval, out);
                    out.push('"');
                    continue;
                }
                if ans == XMLNS_NS {
                    out.push_str(" xmlns:");
                    out.push_str(alocal);
                    out.push_str("=\"");
                    escape_xml_attr(aval, out);
                    out.push('"');
                    continue;
                }
                if ans.is_empty() {
                    out.push(' ');
                    out.push_str(alocal);
                    out.push_str("=\"");
                    escape_xml_attr(aval, out);
                    out.push('"');
                    continue;
                }
                // Namespaced attribute: look up nearest preferred prefix for `ans`.
                let cand_prefix: Option<String> = if ans == XML_NS {
                    Some("xml".to_string())
                } else {
                    map.get(ans)
                        .and_then(|list| list.iter().rev().find_map(|p| p.clone()))
                };
                if let Some(pfx) = cand_prefix {
                    out.push(' ');
                    out.push_str(&pfx);
                    out.push(':');
                    out.push_str(alocal);
                    out.push_str("=\"");
                    escape_xml_attr(aval, out);
                    out.push('"');
                } else {
                    let gen = format!("ns{}", *prefix_index);
                    *prefix_index += 1;
                    map.entry(ans.to_string())
                        .or_default()
                        .push(Some(gen.clone()));
                    out.push_str(" xmlns:");
                    out.push_str(&gen);
                    out.push_str("=\"");
                    escape_xml_attr(ans, out);
                    out.push_str("\" ");
                    out.push_str(&gen);
                    out.push(':');
                    out.push_str(alocal);
                    out.push_str("=\"");
                    escape_xml_attr(aval, out);
                    out.push('"');
                }
            }

            let children: Vec<NodeId> = doc.children(node).collect();
            let is_html_ns = ns == Some(HTML_NS);
            let is_html_void = is_html_ns && HTML_VOID_ELEMENTS.contains(&local);

            if children.is_empty() {
                if is_html_ns && !is_html_void {
                    out.push_str("></");
                    out.push_str(&qualified_name);
                    out.push('>');
                } else if is_html_void {
                    out.push_str(" />");
                } else {
                    out.push_str("/>");
                }
            } else {
                out.push('>');
                for c in children {
                    xml_serialize_node_inner(
                        doc,
                        c,
                        child_default_ns.as_deref(),
                        &mut map,
                        prefix_index,
                        require_well_formed,
                        out,
                    )?;
                }
                out.push_str("</");
                out.push_str(&qualified_name);
                out.push('>');
            }
            Ok(())
        }
        Some(NodeData::Text(text)) => {
            if require_well_formed && text.chars().any(|c| !is_valid_xml_char(c)) {
                return Err("Invalid XML character in Text node".to_string());
            }
            escape_xml_text(text, out);
            Ok(())
        }
        Some(NodeData::CDataSection(text)) => {
            if require_well_formed
                && (text.contains("]]>") || text.chars().any(|c| !is_valid_xml_char(c)))
            {
                return Err("Invalid XML CDATASection content".to_string());
            }
            out.push_str("<![CDATA[");
            out.push_str(text);
            out.push_str("]]>");
            Ok(())
        }
        Some(NodeData::Comment(text)) => {
            if require_well_formed
                && (text.contains("--")
                    || text.ends_with('-')
                    || text.chars().any(|c| !is_valid_xml_char(c)))
            {
                return Err("Invalid XML Comment content".to_string());
            }
            out.push_str("<!--");
            out.push_str(text);
            out.push_str("-->");
            Ok(())
        }
        Some(NodeData::ProcessingInstruction { target, data }) => {
            if require_well_formed
                && (target.contains(':')
                    || target.eq_ignore_ascii_case("xml")
                    || data.contains("?>")
                    || data.chars().any(|c| !is_valid_xml_char(c)))
            {
                return Err("Invalid XML ProcessingInstruction".to_string());
            }
            out.push_str("<?");
            out.push_str(target);
            if !data.is_empty() {
                out.push(' ');
                out.push_str(data);
            }
            out.push_str("?>");
            Ok(())
        }
        Some(NodeData::DocumentType {
            name,
            public_id,
            system_id,
        }) => {
            out.push_str("<!DOCTYPE ");
            out.push_str(name);
            if !public_id.is_empty() {
                out.push_str(" PUBLIC \"");
                out.push_str(public_id);
                out.push_str("\" \"");
                out.push_str(system_id);
                out.push('"');
            } else if !system_id.is_empty() {
                out.push_str(" SYSTEM \"");
                out.push_str(system_id);
                out.push('"');
            }
            out.push('>');
            Ok(())
        }
        Some(NodeData::Document | NodeData::DocumentFragment | NodeData::ShadowRoot { .. }) => {
            for c in doc.children(node) {
                xml_serialize_node_inner(
                    doc,
                    c,
                    inherited_ns,
                    prefix_map,
                    prefix_index,
                    require_well_formed,
                    out,
                )?;
            }
            Ok(())
        }
        None => Ok(()),
    }
}

fn escape_xml_attr(value: &str, out: &mut String) {
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\t' => out.push_str("&#9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            _ => out.push(ch),
        }
    }
}

fn escape_xml_text(value: &str, out: &mut String) {
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
}

fn collect_ancestor_xml_ns_decls(doc: &Document, start: NodeId) -> Vec<(Option<String>, String)> {
    let mut chain = Vec::new();
    let mut cur = Some(start);
    while let Some(n) = cur {
        chain.push(n);
        cur = doc.parent(n);
    }
    let mut decls = Vec::new();
    for n in chain.into_iter().rev() {
        if let Some(NodeData::Element(el)) = doc.get(n) {
            if !el.name.ns.is_empty() && el.name.prefix.is_none() {
                decls.push((None, el.name.ns.to_string()));
            }
            for (aq, aval) in &el.attributes {
                if &*aq.ns == XMLNS_NS {
                    if aq.prefix.is_none() && aq.local.as_str() == "xmlns" {
                        decls.push((None, aval.clone()));
                    } else if aq.prefix.as_deref() == Some("xmlns") {
                        decls.push((Some(aq.local.to_string()), aval.clone()));
                    }
                }
            }
        }
    }
    decls
}

fn init_xml_prefix_map_for_ancestors(doc: &Document, start: NodeId) -> PrefixMap {
    let mut map: PrefixMap = HashMap::new();
    map.insert(XML_NS.to_string(), vec![Some("xml".to_string())]);
    for (p, uri) in collect_ancestor_xml_ns_decls(doc, start) {
        if let Some(pfx) = p {
            map.entry(uri).or_default().push(Some(pfx));
        }
    }
    map
}

fn inherited_default_ns_for_element(doc: &Document, el_id: NodeId) -> Option<String> {
    let mut cur = Some(el_id);
    while let Some(n) = cur {
        if let Some(NodeData::Element(el)) = doc.get(n) {
            for (aq, aval) in &el.attributes {
                if &*aq.ns == XMLNS_NS && aq.prefix.is_none() && aq.local.as_str() == "xmlns" {
                    return (!aval.is_empty()).then(|| aval.clone());
                }
            }
            if el.name.prefix.is_none() {
                return (!el.name.ns.is_empty()).then(|| el.name.ns.to_string());
            }
        }
        cur = doc.parent(n);
    }
    None
}
