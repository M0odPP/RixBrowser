//! W3C Trusted Types API (`trustedTypes`, `TrustedTypePolicyFactory`,
//! `TrustedTypePolicy`, `TrustedHTML`, `TrustedScript`, `TrustedScriptURL`,
//! `TrustedHTMLParserOptions`), Content Security Policy (`require-trusted-types-for`,
//! `connect-src`), and `SecurityPolicyViolationEvent`.

use std::collections::HashMap;
use std::rc::Rc;

use rix_dom::{NodeData, NodeId};
use rix_js::{GcHeap, HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::dom_bridge::{arg, define_global, define_host_accessor, define_host_method, SharedHost};
use crate::dom_parsing::{legacy_null_to_empty_string, to_dom_string};
use crate::dom_sanitizer::{parse_html_method_options, ParsedMethodOptions};
use crate::event_loop::{SharedEventLoop, TaskPriority};
use crate::events::{fire_dom_node_event, SharedEvents};
use crate::web_apis::create_dom_exception;

/// Per-document Content Security Policy state relevant to Trusted Types and fetch/WebSocket sinks.
#[derive(Debug, Clone, Default)]
pub struct DocumentCspPolicy {
    pub enforce_trusted_types: bool,
    pub report_only_trusted_types: bool,
    pub connect_src_none: bool,
    pub original_enforce_policy: String,
    pub original_report_policy: String,
}

impl DocumentCspPolicy {
    /// Parses a `Content-Security-Policy` or `Content-Security-Policy-Report-Only` header/meta value.
    pub fn apply_header(&mut self, raw: &str, report_only: bool) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }
        if report_only {
            if !self.original_report_policy.is_empty() {
                self.original_report_policy.push_str(", ");
            }
            self.original_report_policy.push_str(trimmed);
        } else {
            if !self.original_enforce_policy.is_empty() {
                self.original_enforce_policy.push_str(", ");
            }
            self.original_enforce_policy.push_str(trimmed);
        }
        for policy_part in trimmed.split(',') {
            for directive in policy_part.split(';') {
                let mut tokens = directive.split_ascii_whitespace();
                let Some(name) = tokens.next() else {
                    continue;
                };
                if name.eq_ignore_ascii_case("require-trusted-types-for") {
                    if tokens.any(|t| t.eq_ignore_ascii_case("'script'")) {
                        if report_only {
                            self.report_only_trusted_types = true;
                        } else {
                            self.enforce_trusted_types = true;
                        }
                    }
                } else if name.eq_ignore_ascii_case("connect-src")
                    && !report_only
                    && tokens.any(|t| t.eq_ignore_ascii_case("'none'"))
                {
                    self.connect_src_none = true;
                }
            }
        }
    }

    pub fn requires_trusted_types(&self) -> bool {
        self.enforce_trusted_types || self.report_only_trusted_types
    }
}

#[derive(Debug, Clone)]
struct TrustedPolicyEntry {
    name: String,
    create_html: Option<HeapHandle>,
    create_script: Option<HeapHandle>,
    create_script_url: Option<HeapHandle>,
    create_parser_options: Option<HeapHandle>,
}

/// Per-realm Trusted Types prototypes, policies, and opaque branded objects.
#[derive(Default)]
pub struct TrustedTypesState {
    pub trusted_html_proto: Option<HeapHandle>,
    pub trusted_script_proto: Option<HeapHandle>,
    pub trusted_script_url_proto: Option<HeapHandle>,
    pub trusted_parser_options_proto: Option<HeapHandle>,
    pub policy_proto: Option<HeapHandle>,
    pub factory_proto: Option<HeapHandle>,
    pub csp_event_proto: Option<HeapHandle>,
    pub factory_obj: Option<HeapHandle>,
    pub empty_html_obj: Option<HeapHandle>,
    pub empty_script_obj: Option<HeapHandle>,
    pub default_policy: Option<HeapHandle>,
    policies: HashMap<HeapHandle, TrustedPolicyEntry>,
    html_values: HashMap<HeapHandle, String>,
    script_values: HashMap<HeapHandle, String>,
    script_url_values: HashMap<HeapHandle, String>,
    parser_options_values: HashMap<HeapHandle, ParsedMethodOptions>,
}

