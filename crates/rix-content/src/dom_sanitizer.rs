//! WHATWG HTML Sanitizer API (`Sanitizer`), Positional HTML Insertion Methods
//! (`setHTML`, `appendHTML`, `prependHTML`, `beforeHTML`, `afterHTML`, `replaceWithHTML`
//! and `*Unsafe` variants), and Streaming HTML Methods (`streamHTML`, `streamAppendHTML`,
//! `streamPrependHTML`, `streamBeforeHTML`, `streamAfterHTML`, `streamReplaceWithHTML`
//! and `*Unsafe` variants) backed by `HtmlFragmentParserStream`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use rix_dom::{
    is_valid_shadow_host_name, Document, ElementData, LocalName, Namespace, NodeData, NodeId,
    QualName,
};
use rix_html::{HtmlFragmentParserStream, ParserStreamStep};
use rix_js::promise::{alloc_promise, resolve_promise};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::dom_bridge::{
    arg, define_global, define_host_accessor, define_host_method, notify_custom_element_subtree,
    require_node, wrap_node, DomMutation, SharedHost,
};
use crate::dom_interfaces::{HTML_NS, SVG_NS};
use crate::dom_parsing::to_dom_string;
use crate::event_loop::{SharedEventLoop, TaskPriority};
use crate::events::SharedEvents;
use crate::fetch_ws::{AsyncNetEvent, SharedNetBridge};
use crate::streams::{create_writable_stream_from_callbacks, SharedStreams};
use crate::trusted_types::{check_trusted_html_parser_options, check_trusted_html_sink};
use crate::web_apis::{create_dom_exception, SharedWebApis};

/// Unsafe HTML/SVG elements stripped by the WHATWG safe sanitizer baseline.
const UNSAFE_ELEMENTS: &[(&str, &str)] = &[
    (HTML_NS, "script"),
    (SVG_NS, "script"),
    (HTML_NS, "iframe"),
    (HTML_NS, "frame"),
    (HTML_NS, "frameset"),
    (HTML_NS, "object"),
    (HTML_NS, "embed"),
    (HTML_NS, "base"),
    (HTML_NS, "applet"),
    (HTML_NS, "param"),
];

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NameNs {
    pub name: String,
    pub namespace: Option<String>,
}

impl NameNs {
    fn html_element(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            namespace: Some(HTML_NS.to_string()),
        }
    }

    fn attr(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            namespace: None,
        }
    }

    fn matches_element(&self, ns: &str, local: &str) -> bool {
        let target_ns = self.namespace.as_deref().unwrap_or("");
        target_ns == ns && self.name == local
    }

    fn matches_attr(&self, ns: &str, local: &str) -> bool {
        let target_ns = self.namespace.as_deref().unwrap_or("");
        target_ns == ns && self.name == local
    }
}

#[derive(Debug, Clone)]
pub struct ElementSanitizerEntry {
    pub key: NameNs,
    pub attributes: Option<Vec<NameNs>>,
    pub remove_attributes: Option<Vec<NameNs>>,
}

/// Snapshot of a WHATWG `SanitizerConfig` (WHATWG Sanitizer API §3).
#[derive(Debug, Clone)]
pub struct SanitizerConfig {
    pub elements: Option<Vec<ElementSanitizerEntry>>,
    pub remove_elements: Option<Vec<NameNs>>,
    pub replace_with_children_elements: Option<Vec<NameNs>>,
    pub attributes: Option<Vec<NameNs>>,
    pub remove_attributes: Option<Vec<NameNs>>,
    pub comments: bool,
    pub data_attributes: bool,
    pub enforce_safe: bool,
    pub allow_declarative_shadow: bool,
}

impl SanitizerConfig {
    /// Default safe sanitizer configuration (`new Sanitizer()`, `sanitizer: "default"`,
    /// or omitted `sanitizer` on safe methods).
    #[must_use]
    pub fn default_safe() -> Self {
        Self {
            elements: None,
            remove_elements: Some(
                UNSAFE_ELEMENTS
                    .iter()
                    .map(|&(ns, name)| NameNs {
                        name: name.to_string(),
                        namespace: Some(ns.to_string()),
                    })
                    .collect(),
            ),
            replace_with_children_elements: None,
            attributes: None,
            remove_attributes: Some(vec![NameNs::attr("is")]),
            comments: false,
            data_attributes: true,
            enforce_safe: true,
            allow_declarative_shadow: false,
        }
    }

    /// Pass-through configuration for `*Unsafe` methods with no sanitizer specified.
    #[must_use]
    pub fn allow_all() -> Self {
        Self {
            elements: None,
            remove_elements: None,
            replace_with_children_elements: None,
            attributes: None,
            remove_attributes: None,
            comments: true,
            data_attributes: true,
            enforce_safe: false,
            allow_declarative_shadow: true,
        }
    }

    /// Applies WHATWG `removeUnsafe()` baseline on top of any custom config.
    pub fn apply_remove_unsafe(&mut self) {
        self.enforce_safe = true;
        if let Some(ref mut els) = self.elements {
            els.retain(|e| {
                !UNSAFE_ELEMENTS
                    .iter()
                    .any(|&(ns, name)| e.key.namespace.as_deref() == Some(ns) && e.key.name == name)
            });
        } else {
            let rems = self.remove_elements.get_or_insert_with(Vec::new);
            for &(ns, name) in UNSAFE_ELEMENTS {
                let item = NameNs {
                    name: name.to_string(),
                    namespace: Some(ns.to_string()),
                };
                if !rems.contains(&item) {
                    rems.push(item);
                }
            }
        }
    }

    /// Evaluates what to do with an element `(ns, local)`.
    fn element_action(&self, ns: &str, local: &str) -> ElementAction {
        if self.enforce_safe
            && UNSAFE_ELEMENTS
                .iter()
                .any(|&(uns, uname)| uns == ns && uname == local)
        {
            return ElementAction::Remove;
        }
        if let Some(ref rwc) = self.replace_with_children_elements {
            if rwc.iter().any(|k| k.matches_element(ns, local)) {
                return ElementAction::ReplaceWithChildren;
            }
        }
        if let Some(ref els) = self.elements {
            if let Some(idx) = els.iter().position(|e| e.key.matches_element(ns, local)) {
                return ElementAction::Keep(Some(idx));
            }
            return ElementAction::Remove;
        }
        if let Some(ref rems) = self.remove_elements {
            if rems.iter().any(|k| k.matches_element(ns, local)) {
                return ElementAction::Remove;
            }
        }
        ElementAction::Keep(None)
    }

    /// Returns whether attribute `(ans, alocal, aval)` is kept on an element
    /// whose matched `elements` allow-list entry index is `entry_idx`.
    fn allows_attribute(
        &self,
        entry_idx: Option<usize>,
        ans: &str,
        alocal: &str,
        aval: &str,
    ) -> bool {
        let lower_local = alocal.to_ascii_lowercase();
        if self.enforce_safe {
            if ans.is_empty() && lower_local.starts_with("on") {
                return false;
            }
            if matches!(
                lower_local.as_str(),
                "href" | "src" | "action" | "formaction" | "xlink:href"
            ) {
                let trimmed = aval
                    .trim_start_matches(|c: char| c.is_ascii_control() || c.is_ascii_whitespace())
                    .to_ascii_lowercase();
                if trimmed.starts_with("javascript:") || trimmed.starts_with("vbscript:") {
                    return false;
                }
            }
        }

        // Per-element attribute rules override/refine global rules.
        if let Some(idx) = entry_idx {
            if let Some(entry) = self.elements.as_ref().and_then(|v| v.get(idx)) {
                if let Some(ref per_rem) = entry.remove_attributes {
                    if per_rem.iter().any(|a| a.matches_attr(ans, alocal)) {
                        return false;
                    }
                }
                if let Some(ref per_allow) = entry.attributes {
                    if per_allow.iter().any(|a| a.matches_attr(ans, alocal)) {
                        return true;
                    }
                    // If element has explicit `attributes`, only those (plus global `attributes` or data-*) are allowed.
                    if let Some(ref g_allow) = self.attributes {
                        if g_allow.iter().any(|a| a.matches_attr(ans, alocal)) {
                            return true;
                        }
                    }
                    if self.data_attributes && ans.is_empty() && lower_local.starts_with("data-") {
                        return true;
                    }
                    return false;
                }
            }
        }

        if let Some(ref g_allow) = self.attributes {
            if g_allow.iter().any(|a| a.matches_attr(ans, alocal)) {
                return true;
            }
            return self.data_attributes && ans.is_empty() && lower_local.starts_with("data-");
        }
        if let Some(ref g_rem) = self.remove_attributes {
            if g_rem.iter().any(|a| a.matches_attr(ans, alocal)) {
                return false;
            }
        }
        true
    }

