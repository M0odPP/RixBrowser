//! Weak DOM Bridge (`WeakTable<NodeId>`) binding `rix-dom`, `rix-html`,
//! `rix-css`, and `rix-layout` into `rix-js` with zero memory leaks for
//! detached nodes, dirty-checked incremental layout queries, live `CSSStyleDeclaration`,
//! `ShadowRoot`, `CustomElementRegistry`, and `document.write()` injection.
//!
//! Reference: Spec 05 §2.2, Spec 06 §3 & Issue #16.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use rix_core::{Point, Rect, Size};
use rix_css::{
    CascadeOrigin, ColorRgba, ComputedStyle, DisplayMode, LengthPercentage, LengthPercentageAuto,
    MediaContext, PositionMode, SizeValue, StyleContext, Stylesheet, VisibilityMode,
};
use rix_dom::query::{matches_complex, ComplexSelector};
use rix_dom::{
    is_valid_custom_element_name, is_valid_shadow_host_name, Document, NodeData, NodeId,
};
use rix_html::{parse_html_fragment, HtmlParserStream};
use rix_js::promise::{alloc_promise, resolve_promise};
use rix_js::{
    GcHeap, HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs, WeakTable,
};
use rix_layout::{Fragment, FragmentTree, LayoutEngine};
use rix_text::FontDatabase;

/// Structural/attribute/text mutation emitted by DOM operations (consumed by `MutationObserver`).
#[derive(Debug, Clone)]
pub enum DomMutation {
    ChildList {
        target: NodeId,
        added: Vec<NodeId>,
        removed: Vec<NodeId>,
        previous_sibling: Option<NodeId>,
        next_sibling: Option<NodeId>,
    },
    Attributes {
        target: NodeId,
        attribute_name: String,
        old_value: Option<String>,
    },
    CharacterData {
        target: NodeId,
        old_value: String,
    },
}

/// Callback invoked synchronously whenever a DOM mutation occurs.
pub type MutationSink = Rc<RefCell<Box<dyn FnMut(&DomMutation)>>>;

/// Registered Custom Element definition (`window.customElements.define`).
#[derive(Debug, Clone)]
pub struct CustomElementDef {
    pub constructor: HeapHandle,
    pub observed_attributes: Vec<String>,
}

/// DOM prototype handles installed inside a [`JsVm`] realm.
#[derive(Debug, Clone, Copy)]
pub struct DomPrototypes {
    pub event_target_proto: HeapHandle,
    pub node_proto: HeapHandle,
    pub element_proto: HeapHandle,
    pub html_element_proto: HeapHandle,
    pub html_canvas_element_proto: HeapHandle,
    pub document_proto: HeapHandle,
    pub shadow_root_proto: HeapHandle,
    pub text_proto: HeapHandle,
    pub fragment_proto: HeapHandle,
}

impl DomPrototypes {
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for h in [
            self.event_target_proto,
            self.node_proto,
            self.element_proto,
            self.html_element_proto,
            self.html_canvas_element_proto,
            self.document_proto,
            self.shadow_root_proto,
            self.text_proto,
            self.fragment_proto,
        ] {
            out.push(JsValue::from_object(h));
        }
    }
}

/// Shared host state bridged between `rix-dom`/`rix-css`/`rix-layout` and `rix-js`.
pub struct HostState {
    pub document: Rc<RefCell<Document>>,
    pub wrappers: WeakTable<NodeId>,
    tag_to_node: HashMap<u64, NodeId>,
    node_to_tag: HashMap<NodeId, u64>,
    next_tag: u64,
    pub prototypes: DomPrototypes,
    pub active_parser: Option<Rc<RefCell<HtmlParserStream>>>,
    pub ready_state: String,
    pub viewport: Size,
    pub font_db: Arc<FontDatabase>,
    pub layout_engine: LayoutEngine,
    style_ctx: StyleContext,
    last_style_revs: Option<(u64, u64)>,
    last_layout_revs: Option<(u64, u64)>,
    last_paint_revs: Option<(u64, u64)>,
    cached_fragment_tree: Option<FragmentTree>,
    pub style_recalc_count: u64,
    pub layout_count: u64,
    pub display_list_build_count: u64,
    pub custom_elements: HashMap<String, CustomElementDef>,
    pub when_defined_waiters: HashMap<String, Vec<HeapHandle>>,
    pub mutation_sinks: Vec<MutationSink>,
}

/// Shared handle to [`HostState`].
pub type SharedHost = Rc<RefCell<HostState>>;

impl HostState {
    #[must_use]
    pub fn new(
        document: Rc<RefCell<Document>>,
        prototypes: DomPrototypes,
        viewport: Size,
        font_db: Arc<FontDatabase>,
    ) -> Self {
        let media = MediaContext {
            viewport_width: viewport.width,
            viewport_height: viewport.height,
            ..MediaContext::default()
        };
        Self {
            document,
            wrappers: WeakTable::new(),
            tag_to_node: HashMap::new(),
            node_to_tag: HashMap::new(),
            next_tag: 1,
            prototypes,
            active_parser: None,
            ready_state: "complete".to_string(),
            viewport,
            font_db,
            layout_engine: LayoutEngine::new(),
            style_ctx: StyleContext::new(media),
            last_style_revs: None,
            last_layout_revs: None,
            last_paint_revs: None,
            cached_fragment_tree: None,
            style_recalc_count: 0,
            layout_count: 0,
            display_list_build_count: 0,
            custom_elements: HashMap::new(),
            when_defined_waiters: HashMap::new(),
            mutation_sinks: Vec::new(),
        }
    }

    /// Resolves a JS wrapper's `host_tag` back to its `NodeId`.
    #[must_use]
    pub fn node_for_handle(&self, vm: &JsVm, handle: HeapHandle) -> Option<NodeId> {
        let tag = vm.heap.get_object(handle)?.host_tag?;
        self.tag_to_node.get(&tag).copied()
    }

    /// Returns the `NodeId` for any JS value that wraps a DOM node.
    #[must_use]
    pub fn node_for_value(&self, vm: &JsVm, val: JsValue) -> Option<NodeId> {
        let h = val.as_object()?;
        self.node_for_handle(vm, h)
    }

    /// Emits a [`DomMutation`] to all registered observer sinks.
    pub fn emit_mutation(&self, mutation: DomMutation) {
        let sinks = self.mutation_sinks.clone();
        for sink in sinks {
            (sink.borrow_mut())(&mutation);
        }
    }

    /// Collects all host-pinned roots (prototypes, pinned weak wrappers, custom element ctors).
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        self.prototypes.collect_roots(out);
        self.wrappers.collect_pinned_roots(out);
        for def in self.custom_elements.values() {
            out.push(JsValue::from_object(def.constructor));
        }
        for waiters in self.when_defined_waiters.values() {
            for &p in waiters {
                out.push(JsValue::from_object(p));
            }
        }
    }

    /// Sweeps dead unpinned weak DOM wrappers and releases their tag table entries (zero leaks!).
    pub fn sweep_dead_wrappers(&mut self, heap: &GcHeap) -> usize {
        let removed = self.wrappers.sweep_dead(heap);
        if removed > 0 {
            let mut dead_nodes = Vec::new();
            for (&node, &tag) in &self.node_to_tag {
                if self.wrappers.get_live(&node, heap).is_none() {
                    dead_nodes.push((node, tag));
                }
            }
            for (node, tag) in dead_nodes {
                self.node_to_tag.remove(&node);
                self.tag_to_node.remove(&tag);
            }
        }
        removed
    }

    /// Recomputes CSS cascade ONLY when document structure or style revision changed.
    pub fn ensure_styles(&mut self) -> &StyleContext {
        let revs = {
            let doc = self.document.borrow();
            (doc.revision(), doc.style_revision())
        };
        if self.last_style_revs == Some(revs) {
            return &self.style_ctx;
        }
        let media = MediaContext {
            viewport_width: self.viewport.width,
            viewport_height: self.viewport.height,
            ..MediaContext::default()
        };
        let mut ctx = StyleContext::new(media);
        let sheets: Vec<String> = {
            let doc = self.document.borrow();
            let mut css_blocks = Vec::new();
            for node in doc.descendants_with_shadow_roots(doc.root()) {
                if let Some(NodeData::Element(el)) = doc.get(node) {
                    if el.name.local.as_str().eq_ignore_ascii_case("style") {
                        let text = doc.inner_text(node);
                        if !text.trim().is_empty() {
                            css_blocks.push(text);
                        }
                    }
                }
            }
            for sheet in doc.all_dynamic_stylesheets().values() {
                if !sheet.trim().is_empty() {
                    css_blocks.push(sheet.clone());
                }
            }
            css_blocks
        };
        for css in sheets {
            let parsed = Stylesheet::parse(&css, CascadeOrigin::Author, &media);
            ctx.add_stylesheet(Arc::new(parsed));
        }
        {
            let mut doc = self.document.borrow_mut();
            let _ = ctx.cascade_full(&mut doc);
        }
        let post_revs = {
            let doc = self.document.borrow();
            (doc.revision(), doc.style_revision())
        };
        self.style_ctx = ctx;
        self.last_style_revs = Some(post_revs);
        self.style_recalc_count += 1;
        &self.style_ctx
    }

    /// Recomputes incremental layout ONLY when document structure or style revision changed.
    pub fn ensure_layout(&mut self) -> Option<&FragmentTree> {
        let revs = {
            let doc = self.document.borrow();
            (doc.revision(), doc.style_revision())
        };
        if self.last_layout_revs == Some(revs) && self.cached_fragment_tree.is_some() {
            return self.cached_fragment_tree.as_ref();
        }
        let _ = self.ensure_styles();
        let (tree, post_revs) = {
            let doc = self.document.borrow();
            let (tree, _metrics) =
                self.layout_engine
                    .layout_only(&doc, &self.style_ctx, self.viewport, &self.font_db);
            (tree, (doc.revision(), doc.style_revision()))
        };
        self.cached_fragment_tree = Some(tree);
        self.last_layout_revs = Some(post_revs);
        self.layout_count += 1;
        self.cached_fragment_tree.as_ref()
    }

    /// Finds the absolute border-box rect and content-box rect of `node` in viewport CSS pixels.
    pub fn bounding_rects_for_node(&mut self, node: NodeId) -> Option<(Rect, Rect)> {
        let tree = self.ensure_layout()?;
        let root = tree.root.as_ref()?;
        find_fragment_rects(root, Point::ZERO, node)
    }

    /// Returns the cached `ComputedStyle` for `node`, running dirty-checked cascade if needed.
    pub fn computed_style_for_node(&mut self, node: NodeId) -> Option<Arc<ComputedStyle>> {
        let ctx = self.ensure_styles();
        ctx.style_for(node).cloned()
    }

    /// Rebuilds the page [`rix_render::DisplayList`] ONLY when the DOM or CSS revisions changed.
    /// During 60 FPS Canvas2D animations this returns `None` (zero DisplayList rebuilds!).
    pub fn paint_if_dirty(
        &mut self,
        pipeline: rix_ipc::PipelineId,
        resources: &mut rix_render::ResourceRegistry,
    ) -> Option<rix_render::DisplayList> {
        let revs = {
            let doc = self.document.borrow();
            (doc.revision(), doc.style_revision())
        };
        if self.last_paint_revs == Some(revs) {
            return None;
        }
        let _ = self.ensure_layout();
        let post_revs = {
            let doc = self.document.borrow();
            (doc.revision(), doc.style_revision())
        };
        let tree = self.cached_fragment_tree.as_ref()?;
        let dl = rix_layout::paint_fragment_tree(
            tree,
            &self.font_db,
            &self.layout_engine.assets,
            pipeline,
            resources,
            self.layout_engine.dark_mode,
        );
        self.last_paint_revs = Some(post_revs);
        self.display_list_build_count += 1;
        Some(dl)
    }
}