impl TrustedTypesState {
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for h in [
            self.trusted_html_proto,
            self.trusted_script_proto,
            self.trusted_script_url_proto,
            self.trusted_parser_options_proto,
            self.policy_proto,
            self.factory_proto,
            self.csp_event_proto,
            self.factory_obj,
            self.empty_html_obj,
            self.empty_script_obj,
            self.default_policy,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(h));
        }
        for (&ph, entry) in &self.policies {
            out.push(JsValue::from_object(ph));
            for cb in [
                entry.create_html,
                entry.create_script,
                entry.create_script_url,
                entry.create_parser_options,
            ]
            .into_iter()
            .flatten()
            {
                out.push(JsValue::from_object(cb));
            }
        }
    }

    pub fn sweep_dead(&mut self, heap: &GcHeap) {
        self.html_values
            .retain(|h, _| heap.get_object(*h).is_some());
        self.script_values
            .retain(|h, _| heap.get_object(*h).is_some());
        self.script_url_values
            .retain(|h, _| heap.get_object(*h).is_some());
        self.parser_options_values
            .retain(|h, _| heap.get_object(*h).is_some());
    }

    pub fn is_trusted_html(&self, val: JsValue) -> Option<String> {
        val.as_object()
            .and_then(|h| self.html_values.get(&h).cloned())
    }

    pub fn is_trusted_script(&self, val: JsValue) -> Option<String> {
        val.as_object()
            .and_then(|h| self.script_values.get(&h).cloned())
    }

    pub fn is_trusted_script_url(&self, val: JsValue) -> Option<String> {
        val.as_object()
            .and_then(|h| self.script_url_values.get(&h).cloned())
    }

    pub(crate) fn is_trusted_parser_options(&self, val: JsValue) -> Option<ParsedMethodOptions> {
        val.as_object()
            .and_then(|h| self.parser_options_values.get(&h).cloned())
    }

    fn default_policy_callbacks(&self) -> Option<TrustedPolicyEntry> {
        self.default_policy
            .and_then(|h| self.policies.get(&h).cloned())
    }
}

/// Resolves which document governs Trusted Types CSP & default policy for `doc_id`.
/// Browsing-context documents (main document and iframes with `default_view`) use their own
/// CSP; inert documents (`document.implementation.createHTMLDocument()`, `DOMParser`, `new Document()`)
/// share the relevant global object's CSP (main document) per W3C Trusted Types §4.1.
fn csp_governing_doc(host: &SharedHost, doc_id: NodeId) -> NodeId {
    let hb = host.borrow();
    let main_root = hb.document.borrow().root();
    if doc_id == main_root {
        return main_root;
    }
    if hb
        .doc_meta
        .get(&doc_id)
        .and_then(|m| m.default_view)
        .is_some()
    {
        doc_id
    } else {
        main_root
    }
}

/// Computes effective CSP for `doc_id` by combining HTTP response CSP headers with any
/// `<meta http-equiv="Content-Security-Policy" content="...">` elements in the governing document.
pub fn effective_doc_csp(host: &SharedHost, doc_id: NodeId) -> DocumentCspPolicy {
    let gov_doc = csp_governing_doc(host, doc_id);
    let hb = host.borrow();
    let mut policy = hb.doc_csp.get(&gov_doc).cloned().unwrap_or_default();
    let doc = hb.document.borrow();
    for nid in doc.descendants(gov_doc) {
        if let Some(NodeData::Element(el)) = doc.get(nid) {
            if el.name.local.as_str().eq_ignore_ascii_case("meta") {
                if let (Some(equiv), Some(content)) =
                    (el.get_attribute("http-equiv"), el.get_attribute("content"))
                {
                    if equiv.trim().eq_ignore_ascii_case("content-security-policy") {
                        policy.apply_header(content, false);
                    }
                }
            }
        }
    }
    policy
}

fn alloc_branded_string_obj(
    vm: &mut JsVm,
    proto: HeapHandle,
    map: &mut HashMap<HeapHandle, String>,
    value: String,
) -> HeapHandle {
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    map.insert(obj, value);
    obj
}

