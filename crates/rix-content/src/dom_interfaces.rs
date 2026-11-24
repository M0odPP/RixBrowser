//! WHATWG DOM Standard core interfaces, WebIDL prototype hierarchy, multi-document
//! ownership, XML/HTML name & namespace validation, `Attr`/`NamedNodeMap`, live
//! `DOMTokenList`/`HTMLCollection`/`NodeList` proxies, `CharacterData`/`Text`/`CDATASection`,
//! `DOMImplementation`, `Node` tree validity, and `TreeWalker`/`NodeIterator`/`NodeFilter`.

use std::collections::HashMap;
use std::rc::Rc;

use rix_dom::{is_valid_custom_element_name, Document, NodeData, NodeId};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::dom_bridge::{
    arg, define_global, define_host_accessor, define_host_method, install_child_node_mixin,
    require_node, wrap_node, DomMutation, HostState, SharedHost,
};
use crate::web_apis::create_dom_exception;

pub const HTML_NS: &str = "http://www.w3.org/1999/xhtml";
pub const SVG_NS: &str = "http://www.w3.org/2000/svg";
pub const MATHML_NS: &str = "http://www.w3.org/1998/Math/MathML";
pub const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
pub const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

/// Per-document metadata for the main `document` and every sub-document node
/// (`createHTMLDocument`, `createDocument`, `new Document()`, `DOMParser`).
#[derive(Debug, Clone)]
pub struct DocumentMeta {
    pub is_xml: bool,
    pub use_xml_document_proto: bool,
    pub use_html_document_proto: bool,
    pub content_type: String,
    pub url: String,
    pub character_set: String,
    pub quirks_mode: u8,
    pub default_view: Option<HeapHandle>,
}

impl DocumentMeta {
    #[must_use]
    pub fn html(url: impl Into<String>) -> Self {
        Self {
            is_xml: false,
            use_xml_document_proto: false,
            use_html_document_proto: true,
            content_type: "text/html".to_string(),
            url: url.into(),
            character_set: "UTF-8".to_string(),
            quirks_mode: 0,
            default_view: None,
        }
    }

    #[must_use]
    pub fn xml_generic(url: impl Into<String>) -> Self {
        Self {
            is_xml: true,
            use_xml_document_proto: false,
            use_html_document_proto: false,
            content_type: "application/xml".to_string(),
            url: url.into(),
            character_set: "UTF-8".to_string(),
            quirks_mode: 0,
            default_view: None,
        }
    }

    #[must_use]
    pub fn xml_document(content_type: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            is_xml: true,
            use_xml_document_proto: true,
            use_html_document_proto: false,
            content_type: content_type.into(),
            url: url.into(),
            character_set: "UTF-8".to_string(),
            quirks_mode: 0,
            default_view: None,
        }
    }
}

/// Live or detached WHATWG DOM `Attr` node backing state.
#[derive(Debug, Clone)]
pub struct AttrEntry {
    pub owner_element: Option<NodeId>,
    pub owner_doc: NodeId,
    pub namespace: String,
    pub prefix: Option<String>,
    pub local_name: String,
    pub detached_value: String,
}

impl AttrEntry {
    #[must_use]
    pub fn qualified_name(&self) -> String {
        match &self.prefix {
            Some(p) if !p.is_empty() => format!("{p}:{}", self.local_name),
            _ => self.local_name.clone(),
        }
    }
}

/// Backing query descriptor for live `HTMLCollection` objects.
#[derive(Debug, Clone)]
pub enum HtmlCollectionKind {
    Children(NodeId),
    ByTagName {
        root: NodeId,
        qname: String,
        is_html_doc: bool,
    },
    ByTagNameNS {
        root: NodeId,
        ns: String,
        local: String,
    },
    ByClassName {
        root: NodeId,
        classes: Vec<String>,
        quirks: bool,
    },
}

/// Backing state for `DOMTokenList` objects.
#[derive(Debug, Clone)]
pub struct TokenListEntry {
    pub node: NodeId,
    pub attr_name: &'static str,
    pub supported_tokens: Option<&'static [&'static str]>,
}

/// Backing state for `TreeWalker` / `NodeIterator`.
#[derive(Debug, Clone)]
pub struct TraversalEntry {
    pub root: NodeId,
    pub what_to_show: u32,
    pub filter: Option<HeapHandle>,
    pub current_node: NodeId,
    pub pointer_before: bool,
    pub active: bool,
    pub is_iterator: bool,
}

/// Sets `target[Symbol.toStringTag] = tag` (configurable, non-writable, non-enumerable).
pub fn define_to_string_tag(vm: &mut JsVm, target: HeapHandle, tag: &str) {
    let k = vm.key_symbol_to_string_tag;
    let v = JsValue::from_string(vm.heap.alloc_string(tag));
    vm.heap
        .object_define_data(target, k, v, PropertyAttrs::CONFIGURABLE, &mut vm.shapes);
}

// ---------------------------------------------------------------------------
// WHATWG DOM §4.2 / §4.9 Name, Prefix & Namespace Validation
// ---------------------------------------------------------------------------

#[inline]
pub fn is_ascii_ws(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

#[inline]
pub fn is_valid_namespace_prefix(s: &str) -> bool {
    !s.is_empty()
        && !s
            .bytes()
            .any(|b| is_ascii_ws(b) || matches!(b, 0x00 | b'/' | b'>'))
}

/// WHATWG DOM "valid element local name" (2024+ spec, PR whatwg/dom#1079).
pub fn is_valid_element_local_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first.is_ascii_alphabetic() {
        !s.bytes()
            .any(|b| is_ascii_ws(b) || matches!(b, 0x00 | b'/' | b'>'))
    } else if first == ':' || first == '_' || (first as u32) >= 0x80 {
        chars.all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':' | '_') || (c as u32) >= 0x80
        })
    } else {
        false
    }
}

/// WHATWG DOM "valid attribute local name".
pub fn is_valid_attribute_local_name(s: &str) -> bool {
    !s.is_empty()
        && !s
            .bytes()
            .any(|b| is_ascii_ws(b) || matches!(b, 0x00 | b'/' | b'=' | b'>'))
}

/// WHATWG DOM "valid doctype name".
pub fn is_valid_doctype_name(s: &str) -> bool {
    !s.bytes()
        .any(|b| is_ascii_ws(b) || matches!(b, 0x00 | b'>'))
}

/// WHATWG DOM §4.9 "validate and extract" for element / attribute qualified names.
pub fn validate_and_extract(
    vm: &mut JsVm,
    namespace: &str,
    qualified_name: &str,
    is_element: bool,
) -> Result<(String, Option<String>, String), JsException> {
    let (prefix, local) = match qualified_name.split_once(':') {
        Some((p, l)) => {
            let local_ok = if is_element {
                is_valid_element_local_name(l)
            } else {
                is_valid_attribute_local_name(l)
            };
            if !is_valid_namespace_prefix(p) || !local_ok {
                return Err(create_dom_exception(
                    vm,
                    "InvalidCharacterError",
                    "The qualified name contains an invalid character",
                ));
            }
            (Some(p.to_string()), l.to_string())
        }
        None => {
            let local_ok = if is_element {
                is_valid_element_local_name(qualified_name)
            } else {
                is_valid_attribute_local_name(qualified_name)
            };
            if !local_ok {
                return Err(create_dom_exception(
                    vm,
                    "InvalidCharacterError",
                    "The qualified name contains an invalid character",
                ));
            }
            (None, qualified_name.to_string())
        }
    };

    if prefix.is_some() && namespace.is_empty() {
        return Err(create_dom_exception(
            vm,
            "NamespaceError",
            "A namespace prefix cannot be used with a null namespace",
        ));
    }
    if prefix.as_deref() == Some("xml") && namespace != XML_NS {
        return Err(create_dom_exception(
            vm,
            "NamespaceError",
            "The 'xml' prefix is reserved for the XML namespace",
        ));
    }
    if (prefix.as_deref() == Some("xmlns") || (prefix.is_none() && local == "xmlns"))
        && namespace != XMLNS_NS
    {
        return Err(create_dom_exception(
            vm,
            "NamespaceError",
            "The 'xmlns' prefix or qualified name is reserved for the XMLNS namespace",
        ));
    }
    if namespace == XMLNS_NS
        && prefix.as_deref() != Some("xmlns")
        && !(prefix.is_none() && local == "xmlns")
    {
        return Err(create_dom_exception(
            vm,
            "NamespaceError",
            "The XMLNS namespace requires the 'xmlns' prefix or local name",
        ));
    }

    Ok((namespace.to_string(), prefix, local))
}

/// Validates XML Name for `Document.createProcessingInstruction(target, data)`.
pub fn is_valid_xml_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    is_xml_name_start_char(first) && chars.all(is_xml_name_char)
}

fn is_xml_name_start_char(c: char) -> bool {
    let cp = c as u32;
    c == ':'
        || c == '_'
        || c.is_ascii_alphabetic()
        || matches!(
            cp,
            0xC0..=0xD6
                | 0xD8..=0xF6
                | 0xF8..=0x2FF
                | 0x370..=0x37D
                | 0x37F..=0x1FFF
                | 0x200C..=0x200D
                | 0x2070..=0x218F
                | 0x2C00..=0x2FEF
                | 0x3001..=0xD7FF
                | 0xF900..=0xFDCF
                | 0xFDF0..=0xFFFD
                | 0x10000..=0xEFFFF
        )
}

fn is_xml_name_char(c: char) -> bool {
    let cp = c as u32;
    is_xml_name_start_char(c)
        || c == '-'
        || c == '.'
        || c.is_ascii_digit()
        || cp == 0xB7
        || matches!(cp, 0x0300..=0x036F | 0x203F..=0x2040)
}

// ---------------------------------------------------------------------------
// HTML / SVG / MathML Tag -> Interface Prototype Mapping
// ---------------------------------------------------------------------------

/// Maps an HTML-namespace local name to its exact HTML Living Standard §4 interface name.
#[must_use]
pub fn html_tag_interface_name(local: &str) -> &'static str {
    match local {
        "a" => "HTMLAnchorElement",
        "area" => "HTMLAreaElement",
        "audio" => "HTMLAudioElement",
        "base" => "HTMLBaseElement",
        "blockquote" | "q" => "HTMLQuoteElement",
        "body" => "HTMLBodyElement",
        "br" => "HTMLBRElement",
        "button" => "HTMLButtonElement",
        "canvas" => "HTMLCanvasElement",
        "caption" => "HTMLTableCaptionElement",
        "col" | "colgroup" => "HTMLTableColElement",
        "data" => "HTMLDataElement",
        "datalist" => "HTMLDataListElement",
        "del" | "ins" => "HTMLModElement",
        "details" => "HTMLDetailsElement",
        "dialog" => "HTMLDialogElement",
        "dir" => "HTMLDirectoryElement",
        "div" => "HTMLDivElement",
        "dl" => "HTMLDListElement",
        "embed" => "HTMLEmbedElement",
        "fieldset" => "HTMLFieldSetElement",
        "font" => "HTMLFontElement",
        "form" => "HTMLFormElement",
        "frame" => "HTMLFrameElement",
        "frameset" => "HTMLFrameSetElement",
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => "HTMLHeadingElement",
        "head" => "HTMLHeadElement",
        "hr" => "HTMLHRElement",
        "html" => "HTMLHtmlElement",
        "iframe" => "HTMLIFrameElement",
        "img" => "HTMLImageElement",
        "input" => "HTMLInputElement",
        "label" => "HTMLLabelElement",
        "legend" => "HTMLLegendElement",
        "li" => "HTMLLIElement",
        "link" => "HTMLLinkElement",
        "map" => "HTMLMapElement",
        "marquee" => "HTMLMarqueeElement",
        "menu" => "HTMLMenuElement",
        "meta" => "HTMLMetaElement",
        "meter" => "HTMLMeterElement",
        "object" => "HTMLObjectElement",
        "ol" => "HTMLOListElement",
        "optgroup" => "HTMLOptGroupElement",
        "option" => "HTMLOptionElement",
        "output" => "HTMLOutputElement",
        "p" => "HTMLParagraphElement",
        "param" => "HTMLParamElement",
        "picture" => "HTMLPictureElement",
        "pre" | "listing" | "xmp" => "HTMLPreElement",
        "progress" => "HTMLProgressElement",
        "script" => "HTMLScriptElement",
        "select" => "HTMLSelectElement",
        "slot" => "HTMLSlotElement",
        "source" => "HTMLSourceElement",
        "span" => "HTMLSpanElement",
        "style" => "HTMLStyleElement",
        "table" => "HTMLTableElement",
        "tbody" | "tfoot" | "thead" => "HTMLTableSectionElement",
        "td" | "th" => "HTMLTableCellElement",
        "template" => "HTMLTemplateElement",
        "textarea" => "HTMLTextAreaElement",
        "time" => "HTMLTimeElement",
        "title" => "HTMLTitleElement",
        "tr" => "HTMLTableRowElement",
        "track" => "HTMLTrackElement",
        "ul" => "HTMLUListElement",
        "video" => "HTMLVideoElement",
        // HTML elements that directly use `HTMLElement` (no dedicated subclass):
        "abbr" | "acronym" | "address" | "article" | "aside" | "b" | "bdi" | "bdo" | "big"
        | "center" | "cite" | "code" | "dd" | "dfn" | "dt" | "em" | "figcaption" | "figure"
        | "footer" | "header" | "hgroup" | "i" | "kbd" | "main" | "mark" | "nav" | "nobr"
        | "noembed" | "noframes" | "noscript" | "plaintext" | "rb" | "rp" | "rt" | "rtc"
        | "ruby" | "s" | "samp" | "search" | "section" | "small" | "strike" | "strong" | "sub"
        | "summary" | "sup" | "tt" | "u" | "var" | "wbr" => "HTMLElement",
        other => {
            if is_valid_custom_element_name(other) {
                "HTMLElement"
            } else {
                "HTMLUnknownElement"
            }
        }
    }
}