fn find_fragment_rects(
    frag: &Fragment,
    parent_origin: Point,
    target: NodeId,
) -> Option<(Rect, Rect)> {
    let abs_border = frag.absolute_border_rect(parent_origin);
    if frag.dom_node == Some(target) {
        let abs_content = Rect::new(
            abs_border.origin.x + frag.content_rect.origin.x,
            abs_border.origin.y + frag.content_rect.origin.y,
            frag.content_rect.size.width,
            frag.content_rect.size.height,
        );
        return Some((abs_border, abs_content));
    }
    let child_origin = abs_border.origin;
    for child in frag.children.iter() {
        if let Some(found) = find_fragment_rects(child, child_origin, target) {
            return Some(found);
        }
    }
    None
}

/// Returns the existing JS wrapper for `node_id` or lazily allocates a new **weak** wrapper.
pub fn wrap_node(vm: &mut JsVm, host: &SharedHost, node_id: NodeId) -> JsValue {
    if let Some(existing) = host.borrow().wrappers.get_live(&node_id, &vm.heap) {
        return JsValue::from_object(existing);
    }
    let (proto, tag) = {
        let mut h = host.borrow_mut();
        let proto = {
            let doc = h.document.borrow();
            match doc.get(node_id) {
                Some(NodeData::Document) => h.prototypes.document_proto,
                Some(NodeData::ShadowRoot { .. }) => h.prototypes.shadow_root_proto,
                Some(NodeData::Element(el)) => {
                    if el.name.local.as_str().eq_ignore_ascii_case("canvas") {
                        h.prototypes.html_canvas_element_proto
                    } else {
                        h.prototypes.html_element_proto
                    }
                }
                Some(NodeData::Text(_) | NodeData::Comment(_)) => h.prototypes.text_proto,
                Some(NodeData::DocumentFragment) => h.prototypes.fragment_proto,
                _ => h.prototypes.node_proto,
            }
        };
        let tag = if let Some(&existing_tag) = h.node_to_tag.get(&node_id) {
            existing_tag
        } else {
            let t = h.next_tag;
            h.next_tag = h.next_tag.wrapping_add(1).max(1);
            h.node_to_tag.insert(node_id, t);
            h.tag_to_node.insert(t, node_id);
            t
        };
        (proto, tag)
    };
    let mut obj = JsObject::with_kind(ObjectKind::Host, Some(proto));
    obj.host_tag = Some(tag);
    let handle = vm.heap.alloc_object(obj);
    host.borrow_mut().wrappers.insert(node_id, handle, false);
    JsValue::from_object(handle)
}

/// Allocates the full DOM prototype hierarchy and installs `document`, `window`, `self`,
/// `globalThis`, `customElements`, `getComputedStyle`, and DOM constructors on `vm`.
pub fn install_dom_bridge(
    vm: &mut JsVm,
    document: Rc<RefCell<Document>>,
    viewport: Size,
    font_db: Arc<FontDatabase>,
) -> SharedHost {
    let make_proto = |vm: &mut JsVm, parent: HeapHandle| {
        let p = vm.heap.alloc_empty_object();
        vm.heap
            .object_set_prototype(p, Some(parent), &mut vm.shapes);
        vm.heap.get_object_mut(p).unwrap().is_prototype = true;
        p
    };

    let event_target_proto = make_proto(vm, vm.object_proto);
    let node_proto = make_proto(vm, event_target_proto);
    let element_proto = make_proto(vm, node_proto);
    let html_element_proto = make_proto(vm, element_proto);
    let html_canvas_element_proto = make_proto(vm, html_element_proto);
    let document_proto = make_proto(vm, node_proto);
    let shadow_root_proto = make_proto(vm, node_proto);
    let text_proto = make_proto(vm, node_proto);
    let fragment_proto = make_proto(vm, node_proto);

    let protos = DomPrototypes {
        event_target_proto,
        node_proto,
        element_proto,
        html_element_proto,
        html_canvas_element_proto,
        document_proto,
        shadow_root_proto,
        text_proto,
        fragment_proto,
    };

    let host = Rc::new(RefCell::new(HostState::new(
        document, protos, viewport, font_db,
    )));

    // Make global `window` inherit from `EventTarget.prototype`.
    let g = vm.global;
    vm.heap
        .object_set_prototype(g, Some(event_target_proto), &mut vm.shapes);

    install_node_prototype(vm, &host);
    install_element_prototype(vm, &host);
    install_document_prototype(vm, &host);
    install_shadow_root_prototype(vm, &host);
    install_window_and_cssom(vm, &host);

    // Pin root document wrapper on global `document`.
    let doc_root = host.borrow().document.borrow().root();
    let doc_val = wrap_node(vm, &host, doc_root);
    host.borrow_mut().wrappers.set_pinned(&doc_root, true);
    define_global(vm, "document", doc_val);
    let gv = JsValue::from_object(vm.global);
    define_global(vm, "window", gv);
    define_global(vm, "self", gv);
    define_global(vm, "globalThis", gv);

    host
}

fn define_global(vm: &mut JsVm, name: &str, val: JsValue) {
    let k = vm.heap.intern_key(name);
    let g = vm.global;
    vm.heap
        .object_define_data(g, k, val, PropertyAttrs::METHOD, &mut vm.shapes);
}