fn install_illegal_ctor(vm: &mut JsVm, name: &'static str, proto: HeapHandle) {
    let ctor = vm.alloc_host_fn(name, move |vm, _, _| {
        Err(vm.type_error(format!("Illegal constructor: {name}")))
    });
    vm.mark_constructor(ctor);
    let kp = vm.key_prototype;
    let kc = vm.key_constructor;
    vm.heap.object_define_data(
        ctor,
        kp,
        JsValue::from_object(proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        proto,
        kc,
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let tag_key = vm.key_symbol_to_string_tag;
    let tag_str = vm.heap.alloc_string(name);
    vm.heap.object_define_data(
        proto,
        tag_key,
        JsValue::from_string(tag_str),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    define_global(vm, name, JsValue::from_object(ctor));
}

/// Installs W3C Trusted Types globals (`trustedTypes`, `TrustedHTML`, `TrustedScript`,
/// `TrustedScriptURL`, `TrustedHTMLParserOptions`, `TrustedTypePolicy`,
/// `TrustedTypePolicyFactory`) and `SecurityPolicyViolationEvent`.
pub fn install_trusted_types(vm: &mut JsVm, host: &SharedHost, events: &SharedEvents) {
    let html_proto = vm.alloc_ordinary_object();
    let script_proto = vm.alloc_ordinary_object();
    let script_url_proto = vm.alloc_ordinary_object();
    let parser_opts_proto = vm.alloc_ordinary_object();
    let policy_proto = vm.alloc_ordinary_object();
    let factory_proto = vm.alloc_ordinary_object();
    for p in [
        html_proto,
        script_proto,
        script_url_proto,
        parser_opts_proto,
        policy_proto,
        factory_proto,
    ] {
        vm.heap.get_object_mut(p).unwrap().is_prototype = true;
    }

    // SecurityPolicyViolationEvent inherits from Event.prototype.
    let event_proto = events.borrow().event_proto.unwrap_or(vm.object_proto);
    let csp_ev_proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(event_proto)));
    vm.heap.get_object_mut(csp_ev_proto).unwrap().is_prototype = true;
    let csp_ctor = vm.alloc_host_fn("SecurityPolicyViolationEvent", move |vm, _, args| {
        let ev_type = to_dom_string(vm, arg(args, 0))?;
        let obj = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(csp_ev_proto),
        ));
        let k_type = vm.heap.intern_key("type");
        let tv = JsValue::from_string(vm.heap.alloc_string(ev_type));
        vm.heap
            .object_define_data(obj, k_type, tv, PropertyAttrs::DEFAULT, &mut vm.shapes);
        if let Some(init_h) = arg(args, 1).as_object() {
            for field in [
                "documentURI",
                "referrer",
                "blockedURI",
                "effectiveDirective",
                "violatedDirective",
                "originalPolicy",
                "disposition",
                "sample",
                "sourceFile",
                "statusCode",
                "lineNumber",
                "columnNumber",
            ] {
                let k = vm.heap.intern_key(field);
                if let Ok(v) = vm.get_property_by_key(init_h, k) {
                    if !v.is_undefined() {
                        vm.heap.object_define_data(
                            obj,
                            k,
                            v,
                            PropertyAttrs::DEFAULT,
                            &mut vm.shapes,
                        );
                    }
                }
            }
        }
        Ok(JsValue::from_object(obj))
    });
    vm.mark_constructor(csp_ctor);
    let kp = vm.key_prototype;
    let kc = vm.key_constructor;
    vm.heap.object_define_data(
        csp_ctor,
        kp,
        JsValue::from_object(csp_ev_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        csp_ev_proto,
        kc,
        JsValue::from_object(csp_ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    define_global(
        vm,
        "SecurityPolicyViolationEvent",
        JsValue::from_object(csp_ctor),
    );

    install_illegal_ctor(vm, "TrustedHTML", html_proto);
    install_illegal_ctor(vm, "TrustedScript", script_proto);
    install_illegal_ctor(vm, "TrustedScriptURL", script_url_proto);
    install_illegal_ctor(vm, "TrustedHTMLParserOptions", parser_opts_proto);
    install_illegal_ctor(vm, "TrustedTypePolicy", policy_proto);
    install_illegal_ctor(vm, "TrustedTypePolicyFactory", factory_proto);

    // toString / toJSON on TrustedHTML, TrustedScript, TrustedScriptURL
    for (proto, kind) in [
        (html_proto, 0u8),
        (script_proto, 1u8),
        (script_url_proto, 2u8),
    ] {
        for mname in ["toString", "toJSON"] {
            let h = Rc::clone(host);
            define_host_method(vm, proto, mname, move |vm, t, _| {
                let Some(oh) = t.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                let s = {
                    let hb = h.borrow();
                    match kind {
                        0 => hb.trusted_types.html_values.get(&oh).cloned(),
                        1 => hb.trusted_types.script_values.get(&oh).cloned(),
                        _ => hb.trusted_types.script_url_values.get(&oh).cloned(),
                    }
                };
                match s {
                    Some(val) => Ok(JsValue::from_string(vm.heap.alloc_string(val))),
                    None => Err(vm.type_error("Illegal invocation")),
                }
            });
        }
    }

    // TrustedTypePolicy.prototype.{name, createHTML, createScript, createScriptURL, createParserOptions}
    {
        let h = Rc::clone(host);
        let g = vm.alloc_host_fn("get name", move |vm, t, _| {
            let Some(oh) = t.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let name = h
                .borrow()
                .trusted_types
                .policies
                .get(&oh)
                .map(|p| p.name.clone())
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            Ok(JsValue::from_string(vm.heap.alloc_string(name)))
        });
        define_host_accessor(vm, policy_proto, "name", Some(g), None);
    }

    for (mname, kind) in [
        ("createHTML", 0u8),
        ("createScript", 1u8),
        ("createScriptURL", 2u8),
    ] {
        let h = Rc::clone(host);
        define_host_method(vm, policy_proto, mname, move |vm, t, args| {
            let Some(oh) = t.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let (cb_opt, proto) = {
                let hb = h.borrow();
                let Some(pol) = hb.trusted_types.policies.get(&oh) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                let cb = match kind {
                    0 => pol.create_html,
                    1 => pol.create_script,
                    _ => pol.create_script_url,
                };
                let p = match kind {
                    0 => hb.trusted_types.trusted_html_proto,
                    1 => hb.trusted_types.trusted_script_proto,
                    _ => hb.trusted_types.trusted_script_url_proto,
                };
                (cb, p.unwrap_or(vm.object_proto))
            };
            let Some(cb) = cb_opt else {
                return Err(vm.type_error(format!("Policy does not implement {mname}")));
            };
            let input_str = to_dom_string(vm, arg(args, 0))?;
            let mut call_args = Vec::with_capacity(args.len().max(1));
            call_args.push(JsValue::from_string(vm.heap.alloc_string(input_str)));
            if args.len() > 1 {
                call_args.extend_from_slice(&args[1..]);
            }
            let ret = vm.call_value(JsValue::from_object(cb), JsValue::UNDEFINED, &call_args)?;
            let out_str = if ret.is_null() || ret.is_undefined() {
                String::new()
            } else {
                to_dom_string(vm, ret)?
            };
            let mut hb = h.borrow_mut();
            let obj = match kind {
                0 => {
                    alloc_branded_string_obj(vm, proto, &mut hb.trusted_types.html_values, out_str)
                }
                1 => alloc_branded_string_obj(
                    vm,
                    proto,
                    &mut hb.trusted_types.script_values,
                    out_str,
                ),
                _ => alloc_branded_string_obj(
                    vm,
                    proto,
                    &mut hb.trusted_types.script_url_values,
                    out_str,
                ),
            };
            Ok(JsValue::from_object(obj))
        });
    }

    {
        let h = Rc::clone(host);
        define_host_method(
            vm,
            policy_proto,
            "createParserOptions",
            move |vm, t, args| {
                let Some(oh) = t.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                let (cb_opt, proto) = {
                    let hb = h.borrow();
                    let Some(pol) = hb.trusted_types.policies.get(&oh) else {
                        return Err(vm.type_error("Illegal invocation"));
                    };
                    (
                        pol.create_parser_options,
                        hb.trusted_types
                            .trusted_parser_options_proto
                            .unwrap_or(vm.object_proto),
                    )
                };
                let Some(cb) = cb_opt else {
                    return Err(vm.type_error("Policy does not implement createParserOptions"));
                };
                let first_arg = if args.is_empty() || arg(args, 0).is_undefined() {
                    JsValue::from_object(vm.alloc_ordinary_object())
                } else {
                    arg(args, 0)
                };
                let mut call_args = Vec::with_capacity(args.len().max(1));
                call_args.push(first_arg);
                if args.len() > 1 {
                    call_args.extend_from_slice(&args[1..]);
                }
                let ret =
                    vm.call_value(JsValue::from_object(cb), JsValue::UNDEFINED, &call_args)?;
                let parsed = parse_html_method_options(vm, &h, ret, false)?;
                let obj = vm
                    .heap
                    .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
                h.borrow_mut()
                    .trusted_types
                    .parser_options_values
                    .insert(obj, parsed);
                Ok(JsValue::from_object(obj))
            },
        );
    }

    // TrustedTypePolicyFactory.prototype.{createPolicy, isHTML, isScript, isScriptURL, isHTMLParserOptions, defaultPolicy, emptyHTML, emptyScript}
    {
        let h = Rc::clone(host);
        define_host_method(vm, factory_proto, "createPolicy", move |vm, _, args| {
            let name = to_dom_string(vm, arg(args, 0))?;
            if name == "default" && h.borrow().trusted_types.default_policy.is_some() {
                return Err(vm.type_error("Policy with name \"default\" already exists"));
            }
            let mut entry = TrustedPolicyEntry {
                name: name.clone(),
                create_html: None,
                create_script: None,
                create_script_url: None,
                create_parser_options: None,
            };
            let rules_val = arg(args, 1);
            if !rules_val.is_undefined() && !rules_val.is_null() {
                let Some(rh) = rules_val.as_object() else {
                    return Err(vm.type_error("Policy options must be an object"));
                };
                for (prop, slot) in [
                    ("createHTML", &mut entry.create_html),
                    ("createScript", &mut entry.create_script),
                    ("createScriptURL", &mut entry.create_script_url),
                    ("createParserOptions", &mut entry.create_parser_options),
                ] {
                    let k = vm.heap.intern_key(prop);
                    let v = vm.get_property_by_key(rh, k)?;
                    if !v.is_undefined() {
                        let fh = v
                            .as_object()
                            .filter(|&f| vm.is_callable_handle(f))
                            .ok_or_else(|| vm.type_error(format!("{prop} must be a function")))?;
                        *slot = Some(fh);
                    }
                }
            }
            let p_proto = h
                .borrow()
                .trusted_types
                .policy_proto
                .unwrap_or(vm.object_proto);
            let pol_obj = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(p_proto)));
            let mut hb = h.borrow_mut();
            hb.trusted_types.policies.insert(pol_obj, entry);
            if name == "default" {
                hb.trusted_types.default_policy = Some(pol_obj);
            }
            Ok(JsValue::from_object(pol_obj))
        });
    }

    for (mname, kind) in [
        ("isHTML", 0u8),
        ("isScript", 1u8),
        ("isScriptURL", 2u8),
        ("isHTMLParserOptions", 3u8),
    ] {
        let h = Rc::clone(host);
        define_host_method(vm, factory_proto, mname, move |_, _, args| {
            let v = arg(args, 0);
            let hb = h.borrow();
            let matched = match kind {
                0 => hb.trusted_types.is_trusted_html(v).is_some(),
                1 => hb.trusted_types.is_trusted_script(v).is_some(),
                2 => hb.trusted_types.is_trusted_script_url(v).is_some(),
                _ => hb.trusted_types.is_trusted_parser_options(v).is_some(),
            };
            Ok(JsValue::from_bool(matched))
        });
    }

    {
        let h = Rc::clone(host);
        let g = vm.alloc_host_fn("get defaultPolicy", move |_, _, _| {
            Ok(h.borrow()
                .trusted_types
                .default_policy
                .map_or(JsValue::NULL, JsValue::from_object))
        });
        define_host_accessor(vm, factory_proto, "defaultPolicy", Some(g), None);
    }

    let empty_html = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(html_proto)));
    let empty_script = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(script_proto),
    ));
    let factory_obj = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(factory_proto),
    ));

    {
        let h = Rc::clone(host);
        let g = vm.alloc_host_fn("get emptyHTML", move |_, _, _| {
            Ok(h.borrow()
                .trusted_types
                .empty_html_obj
                .map_or(JsValue::NULL, JsValue::from_object))
        });
        define_host_accessor(vm, factory_proto, "emptyHTML", Some(g), None);
    }
    {
        let h = Rc::clone(host);
        let g = vm.alloc_host_fn("get emptyScript", move |_, _, _| {
            Ok(h.borrow()
                .trusted_types
                .empty_script_obj
                .map_or(JsValue::NULL, JsValue::from_object))
        });
        define_host_accessor(vm, factory_proto, "emptyScript", Some(g), None);
    }

    {
        let mut hb = host.borrow_mut();
        hb.trusted_types.trusted_html_proto = Some(html_proto);
        hb.trusted_types.trusted_script_proto = Some(script_proto);
        hb.trusted_types.trusted_script_url_proto = Some(script_url_proto);
        hb.trusted_types.trusted_parser_options_proto = Some(parser_opts_proto);
        hb.trusted_types.policy_proto = Some(policy_proto);
        hb.trusted_types.factory_proto = Some(factory_proto);
        hb.trusted_types.csp_event_proto = Some(csp_ev_proto);
        hb.trusted_types.factory_obj = Some(factory_obj);
        hb.trusted_types.empty_html_obj = Some(empty_html);
        hb.trusted_types.empty_script_obj = Some(empty_script);
        hb.trusted_types
            .html_values
            .insert(empty_html, String::new());
        hb.trusted_types
            .script_values
            .insert(empty_script, String::new());
    }

    define_global(vm, "trustedTypes", JsValue::from_object(factory_obj));
}