/// Maps an SVG-namespace local name to its SVG interface name.
#[must_use]
pub fn svg_tag_interface_name(local: &str) -> &'static str {
    match local {
        "svg" => "SVGSVGElement",
        "g" => "SVGGElement",
        "defs" => "SVGDefsElement",
        "use" => "SVGUseElement",
        "symbol" => "SVGSymbolElement",
        "path" => "SVGPathElement",
        "rect" => "SVGRectElement",
        "circle" => "SVGCircleElement",
        "ellipse" => "SVGEllipseElement",
        "line" => "SVGLineElement",
        "polyline" => "SVGPolylineElement",
        "polygon" => "SVGPolygonElement",
        "text" => "SVGTextElement",
        "tspan" => "SVGTSpanElement",
        "image" => "SVGImageElement",
        "foreignObject" => "SVGForeignObjectElement",
        "style" => "SVGStyleElement",
        "script" => "SVGScriptElement",
        "title" => "SVGTitleElement",
        "desc" => "SVGDescElement",
        "a" => "SVGAElement",
        "clipPath" => "SVGClipPathElement",
        "mask" => "SVGMaskElement",
        "linearGradient" => "SVGLinearGradientElement",
        "radialGradient" => "SVGRadialGradientElement",
        "stop" => "SVGStopElement",
        "pattern" => "SVGPatternElement",
        "filter" => "SVGFilterElement",
        "marker" => "SVGMarkerElement",
        _ => "SVGElement",
    }
}

pub const HTML_ELEMENT_INTERFACES: &[&str] = &[
    "HTMLAnchorElement",
    "HTMLAreaElement",
    "HTMLBaseElement",
    "HTMLBodyElement",
    "HTMLBRElement",
    "HTMLButtonElement",
    "HTMLCanvasElement",
    "HTMLDataElement",
    "HTMLDataListElement",
    "HTMLDetailsElement",
    "HTMLDialogElement",
    "HTMLDirectoryElement",
    "HTMLDivElement",
    "HTMLDListElement",
    "HTMLEmbedElement",
    "HTMLFieldSetElement",
    "HTMLFontElement",
    "HTMLFormElement",
    "HTMLFrameElement",
    "HTMLFrameSetElement",
    "HTMLHeadElement",
    "HTMLHeadingElement",
    "HTMLHRElement",
    "HTMLHtmlElement",
    "HTMLIFrameElement",
    "HTMLImageElement",
    "HTMLInputElement",
    "HTMLLabelElement",
    "HTMLLegendElement",
    "HTMLLIElement",
    "HTMLLinkElement",
    "HTMLMapElement",
    "HTMLMarqueeElement",
    "HTMLMediaElement",
    "HTMLMenuElement",
    "HTMLMetaElement",
    "HTMLMeterElement",
    "HTMLModElement",
    "HTMLObjectElement",
    "HTMLOListElement",
    "HTMLOptGroupElement",
    "HTMLOptionElement",
    "HTMLOutputElement",
    "HTMLParagraphElement",
    "HTMLParamElement",
    "HTMLPictureElement",
    "HTMLPreElement",
    "HTMLProgressElement",
    "HTMLQuoteElement",
    "HTMLScriptElement",
    "HTMLSelectElement",
    "HTMLSlotElement",
    "HTMLSourceElement",
    "HTMLSpanElement",
    "HTMLStyleElement",
    "HTMLTableCaptionElement",
    "HTMLTableCellElement",
    "HTMLTableColElement",
    "HTMLTableElement",
    "HTMLTableRowElement",
    "HTMLTableSectionElement",
    "HTMLTemplateElement",
    "HTMLTextAreaElement",
    "HTMLTimeElement",
    "HTMLTitleElement",
    "HTMLTrackElement",
    "HTMLUListElement",
    "HTMLUnknownElement",
];

pub const SVG_ELEMENT_INTERFACES: &[&str] = &[
    "SVGSVGElement",
    "SVGGElement",
    "SVGDefsElement",
    "SVGUseElement",
    "SVGSymbolElement",
    "SVGGeometryElement",
    "SVGPathElement",
    "SVGRectElement",
    "SVGCircleElement",
    "SVGEllipseElement",
    "SVGLineElement",
    "SVGPolylineElement",
    "SVGPolygonElement",
    "SVGTextElement",
    "SVGTSpanElement",
    "SVGImageElement",
    "SVGForeignObjectElement",
    "SVGStyleElement",
    "SVGScriptElement",
    "SVGTitleElement",
    "SVGDescElement",
    "SVGAElement",
    "SVGClipPathElement",
    "SVGMaskElement",
    "SVGLinearGradientElement",
    "SVGRadialGradientElement",
    "SVGStopElement",
    "SVGPatternElement",
    "SVGFilterElement",
    "SVGMarkerElement",
];

/// Registers a WebIDL interface constructor + prototype pair on `global`.
/// When `ce_host` is `Some(host)`, the constructor supports WHATWG §4.13.2 Custom Element
/// `super()` calls (for both autonomous custom elements extending `HTMLElement` and customized
/// built-ins extending `HTML*Element`).
pub fn register_dom_interface(
    vm: &mut JsVm,
    name: &'static str,
    proto: HeapHandle,
    parent_ctor: Option<HeapHandle>,
    allow_construct: bool,
) -> HeapHandle {
    register_dom_interface_with_ce(vm, name, proto, parent_ctor, allow_construct, None)
}

fn register_dom_interface_with_ce(
    vm: &mut JsVm,
    name: &'static str,
    proto: HeapHandle,
    parent_ctor: Option<HeapHandle>,
    allow_construct: bool,
    ce_host: Option<&SharedHost>,
) -> HeapHandle {
    define_to_string_tag(vm, proto, name);
    let ctor = if let Some(h_ref) = ce_host {
        let h = Rc::clone(h_ref);
        vm.alloc_host_fn(name, move |vm, this_val, _| {
            // 1. Upgrade stack: `upgrade_if_custom_element` set `pending_ce_upgrade_node`.
            let pending_nid = h.borrow_mut().pending_ce_upgrade_node.take();
            if let Some(nid) = pending_nid {
                let wrapper = wrap_node(vm, &h, nid);
                if let (Some(wh), Some(nt)) =
                    (wrapper.as_object(), vm.native_new_target.as_object())
                {
                    if let Ok(nt_proto) = vm.get_property_by_key(nt, vm.key_prototype) {
                        if let Some(ph) = nt_proto.as_object() {
                            vm.heap.object_set_prototype(wh, Some(ph), &mut vm.shapes);
                        }
                    }
                }
                return Ok(wrapper);
            }
            // 2. Direct `new MyCustomElement()` construction via registered constructor.
            if let Some(nt) = vm.native_new_target.as_object() {
                let found_def = h
                    .borrow()
                    .custom_elements
                    .iter()
                    .find(|(_, d)| d.constructor == nt)
                    .map(|(ce_name, d)| (ce_name.clone(), d.extends.clone()));
                if let Some((ce_name, extends)) = found_def {
                    let (local, is_attr) = match extends {
                        Some(ext) => (ext, Some(ce_name)),
                        None => (ce_name, None),
                    };
                    let nid = {
                        let doc_rc = Rc::clone(&h.borrow().document);
                        let mut doc = doc_rc.borrow_mut();
                        let n = doc.create_element_ns(HTML_NS, None, &local);
                        if let Some(NodeData::Element(e)) = doc.get_mut(n) {
                            if let Some(ref is_val) = is_attr {
                                e.set_attribute("is", is_val);
                            }
                            e.pseudo_state.insert(rix_dom::ElementState::DEFINED);
                        }
                        n
                    };
                    {
                        let mut hb = h.borrow_mut();
                        let root = hb.document.borrow().root();
                        hb.set_owner_doc(nid, root);
                        hb.upgraded_custom_elements.insert(nid);
                    }
                    let wrapper = wrap_node(vm, &h, nid);
                    if let Some(wh) = wrapper.as_object() {
                        if let Ok(nt_proto) = vm.get_property_by_key(nt, vm.key_prototype) {
                            if let Some(ph) = nt_proto.as_object() {
                                vm.heap.object_set_prototype(wh, Some(ph), &mut vm.shapes);
                            }
                        }
                    }
                    return Ok(wrapper);
                }
            }
            if allow_construct {
                return Ok(this_val);
            }
            Err(vm.type_error(format!("Illegal constructor: {name}")))
        })
    } else if allow_construct {
        vm.alloc_host_fn(name, |_vm, this_val, _| Ok(this_val))
    } else {
        vm.alloc_host_fn(name, move |vm, _, _| {
            Err(vm.type_error(format!("Illegal constructor: {name}")))
        })
    };
    if let Some(pc) = parent_ctor {
        vm.heap.object_set_prototype(ctor, Some(pc), &mut vm.shapes);
    }
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
    vm.mark_constructor(ctor);
    define_global(vm, name, JsValue::from_object(ctor));
    ctor
}

/// Allocates a new prototype inheriting from `parent`.
pub fn make_proto(vm: &mut JsVm, parent: HeapHandle) -> HeapHandle {
    let p = vm.heap.alloc_empty_object();
    vm.heap
        .object_set_prototype(p, Some(parent), &mut vm.shapes);
    vm.heap.get_object_mut(p).unwrap().is_prototype = true;
    p
}