fn define_host_method(
    vm: &mut JsVm,
    target: HeapHandle,
    name: &str,
    f: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
) {
    let key = vm.heap.intern_key(name);
    let func = vm.alloc_host_fn(name, f);
    vm.heap.object_define_data(
        target,
        key,
        JsValue::from_object(func),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
}

fn define_host_accessor(
    vm: &mut JsVm,
    target: HeapHandle,
    name: &str,
    getter: Option<HeapHandle>,
    setter: Option<HeapHandle>,
) {
    let key = vm.heap.intern_key(name);
    vm.heap.object_define_accessor(
        target,
        key,
        getter,
        setter,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
}

fn require_node(vm: &mut JsVm, host: &SharedHost, val: JsValue) -> Result<NodeId, JsException> {
    host.borrow()
        .node_for_value(vm, val)
        .ok_or_else(|| vm.type_error("Illegal invocation: receiver is not a DOM Node"))
}

fn arg(args: &[JsValue], i: usize) -> JsValue {
    args.get(i).copied().unwrap_or(JsValue::UNDEFINED)
}

fn wrap_node_list(vm: &mut JsVm, host: &SharedHost, nodes: Vec<NodeId>) -> JsValue {
    let arr = vm.alloc_array();
    for nid in nodes {
        let v = wrap_node(vm, host, nid);
        vm.heap.object_push_element(arr, v);
    }
    JsValue::from_object(arr)
}

// ---------------------------------------------------------------------------
// Node.prototype
// ---------------------------------------------------------------------------

fn install_node_prototype(vm: &mut JsVm, host: &SharedHost) {
    let np = host.borrow().prototypes.node_proto;

    // nodeType
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get nodeType", move |vm, this_val, _| {
        let nid = require_node(vm, &h, this_val)?;
        let ty = match h.borrow().document.borrow().get(nid) {
            Some(NodeData::Element(_)) => 1,
            Some(NodeData::Text(_)) => 3,
            Some(NodeData::Comment(_)) => 8,
            Some(NodeData::Document) => 9,
            Some(NodeData::DocumentType { .. }) => 10,
            Some(NodeData::DocumentFragment | NodeData::ShadowRoot { .. }) => 11,
            None => 0,
        };
        Ok(JsValue::from_i32(ty))
    });
    define_host_accessor(vm, np, "nodeType", Some(g), None);

    // nodeName
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get nodeName", move |vm, this_val, _| {
        let nid = require_node(vm, &h, this_val)?;
        let name = match h.borrow().document.borrow().get(nid) {
            Some(NodeData::Element(e)) => e.name.local.as_str().to_ascii_uppercase(),
            Some(NodeData::Text(_)) => "#text".into(),
            Some(NodeData::Comment(_)) => "#comment".into(),
            Some(NodeData::Document) => "#document".into(),
            Some(NodeData::DocumentFragment | NodeData::ShadowRoot { .. }) => {
                "#document-fragment".into()
            }
            Some(NodeData::DocumentType { name, .. }) => name.clone(),
            None => String::new(),
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(name)))
    });
    define_host_accessor(vm, np, "nodeName", Some(g), None);

    // parentNode & parentElement
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get parentNode", move |vm, this_val, _| {
        let nid = require_node(vm, &h, this_val)?;
        let parent = h.borrow().document.borrow().parent(nid);
        Ok(parent.map_or(JsValue::NULL, |p| wrap_node(vm, &h, p)))
    });
    define_host_accessor(vm, np, "parentNode", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get parentElement", move |vm, this_val, _| {
        let nid = require_node(vm, &h, this_val)?;
        let pe = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let doc = _doc_rc.borrow();
            doc.parent(nid)
                .filter(|&p| matches!(doc.get(p), Some(NodeData::Element(_))))
        };
        Ok(pe.map_or(JsValue::NULL, |p| wrap_node(vm, &h, p)))
    });
    define_host_accessor(vm, np, "parentElement", Some(g), None);

    // isConnected
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get isConnected", move |vm, this_val, _| {
        let nid = require_node(vm, &h, this_val)?;
        let conn = h.borrow().document.borrow().is_connected(nid);
        Ok(JsValue::from_bool(conn))
    });
    define_host_accessor(vm, np, "isConnected", Some(g), None);

    // ownerDocument
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get ownerDocument", move |vm, this_val, _| {
        let nid = require_node(vm, &h, this_val)?;
        let root = h.borrow().document.borrow().root();
        if nid == root {
            Ok(JsValue::NULL)
        } else {
            Ok(wrap_node(vm, &h, root))
        }
    });
    define_host_accessor(vm, np, "ownerDocument", Some(g), None);

    // firstChild / lastChild / nextSibling / previousSibling / childNodes
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get firstChild", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let c = h.borrow().document.borrow().first_child(nid);
        Ok(c.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, np, "firstChild", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get lastChild", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let c = h.borrow().document.borrow().last_child(nid);
        Ok(c.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, np, "lastChild", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get nextSibling", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let c = h.borrow().document.borrow().next_sibling(nid);
        Ok(c.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, np, "nextSibling", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get previousSibling", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let c = h.borrow().document.borrow().previous_sibling(nid);
        Ok(c.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, np, "previousSibling", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get childNodes", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let kids: Vec<NodeId> = h.borrow().document.borrow().children(nid).collect();
        Ok(wrap_node_list(vm, &h, kids))
    });
    define_host_accessor(vm, np, "childNodes", Some(g), None);

    // textContent / nodeValue (getter + setter)
    let h1 = Rc::clone(host);
    let g = vm.alloc_host_fn("get textContent", move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        let txt = h1.borrow().document.borrow().inner_text(nid);
        Ok(JsValue::from_string(vm.heap.alloc_string(txt)))
    });
    let h2 = Rc::clone(host);
    let s = vm.alloc_host_fn("set textContent", move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let new_text = if arg(args, 0).is_null() || arg(args, 0).is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(arg(args, 0))
        };
        let is_text = matches!(
            h2.borrow().document.borrow().get(nid),
            Some(NodeData::Text(_) | NodeData::Comment(_))
        );
        if is_text {
            let old = {
                let _doc_rc = Rc::clone(&h2.borrow().document);
                let mut doc = _doc_rc.borrow_mut();
                match doc.get_mut(nid) {
                    Some(NodeData::Text(s) | NodeData::Comment(s)) => {
                        std::mem::replace(s, new_text)
                    }
                    _ => String::new(),
                }
            };
            h2.borrow().emit_mutation(DomMutation::CharacterData {
                target: nid,
                old_value: old,
            });
        } else {
            let (removed, added) = {
                let _doc_rc = Rc::clone(&h2.borrow().document);
                let mut doc = _doc_rc.borrow_mut();
                let removed: Vec<NodeId> = doc.children(nid).collect();
                for &child in &removed {
                    doc.remove_child(child);
                }
                let mut added = Vec::new();
                if !new_text.is_empty() {
                    let tn = doc.create_text(&new_text);
                    doc.append_child(nid, tn);
                    added.push(tn);
                }
                (removed, added)
            };
            h2.borrow().emit_mutation(DomMutation::ChildList {
                target: nid,
                added,
                removed,
                previous_sibling: None,
                next_sibling: None,
            });
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, np, "textContent", Some(g), Some(s));
    define_host_accessor(vm, np, "nodeValue", Some(g), Some(s));

    // appendChild(child)
    let h = Rc::clone(host);
    define_host_method(vm, np, "appendChild", move |vm, t, args| {
        let parent = require_node(vm, &h, t)?;
        let child_val = arg(args, 0);
        let child = require_node(vm, &h, child_val)?;
        insert_node_into_parent(vm, &h, parent, child, None)?;
        Ok(child_val)
    });

    // insertBefore(newNode, referenceNode)
    let h = Rc::clone(host);
    define_host_method(vm, np, "insertBefore", move |vm, t, args| {
        let parent = require_node(vm, &h, t)?;
        let child_val = arg(args, 0);
        let child = require_node(vm, &h, child_val)?;
        let ref_val = arg(args, 1);
        let ref_node = if ref_val.is_null() || ref_val.is_undefined() {
            None
        } else {
            Some(require_node(vm, &h, ref_val)?)
        };
        insert_node_into_parent(vm, &h, parent, child, ref_node)?;
        Ok(child_val)
    });

    // removeChild(child)
    let h = Rc::clone(host);
    define_host_method(vm, np, "removeChild", move |vm, t, args| {
        let parent = require_node(vm, &h, t)?;
        let child_val = arg(args, 0);
        let child = require_node(vm, &h, child_val)?;
        let (was_connected, prev_sib, next_sib) = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let doc = _doc_rc.borrow();
            if doc.parent(child) != Some(parent) {
                return Err(vm.make_error(
                    "NotFoundError",
                    "The node to be removed is not a child of this node",
                ));
            }
            (
                doc.is_connected(child),
                doc.previous_sibling(child),
                doc.next_sibling(child),
            )
        };
        h.borrow().document.borrow_mut().remove_child(child);
        h.borrow().emit_mutation(DomMutation::ChildList {
            target: parent,
            added: Vec::new(),
            removed: vec![child],
            previous_sibling: prev_sib,
            next_sibling: next_sib,
        });
        if was_connected {
            notify_custom_element_subtree(vm, &h, child, false);
        }
        Ok(child_val)
    });

    // replaceChild(newChild, oldChild)
    let h = Rc::clone(host);
    define_host_method(vm, np, "replaceChild", move |vm, t, args| {
        let parent = require_node(vm, &h, t)?;
        let new_child = require_node(vm, &h, arg(args, 0))?;
        let old_val = arg(args, 1);
        let old_child = require_node(vm, &h, old_val)?;
        insert_node_into_parent(vm, &h, parent, new_child, Some(old_child))?;
        h.borrow().document.borrow_mut().remove_child(old_child);
        h.borrow().emit_mutation(DomMutation::ChildList {
            target: parent,
            added: Vec::new(),
            removed: vec![old_child],
            previous_sibling: Some(new_child),
            next_sibling: None,
        });
        Ok(old_val)
    });

    // cloneNode(deep?)
    let h = Rc::clone(host);
    define_host_method(vm, np, "cloneNode", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let deep = vm.to_boolean(arg(args, 0));
        let cloned = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let mut doc = _doc_rc.borrow_mut();
            if deep {
                let snapshot = doc.clone();
                doc.clone_subtree_from(&snapshot, nid)
            } else {
                let data = doc.get(nid).cloned().unwrap_or(NodeData::DocumentFragment);
                doc.new_node(data)
            }
        };
        Ok(wrap_node(vm, &h, cloned))
    });

    // contains(other)
    let h = Rc::clone(host);
    define_host_method(vm, np, "contains", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let Some(other) = h.borrow().node_for_value(vm, arg(args, 0)) else {
            return Ok(JsValue::FALSE);
        };
        let found = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let doc = _doc_rc.borrow();
            nid == other || doc.ancestors(other).any(|a| a == nid)
        };
        Ok(JsValue::from_bool(found))
    });
}