/// Queues a `securitypolicyviolation` `SecurityPolicyViolationEvent` targeting `owner_doc`
/// (bubbling up to `window` per CSP Level 3 §5.5).
#[allow(clippy::too_many_arguments)]
pub(crate) fn queue_csp_violation(
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    owner_doc: NodeId,
    directive: &'static str,
    disposition: &'static str,
    blocked_uri: &str,
    sample: String,
    original_policy: String,
) {
    let h = Rc::clone(host);
    let evs = Rc::clone(events);
    let el = Rc::clone(event_loop);
    let el_inner = Rc::clone(event_loop);
    let blocked = blocked_uri.to_string();
    let doc_uri = host.borrow().doc_url_for_node(owner_doc);
    el.borrow_mut()
        .post_host_task(TaskPriority::Networking, move |vm| {
            let props = [
                (
                    "documentURI",
                    JsValue::from_string(vm.heap.alloc_string(doc_uri)),
                ),
                ("referrer", JsValue::from_string(vm.heap.alloc_string(""))),
                (
                    "blockedURI",
                    JsValue::from_string(vm.heap.alloc_string(blocked)),
                ),
                (
                    "effectiveDirective",
                    JsValue::from_string(vm.heap.alloc_string(directive)),
                ),
                (
                    "violatedDirective",
                    JsValue::from_string(vm.heap.alloc_string(directive)),
                ),
                (
                    "originalPolicy",
                    JsValue::from_string(vm.heap.alloc_string(original_policy)),
                ),
                (
                    "disposition",
                    JsValue::from_string(vm.heap.alloc_string(disposition)),
                ),
                ("sample", JsValue::from_string(vm.heap.alloc_string(sample))),
                ("sourceFile", JsValue::from_string(vm.heap.alloc_string(""))),
                ("statusCode", JsValue::from_i32(200)),
                ("lineNumber", JsValue::from_i32(0)),
                ("columnNumber", JsValue::from_i32(0)),
            ];
            let _ = fire_dom_node_event(
                vm,
                &h,
                &evs,
                &el_inner,
                owner_doc,
                "securitypolicyviolation",
                true,
                false,
                &props,
            );
        });
}