/// Installs all WebIDL DOM/HTML/SVG/MathML interface constructors, `Node` constants,
/// constructible `Text`/`Comment`/`DocumentFragment`/`Document` constructors,
/// `CharacterData`/`DocumentType`/`ProcessingInstruction`/`Attr`/`NamedNodeMap`,
/// `DOMTokenList`, `HTMLCollection`/`NodeList`, and `TreeWalker`/`NodeIterator`/`NodeFilter`.
pub fn install_all_dom_interfaces(vm: &mut JsVm, host: &SharedHost) {
    let p = host.borrow().prototypes;

    // 1. Node constants on Node.prototype (also copied onto Node constructor below).
    let node_consts: &[(&str, i32)] = &[
        ("ELEMENT_NODE", 1),
        ("ATTRIBUTE_NODE", 2),
        ("TEXT_NODE", 3),
        ("CDATA_SECTION_NODE", 4),
        ("ENTITY_REFERENCE_NODE", 5),
        ("ENTITY_NODE", 6),
        ("PROCESSING_INSTRUCTION_NODE", 7),
        ("COMMENT_NODE", 8),
        ("DOCUMENT_NODE", 9),
        ("DOCUMENT_TYPE_NODE", 10),
        ("DOCUMENT_FRAGMENT_NODE", 11),
        ("NOTATION_NODE", 12),
        ("DOCUMENT_POSITION_DISCONNECTED", 0x01),
        ("DOCUMENT_POSITION_PRECEDING", 0x02),
        ("DOCUMENT_POSITION_FOLLOWING", 0x04),
        ("DOCUMENT_POSITION_CONTAINS", 0x08),
        ("DOCUMENT_POSITION_CONTAINED_BY", 0x10),
        ("DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC", 0x20),
    ];
    let define_consts = |vm: &mut JsVm, target: HeapHandle| {
        for &(cname, cval) in node_consts {
            let k = vm.heap.intern_key(cname);
            vm.heap.object_define_data(
                target,
                k,
                JsValue::from_i32(cval),
                PropertyAttrs::ENUMERABLE,
                &mut vm.shapes,
            );
        }
    };
    define_consts(vm, p.node_proto);

    // 2. Core inheritance chain of constructors (`EventTarget` is later replaced by events.rs).
    let et_ctor = register_dom_interface(vm, "EventTarget", p.event_target_proto, None, false);
    let node_ctor = register_dom_interface(vm, "Node", p.node_proto, Some(et_ctor), false);
    define_consts(vm, node_ctor);

    let elem_ctor = register_dom_interface(vm, "Element", p.element_proto, Some(node_ctor), false);
    let html_elem_ctor = register_dom_interface_with_ce(
        vm,
        "HTMLElement",
        p.html_element_proto,
        Some(elem_ctor),
        true, // Custom elements extend HTMLElement via super()
        Some(host),
    );
    let svg_elem_ctor = register_dom_interface(
        vm,
        "SVGElement",
        p.svg_element_proto,
        Some(elem_ctor),
        false,
    );
    let _mathml_ctor = register_dom_interface(
        vm,
        "MathMLElement",
        p.mathml_element_proto,
        Some(elem_ctor),
        false,
    );

    // Register all ~65 HTML*Element subclasses and Video/Audio under HTMLMediaElement.
    let mut media_ctor = html_elem_ctor;
    for &iface in HTML_ELEMENT_INTERFACES {
        if let Some(&proto) = host.borrow().interface_protos.get(iface) {
            let ctor = register_dom_interface_with_ce(
                vm,
                iface,
                proto,
                Some(html_elem_ctor),
                false,
                Some(host),
            );
            if iface == "HTMLMediaElement" {
                media_ctor = ctor;
            }
        }
    }
    let _ = register_dom_interface(
        vm,
        "HTMLVideoElement",
        p.html_video_element_proto,
        Some(media_ctor),
        false,
    );
    let _ = register_dom_interface(
        vm,
        "HTMLAudioElement",
        p.html_audio_element_proto,
        Some(media_ctor),
        false,
    );

    // Register SVG*Element subclasses.
    for &iface in SVG_ELEMENT_INTERFACES {
        if let Some(&proto) = host.borrow().interface_protos.get(iface) {
            let _ = register_dom_interface(vm, iface, proto, Some(svg_elem_ctor), false);
        }
    }

    // CharacterData -> Text -> CDATASection, Comment, ProcessingInstruction
    let cd_ctor = register_dom_interface(
        vm,
        "CharacterData",
        p.character_data_proto,
        Some(node_ctor),
        false,
    );

    // Constructible `new Text(data = "")`
    define_to_string_tag(vm, p.text_proto, "Text");
    let h_txt = Rc::clone(host);
    let text_ctor = vm.alloc_host_fn("Text", move |vm, _, args| {
        let data = if args.first().is_some_and(|v| !v.is_undefined()) {
            vm.value_to_rust_string(args[0])
        } else {
            String::new()
        };
        let nid = h_txt.borrow().document.borrow_mut().create_text(&data);
        Ok(wrap_node(vm, &h_txt, nid))
    });
    vm.heap
        .object_set_prototype(text_ctor, Some(cd_ctor), &mut vm.shapes);
    vm.heap.object_define_data(
        text_ctor,
        vm.key_prototype,
        JsValue::from_object(p.text_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        p.text_proto,
        vm.key_constructor,
        JsValue::from_object(text_ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.mark_constructor(text_ctor);
    define_global(vm, "Text", JsValue::from_object(text_ctor));

    let _ = register_dom_interface(
        vm,
        "CDATASection",
        p.cdata_section_proto,
        Some(text_ctor),
        false,
    );

    // Constructible `new Comment(data = "")`
    define_to_string_tag(vm, p.comment_proto, "Comment");
    let h_cmt = Rc::clone(host);
    let comment_ctor = vm.alloc_host_fn("Comment", move |vm, _, args| {
        let data = if args.first().is_some_and(|v| !v.is_undefined()) {
            vm.value_to_rust_string(args[0])
        } else {
            String::new()
        };
        let nid = h_cmt.borrow().document.borrow_mut().create_comment(&data);
        Ok(wrap_node(vm, &h_cmt, nid))
    });
    vm.heap
        .object_set_prototype(comment_ctor, Some(cd_ctor), &mut vm.shapes);
    vm.heap.object_define_data(
        comment_ctor,
        vm.key_prototype,
        JsValue::from_object(p.comment_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        p.comment_proto,
        vm.key_constructor,
        JsValue::from_object(comment_ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.mark_constructor(comment_ctor);
    define_global(vm, "Comment", JsValue::from_object(comment_ctor));

    let _ = register_dom_interface(
        vm,
        "ProcessingInstruction",
        p.processing_instruction_proto,
        Some(cd_ctor),
        false,
    );

    // DocumentType, Attr
    let _ = register_dom_interface(
        vm,
        "DocumentType",
        p.document_type_proto,
        Some(node_ctor),
        false,
    );
    let _ = register_dom_interface(vm, "Attr", p.attr_proto, Some(node_ctor), false);

    // Constructible `new DocumentFragment()` & `ShadowRoot`
    define_to_string_tag(vm, p.fragment_proto, "DocumentFragment");
    let h_df = Rc::clone(host);
    let df_ctor = vm.alloc_host_fn("DocumentFragment", move |vm, _, _| {
        let nid = h_df
            .borrow()
            .document
            .borrow_mut()
            .new_node(NodeData::DocumentFragment);
        Ok(wrap_node(vm, &h_df, nid))
    });
    vm.heap
        .object_set_prototype(df_ctor, Some(node_ctor), &mut vm.shapes);
    vm.heap.object_define_data(
        df_ctor,
        vm.key_prototype,
        JsValue::from_object(p.fragment_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        p.fragment_proto,
        vm.key_constructor,
        JsValue::from_object(df_ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.mark_constructor(df_ctor);
    define_global(vm, "DocumentFragment", JsValue::from_object(df_ctor));

    let _ = register_dom_interface(vm, "ShadowRoot", p.shadow_root_proto, Some(df_ctor), false);

    // Constructible `new Document()`, plus `HTMLDocument` and `XMLDocument`
    define_to_string_tag(vm, p.document_proto, "Document");
    let h_doc = Rc::clone(host);
    let doc_ctor = vm.alloc_host_fn("Document", move |vm, _, _| {
        let nid = h_doc
            .borrow()
            .document
            .borrow_mut()
            .new_node(NodeData::Document);
        h_doc
            .borrow_mut()
            .doc_meta
            .insert(nid, DocumentMeta::xml_generic("about:blank"));
        Ok(wrap_node(vm, &h_doc, nid))
    });
    vm.heap
        .object_set_prototype(doc_ctor, Some(node_ctor), &mut vm.shapes);
    vm.heap.object_define_data(
        doc_ctor,
        vm.key_prototype,
        JsValue::from_object(p.document_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        p.document_proto,
        vm.key_constructor,
        JsValue::from_object(doc_ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.mark_constructor(doc_ctor);
    define_global(vm, "Document", JsValue::from_object(doc_ctor));

    let _ = register_dom_interface(
        vm,
        "HTMLDocument",
        p.html_document_proto,
        Some(doc_ctor),
        false,
    );
    let _ = register_dom_interface(
        vm,
        "XMLDocument",
        p.xml_document_proto,
        Some(doc_ctor),
        false,
    );

    // Collection & Utility interfaces
    let _ = register_dom_interface(
        vm,
        "DOMImplementation",
        p.dom_implementation_proto,
        None,
        false,
    );
    let _ = register_dom_interface(vm, "DOMTokenList", p.dom_token_list_proto, None, false);
    let _ = register_dom_interface(vm, "NamedNodeMap", p.named_node_map_proto, None, false);
    let _ = register_dom_interface(vm, "NodeList", p.node_list_proto, None, false);
    let _ = register_dom_interface(vm, "HTMLCollection", p.html_collection_proto, None, false);
    let _ = register_dom_interface(vm, "DOMStringMap", p.dom_string_map_proto, None, false);
    let _ = register_dom_interface(vm, "TreeWalker", p.tree_walker_proto, None, false);
    let _ = register_dom_interface(vm, "NodeIterator", p.node_iterator_proto, None, false);
    let mr_proto = make_proto(vm, vm.object_proto);
    let _ = register_dom_interface(vm, "MutationRecord", mr_proto, None, false);

    // Install prototype methods & accessors for all these interfaces!
    install_character_data_and_subclasses(vm, host);
    install_document_type_prototype(vm, host);
    install_dom_implementation_prototype(vm, host);
    install_extra_node_methods(vm, host);
    crate::dom_collections::install_collections_and_traversal(vm, host);
}

// ---------------------------------------------------------------------------
// CharacterData, Text, CDATASection, Comment, ProcessingInstruction
// ---------------------------------------------------------------------------

/// Slices/replaces a UTF-8 string using UTF-16 code-unit `(offset, count)` per WHATWG DOM §4.10.
fn utf16_replace(s: &str, offset: usize, count: usize, replacement: &str) -> Option<String> {
    let units: Vec<u16> = s.encode_utf16().collect();
    if offset > units.len() {
        return None;
    }
    let end = offset.saturating_add(count).min(units.len());
    let mut out = Vec::with_capacity(units.len() - (end - offset) + replacement.len());
    out.extend_from_slice(&units[..offset]);
    out.extend(replacement.encode_utf16());
    out.extend_from_slice(&units[end..]);
    Some(String::from_utf16_lossy(&out))
}

fn utf16_substring(s: &str, offset: usize, count: usize) -> Option<String> {
    let units: Vec<u16> = s.encode_utf16().collect();
    if offset > units.len() {
        return None;
    }
    let end = offset.saturating_add(count).min(units.len());
    Some(String::from_utf16_lossy(&units[offset..end]))
}

/// Performs WHATWG DOM §4.10 "replace data" with `IndexSizeError`, `MutationObserver` emit,
/// and live `Range` adjustment.
pub fn character_data_replace(
    vm: &mut JsVm,
    host: &SharedHost,
    nid: NodeId,
    offset: usize,
    count: usize,
    data: &str,
) -> Result<(), JsException> {
    let (old_data, eff_count, parent_opt) = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = doc_rc.borrow_mut();
        let parent_opt = doc.parent(nid);
        let cur = match doc.get(nid) {
            Some(
                NodeData::Text(s)
                | NodeData::CDataSection(s)
                | NodeData::Comment(s)
                | NodeData::ProcessingInstruction { data: s, .. },
            ) => s.clone(),
            _ => {
                return Err(vm.type_error("Illegal invocation: receiver is not CharacterData"));
            }
        };
        let cur_u16_len = cur.encode_utf16().count();
        if offset > cur_u16_len {
            return Err(create_dom_exception(
                vm,
                "IndexSizeError",
                "The offset is greater than the node's length",
            ));
        }
        let eff_count = count.min(cur_u16_len - offset);
        let replaced = utf16_replace(&cur, offset, eff_count, data).unwrap_or_default();
        if let Some(
            NodeData::Text(s)
            | NodeData::CDataSection(s)
            | NodeData::Comment(s)
            | NodeData::ProcessingInstruction { data: s, .. },
        ) = doc.get_mut(nid)
        {
            *s = replaced;
        }
        (cur, eff_count, parent_opt)
    };

    let inserted_len = data.encode_utf16().count();
    crate::selection::adjust_ranges_for_replace_data(host, nid, offset, eff_count, inserted_len);

    if let Some(parent) = parent_opt {
        host.borrow_mut().invalidate_dom_mutation(parent, None);
    }
    host.borrow().emit_mutation(DomMutation::CharacterData {
        target: nid,
        old_value: old_data,
    });
    Ok(())
}

pub(crate) fn to_webidl_u32(vm: &mut JsVm, val: JsValue) -> Result<usize, JsException> {
    let n = vm.to_number(val)?;
    if !n.is_finite() || n == 0.0 {
        return Ok(0);
    }
    let truncated = n.trunc();
    let wrapped = truncated.rem_euclid(4_294_967_296.0);
    Ok(wrapped as u32 as usize)
}

fn install_character_data_and_subclasses(vm: &mut JsVm, host: &SharedHost) {
    let cdp = host.borrow().prototypes.character_data_proto;
    let tp = host.borrow().prototypes.text_proto;
    let pip = host.borrow().prototypes.processing_instruction_proto;

    install_child_node_mixin(vm, host, cdp);

    // CharacterData.prototype.data (getter + setter)
    let h1 = Rc::clone(host);
    let g_data = vm.alloc_host_fn("get data", move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        let text = match h1.borrow().document.borrow().get(nid) {
            Some(
                NodeData::Text(s)
                | NodeData::CDataSection(s)
                | NodeData::Comment(s)
                | NodeData::ProcessingInstruction { data: s, .. },
            ) => s.clone(),
            _ => return Err(vm.type_error("Illegal invocation: receiver is not CharacterData")),
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(text)))
    });
    let h2 = Rc::clone(host);
    let s_data = vm.alloc_host_fn("set data", move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let new_val = if arg(args, 0).is_null() {
            String::new()
        } else {
            vm.value_to_rust_string(arg(args, 0))
        };
        let cur_len = match h2.borrow().document.borrow().get(nid) {
            Some(
                NodeData::Text(s)
                | NodeData::CDataSection(s)
                | NodeData::Comment(s)
                | NodeData::ProcessingInstruction { data: s, .. },
            ) => s.encode_utf16().count(),
            _ => return Err(vm.type_error("Illegal invocation: receiver is not CharacterData")),
        };
        character_data_replace(vm, &h2, nid, 0, cur_len, &new_val)?;
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, cdp, "data", Some(g_data), Some(s_data));

    // CharacterData.prototype.length
    let h = Rc::clone(host);
    let g_len = vm.alloc_host_fn("get length", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let len = match h.borrow().document.borrow().get(nid) {
            Some(
                NodeData::Text(s)
                | NodeData::CDataSection(s)
                | NodeData::Comment(s)
                | NodeData::ProcessingInstruction { data: s, .. },
            ) => s.encode_utf16().count(),
            _ => return Err(vm.type_error("Illegal invocation: receiver is not CharacterData")),
        };
        Ok(JsValue::from_f64(len as f64))
    });
    define_host_accessor(vm, cdp, "length", Some(g_len), None);

    // NonDocumentTypeChildNode mixin on CharacterData: previousElementSibling / nextElementSibling
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get previousElementSibling", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let c = h.borrow().document.borrow().previous_element_sibling(nid);
        Ok(c.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, cdp, "previousElementSibling", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get nextElementSibling", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let c = h.borrow().document.borrow().next_element_sibling(nid);
        Ok(c.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, cdp, "nextElementSibling", Some(g), None);

    // substringData(offset, count)
    let h = Rc::clone(host);
    define_host_method(vm, cdp, "substringData", move |vm, t, args| {
        if args.len() < 2 {
            return Err(vm.type_error("substringData requires 2 arguments"));
        }
        let nid = require_node(vm, &h, t)?;
        let offset = to_webidl_u32(vm, args[0])?;
        let count = to_webidl_u32(vm, args[1])?;
        let res = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(nid) {
                Some(
                    NodeData::Text(s)
                    | NodeData::CDataSection(s)
                    | NodeData::Comment(s)
                    | NodeData::ProcessingInstruction { data: s, .. },
                ) => utf16_substring(s, offset, count),
                _ => {
                    return Err(vm.type_error("Illegal invocation: receiver is not CharacterData"));
                }
            }
        };
        match res {
            Some(sub) => Ok(JsValue::from_string(vm.heap.alloc_string(sub))),
            None => Err(create_dom_exception(
                vm,
                "IndexSizeError",
                "The offset is greater than the node's length",
            )),
        }
    });

    // appendData(data)
    let h = Rc::clone(host);
    define_host_method(vm, cdp, "appendData", move |vm, t, args| {
        if args.is_empty() {
            return Err(vm.type_error("appendData requires 1 argument"));
        }
        let nid = require_node(vm, &h, t)?;
        let data = vm.value_to_rust_string(args[0]);
        let len = match h.borrow().document.borrow().get(nid) {
            Some(
                NodeData::Text(s)
                | NodeData::CDataSection(s)
                | NodeData::Comment(s)
                | NodeData::ProcessingInstruction { data: s, .. },
            ) => s.encode_utf16().count(),
            _ => return Err(vm.type_error("Illegal invocation: receiver is not CharacterData")),
        };
        character_data_replace(vm, &h, nid, len, 0, &data)?;
        Ok(JsValue::UNDEFINED)
    });

    // insertData(offset, data)
    let h = Rc::clone(host);
    define_host_method(vm, cdp, "insertData", move |vm, t, args| {
        if args.len() < 2 {
            return Err(vm.type_error("insertData requires 2 arguments"));
        }
        let nid = require_node(vm, &h, t)?;
        let offset = to_webidl_u32(vm, args[0])?;
        let data = vm.value_to_rust_string(args[1]);
        character_data_replace(vm, &h, nid, offset, 0, &data)?;
        Ok(JsValue::UNDEFINED)
    });

    // deleteData(offset, count)
    let h = Rc::clone(host);
    define_host_method(vm, cdp, "deleteData", move |vm, t, args| {
        if args.len() < 2 {
            return Err(vm.type_error("deleteData requires 2 arguments"));
        }
        let nid = require_node(vm, &h, t)?;
        let offset = to_webidl_u32(vm, args[0])?;
        let count = to_webidl_u32(vm, args[1])?;
        character_data_replace(vm, &h, nid, offset, count, "")?;
        Ok(JsValue::UNDEFINED)
    });

    // replaceData(offset, count, data)
    let h = Rc::clone(host);
    define_host_method(vm, cdp, "replaceData", move |vm, t, args| {
        if args.len() < 3 {
            return Err(vm.type_error("replaceData requires 3 arguments"));
        }
        let nid = require_node(vm, &h, t)?;
        let offset = to_webidl_u32(vm, args[0])?;
        let count = to_webidl_u32(vm, args[1])?;
        let data = vm.value_to_rust_string(args[2]);
        character_data_replace(vm, &h, nid, offset, count, &data)?;
        Ok(JsValue::UNDEFINED)
    });

    // Text.prototype.splitText(offset) (WHATWG DOM §4.11)
    let h = Rc::clone(host);
    define_host_method(vm, tp, "splitText", move |vm, t, args| {
        if args.is_empty() {
            return Err(vm.type_error("splitText requires 1 argument"));
        }
        let nid = require_node(vm, &h, t)?;
        let offset = to_webidl_u32(vm, args[0])?;
        let (tail, count, parent_opt, next_sib, node_index, owner_doc) = {
            let hb = h.borrow();
            let doc = hb.document.borrow();
            let s = match doc.get(nid) {
                Some(NodeData::Text(s) | NodeData::CDataSection(s)) => s.clone(),
                _ => return Err(vm.type_error("Illegal invocation: receiver is not Text")),
            };
            let len = s.encode_utf16().count();
            if offset > len {
                return Err(create_dom_exception(
                    vm,
                    "IndexSizeError",
                    "The offset is greater than the text node's length",
                ));
            }
            let count = len - offset;
            let tail = utf16_substring(&s, offset, count).unwrap_or_default();
            let parent_opt = doc.parent(nid);
            let next_sib = doc.next_sibling(nid);
            let node_index = parent_opt
                .and_then(|p| doc.children(p).position(|c| c == nid))
                .unwrap_or(0);
            let owner_doc = hb.owner_doc_of(nid);
            (tail, count, parent_opt, next_sib, node_index, owner_doc)
        };

        // 1. Create new Text node with `tail` and same ownerDocument.
        let new_nid = h.borrow().document.borrow_mut().create_text(&tail);
        h.borrow_mut().set_owner_doc(new_nid, owner_doc);

        // 2. If parent is non-null: insert newNode after node, adjust ranges per §4.11,
        //    and replace data on original node.
        if let Some(parent) = parent_opt {
            // Insert into tree first.
            {
                let doc_rc = Rc::clone(&h.borrow().document);
                let mut doc = doc_rc.borrow_mut();
                if let Some(ns) = next_sib {
                    doc.insert_before(ns, new_nid);
                } else {
                    doc.append_child(parent, new_nid);
                }
            }
            // Adjust live ranges per WHATWG DOM §4.11 steps 7.2 - 7.5 BEFORE replaceData!
            crate::selection::adjust_ranges_for_split_text(
                &h, nid, new_nid, parent, node_index, offset,
            );
            h.borrow_mut()
                .invalidate_dom_mutation(parent, Some(new_nid));
            h.borrow().emit_mutation(DomMutation::ChildList {
                target: parent,
                added: vec![new_nid],
                removed: Vec::new(),
                previous_sibling: Some(nid),
                next_sibling: next_sib,
            });
        }
        // Step 8: Replace data on `node` with offset, count, "".
        character_data_replace(vm, &h, nid, offset, count, "")?;
        Ok(wrap_node(vm, &h, new_nid))
    });

    // Text.prototype.wholeText
    let h = Rc::clone(host);
    let g_wt = vm.alloc_host_fn("get wholeText", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let whole = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            if !matches!(
                doc.get(nid),
                Some(NodeData::Text(_) | NodeData::CDataSection(_))
            ) {
                return Err(vm.type_error("Illegal invocation: receiver is not Text"));
            }
            let mut first = nid;
            while let Some(prev) = doc.previous_sibling(first) {
                if matches!(
                    doc.get(prev),
                    Some(NodeData::Text(_) | NodeData::CDataSection(_))
                ) {
                    first = prev;
                } else {
                    break;
                }
            }
            let mut out = String::new();
            let mut cur = Some(first);
            while let Some(n) = cur {
                match doc.get(n) {
                    Some(NodeData::Text(s) | NodeData::CDataSection(s)) => {
                        out.push_str(s);
                        cur = doc.next_sibling(n);
                    }
                    _ => break,
                }
            }
            out
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(whole)))
    });
    define_host_accessor(vm, tp, "wholeText", Some(g_wt), None);

    // ProcessingInstruction.prototype.target
    let h = Rc::clone(host);
    let g_tgt = vm.alloc_host_fn("get target", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let target = match h.borrow().document.borrow().get(nid) {
            Some(NodeData::ProcessingInstruction { target, .. }) => target.clone(),
            _ => {
                return Err(
                    vm.type_error("Illegal invocation: receiver is not ProcessingInstruction")
                );
            }
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(target)))
    });
    define_host_accessor(vm, pip, "target", Some(g_tgt), None);
}

// ---------------------------------------------------------------------------
// DocumentType.prototype
// ---------------------------------------------------------------------------

fn install_document_type_prototype(vm: &mut JsVm, host: &SharedHost) {
    let dtp = host.borrow().prototypes.document_type_proto;
    install_child_node_mixin(vm, host, dtp);

    for (prop, idx) in [("name", 0u8), ("publicId", 1), ("systemId", 2)] {
        let h = Rc::clone(host);
        let g = vm.alloc_host_fn(prop, move |vm, t, _| {
            let nid = require_node(vm, &h, t)?;
            let val = match h.borrow().document.borrow().get(nid) {
                Some(NodeData::DocumentType {
                    name,
                    public_id,
                    system_id,
                }) => match idx {
                    0 => name.clone(),
                    1 => public_id.clone(),
                    _ => system_id.clone(),
                },
                _ => return Err(vm.type_error("Illegal invocation: receiver is not DocumentType")),
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(val)))
        });
        define_host_accessor(vm, dtp, prop, Some(g), None);
    }
}

// ---------------------------------------------------------------------------
// DOMImplementation.prototype
// ---------------------------------------------------------------------------

pub fn create_dom_implementation_obj(
    vm: &mut JsVm,
    host: &SharedHost,
    doc_id: NodeId,
) -> HeapHandle {
    if let Some(&existing) = host.borrow().doc_implementations.get(&doc_id) {
        if vm.heap.get_object(existing).is_some() {
            return existing;
        }
    }
    let proto = host.borrow().prototypes.dom_implementation_proto;
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    host.borrow_mut().doc_implementations.insert(doc_id, obj);
    host.borrow_mut().implementation_to_doc.insert(obj, doc_id);
    obj
}

fn install_dom_implementation_prototype(vm: &mut JsVm, host: &SharedHost) {
    let imp_p = host.borrow().prototypes.dom_implementation_proto;

    // hasFeature() always returns true (WHATWG DOM §4.5.1).
    define_host_method(vm, imp_p, "hasFeature", |_vm, _, _| Ok(JsValue::TRUE));

    // createDocumentType(qualifiedName, publicId, systemId)
    let h = Rc::clone(host);
    define_host_method(
        vm,
        imp_p,
        "createDocumentType",
        move |vm, this_val, args| {
            if args.len() < 3 {
                return Err(vm.type_error("createDocumentType requires 3 arguments"));
            }
            let doc_id = this_val
                .as_object()
                .and_then(|oh| h.borrow().implementation_to_doc.get(&oh).copied())
                .unwrap_or_else(|| h.borrow().document.borrow().root());
            let name = vm.value_to_rust_string(args[0]);
            let pub_id = vm.value_to_rust_string(args[1]);
            let sys_id = vm.value_to_rust_string(args[2]);
            if !is_valid_doctype_name(&name) {
                return Err(create_dom_exception(
                    vm,
                    "InvalidCharacterError",
                    "The doctype name contains an invalid character",
                ));
            }
            let nid = h
                .borrow()
                .document
                .borrow_mut()
                .new_node(NodeData::DocumentType {
                    name,
                    public_id: pub_id,
                    system_id: sys_id,
                });
            h.borrow_mut().set_owner_doc(nid, doc_id);
            Ok(wrap_node(vm, &h, nid))
        },
    );

    // createHTMLDocument(title?)
    let h = Rc::clone(host);
    define_host_method(vm, imp_p, "createHTMLDocument", move |vm, _, args| {
        let title = if args.first().is_some_and(|v| !v.is_undefined()) {
            Some(vm.value_to_rust_string(args[0]))
        } else {
            None
        };
        let sub_root = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            let r = doc.new_node(NodeData::Document);
            let dt = doc.new_node(NodeData::DocumentType {
                name: "html".to_string(),
                public_id: String::new(),
                system_id: String::new(),
            });
            doc.append_child(r, dt);
            let html_el = doc.create_element("html");
            doc.append_child(r, html_el);
            let head_el = doc.create_element("head");
            doc.append_child(html_el, head_el);
            if let Some(t) = title {
                let title_el = doc.create_element("title");
                doc.append_child(head_el, title_el);
                if !t.is_empty() {
                    let tn = doc.create_text(&t);
                    doc.append_child(title_el, tn);
                }
            }
            let body_el = doc.create_element("body");
            doc.append_child(html_el, body_el);
            r
        };
        {
            let mut hb = h.borrow_mut();
            hb.doc_meta
                .insert(sub_root, DocumentMeta::html("about:blank"));
            let all_kids: Vec<NodeId> = hb.document.borrow().descendants(sub_root).collect();
            for n in all_kids {
                if n != sub_root {
                    hb.owner_docs.insert(n, sub_root);
                }
            }
        }
        Ok(wrap_node(vm, &h, sub_root))
    });

    // createDocument(namespace, qualifiedName, doctype = null)
    let h = Rc::clone(host);
    define_host_method(vm, imp_p, "createDocument", move |vm, _, args| {
        if args.len() < 2 {
            return Err(vm.type_error("createDocument requires at least 2 arguments"));
        }
        let ns_arg = args[0];
        let ns = if ns_arg.is_null() || ns_arg.is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(ns_arg)
        };
        let qn_arg = args[1];
        let qname = if qn_arg.is_null() {
            String::new()
        } else {
            vm.value_to_rust_string(qn_arg)
        };
        let dt_arg = arg(args, 2);
        let doctype_id = if dt_arg.is_null() || dt_arg.is_undefined() {
            None
        } else {
            let Some(nid) = h.borrow().node_for_value(vm, dt_arg) else {
                return Err(vm.type_error("Argument 3 to createDocument is not a DocumentType"));
            };
            let is_dt = matches!(
                h.borrow().document.borrow().get(nid),
                Some(NodeData::DocumentType { .. })
            );
            if !is_dt {
                return Err(vm.type_error("Argument 3 to createDocument is not a DocumentType"));
            }
            Some(nid)
        };

        // Validate qualifiedName BEFORE creating document if non-empty.
        let root_elem_spec = if !qname.is_empty() {
            Some(validate_and_extract(vm, &ns, &qname, true)?)
        } else {
            None
        };

        let content_type = match ns.as_str() {
            HTML_NS => "application/xhtml+xml",
            SVG_NS => "image/svg+xml",
            _ => "application/xml",
        };
        let doc_nid = h
            .borrow()
            .document
            .borrow_mut()
            .new_node(NodeData::Document);
        h.borrow_mut().doc_meta.insert(
            doc_nid,
            DocumentMeta::xml_document(content_type, "about:blank"),
        );

        if let Some(dt) = doctype_id {
            // Detach from any previous parent and adopt into new XMLDocument.
            {
                let doc_rc = Rc::clone(&h.borrow().document);
                let mut doc = doc_rc.borrow_mut();
                if doc.parent(dt).is_some() {
                    doc.remove_child(dt);
                }
                doc.append_child(doc_nid, dt);
            }
            h.borrow_mut().set_owner_doc(dt, doc_nid);
        }

        if let Some((el_ns, prefix, local)) = root_elem_spec {
            let el_nid = h.borrow().document.borrow_mut().create_element_ns(
                &el_ns,
                prefix.as_deref(),
                &local,
            );
            h.borrow_mut().set_owner_doc(el_nid, doc_nid);
            h.borrow()
                .document
                .borrow_mut()
                .append_child(doc_nid, el_nid);
        }

        Ok(wrap_node(vm, &h, doc_nid))
    });
}