fn insert_node_into_parent(
    vm: &mut JsVm,
    host: &SharedHost,
    parent: NodeId,
    child: NodeId,
    before: Option<NodeId>,
) -> Result<(), JsException> {
    let is_frag = matches!(
        host.borrow().document.borrow().get(child),
        Some(NodeData::DocumentFragment)
    );
    let to_insert: Vec<NodeId> = if is_frag {
        host.borrow().document.borrow().children(child).collect()
    } else {
        vec![child]
    };
    if to_insert.is_empty() {
        return Ok(());
    }
    let parent_connected = host.borrow().document.borrow().is_connected(parent);
    let (prev_sib, next_sib) = {
        let _doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = _doc_rc.borrow_mut();
        if let Some(ref_n) = before {
            if doc.parent(ref_n) != Some(parent) {
                return Err(vm.make_error(
                    "NotFoundError",
                    "The reference node is not a child of this parent",
                ));
            }
        }
        let prev = before
            .and_then(|r| doc.previous_sibling(r))
            .or_else(|| doc.last_child(parent));
        for &n in &to_insert {
            if let Some(ref_n) = before {
                doc.insert_before(ref_n, n);
            } else {
                doc.append_child(parent, n);
            }
        }
        (prev, before)
    };
    host.borrow().emit_mutation(DomMutation::ChildList {
        target: parent,
        added: to_insert.clone(),
        removed: Vec::new(),
        previous_sibling: prev_sib,
        next_sibling: next_sib,
    });
    if parent_connected {
        for n in to_insert {
            notify_custom_element_subtree(vm, host, n, true);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Element.prototype & HTMLElement.prototype
// ---------------------------------------------------------------------------

fn install_element_prototype(vm: &mut JsVm, host: &SharedHost) {
    let ep = host.borrow().prototypes.element_proto;

    // tagName
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get tagName", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let tag = match h.borrow().document.borrow().get(nid) {
            Some(NodeData::Element(e)) => e.name.local.as_str().to_ascii_uppercase(),
            _ => String::new(),
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(tag)))
    });
    define_host_accessor(vm, ep, "tagName", Some(g), None);

    // id (getter + setter)
    let h1 = Rc::clone(host);
    let g = vm.alloc_host_fn("get id", move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        let id = get_attr_string(&h1, nid, "id").unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(id)))
    });
    let h2 = Rc::clone(host);
    let s = vm.alloc_host_fn("set id", move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let val = vm.value_to_rust_string(arg(args, 0));
        set_element_attr(vm, &h2, nid, "id", &val);
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, ep, "id", Some(g), Some(s));

    // className (getter + setter)
    let h1 = Rc::clone(host);
    let g = vm.alloc_host_fn("get className", move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        let cls = get_attr_string(&h1, nid, "class").unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(cls)))
    });
    let h2 = Rc::clone(host);
    let s = vm.alloc_host_fn("set className", move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let val = vm.value_to_rust_string(arg(args, 0));
        set_element_attr(vm, &h2, nid, "class", &val);
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, ep, "className", Some(g), Some(s));

    // classList (live DOMTokenList)
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get classList", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        Ok(JsValue::from_object(create_class_list_obj(vm, &h, nid)))
    });
    define_host_accessor(vm, ep, "classList", Some(g), None);

    // style (live CSSStyleDeclaration Proxy)
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get style", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        Ok(create_inline_style_proxy(vm, &h, nid))
    });
    define_host_accessor(vm, ep, "style", Some(g), None);

    // dataset (live Proxy over `data-*` attributes)
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get dataset", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        Ok(create_dataset_proxy(vm, &h, nid))
    });
    define_host_accessor(vm, ep, "dataset", Some(g), None);

    // Attribute methods
    let h = Rc::clone(host);
    define_host_method(vm, ep, "getAttribute", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let name = vm.value_to_rust_string(arg(args, 0));
        Ok(match get_attr_string(&h, nid, &name) {
            Some(v) => JsValue::from_string(vm.heap.alloc_string(v)),
            None => JsValue::NULL,
        })
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "setAttribute", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let name = vm.value_to_rust_string(arg(args, 0));
        let val = vm.value_to_rust_string(arg(args, 1));
        set_element_attr(vm, &h, nid, &name, &val);
        Ok(JsValue::UNDEFINED)
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "removeAttribute", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let name = vm.value_to_rust_string(arg(args, 0));
        remove_element_attr(vm, &h, nid, &name);
        Ok(JsValue::UNDEFINED)
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "hasAttribute", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let name = vm.value_to_rust_string(arg(args, 0));
        Ok(JsValue::from_bool(
            get_attr_string(&h, nid, &name).is_some(),
        ))
    });

    // Element traversal getters
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get children", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let kids: Vec<NodeId> = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let doc = _doc_rc.borrow();
            doc.children(nid)
                .filter(|&c| matches!(doc.get(c), Some(NodeData::Element(_))))
                .collect()
        };
        Ok(wrap_node_list(vm, &h, kids))
    });
    define_host_accessor(vm, ep, "children", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get firstElementChild", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let c = h.borrow().document.borrow().first_element_child(nid);
        Ok(c.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, ep, "firstElementChild", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get lastElementChild", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let c = h.borrow().document.borrow().last_element_child(nid);
        Ok(c.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, ep, "lastElementChild", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get nextElementSibling", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let c = h.borrow().document.borrow().next_element_sibling(nid);
        Ok(c.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, ep, "nextElementSibling", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get previousElementSibling", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let c = h.borrow().document.borrow().previous_element_sibling(nid);
        Ok(c.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, ep, "previousElementSibling", Some(g), None);

    // Selector queries: querySelector, querySelectorAll, matches, closest, getElementsBy*
    let h = Rc::clone(host);
    define_host_method(vm, ep, "querySelector", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let sel = vm.value_to_rust_string(arg(args, 0));
        let found = h.borrow().document.borrow().query_selector(nid, &sel);
        Ok(found.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "querySelectorAll", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let sel = vm.value_to_rust_string(arg(args, 0));
        let list = h.borrow().document.borrow().query_selector_all(nid, &sel);
        Ok(wrap_node_list(vm, &h, list))
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "matches", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let sel = vm.value_to_rust_string(arg(args, 0));
        let parsed =
            ComplexSelector::parse_shared(&sel).map_err(|e| vm.make_error("SyntaxError", e))?;
        let _doc_rc = Rc::clone(&h.borrow().document);
        let doc = _doc_rc.borrow();
        let ok = parsed.iter().any(|cs| matches_complex(&doc, nid, cs));
        Ok(JsValue::from_bool(ok))
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "closest", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let sel = vm.value_to_rust_string(arg(args, 0));
        let parsed =
            ComplexSelector::parse_shared(&sel).map_err(|e| vm.make_error("SyntaxError", e))?;
        let found = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let doc = _doc_rc.borrow();
            let mut cur = Some(nid);
            let mut matched = None;
            while let Some(n) = cur {
                if matches!(doc.get(n), Some(NodeData::Element(_)))
                    && parsed.iter().any(|cs| matches_complex(&doc, n, cs))
                {
                    matched = Some(n);
                    break;
                }
                cur = doc.parent(n);
            }
            matched
        };
        Ok(found.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "getElementsByTagName", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let tag = vm.value_to_rust_string(arg(args, 0));
        let found = find_elements_by_tag(&h, nid, &tag);
        Ok(wrap_node_list(vm, &h, found))
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "getElementsByClassName", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let cls = vm.value_to_rust_string(arg(args, 0));
        let found = find_elements_by_class(&h, nid, &cls);
        Ok(wrap_node_list(vm, &h, found))
    });

    // innerHTML / outerHTML
    let h1 = Rc::clone(host);
    let g = vm.alloc_host_fn("get innerHTML", move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        let html = {
            let _doc_rc = Rc::clone(&h1.borrow().document);
            let doc = _doc_rc.borrow();
            let mut buf = String::new();
            for c in doc.children(nid) {
                buf.push_str(&doc.to_html_string(c));
            }
            buf
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(html)))
    });
    let h2 = Rc::clone(host);
    let s = vm.alloc_host_fn("set innerHTML", move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let html = vm.value_to_rust_string(arg(args, 0));
        set_inner_html(vm, &h2, nid, &html);
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, ep, "innerHTML", Some(g), Some(s));

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get outerHTML", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let html = h.borrow().document.borrow().to_html_string(nid);
        Ok(JsValue::from_string(vm.heap.alloc_string(html)))
    });
    define_host_accessor(vm, ep, "outerHTML", Some(g), None);

    // Modern DOM convenience methods: remove(), append(...), prepend(...)
    let h = Rc::clone(host);
    define_host_method(vm, ep, "remove", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let info = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let doc = _doc_rc.borrow();
            doc.parent(nid).map(|p| {
                (
                    p,
                    doc.is_connected(nid),
                    doc.previous_sibling(nid),
                    doc.next_sibling(nid),
                )
            })
        };
        if let Some((parent, was_conn, prev, next)) = info {
            h.borrow().document.borrow_mut().remove_child(nid);
            h.borrow().emit_mutation(DomMutation::ChildList {
                target: parent,
                added: Vec::new(),
                removed: vec![nid],
                previous_sibling: prev,
                next_sibling: next,
            });
            if was_conn {
                notify_custom_element_subtree(vm, &h, nid, false);
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, ep, "append", move |vm, t, args| {
        let parent = require_node(vm, &h, t)?;
        for &a in args {
            let child = coerce_node_or_text(vm, &h, a);
            insert_node_into_parent(vm, &h, parent, child, None)?;
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, ep, "prepend", move |vm, t, args| {
        let parent = require_node(vm, &h, t)?;
        let first = h.borrow().document.borrow().first_child(parent);
        for &a in args {
            let child = coerce_node_or_text(vm, &h, a);
            insert_node_into_parent(vm, &h, parent, child, first)?;
        }
        Ok(JsValue::UNDEFINED)
    });

    // Shadow DOM: attachShadow({ mode }) & shadowRoot getter
    let h = Rc::clone(host);
    define_host_method(vm, ep, "attachShadow", move |vm, t, args| {
        let host_id = require_node(vm, &h, t)?;
        let opts = arg(args, 0);
        let Some(opts_h) = opts.as_object() else {
            return Err(vm.type_error("attachShadow requires an options object with 'mode'"));
        };
        let k_mode = vm.heap.intern_key("mode");
        let mode_val = vm.get_property_by_key(opts_h, k_mode)?;
        let mode = vm.value_to_rust_string(mode_val);
        if mode != "open" && mode != "closed" {
            return Err(vm.type_error("ShadowRoot mode must be 'open' or 'closed'"));
        }
        let sr_id = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let mut doc = _doc_rc.borrow_mut();
            let valid = match doc.get(host_id) {
                Some(NodeData::Element(e)) => {
                    e.shadow_root.is_none() && is_valid_shadow_host_name(&e.name.local)
                }
                _ => false,
            };
            if !valid {
                return Err(vm.make_error(
                    "NotSupportedError",
                    "Element cannot attach a shadow root or already has one",
                ));
            }
            let sr = doc.new_node(NodeData::ShadowRoot {
                host: host_id,
                mode,
                delegates_focus: false,
                clonable: false,
            });
            if let Some(NodeData::Element(e)) = doc.get_mut(host_id) {
                e.shadow_root = Some(sr);
            }
            sr
        };
        Ok(wrap_node(vm, &h, sr_id))
    });

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get shadowRoot", move |vm, t, _| {
        let host_id = require_node(vm, &h, t)?;
        let open_sr = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let doc = _doc_rc.borrow();
            match doc.get(host_id) {
                Some(NodeData::Element(e)) => e.shadow_root.filter(|&sr| {
                    matches!(doc.get(sr), Some(NodeData::ShadowRoot { mode, .. }) if mode == "open")
                }),
                _ => None,
            }
        };
        Ok(open_sr.map_or(JsValue::NULL, |sr| wrap_node(vm, &h, sr)))
    });
    define_host_accessor(vm, ep, "shadowRoot", Some(g), None);

    // Dirty-checked incremental geometry queries (`getBoundingClientRect`, `offset*`, `client*`)
    let h = Rc::clone(host);
    define_host_method(vm, ep, "getBoundingClientRect", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let rect = h
            .borrow_mut()
            .bounding_rects_for_node(nid)
            .map_or(Rect::ZERO, |(b, _)| b);
        Ok(JsValue::from_object(alloc_dom_rect(vm, rect)))
    });

    for (prop, field) in [
        ("offsetLeft", 0u8),
        ("offsetTop", 1),
        ("offsetWidth", 2),
        ("offsetHeight", 3),
        ("clientWidth", 4),
        ("clientHeight", 5),
    ] {
        let h = Rc::clone(host);
        let g = vm.alloc_host_fn(prop, move |vm, t, _| {
            let nid = require_node(vm, &h, t)?;
            let (border, content) = h
                .borrow_mut()
                .bounding_rects_for_node(nid)
                .unwrap_or((Rect::ZERO, Rect::ZERO));
            let val = match field {
                0 => border.origin.x,
                1 => border.origin.y,
                2 => border.size.width,
                3 => border.size.height,
                4 => content.size.width,
                _ => content.size.height,
            };
            Ok(JsValue::from_f64(f64::from(val.round())))
        });
        define_host_accessor(vm, ep, prop, Some(g), None);
    }
}