#[allow(clippy::too_many_arguments)]
fn emit_tt_violation_and_maybe_throw(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    owner_doc: NodeId,
    csp: &DocumentCspPolicy,
    sink_name: &str,
    sample_tail: &str,
) -> Result<(), JsException> {
    let clipped: String = sample_tail.chars().take(40).collect();
    let sample = format!("{sink_name}|{clipped}");
    if csp.report_only_trusted_types {
        queue_csp_violation(
            host,
            events,
            event_loop,
            owner_doc,
            "require-trusted-types-for",
            "report",
            "trusted-types-sink",
            sample.clone(),
            csp.original_report_policy.clone(),
        );
    }
    if csp.enforce_trusted_types {
        queue_csp_violation(
            host,
            events,
            event_loop,
            owner_doc,
            "require-trusted-types-for",
            "enforce",
            "trusted-types-sink",
            sample,
            csp.original_enforce_policy.clone(),
        );
        return Err(vm.type_error(format!(
            "This document requires 'TrustedHTML' or 'TrustedHTMLParserOptions' assignment for '{sink_name}'"
        )));
    }
    Ok(())
}

/// Returns true iff `options_val` is absent/empty or contains neither `sanitizer` nor truthy `runScripts`.
fn is_vetting_free_options(vm: &mut JsVm, options_val: JsValue) -> Result<bool, JsException> {
    if options_val.is_undefined() || options_val.is_null() {
        return Ok(true);
    }
    let Some(oh) = options_val.as_object() else {
        return Ok(false);
    };
    let k_san = vm.heap.intern_key("sanitizer");
    let san_v = vm.get_property_by_key(oh, k_san)?;
    if !san_v.is_undefined() {
        return Ok(false);
    }
    let k_rs = vm.heap.intern_key("runScripts");
    let rs_v = vm.get_property_by_key(oh, k_rs)?;
    Ok(rs_v.is_undefined() || !vm.to_boolean(rs_v))
}