// ---------------------------------------------------------------------------

/// WHATWG DOM §4.2.4 "ensure pre-insertion validity" (parent, node, child).
pub fn validate_pre_insert(
    vm: &mut JsVm,
    doc: &Document,
    parent: NodeId,
    node: NodeId,
    before: Option<NodeId>,
) -> Result<(), JsException> {
    // Step 1: parent must be Document, DocumentFragment, or Element.
    if !matches!(
        doc.get(parent),
        Some(
            NodeData::Document
                | NodeData::DocumentFragment
                | NodeData::ShadowRoot { .. }
                | NodeData::Element(_)
        )
    ) {
        return Err(create_dom_exception(
            vm,
            "HierarchyRequestError",
            "This node type does not support children",
        ));
    }
    // Step 2: node must not be a host-including inclusive ancestor of parent.
    if is_host_including_inclusive_ancestor(doc, node, parent) {
        return Err(create_dom_exception(
            vm,
            "HierarchyRequestError",
            "The new child node is an ancestor of the parent",
        ));
    }
    // Step 3: if child is non-null and child's parent != parent -> NotFoundError.
    if let Some(ref_n) = before {
        if doc.parent(ref_n) != Some(parent) {
            return Err(create_dom_exception(
                vm,
                "NotFoundError",
                "The reference node is not a child of this parent",
            ));
        }
    }
    // Step 4: node must be DocumentFragment, DocumentType, Element, or CharacterData.
    if matches!(
        doc.get(node),
        Some(NodeData::Document | NodeData::ShadowRoot { .. })
    ) {
        return Err(create_dom_exception(
            vm,
            "HierarchyRequestError",
            "Cannot insert a Document or ShadowRoot node",
        ));
    }
    let parent_is_doc = matches!(doc.get(parent), Some(NodeData::Document));
    // Step 5: Text cannot be inserted into Document; DocumentType can only be inserted into Document.
    match doc.get(node) {
        Some(NodeData::Text(_) | NodeData::CDataSection(_)) if parent_is_doc => {
            return Err(create_dom_exception(
                vm,
                "HierarchyRequestError",
                "Cannot insert a Text node into a Document",
            ));
        }
        Some(NodeData::DocumentType { .. }) if !parent_is_doc => {
            return Err(create_dom_exception(
                vm,
                "HierarchyRequestError",
                "DocumentType can only be inserted into a Document",
            ));
        }
        _ => {}
    }
    // Step 6: Document parent constraints.
    if parent_is_doc {
        let has_existing_elem = doc
            .children(parent)
            .any(|c| c != node && matches!(doc.get(c), Some(NodeData::Element(_))));
        let has_existing_dt = doc
            .children(parent)
            .any(|c| c != node && matches!(doc.get(c), Some(NodeData::DocumentType { .. })));
        let dt_following_before = before.is_some_and(|b| {
            let mut cur = doc.next_sibling(b);
            while let Some(s) = cur {
                if matches!(doc.get(s), Some(NodeData::DocumentType { .. })) {
                    return true;
                }
                cur = doc.next_sibling(s);
            }
            false
        });
        let elem_preceding_before = || {
            for c in doc.children(parent) {
                if Some(c) == before {
                    break;
                }
                if matches!(doc.get(c), Some(NodeData::Element(_))) {
                    return true;
                }
            }
            false
        };
        match doc.get(node) {
            Some(NodeData::DocumentFragment) => {
                let mut frag_elems = 0usize;
                for c in doc.children(node) {
                    if matches!(
                        doc.get(c),
                        Some(NodeData::Text(_) | NodeData::CDataSection(_))
                    ) {
                        return Err(create_dom_exception(
                            vm,
                            "HierarchyRequestError",
                            "Cannot insert Text into a Document",
                        ));
                    }
                    if matches!(doc.get(c), Some(NodeData::Element(_))) {
                        frag_elems += 1;
                    }
                }
                if frag_elems > 1
                    || (frag_elems == 1
                        && (has_existing_elem
                            || before.is_some_and(|b| {
                                matches!(doc.get(b), Some(NodeData::DocumentType { .. }))
                            })
                            || dt_following_before))
                {
                    return Err(create_dom_exception(
                        vm,
                        "HierarchyRequestError",
                        "Document can only contain one Element child after any DocumentType",
                    ));
                }
            }
            Some(NodeData::Element(_)) => {
                if has_existing_elem
                    || before
                        .is_some_and(|b| matches!(doc.get(b), Some(NodeData::DocumentType { .. })))
                    || dt_following_before
                {
                    return Err(create_dom_exception(
                        vm,
                        "HierarchyRequestError",
                        "Document already has an Element child or DocumentType follows",
                    ));
                }
            }
            Some(NodeData::DocumentType { .. }) if has_existing_dt || elem_preceding_before() => {
                return Err(create_dom_exception(
                    vm,
                    "HierarchyRequestError",
                    "Document already has a DocumentType or Element precedes insertion point",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn is_host_including_inclusive_ancestor(doc: &Document, a: NodeId, b: NodeId) -> bool {
    let mut cur = Some(b);
    for _ in 0..4096 {
        let Some(n) = cur else {
            return false;
        };
        if n == a {
            return true;
        }
        cur = match doc.parent(n) {
            Some(p) => Some(p),
            None => match doc.get(n) {
                Some(NodeData::ShadowRoot { host, .. }) => Some(*host),
                _ => None,
            },
        };
    }
    false
}

/// Tree-order comparison of two nodes for `Node.compareDocumentPosition` (WHATWG DOM §4.4).
pub fn compare_document_position(doc: &Document, this_node: NodeId, other: NodeId) -> i32 {
    if this_node == other {
        return 0;
    }
    let root_a = root_of(doc, this_node);
    let root_b = root_of(doc, other);
    if root_a != root_b {
        let order_bit = if usize::from(this_node) < usize::from(other) {
            0x04 // FOLLOWING
        } else {
            0x02 // PRECEDING
        };
        return 0x01 | 0x20 | order_bit;
    }
    if doc.ancestors(other).any(|a| a == this_node) {
        return 0x10 | 0x04; // CONTAINED_BY | FOLLOWING
    }
    if doc.ancestors(this_node).any(|a| a == other) {
        return 0x08 | 0x02; // CONTAINS | PRECEDING
    }
    // Walk ancestors to find lowest common ancestor.
    let anc_a: Vec<NodeId> = doc.ancestors(this_node).collect();
    let anc_b: Vec<NodeId> = doc.ancestors(other).collect();
    let mut ia = anc_a.len();
    let mut ib = anc_b.len();
    while ia > 0 && ib > 0 && anc_a[ia - 1] == anc_b[ib - 1] {
        ia -= 1;
        ib -= 1;
    }
    if ia > 0 && ib > 0 {
        let lca = anc_a[ia];
        let child_a = anc_a[ia - 1];
        let child_b = anc_b[ib - 1];
        for c in doc.children(lca) {
            if c == child_a {
                return 0x04; // FOLLOWING (other follows this)
            }
            if c == child_b {
                return 0x02; // PRECEDING (other precedes this)
            }
        }
    }
    0x04
}

fn root_of(doc: &Document, mut n: NodeId) -> NodeId {
    while let Some(p) = doc.parent(n) {
        n = p;
    }
    n
}

fn nodes_are_equal(doc: &Document, a: NodeId, b: NodeId) -> bool {
    let (Some(da), Some(db)) = (doc.get(a), doc.get(b)) else {
        return false;
    };
    let same_node = match (da, db) {
        (NodeData::Document, NodeData::Document)
        | (NodeData::DocumentFragment, NodeData::DocumentFragment) => true,
        (
            NodeData::DocumentType {
                name: n1,
                public_id: p1,
                system_id: s1,
            },
            NodeData::DocumentType {
                name: n2,
                public_id: p2,
                system_id: s2,
            },
        ) => n1 == n2 && p1 == p2 && s1 == s2,
        (NodeData::Text(t1), NodeData::Text(t2))
        | (NodeData::CDataSection(t1), NodeData::CDataSection(t2))
        | (NodeData::Comment(t1), NodeData::Comment(t2)) => t1 == t2,
        (
            NodeData::ProcessingInstruction {
                target: t1,
                data: d1,
            },
            NodeData::ProcessingInstruction {
                target: t2,
                data: d2,
            },
        ) => t1 == t2 && d1 == d2,
        (NodeData::Element(e1), NodeData::Element(e2)) => {
            e1.name.ns == e2.name.ns
                && e1.name.prefix == e2.name.prefix
                && e1.name.local == e2.name.local
                && e1.attributes.len() == e2.attributes.len()
                && e1.attributes.iter().all(|(q1, v1)| {
                    e2.attributes
                        .iter()
                        .any(|(q2, v2)| q1.ns == q2.ns && q1.local == q2.local && v1 == v2)
                })
        }
        _ => false,
    };
    if !same_node {
        return false;
    }
    let ka: Vec<NodeId> = doc.children(a).collect();
    let kb: Vec<NodeId> = doc.children(b).collect();
    if ka.len() != kb.len() {
        return false;
    }
    ka.into_iter()
        .zip(kb)
        .all(|(ca, cb)| nodes_are_equal(doc, ca, cb))
}

fn install_extra_node_methods(vm: &mut JsVm, host: &SharedHost) {
    let np = host.borrow().prototypes.node_proto;

    // hasChildNodes()
    let h = Rc::clone(host);
    define_host_method(vm, np, "hasChildNodes", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let has = h.borrow().document.borrow().first_child(nid).is_some();
        Ok(JsValue::from_bool(has))
    });

    // isSameNode(other)
    let h = Rc::clone(host);
    define_host_method(vm, np, "isSameNode", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let other = h.borrow().node_for_value(vm, arg(args, 0));
        Ok(JsValue::from_bool(other == Some(nid)))
    });

    // isEqualNode(other)
    let h = Rc::clone(host);
    define_host_method(vm, np, "isEqualNode", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let other_val = arg(args, 0);
        if other_val.is_null() || other_val.is_undefined() {
            return Ok(JsValue::FALSE);
        }
        let Some(other) = h.borrow().node_for_value(vm, other_val) else {
            return Err(vm.type_error("isEqualNode argument must be a Node or null"));
        };
        let eq = nodes_are_equal(&h.borrow().document.borrow(), nid, other);
        Ok(JsValue::from_bool(eq))
    });

    // compareDocumentPosition(other)
    let h = Rc::clone(host);
    define_host_method(vm, np, "compareDocumentPosition", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let other = require_node(vm, &h, arg(args, 0))?;
        let bits = compare_document_position(&h.borrow().document.borrow(), nid, other);
        Ok(JsValue::from_i32(bits))
    });

    // getRootNode(options?)
    let h = Rc::clone(host);
    define_host_method(vm, np, "getRootNode", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let composed = arg(args, 0).as_object().is_some_and(|oh| {
            let k = vm.heap.intern_key("composed");
            vm.get_property_by_key(oh, k)
                .is_ok_and(|v| vm.to_boolean(v))
        });
        let root = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            let mut cur = nid;
            for _ in 0..4096 {
                if let Some(p) = doc.parent(cur) {
                    cur = p;
                } else if composed {
                    if let Some(NodeData::ShadowRoot { host: sh, .. }) = doc.get(cur) {
                        cur = *sh;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            cur
        };
        Ok(wrap_node(vm, &h, root))
    });

    // baseURI (WHATWG HTML §2.5.3: document base URL honoring first `<base href>`)
    let h = Rc::clone(host);
    let g_base = vm.alloc_host_fn("get baseURI", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let url = {
            let hb = h.borrow();
            let doc_id = {
                let doc = hb.document.borrow();
                if matches!(doc.get(nid), Some(NodeData::Document)) {
                    nid
                } else {
                    hb.owner_doc_of(nid)
                }
            };
            let doc_url = hb.doc_url_for_node(doc_id);
            let base_href = {
                let doc = hb.document.borrow();
                let found = doc.descendants(doc_id).find_map(|d| {
                    if let Some(NodeData::Element(el)) = doc.get(d) {
                        if &*el.name.ns == HTML_NS && el.name.local.as_str() == "base" {
                            return el.get_attribute("href").map(ToString::to_string);
                        }
                    }
                    None
                });
                found
            };
            if let Some(href) = base_href {
                url::Url::parse(&doc_url)
                    .ok()
                    .and_then(|base| base.join(&href).ok())
                    .map(|u| u.to_string())
                    .unwrap_or(doc_url)
            } else {
                doc_url
            }
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(url)))
    });
    define_host_accessor(vm, np, "baseURI", Some(g_base), None);

    // Node.prototype.normalize() (WHATWG DOM §4.4 — exclusive Text nodes only!)
    let h = Rc::clone(host);
    define_host_method(vm, np, "normalize", move |vm, t, _| {
        let root_nid = require_node(vm, &h, t)?;
        normalize_subtree(vm, &h, root_nid)?;
        Ok(JsValue::UNDEFINED)
    });

    // lookupNamespaceURI / lookupPrefix / isDefaultNamespace (WHATWG DOM §4.4)
    let h = Rc::clone(host);
    define_host_method(vm, np, "lookupNamespaceURI", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let pref_arg = arg(args, 0);
        let pref = if pref_arg.is_null() || pref_arg.is_undefined() {
            None
        } else {
            let s = vm.value_to_rust_string(pref_arg);
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };
        let found = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            lookup_ns_uri(&doc, nid, pref.as_deref())
        };
        Ok(found.map_or(JsValue::NULL, |s| {
            JsValue::from_string(vm.heap.alloc_string(s))
        }))
    });

    let h = Rc::clone(host);
    define_host_method(vm, np, "lookupPrefix", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let ns_arg = arg(args, 0);
        if ns_arg.is_null() || ns_arg.is_undefined() {
            return Ok(JsValue::NULL);
        }
        let ns = vm.value_to_rust_string(ns_arg);
        if ns.is_empty() {
            return Ok(JsValue::NULL);
        }
        let found = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            lookup_prefix(&doc, nid, &ns)
        };
        Ok(found.map_or(JsValue::NULL, |s| {
            JsValue::from_string(vm.heap.alloc_string(s))
        }))
    });

    let h = Rc::clone(host);
    define_host_method(vm, np, "isDefaultNamespace", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let ns_arg = arg(args, 0);
        let ns = if ns_arg.is_null() || ns_arg.is_undefined() {
            None
        } else {
            let s = vm.value_to_rust_string(ns_arg);
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };
        let default_ns = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            lookup_ns_uri(&doc, nid, None)
        };
        Ok(JsValue::from_bool(default_ns == ns))
    });
}