fn alloc_dom_rect(vm: &mut JsVm, rect: Rect) -> HeapHandle {
    let obj = vm.alloc_ordinary_object();
    let fields = [
        ("x", f64::from(rect.origin.x)),
        ("y", f64::from(rect.origin.y)),
        ("left", f64::from(rect.origin.x)),
        ("top", f64::from(rect.origin.y)),
        ("width", f64::from(rect.size.width)),
        ("height", f64::from(rect.size.height)),
        ("right", f64::from(rect.origin.x + rect.size.width)),
        ("bottom", f64::from(rect.origin.y + rect.size.height)),
    ];
    for (name, val) in fields {
        let k = vm.heap.intern_key(name);
        vm.heap.object_define_data(
            obj,
            k,
            JsValue::from_f64(val),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
    }
    obj
}

fn coerce_node_or_text(vm: &JsVm, host: &SharedHost, val: JsValue) -> NodeId {
    if let Some(nid) = host.borrow().node_for_value(vm, val) {
        return nid;
    }
    let text = vm.value_to_rust_string(val);
    host.borrow().document.borrow_mut().create_text(&text)
}

fn set_inner_html(vm: &mut JsVm, host: &SharedHost, nid: NodeId, html: &str) {
    let ctx_name = match host.borrow().document.borrow().get(nid) {
        Some(NodeData::Element(e)) => Some(e.name.clone()),
        _ => None,
    };
    let (frag_doc, frag_roots) = parse_html_fragment(html, ctx_name.as_ref());
    let (removed, added, is_conn) = {
        let _doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = _doc_rc.borrow_mut();
        let removed: Vec<NodeId> = doc.children(nid).collect();
        for &c in &removed {
            doc.remove_child(c);
        }
        let mut added = Vec::with_capacity(frag_roots.len());
        for src_root in frag_roots {
            let cloned = doc.clone_subtree_from(&frag_doc, src_root);
            doc.append_child(nid, cloned);
            added.push(cloned);
        }
        let is_conn = doc.is_connected(nid);
        (removed, added, is_conn)
    };
    host.borrow().emit_mutation(DomMutation::ChildList {
        target: nid,
        added: added.clone(),
        removed,
        previous_sibling: None,
        next_sibling: None,
    });
    if is_conn {
        for n in added {
            notify_custom_element_subtree(vm, host, n, true);
        }
    }
}

// ---------------------------------------------------------------------------
// Document.prototype & ShadowRoot.prototype
// ---------------------------------------------------------------------------

fn install_document_prototype(vm: &mut JsVm, host: &SharedHost) {
    let dp = host.borrow().prototypes.document_proto;

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get documentElement", move |vm, _, _| {
        let el = h.borrow().document.borrow().document_element();
        Ok(el.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, dp, "documentElement", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get head", move |vm, _, _| {
        let root = h.borrow().document.borrow().root();
        let el = find_elements_by_tag(&h, root, "head").into_iter().next();
        Ok(el.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, dp, "head", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get body", move |vm, _, _| {
        let root = h.borrow().document.borrow().root();
        let el = find_elements_by_tag(&h, root, "body").into_iter().next();
        Ok(el.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, dp, "body", Some(g), None);

    let h1 = Rc::clone(host);
    let g = vm.alloc_host_fn("get title", move |vm, _, _| {
        let title = {
            let _doc_rc = Rc::clone(&h1.borrow().document);
            let doc = _doc_rc.borrow();
            doc.document_title(doc.root())
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(title)))
    });
    let h2 = Rc::clone(host);
    let s = vm.alloc_host_fn("set title", move |vm, _, args| {
        let new_title = vm.value_to_rust_string(arg(args, 0));
        let _doc_rc = Rc::clone(&h2.borrow().document);
        let mut doc = _doc_rc.borrow_mut();
        let root = doc.root();
        if let Some(title_el) = doc.document_title_element(root) {
            let kids: Vec<NodeId> = doc.children(title_el).collect();
            for c in kids {
                doc.remove_child(c);
            }
            let tn = doc.create_text(&new_title);
            doc.append_child(title_el, tn);
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, dp, "title", Some(g), Some(s));

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get readyState", move |vm, _, _| {
        let rs = h.borrow().ready_state.clone();
        Ok(JsValue::from_string(vm.heap.alloc_string(rs)))
    });
    define_host_accessor(vm, dp, "readyState", Some(g), None);

    // Node factory methods
    let h = Rc::clone(host);
    define_host_method(vm, dp, "createElement", move |vm, _, args| {
        let raw_tag = vm.value_to_rust_string(arg(args, 0));
        let tag = raw_tag.to_ascii_lowercase();
        let nid = h.borrow().document.borrow_mut().create_element(&tag);
        let wrapper = wrap_node(vm, &h, nid);
        upgrade_if_custom_element(vm, &h, nid, &tag);
        Ok(wrapper)
    });
    let h = Rc::clone(host);
    define_host_method(vm, dp, "createTextNode", move |vm, _, args| {
        let text = vm.value_to_rust_string(arg(args, 0));
        let nid = h.borrow().document.borrow_mut().create_text(&text);
        Ok(wrap_node(vm, &h, nid))
    });
    let h = Rc::clone(host);
    define_host_method(vm, dp, "createComment", move |vm, _, args| {
        let text = vm.value_to_rust_string(arg(args, 0));
        let nid = h.borrow().document.borrow_mut().create_comment(&text);
        Ok(wrap_node(vm, &h, nid))
    });
    let h = Rc::clone(host);
    define_host_method(vm, dp, "createDocumentFragment", move |vm, _, _| {
        let nid = h
            .borrow()
            .document
            .borrow_mut()
            .new_node(NodeData::DocumentFragment);
        Ok(wrap_node(vm, &h, nid))
    });

    // Document queries
    let h = Rc::clone(host);
    define_host_method(vm, dp, "getElementById", move |vm, _, args| {
        let id = vm.value_to_rust_string(arg(args, 0));
        let found = h.borrow().document.borrow().get_element_by_id(&id);
        Ok(found.map_or(JsValue::NULL, |nid| wrap_node(vm, &h, nid)))
    });
    let h = Rc::clone(host);
    define_host_method(vm, dp, "querySelector", move |vm, _, args| {
        let sel = vm.value_to_rust_string(arg(args, 0));
        let found = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let doc = _doc_rc.borrow();
            doc.query_selector(doc.root(), &sel)
        };
        Ok(found.map_or(JsValue::NULL, |nid| wrap_node(vm, &h, nid)))
    });
    let h = Rc::clone(host);
    define_host_method(vm, dp, "querySelectorAll", move |vm, _, args| {
        let sel = vm.value_to_rust_string(arg(args, 0));
        let list = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let doc = _doc_rc.borrow();
            doc.query_selector_all(doc.root(), &sel)
        };
        Ok(wrap_node_list(vm, &h, list))
    });
    let h = Rc::clone(host);
    define_host_method(vm, dp, "getElementsByTagName", move |vm, _, args| {
        let tag = vm.value_to_rust_string(arg(args, 0));
        let root = h.borrow().document.borrow().root();
        let list = find_elements_by_tag(&h, root, &tag);
        Ok(wrap_node_list(vm, &h, list))
    });
    let h = Rc::clone(host);
    define_host_method(vm, dp, "getElementsByClassName", move |vm, _, args| {
        let cls = vm.value_to_rust_string(arg(args, 0));
        let root = h.borrow().document.borrow().root();
        let list = find_elements_by_class(&h, root, &cls);
        Ok(wrap_node_list(vm, &h, list))
    });

    // document.write(...) / document.writeln(...) -> injects into paused HtmlParserStream!
    let h = Rc::clone(host);
    define_host_method(vm, dp, "write", move |vm, _, args| {
        let mut html = String::new();
        for &a in args {
            html.push_str(&vm.value_to_rust_string(a));
        }
        let parser_opt = h.borrow().active_parser.clone();
        if let Some(parser) = parser_opt {
            parser.borrow_mut().insert_document_write(&html);
        }
        Ok(JsValue::UNDEFINED)
    });
    let h = Rc::clone(host);
    define_host_method(vm, dp, "writeln", move |vm, _, args| {
        let mut html = String::new();
        for &a in args {
            html.push_str(&vm.value_to_rust_string(a));
        }
        html.push('\n');
        let parser_opt = h.borrow().active_parser.clone();
        if let Some(parser) = parser_opt {
            parser.borrow_mut().insert_document_write(&html);
        }
        Ok(JsValue::UNDEFINED)
    });
}