/// Validates and resolves `TrustedHTMLParserOptions` for `*HTMLUnsafe` positional and streaming
/// methods under CSP `require-trusted-types-for 'script'`.
///
/// Returns:
/// - `Ok(Some(opts))` when trusted parser options were supplied or produced by `defaultPolicy.createParserOptions`
///   (caller should use `opts` directly and skip `createHTML` vetting on positional methods).
/// - `Ok(None)` when no trusted parser options were produced (either TT not active, report-only
///   violation allowed continuation, or positional vetting-free options deferred to `createHTML`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_trusted_html_parser_options(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    target_nid: NodeId,
    sink_name: &str,
    options_val: JsValue,
    allow_vetting_free_bypass: bool,
) -> Result<Option<ParsedMethodOptions>, JsException> {
    // Direct `TrustedHTMLParserOptions` instance always bypasses the default policy.
    if let Some(opts) = host
        .borrow()
        .trusted_types
        .is_trusted_parser_options(options_val)
    {
        return Ok(Some(opts));
    }

    let owner_doc = host.borrow().owner_doc_of(target_nid);
    let gov_doc = csp_governing_doc(host, owner_doc);
    let csp = effective_doc_csp(host, owner_doc);
    if !csp.requires_trusted_types() {
        return Ok(None);
    }

    let main_root = host.borrow().document.borrow().root();
    let default_cpo = if gov_doc == main_root {
        host.borrow()
            .trusted_types
            .default_policy_callbacks()
            .and_then(|p| p.create_parser_options)
    } else {
        None
    };

    if let Some(cpo_fn) = default_cpo {
        let arg0 = if options_val.is_undefined() || options_val.is_null() {
            JsValue::from_object(vm.alloc_ordinary_object())
        } else {
            options_val
        };
        let type_str = JsValue::from_string(vm.heap.alloc_string("TrustedHTMLParserOptions"));
        let sink_str = JsValue::from_string(vm.heap.alloc_string(sink_name));
        let ret = vm.call_value(
            JsValue::from_object(cpo_fn),
            JsValue::UNDEFINED,
            &[arg0, type_str, sink_str],
        )?;
        if ret.is_null() || ret.is_undefined() {
            emit_tt_violation_and_maybe_throw(
                vm, host, events, event_loop, owner_doc, &csp, sink_name, "",
            )?;
            return Ok(None);
        }
        let parsed = parse_html_method_options(vm, host, ret, false)?;
        return Ok(Some(parsed));
    }

    // No `defaultPolicy.createParserOptions`:
    // For positional methods with vetting-free options (`undefined`, `{}`, `{ runScripts: false }`),
    // fall through so `createHTML` handles vetting instead!
    if allow_vetting_free_bypass && is_vetting_free_options(vm, options_val)? {
        return Ok(None);
    }

    emit_tt_violation_and_maybe_throw(
        vm, host, events, event_loop, owner_doc, &csp, sink_name, "",
    )?;
    Ok(None)
}