fn normalize_subtree(vm: &mut JsVm, host: &SharedHost, root: NodeId) -> Result<(), JsException> {
    let mut stack = vec![root];
    while let Some(parent) = stack.pop() {
        let mut child_opt = host.borrow().document.borrow().first_child(parent);
        let mut child_elements = Vec::new();
        while let Some(node) = child_opt {
            let (is_exclusive_text, text_len, next_sib) = {
                let doc_rc = Rc::clone(&host.borrow().document);
                let doc = doc_rc.borrow();
                let next_sib = doc.next_sibling(node);
                match doc.get(node) {
                    Some(NodeData::Text(s)) => (true, s.encode_utf16().count(), next_sib),
                    _ => (false, 0, next_sib),
                }
            };
            if !is_exclusive_text {
                child_elements.push(node);
                child_opt = next_sib;
                continue;
            }
            if text_len == 0 {
                // Remove empty exclusive Text node.
                let (prev_s, next_s, idx) = {
                    let doc_rc = Rc::clone(&host.borrow().document);
                    let doc = doc_rc.borrow();
                    let idx = doc.children(parent).position(|c| c == node).unwrap_or(0);
                    (doc.previous_sibling(node), doc.next_sibling(node), idx)
                };
                crate::selection::adjust_ranges_for_remove_node(host, node, parent, idx);
                host.borrow().document.borrow_mut().remove_child(node);
                host.borrow_mut().invalidate_dom_mutation(parent, None);
                host.borrow().emit_mutation(DomMutation::ChildList {
                    target: parent,
                    added: Vec::new(),
                    removed: vec![node],
                    previous_sibling: prev_s,
                    next_sibling: next_s,
                });
                child_opt = next_sib;
                continue;
            }

            // Collect all contiguous following exclusive Text nodes.
            let mut following: Vec<(NodeId, String)> = Vec::new();
            let mut scan = next_sib;
            while let Some(sn) = scan {
                let (data_opt, after) = {
                    let doc_rc = Rc::clone(&host.borrow().document);
                    let doc = doc_rc.borrow();
                    let after = doc.next_sibling(sn);
                    match doc.get(sn) {
                        Some(NodeData::Text(s)) => (Some(s.clone()), after),
                        _ => (None, after),
                    }
                };
                if let Some(s) = data_opt {
                    following.push((sn, s));
                    scan = after;
                } else {
                    break;
                }
            }
            if !following.is_empty() {
                let mut combined = String::new();
                for (_, s) in &following {
                    combined.push_str(s);
                }
                // Replace data at end of `node` (appends combined text).
                character_data_replace(vm, host, node, text_len, 0, &combined)?;

                let mut current_len = text_len;
                for (rem_nid, rem_s) in following {
                    let rem_u16 = rem_s.encode_utf16().count();
                    let (prev_s, next_s, rem_idx) = {
                        let doc_rc = Rc::clone(&host.borrow().document);
                        let doc = doc_rc.borrow();
                        let idx = doc.children(parent).position(|c| c == rem_nid).unwrap_or(0);
                        (
                            doc.previous_sibling(rem_nid),
                            doc.next_sibling(rem_nid),
                            idx,
                        )
                    };
                    crate::selection::adjust_ranges_for_normalize_merge(
                        host,
                        node,
                        rem_nid,
                        parent,
                        rem_idx,
                        current_len,
                    );
                    current_len += rem_u16;
                    host.borrow().document.borrow_mut().remove_child(rem_nid);
                    host.borrow_mut().invalidate_dom_mutation(parent, None);
                    host.borrow().emit_mutation(DomMutation::ChildList {
                        target: parent,
                        added: Vec::new(),
                        removed: vec![rem_nid],
                        previous_sibling: prev_s,
                        next_sibling: next_s,
                    });
                }
            }
            child_opt = scan;
        }
        for c in child_elements.into_iter().rev() {
            stack.push(c);
        }
    }
    Ok(())
}