fn install_shadow_root_prototype(vm: &mut JsVm, host: &SharedHost) {
    let sp = host.borrow().prototypes.shadow_root_proto;

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get host", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let host_el = match h.borrow().document.borrow().get(nid) {
            Some(NodeData::ShadowRoot { host: he, .. }) => Some(*he),
            _ => None,
        };
        Ok(host_el.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, sp, "host", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get mode", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let mode = match h.borrow().document.borrow().get(nid) {
            Some(NodeData::ShadowRoot { mode, .. }) => mode.clone(),
            _ => String::new(),
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(mode)))
    });
    define_host_accessor(vm, sp, "mode", Some(g), None);

    let h1 = Rc::clone(host);
    let g = vm.alloc_host_fn("get innerHTML", move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        let html = {
            let _doc_rc = Rc::clone(&h1.borrow().document);
            let doc = _doc_rc.borrow();
            let mut buf = String::new();
            for c in doc.children(nid) {
                buf.push_str(&doc.to_html_string(c));
            }
            buf
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(html)))
    });
    let h2 = Rc::clone(host);
    let s = vm.alloc_host_fn("set innerHTML", move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let html = vm.value_to_rust_string(arg(args, 0));
        set_inner_html(vm, &h2, nid, &html);
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, sp, "innerHTML", Some(g), Some(s));

    let h = Rc::clone(host);
    define_host_method(vm, sp, "querySelector", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let sel = vm.value_to_rust_string(arg(args, 0));
        let found = h.borrow().document.borrow().query_selector(nid, &sel);
        Ok(found.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    let h = Rc::clone(host);
    define_host_method(vm, sp, "querySelectorAll", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let sel = vm.value_to_rust_string(arg(args, 0));
        let list = h.borrow().document.borrow().query_selector_all(nid, &sel);
        Ok(wrap_node_list(vm, &h, list))
    });
    let h = Rc::clone(host);
    define_host_method(vm, sp, "getElementById", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let id = vm.value_to_rust_string(arg(args, 0));
        let found = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let doc = _doc_rc.borrow();
            let found_id = doc.descendants(nid).find(|&n| {
                matches!(doc.get(n), Some(NodeData::Element(e)) if e.get_attribute("id") == Some(id.as_str()))
            });
            found_id
        };
        Ok(found.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
}

// ---------------------------------------------------------------------------
// Window: CustomElementRegistry & CSSOM (`getComputedStyle`)
// ---------------------------------------------------------------------------

fn install_window_and_cssom(vm: &mut JsVm, host: &SharedHost) {
    // Expose base constructors (`HTMLElement`, `Element`, `Node`, `EventTarget`, `Document`).
    let protos = host.borrow().prototypes;
    for (name, proto) in [
        ("EventTarget", protos.event_target_proto),
        ("Node", protos.node_proto),
        ("Element", protos.element_proto),
        ("HTMLElement", protos.html_element_proto),
        ("Document", protos.document_proto),
        ("ShadowRoot", protos.shadow_root_proto),
    ] {
        let ctor = vm.alloc_host_fn(name, |_vm, this_val, _| Ok(this_val));
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
        define_global(vm, name, JsValue::from_object(ctor));
    }

    // window.customElements (`CustomElementRegistry`)
    let ce_reg = vm.alloc_ordinary_object();
    let h = Rc::clone(host);
    define_host_method(vm, ce_reg, "define", move |vm, _, args| {
        let name = vm.value_to_rust_string(arg(args, 0));
        if !is_valid_custom_element_name(&name) {
            return Err(vm.make_error(
                "SyntaxError",
                format!("'{name}' is not a valid custom element name"),
            ));
        }
        let Some(ctor) = arg(args, 1).as_object() else {
            return Err(vm.type_error("customElements.define requires a constructor function"));
        };
        if !vm.is_callable_handle(ctor) {
            return Err(vm.type_error("customElements.define requires a constructor function"));
        }
        if h.borrow().custom_elements.contains_key(&name) {
            return Err(vm.make_error(
                "NotSupportedError",
                format!("Custom element '{name}' is already defined"),
            ));
        }
        // Read optional `static observedAttributes` array on constructor.
        let k_obs = vm.heap.intern_key("observedAttributes");
        let obs_val = vm
            .get_property_by_key(ctor, k_obs)
            .unwrap_or(JsValue::UNDEFINED);
        let mut observed_attributes = Vec::new();
        if obs_val.is_object() {
            if let Ok(items) = vm.collect_iterable(obs_val) {
                for item in items {
                    observed_attributes.push(vm.value_to_rust_string(item));
                }
            }
        }
        let waiters = {
            let mut hs = h.borrow_mut();
            hs.custom_elements.insert(
                name.clone(),
                CustomElementDef {
                    constructor: ctor,
                    observed_attributes,
                },
            );
            hs.when_defined_waiters.remove(&name).unwrap_or_default()
        };
        for p in waiters {
            resolve_promise(vm, p, JsValue::from_object(ctor));
        }
        // Upgrade any already-connected matching elements in the document.
        let root = h.borrow().document.borrow().root();
        let matching = find_elements_by_tag(&h, root, &name);
        for nid in matching {
            upgrade_if_custom_element(vm, &h, nid, &name);
            invoke_custom_element_lifecycle(vm, &h, nid, "connectedCallback", &[]);
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, ce_reg, "get", move |_vm, _, args| {
        let name = _vm.value_to_rust_string(arg(args, 0));
        Ok(h.borrow()
            .custom_elements
            .get(&name)
            .map_or(JsValue::UNDEFINED, |d| JsValue::from_object(d.constructor)))
    });

    let h = Rc::clone(host);
    define_host_method(vm, ce_reg, "whenDefined", move |vm, _, args| {
        let name = vm.value_to_rust_string(arg(args, 0));
        let p = alloc_promise(vm);
        let existing = h.borrow().custom_elements.get(&name).map(|d| d.constructor);
        if let Some(ctor) = existing {
            resolve_promise(vm, p, JsValue::from_object(ctor));
        } else {
            h.borrow_mut()
                .when_defined_waiters
                .entry(name)
                .or_default()
                .push(p);
        }
        Ok(JsValue::from_object(p))
    });
    define_global(vm, "customElements", JsValue::from_object(ce_reg));

    // window.getComputedStyle(element)
    let h = Rc::clone(host);
    let gcs = vm.alloc_host_fn("getComputedStyle", move |vm, _, args| {
        let nid = require_node(vm, &h, arg(args, 0))?;
        let style = h
            .borrow_mut()
            .computed_style_for_node(nid)
            .unwrap_or_else(|| Arc::new(ComputedStyle::default()));
        Ok(create_computed_style_proxy(vm, style))
    });
    define_global(vm, "getComputedStyle", JsValue::from_object(gcs));
}

fn upgrade_if_custom_element(vm: &mut JsVm, host: &SharedHost, nid: NodeId, tag: &str) {
    let def_opt = host.borrow().custom_elements.get(tag).cloned();
    let Some(def) = def_opt else {
        return;
    };
    let wrapper = wrap_node(vm, host, nid);
    if let Some(wh) = wrapper.as_object() {
        if let Ok(ctor_proto) = vm.get_property_by_key(def.constructor, vm.key_prototype) {
            if let Some(ph) = ctor_proto.as_object() {
                vm.heap.object_set_prototype(wh, Some(ph), &mut vm.shapes);
            }
        }
    }
}

fn notify_custom_element_subtree(vm: &mut JsVm, host: &SharedHost, root: NodeId, connected: bool) {
    if host.borrow().custom_elements.is_empty() {
        return;
    }
    let nodes: Vec<(NodeId, String)> = {
        let _doc_rc = Rc::clone(&host.borrow().document);
        let doc = _doc_rc.borrow();
        let mut out = Vec::new();
        for n in doc.descendants(root) {
            if let Some(NodeData::Element(e)) = doc.get(n) {
                let tag = e.name.local.as_str().to_ascii_lowercase();
                if host.borrow().custom_elements.contains_key(&tag) {
                    out.push((n, tag));
                }
            }
        }
        out
    };
    let cb_name = if connected {
        "connectedCallback"
    } else {
        "disconnectedCallback"
    };
    for (nid, tag) in nodes {
        if connected {
            upgrade_if_custom_element(vm, host, nid, &tag);
        }
        invoke_custom_element_lifecycle(vm, host, nid, cb_name, &[]);
    }
}

fn invoke_custom_element_lifecycle(
    vm: &mut JsVm,
    host: &SharedHost,
    nid: NodeId,
    method: &str,
    args: &[JsValue],
) {
    let wrapper = wrap_node(vm, host, nid);
    let Some(wh) = wrapper.as_object() else {
        return;
    };
    let k = vm.heap.intern_key(method);
    if let Ok(cb) = vm.get_property_by_key(wh, k) {
        if cb.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
            let _ = vm.call_value(cb, wrapper, args);
        }
    }
}

// ---------------------------------------------------------------------------
// Attributes, classList, inline style Proxy, dataset Proxy & getComputedStyle
// ---------------------------------------------------------------------------

fn get_attr_string(host: &SharedHost, nid: NodeId, name: &str) -> Option<String> {
    let _doc_rc = Rc::clone(&host.borrow().document);
    let doc = _doc_rc.borrow();
    match doc.get(nid) {
        Some(NodeData::Element(e)) => e.get_attribute(name).map(str::to_string),
        _ => None,
    }
}

pub(crate) fn set_element_attr(
    vm: &mut JsVm,
    host: &SharedHost,
    nid: NodeId,
    name: &str,
    val: &str,
) {
    let (old_val, tag_lower) = {
        let _doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = _doc_rc.borrow_mut();
        let (old, tag) = match doc.get_mut(nid) {
            Some(NodeData::Element(e)) => {
                let old = e.get_attribute(name).map(str::to_string);
                let tag = e.name.local.as_str().to_ascii_lowercase();
                e.set_attribute(name, val);
                (old, tag)
            }
            _ => return,
        };
        doc.bump_style_revision_for(nid);
        (old, tag)
    };
    host.borrow().emit_mutation(DomMutation::Attributes {
        target: nid,
        attribute_name: name.to_string(),
        old_value: old_val.clone(),
    });
    let observed = host
        .borrow()
        .custom_elements
        .get(&tag_lower)
        .is_some_and(|d| d.observed_attributes.iter().any(|a| a == name));
    if observed {
        let n_val = JsValue::from_string(vm.heap.alloc_string(name));
        let o_val = old_val.map_or(JsValue::NULL, |s| {
            JsValue::from_string(vm.heap.alloc_string(s))
        });
        let v_val = JsValue::from_string(vm.heap.alloc_string(val));
        invoke_custom_element_lifecycle(
            vm,
            host,
            nid,
            "attributeChangedCallback",
            &[n_val, o_val, v_val],
        );
    }
}

fn remove_element_attr(vm: &mut JsVm, host: &SharedHost, nid: NodeId, name: &str) {
    let (old_val, tag_lower) = {
        let _doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = _doc_rc.borrow_mut();
        let (old, tag) = match doc.get_mut(nid) {
            Some(NodeData::Element(e)) => {
                let old = e.get_attribute(name).map(str::to_string);
                let tag = e.name.local.as_str().to_ascii_lowercase();
                e.remove_attribute(name);
                (old, tag)
            }
            _ => return,
        };
        doc.bump_style_revision_for(nid);
        (old, tag)
    };
    if old_val.is_some() {
        host.borrow().emit_mutation(DomMutation::Attributes {
            target: nid,
            attribute_name: name.to_string(),
            old_value: old_val.clone(),
        });
        let observed = host
            .borrow()
            .custom_elements
            .get(&tag_lower)
            .is_some_and(|d| d.observed_attributes.iter().any(|a| a == name));
        if observed {
            let n_val = JsValue::from_string(vm.heap.alloc_string(name));
            let o_val = old_val.map_or(JsValue::NULL, |s| {
                JsValue::from_string(vm.heap.alloc_string(s))
            });
            invoke_custom_element_lifecycle(
                vm,
                host,
                nid,
                "attributeChangedCallback",
                &[n_val, o_val, JsValue::NULL],
            );
        }
    }
}

fn find_elements_by_tag(host: &SharedHost, root: NodeId, tag: &str) -> Vec<NodeId> {
    let _doc_rc = Rc::clone(&host.borrow().document);
    let doc = _doc_rc.borrow();
    let wildcard = tag == "*";
    doc.descendants(root)
        .filter(|&n| {
            n != root
                && matches!(doc.get(n), Some(NodeData::Element(e)) if wildcard || e.name.local.as_str().eq_ignore_ascii_case(tag))
        })
        .collect()
}

fn find_elements_by_class(host: &SharedHost, root: NodeId, classes: &str) -> Vec<NodeId> {
    let wanted: Vec<&str> = classes.split_ascii_whitespace().collect();
    if wanted.is_empty() {
        return Vec::new();
    }
    let _doc_rc = Rc::clone(&host.borrow().document);
    let doc = _doc_rc.borrow();
    doc.descendants(root)
        .filter(|&n| {
            n != root
                && matches!(doc.get(n), Some(NodeData::Element(e)) if {
                    let actual: Vec<&str> = e
                        .get_attribute("class")
                        .unwrap_or("")
                        .split_ascii_whitespace()
                        .collect();
                    wanted.iter().all(|w| actual.contains(w))
                })
        })
        .collect()
}

fn create_class_list_obj(vm: &mut JsVm, host: &SharedHost, nid: NodeId) -> HeapHandle {
    let obj = vm.alloc_ordinary_object();
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get length", move |_vm, _, _| {
        let len =
            get_attr_string(&h, nid, "class").map_or(0, |s| s.split_ascii_whitespace().count());
        Ok(JsValue::from_i32(len as i32))
    });
    define_host_accessor(vm, obj, "length", Some(g), None);

    let h = Rc::clone(host);
    define_host_method(vm, obj, "contains", move |vm, _, args| {
        let token = vm.value_to_rust_string(arg(args, 0));
        let has = get_attr_string(&h, nid, "class")
            .is_some_and(|s| s.split_ascii_whitespace().any(|t| t == token));
        Ok(JsValue::from_bool(has))
    });

    let h = Rc::clone(host);
    define_host_method(vm, obj, "add", move |vm, _, args| {
        let mut tokens: Vec<String> = get_attr_string(&h, nid, "class")
            .unwrap_or_default()
            .split_ascii_whitespace()
            .map(str::to_string)
            .collect();
        for &a in args {
            let t = vm.value_to_rust_string(a);
            if !t.is_empty() && !tokens.contains(&t) {
                tokens.push(t);
            }
        }
        set_element_attr(vm, &h, nid, "class", &tokens.join(" "));
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, obj, "remove", move |vm, _, args| {
        let to_rm: Vec<String> = args.iter().map(|&a| vm.value_to_rust_string(a)).collect();
        let tokens: Vec<String> = get_attr_string(&h, nid, "class")
            .unwrap_or_default()
            .split_ascii_whitespace()
            .filter(|t| !to_rm.iter().any(|r| r == t))
            .map(str::to_string)
            .collect();
        set_element_attr(vm, &h, nid, "class", &tokens.join(" "));
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, obj, "toggle", move |vm, _, args| {
        let token = vm.value_to_rust_string(arg(args, 0));
        let force = args
            .get(1)
            .copied()
            .filter(|v| !v.is_undefined())
            .map(|v| vm.to_boolean(v));
        let mut tokens: Vec<String> = get_attr_string(&h, nid, "class")
            .unwrap_or_default()
            .split_ascii_whitespace()
            .map(str::to_string)
            .collect();
        let exists = tokens.contains(&token);
        let result = match (exists, force) {
            (true, Some(true)) => true,
            (true, _) => {
                tokens.retain(|t| t != &token);
                false
            }
            (false, Some(false)) => false,
            (false, _) => {
                tokens.push(token);
                true
            }
        };
        set_element_attr(vm, &h, nid, "class", &tokens.join(" "));
        Ok(JsValue::from_bool(result))
    });

    let h = Rc::clone(host);
    define_host_method(vm, obj, "replace", move |vm, _, args| {
        let old_t = vm.value_to_rust_string(arg(args, 0));
        let new_t = vm.value_to_rust_string(arg(args, 1));
        let mut tokens: Vec<String> = get_attr_string(&h, nid, "class")
            .unwrap_or_default()
            .split_ascii_whitespace()
            .map(str::to_string)
            .collect();
        if let Some(pos) = tokens.iter().position(|t| t == &old_t) {
            tokens[pos] = new_t;
            set_element_attr(vm, &h, nid, "class", &tokens.join(" "));
            Ok(JsValue::TRUE)
        } else {
            Ok(JsValue::FALSE)
        }
    });
    obj
}

fn camel_to_kebab(input: &str) -> String {
    if input.starts_with("--") {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len() + 4);
    for ch in input.chars() {
        if ch.is_ascii_uppercase() {
            out.push('-');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn parse_style_decls(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for part in raw.split(';') {
        if let Some((k, v)) = part.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            if !key.is_empty() && !val.is_empty() {
                if let Some(existing) = out.iter_mut().find(|(ek, _)| ek == &key) {
                    existing.1 = val;
                } else {
                    out.push((key, val));
                }
            }
        }
    }
    out
}

fn serialize_style_decls(decls: &[(String, String)]) -> String {
    decls
        .iter()
        .map(|(k, v)| format!("{k}: {v};"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn create_inline_style_proxy(vm: &mut JsVm, host: &SharedHost, nid: NodeId) -> JsValue {
    let target = vm.alloc_ordinary_object();
    let h = Rc::clone(host);
    define_host_method(vm, target, "getPropertyValue", move |vm, _, args| {
        let prop = vm.value_to_rust_string(arg(args, 0)).to_ascii_lowercase();
        let raw = get_attr_string(&h, nid, "style").unwrap_or_default();
        let val = parse_style_decls(&raw)
            .into_iter()
            .find(|(k, _)| k == &prop)
            .map(|(_, v)| v)
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(val)))
    });
    let h = Rc::clone(host);
    define_host_method(vm, target, "setProperty", move |vm, _, args| {
        let prop = vm.value_to_rust_string(arg(args, 0)).to_ascii_lowercase();
        let val = vm.value_to_rust_string(arg(args, 1));
        let raw = get_attr_string(&h, nid, "style").unwrap_or_default();
        let mut decls = parse_style_decls(&raw);
        if val.is_empty() {
            decls.retain(|(k, _)| k != &prop);
        } else if let Some(existing) = decls.iter_mut().find(|(k, _)| k == &prop) {
            existing.1 = val;
        } else {
            decls.push((prop, val));
        }
        set_element_attr(vm, &h, nid, "style", &serialize_style_decls(&decls));
        Ok(JsValue::UNDEFINED)
    });
    let h = Rc::clone(host);
    define_host_method(vm, target, "removeProperty", move |vm, _, args| {
        let prop = vm.value_to_rust_string(arg(args, 0)).to_ascii_lowercase();
        let raw = get_attr_string(&h, nid, "style").unwrap_or_default();
        let mut decls = parse_style_decls(&raw);
        let old = decls
            .iter()
            .find(|(k, _)| k == &prop)
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        decls.retain(|(k, _)| k != &prop);
        set_element_attr(vm, &h, nid, "style", &serialize_style_decls(&decls));
        Ok(JsValue::from_string(vm.heap.alloc_string(old)))
    });

    let handler = vm.alloc_ordinary_object();
    let h_get = Rc::clone(host);
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let prop_name = vm.value_to_rust_string(arg(args, 1));
        if prop_name == "cssText" {
            let raw = get_attr_string(&h_get, nid, "style").unwrap_or_default();
            return Ok(JsValue::from_string(vm.heap.alloc_string(raw)));
        }
        let k = vm.heap.intern_key(&prop_name);
        if let Some(t_h) = arg(args, 0).as_object() {
            if vm.heap.object_get_own(t_h, k, &vm.shapes).is_some() {
                return vm.get_property_by_key(t_h, k);
            }
        }
        let css_key = camel_to_kebab(&prop_name);
        let raw = get_attr_string(&h_get, nid, "style").unwrap_or_default();
        let val = parse_style_decls(&raw)
            .into_iter()
            .find(|(k, _)| k == &css_key)
            .map(|(_, v)| v)
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(val)))
    });
    let h_set = Rc::clone(host);
    define_host_method(vm, handler, "set", move |vm, _, args| {
        let prop_name = vm.value_to_rust_string(arg(args, 1));
        let val = vm.value_to_rust_string(arg(args, 2));
        if prop_name == "cssText" {
            set_element_attr(vm, &h_set, nid, "style", &val);
            return Ok(JsValue::TRUE);
        }
        let css_key = camel_to_kebab(&prop_name);
        let raw = get_attr_string(&h_set, nid, "style").unwrap_or_default();
        let mut decls = parse_style_decls(&raw);
        if val.is_empty() {
            decls.retain(|(k, _)| k != &css_key);
        } else if let Some(existing) = decls.iter_mut().find(|(k, _)| k == &css_key) {
            existing.1 = val;
        } else {
            decls.push((css_key, val));
        }
        set_element_attr(vm, &h_set, nid, "style", &serialize_style_decls(&decls));
        Ok(JsValue::TRUE)
    });

    let proxy = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(rix_js::ProxyObject {
            target,
            handler,
            revoked: false,
        }),
        Some(vm.object_proto),
    ));
    JsValue::from_object(proxy)
}