/// Validates a `TrustedHTML` / string sink (`innerHTML`, `outerHTML`, `insertAdjacentHTML`,
/// and positional `*HTMLUnsafe` methods when `createParserOptions` did not already vet the call).
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_trusted_html_sink(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    target_nid: NodeId,
    sink_name: &str,
    val: JsValue,
    legacy_null_to_empty: bool,
) -> Result<String, JsException> {
    if let Some(html) = host.borrow().trusted_types.is_trusted_html(val) {
        return Ok(html);
    }
    let raw = if legacy_null_to_empty {
        legacy_null_to_empty_string(vm, val)?
    } else {
        to_dom_string(vm, val)?
    };
    let owner_doc = host.borrow().owner_doc_of(target_nid);
    let gov_doc = csp_governing_doc(host, owner_doc);
    let csp = effective_doc_csp(host, owner_doc);
    if !csp.requires_trusted_types() {
        return Ok(raw);
    }

    let main_root = host.borrow().document.borrow().root();
    let default_ch = if gov_doc == main_root {
        host.borrow()
            .trusted_types
            .default_policy_callbacks()
            .and_then(|p| p.create_html)
    } else {
        None
    };

    if let Some(ch_fn) = default_ch {
        let arg0 = JsValue::from_string(vm.heap.alloc_string(raw.clone()));
        let type_str = JsValue::from_string(vm.heap.alloc_string("TrustedHTML"));
        let sink_str = JsValue::from_string(vm.heap.alloc_string(sink_name));
        let ret = vm.call_value(
            JsValue::from_object(ch_fn),
            JsValue::UNDEFINED,
            &[arg0, type_str, sink_str],
        )?;
        if ret.is_null() || ret.is_undefined() {
            emit_tt_violation_and_maybe_throw(
                vm, host, events, event_loop, owner_doc, &csp, sink_name, &raw,
            )?;
            return Ok(raw);
        }
        return to_dom_string(vm, ret);
    }

    emit_tt_violation_and_maybe_throw(
        vm, host, events, event_loop, owner_doc, &csp, sink_name, &raw,
    )?;
    Ok(raw)
}