fn lookup_ns_uri(doc: &Document, node: NodeId, prefix: Option<&str>) -> Option<String> {
    match doc.get(node) {
        Some(NodeData::Element(e)) => {
            if prefix == Some("xml") {
                return Some(XML_NS.to_string());
            }
            if prefix == Some("xmlns") {
                return Some(XMLNS_NS.to_string());
            }
            if !e.name.ns.is_empty() && e.name.prefix.as_deref() == prefix {
                return Some(e.name.ns.to_string());
            }
            for (q, val) in &e.attributes {
                if &*q.ns == XMLNS_NS {
                    if q.prefix.as_deref() == Some("xmlns") && prefix == Some(q.local.as_ref()) {
                        return if val.is_empty() {
                            None
                        } else {
                            Some(val.clone())
                        };
                    }
                    if q.prefix.is_none() && &*q.local == "xmlns" && prefix.is_none() {
                        return if val.is_empty() {
                            None
                        } else {
                            Some(val.clone())
                        };
                    }
                }
            }
            doc.parent(node)
                .filter(|&p| matches!(doc.get(p), Some(NodeData::Element(_))))
                .and_then(|p| lookup_ns_uri(doc, p, prefix))
        }
        Some(NodeData::Document) => doc
            .document_element_of(node)
            .and_then(|de| lookup_ns_uri(doc, de, prefix)),
        Some(
            NodeData::DocumentType { .. }
            | NodeData::DocumentFragment
            | NodeData::ShadowRoot { .. },
        ) => None,
        _ => doc
            .parent(node)
            .filter(|&p| matches!(doc.get(p), Some(NodeData::Element(_))))
            .and_then(|p| lookup_ns_uri(doc, p, prefix)),
    }
}

fn lookup_prefix(doc: &Document, node: NodeId, ns: &str) -> Option<String> {
    match doc.get(node) {
        Some(NodeData::Element(e)) => {
            if &*e.name.ns == ns {
                if let Some(p) = &e.name.prefix {
                    if !p.is_empty() {
                        return Some(p.to_string());
                    }
                }
            }
            for (q, val) in &e.attributes {
                if &*q.ns == XMLNS_NS && q.prefix.as_deref() == Some("xmlns") && val == ns {
                    return Some(q.local.to_string());
                }
            }
            doc.parent(node)
                .filter(|&p| matches!(doc.get(p), Some(NodeData::Element(_))))
                .and_then(|p| lookup_prefix(doc, p, ns))
        }
        Some(NodeData::Document) => doc
            .document_element_of(node)
            .and_then(|de| lookup_prefix(doc, de, ns)),
        Some(
            NodeData::DocumentType { .. }
            | NodeData::DocumentFragment
            | NodeData::ShadowRoot { .. },
        ) => None,
        _ => doc
            .parent(node)
            .filter(|&p| matches!(doc.get(p), Some(NodeData::Element(_))))
            .and_then(|p| lookup_prefix(doc, p, ns)),
    }
}