    fn allows_declarative_shadow(&self) -> bool {
        if !self.allow_declarative_shadow {
            return false;
        }
        // If `shadowrootmode` is explicitly stripped by attribute rules, DSD must NOT attach.
        self.allows_attribute(None, "", "shadowrootmode", "open")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElementAction {
    Keep(Option<usize>),
    Remove,
    ReplaceWithChildren,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InsertKind {
    Set,
    Append,
    Prepend,
    Before,
    After,
    ReplaceWith,
}

impl InsertKind {
    fn is_sibling(self) -> bool {
        matches!(self, Self::Before | Self::After | Self::ReplaceWith)
    }

    fn removes_immediately(self) -> bool {
        matches!(self, Self::Set | Self::ReplaceWith)
    }
}

/// Installs `Sanitizer`, all 12 positional HTML methods, all 12 streaming HTML methods,
/// and companion HTML reflections (`HTMLTableElement.rows`, `HTMLOptGroupElement.label`,
/// `HTMLScriptElement.{src,type,defer,async}`, `Image`).
pub fn install_dom_sanitizer_and_streaming(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    net: &SharedNetBridge,
) {
    // Register weak connected-script hook so `<script>` elements created by
    // `createContextualFragment`, disconnected `*HTMLUnsafe({ runScripts: true })`,
    // and Trusted Types `createParserOptions({ runScripts: true })` run inline sync
    // and external scripts asynchronously (zero `Rc` cycles via `Weak`).
    {
        let wh = Rc::downgrade(host);
        let wel = Rc::downgrade(event_loop);
        let wwa = Rc::downgrade(web_apis);
        let wn = Rc::downgrade(net);
        host.borrow_mut().script_connect_hook = Some(Rc::new(move |vm, sid| {
            let (Some(h), Some(el), Some(wa), Some(n)) =
                (wh.upgrade(), wel.upgrade(), wwa.upgrade(), wn.upgrade())
            else {
                return;
            };
            run_fragment_script_now(vm, &h, &el, &wa, &n, sid);
        }));
    }
    {
        let wh = Rc::downgrade(host);
        let wev = Rc::downgrade(events);
        let wel = Rc::downgrade(event_loop);
        let wwa = Rc::downgrade(web_apis);
        let wn = Rc::downgrade(net);
        host.borrow_mut().subresource_connect_hook = Some(Rc::new(move |_vm, nid| {
            let (Some(h), Some(ev), Some(el), Some(wa), Some(n)) = (
                wh.upgrade(),
                wev.upgrade(),
                wel.upgrade(),
                wwa.upgrade(),
                wn.upgrade(),
            ) else {
                return;
            };
            schedule_connected_subresource(&h, &ev, &el, &wa, &n, nid);
        }));
    }
    {
        let wh = Rc::downgrade(host);
        let wwa = Rc::downgrade(web_apis);
        let wn = Rc::downgrade(net);
        host.borrow_mut().font_load_hook = Some(Rc::new(move |vm, promise_h, _font_spec| {
            let (Some(h), Some(wa), Some(n)) = (wh.upgrade(), wwa.upgrade(), wn.upgrade()) else {
                let arr = vm.alloc_array();
                rix_js::promise::resolve_promise(vm, promise_h, JsValue::from_object(arr));
                return;
            };
            schedule_web_font_load(vm, &h, &wa, &n, promise_h);
        }));
    }
    install_sanitizer_ctor(vm, host);
    install_html_helper_reflections(vm, host, events, event_loop, web_apis, net);
    install_positional_html_methods(vm, host, events, event_loop, web_apis, net);
    install_streaming_html_methods(vm, host, events, event_loop, web_apis, streams, net);
}

// ---------------------------------------------------------------------------
// `Sanitizer` Constructor & Config Parser
// ---------------------------------------------------------------------------

fn install_sanitizer_ctor(vm: &mut JsVm, host: &SharedHost) {
    let sp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(sp).unwrap().is_prototype = true;
    host.borrow_mut().sanitizer_proto = Some(sp);

    let h = Rc::clone(host);
    define_host_method(vm, sp, "removeElement", move |vm, t, args| {
        let sh = t
            .as_object()
            .filter(|o| h.borrow().sanitizers.contains_key(o))
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let key = parse_element_name_ns(vm, arg(args, 0))?;
        if let Some(cfg) = h.borrow_mut().sanitizers.get_mut(&sh) {
            if let Some(ref mut els) = cfg.elements {
                els.retain(|e| e.key != key);
            } else {
                let rems = cfg.remove_elements.get_or_insert_with(Vec::new);
                if !rems.contains(&key) {
                    rems.push(key);
                }
            }
        }
        Ok(JsValue::TRUE)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sp, "allowElement", move |vm, t, args| {
        let sh = t
            .as_object()
            .filter(|o| h.borrow().sanitizers.contains_key(o))
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let entry = parse_element_entry(vm, arg(args, 0))?;
        if let Some(cfg) = h.borrow_mut().sanitizers.get_mut(&sh) {
            if let Some(ref mut rems) = cfg.remove_elements {
                rems.retain(|k| k != &entry.key);
            }
            if let Some(ref mut rwc) = cfg.replace_with_children_elements {
                rwc.retain(|k| k != &entry.key);
            }
            if let Some(ref mut els) = cfg.elements {
                els.retain(|e| e.key != entry.key);
                els.push(entry);
            }
        }
        Ok(JsValue::TRUE)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sp, "replaceElementWithChildren", move |vm, t, args| {
        let sh = t
            .as_object()
            .filter(|o| h.borrow().sanitizers.contains_key(o))
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let key = parse_element_name_ns(vm, arg(args, 0))?;
        if let Some(cfg) = h.borrow_mut().sanitizers.get_mut(&sh) {
            if let Some(ref mut els) = cfg.elements {
                els.retain(|e| e.key != key);
            }
            if let Some(ref mut rems) = cfg.remove_elements {
                rems.retain(|k| k != &key);
            }
            let rwc = cfg
                .replace_with_children_elements
                .get_or_insert_with(Vec::new);
            if !rwc.contains(&key) {
                rwc.push(key);
            }
        }
        Ok(JsValue::TRUE)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sp, "removeAttribute", move |vm, t, args| {
        let sh = t
            .as_object()
            .filter(|o| h.borrow().sanitizers.contains_key(o))
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let key = parse_attr_name_ns(vm, arg(args, 0))?;
        if let Some(cfg) = h.borrow_mut().sanitizers.get_mut(&sh) {
            if let Some(ref mut allow) = cfg.attributes {
                allow.retain(|k| k != &key);
            } else {
                let rems = cfg.remove_attributes.get_or_insert_with(Vec::new);
                if !rems.contains(&key) {
                    rems.push(key);
                }
            }
        }
        Ok(JsValue::TRUE)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sp, "allowAttribute", move |vm, t, args| {
        let sh = t
            .as_object()
            .filter(|o| h.borrow().sanitizers.contains_key(o))
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let key = parse_attr_name_ns(vm, arg(args, 0))?;
        if let Some(cfg) = h.borrow_mut().sanitizers.get_mut(&sh) {
            if let Some(ref mut rems) = cfg.remove_attributes {
                rems.retain(|k| k != &key);
            }
            if let Some(ref mut allow) = cfg.attributes {
                if !allow.contains(&key) {
                    allow.push(key);
                }
            }
        }
        Ok(JsValue::TRUE)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sp, "setComments", move |vm, t, args| {
        let sh = t
            .as_object()
            .filter(|o| h.borrow().sanitizers.contains_key(o))
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let allow = vm.to_boolean(arg(args, 0));
        if let Some(cfg) = h.borrow_mut().sanitizers.get_mut(&sh) {
            cfg.comments = allow;
        }
        Ok(JsValue::TRUE)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sp, "setDataAttributes", move |vm, t, args| {
        let sh = t
            .as_object()
            .filter(|o| h.borrow().sanitizers.contains_key(o))
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let allow = vm.to_boolean(arg(args, 0));
        if let Some(cfg) = h.borrow_mut().sanitizers.get_mut(&sh) {
            cfg.data_attributes = allow;
        }
        Ok(JsValue::TRUE)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sp, "removeUnsafe", move |vm, t, _| {
        let sh = t
            .as_object()
            .filter(|o| h.borrow().sanitizers.contains_key(o))
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        if let Some(cfg) = h.borrow_mut().sanitizers.get_mut(&sh) {
            cfg.apply_remove_unsafe();
        }
        Ok(JsValue::TRUE)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sp, "get", move |vm, t, _| {
        let sh = t
            .as_object()
            .filter(|o| h.borrow().sanitizers.contains_key(o))
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let cfg = h.borrow().sanitizers.get(&sh).cloned().unwrap();
        let obj = vm.alloc_ordinary_object();
        let kc = vm.heap.intern_key("comments");
        vm.heap.object_define_data(
            obj,
            kc,
            JsValue::from_bool(cfg.comments),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        let kd = vm.heap.intern_key("dataAttributes");
        vm.heap.object_define_data(
            obj,
            kd,
            JsValue::from_bool(cfg.data_attributes),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        Ok(JsValue::from_object(obj))
    });

    let h = Rc::clone(host);
    let ctor = vm.alloc_host_fn("Sanitizer", move |vm, _, args| {
        let cfg = if args.is_empty() || arg(args, 0).is_undefined() {
            SanitizerConfig::default_safe()
        } else {
            parse_sanitizer_config_dict(vm, arg(args, 0), false)?
        };
        let inst = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(sp)));
        h.borrow_mut().sanitizers.insert(inst, cfg);
        Ok(JsValue::from_object(inst))
    });
    let kp = vm.key_prototype;
    vm.heap.object_define_data(
        ctor,
        kp,
        JsValue::from_object(sp),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let kc = vm.key_constructor;
    vm.heap.object_define_data(
        sp,
        kc,
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.mark_constructor(ctor);
    define_global(vm, "Sanitizer", JsValue::from_object(ctor));
}

fn parse_element_name_ns(vm: &mut JsVm, val: JsValue) -> Result<NameNs, JsException> {
    if let Some(oh) = val.as_object() {
        let kn = vm.heap.intern_key("name");
        let name_v = vm.get_property_by_key(oh, kn)?;
        if name_v.is_undefined() {
            return Err(vm.type_error("SanitizerElementNamespace requires 'name'"));
        }
        let name = to_dom_string(vm, name_v)?;
        let kns = vm.heap.intern_key("namespace");
        let ns_v = vm.get_property_by_key(oh, kns)?;
        let namespace = if ns_v.is_undefined() {
            Some(HTML_NS.to_string())
        } else if ns_v.is_null() {
            None
        } else {
            let s = to_dom_string(vm, ns_v)?;
            (!s.is_empty()).then_some(s)
        };
        Ok(NameNs { name, namespace })
    } else {
        let name = to_dom_string(vm, val)?;
        Ok(NameNs::html_element(name))
    }
}

fn parse_attr_name_ns(vm: &mut JsVm, val: JsValue) -> Result<NameNs, JsException> {
    if let Some(oh) = val.as_object() {
        let kn = vm.heap.intern_key("name");
        let name_v = vm.get_property_by_key(oh, kn)?;
        if name_v.is_undefined() {
            return Err(vm.type_error("SanitizerAttributeNamespace requires 'name'"));
        }
        let name = to_dom_string(vm, name_v)?;
        let kns = vm.heap.intern_key("namespace");
        let ns_v = vm.get_property_by_key(oh, kns)?;
        let namespace = if ns_v.is_undefined() || ns_v.is_null() {
            None
        } else {
            let s = to_dom_string(vm, ns_v)?;
            (!s.is_empty()).then_some(s)
        };
        Ok(NameNs { name, namespace })
    } else {
        let name = to_dom_string(vm, val)?;
        Ok(NameNs::attr(name))
    }
}

fn parse_element_entry(vm: &mut JsVm, val: JsValue) -> Result<ElementSanitizerEntry, JsException> {
    let key = parse_element_name_ns(vm, val)?;
    let mut attributes = None;
    let mut remove_attributes = None;
    if let Some(oh) = val.as_object() {
        let ka = vm.heap.intern_key("attributes");
        let av = vm.get_property_by_key(oh, ka)?;
        if !av.is_undefined() {
            attributes = Some(parse_attr_list(vm, av)?);
        }
        let kra = vm.heap.intern_key("removeAttributes");
        let rav = vm.get_property_by_key(oh, kra)?;
        if !rav.is_undefined() {
            remove_attributes = Some(parse_attr_list(vm, rav)?);
        }
        if attributes.is_some() && remove_attributes.is_some() {
            return Err(vm.type_error(
                "Sanitizer element entry cannot specify both 'attributes' and 'removeAttributes'",
            ));
        }
    }
    Ok(ElementSanitizerEntry {
        key,
        attributes,
        remove_attributes,
    })
}

fn parse_attr_list(vm: &mut JsVm, val: JsValue) -> Result<Vec<NameNs>, JsException> {
    let items = vm.collect_iterable(val)?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(parse_attr_name_ns(vm, item)?);
    }
    Ok(out)
}

fn parse_name_ns_list(vm: &mut JsVm, val: JsValue) -> Result<Vec<NameNs>, JsException> {
    let items = vm.collect_iterable(val)?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(parse_element_name_ns(vm, item)?);
    }
    Ok(out)
}

fn parse_sanitizer_config_dict(
    vm: &mut JsVm,
    val: JsValue,
    is_safe: bool,
) -> Result<SanitizerConfig, JsException> {
    let Some(oh) = val.as_object() else {
        return Err(vm.type_error("Sanitizer configuration must be an object"));
    };
    let ke = vm.heap.intern_key("elements");
    let kre = vm.heap.intern_key("removeElements");
    let krwc = vm.heap.intern_key("replaceWithChildrenElements");
    let ka = vm.heap.intern_key("attributes");
    let kra = vm.heap.intern_key("removeAttributes");
    let kc = vm.heap.intern_key("comments");
    let kda = vm.heap.intern_key("dataAttributes");

    let ev = vm.get_property_by_key(oh, ke)?;
    let rev = vm.get_property_by_key(oh, kre)?;
    let rwcv = vm.get_property_by_key(oh, krwc)?;
    let av = vm.get_property_by_key(oh, ka)?;
    let rav = vm.get_property_by_key(oh, kra)?;
    let cv = vm.get_property_by_key(oh, kc)?;
    let dav = vm.get_property_by_key(oh, kda)?;

    let elements = if ev.is_undefined() {
        None
    } else {
        let items = vm.collect_iterable(ev)?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            out.push(parse_element_entry(vm, item)?);
        }
        Some(out)
    };
    let remove_elements = if rev.is_undefined() {
        None
    } else {
        Some(parse_name_ns_list(vm, rev)?)
    };
    let replace_with_children_elements = if rwcv.is_undefined() {
        None
    } else {
        Some(parse_name_ns_list(vm, rwcv)?)
    };
    let attributes = if av.is_undefined() {
        None
    } else {
        Some(parse_attr_list(vm, av)?)
    };
    let remove_attributes = if rav.is_undefined() {
        None
    } else {
        Some(parse_attr_list(vm, rav)?)
    };

    // WHATWG Sanitizer API §3.1 config validity checks:
    if elements.is_some() && remove_elements.is_some() {
        return Err(
            vm.type_error("Sanitizer config cannot contain both 'elements' and 'removeElements'")
        );
    }
    if attributes.is_some() && remove_attributes.is_some() {
        return Err(vm.type_error(
            "Sanitizer config cannot contain both 'attributes' and 'removeAttributes'",
        ));
    }
    if let (Some(ref els), Some(ref rwc)) = (&elements, &replace_with_children_elements) {
        for r in rwc {
            if els.iter().any(|e| &e.key == r) {
                return Err(vm.type_error(
                    "Element cannot appear in both 'elements' and 'replaceWithChildrenElements'",
                ));
            }
        }
    }
    if let (Some(ref rems), Some(ref rwc)) = (&remove_elements, &replace_with_children_elements) {
        for r in rwc {
            if rems.contains(r) {
                return Err(vm.type_error(
                    "Element cannot appear in both 'removeElements' and 'replaceWithChildrenElements'",
                ));
            }
        }
    }
    if let (Some(ref els), Some(ref g_allow)) = (&elements, &attributes) {
        let _ = (els, g_allow);
    }

    let comments = if cv.is_undefined() {
        !is_safe
    } else {
        vm.to_boolean(cv)
    };
    let data_attributes = if dav.is_undefined() {
        attributes.is_none()
    } else {
        if remove_attributes.is_some() && vm.to_boolean(dav) {
            return Err(vm.type_error(
                "Sanitizer config cannot specify 'dataAttributes: true' with 'removeAttributes'",
            ));
        }
        vm.to_boolean(dav)
    };

    let mut cfg = SanitizerConfig {
        elements,
        remove_elements,
        replace_with_children_elements,
        attributes,
        remove_attributes,
        comments,
        data_attributes,
        enforce_safe: is_safe,
        allow_declarative_shadow: true,
    };
    if is_safe {
        cfg.apply_remove_unsafe();
    }
    Ok(cfg)
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedMethodOptions {
    pub sanitizer: SanitizerConfig,
    pub run_scripts: bool,
}

pub(crate) fn parse_html_method_options(
    vm: &mut JsVm,
    host: &SharedHost,
    opts_val: JsValue,
    is_safe: bool,
) -> Result<ParsedMethodOptions, JsException> {
    if opts_val.is_undefined() || opts_val.is_null() {
        return Ok(ParsedMethodOptions {
            sanitizer: if is_safe {
                SanitizerConfig::default_safe()
            } else {
                SanitizerConfig::allow_all()
            },
            run_scripts: false,
        });
    }
    let Some(oh) = opts_val.as_object() else {
        return Err(vm.type_error("HTML insertion options must be an object"));
    };
    let k_rs = vm.heap.intern_key("runScripts");
    let run_scripts = if is_safe {
        false
    } else {
        vm.get_property_by_key(oh, k_rs)
            .is_ok_and(|v| !v.is_undefined() && vm.to_boolean(v))
    };

    let k_san = vm.heap.intern_key("sanitizer");
    let san_v = vm.get_property_by_key(oh, k_san)?;
    let sanitizer = if san_v.is_undefined() {
        if is_safe {
            SanitizerConfig::default_safe()
        } else {
            SanitizerConfig::allow_all()
        }
    } else if san_v.is_string() {
        let s = to_dom_string(vm, san_v)?;
        if s == "default" {
            SanitizerConfig::default_safe()
        } else {
            return Err(vm.type_error(format!("Unknown sanitizer preset '{s}'")));
        }
    } else if let Some(sh) = san_v.as_object() {
        if let Some(mut existing) = host.borrow().sanitizers.get(&sh).cloned() {
            if is_safe {
                existing.apply_remove_unsafe();
            }
            existing
        } else {
            parse_sanitizer_config_dict(vm, san_v, is_safe)?
        }
    } else {
        return Err(vm.type_error("Invalid 'sanitizer' option"));
    };

    Ok(ParsedMethodOptions {
        sanitizer,
        run_scripts,
    })
}

// ---------------------------------------------------------------------------
// Companion HTML Reflections (`table.rows`, `optgroup.label`, `script.*`, `Image`)
// ---------------------------------------------------------------------------

fn install_str_attr_prop(
    vm: &mut JsVm,
    proto: HeapHandle,
    host: &SharedHost,
    prop: &str,
    attr: &'static str,
) {
    let h1 = Rc::clone(host);
    let g = vm.alloc_host_fn(prop, move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        let v = match h1.borrow().document.borrow().get(nid) {
            Some(NodeData::Element(e)) => e.get_attribute(attr).unwrap_or("").to_string(),
            _ => String::new(),
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(v)))
    });
    let h2 = Rc::clone(host);
    let s = vm.alloc_host_fn(prop, move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let v = to_dom_string(vm, arg(args, 0))?;
        if let Some(NodeData::Element(e)) = h2.borrow().document.borrow_mut().get_mut(nid) {
            e.set_attribute(attr, &v);
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, proto, prop, Some(g), Some(s));
}

fn install_html_helper_reflections(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
) {
    let protos = host.borrow().interface_protos.clone();

    // `HTMLTableElement.prototype.rows`
    if let Some(&tp) = protos.get("HTMLTableElement") {
        let h = Rc::clone(host);
        let g = vm.alloc_host_fn("get rows", move |vm, t, _| {
            let nid = require_node(vm, &h, t)?;
            let rows: Vec<NodeId> = {
                let doc_rc = Rc::clone(&h.borrow().document);
                let doc = doc_rc.borrow();
                let mut out = Vec::new();
                for child in doc.children(nid) {
                    if let Some(NodeData::Element(e)) = doc.get(child) {
                        match e.name.local.as_str() {
                            "tr" => out.push(child),
                            "thead" | "tbody" | "tfoot" => {
                                out.extend(doc.children(child).filter(|&gc| {
                                    matches!(
                                        doc.get(gc),
                                        Some(NodeData::Element(ge)) if ge.name.local.as_str() == "tr"
                                    )
                                }));
                            }
                            _ => {}
                        }
                    }
                }
                out
            };
            let arr = vm.alloc_array();
            for r in rows {
                let w = wrap_node(vm, &h, r);
                vm.heap.object_push_element(arr, w);
            }
            Ok(JsValue::from_object(arr))
        });
        define_host_accessor(vm, tp, "rows", Some(g), None);

        // `HTMLTableElement.prototype.{caption, tHead, tFoot}` + `deleteCaption/deleteTHead/deleteTFoot/deleteRow`
        for (prop, tag, del_method) in [
            ("caption", "caption", "deleteCaption"),
            ("tHead", "thead", "deleteTHead"),
            ("tFoot", "tfoot", "deleteTFoot"),
        ] {
            let h1 = Rc::clone(host);
            let g = vm.alloc_host_fn(&format!("get {prop}"), move |vm, t, _| {
                let tid = require_node(vm, &h1, t)?;
                let found = {
                    let hb = h1.borrow();
                    let doc = hb.document.borrow();
                    let found = doc.children(tid).find(|&c| {
                        matches!(
                            doc.get(c),
                            Some(NodeData::Element(e)) if e.name.local.as_str().eq_ignore_ascii_case(tag)
                        )
                    });
                    found
                };
                Ok(found.map_or(JsValue::NULL, |n| wrap_node(vm, &h1, n)))
            });
            let h2 = Rc::clone(host);
            let s = vm.alloc_host_fn(&format!("set {prop}"), move |vm, t, args| {
                let tid = require_node(vm, &h2, t)?;
                let v = arg(args, 0);
                let new_node = if v.is_null() {
                    None
                } else {
                    let nid = require_node(vm, &h2, v)?;
                    let ok = matches!(
                        h2.borrow().document.borrow().get(nid),
                        Some(NodeData::Element(e)) if e.name.local.as_str().eq_ignore_ascii_case(tag)
                    );
                    if !ok {
                        return Err(crate::web_apis::create_dom_exception(
                            vm,
                            "HierarchyRequestError",
                            &format!("HTMLTableElement.{prop} requires a <{tag}> element"),
                        ));
                    }
                    Some(nid)
                };
                crate::custom_elements::with_ce_reactions(vm, &h2, |vm| {
                    let existing = {
                        let hb = h2.borrow();
                        let doc = hb.document.borrow();
                        let found = doc.children(tid).find(|&c| {
                            matches!(
                                doc.get(c),
                                Some(NodeData::Element(e)) if e.name.local.as_str().eq_ignore_ascii_case(tag)
                            )
                        });
                        found
                    };
                    if let Some(ex) = existing {
                        crate::dom_bridge::remove_node_from_parent(vm, &h2, tid, ex)?;
                    }
                    if let Some(nn) = new_node {
                        let before = if tag == "caption" {
                            h2.borrow().document.borrow().first_child(tid)
                        } else {
                            None
                        };
                        crate::dom_bridge::insert_node_into_parent(vm, &h2, tid, nn, before)?;
                    }
                    Ok(())
                })?;
                Ok(JsValue::UNDEFINED)
            });
            define_host_accessor(vm, tp, prop, Some(g), Some(s));

            let h3 = Rc::clone(host);
            define_host_method(vm, tp, del_method, move |vm, t, _| {
                let tid = require_node(vm, &h3, t)?;
                crate::custom_elements::with_ce_reactions(vm, &h3, |vm| {
                    let existing = {
                        let hb = h3.borrow();
                        let doc = hb.document.borrow();
                        let found = doc.children(tid).find(|&c| {
                            matches!(
                                doc.get(c),
                                Some(NodeData::Element(e)) if e.name.local.as_str().eq_ignore_ascii_case(tag)
                            )
                        });
                        found
                    };
                    if let Some(ex) = existing {
                        crate::dom_bridge::remove_node_from_parent(vm, &h3, tid, ex)?;
                    }
                    Ok(())
                })?;
                Ok(JsValue::UNDEFINED)
            });
        }
        let h_dr = Rc::clone(host);
        define_host_method(vm, tp, "deleteRow", move |vm, t, args| {
            let tid = require_node(vm, &h_dr, t)?;
            let idx = vm.to_number(arg(args, 0))? as isize;
            let rows: Vec<NodeId> = {
                let hb = h_dr.borrow();
                let doc = hb.document.borrow();
                let mut out = Vec::new();
                for child in doc.children(tid) {
                    if let Some(NodeData::Element(e)) = doc.get(child) {
                        match e.name.local.as_str() {
                            "tr" => out.push(child),
                            "thead" | "tbody" | "tfoot" => {
                                out.extend(doc.children(child).filter(|&gc| {
                                    matches!(
                                        doc.get(gc),
                                        Some(NodeData::Element(ge)) if ge.name.local.as_str() == "tr"
                                    )
                                }));
                            }
                            _ => {}
                        }
                    }
                }
                out
            };
            let target_row = if idx == -1 {
                rows.last().copied()
            } else if idx >= 0 {
                rows.get(idx as usize).copied()
            } else {
                None
            };
            let Some(row) = target_row else {
                return Err(crate::web_apis::create_dom_exception(
                    vm,
                    "IndexSizeError",
                    "Row index out of range",
                ));
            };
            let row_parent = h_dr.borrow().document.borrow().parent(row);
            if let Some(rp) = row_parent {
                crate::custom_elements::with_ce_reactions(vm, &h_dr, |vm| {
                    crate::dom_bridge::remove_node_from_parent(vm, &h_dr, rp, row)
                })?;
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // `HTMLSelectElement.prototype.{options, length, add, remove}` with `[CEReactions]`
    if let Some(&sp) = protos.get("HTMLSelectElement") {
        let collect_options = |h: &SharedHost, sid: NodeId| -> Vec<NodeId> {
            let hb = h.borrow();
            let doc = hb.document.borrow();
            doc.descendants(sid)
                .filter(|&c| {
                    c != sid
                        && matches!(
                            doc.get(c),
                            Some(NodeData::Element(e)) if e.name.local.as_str().eq_ignore_ascii_case("option")
                        )
                })
                .collect()
        };
        let h1 = Rc::clone(host);
        let g_opts = vm.alloc_host_fn("get options", move |vm, t, _| {
            let sid = require_node(vm, &h1, t)?;
            let opts = collect_options(&h1, sid);
            let arr = vm.alloc_array();
            for o in opts {
                let w = wrap_node(vm, &h1, o);
                vm.heap.object_push_element(arr, w);
            }
            Ok(JsValue::from_object(arr))
        });
        define_host_accessor(vm, sp, "options", Some(g_opts), None);

        let h1 = Rc::clone(host);
        let g_len = vm.alloc_host_fn("get length", move |vm, t, _| {
            let sid = require_node(vm, &h1, t)?;
            let len = collect_options(&h1, sid).len();
            Ok(JsValue::from_f64(len as f64))
        });
        let h2 = Rc::clone(host);
        let s_len = vm.alloc_host_fn("set length", move |vm, t, args| {
            let sid = require_node(vm, &h2, t)?;
            let target_len = vm.to_number(arg(args, 0))?.max(0.0) as usize;
            crate::custom_elements::with_ce_reactions(vm, &h2, |vm| {
                let opts = collect_options(&h2, sid);
                if target_len < opts.len() {
                    for &opt in &opts[target_len..] {
                        let parent_opt = h2.borrow().document.borrow().parent(opt);
                        if let Some(p) = parent_opt {
                            crate::dom_bridge::remove_node_from_parent(vm, &h2, p, opt)?;
                        }
                    }
                } else if target_len > opts.len() {
                    let owner = h2.borrow().owner_doc_of(sid);
                    for _ in opts.len()..target_len {
                        let new_opt = h2.borrow().document.borrow_mut().create_element_ns(
                            crate::dom_interfaces::HTML_NS,
                            None,
                            "option",
                        );
                        h2.borrow_mut().set_owner_doc(new_opt, owner);
                        crate::dom_bridge::insert_node_into_parent(vm, &h2, sid, new_opt, None)?;
                    }
                }
                Ok(())
            })?;
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, sp, "length", Some(g_len), Some(s_len));

        let h_add = Rc::clone(host);
        define_host_method(vm, sp, "add", move |vm, t, args| {
            let sid = require_node(vm, &h_add, t)?;
            let item = require_node(vm, &h_add, arg(args, 0))?;
            let before_arg = arg(args, 1);
            let before = if before_arg.is_undefined() || before_arg.is_null() {
                None
            } else if before_arg.is_number() {
                let idx = vm.to_number(before_arg)? as isize;
                if idx >= 0 {
                    collect_options(&h_add, sid).get(idx as usize).copied()
                } else {
                    None
                }
            } else {
                Some(require_node(vm, &h_add, before_arg)?)
            };
            crate::custom_elements::with_ce_reactions(vm, &h_add, |vm| {
                crate::dom_bridge::insert_node_into_parent(vm, &h_add, sid, item, before)
            })?;
            Ok(JsValue::UNDEFINED)
        });

        let h_rem = Rc::clone(host);
        define_host_method(vm, sp, "remove", move |vm, t, args| {
            let sid = require_node(vm, &h_rem, t)?;
            if args.is_empty() {
                // `ChildNode.prototype.remove()` fallback when called with 0 args!
                let parent_opt = h_rem.borrow().document.borrow().parent(sid);
                if let Some(p) = parent_opt {
                    crate::custom_elements::with_ce_reactions(vm, &h_rem, |vm| {
                        crate::dom_bridge::remove_node_from_parent(vm, &h_rem, p, sid)
                    })?;
                }
                return Ok(JsValue::UNDEFINED);
            }
            let idx = vm.to_number(arg(args, 0))? as isize;
            if idx >= 0 {
                if let Some(&opt) = collect_options(&h_rem, sid).get(idx as usize) {
                    let parent_opt = h_rem.borrow().document.borrow().parent(opt);
                    if let Some(p) = parent_opt {
                        crate::custom_elements::with_ce_reactions(vm, &h_rem, |vm| {
                            crate::dom_bridge::remove_node_from_parent(vm, &h_rem, p, opt)
                        })?;
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // `HTMLOptGroupElement.prototype.label` & `HTMLScriptElement.prototype.{src, type, defer, async}`
    if let Some(&ogp) = protos.get("HTMLOptGroupElement") {
        install_str_attr_prop(vm, ogp, host, "label", "label");
    }
    if let Some(&sp) = protos.get("HTMLScriptElement") {
        // HTML §4.12.1.1 / §2.6.10: `HTMLScriptElement.prototype.src` reflects as a resolved URL
        // against the document's base URL (or `""` when absent/empty).
        let h_src_g = Rc::clone(host);
        let src_g = vm.alloc_host_fn("get src", move |vm, t, _| {
            let nid = require_node(vm, &h_src_g, t)?;
            let hb = h_src_g.borrow();
            let raw = match hb.document.borrow().get(nid) {
                Some(NodeData::Element(e)) => e.get_attribute("src").map(ToString::to_string),
                _ => None,
            };
            let resolved = match raw {
                Some(ref r) => {
                    let trimmed = r.trim();
                    if trimmed.is_empty() {
                        hb.main_document_url.clone()
                    } else if let Ok(base) = url::Url::parse(&hb.main_document_url) {
                        base.join(trimmed)
                            .map(|u| u.to_string())
                            .unwrap_or_else(|_| trimmed.to_string())
                    } else {
                        trimmed.to_string()
                    }
                }
                None => String::new(),
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(resolved)))
        });
        let h_src_s = Rc::clone(host);
        let src_s = vm.alloc_host_fn("set src", move |vm, t, args| {
            let nid = require_node(vm, &h_src_s, t)?;
            let val = to_dom_string(vm, arg(args, 0))?;
            if let Some(NodeData::Element(e)) = h_src_s.borrow().document.borrow_mut().get_mut(nid)
            {
                e.set_attribute("src", &val);
            }
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, sp, "src", Some(src_g), Some(src_s));
        install_str_attr_prop(vm, sp, host, "type", "type");
        for attr in ["defer", "async"] {
            let h1 = Rc::clone(host);
            let is_async_prop = attr == "async";
            let g = vm.alloc_host_fn(attr, move |vm, t, _| {
                let nid = require_node(vm, &h1, t)?;
                let hb = h1.borrow();
                let has = matches!(
                    hb.document.borrow().get(nid),
                    Some(NodeData::Element(e)) if e.has_attribute(attr)
                ) || (is_async_prop && hb.script_force_async.contains(&nid));
                Ok(JsValue::from_bool(has))
            });
            let h2 = Rc::clone(host);
            let s = vm.alloc_host_fn(attr, move |vm, t, args| {
                let nid = require_node(vm, &h2, t)?;
                let on = vm.to_boolean(arg(args, 0));
                if is_async_prop {
                    h2.borrow_mut().script_force_async.remove(&nid);
                }
                if let Some(NodeData::Element(e)) = h2.borrow().document.borrow_mut().get_mut(nid) {
                    if on {
                        e.set_attribute(attr, "");
                    } else {
                        e.remove_attribute(attr);
                    }
                }
                Ok(JsValue::UNDEFINED)
            });
            define_host_accessor(vm, sp, attr, Some(g), Some(s));
        }
    }

    // `HTMLImageElement.prototype.src` async error dispatch + `new Image(w, h)` constructor
    if let Some(&img_proto) = protos.get("HTMLImageElement") {
        let h1 = Rc::clone(host);
        let g = vm.alloc_host_fn("get src", move |vm, t, _| {
            let nid = require_node(vm, &h1, t)?;
            let hb = h1.borrow();
            let raw = match hb.document.borrow().get(nid) {
                Some(NodeData::Element(e)) => e.get_attribute("src").map(ToString::to_string),
                _ => None,
            };
            let v = match raw {
                Some(ref r) => {
                    let trimmed = r.trim();
                    if trimmed.is_empty() {
                        hb.main_document_url.clone()
                    } else if let Ok(base) = url::Url::parse(&hb.main_document_url) {
                        base.join(trimmed)
                            .map(|u| u.to_string())
                            .unwrap_or_else(|_| trimmed.to_string())
                    } else {
                        trimmed.to_string()
                    }
                }
                None => String::new(),
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(v)))
        });
        let h2 = Rc::clone(host);
        let ev2 = Rc::clone(events);
        let el2 = Rc::clone(event_loop);
        let wa2 = Rc::clone(web_apis);
        let n2 = Rc::clone(net);
        let s = vm.alloc_host_fn("set src", move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            let val = to_dom_string(vm, arg(args, 0))?;
            if let Some(NodeData::Element(e)) = h2.borrow().document.borrow_mut().get_mut(nid) {
                e.set_attribute("src", &val);
            }
            if !val.trim().is_empty() {
                schedule_dynamic_image_load(&h2, &ev2, &el2, &wa2, &n2, nid, &val);
            } else {
                let mut hmut = h2.borrow_mut();
                hmut.pending_dynamic_images.remove(&nid);
                hmut.completed_images.remove(&nid);
            }
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, img_proto, "src", Some(g), Some(s));

        let h_img = Rc::clone(host);
        let img_ctor = vm.alloc_host_fn("Image", move |vm, _, args| {
            let nid = {
                let doc_rc = Rc::clone(&h_img.borrow().document);
                let mut doc = doc_rc.borrow_mut();
                let n = doc.create_element_ns(HTML_NS, None, "img");
                if let Some(NodeData::Element(e)) = doc.get_mut(n) {
                    if !args.is_empty() && !arg(args, 0).is_undefined() {
                        if let Some(w) = arg(args, 0).as_f64() {
                            e.set_attribute("width", &(w as i64).to_string());
                        }
                    }
                    if args.len() >= 2 && !arg(args, 1).is_undefined() {
                        if let Some(h) = arg(args, 1).as_f64() {
                            e.set_attribute("height", &(h as i64).to_string());
                        }
                    }
                }
                n
            };
            let root = h_img.borrow().document.borrow().root();
            h_img.borrow_mut().set_owner_doc(nid, root);
            Ok(wrap_node(vm, &h_img, nid))
        });
        let kp = vm.key_prototype;
        vm.heap.object_define_data(
            img_ctor,
            kp,
            JsValue::from_object(img_proto),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
        vm.mark_constructor(img_ctor);
        define_global(vm, "Image", JsValue::from_object(img_ctor));
    }
}

// ---------------------------------------------------------------------------
// Target Validation & Context Element Resolution (Shared by Positional & Streaming)
// ---------------------------------------------------------------------------

struct ResolvedTarget {
    /// Node in `host.document` where parsed children will be inserted.
    insert_container: NodeId,
    /// Original logical element/shadow-root container (differs from `insert_container` for `<template>`).
    logical_container: NodeId,
    /// Reference child before which inserted nodes go (`None` = append at end).
    ref_node: Option<NodeId>,
    /// Context element `QualName` for the HTML fragment parser.
    context_qname: QualName,
    /// Whether `insert_container` is a `<template>.content` inert document fragment.
    is_template_content: bool,
    /// Owner document of `insert_container`.
    owner_doc: NodeId,
}

fn is_script_element(doc: &Document, nid: NodeId) -> bool {
    matches!(
        doc.get(nid),
        Some(NodeData::Element(e))
            if e.name.local.as_str() == "script"
                && (&*e.name.ns == HTML_NS || &*e.name.ns == SVG_NS)
    )
}

fn is_html_element(doc: &Document, nid: NodeId, tag: &str) -> bool {
    matches!(
        doc.get(nid),
        Some(NodeData::Element(e)) if &*e.name.ns == HTML_NS && e.name.local.as_str() == tag
    )
}

fn resolve_target_for_html_method(
    vm: &mut JsVm,
    host: &SharedHost,
    self_nid: NodeId,
    kind: InsertKind,
    is_streaming: bool,
    is_safe: bool,
) -> Result<Option<ResolvedTarget>, JsException> {
    let hb = host.borrow();
    let doc = hb.document.borrow();

    // 1. Resolve logical container & ref node + HierarchyRequestError checks BEFORE sanitizer validation!
    let (logical_container, ref_node) = if kind.is_sibling() {
        let Some(parent) = doc.parent(self_nid) else {
            if is_streaming {
                return Err(create_dom_exception(
                    vm,
                    "HierarchyRequestError",
                    "Cannot stream positional HTML on a node without a parent",
                ));
            }
            // Non-streaming positional methods on detached node are a silent no-op returning undefined.
            return Ok(None);
        };
        // Parent must be an Element, ShadowRoot, or `<template>.content` DocumentFragment
        // (not Document, plain standalone DocumentFragment, HTML `<html>`, or HTML `<template>`).
        match doc.get(parent) {
            Some(NodeData::Document) => {
                return Err(create_dom_exception(
                    vm,
                    "HierarchyRequestError",
                    "Cannot insert positional HTML when parent is a Document",
                ));
            }
            Some(NodeData::DocumentFragment) => {
                let is_template_frag = find_template_host_for_fragment(&doc, parent).is_some();
                if !is_template_frag {
                    return Err(create_dom_exception(
                        vm,
                        "HierarchyRequestError",
                        "Cannot insert positional HTML when parent is a DocumentFragment",
                    ));
                }
            }
            Some(NodeData::Element(e))
                if &*e.name.ns == HTML_NS
                    && (e.name.local.as_str() == "html" || e.name.local.as_str() == "template") =>
            {
                return Err(create_dom_exception(
                    vm,
                    "HierarchyRequestError",
                    "Cannot insert positional HTML as child of <html> or direct child of <template>",
                ));
            }
            _ => {}
        }
        let r = match kind {
            InsertKind::Before => Some(self_nid),
            InsertKind::After | InsertKind::ReplaceWith => doc.next_sibling(self_nid),
            _ => unreachable!(),
        };
        (parent, r)
    } else {
        // Container methods: `set`, `append`, `prepend`
        if matches!(kind, InsertKind::Append | InsertKind::Prepend)
            && is_html_element(&doc, self_nid, "html")
        {
            return Err(create_dom_exception(
                vm,
                "HierarchyRequestError",
                "Cannot appendHTML/prependHTML directly onto <html>",
            ));
        }
        let r = match kind {
            InsertKind::Prepend => {
                let container = match doc.get(self_nid) {
                    Some(NodeData::Element(e))
                        if &*e.name.ns == HTML_NS && e.name.local.as_str() == "template" =>
                    {
                        e.template_contents.unwrap_or(self_nid)
                    }
                    _ => self_nid,
                };
                doc.first_child(container)
            }
            _ => None,
        };
        (self_nid, r)
    };

    // 2. `<script>` targeting checks (BEFORE sanitizer config validation!):
    if is_script_element(&doc, logical_container) {
        if is_streaming {
            return Err(create_dom_exception(
                vm,
                "NotSupportedError",
                "Streaming HTML methods cannot target a <script> element",
            ));
        }
        if is_safe {
            // Safe positional methods targeting HTML/SVG `<script>` are a silent no-op!
            return Ok(None);
        }
    }

    // 3. Determine actual insertion container (`template.content` for `<template>`), context element, & ownerDocument.
    let (insert_container, is_template_content, context_qname) = match doc.get(logical_container) {
        Some(NodeData::Element(e))
            if &*e.name.ns == HTML_NS && e.name.local.as_str() == "template" =>
        {
            let tc = e.template_contents.unwrap_or(logical_container);
            (tc, true, e.name.clone())
        }
        Some(NodeData::DocumentFragment) => (
            logical_container,
            true,
            QualName::new(None, Namespace::from(HTML_NS), LocalName::from("template")),
        ),
        Some(NodeData::Element(e)) => (logical_container, false, e.name.clone()),
        Some(NodeData::ShadowRoot { host: sh, .. }) => {
            let q = match doc.get(*sh) {
                Some(NodeData::Element(e)) => e.name.clone(),
                _ => QualName::new(None, Namespace::from(HTML_NS), LocalName::from("body")),
            };
            (logical_container, false, q)
        }
        _ => (
            logical_container,
            false,
            QualName::new(None, Namespace::from(HTML_NS), LocalName::from("body")),
        ),
    };

    let owner_doc = hb.owner_doc_of(logical_container);
    Ok(Some(ResolvedTarget {
        insert_container,
        logical_container,
        ref_node,
        context_qname,
        is_template_content,
        owner_doc,
    }))
}

fn find_template_host_for_fragment(doc: &Document, frag_id: NodeId) -> Option<NodeId> {
    doc.is_template_contents_fragment(frag_id)
        .then_some(frag_id)
}

fn is_live_browsing_context_doc(host: &SharedHost, doc_id: NodeId) -> bool {
    let hb = host.borrow();
    if doc_id == hb.document.borrow().root() {
        return true;
    }
    hb.doc_meta
        .get(&doc_id)
        .is_some_and(|m| m.default_view.is_some())
}

// ---------------------------------------------------------------------------
// Live Incremental Mirror + Sanitizer Synchronization Engine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum MirrorEntry {
    Mapped(NodeId),
    Removed,
    Unwrapped,
    ShadowRoot(NodeId),
}

struct StreamMirrorState {
    parser: HtmlFragmentParserStream,
    sanitizer: SanitizerConfig,
    insert_container: NodeId,
    logical_container: NodeId,
    ref_node: Option<NodeId>,
    owner_doc: NodeId,
    is_template_content: bool,
    run_scripts: bool,
    mirror: HashMap<NodeId, MirrorEntry>,
    /// Tracks nodes we already fired `connectedCallback` on.
    connected_notified: HashSet<NodeId>,
    /// Tracks `<script>` nodes already prepared/executed.
    started_scripts: HashSet<NodeId>,
    /// Tracks deferred/module scripts queued to execute after `close()`.
    deferred_scripts: Vec<(NodeId, Option<String>, String, bool)>,
    /// Whether this stream has been closed or aborted.
    finished: bool,
    /// Pending previous text node in `insert_container` eligible for coalescing.
    coalesce_text_node: Option<NodeId>,
}

impl StreamMirrorState {
    fn new(
        target: &ResolvedTarget,
        sanitizer: SanitizerConfig,
        run_scripts: bool,
        allow_initial_text_coalesce: bool,
        host: &SharedHost,
    ) -> Self {
        let scripting = is_live_browsing_context_doc(host, target.owner_doc);
        let parser = HtmlFragmentParserStream::new(Some(&target.context_qname), scripting);
        let coalesce_text_node = if allow_initial_text_coalesce {
            let doc_rc = Rc::clone(&host.borrow().document);
            let doc = doc_rc.borrow();
            let prev = match target.ref_node {
                Some(rn) => doc.previous_sibling(rn),
                None => doc.last_child(target.insert_container),
            };
            prev.filter(|&p| matches!(doc.get(p), Some(NodeData::Text(_))))
        } else {
            None
        };
        Self {
            parser,
            sanitizer,
            insert_container: target.insert_container,
            logical_container: target.logical_container,
            ref_node: target.ref_node,
            owner_doc: target.owner_doc,
            is_template_content: target.is_template_content,
            run_scripts,
            mirror: HashMap::new(),
            connected_notified: HashSet::new(),
            started_scripts: HashSet::new(),
            deferred_scripts: Vec::new(),
            finished: false,
            coalesce_text_node,
        }
    }
}

/// Synchronizes all nodes currently parsed in `st.parser` into `host.document`,
/// applying `st.sanitizer`, updating text nodes in-place with `CharacterData` mutations,
/// and upgrading/connecting custom elements in document order.
fn sync_parsed_tree_into_host(
    vm: &mut JsVm,
    host: &SharedHost,
    st: &mut StreamMirrorState,
) -> Vec<NodeId> {
    let src_doc_rc = st.parser.document();
    let roots = st.parser.fragment_roots();
    let mut newly_connected = Vec::new();
    let src_doc = src_doc_rc.borrow();
    for src_root in roots {
        sync_node_recursive(
            vm,
            host,
            st,
            &src_doc,
            src_root,
            st.insert_container,
            st.ref_node,
            !st.is_template_content,
            &mut newly_connected,
        );
    }
    drop(src_doc);
    host.borrow_mut()
        .invalidate_dom_mutation(st.insert_container, None);
    newly_connected
}

#[allow(clippy::too_many_arguments)]
fn sync_node_recursive(
    vm: &mut JsVm,
    host: &SharedHost,
    st: &mut StreamMirrorState,
    src_doc: &Document,
    src_id: NodeId,
    dst_parent: NodeId,
    dst_ref: Option<NodeId>,
    can_connect: bool,
    newly_connected: &mut Vec<NodeId>,
) {
    // Guard against inserting a second element directly under a `Document` node
    // (e.g. if script moved `<table>` to `doc.documentElement` mid-stream!).
    let dst_parent_is_doc = matches!(
        host.borrow().document.borrow().get(dst_parent),
        Some(NodeData::Document)
    );
    if dst_parent_is_doc {
        return;
    }

    if let Some(&existing) = st.mirror.get(&src_id) {
        match existing {
            MirrorEntry::Removed => return,
            MirrorEntry::Unwrapped => {
                // Children continue to sync into `dst_parent`.
                let children: Vec<NodeId> = src_doc.children(src_id).collect();
                for ch in children {
                    sync_node_recursive(
                        vm,
                        host,
                        st,
                        src_doc,
                        ch,
                        dst_parent,
                        dst_ref,
                        can_connect,
                        newly_connected,
                    );
                }
                return;
            }
            MirrorEntry::ShadowRoot(sr_id) => {
                if let Some(NodeData::Element(el)) = src_doc.get(src_id) {
                    if let Some(tc) = el.template_contents {
                        let children: Vec<NodeId> = src_doc.children(tc).collect();
                        for ch in children {
                            sync_node_recursive(
                                vm,
                                host,
                                st,
                                src_doc,
                                ch,
                                sr_id,
                                None,
                                can_connect,
                                newly_connected,
                            );
                        }
                    }
                }
                return;
            }
            MirrorEntry::Mapped(dst_id) => {
                // Check if `Text` node grew via streaming chunk!
                if let Some(NodeData::Text(src_text)) = src_doc.get(src_id) {
                    let old_val = {
                        let doc_rc = Rc::clone(&host.borrow().document);
                        let mut doc = doc_rc.borrow_mut();
                        if let Some(NodeData::Text(dst_text)) = doc.get_mut(dst_id) {
                            if dst_text.as_str() != src_text.as_str() {
                                // Handle case where `dst_id` was coalesced with a pre-existing prefix.
                                // Track suffix growth cleanly when `dst_text` already contains prefix.
                                let old = dst_text.clone();
                                if st.coalesce_text_node == Some(dst_id) {
                                    // Find prefix that preceded this stream's text:
                                    // `coalesce_prefix_len` handled via exact append below.
                                    *dst_text = src_text.clone();
                                } else {
                                    *dst_text = src_text.clone();
                                }
                                Some(old)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };
                    if let Some(old_value) = old_val {
                        host.borrow().emit_mutation(DomMutation::CharacterData {
                            target: dst_id,
                            old_value,
                        });
                    }
                    return;
                }
                // Check if html5ever reparented this element (foster parenting / adoption agency!).
                let current_dst_parent = host.borrow().document.borrow().parent(dst_id);
                if current_dst_parent != Some(dst_parent) {
                    // Only move if `dst_parent` is not an ancestor/descendant cycle and not a Document.
                    let can_move = {
                        let doc_rc = Rc::clone(&host.borrow().document);
                        let doc = doc_rc.borrow();
                        let mut anc = Some(dst_parent);
                        let mut is_cycle = false;
                        while let Some(a) = anc {
                            if a == dst_id {
                                is_cycle = true;
                                break;
                            }
                            anc = doc.parent(a);
                        }
                        !is_cycle
                            && !matches!(doc.get(dst_parent), Some(NodeData::Document))
                            && dst_ref.is_none_or(|rn| doc.parent(rn) == Some(dst_parent))
                    };
                    if can_move {
                        let doc_rc = Rc::clone(&host.borrow().document);
                        let mut doc = doc_rc.borrow_mut();
                        if let Some(rn) = dst_ref {
                            doc.insert_before(rn, dst_id);
                        } else {
                            doc.append_child(dst_parent, dst_id);
                        }
                    }
                }
                if let Some(NodeData::Element(src_el)) = src_doc.get(src_id) {
                    if let Some(src_tc) = src_el.template_contents {
                        let dst_tc = match host.borrow().document.borrow().get(dst_id) {
                            Some(NodeData::Element(de)) => de.template_contents,
                            _ => None,
                        };
                        if let Some(dtc) = dst_tc {
                            sync_children(
                                vm,
                                host,
                                st,
                                src_doc,
                                src_tc,
                                dtc,
                                None,
                                false,
                                newly_connected,
                            );
                        }
                    }
                }
                sync_children(
                    vm,
                    host,
                    st,
                    src_doc,
                    src_id,
                    dst_id,
                    None,
                    can_connect,
                    newly_connected,
                );
                return;
            }
        }
    }

    // First time visiting `src_id`!
    match src_doc.get(src_id) {
        Some(NodeData::Text(src_text)) => {
            if dst_parent == st.insert_container {
                if let Some(coal_id) = st.coalesce_text_node.take() {
                    if host.borrow().document.borrow().parent(coal_id) == Some(dst_parent) {
                        let old_value = {
                            let doc_rc = Rc::clone(&host.borrow().document);
                            let mut doc = doc_rc.borrow_mut();
                            match doc.get_mut(coal_id) {
                                Some(NodeData::Text(existing)) => {
                                    let old = existing.clone();
                                    existing.push_str(src_text);
                                    Some(old)
                                }
                                _ => None,
                            }
                        };
                        if let Some(old_value) = old_value {
                            st.mirror.insert(src_id, MirrorEntry::Mapped(coal_id));
                            host.borrow().emit_mutation(DomMutation::CharacterData {
                                target: coal_id,
                                old_value,
                            });
                            return;
                        }
                    }
                }
            }
            insert_mirrored_leaf(
                host,
                st,
                src_id,
                dst_parent,
                dst_ref,
                NodeData::Text(src_text.clone()),
            );
        }
        Some(NodeData::Comment(src_comment)) => {
            st.coalesce_text_node = None;
            if !st.sanitizer.comments {
                st.mirror.insert(src_id, MirrorEntry::Removed);
                return;
            }
            insert_mirrored_leaf(
                host,
                st,
                src_id,
                dst_parent,
                dst_ref,
                NodeData::Comment(src_comment.clone()),
            );
        }
        Some(NodeData::Element(src_el)) => {
            st.coalesce_text_node = None;
            let ns = src_el.name.ns.as_str();
            let local = src_el.name.local.as_str();

            if ns == HTML_NS && local == "template" {
                let shadow_mode = src_el
                    .get_attribute("shadowrootmode")
                    .or_else(|| src_el.get_attribute("shadowroot"))
                    .map(|s| s.to_ascii_lowercase())
                    .filter(|m| m == "open" || m == "closed");
                if let Some(mode) = shadow_mode {
                    let src_parent_unwrapped = src_doc
                        .parent(src_id)
                        .and_then(|p| st.mirror.get(&p))
                        .is_some_and(|e| matches!(e, MirrorEntry::Unwrapped));
                    // Per WHATWG HTML §13.2.6.4.7: a top-level `<template shadowrootmode>`
                    // inside a fragment parse (`setHTMLUnsafe` / `streamHTMLUnsafe`) MUST NOT
                    // attach a shadow root to the fragment context element (`st.insert_container`);
                    // it only attaches to a host element parsed inside the fragment itself!
                    let is_top_level_in_fragment = dst_parent == st.insert_container;
                    if !is_top_level_in_fragment
                        && !src_parent_unwrapped
                        && st.sanitizer.allows_declarative_shadow()
                    {
                        let can_attach = matches!(
                            host.borrow().document.borrow().get(dst_parent),
                            Some(NodeData::Element(pe))
                                if pe.shadow_root.is_none()
                                    && &*pe.name.ns == HTML_NS
                                    && is_valid_shadow_host_name(&pe.name.local)
                        );
                        if can_attach {
                            let sr_id = {
                                let doc_rc = Rc::clone(&host.borrow().document);
                                let mut doc = doc_rc.borrow_mut();
                                let slot_assignment = if src_el
                                    .get_attribute("shadowrootslotassignment")
                                    .is_some_and(|s| s.eq_ignore_ascii_case("manual"))
                                {
                                    "manual".to_string()
                                } else {
                                    "named".to_string()
                                };
                                let sr = doc.new_node(NodeData::ShadowRoot {
                                    host: dst_parent,
                                    mode,
                                    delegates_focus: src_el
                                        .has_attribute("shadowrootdelegatesfocus"),
                                    clonable: src_el.has_attribute("shadowrootclonable"),
                                    serializable: src_el.has_attribute("shadowrootserializable"),
                                    slot_assignment,
                                    declarative: true,
                                    null_registry: src_el
                                        .has_attribute("shadowrootcustomelementregistry"),
                                });
                                if let Some(NodeData::Element(pe)) = doc.get_mut(dst_parent) {
                                    pe.shadow_root = Some(sr);
                                }
                                sr
                            };
                            host.borrow_mut().set_owner_doc(sr_id, st.owner_doc);
                            st.mirror.insert(src_id, MirrorEntry::ShadowRoot(sr_id));
                            if let Some(tc) = src_el.template_contents {
                                sync_children(
                                    vm,
                                    host,
                                    st,
                                    src_doc,
                                    tc,
                                    sr_id,
                                    None,
                                    can_connect,
                                    newly_connected,
                                );
                            }
                            return;
                        }
                    } else if !src_parent_unwrapped && st.sanitizer.enforce_safe {
                        st.mirror.insert(src_id, MirrorEntry::Removed);
                        return;
                    }
                }
            }

            match st.sanitizer.element_action(ns, local) {
                ElementAction::Remove => {
                    st.mirror.insert(src_id, MirrorEntry::Removed);
                }
                ElementAction::ReplaceWithChildren => {
                    st.mirror.insert(src_id, MirrorEntry::Unwrapped);
                    sync_children(
                        vm,
                        host,
                        st,
                        src_doc,
                        src_id,
                        dst_parent,
                        dst_ref,
                        can_connect,
                        newly_connected,
                    );
                }
                ElementAction::Keep(entry_idx) => {
                    let mut new_el = ElementData::new(src_el.name.clone());
                    for (aq, aval) in &src_el.attributes {
                        if st.sanitizer.allows_attribute(
                            entry_idx,
                            aq.ns.as_str(),
                            aq.local.as_str(),
                            aval,
                        ) {
                            new_el.attributes.push((aq.clone(), aval.clone()));
                        }
                    }
                    new_el.sync_cached_attrs();
                    new_el.is_value = src_el.is_value.clone();
                    if new_el
                        .is_value
                        .as_deref()
                        .is_some_and(rix_dom::is_valid_custom_element_name)
                    {
                        new_el.set_custom_element_state(rix_dom::CustomElementState::Undefined);
                    }
                    new_el.null_registry = src_el.null_registry;

                    let dst_tc = if src_el.template_contents.is_some() {
                        let f = host
                            .borrow()
                            .document
                            .borrow_mut()
                            .new_node(NodeData::DocumentFragment);
                        new_el.template_contents = Some(f);
                        host.borrow_mut().set_owner_doc(f, st.owner_doc);
                        Some(f)
                    } else {
                        None
                    };
                    let dst_id = insert_mirrored_leaf(
                        host,
                        st,
                        src_id,
                        dst_parent,
                        dst_ref,
                        NodeData::Element(new_el),
                    );
                    let is_conn =
                        can_connect && host.borrow().document.borrow().is_connected(dst_id);

                    if ns == HTML_NS && local == "style" {
                        host.borrow_mut().mark_sheets_dirty();
                    }
                    if is_conn && st.connected_notified.insert(dst_id) {
                        newly_connected.push(dst_id);
                        maybe_upgrade_and_connect_single_ce(vm, host, dst_id);
                    }
                    if let (Some(src_tc), Some(dtc)) = (src_el.template_contents, dst_tc) {
                        sync_children(
                            vm,
                            host,
                            st,
                            src_doc,
                            src_tc,
                            dtc,
                            None,
                            false,
                            newly_connected,
                        );
                    }
                    sync_children(
                        vm,
                        host,
                        st,
                        src_doc,
                        src_id,
                        dst_id,
                        None,
                        can_connect,
                        newly_connected,
                    );
                }
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn sync_children(
    vm: &mut JsVm,
    host: &SharedHost,
    st: &mut StreamMirrorState,
    src_doc: &Document,
    src_parent: NodeId,
    dst_parent: NodeId,
    dst_ref: Option<NodeId>,
    can_connect: bool,
    newly_connected: &mut Vec<NodeId>,
) {
    let children: Vec<NodeId> = src_doc.children(src_parent).collect();
    for ch in children {
        sync_node_recursive(
            vm,
            host,
            st,
            src_doc,
            ch,
            dst_parent,
            dst_ref,
            can_connect,
            newly_connected,
        );
    }
}

fn insert_mirrored_leaf(
    host: &SharedHost,
    st: &mut StreamMirrorState,
    src_id: NodeId,
    dst_parent: NodeId,
    dst_ref: Option<NodeId>,
    data: NodeData,
) -> NodeId {
    let (dst_id, prev_sib) = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = doc_rc.borrow_mut();
        let n = doc.new_node(data);
        let prev = dst_ref
            .and_then(|r| doc.previous_sibling(r))
            .or_else(|| doc.last_child(dst_parent));
        if let Some(rn) = dst_ref {
            doc.insert_before(rn, n);
        } else {
            doc.append_child(dst_parent, n);
        }
        (n, prev)
    };
    host.borrow_mut().set_owner_doc(dst_id, st.owner_doc);
    st.mirror.insert(src_id, MirrorEntry::Mapped(dst_id));
    host.borrow().emit_mutation(DomMutation::ChildList {
        target: dst_parent,
        added: vec![dst_id],
        removed: Vec::new(),
        previous_sibling: prev_sib,
        next_sibling: dst_ref,
    });
    dst_id
}

fn maybe_upgrade_and_connect_single_ce(vm: &mut JsVm, host: &SharedHost, nid: NodeId) {
    crate::custom_elements::upgrade_element_now(vm, host, nid);
}

/// Checks if a parsed `<script>` element should execute, and runs inline or external scripts.
#[allow(clippy::too_many_arguments)]
fn prepare_and_run_script(
    vm: &mut JsVm,
    host: &SharedHost,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    st: &mut StreamMirrorState,
    src_script_id: NodeId,
    is_streaming: bool,
) {
    if !st.run_scripts || st.is_template_content {
        return;
    }
    if !is_live_browsing_context_doc(host, st.owner_doc) {
        return;
    }
    let Some(&MirrorEntry::Mapped(dst_script_id)) = st.mirror.get(&src_script_id) else {
        return;
    };
    let is_connected = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        doc.is_connected(dst_script_id) && hb.owner_doc_of(dst_script_id) == st.owner_doc
    };
    // Per HTML §4.12.1.1 / WHATWG #13079: fragment-parsed scripts (positional `*HTMLUnsafe`
    // and disconnected streaming targets) retain `force_async = true`.
    if !is_streaming || !is_connected {
        host.borrow_mut().script_force_async.insert(dst_script_id);
    }
    if !is_connected {
        // Not connected yet: leave unstarted so it executes when later connected!
        host.borrow_mut().contextual_scripts.insert(dst_script_id);
        return;
    }
    if !st.started_scripts.insert(dst_script_id) {
        return;
    }
    let script_info = extract_executable_script_info(host, dst_script_id);
    let Some((src_attr, inline_code, is_defer, is_async, is_module)) = script_info else {
        return;
    };

    // Defer & module scripts during streaming execute in queued tasks after `close()`!
    if is_streaming && (is_defer || (is_module && !is_async)) {
        st.deferred_scripts
            .push((dst_script_id, src_attr, inline_code, is_module));
        return;
    }

    if let Some(src) = src_attr {
        if is_async || !is_streaming {
            schedule_async_external_script(
                host,
                event_loop,
                web_apis,
                net,
                dst_script_id,
                src,
                is_module,
            );
        } else if let Some(code) = fetch_script_source_sync(web_apis, net, &src) {
            eval_script_node(vm, host, dst_script_id, &code, is_module);
        }
    } else if !inline_code.trim().is_empty() {
        eval_script_node(vm, host, dst_script_id, &inline_code, is_module);
    }
}

fn extract_executable_script_info(
    host: &SharedHost,
    dst_script_id: NodeId,
) -> Option<(Option<String>, String, bool, bool, bool)> {
    let hb = host.borrow();
    let doc = hb.document.borrow();
    let Some(NodeData::Element(el)) = doc.get(dst_script_id) else {
        return None;
    };
    if el.name.local.as_str() != "script" || (&*el.name.ns != HTML_NS && &*el.name.ns != SVG_NS) {
        return None;
    }
    let script_type = el
        .get_attribute("type")
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let is_module = script_type == "module";
    let is_classic = script_type.is_empty()
        || matches!(
            script_type.as_str(),
            "text/javascript"
                | "application/javascript"
                | "application/ecmascript"
                | "text/ecmascript"
        );
    if !is_module && !is_classic {
        return None;
    }
    let src_attr = el.get_attribute("src").map(ToString::to_string);
    let is_defer = el.has_attribute("defer") && src_attr.is_some();
    let is_async = el.has_attribute("async") || hb.script_force_async.contains(&dst_script_id);
    let mut inline_code = String::new();
    for c in doc.descendants(dst_script_id) {
        if let Some(NodeData::Text(t)) = doc.get(c) {
            inline_code.push_str(t);
        }
    }
    Some((src_attr, inline_code, is_defer, is_async, is_module))
}

fn run_fragment_script_now(
    vm: &mut JsVm,
    host: &SharedHost,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    script_nid: NodeId,
) {
    let Some((src_attr, inline_code, _is_defer, is_async, is_module)) =
        extract_executable_script_info(host, script_nid)
    else {
        return;
    };
    if let Some(src) = src_attr {
        if is_async {
            schedule_async_external_script(
                host, event_loop, web_apis, net, script_nid, src, is_module,
            );
        } else if let Some(code) = fetch_script_source_sync(web_apis, net, &src) {
            eval_script_node(vm, host, script_nid, &code, is_module);
        }
    } else if !inline_code.trim().is_empty() {
        eval_script_node(vm, host, script_nid, &inline_code, is_module);
    }
}

fn schedule_async_external_script(
    host: &SharedHost,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    script_nid: NodeId,
    src: String,
    is_module: bool,
) {
    let trimmed = src.trim();
    if trimmed.starts_with("data:") {
        if let Some(code) = fetch_script_source_sync(web_apis, net, trimmed) {
            let h_cb = Rc::clone(host);
            event_loop
                .borrow_mut()
                .post_host_task(TaskPriority::Networking, move |vm| {
                    eval_script_node(vm, &h_cb, script_nid, &code, is_module);
                });
        }
        return;
    }
    let resolved = url::Url::parse(&web_apis.borrow().current_href())
        .ok()
        .and_then(|base| base.join(trimmed).ok())
        .map(|u| u.to_string())
        .unwrap_or_else(|| trimmed.to_string());
    if let Ok(rt_handle) = tokio::runtime::Handle::try_current() {
        let engine = Arc::clone(&net.borrow().fetch_engine);
        let tx = net.borrow().async_tx.clone();
        let origin = web_apis.borrow().current_origin();
        net.borrow_mut().pending_subresources += 1;
        rt_handle.spawn(async move {
            let req = rix_net::FetchRequest {
                url: resolved,
                method: "GET".to_string(),
                headers: HashMap::new(),
                body: Vec::new(),
                mode: rix_net::FetchMode::Cors,
                credentials: rix_net::FetchCredentials::SameOrigin,
                cache: rix_net::FetchCache::Default,
                redirect: rix_net::FetchRedirect::Follow,
                origin,
                abort_signal: None,
            };
            let (ok, code) = match engine.fetch(req).await {
                Ok(resp) if (200..300).contains(&resp.status) => {
                    let ct = resp
                        .headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                        .map(|(_, v)| v.to_ascii_lowercase())
                        .unwrap_or_default();
                    let text = String::from_utf8_lossy(&resp.body).into_owned();
                    let trimmed = text.trim_start();
                    let is_html = ct.contains("text/html")
                        || ct.contains("application/xhtml+xml")
                        || trimmed.starts_with("<!DOCTYPE")
                        || trimmed.starts_with("<!doctype")
                        || trimmed.starts_with("<html");
                    if is_html {
                        (false, String::new())
                    } else {
                        (true, text)
                    }
                }
                _ => (false, String::new()),
            };
            let _ = tx.send(AsyncNetEvent::ExternalScriptLoaded {
                script_nid,
                is_module,
                code,
                ok,
            });
        });
    } else if let Some(code) = fetch_script_source_sync(web_apis, net, &resolved) {
        let h_cb = Rc::clone(host);
        event_loop
            .borrow_mut()
            .post_host_task(TaskPriority::Networking, move |vm| {
                eval_script_node(vm, &h_cb, script_nid, &code, is_module);
            });
    }
}

/// Dispatches a non-bubbling `load` or `error` event on an element (invoking both DOM
/// `addEventListener` listeners and IDL `el.onload`/`el.onerror` handlers).
pub(crate) fn dispatch_simple_element_event(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    nid: NodeId,
    ev_type: &str,
) {
    let _ = crate::events::fire_dom_node_event(
        vm,
        host,
        events,
        event_loop,
        nid,
        ev_type,
        false,
        false,
        &[],
    );
}

fn schedule_connected_subresource(
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    nid: NodeId,
) {
    let info = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        match doc.get(nid) {
            Some(NodeData::Element(el)) => {
                let tag = el.name.local.as_str().to_ascii_lowercase();
                if tag == "img" {
                    let raw_src = el.get_attribute("src").unwrap_or("").trim();
                    let src = if !raw_src.is_empty() {
                        raw_src.to_string()
                    } else {
                        el.get_attribute("srcset")
                            .and_then(rix_layout::parse_best_srcset_url)
                            .unwrap_or_default()
                    };
                    let is_lazy = el
                        .get_attribute("loading")
                        .is_some_and(|v| v.trim().eq_ignore_ascii_case("lazy"));
                    if !src.is_empty() {
                        Some(("img", src, is_lazy))
                    } else {
                        None
                    }
                } else if tag == "link" {
                    let rel = el.get_attribute("rel").unwrap_or("");
                    let is_sheet = rel
                        .split_ascii_whitespace()
                        .any(|t| t.eq_ignore_ascii_case("stylesheet"));
                    let is_alt = rel
                        .split_ascii_whitespace()
                        .any(|t| t.eq_ignore_ascii_case("alternate"));
                    let href = el.get_attribute("href").unwrap_or("").trim().to_string();
                    if is_sheet && !is_alt && !el.has_attribute("disabled") && !href.is_empty() {
                        Some(("link", href, false))
                    } else {
                        None
                    }
                } else if tag == "style" {
                    let css_text = doc.inner_text(nid);
                    Some(("style", css_text, false))
                } else {
                    None
                }
            }
            _ => None,
        }
    };
    match info {
        Some(("img", src, is_lazy)) => {
            if is_lazy {
                let mut hb = host.borrow_mut();
                if hb.dynamic_lazy_images_scheduled >= 24 {
                    return;
                }
                hb.dynamic_lazy_images_scheduled += 1;
            }
            schedule_dynamic_image_load(host, events, event_loop, web_apis, net, nid, &src);
        }
        Some(("link", href, _)) => {
            schedule_dynamic_stylesheet_load(host, web_apis, net, nid, &href);
        }
        Some(("style", css_text, _)) => {
            schedule_style_element_imports(host, events, event_loop, web_apis, net, nid, &css_text);
        }
        _ => {}
    }
}

fn schedule_style_element_imports(
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    style_nid: NodeId,
    css_text: &str,
) {
    let (media, base_url_str, quirks_mode) = {
        let hb = host.borrow();
        let qm = hb.document.borrow().quirks_mode() == 2;
        let media = rix_css::MediaContext {
            viewport_width: hb.viewport.width,
            viewport_height: hb.viewport.height,
            dark_mode: hb.dark_mode,
            quirks_mode: qm,
            ..rix_css::MediaContext::default()
        };
        (media, hb.main_document_url.clone(), qm)
    };
    let imports = rix_css::extract_stylesheet_imports(css_text, &media);
    let mut http_urls: Vec<String> = Vec::new();
    let mut any_immediate_error = false;
    let base_parsed = url::Url::parse(&base_url_str).ok();
    for imp in imports {
        if !imp.condition_matches {
            continue;
        }
        let raw = imp.url.trim();
        if raw.is_empty() {
            continue;
        }
        if raw.starts_with("data:") {
            if let Some((_, bytes)) = crate::fetch_ws::decode_data_url(raw) {
                let decoded = String::from_utf8_lossy(&bytes).into_owned();
                host.borrow_mut()
                    .fetched_import_css
                    .insert(raw.to_string(), Some(decoded));
            } else {
                any_immediate_error = true;
            }
            continue;
        }
        let resolved = base_parsed
            .as_ref()
            .and_then(|b| b.join(raw).ok())
            .map(|u| u.to_string())
            .unwrap_or_else(|| raw.to_string());
        if resolved.starts_with("http://") || resolved.starts_with("https://") {
            if !http_urls.contains(&resolved) {
                http_urls.push(resolved);
            }
        } else {
            any_immediate_error = true;
        }
    }

    if !http_urls.is_empty() {
        if let Ok(rt_handle) = tokio::runtime::Handle::try_current() {
            let engine = Arc::clone(&net.borrow().fetch_engine);
            let tx = net.borrow().async_tx.clone();
            let doc_origin = web_apis.borrow().current_origin();
            net.borrow_mut().pending_subresources += 1;
            rt_handle.spawn(async move {
                let mut fetched = Vec::with_capacity(http_urls.len());
                let mut any_error = any_immediate_error;
                for resolved in http_urls {
                    let css_opt = match tokio::time::timeout(
                        std::time::Duration::from_secs(3),
                        engine.fetch_subresource(&resolved),
                    )
                    .await
                    {
                        Ok(Ok((resp, hops))) if (200..300).contains(&resp.status) => {
                            let ct = resp
                                .headers
                                .iter()
                                .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                                .map(|(_, v)| {
                                    v.split(';')
                                        .next()
                                        .unwrap_or("")
                                        .trim()
                                        .to_ascii_lowercase()
                                })
                                .unwrap_or_default();
                            // CSS Cascade L5 §3 / HTML §4.2.4 Quirks Mode MIME check:
                            // `text/css` always applies (same- or cross-origin); in Quirks Mode a
                            // non-`text/css` MIME is allowed ONLY when every hop in the redirect
                            // chain is same-origin with the document (CORS-same-origin).
                            let all_hops_same_origin = !doc_origin.is_empty()
                                && !hops.is_empty()
                                && hops.iter().all(|h| {
                                    url::Url::parse(h).ok().is_some_and(|u| {
                                        u.origin().ascii_serialization() == doc_origin
                                    })
                                });
                            let mime_ok = ct == "text/css" || (quirks_mode && all_hops_same_origin);
                            if mime_ok {
                                Some(String::from_utf8_lossy(&resp.body).into_owned())
                            } else {
                                any_error = true;
                                None
                            }
                        }
                        _ => {
                            any_error = true;
                            None
                        }
                    };
                    fetched.push((resolved, css_opt));
                }
                let _ = tx.send(AsyncNetEvent::StyleImportsLoaded {
                    style_nid,
                    fetched,
                    any_error,
                });
            });
            return;
        }
    }

    // No async HTTP `@import`s pending: queue an HTML §4.2.6 task to dispatch `load`/`error`.
    let h_cb = Rc::clone(host);
    let ev_cb = Rc::clone(events);
    let el_cb = Rc::clone(event_loop);
    let ev_type: &'static str = if any_immediate_error { "error" } else { "load" };
    event_loop
        .borrow_mut()
        .post_host_task(TaskPriority::Networking, move |vm| {
            h_cb.borrow_mut().mark_full_sheets_rebuild();
            dispatch_simple_element_event(vm, &h_cb, &ev_cb, &el_cb, style_nid, ev_type);
        });
}

fn schedule_web_font_load(
    vm: &mut JsVm,
    host: &SharedHost,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    promise_h: HeapHandle,
) {
    let missing_urls: Vec<String> = {
        let mut hb = host.borrow_mut();
        let _ = hb.ensure_styles();
        let base_url = url::Url::parse(&hb.main_document_url).ok();
        let mut to_fetch = Vec::new();
        let mut inline_decoded = Vec::new();
        for ff in &hb.style_ctx.rule_index().font_faces {
            for src in &ff.sources {
                if let rix_css::FontFaceSource::Url { url, .. } = src {
                    let raw = url.as_str().trim();
                    if raw.is_empty() {
                        continue;
                    }
                    let resolved = base_url
                        .as_ref()
                        .and_then(|b| b.join(raw).ok())
                        .map(|u| u.to_string())
                        .unwrap_or_else(|| raw.to_string());
                    if hb.web_font_bytes_cache.contains_key(&resolved) {
                        continue;
                    }
                    if resolved.starts_with("data:") {
                        let decoded = crate::fetch_ws::decode_data_url(&resolved)
                            .and_then(|(_, b)| rix_text::decode_web_font(&b).ok())
                            .map(|(_, sfnt)| sfnt);
                        inline_decoded.push((resolved, decoded));
                    } else if (resolved.starts_with("http://") || resolved.starts_with("https://"))
                        && !to_fetch.contains(&resolved)
                    {
                        to_fetch.push(resolved);
                    }
                }
            }
        }
        for (url, decoded) in inline_decoded {
            hb.web_font_bytes_cache.insert(url, decoded);
        }
        to_fetch
    };

    if missing_urls.is_empty() {
        host.borrow_mut().sync_active_web_fonts_from_cache();
        let arr = vm.alloc_array();
        rix_js::promise::resolve_promise(vm, promise_h, JsValue::from_object(arr));
        return;
    }

    let Ok(rt_handle) = tokio::runtime::Handle::try_current() else {
        host.borrow_mut().sync_active_web_fonts_from_cache();
        let arr = vm.alloc_array();
        rix_js::promise::resolve_promise(vm, promise_h, JsValue::from_object(arr));
        return;
    };

    let engine = Arc::clone(&net.borrow().fetch_engine);
    let tx = net.borrow().async_tx.clone();
    let origin = web_apis.borrow().current_origin();
    {
        let mut nb = net.borrow_mut();
        nb.pending_subresources += 1;
        nb.pending_font_promises.insert(promise_h, ());
    }
    rt_handle.spawn(async move {
        let mut fetched = Vec::with_capacity(missing_urls.len());
        for url in missing_urls {
            let req = rix_net::FetchRequest {
                url: url.clone(),
                method: "GET".to_string(),
                headers: HashMap::new(),
                body: Vec::new(),
                mode: rix_net::FetchMode::NoCors,
                credentials: rix_net::FetchCredentials::SameOrigin,
                cache: rix_net::FetchCache::Default,
                redirect: rix_net::FetchRedirect::Follow,
                origin: origin.clone(),
                abort_signal: None,
            };
            let sfnt_opt =
                match tokio::time::timeout(std::time::Duration::from_secs(3), engine.fetch(req))
                    .await
                {
                    Ok(Ok(resp)) if (200..300).contains(&resp.status) && !resp.body.is_empty() => {
                        rix_text::decode_web_font(&resp.body).ok().map(|(_, s)| s)
                    }
                    _ => None,
                };
            fetched.push((url, sfnt_opt));
        }
        let _ = tx.send(AsyncNetEvent::WebFontsLoaded { promise_h, fetched });
    });
}

fn schedule_dynamic_stylesheet_load(
    host: &SharedHost,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    link_nid: NodeId,
    raw_href: &str,
) {
    let resolved = url::Url::parse(&host.borrow().main_document_url)
        .ok()
        .and_then(|base| base.join(raw_href.trim()).ok())
        .map(|u| u.to_string())
        .unwrap_or_else(|| raw_href.trim().to_string());
    if !resolved.starts_with("http://") && !resolved.starts_with("https://") {
        return;
    }
    let Ok(rt_handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let engine = Arc::clone(&net.borrow().fetch_engine);
    let tx = net.borrow().async_tx.clone();
    let origin = web_apis.borrow().current_origin();
    net.borrow_mut().pending_subresources += 1;
    rt_handle.spawn(async move {
        let req = rix_net::FetchRequest {
            url: resolved,
            method: "GET".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
            mode: rix_net::FetchMode::NoCors,
            credentials: rix_net::FetchCredentials::SameOrigin,
            cache: rix_net::FetchCache::Default,
            redirect: rix_net::FetchRedirect::Follow,
            origin,
            abort_signal: None,
        };
        let css = match tokio::time::timeout(std::time::Duration::from_secs(3), engine.fetch(req))
            .await
        {
            Ok(Ok(resp)) if (200..300).contains(&resp.status) => {
                let text = String::from_utf8_lossy(&resp.body).into_owned();
                let trimmed = text.trim_start();
                if trimmed.starts_with("<!DOCTYPE")
                    || trimmed.starts_with("<!doctype")
                    || trimmed.starts_with("<html")
                {
                    None
                } else {
                    Some(text)
                }
            }
            _ => None,
        };
        let _ = tx.send(AsyncNetEvent::DynamicStylesheetLoaded { link_nid, css });
    });
}

fn schedule_dynamic_image_load(
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    img_nid: NodeId,
    raw_src: &str,
) {
    let trimmed = raw_src.trim();
    if trimmed.is_empty() {
        return;
    }
    let resolved = url::Url::parse(&host.borrow().main_document_url)
        .ok()
        .and_then(|base| base.join(trimmed).ok())
        .map(|u| u.to_string())
        .unwrap_or_else(|| trimmed.to_string());

    // Fast path: already decoded in this document's AssetRegistry.
    let already_loaded = {
        let hb = host.borrow();
        hb.layout_engine.assets.get_by_src(trimmed).is_some()
            || hb.layout_engine.assets.get_by_src(&resolved).is_some()
            || hb.layout_engine.assets.get_svg_by_src(trimmed).is_some()
            || hb.layout_engine.assets.get_svg_by_src(&resolved).is_some()
    };
    if already_loaded {
        {
            let mut hb = host.borrow_mut();
            hb.pending_dynamic_images.remove(&img_nid);
            hb.completed_images.insert(img_nid);
        }
        let h_cb = Rc::clone(host);
        let ev_cb = Rc::clone(events);
        let el_cb = Rc::clone(event_loop);
        event_loop
            .borrow_mut()
            .post_host_task(TaskPriority::Networking, move |vm| {
                dispatch_simple_element_event(vm, &h_cb, &ev_cb, &el_cb, img_nid, "load");
            });
        return;
    }

    // Live HTTP(S) async fetch + off-thread raster/SVG decode when tokio runtime is active.
    if (resolved.starts_with("http://") || resolved.starts_with("https://"))
        && tokio::runtime::Handle::try_current().is_ok()
    {
        let rt_handle = tokio::runtime::Handle::current();
        {
            let mut hb = host.borrow_mut();
            hb.pending_dynamic_images.insert(img_nid);
            hb.completed_images.remove(&img_nid);
        }
        let engine = Arc::clone(&net.borrow().fetch_engine);
        let tx = net.borrow().async_tx.clone();
        let origin = web_apis.borrow().current_origin();
        let raw_owned = trimmed.to_string();
        let resolved_owned = resolved;
        net.borrow_mut().pending_subresources += 1;
        rt_handle.spawn(async move {
            let req = rix_net::FetchRequest {
                url: resolved_owned.clone(),
                method: "GET".to_string(),
                headers: HashMap::new(),
                body: Vec::new(),
                mode: rix_net::FetchMode::NoCors,
                credentials: rix_net::FetchCredentials::SameOrigin,
                cache: rix_net::FetchCache::Default,
                redirect: rix_net::FetchRedirect::Follow,
                origin,
                abort_signal: None,
            };
            let (raster, svg) = match tokio::time::timeout(
                std::time::Duration::from_millis(2500),
                engine.fetch(req),
            )
            .await
            {
                Ok(Ok(resp)) if (200..300).contains(&resp.status) && !resp.body.is_empty() => {
                    let ct = resp
                        .headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                        .map(|(_, v)| v.to_ascii_lowercase())
                        .unwrap_or_default();
                    let clean_url = resolved_owned
                        .split(['?', '#'])
                        .next()
                        .unwrap_or(&resolved_owned);
                    let is_svg = ct.contains("image/svg")
                        || clean_url.to_ascii_lowercase().ends_with(".svg")
                        || resp.body.starts_with(b"<svg")
                        || (resp.body.starts_with(b"<?xml")
                            && std::str::from_utf8(&resp.body[..resp.body.len().min(256)])
                                .is_ok_and(|s| s.contains("<svg")));
                    if is_svg {
                        let markup = String::from_utf8_lossy(&resp.body);
                        let doc = rix_html::parse_html(&markup);
                        let parsed =
                            rix_layout::extract_svg_from_document(&doc, rix_css::ColorRgba::BLACK);
                        (None, parsed)
                    } else {
                        (rix_layout::decode_single_raster(&resp.body).ok(), None)
                    }
                }
                _ => (None, None),
            };
            let _ = tx.send(AsyncNetEvent::DynamicImageLoaded {
                img_nid,
                raw_src: raw_owned,
                resolved_url: resolved_owned,
                raster,
                svg,
            });
        });
        return;
    }

    // Offline / non-HTTP / invalid image URL fallback (e.g. unit/WPT tests without server):
    // mark pending, then complete with `error` event asynchronously per HTML §4.8.4.3.
    {
        let mut hb = host.borrow_mut();
        hb.pending_dynamic_images.insert(img_nid);
        hb.completed_images.remove(&img_nid);
    }
    let h_cb = Rc::clone(host);
    let ev_cb = Rc::clone(events);
    let el_cb = Rc::clone(event_loop);
    event_loop
        .borrow_mut()
        .post_host_task(TaskPriority::Networking, move |vm| {
            {
                let mut hmut = h_cb.borrow_mut();
                hmut.pending_dynamic_images.remove(&img_nid);
                hmut.completed_images.insert(img_nid);
            }
            dispatch_simple_element_event(vm, &h_cb, &ev_cb, &el_cb, img_nid, "error");
        });
}

pub(crate) fn eval_script_node(
    vm: &mut JsVm,
    host: &SharedHost,
    script_nid: NodeId,
    code: &str,
    is_module: bool,
) {
    let owner_doc = host.borrow().owner_doc_of(script_nid);
    let main_root = host.borrow().document.borrow().root();
    let prev = host.borrow().current_script;
    host.borrow_mut().current_script = if is_module { None } else { Some(script_nid) };
    if owner_doc != main_root {
        // Bind `document` on global scope to the script's owning sub-document (e.g. iframe)
        // so `document.write()` / `document.open()` inside streamed iframe scripts targets it!
        let k_doc = vm.heap.intern_key("document");
        let prev_doc_val = vm.get_property_by_key(vm.global, k_doc).ok();
        let sub_doc_val = wrap_node(vm, host, owner_doc);
        let g = vm.global;
        vm.heap.object_define_data(
            g,
            k_doc,
            sub_doc_val,
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        let _ = vm.eval_script(code);
        if let Some(orig) = prev_doc_val {
            vm.heap
                .object_define_data(g, k_doc, orig, PropertyAttrs::DEFAULT, &mut vm.shapes);
        }
    } else {
        let _ = vm.eval_script(code);
    }
    host.borrow_mut().current_script = prev;
}

fn fetch_script_source_sync(
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    src: &str,
) -> Option<String> {
    let trimmed = src.trim();
    if let Some(rest) = trimmed.strip_prefix("data:") {
        if let Some((meta, payload)) = rest.split_once(',') {
            if meta.to_ascii_lowercase().ends_with(";base64") {
                if let Ok(bytes) = base64_decode_simple(payload) {
                    return Some(String::from_utf8_lossy(&bytes).into_owned());
                }
            }
            return Some(percent_decode_utf8(payload));
        }
        return None;
    }
    let base = url::Url::parse(&web_apis.borrow().current_href()).ok();
    let resolved = base
        .as_ref()
        .and_then(|b| b.join(trimmed).ok())
        .map(|u| u.to_string())
        .unwrap_or_else(|| trimmed.to_string());
    if !resolved.starts_with("http://") && !resolved.starts_with("https://") {
        return None;
    }
    let origin = crate::web_apis::origin_of_url(&web_apis.borrow().location_url);
    crate::frames::fetch_iframe_url_sync(net, &resolved, &origin, None).map(|(body, _, _)| body)
}

fn percent_decode_utf8(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(s) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(v) = u8::from_str_radix(s, 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64_decode_simple(input: &str) -> Result<Vec<u8>, ()> {
    let clean: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    let val = |b: u8| -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some(u32::from(b - b'A')),
            b'a'..=b'z' => Some(u32::from(b - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(b - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut i = 0;
    while i < clean.len() {
        let a = val(clean[i]).ok_or(())?;
        let b = clean.get(i + 1).copied().and_then(val).ok_or(())?;
        out.push(((a << 2) | (b >> 4)) as u8);
        if let Some(&c_byte) = clean.get(i + 2) {
            if c_byte != b'=' {
                let c = val(c_byte).ok_or(())?;
                out.push((((b & 0xF) << 4) | (c >> 2)) as u8);
                if let Some(&d_byte) = clean.get(i + 3) {
                    if d_byte != b'=' {
                        let d = val(d_byte).ok_or(())?;
                        out.push((((c & 0x3) << 6) | d) as u8);
                    }
                }
            }
        }
        i += 4;
    }
    Ok(out)
}

/// Immediately clears children of `container` (for `setHTML`/`streamHTML`) or removes
/// `self_nid` (for `replaceWithHTML`/`streamReplaceWithHTML`), firing `disconnectedCallback`
/// synchronously and emitting `DomMutation::ChildList`.
fn perform_immediate_removal(
    vm: &mut JsVm,
    host: &SharedHost,
    self_nid: NodeId,
    target: &ResolvedTarget,
    kind: InsertKind,
) {
    match kind {
        InsertKind::Set => {
            let (removed, was_connected) = {
                let doc_rc = Rc::clone(&host.borrow().document);
                let mut doc = doc_rc.borrow_mut();
                let was_conn =
                    !target.is_template_content && doc.is_connected(target.insert_container);
                let removed: Vec<NodeId> = doc.children(target.insert_container).collect();
                for &c in &removed {
                    doc.remove_child(c);
                }
                (removed, was_conn)
            };
            if !removed.is_empty() {
                host.borrow_mut()
                    .invalidate_dom_mutation(target.insert_container, None);
                host.borrow().emit_mutation(DomMutation::ChildList {
                    target: target.insert_container,
                    added: Vec::new(),
                    removed: removed.clone(),
                    previous_sibling: None,
                    next_sibling: None,
                });
                if was_connected {
                    for c in removed {
                        notify_custom_element_subtree(vm, host, c, false);
                    }
                }
            }
        }
        InsertKind::ReplaceWith => {
            let (prev_sib, next_sib, was_connected) = {
                let doc_rc = Rc::clone(&host.borrow().document);
                let mut doc = doc_rc.borrow_mut();
                let was_conn = doc.is_connected(self_nid);
                let prev = doc.previous_sibling(self_nid);
                let next = doc.next_sibling(self_nid);
                doc.remove_child(self_nid);
                (prev, next, was_conn)
            };
            host.borrow_mut()
                .invalidate_dom_mutation(target.insert_container, None);
            host.borrow().emit_mutation(DomMutation::ChildList {
                target: target.insert_container,
                added: Vec::new(),
                removed: vec![self_nid],
                previous_sibling: prev_sib,
                next_sibling: next_sib,
            });
            if was_connected {
                notify_custom_element_subtree(vm, host, self_nid, false);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Positional HTML Insertion Methods (12 methods)
// ---------------------------------------------------------------------------

fn trusted_sink_name(
    host: &SharedHost,
    self_nid: NodeId,
    kind: InsertKind,
    method: &str,
) -> String {
    if kind.is_sibling() {
        format!("Node {method}")
    } else if matches!(
        host.borrow().document.borrow().get(self_nid),
        Some(NodeData::ShadowRoot { .. })
    ) {
        format!("ShadowRoot {method}")
    } else {
        format!("Element {method}")
    }
}

fn install_positional_html_methods(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
) {
    let ep = host.borrow().prototypes.element_proto;
    let srp = host.borrow().prototypes.shadow_root_proto;
    let cdp = host.borrow().prototypes.character_data_proto;

    let container_methods: &[(&str, InsertKind, bool)] = &[
        ("setHTML", InsertKind::Set, true),
        ("setHTMLUnsafe", InsertKind::Set, false),
        ("appendHTML", InsertKind::Append, true),
        ("appendHTMLUnsafe", InsertKind::Append, false),
        ("prependHTML", InsertKind::Prepend, true),
        ("prependHTMLUnsafe", InsertKind::Prepend, false),
    ];
    for &(name, kind, is_safe) in container_methods {
        for proto in [ep, srp] {
            install_one_positional_method(
                vm, proto, name, kind, is_safe, host, events, event_loop, web_apis, net,
            );
        }
    }

    let sibling_methods: &[(&str, InsertKind, bool)] = &[
        ("beforeHTML", InsertKind::Before, true),
        ("beforeHTMLUnsafe", InsertKind::Before, false),
        ("afterHTML", InsertKind::After, true),
        ("afterHTMLUnsafe", InsertKind::After, false),
        ("replaceWithHTML", InsertKind::ReplaceWith, true),
        ("replaceWithHTMLUnsafe", InsertKind::ReplaceWith, false),
    ];
    for &(name, kind, is_safe) in sibling_methods {
        for proto in [ep, cdp] {
            install_one_positional_method(
                vm, proto, name, kind, is_safe, host, events, event_loop, web_apis, net,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn install_one_positional_method(
    vm: &mut JsVm,
    proto: HeapHandle,
    name: &'static str,
    kind: InsertKind,
    is_safe: bool,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
) {
    let h = Rc::clone(host);
    let evs = Rc::clone(events);
    let el = Rc::clone(event_loop);
    let wa = Rc::clone(web_apis);
    let n = Rc::clone(net);
    define_host_method(vm, proto, name, move |vm, this_val, args| {
        let self_nid = require_node(vm, &h, this_val)?;
        if args.is_empty() {
            return Err(vm.type_error(format!("{name} requires at least 1 argument")));
        }
        let html_arg = arg(args, 0);
        let opts_arg = arg(args, 1);
        let (markup, pre_opts) = if is_safe {
            let markup = to_dom_string(vm, html_arg)?;
            (markup, None)
        } else {
            let sink_name = trusted_sink_name(&h, self_nid, kind, name);
            let trusted_opts = check_trusted_html_parser_options(
                vm, &h, &evs, &el, self_nid, &sink_name, opts_arg, true,
            )?;
            if let Some(opts) = trusted_opts {
                let markup = if let Some(s) = h.borrow().trusted_types.is_trusted_html(html_arg) {
                    s
                } else {
                    to_dom_string(vm, html_arg)?
                };
                (markup, Some(opts))
            } else {
                let markup = check_trusted_html_sink(
                    vm, &h, &evs, &el, self_nid, &sink_name, html_arg, false,
                )?;
                (markup, None)
            }
        };
        let Some(target) = resolve_target_for_html_method(vm, &h, self_nid, kind, false, is_safe)?
        else {
            return Ok(JsValue::UNDEFINED);
        };
        let opts = match pre_opts {
            Some(o) => o,
            None => parse_html_method_options(vm, &h, opts_arg, is_safe)?,
        };

        if kind.removes_immediately() {
            perform_immediate_removal(vm, &h, self_nid, &target, kind);
        }

        let mut st = StreamMirrorState::new(&target, opts.sanitizer, opts.run_scripts, false, &h);
        st.parser.push_chunk(&markup);
        let mut closed_scripts = Vec::new();
        while let ParserStreamStep::Script(sid) = st.parser.step() {
            sync_parsed_tree_into_host(vm, &h, &mut st);
            closed_scripts.push(sid);
        }
        st.parser.finish();
        sync_parsed_tree_into_host(vm, &h, &mut st);

        for sid in closed_scripts {
            prepare_and_run_script(vm, &h, &el, &wa, &n, &mut st, sid, false);
        }
        // Trigger CSS invalidation if streaming into `<style>` or `<html>`.
        if is_html_element(
            &h.borrow().document.borrow(),
            target.logical_container,
            "style",
        ) {
            h.borrow_mut().mark_sheets_dirty();
        }
        Ok(JsValue::UNDEFINED)
    });
}

// ---------------------------------------------------------------------------
// Streaming HTML Methods (12 methods)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn install_streaming_html_methods(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    net: &SharedNetBridge,
) {
    let ep = host.borrow().prototypes.element_proto;
    let srp = host.borrow().prototypes.shadow_root_proto;
    let cdp = host.borrow().prototypes.character_data_proto;

    let container_methods: &[(&str, InsertKind, bool)] = &[
        ("streamHTML", InsertKind::Set, true),
        ("streamHTMLUnsafe", InsertKind::Set, false),
        ("streamAppendHTML", InsertKind::Append, true),
        ("streamAppendHTMLUnsafe", InsertKind::Append, false),
        ("streamPrependHTML", InsertKind::Prepend, true),
        ("streamPrependHTMLUnsafe", InsertKind::Prepend, false),
    ];
    for &(name, kind, is_safe) in container_methods {
        for proto in [ep, srp] {
            install_one_streaming_method(
                vm, proto, name, kind, is_safe, host, events, event_loop, web_apis, streams, net,
            );
        }
    }

    let sibling_methods: &[(&str, InsertKind, bool)] = &[
        ("streamBeforeHTML", InsertKind::Before, true),
        ("streamBeforeHTMLUnsafe", InsertKind::Before, false),
        ("streamAfterHTML", InsertKind::After, true),
        ("streamAfterHTMLUnsafe", InsertKind::After, false),
        ("streamReplaceWithHTML", InsertKind::ReplaceWith, true),
        (
            "streamReplaceWithHTMLUnsafe",
            InsertKind::ReplaceWith,
            false,
        ),
    ];
    for &(name, kind, is_safe) in sibling_methods {
        for proto in [ep, cdp] {
            install_one_streaming_method(
                vm, proto, name, kind, is_safe, host, events, event_loop, web_apis, streams, net,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn install_one_streaming_method(
    vm: &mut JsVm,
    proto: HeapHandle,
    name: &'static str,
    kind: InsertKind,
    is_safe: bool,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    net: &SharedNetBridge,
) {
    let h = Rc::clone(host);
    let evs = Rc::clone(events);
    let el = Rc::clone(event_loop);
    let wa = Rc::clone(web_apis);
    let ss = Rc::clone(streams);
    let n = Rc::clone(net);
    define_host_method(vm, proto, name, move |vm, this_val, args| {
        let self_nid = require_node(vm, &h, this_val)?;
        // Per WHATWG HTML streaming methods + W3C Trusted Types:
        // for unsafe methods, vet `TrustedHTMLParserOptions` BEFORE hierarchy checks,
        // parent/sibling capture, or clearing children; for safe methods (and unvetted
        // options), validate target hierarchy BEFORE validating the sanitizer config!
        let pre_opts = if is_safe {
            None
        } else {
            let sink_name = trusted_sink_name(&h, self_nid, kind, name);
            check_trusted_html_parser_options(
                vm,
                &h,
                &evs,
                &el,
                self_nid,
                &sink_name,
                arg(args, 0),
                false,
            )?
        };
        let Some(target) = resolve_target_for_html_method(vm, &h, self_nid, kind, true, is_safe)?
        else {
            return Err(create_dom_exception(
                vm,
                "HierarchyRequestError",
                "Invalid target for HTML streaming method",
            ));
        };
        let opts = match pre_opts {
            Some(o) => o,
            None => parse_html_method_options(vm, &h, arg(args, 0), is_safe)?,
        };

        if kind.removes_immediately() {
            perform_immediate_removal(vm, &h, self_nid, &target, kind);
        }

        // Coalesce onto an immediately preceding sibling `Text` node before the insertion point
        // (for `streamAppendHTML*`, `streamBeforeHTML*`, `streamAfterHTML*`, `streamReplaceWithHTML*`).
        let allow_initial_coalesce = !matches!(kind, InsertKind::Set);
        let state = Rc::new(RefCell::new(StreamMirrorState::new(
            &target,
            opts.sanitizer,
            opts.run_scripts,
            allow_initial_coalesce,
            &h,
        )));

        // Track coalesced text prefix so subsequent chunks growing the same src Text node keep the prefix!
        let coalesced_prefixes: Rc<RefCell<HashMap<NodeId, String>>> =
            Rc::new(RefCell::new(HashMap::new()));
        if allow_initial_coalesce {
            if let Some(coal_id) = state.borrow().coalesce_text_node {
                if let Some(NodeData::Text(existing)) = h.borrow().document.borrow().get(coal_id) {
                    coalesced_prefixes
                        .borrow_mut()
                        .insert(coal_id, existing.clone());
                }
            }
        }

        // `write(chunk)` callback
        let h_w = Rc::clone(&h);
        let el_w = Rc::clone(&el);
        let wa_w = Rc::clone(&wa);
        let n_w = Rc::clone(&n);
        let st_w = Rc::clone(&state);
        let cp_w = Rc::clone(&coalesced_prefixes);
        let write_fn = vm.alloc_host_fn("stream_html_write", move |vm, _, args| {
            // 1. Convert chunk to DOMString FIRST (WHATWG §8.4: user `toString()` may mutate DOM/detach ref_node!).
            if st_w.borrow().finished {
                return Ok(JsValue::UNDEFINED);
            }
            let chunk_str = to_dom_string(vm, arg(args, 0))?;
            // 2. Validate ref_node is STILL a child of `insert_container` AFTER `ToString`!
            {
                let sb = st_w.borrow();
                if sb.finished {
                    return Ok(JsValue::UNDEFINED);
                }
                if let Some(rn) = sb.ref_node {
                    let parent_ok =
                        h_w.borrow().document.borrow().parent(rn) == Some(sb.insert_container);
                    if !parent_ok {
                        return Err(create_dom_exception(
                            vm,
                            "HierarchyRequestError",
                            "Reference node is no longer a child of the streaming container",
                        ));
                    }
                }
            }
            // 3. Feed chunk and step parser, syncing DOM and executing closed scripts in order.
            st_w.borrow_mut().parser.push_chunk(&chunk_str);
            loop {
                let step_res = st_w.borrow_mut().parser.step();
                match step_res {
                    ParserStreamStep::Script(sid) => {
                        {
                            let mut sb = st_w.borrow_mut();
                            sync_with_coalesced_prefix(vm, &h_w, &mut sb, &cp_w);
                        }
                        // Execute closed script with `st_w` borrow released so reentrant calls work!
                        let mut sb = st_w.borrow_mut();
                        let run = sb.run_scripts && !sb.is_template_content;
                        if run {
                            let script_action = extract_script_action(&h_w, &mut sb, sid);
                            drop(sb);
                            dispatch_stream_script(
                                vm,
                                &h_w,
                                &el_w,
                                &wa_w,
                                &n_w,
                                &st_w,
                                script_action,
                            );
                        }
                    }
                    ParserStreamStep::CustomElement(_) => {}
                    ParserStreamStep::Done => {
                        let svg_scripts = {
                            let mut sb = st_w.borrow_mut();
                            sync_with_coalesced_prefix(vm, &h_w, &mut sb, &cp_w);
                            collect_pending_svg_scripts(&h_w, &mut sb)
                        };
                        for action in svg_scripts {
                            dispatch_stream_script(
                                vm,
                                &h_w,
                                &el_w,
                                &wa_w,
                                &n_w,
                                &st_w,
                                Some(action),
                            );
                        }
                        break;
                    }
                }
            }
            // Queue async `onerror` for any newly inserted `<img src="..." onerror="...">` whose attrs survived sanitization.
            schedule_unsanitized_img_onerror(&h_w, &el_w, &st_w);
            if is_html_element(
                &h_w.borrow().document.borrow(),
                st_w.borrow().logical_container,
                "style",
            ) {
                h_w.borrow_mut().mark_sheets_dirty();
            }
            Ok(JsValue::UNDEFINED)
        });

        // `close()` callback
        let h_c = Rc::clone(&h);
        let el_c = Rc::clone(&el);
        let wa_c = Rc::clone(&wa);
        let n_c = Rc::clone(&n);
        let st_c = Rc::clone(&state);
        let cp_c = Rc::clone(&coalesced_prefixes);
        let close_fn = vm.alloc_host_fn("stream_html_close", move |vm, _, _| {
            let deferred = {
                let mut sb = st_c.borrow_mut();
                if sb.finished {
                    return Ok(JsValue::UNDEFINED);
                }
                sb.parser.finish();
                sync_with_coalesced_prefix(vm, &h_c, &mut sb, &cp_c);
                sb.finished = true;
                std::mem::take(&mut sb.deferred_scripts)
            };
            if is_html_element(
                &h_c.borrow().document.borrow(),
                st_c.borrow().logical_container,
                "style",
            ) {
                h_c.borrow_mut().mark_sheets_dirty();
            }
            // Queue deferred & module scripts as tasks after close() resolves!
            for (dst_sid, src_attr, inline_code, is_mod) in deferred {
                let code_opt = match src_attr {
                    Some(src) => fetch_script_source_sync(&wa_c, &n_c, &src),
                    None => Some(inline_code),
                };
                if let Some(code) = code_opt {
                    let h_cb = Rc::clone(&h_c);
                    el_c.borrow_mut()
                        .post_host_task(TaskPriority::Networking, move |vm| {
                            eval_script_node(vm, &h_cb, dst_sid, &code, is_mod);
                        });
                }
            }
            let p = alloc_promise(vm);
            resolve_promise(vm, p, JsValue::UNDEFINED);
            Ok(JsValue::from_object(p))
        });

        // `abort(reason)` callback
        let st_a = Rc::clone(&state);
        let abort_fn = vm.alloc_host_fn("stream_html_abort", move |_, _, _| {
            st_a.borrow_mut().finished = true;
            Ok(JsValue::UNDEFINED)
        });

        let ws_h = create_writable_stream_from_callbacks(vm, &ss, write_fn, close_fn, abort_fn);
        Ok(JsValue::from_object(ws_h))
    });
}

fn sync_with_coalesced_prefix(
    vm: &mut JsVm,
    host: &SharedHost,
    st: &mut StreamMirrorState,
    coalesced_prefixes: &Rc<RefCell<HashMap<NodeId, String>>>,
) {
    // Sync parsed tree into host, preserving any pre-existing prefix on a coalesced Text node.
    let src_doc_rc = st.parser.document();
    let roots = st.parser.fragment_roots();
    let mut newly_connected = Vec::new();
    let src_doc = src_doc_rc.borrow();
    for src_root in roots {
        if let Some(&MirrorEntry::Mapped(dst_id)) = st.mirror.get(&src_root) {
            if let Some(prefix) = coalesced_prefixes.borrow().get(&dst_id).cloned() {
                if let Some(NodeData::Text(src_text)) = src_doc.get(src_root) {
                    let desired = format!("{prefix}{src_text}");
                    let old_val = {
                        let doc_rc = Rc::clone(&host.borrow().document);
                        let mut doc = doc_rc.borrow_mut();
                        if let Some(NodeData::Text(dst_text)) = doc.get_mut(dst_id) {
                            if dst_text.as_str() != desired.as_str() {
                                let old = dst_text.clone();
                                *dst_text = desired;
                                Some(old)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };
                    if let Some(old_value) = old_val {
                        host.borrow().emit_mutation(DomMutation::CharacterData {
                            target: dst_id,
                            old_value,
                        });
                    }
                    continue;
                }
            }
        }
        sync_node_recursive(
            vm,
            host,
            st,
            &src_doc,
            src_root,
            st.insert_container,
            st.ref_node,
            !st.is_template_content,
            &mut newly_connected,
        );
    }
    drop(src_doc);
    host.borrow_mut()
        .invalidate_dom_mutation(st.insert_container, None);
}

#[allow(clippy::type_complexity)]
fn extract_script_action(
    host: &SharedHost,
    st: &mut StreamMirrorState,
    src_script_id: NodeId,
) -> Option<(NodeId, Option<String>, String, bool, bool, bool)> {
    if !is_live_browsing_context_doc(host, st.owner_doc) {
        return None;
    }
    let &MirrorEntry::Mapped(dst_script_id) = st.mirror.get(&src_script_id)? else {
        return None;
    };
    let (is_connected, same_doc) = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        (
            doc.is_connected(dst_script_id),
            hb.owner_doc_of(dst_script_id) == st.owner_doc,
        )
    };
    if !is_connected {
        let mut hb = host.borrow_mut();
        hb.script_force_async.insert(dst_script_id);
        hb.contextual_scripts.insert(dst_script_id);
        return None;
    }
    if !same_doc {
        return None;
    }
    if !st.started_scripts.insert(dst_script_id) {
        return None;
    }
    let (src_attr, inline_code, is_defer, is_async, is_module) =
        extract_executable_script_info(host, dst_script_id)?;
    Some((
        dst_script_id,
        src_attr,
        inline_code,
        is_defer,
        is_async,
        is_module,
    ))
}

#[allow(clippy::type_complexity)]
fn dispatch_stream_script(
    vm: &mut JsVm,
    host: &SharedHost,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    net: &SharedNetBridge,
    st_rc: &Rc<RefCell<StreamMirrorState>>,
    action: Option<(NodeId, Option<String>, String, bool, bool, bool)>,
) {
    let Some((dst_sid, src_attr, inline_code, is_defer, is_async, is_mod)) = action else {
        return;
    };
    if is_defer || (is_mod && !is_async) {
        st_rc
            .borrow_mut()
            .deferred_scripts
            .push((dst_sid, src_attr, inline_code, is_mod));
    } else if let Some(src) = src_attr {
        if is_async {
            schedule_async_external_script(host, event_loop, web_apis, net, dst_sid, src, is_mod);
        } else if let Some(code) = fetch_script_source_sync(web_apis, net, &src) {
            eval_script_node(vm, host, dst_sid, &code, is_mod);
        }
    } else if !inline_code.trim().is_empty() {
        eval_script_node(vm, host, dst_sid, &inline_code, is_mod);
    }
}

#[allow(clippy::type_complexity)]
fn collect_pending_svg_scripts(
    host: &SharedHost,
    st: &mut StreamMirrorState,
) -> Vec<(NodeId, Option<String>, String, bool, bool, bool)> {
    if !st.run_scripts || st.is_template_content {
        return Vec::new();
    }
    let src_svg_scripts: Vec<NodeId> = {
        let src_doc_rc = st.parser.document();
        let src_doc = src_doc_rc.borrow();
        st.mirror
            .iter()
            .filter_map(|(&src_id, &entry)| {
                let MirrorEntry::Mapped(_) = entry else {
                    return None;
                };
                match src_doc.get(src_id) {
                    Some(NodeData::Element(el))
                        if &*el.name.ns == SVG_NS && el.name.local.as_str() == "script" =>
                    {
                        Some(src_id)
                    }
                    _ => None,
                }
            })
            .collect()
    };
    let mut out = Vec::new();
    for sid in src_svg_scripts {
        if let Some(action) = extract_script_action(host, st, sid) {
            out.push(action);
        }
    }
    out
}

fn schedule_unsanitized_img_onerror(
    host: &SharedHost,
    event_loop: &SharedEventLoop,
    st_rc: &Rc<RefCell<StreamMirrorState>>,
) {
    let imgs: Vec<(NodeId, String)> = {
        let mut st = st_rc.borrow_mut();
        let hb = host.borrow();
        let doc = hb.document.borrow();
        let mut out = Vec::new();
        for &entry in st.mirror.values() {
            if let MirrorEntry::Mapped(dst_id) = entry {
                if st.started_scripts.contains(&dst_id) {
                    continue;
                }
                if let Some(NodeData::Element(el)) = doc.get(dst_id) {
                    if &*el.name.ns == HTML_NS && el.name.local.as_str() == "img" {
                        if let (Some(src), Some(onerror)) =
                            (el.get_attribute("src"), el.get_attribute("onerror"))
                        {
                            if !src.is_empty() && !onerror.is_empty() {
                                out.push((dst_id, onerror.to_string()));
                            }
                        }
                    }
                }
            }
        }
        for (id, _) in &out {
            st.started_scripts.insert(*id);
        }
        out
    };
    for (_nid, code) in imgs {
        event_loop
            .borrow_mut()
            .post_host_task(TaskPriority::Networking, move |vm| {
                let _ = vm.eval_script(&code);
            });
    }
}