/// Checks if the default Trusted Types policy defines `createParserOptions` for classic HTML sinks
/// (`innerHTML`, `outerHTML`, `insertAdjacentHTML`), returning vetted parser options if so.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_classic_html_sink_options(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    target_nid: NodeId,
    sink_name: &str,
) -> Result<Option<ParsedMethodOptions>, JsException> {
    let owner_doc = host.borrow().owner_doc_of(target_nid);
    let gov_doc = csp_governing_doc(host, owner_doc);
    let csp = effective_doc_csp(host, owner_doc);
    if !csp.requires_trusted_types() {
        return Ok(None);
    }
    let main_root = host.borrow().document.borrow().root();
    let default_cpo = if gov_doc == main_root {
        host.borrow()
            .trusted_types
            .default_policy_callbacks()
            .and_then(|p| p.create_parser_options)
    } else {
        None
    };
    let Some(cpo_fn) = default_cpo else {
        return Ok(None);
    };
    let empty_opts = JsValue::from_object(vm.alloc_ordinary_object());
    let type_str = JsValue::from_string(vm.heap.alloc_string("TrustedHTMLParserOptions"));
    let sink_str = JsValue::from_string(vm.heap.alloc_string(sink_name));
    let ret = vm.call_value(
        JsValue::from_object(cpo_fn),
        JsValue::UNDEFINED,
        &[empty_opts, type_str, sink_str],
    )?;
    if ret.is_null() || ret.is_undefined() {
        emit_tt_violation_and_maybe_throw(
            vm, host, events, event_loop, owner_doc, &csp, sink_name, "",
        )?;
        return Ok(None);
    }
    Ok(Some(parse_html_method_options(vm, host, ret, false)?))
}

/// Enforces CSP `connect-src 'none'` for `WebSocket` connections, queueing a
/// `SecurityPolicyViolationEvent` and returning `SecurityError` when blocked.
pub(crate) fn check_connect_src_allowed(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    url: &str,
) -> Result<(), JsException> {
    let root = host.borrow().document.borrow().root();
    let csp = effective_doc_csp(host, root);
    if csp.connect_src_none {
        queue_csp_violation(
            host,
            events,
            event_loop,
            root,
            "connect-src",
            "enforce",
            url,
            String::new(),
            csp.original_enforce_policy,
        );
        return Err(create_dom_exception(
            vm,
            "SecurityError",
            &format!("Refused to connect to '{url}' because it violates CSP connect-src"),
        ));
    }
    Ok(())
}