/// Helper methods added on `HostState` for multi-document ownership & GC sweeping.
impl HostState {
    #[must_use]
    pub fn owner_doc_of(&self, nid: NodeId) -> NodeId {
        self.owner_docs
            .get(&nid)
            .copied()
            .unwrap_or_else(|| self.document.borrow().root())
    }

    pub fn set_owner_doc(&mut self, nid: NodeId, doc_id: NodeId) {
        let main_root = self.document.borrow().root();
        if doc_id == main_root {
            self.owner_docs.remove(&nid);
        } else {
            self.owner_docs.insert(nid, doc_id);
        }
    }

    pub fn adopt_subtree(&mut self, root: NodeId, target_doc: NodeId) {
        let nodes: Vec<NodeId> = self.document.borrow().descendants(root).collect();
        let main_root = self.document.borrow().root();
        for n in nodes {
            if target_doc == main_root {
                self.owner_docs.remove(&n);
            } else {
                self.owner_docs.insert(n, target_doc);
            }
        }
    }

    #[must_use]
    pub fn is_xml_doc(&self, doc_id: NodeId) -> bool {
        self.doc_meta.get(&doc_id).is_some_and(|m| m.is_xml)
    }

    #[must_use]
    pub fn is_xml_doc_for_node(&self, nid: NodeId) -> bool {
        let od = self.owner_doc_of(nid);
        self.is_xml_doc(od)
    }

    #[must_use]
    pub fn doc_url_for_node(&self, nid: NodeId) -> String {
        let doc_id = if matches!(self.document.borrow().get(nid), Some(NodeData::Document)) {
            nid
        } else {
            self.owner_doc_of(nid)
        };
        self.doc_meta
            .get(&doc_id)
            .map(|m| m.url.clone())
            .unwrap_or_else(|| self.main_document_url.clone())
    }

    /// Resolves the exact JS prototype for `node_id` using namespace, tag, and document type.
    #[must_use]
    pub fn proto_for_node(&self, node_id: NodeId) -> HeapHandle {
        let doc = self.document.borrow();
        match doc.get(node_id) {
            Some(NodeData::Document) => match self.doc_meta.get(&node_id) {
                Some(m) if m.use_xml_document_proto => self.prototypes.xml_document_proto,
                Some(m) if m.use_html_document_proto => self.prototypes.html_document_proto,
                Some(_) => self.prototypes.document_proto,
                None => self.prototypes.html_document_proto,
            },
            Some(NodeData::ShadowRoot { .. }) => self.prototypes.shadow_root_proto,
            Some(NodeData::DocumentFragment) => self.prototypes.fragment_proto,
            Some(NodeData::DocumentType { .. }) => self.prototypes.document_type_proto,
            Some(NodeData::Text(_)) => self.prototypes.text_proto,
            Some(NodeData::CDataSection(_)) => self.prototypes.cdata_section_proto,
            Some(NodeData::Comment(_)) => self.prototypes.comment_proto,
            Some(NodeData::ProcessingInstruction { .. }) => {
                self.prototypes.processing_instruction_proto
            }
            Some(NodeData::Element(el)) => {
                let ns = &*el.name.ns;
                let local = el.name.local.as_str();
                if ns == HTML_NS {
                    match local {
                        "video" => self.prototypes.html_video_element_proto,
                        "audio" => self.prototypes.html_audio_element_proto,
                        _ => {
                            let iface = html_tag_interface_name(local);
                            if iface == "HTMLElement" {
                                self.prototypes.html_element_proto
                            } else {
                                self.interface_protos
                                    .get(iface)
                                    .copied()
                                    .unwrap_or(self.prototypes.html_element_proto)
                            }
                        }
                    }
                } else if ns == SVG_NS {
                    let iface = svg_tag_interface_name(local);
                    if iface == "SVGElement" {
                        self.prototypes.svg_element_proto
                    } else {
                        self.interface_protos
                            .get(iface)
                            .copied()
                            .unwrap_or(self.prototypes.svg_element_proto)
                    }
                } else if ns == MATHML_NS {
                    self.prototypes.mathml_element_proto
                } else {
                    self.prototypes.element_proto
                }
            }
            None => self.prototypes.node_proto,
        }
    }
}

/// Builds the full `HashMap<&'static str, HeapHandle>` of HTML and SVG element subclass prototypes.
pub fn build_element_interface_protos(
    vm: &mut JsVm,
    html_element_proto: HeapHandle,
    svg_element_proto: HeapHandle,
) -> HashMap<&'static str, HeapHandle> {
    let mut map =
        HashMap::with_capacity(HTML_ELEMENT_INTERFACES.len() + SVG_ELEMENT_INTERFACES.len());
    for &iface in HTML_ELEMENT_INTERFACES {
        map.insert(iface, make_proto(vm, html_element_proto));
    }
    let svg_geom = make_proto(vm, svg_element_proto);
    map.insert("SVGGeometryElement", svg_geom);
    for &iface in SVG_ELEMENT_INTERFACES {
        if iface == "SVGGeometryElement" {
            continue;
        }
        let parent = match iface {
            "SVGPathElement" | "SVGRectElement" | "SVGCircleElement" | "SVGEllipseElement"
            | "SVGLineElement" | "SVGPolylineElement" | "SVGPolygonElement" => svg_geom,
            _ => svg_element_proto,
        };
        map.insert(iface, make_proto(vm, parent));
    }
    map
}

// ---------------------------------------------------------------------------
// Pure-Rust Well-Formed XML 1.0 + Namespaces Parser (shared by iframes & DOMParser)
// ---------------------------------------------------------------------------

type XmlNsDecl = (Option<String>, String);
type XmlElementFrame = (NodeId, String, Vec<XmlNsDecl>);

/// Parses an XML 1.0 + Namespaces string into `parent_doc` inside `doc`.
/// Enforces well-formedness (matching start/end tags, valid names, quoted attributes,
/// declared namespace prefixes, single root element, internal DTD `<!ENTITY>`,
/// valid XML 1.0 Char production) and returns `Err(msg)` on any XML syntax/namespace error.
pub fn parse_xml_into_document(
    doc: &mut Document,
    parent_doc: NodeId,
    raw_xml: &str,
) -> Result<(), String> {
    parse_xml_internal(doc, parent_doc, raw_xml, false, &[])
}

/// Parses an XML 1.0 + Namespaces fragment into `parent_node` inside `doc`,
/// inheriting `initial_ns` bindings from the context element's ancestor chain.
pub fn parse_xml_fragment_into_node(
    doc: &mut Document,
    parent_node: NodeId,
    raw_xml: &str,
    initial_ns: &[XmlNsDecl],
) -> Result<(), String> {
    parse_xml_internal(doc, parent_node, raw_xml, true, initial_ns)
}