fn create_dataset_proxy(vm: &mut JsVm, host: &SharedHost, nid: NodeId) -> JsValue {
    let target = vm.alloc_ordinary_object();
    let handler = vm.alloc_ordinary_object();
    let h1 = Rc::clone(host);
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let key = vm.value_to_rust_string(arg(args, 1));
        let attr = format!("data-{}", camel_to_kebab(&key));
        Ok(match get_attr_string(&h1, nid, &attr) {
            Some(v) => JsValue::from_string(vm.heap.alloc_string(v)),
            None => JsValue::UNDEFINED,
        })
    });
    let h2 = Rc::clone(host);
    define_host_method(vm, handler, "set", move |vm, _, args| {
        let key = vm.value_to_rust_string(arg(args, 1));
        let val = vm.value_to_rust_string(arg(args, 2));
        let attr = format!("data-{}", camel_to_kebab(&key));
        set_element_attr(vm, &h2, nid, &attr, &val);
        Ok(JsValue::TRUE)
    });
    let proxy = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(rix_js::ProxyObject {
            target,
            handler,
            revoked: false,
        }),
        Some(vm.object_proto),
    ));
    JsValue::from_object(proxy)
}

fn create_computed_style_proxy(vm: &mut JsVm, style: Arc<ComputedStyle>) -> JsValue {
    let target = vm.alloc_ordinary_object();
    let s1 = Arc::clone(&style);
    define_host_method(vm, target, "getPropertyValue", move |vm, _, args| {
        let prop = vm.value_to_rust_string(arg(args, 0));
        let val = format_computed_property(&s1, &prop);
        Ok(JsValue::from_string(vm.heap.alloc_string(val)))
    });
    let handler = vm.alloc_ordinary_object();
    let s2 = style;
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let prop = vm.value_to_rust_string(arg(args, 1));
        let k = vm.heap.intern_key(&prop);
        if let Some(t_h) = arg(args, 0).as_object() {
            if vm.heap.object_get_own(t_h, k, &vm.shapes).is_some() {
                return vm.get_property_by_key(t_h, k);
            }
        }
        let css_key = camel_to_kebab(&prop);
        let val = format_computed_property(&s2, &css_key);
        Ok(JsValue::from_string(vm.heap.alloc_string(val)))
    });
    let proxy = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(rix_js::ProxyObject {
            target,
            handler,
            revoked: false,
        }),
        Some(vm.object_proto),
    ));
    JsValue::from_object(proxy)
}

fn format_rgba(c: ColorRgba) -> String {
    if c.a == 255 {
        format!("rgb({}, {}, {})", c.r, c.g, c.b)
    } else {
        format!(
            "rgba({}, {}, {}, {:.3})",
            c.r,
            c.g,
            c.b,
            f32::from(c.a) / 255.0
        )
    }
}

fn format_size(s: &SizeValue) -> String {
    match s {
        SizeValue::Auto => "auto".into(),
        SizeValue::None => "none".into(),
        SizeValue::MinContent => "min-content".into(),
        SizeValue::MaxContent => "max-content".into(),
        SizeValue::FitContent(_) => "fit-content".into(),
        SizeValue::Stretch => "stretch".into(),
        SizeValue::Length(lp) => format_lp(lp),
    }
}

fn format_lp(lp: &LengthPercentage) -> String {
    match lp {
        LengthPercentage::Zero => "0px".into(),
        LengthPercentage::Length(v, _) => format!("{v}px"),
        LengthPercentage::Percent(p) => format!("{p}%"),
        LengthPercentage::Calc(_) => "0px".into(),
    }
}

fn format_lpa(v: &LengthPercentageAuto) -> String {
    match v {
        LengthPercentageAuto::Auto => "auto".into(),
        LengthPercentageAuto::Value(lp) => format_lp(lp),
    }
}

fn format_computed_property(style: &ComputedStyle, prop: &str) -> String {
    if prop.starts_with("--") {
        let key = rix_core::intern_str(prop);
        if let Some(tokens) = style.custom_properties.get(&key) {
            return tokens
                .iter()
                .map(|t| format!("{t:?}"))
                .collect::<Vec<_>>()
                .join(" ");
        }
        return String::new();
    }
    match prop {
        "display" => match style.display {
            DisplayMode::None => "none",
            DisplayMode::Block => "block",
            DisplayMode::Inline => "inline",
            DisplayMode::InlineBlock => "inline-block",
            DisplayMode::Flex => "flex",
            DisplayMode::InlineFlex => "inline-flex",
            DisplayMode::Grid => "grid",
            DisplayMode::InlineGrid => "inline-grid",
            DisplayMode::Table => "table",
            DisplayMode::ListItem => "list-item",
            DisplayMode::Contents => "contents",
            _ => "block",
        }
        .into(),
        "position" => match style.position {
            PositionMode::Static => "static",
            PositionMode::Relative => "relative",
            PositionMode::Absolute => "absolute",
            PositionMode::Fixed => "fixed",
            PositionMode::Sticky => "sticky",
        }
        .into(),
        "visibility" => match style.visibility {
            VisibilityMode::Visible => "visible",
            VisibilityMode::Hidden => "hidden",
            VisibilityMode::Collapse => "collapse",
        }
        .into(),
        "color" => format_rgba(style.text.color),
        "background-color" => format_rgba(style.visual.background_color),
        "opacity" => format!("{}", style.opacity),
        "font-size" => format!("{}px", style.text.font_size_px),
        "font-weight" => format!("{}", style.text.font_weight),
        "width" => format_size(&style.box_model.width),
        "height" => format_size(&style.box_model.height),
        "margin-top" => format_lpa(&style.box_model.margin.top),
        "margin-right" => format_lpa(&style.box_model.margin.right),
        "margin-bottom" => format_lpa(&style.box_model.margin.bottom),
        "margin-left" => format_lpa(&style.box_model.margin.left),
        "padding-top" => format_lp(&style.box_model.padding.top),
        "padding-right" => format_lp(&style.box_model.padding.right),
        "padding-bottom" => format_lp(&style.box_model.padding.bottom),
        "padding-left" => format_lp(&style.box_model.padding.left),
        _ => String::new(),
    }
}