fn parse_xml_internal(
    doc: &mut Document,
    parent_doc: NodeId,
    raw_xml: &str,
    is_fragment: bool,
    initial_ns: &[XmlNsDecl],
) -> Result<(), String> {
    let xml = raw_xml.replace("\r\n", "\n").replace('\r', "\n");
    if xml.chars().any(|c| !is_valid_xml_char(c)) {
        return Err("Invalid XML 1.0 character in input".to_string());
    }
    let bytes = xml.as_bytes();
    let len = bytes.len();
    let mut pos = 0usize;

    // Strip optional UTF-8 BOM.
    if xml.starts_with('\u{FEFF}') {
        pos = 3;
    }

    let mut stack: Vec<XmlElementFrame> = Vec::new();
    let mut entities: HashMap<String, String> = HashMap::new();
    let mut root_elem_seen = false;
    let mut doctype_seen = false;

    while pos < len {
        let rest = &xml[pos..];
        if !rest.starts_with('<') {
            let next_lt = rest.find('<').map_or(len, |i| pos + i);
            let raw_text = &xml[pos..next_lt];
            pos = next_lt;
            if stack.is_empty() && !is_fragment {
                if raw_text
                    .bytes()
                    .any(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
                {
                    return Err("Unexpected character data outside root element".to_string());
                }
                continue;
            }
            if raw_text.contains("]]>") {
                return Err("']]>' is not allowed in XML text content".to_string());
            }
            let decoded = decode_xml_entities(raw_text, &entities)?;
            let parent = stack.last().map_or(parent_doc, |s| s.0);
            let tn = doc.create_text(&decoded);
            doc.append_child(parent, tn);
            continue;
        }

        // Comment: <!-- ... -->
        if let Some(after) = rest.strip_prefix("<!--") {
            let Some(end_rel) = after.find("-->") else {
                return Err("Unterminated XML comment".to_string());
            };
            let body = &after[..end_rel];
            if body.contains("--") || body.ends_with('-') {
                return Err("XML comment cannot contain '--'".to_string());
            }
            let parent = stack.last().map_or(parent_doc, |s| s.0);
            let cn = doc.create_comment(body);
            doc.append_child(parent, cn);
            pos += 4 + end_rel + 3;
            continue;
        }

        // CDATA: <![CDATA[ ... ]]>
        if let Some(after) = rest.strip_prefix("<![CDATA[") {
            if stack.is_empty() && !is_fragment {
                return Err("CDATA section outside root element".to_string());
            }
            let Some(end_rel) = after.find("]]>") else {
                return Err("Unterminated CDATA section".to_string());
            };
            let body = &after[..end_rel];
            let parent = stack.last().map_or(parent_doc, |s| s.0);
            let cn = doc.create_cdata_section(body);
            doc.append_child(parent, cn);
            pos += 9 + end_rel + 3;
            continue;
        }

        // DOCTYPE: <!DOCTYPE ...>
        if rest.starts_with("<!DOCTYPE") {
            if is_fragment || root_elem_seen || doctype_seen || !stack.is_empty() {
                return Err("Misplaced or duplicate DOCTYPE declaration".to_string());
            }
            doctype_seen = true;
            pos += 9;
            if !skip_xml_ws(bytes, &mut pos) {
                return Err("Expected whitespace after <!DOCTYPE".to_string());
            }
            let name_start = pos;
            while pos < len && !is_xml_ws_byte(bytes[pos]) && !matches!(bytes[pos], b'>' | b'[') {
                pos += 1;
            }
            let dt_name = xml[name_start..pos].to_string();
            if dt_name.is_empty() || !is_valid_xml_name(&dt_name) {
                return Err("Invalid DOCTYPE name".to_string());
            }
            let had_ws_after_name = skip_xml_ws(bytes, &mut pos);
            let mut public_id = String::new();
            let mut system_id = String::new();
            if had_ws_after_name && xml[pos..].starts_with("PUBLIC") {
                pos += 6;
                if !skip_xml_ws(bytes, &mut pos) {
                    return Err("Expected whitespace after PUBLIC".to_string());
                }
                public_id = read_xml_quoted(&xml, &mut pos)?;
                if !skip_xml_ws(bytes, &mut pos) {
                    return Err("Expected system literal after PUBLIC literal".to_string());
                }
                system_id = read_xml_quoted(&xml, &mut pos)?;
            } else if had_ws_after_name && xml[pos..].starts_with("SYSTEM") {
                pos += 6;
                if !skip_xml_ws(bytes, &mut pos) {
                    return Err("Expected whitespace after SYSTEM".to_string());
                }
                system_id = read_xml_quoted(&xml, &mut pos)?;
            }
            skip_xml_ws(bytes, &mut pos);
            // Optional internal subset `[ ... ]` with `<!ENTITY name "value">` parsing
            if pos < len && bytes[pos] == b'[' {
                pos += 1;
                let subset_start = pos;
                let mut depth = 1usize;
                while pos < len && depth > 0 {
                    match bytes[pos] {
                        b'"' | b'\'' => {
                            let q = bytes[pos];
                            pos += 1;
                            while pos < len && bytes[pos] != q {
                                pos += 1;
                            }
                            if pos < len {
                                pos += 1;
                            }
                        }
                        b'[' => {
                            depth += 1;
                            pos += 1;
                        }
                        b']' => {
                            depth -= 1;
                            pos += 1;
                        }
                        _ => pos += 1,
                    }
                }
                if depth != 0 {
                    return Err("Unterminated DOCTYPE internal subset".to_string());
                }
                let subset_body = &xml[subset_start..pos - 1];
                parse_dtd_internal_entities(subset_body, &mut entities);
                skip_xml_ws(bytes, &mut pos);
            }
            if pos >= len || bytes[pos] != b'>' {
                return Err("Unterminated DOCTYPE declaration".to_string());
            }
            pos += 1;
            let dn = doc.new_node(NodeData::DocumentType {
                name: dt_name,
                public_id,
                system_id,
            });
            doc.append_child(parent_doc, dn);
            continue;
        }

        // Processing instruction or XML declaration: <?target ...?>
        if let Some(after) = rest.strip_prefix("<?") {
            let Some(end_rel) = after.find("?>") else {
                return Err("Unterminated processing instruction".to_string());
            };
            let content = &after[..end_rel];
            let (target, data) = match content.find([' ', '\t', '\n', '\r']) {
                Some(i) => (&content[..i], content[i..].trim_start()),
                None => (content, ""),
            };
            if target.is_empty() || !is_valid_xml_name(target) {
                return Err("Invalid processing instruction target".to_string());
            }
            if target.eq_ignore_ascii_case("xml") {
                if is_fragment || target != "xml" || pos > 3 || !stack.is_empty() || root_elem_seen
                {
                    return Err("Invalid XML declaration placement".to_string());
                }
                // XML declaration does not create a DOM ProcessingInstruction node.
            } else {
                let parent = stack.last().map_or(parent_doc, |s| s.0);
                let pi = doc.new_node(NodeData::ProcessingInstruction {
                    target: target.to_string(),
                    data: data.to_string(),
                });
                doc.append_child(parent, pi);
            }
            pos += 2 + end_rel + 2;
            continue;
        }

        // Closing tag: </name>
        if rest.starts_with("</") {
            pos += 2;
            let name_start = pos;
            while pos < len && !is_xml_ws_byte(bytes[pos]) && bytes[pos] != b'>' {
                pos += 1;
            }
            let close_name = &xml[name_start..pos];
            skip_xml_ws(bytes, &mut pos);
            if pos >= len || bytes[pos] != b'>' {
                return Err("Malformed XML closing tag".to_string());
            }
            pos += 1;
            let Some((_, open_name, _)) = stack.pop() else {
                return Err(format!("Unexpected closing tag </{close_name}>"));
            };
            if open_name != close_name {
                return Err(format!(
                    "Mismatched closing tag </{close_name}> for <{open_name}>"
                ));
            }
            continue;
        }

        // Opening or self-closing tag: <name attr="val" ... /?>
        pos += 1;
        if pos >= len || is_xml_ws_byte(bytes[pos]) {
            return Err("Invalid XML start tag".to_string());
        }
        let name_start = pos;
        while pos < len && !is_xml_ws_byte(bytes[pos]) && !matches!(bytes[pos], b'>' | b'/') {
            pos += 1;
        }
        let qname = xml[name_start..pos].to_string();
        if qname.is_empty() || !is_valid_xml_qname(&qname) {
            return Err(format!("Invalid XML element name: {qname}"));
        }
        if stack.is_empty() && !is_fragment {
            if root_elem_seen {
                return Err("Multiple root elements in XML document".to_string());
            }
            root_elem_seen = true;
        }

        // Parse attributes
        let mut raw_attrs: Vec<(String, String)> = Vec::new();
        let mut ns_decls: Vec<(Option<String>, String)> = Vec::new();
        let mut self_closing = false;

        loop {
            let had_ws = skip_xml_ws(bytes, &mut pos);
            if pos >= len {
                return Err("Unterminated XML start tag".to_string());
            }
            if bytes[pos] == b'>' {
                pos += 1;
                break;
            }
            if bytes[pos] == b'/' {
                if pos + 1 >= len || bytes[pos + 1] != b'>' {
                    return Err("Expected '>' after '/' in empty-element tag".to_string());
                }
                self_closing = true;
                pos += 2;
                break;
            }
            if !had_ws && !raw_attrs.is_empty() {
                return Err("Missing whitespace between XML attributes".to_string());
            }
            let an_start = pos;
            while pos < len
                && !is_xml_ws_byte(bytes[pos])
                && !matches!(bytes[pos], b'=' | b'>' | b'/')
            {
                pos += 1;
            }
            let attr_qname = xml[an_start..pos].to_string();
            if attr_qname.is_empty() || !is_valid_xml_qname(&attr_qname) {
                return Err(format!("Invalid XML attribute name: {attr_qname}"));
            }
            skip_xml_ws(bytes, &mut pos);
            if pos >= len || bytes[pos] != b'=' {
                return Err(format!("Missing '=' after XML attribute {attr_qname}"));
            }
            pos += 1;
            skip_xml_ws(bytes, &mut pos);
            let raw_val = read_xml_quoted(&xml, &mut pos)?;
            if raw_val.contains('<') {
                return Err("Unescaped '<' in XML attribute value".to_string());
            }
            let attr_val = decode_xml_entities(&raw_val, &entities)?;
            if attr_qname == "xmlns" {
                if attr_val == XMLNS_NS {
                    return Err("Cannot bind default namespace to XMLNS namespace".to_string());
                }
                ns_decls.push((None, attr_val.clone()));
            } else if let Some(pfx) = attr_qname.strip_prefix("xmlns:") {
                if pfx.is_empty() || pfx == "xmlns" || (pfx == "xml" && attr_val != XML_NS) {
                    return Err(format!("Invalid xmlns prefix declaration: {attr_qname}"));
                }
                if attr_val == XMLNS_NS || (attr_val == XML_NS && pfx != "xml") {
                    return Err(format!("Reserved namespace binding on {attr_qname}"));
                }
                ns_decls.push((Some(pfx.to_string()), attr_val.clone()));
            }
            raw_attrs.push((attr_qname, attr_val));
        }

        // Resolve element namespace from `ns_decls` + ancestor stack + `initial_ns`.
        let (el_prefix, el_local) = split_qname(&qname);
        let el_ns = match el_prefix {
            Some("xml") => XML_NS.to_string(),
            Some("xmlns") => return Err("Element prefix cannot be 'xmlns'".to_string()),
            Some(pfx) => resolve_xml_ns(&stack, &ns_decls, initial_ns, Some(pfx))
                .ok_or_else(|| format!("Undeclared XML namespace prefix: {pfx}"))?,
            None => resolve_xml_ns(&stack, &ns_decls, initial_ns, None).unwrap_or_default(),
        };

        let mut el_data = rix_dom::ElementData::new(rix_dom::QualName::new(
            el_prefix.map(Into::into),
            rix_dom::Namespace::from(el_ns.as_str()),
            rix_dom::LocalName::from(el_local),
        ));
        let mut seen_attrs: Vec<(String, String)> = Vec::new();
        for (aqn, aval) in raw_attrs {
            let (apfx, alocal, ans) = if aqn == "xmlns" {
                (None, "xmlns", XMLNS_NS.to_string())
            } else if let Some(p) = aqn.strip_prefix("xmlns:") {
                (Some("xmlns"), p, XMLNS_NS.to_string())
            } else {
                let (p, l) = split_qname(&aqn);
                let ns = match p {
                    None => String::new(),
                    Some("xml") => XML_NS.to_string(),
                    Some("xmlns") => XMLNS_NS.to_string(),
                    Some(pfx) => resolve_xml_ns(&stack, &ns_decls, initial_ns, Some(pfx))
                        .ok_or_else(|| format!("Undeclared XML attribute prefix: {pfx}"))?,
                };
                (p, l, ns)
            };
            let key = (ans.clone(), alocal.to_string());
            if seen_attrs.contains(&key) {
                return Err(format!("Duplicate XML attribute: {aqn}"));
            }
            seen_attrs.push(key);
            el_data.set_attribute_ns(&ans, apfx, alocal, &aval);
        }

        let parent = stack.last().map_or(parent_doc, |s| s.0);
        let el_nid = doc.new_node(NodeData::Element(el_data));
        doc.append_child(parent, el_nid);

        if !self_closing {
            stack.push((el_nid, qname, ns_decls));
        }
    }

    if !stack.is_empty() {
        return Err(format!(
            "Unclosed XML element <{}>",
            stack.last().unwrap().1
        ));
    }
    if !is_fragment && !root_elem_seen {
        return Err("XML document has no root element".to_string());
    }
    Ok(())
}

/// XML 1.0 Fifth Edition `Char` production:
/// `#x9 | #xA | #xD | [#x20-#xD7FF] | [#xE000-#xFFFD] | [#x10000-#x10FFFF]`.
#[inline]
pub fn is_valid_xml_char(c: char) -> bool {
    let cp = c as u32;
    matches!(
        cp,
        0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF
    )
}

fn parse_dtd_internal_entities(subset: &str, out: &mut HashMap<String, String>) {
    let bytes = subset.as_bytes();
    let len = bytes.len();
    let mut pos = 0usize;
    while pos < len {
        let Some(rel) = subset[pos..].find("<!ENTITY") else {
            break;
        };
        pos += rel + 8;
        if !skip_xml_ws(bytes, &mut pos) {
            continue;
        }
        // Skip parameter entities (`% name ...`).
        if pos < len && bytes[pos] == b'%' {
            continue;
        }
        let name_start = pos;
        while pos < len && !is_xml_ws_byte(bytes[pos]) && bytes[pos] != b'>' {
            pos += 1;
        }
        let ent_name = subset[name_start..pos].to_string();
        if ent_name.is_empty() {
            continue;
        }
        skip_xml_ws(bytes, &mut pos);
        if let Ok(ent_val) = read_xml_quoted(subset, &mut pos) {
            out.entry(ent_name).or_insert(ent_val);
        }
    }
}

#[inline]
fn is_xml_ws_byte(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

fn skip_xml_ws(bytes: &[u8], pos: &mut usize) -> bool {
    let start = *pos;
    while *pos < bytes.len() && is_xml_ws_byte(bytes[*pos]) {
        *pos += 1;
    }
    *pos > start
}

fn is_valid_xml_qname(s: &str) -> bool {
    let mut parts = s.split(':');
    let first = parts.next().unwrap_or("");
    if let Some(second) = parts.next() {
        if parts.next().is_some() || first.is_empty() || second.is_empty() {
            return false;
        }
        is_valid_xml_ncname(first) && is_valid_xml_ncname(second)
    } else {
        is_valid_xml_ncname(first)
    }
}

fn is_valid_xml_ncname(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first == ':' || !is_xml_name_start_char(first) {
        return false;
    }
    chars.all(|c| c != ':' && is_xml_name_char(c))
}

fn split_qname(qname: &str) -> (Option<&str>, &str) {
    match qname.split_once(':') {
        Some((p, l)) => (Some(p), l),
        None => (None, qname),
    }
}

fn resolve_xml_ns(
    stack: &[XmlElementFrame],
    local_decls: &[XmlNsDecl],
    initial_ns: &[XmlNsDecl],
    prefix: Option<&str>,
) -> Option<String> {
    for (p, uri) in local_decls.iter().rev() {
        if p.as_deref() == prefix {
            return if uri.is_empty() {
                None
            } else {
                Some(uri.clone())
            };
        }
    }
    for (_, _, decls) in stack.iter().rev() {
        for (p, uri) in decls.iter().rev() {
            if p.as_deref() == prefix {
                return if uri.is_empty() {
                    None
                } else {
                    Some(uri.clone())
                };
            }
        }
    }
    for (p, uri) in initial_ns.iter().rev() {
        if p.as_deref() == prefix {
            return if uri.is_empty() {
                None
            } else {
                Some(uri.clone())
            };
        }
    }
    None
}

fn read_xml_quoted(xml: &str, pos: &mut usize) -> Result<String, String> {
    let bytes = xml.as_bytes();
    if *pos >= bytes.len() || !matches!(bytes[*pos], b'"' | b'\'') {
        return Err("Expected quoted string in XML".to_string());
    }
    let q = bytes[*pos];
    *pos += 1;
    let start = *pos;
    while *pos < bytes.len() && bytes[*pos] != q {
        *pos += 1;
    }
    if *pos >= bytes.len() {
        return Err("Unterminated quoted string in XML".to_string());
    }
    let val = xml[start..*pos].to_string();
    *pos += 1;
    Ok(val)
}

fn decode_xml_entities(input: &str, entities: &HashMap<String, String>) -> Result<String, String> {
    if !input.contains('&') {
        return Ok(input.to_string());
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let Some(semi) = after.find(';') else {
            return Err("Unterminated XML entity reference".to_string());
        };
        let ent = &after[..semi];
        match ent {
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "amp" => out.push('&'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                let code = u32::from_str_radix(&ent[2..], 16)
                    .map_err(|_| format!("Invalid hex character reference: &{ent};"))?;
                let ch = char::from_u32(code)
                    .filter(|&c| is_valid_xml_char(c))
                    .ok_or_else(|| format!("Invalid XML Unicode scalar: &{ent};"))?;
                out.push(ch);
            }
            _ if ent.starts_with('#') => {
                let code: u32 = ent[1..]
                    .parse()
                    .map_err(|_| format!("Invalid decimal character reference: &{ent};"))?;
                let ch = char::from_u32(code)
                    .filter(|&c| is_valid_xml_char(c))
                    .ok_or_else(|| format!("Invalid XML Unicode scalar: &{ent};"))?;
                out.push(ch);
            }
            _ => {
                if let Some(val) = entities.get(ent) {
                    out.push_str(val);
                } else {
                    return Err(format!("Unknown XML entity reference: &{ent};"));
                }
            }
        }
        rest = &after[semi + 1..];
    }
    out.push_str(rest);
    Ok(out)
}
