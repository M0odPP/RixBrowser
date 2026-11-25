//! Weak DOM Bridge (`WeakTable<NodeId>`) binding `rix-dom`, `rix-html`,
//! `rix-css`, and `rix-layout` into `rix-js` with zero memory leaks for
//! detached nodes, dirty-checked incremental layout queries, live `CSSStyleDeclaration`,
//! `ShadowRoot`, `CustomElementRegistry`, and `document.write()` injection.
//!
//! Reference: Spec 05 §2.2, Spec 06 §3 & Issue #16.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use rix_core::{Point, Rect, Size};
use rix_css::{
    matches_element_with_scope, parse_selector_list_strict, query_selector_all_in_subtree,
    query_selector_in_subtree, tokenize_custom_value_str, CascadeOrigin, ComputedStyle,
    DisplayMode, LayerId, MediaContext, RegisteredProperty, RixPseudoElement, StyleContext,
    Stylesheet,
};
use rix_dom::{
    is_valid_custom_element_name, is_valid_shadow_host_name, Document, NodeData, NodeId,
};
use rix_html::{parse_html_fragment, HtmlParserStream};
use rix_js::promise::{alloc_promise, resolve_promise};
use rix_js::{
    GcHeap, HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs, WeakTable,
};
use rix_layout::{EdgeInsets, Fragment, FragmentTree, LayoutEngine};
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
        attribute_namespace: Option<String>,
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
    pub extends: Option<String>,
}

/// DOM prototype handles installed inside a [`JsVm`] realm.
#[derive(Debug, Clone, Copy)]
pub struct DomPrototypes {
    pub event_target_proto: HeapHandle,
    pub node_proto: HeapHandle,
    pub element_proto: HeapHandle,
    pub html_element_proto: HeapHandle,
    pub html_canvas_element_proto: HeapHandle,
    pub html_iframe_element_proto: HeapHandle,
    pub html_anchor_element_proto: HeapHandle,
    pub html_area_element_proto: HeapHandle,
    pub html_base_element_proto: HeapHandle,
    pub html_form_element_proto: HeapHandle,
    pub html_media_element_proto: HeapHandle,
    pub html_video_element_proto: HeapHandle,
    pub html_audio_element_proto: HeapHandle,
    pub svg_element_proto: HeapHandle,
    pub mathml_element_proto: HeapHandle,
    pub document_proto: HeapHandle,
    pub html_document_proto: HeapHandle,
    pub xml_document_proto: HeapHandle,
    pub shadow_root_proto: HeapHandle,
    pub character_data_proto: HeapHandle,
    pub text_proto: HeapHandle,
    pub cdata_section_proto: HeapHandle,
    pub comment_proto: HeapHandle,
    pub processing_instruction_proto: HeapHandle,
    pub document_type_proto: HeapHandle,
    pub attr_proto: HeapHandle,
    pub fragment_proto: HeapHandle,
    pub dom_implementation_proto: HeapHandle,
    pub dom_token_list_proto: HeapHandle,
    pub named_node_map_proto: HeapHandle,
    pub node_list_proto: HeapHandle,
    pub html_collection_proto: HeapHandle,
    pub dom_string_map_proto: HeapHandle,
    pub tree_walker_proto: HeapHandle,
    pub node_iterator_proto: HeapHandle,
}

impl DomPrototypes {
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for h in [
            self.event_target_proto,
            self.node_proto,
            self.element_proto,
            self.html_element_proto,
            self.html_canvas_element_proto,
            self.html_iframe_element_proto,
            self.html_anchor_element_proto,
            self.html_area_element_proto,
            self.html_base_element_proto,
            self.html_form_element_proto,
            self.html_media_element_proto,
            self.html_video_element_proto,
            self.html_audio_element_proto,
            self.svg_element_proto,
            self.mathml_element_proto,
            self.document_proto,
            self.html_document_proto,
            self.xml_document_proto,
            self.shadow_root_proto,
            self.character_data_proto,
            self.text_proto,
            self.cdata_section_proto,
            self.comment_proto,
            self.processing_instruction_proto,
            self.document_type_proto,
            self.attr_proto,
            self.fragment_proto,
            self.dom_implementation_proto,
            self.dom_token_list_proto,
            self.named_node_map_proto,
            self.node_list_proto,
            self.html_collection_proto,
            self.dom_string_map_proto,
            self.tree_walker_proto,
            self.node_iterator_proto,
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
    pub interface_protos: HashMap<&'static str, HeapHandle>,
    pub doc_meta: HashMap<NodeId, crate::dom_interfaces::DocumentMeta>,
    pub owner_docs: HashMap<NodeId, NodeId>,
    pub main_document_url: String,
    pub doc_implementations: HashMap<NodeId, HeapHandle>,
    pub implementation_to_doc: HashMap<HeapHandle, NodeId>,
    pub attrs: HashMap<HeapHandle, crate::dom_interfaces::AttrEntry>,
    pub elem_attr_handles: HashMap<(NodeId, String, String), HeapHandle>,
    pub named_node_maps: HashMap<NodeId, HeapHandle>,
    pub named_node_map_to_elem: HashMap<HeapHandle, NodeId>,
    pub token_list_cache: HashMap<(NodeId, &'static str), HeapHandle>,
    pub token_lists: HashMap<HeapHandle, crate::dom_interfaces::TokenListEntry>,
    pub html_collections: HashMap<HeapHandle, crate::dom_interfaces::HtmlCollectionKind>,
    pub child_nodes_cache: HashMap<NodeId, HeapHandle>,
    pub live_node_lists: HashMap<HeapHandle, NodeId>,
    pub static_node_lists: HashMap<HeapHandle, Vec<NodeId>>,
    pub traversals: HashMap<HeapHandle, crate::dom_interfaces::TraversalEntry>,
    pub active_parser: Option<Rc<RefCell<HtmlParserStream>>>,
    pub ready_state: String,
    pub viewport: Size,
    pub scroll_offset: Point,
    pub element_scroll_offsets: HashMap<NodeId, Point>,
    pub device_pixel_ratio: f32,
    pub dark_mode: bool,
    pub font_db: Arc<FontDatabase>,
    pub layout_engine: LayoutEngine,
    pub(crate) style_ctx: StyleContext,
    external_stylesheets: Vec<Arc<Stylesheet>>,
    sheets_dirty: bool,
    last_inline_style_hash: u64,
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
    pub current_script: Option<NodeId>,
    pub custom_validity: HashMap<NodeId, String>,
    pub dialog_return_values: HashMap<NodeId, String>,
    pub open_popovers: Vec<NodeId>,
    pub css_stylesheet_proto: Option<HeapHandle>,
    pub css_rule_protos: HashMap<&'static str, HeapHandle>,
    pub style_element_sheets: HashMap<NodeId, crate::cssom::SharedStyleSheet>,
    pub stylesheet_handle_map: HashMap<HeapHandle, crate::cssom::SharedStyleSheet>,
    pub adopted_stylesheets: Vec<crate::cssom::SharedStyleSheet>,
    pub registered_properties: HashMap<String, (String, bool, Option<String>)>,
    pub active_element: Option<NodeId>,
    pub next_web_anim_id: u64,
    pub selection: crate::selection::SelectionState,
    pub(crate) dom_parser_proto: Option<HeapHandle>,
    pub(crate) sanitizer_proto: Option<HeapHandle>,
    pub(crate) sanitizers: HashMap<HeapHandle, crate::dom_sanitizer::SanitizerConfig>,
    pub(crate) trusted_types: crate::trusted_types::TrustedTypesState,
    pub(crate) doc_csp: HashMap<NodeId, crate::trusted_types::DocumentCspPolicy>,
    pub(crate) shadow_dom: RefCell<crate::shadow_dom::ShadowDomState>,
    pub(crate) contextual_scripts: HashSet<NodeId>,
    pub(crate) script_force_async: HashSet<NodeId>,
    pub(crate) upgraded_custom_elements: HashSet<NodeId>,
    pub(crate) pending_ce_upgrade_node: Option<NodeId>,
    pub(crate) iframe_connect_hook: Option<IframeConnectHook>,
    pub(crate) body_attr_handler_hook: Option<BodyAttrHandlerHook>,
    pub(crate) script_connect_hook: Option<ScriptConnectHook>,
}

/// Callback invoked when an `<iframe>` element is connected into a live document.
pub(crate) type IframeConnectHook = Rc<dyn Fn(&mut JsVm, NodeId)>;

/// Callback invoked when an `on*` attribute is set on `<body>`/`<frameset>`.
pub(crate) type BodyAttrHandlerHook = Rc<dyn Fn(&mut JsVm, NodeId, &str, &str)>;

/// Callback invoked when an executable fragment-created `<script>` element becomes connected.
pub(crate) type ScriptConnectHook = Rc<dyn Fn(&mut JsVm, NodeId)>;

/// Shared handle to [`HostState`].
pub type SharedHost = Rc<RefCell<HostState>>;

impl HostState {
    #[must_use]
    pub fn new(
        document: Rc<RefCell<Document>>,
        prototypes: DomPrototypes,
        interface_protos: HashMap<&'static str, HeapHandle>,
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
            interface_protos,
            doc_meta: HashMap::new(),
            owner_docs: HashMap::new(),
            main_document_url: crate::web_apis::DEFAULT_DOCUMENT_URL.to_string(),
            doc_implementations: HashMap::new(),
            implementation_to_doc: HashMap::new(),
            attrs: HashMap::new(),
            elem_attr_handles: HashMap::new(),
            named_node_maps: HashMap::new(),
            named_node_map_to_elem: HashMap::new(),
            token_list_cache: HashMap::new(),
            token_lists: HashMap::new(),
            html_collections: HashMap::new(),
            child_nodes_cache: HashMap::new(),
            live_node_lists: HashMap::new(),
            static_node_lists: HashMap::new(),
            traversals: HashMap::new(),
            active_parser: None,
            ready_state: "complete".to_string(),
            viewport,
            scroll_offset: Point::ZERO,
            element_scroll_offsets: HashMap::new(),
            device_pixel_ratio: 1.0,
            dark_mode: false,
            font_db,
            layout_engine: LayoutEngine::new(),
            style_ctx: StyleContext::new(media),
            external_stylesheets: Vec::new(),
            sheets_dirty: true,
            last_inline_style_hash: 0,
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
            current_script: None,
            custom_validity: HashMap::new(),
            dialog_return_values: HashMap::new(),
            open_popovers: Vec::new(),
            css_stylesheet_proto: None,
            css_rule_protos: HashMap::new(),
            style_element_sheets: HashMap::new(),
            stylesheet_handle_map: HashMap::new(),
            adopted_stylesheets: Vec::new(),
            registered_properties: HashMap::new(),
            active_element: None,
            next_web_anim_id: 1,
            selection: crate::selection::SelectionState::default(),
            dom_parser_proto: None,
            sanitizer_proto: None,
            sanitizers: HashMap::new(),
            trusted_types: crate::trusted_types::TrustedTypesState::default(),
            doc_csp: HashMap::new(),
            shadow_dom: RefCell::new(crate::shadow_dom::ShadowDomState::default()),
            contextual_scripts: HashSet::new(),
            script_force_async: HashSet::new(),
            upgraded_custom_elements: HashSet::new(),
            pending_ce_upgrade_node: None,
            iframe_connect_hook: None,
            body_attr_handler_hook: None,
            script_connect_hook: None,
        }
    }

    /// Marks document stylesheets dirty so the next style/layout read rebuilds the cascade.
    pub fn mark_sheets_dirty(&mut self) {
        self.sheets_dirty = true;
        self.last_style_revs = None;
        self.last_layout_revs = None;
        self.last_paint_revs = None;
    }

    /// Invalidates all CSS rules affected by an attribute/class/id mutation on `nid`.
    pub fn invalidate_attr_mutation(
        &mut self,
        nid: NodeId,
        attr_name: &str,
        old_val: Option<&str>,
        new_val: Option<&str>,
    ) {
        let doc_rc = Rc::clone(&self.document);
        let mut doc = doc_rc.borrow_mut();
        doc.bump_style_revision_for(nid);
        if attr_name.eq_ignore_ascii_case("class") {
            let old_set: Vec<&str> = old_val.unwrap_or("").split_ascii_whitespace().collect();
            let new_set: Vec<&str> = new_val.unwrap_or("").split_ascii_whitespace().collect();
            for &cls in &old_set {
                if !new_set.contains(&cls) {
                    self.style_ctx.invalidate_class_change(&mut doc, nid, cls);
                }
            }
            for &cls in &new_set {
                if !old_set.contains(&cls) {
                    self.style_ctx.invalidate_class_change(&mut doc, nid, cls);
                }
            }
        } else if attr_name.eq_ignore_ascii_case("id") {
            self.style_ctx
                .invalidate_id_change(&mut doc, nid, old_val, new_val);
        }
        self.style_ctx
            .invalidate_attribute_change(&mut doc, nid, attr_name);
    }

    /// Invalidates CSS rules affected by structural DOM mutations under `parent`.
    pub fn invalidate_dom_mutation(&mut self, parent: NodeId, child: Option<NodeId>) {
        let doc_rc = Rc::clone(&self.document);
        let touches_style = {
            let mut doc = doc_rc.borrow_mut();
            doc.bump_style_revision_for(parent);
            self.style_ctx
                .invalidate_subtree_mutation(&mut doc, parent, child);
            let is_style = |nid: NodeId| {
                matches!(
                    doc.get(nid),
                    Some(NodeData::Element(e)) if e.name.local.as_str().eq_ignore_ascii_case("style")
                )
            };
            is_style(parent) || child.is_some_and(is_style)
        };
        if touches_style {
            self.mark_sheets_dirty();
        }
    }

    /// Updates focus (`:focus`, `:focus-within`, `:focus-visible`, `document.activeElement`).
    pub fn set_focused_element(&mut self, target: Option<NodeId>, visible: bool) {
        self.active_element = target;
        let doc_rc = Rc::clone(&self.document);
        let mut doc = doc_rc.borrow_mut();
        self.style_ctx.set_focused(&mut doc, target, visible);
        doc.bump_style_revision();
    }

    /// Invalidates CSS rules affected by a pseudo-state bit change on `nid`.
    pub fn invalidate_element_state(&mut self, nid: NodeId, changed: rix_dom::ElementState) {
        let doc_rc = Rc::clone(&self.document);
        let mut doc = doc_rc.borrow_mut();
        doc.bump_style_revision_for(nid);
        self.style_ctx
            .invalidate_state_change(&mut doc, nid, changed);
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
        for meta in self.doc_meta.values() {
            if let Some(dv) = meta.default_view {
                out.push(JsValue::from_object(dv));
            }
        }
        if let Some(dpp) = self.dom_parser_proto {
            out.push(JsValue::from_object(dpp));
        }
        if let Some(sp) = self.sanitizer_proto {
            out.push(JsValue::from_object(sp));
        }
        self.trusted_types.collect_roots(out);
        self.prototypes.collect_roots(out);
        for &proto in self.interface_protos.values() {
            out.push(JsValue::from_object(proto));
        }
        self.wrappers.collect_pinned_roots(out);
        for def in self.custom_elements.values() {
            out.push(JsValue::from_object(def.constructor));
        }
        for waiters in self.when_defined_waiters.values() {
            for &p in waiters {
                out.push(JsValue::from_object(p));
            }
        }
        if let Some(proto) = self.css_stylesheet_proto {
            out.push(JsValue::from_object(proto));
        }
        for &proto in self.css_rule_protos.values() {
            out.push(JsValue::from_object(proto));
        }
        if let Some(sel) = self.selection.selection_handle {
            out.push(JsValue::from_object(sel));
        }
        if let Some(sr) = self.selection.selected_range {
            out.push(JsValue::from_object(sr));
        }
        for p in [
            self.selection.abstract_range_proto,
            self.selection.range_proto,
            self.selection.static_range_proto,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(p));
        }
        for entry in self.traversals.values() {
            if let Some(f) = entry.filter {
                out.push(JsValue::from_object(f));
            }
        }
    }

    /// Sweeps dead unpinned weak DOM wrappers and releases their tag table entries (zero leaks!).
    pub fn sweep_dead_wrappers(&mut self, heap: &GcHeap) -> usize {
        self.stylesheet_handle_map
            .retain(|h, _| heap.get_object(*h).is_some());
        self.selection
            .ranges
            .retain(|h, _| heap.get_object(*h).is_some());
        self.doc_implementations
            .retain(|_, h| heap.get_object(*h).is_some());
        self.implementation_to_doc
            .retain(|h, _| heap.get_object(*h).is_some());
        self.attrs.retain(|h, _| heap.get_object(*h).is_some());
        self.elem_attr_handles
            .retain(|_, h| heap.get_object(*h).is_some());
        self.named_node_maps
            .retain(|_, h| heap.get_object(*h).is_some());
        self.named_node_map_to_elem
            .retain(|h, _| heap.get_object(*h).is_some());
        self.token_list_cache
            .retain(|_, h| heap.get_object(*h).is_some());
        self.token_lists
            .retain(|h, _| heap.get_object(*h).is_some());
        self.html_collections
            .retain(|h, _| heap.get_object(*h).is_some());
        self.child_nodes_cache
            .retain(|_, h| heap.get_object(*h).is_some());
        self.live_node_lists
            .retain(|h, _| heap.get_object(*h).is_some());
        self.static_node_lists
            .retain(|h, _| heap.get_object(*h).is_some());
        self.traversals.retain(|h, _| heap.get_object(*h).is_some());
        self.trusted_types.sweep_dead(heap);
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

    /// Installs pre-parsed external `<link rel="stylesheet">` stylesheets and triggers a style refresh.
    pub fn set_external_stylesheets(&mut self, sheets: Vec<Arc<Stylesheet>>) {
        self.external_stylesheets = sheets;
        self.sheets_dirty = true;
        self.last_style_revs = None;
        self.last_layout_revs = None;
        self.last_paint_revs = None;
    }

    /// Updates viewport dimensions and marks styles/layout dirty when size changed.
    pub fn set_viewport(&mut self, viewport: Size) {
        if (self.viewport.width - viewport.width).abs() > 0.5
            || (self.viewport.height - viewport.height).abs() > 0.5
        {
            self.viewport = viewport;
            self.sheets_dirty = true;
            self.last_style_revs = None;
            self.last_layout_revs = None;
            self.last_paint_revs = None;
        }
    }

    /// Updates `prefers-color-scheme` dark mode state for CSS `@media` queries and painting.
    pub fn set_dark_mode(&mut self, dark_mode: bool) {
        if self.dark_mode != dark_mode {
            self.dark_mode = dark_mode;
            self.layout_engine.dark_mode = dark_mode;
            self.sheets_dirty = true;
            self.last_style_revs = None;
            self.last_layout_revs = None;
            self.last_paint_revs = None;
        }
    }

    /// Forces the next `paint_if_dirty*` call to repaint even if DOM revisions did not change
    /// (for example after async images or web fonts finish loading).
    pub fn invalidate_paint(&mut self) {
        self.last_layout_revs = None;
        self.last_paint_revs = None;
    }

    #[must_use]
    pub fn style_context(&self) -> &StyleContext {
        &self.style_ctx
    }

    #[must_use]
    pub fn cached_fragment_tree(&self) -> Option<&FragmentTree> {
        self.cached_fragment_tree.as_ref()
    }

    #[must_use]
    pub fn missing_css_features(&self) -> &[rix_css::MissingCssFeature] {
        self.style_ctx.missing_features()
    }

    /// Recomputes CSS cascade ONLY when document structure, style revision, or stylesheets changed.
    /// Uses fast incremental `restyle_dirty` when only DOM nodes/attributes mutated and cached
    /// `Stylesheet::parse_cached` when rebuilding the stylesheet list.
    pub fn ensure_styles(&mut self) -> &StyleContext {
        let revs = {
            let doc = self.document.borrow();
            (doc.revision(), doc.style_revision())
        };
        if !self.sheets_dirty && self.last_style_revs == Some(revs) {
            return &self.style_ctx;
        }

        let (inline_hash, maybe_sheets) = {
            let doc = self.document.borrow();
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            let mut css_blocks: Vec<(String, Option<NodeId>)> = Vec::new();
            for node in doc.descendants_with_shadow_roots(doc.root()) {
                if let Some(NodeData::Element(el)) = doc.get(node) {
                    if el.name.local.as_str().eq_ignore_ascii_case("style") {
                        let is_disabled = self
                            .style_element_sheets
                            .get(&node)
                            .is_some_and(|sh| sh.borrow().disabled);
                        if is_disabled {
                            continue;
                        }
                        let text = doc.inner_text(node);
                        if !text.trim().is_empty() {
                            let owner_parent = doc.parent(node);
                            h = fnv_mix(h, text.as_bytes());
                            if text.contains("@scope") || text.contains("@SCOPE") {
                                if let Some(pid) = owner_parent {
                                    h = fnv_mix(h, &usize::from(pid).to_le_bytes());
                                }
                            }
                            css_blocks.push((text, owner_parent));
                        }
                    }
                }
            }
            for sheet in doc.all_dynamic_stylesheets().values() {
                if !sheet.trim().is_empty() {
                    h = fnv_mix(h, sheet.as_bytes());
                    css_blocks.push((sheet.clone(), None));
                }
            }
            for adopted in &self.adopted_stylesheets {
                let st = adopted.borrow();
                if !st.disabled {
                    let css = st.to_css_text();
                    if !css.trim().is_empty() {
                        h = fnv_mix(h, css.as_bytes());
                        css_blocks.push((css, None));
                    }
                }
            }
            (h, css_blocks)
        };

        if !self.sheets_dirty
            && self.last_style_revs.is_some()
            && inline_hash == self.last_inline_style_hash
        {
            // Fast path: stylesheets unchanged — run targeted incremental `restyle_dirty` only!
            {
                let mut doc = self.document.borrow_mut();
                let _ = self.style_ctx.restyle_dirty(&mut doc);
            }
            let post_revs = {
                let doc = self.document.borrow();
                (doc.revision(), doc.style_revision())
            };
            self.last_style_revs = Some(post_revs);
            self.style_recalc_count += 1;
            return &self.style_ctx;
        }

        let media = MediaContext {
            viewport_width: self.viewport.width,
            viewport_height: self.viewport.height,
            dark_mode: self.dark_mode,
            ..MediaContext::default()
        };
        let mut ctx = StyleContext::new(media);
        let anim_snap = self.style_ctx.take_animation_state();
        ctx.restore_animation_state(anim_snap);
        for (name, (syntax, inherits, init)) in &self.registered_properties {
            ctx.register_property(RegisteredProperty {
                name: rix_core::intern_str(name),
                syntax: syntax.clone(),
                inherits: *inherits,
                initial_value: init.as_deref().map(tokenize_custom_value_str),
                layer_id: LayerId::UNLAYERED,
            });
        }
        for ext in &self.external_stylesheets {
            ctx.add_stylesheet(Arc::clone(ext));
        }
        for (css, owner_parent) in maybe_sheets {
            if owner_parent.is_some() && (css.contains("@scope") || css.contains("@SCOPE")) {
                ctx.add_stylesheet(Arc::new(Stylesheet::parse_with_owner(
                    &css,
                    CascadeOrigin::Author,
                    &media,
                    owner_parent,
                )));
            } else {
                ctx.add_stylesheet(Stylesheet::parse_cached(
                    &css,
                    CascadeOrigin::Author,
                    &media,
                ));
            }
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
        self.sheets_dirty = false;
        self.last_inline_style_hash = inline_hash;
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
            let (tree, _metrics) = self.layout_engine.layout_only_with_scroll(
                &doc,
                &self.style_ctx,
                self.viewport,
                &self.font_db,
                &self.element_scroll_offsets,
                self.scroll_offset,
            );
            (tree, (doc.revision(), doc.style_revision()))
        };
        self.cached_fragment_tree = Some(tree);
        self.last_layout_revs = Some(post_revs);
        self.layout_count += 1;
        self.cached_fragment_tree.as_ref()
    }

    /// Returns full CSSOM View geometry for `node` (document-space rects, box insets,
    /// overflow rect, and accumulated ancestor scroll offset).
    pub fn element_geometry_for_node(&mut self, node: NodeId) -> Option<ElementGeometryInfo> {
        let root_scroll = self.scroll_offset;
        let scroll_map = self.element_scroll_offsets.clone();
        let tree = self.ensure_layout()?;
        let root = tree.root.as_ref()?;
        find_element_geometry(root, Point::ZERO, root_scroll, &scroll_map, node, true)
    }

    /// Finds the viewport-relative border-box rect and content-box rect of `node` in CSS pixels.
    pub fn bounding_rects_for_node(&mut self, node: NodeId) -> Option<(Rect, Rect)> {
        let info = self.element_geometry_for_node(node)?;
        let b = Rect::new(
            info.abs_border.origin.x - info.ancestor_scroll.x,
            info.abs_border.origin.y - info.ancestor_scroll.y,
            info.abs_border.size.width,
            info.abs_border.size.height,
        );
        let c = Rect::new(
            info.abs_content.origin.x - info.ancestor_scroll.x,
            info.abs_content.origin.y - info.ancestor_scroll.y,
            info.abs_content.size.width,
            info.abs_content.size.height,
        );
        Some((b, c))
    }

    /// Resolves CSSOM View §7.1 `HTMLElement.prototype.offsetParent` for `node`.
    pub fn offset_parent_for_node(&mut self, node: NodeId) -> Option<NodeId> {
        let _ = self.ensure_layout()?;
        let doc = self.document.borrow();
        let style = self.style_ctx.style_for(node)?;
        if style.display == DisplayMode::None
            || style.display == DisplayMode::Contents
            || style.position == rix_css::PositionMode::Fixed
        {
            return None;
        }
        if let Some(NodeData::Element(el)) = doc.get(node) {
            let tag = el.name.local.as_str();
            if tag.eq_ignore_ascii_case("html") || tag.eq_ignore_ascii_case("body") {
                return None;
            }
        }
        let self_is_static = style.position == rix_css::PositionMode::Static;
        let next_anc = |n: NodeId| -> Option<NodeId> {
            match doc.parent(n) {
                Some(p) => Some(p),
                None => match doc.get(n) {
                    Some(NodeData::ShadowRoot { host, .. }) => Some(*host),
                    _ => None,
                },
            }
        };
        let mut cur = next_anc(node);
        while let Some(anc) = cur {
            if let Some(NodeData::Element(el)) = doc.get(anc) {
                let tag = el.name.local.as_str();
                if let Some(ast) = self.style_ctx.style_for(anc) {
                    if ast.display == DisplayMode::Contents {
                        cur = next_anc(anc);
                        continue;
                    }
                    if ast.position != rix_css::PositionMode::Static {
                        return Some(anc);
                    }
                    if tag.eq_ignore_ascii_case("body") {
                        return Some(anc);
                    }
                    if self_is_static
                        && (tag.eq_ignore_ascii_case("td")
                            || tag.eq_ignore_ascii_case("th")
                            || tag.eq_ignore_ascii_case("table"))
                    {
                        return Some(anc);
                    }
                }
            }
            cur = next_anc(anc);
        }
        None
    }

    /// Computes CSSOM View `clientWidth` and `clientHeight` (padding-box, or viewport for root).
    pub fn client_size_for_node(&mut self, node: NodeId) -> (f32, f32) {
        let Some(info) = self.element_geometry_for_node(node) else {
            return (0.0, 0.0);
        };
        if info.is_root_elem {
            return (self.viewport.width, self.viewport.height);
        }
        if info.is_inline_non_replaced {
            return (0.0, 0.0);
        }
        (info.abs_padding.size.width, info.abs_padding.size.height)
    }

    /// Computes CSSOM View `scrollWidth` and `scrollHeight` (including end-edge padding
    /// and reversed scroll-origin negative overflow per CSS Overflow L3 §2.2).
    pub fn scroll_size_for_node(&mut self, node: NodeId) -> (f32, f32) {
        let Some(info) = self.element_geometry_for_node(node) else {
            return (0.0, 0.0);
        };
        if info.is_root_elem {
            let sw = info.overflow_rect.max_x().max(self.viewport.width);
            let sh = info.overflow_rect.max_y().max(self.viewport.height);
            return (sw, sh);
        }
        let pad_lo_x = info.border.left;
        let pad_hi_x = info.border.left + info.abs_padding.size.width;
        let pad_lo_y = info.border.top;
        let pad_hi_y = info.border.top + info.abs_padding.size.height;
        let (eff_lo_x, eff_hi_x) = if info.has_children {
            let lo = if info.scroll_neg_x {
                pad_lo_x.min(info.children_bounds.origin.x - info.padding.left)
            } else {
                pad_lo_x
            };
            let hi = pad_hi_x.max(info.children_bounds.max_x() + info.padding.right);
            (lo, hi)
        } else {
            (pad_lo_x, pad_hi_x)
        };
        let (eff_lo_y, eff_hi_y) = if info.has_children {
            let lo = if info.scroll_neg_y {
                pad_lo_y.min(info.children_bounds.origin.y - info.padding.top)
            } else {
                pad_lo_y
            };
            let hi = pad_hi_y.max(info.children_bounds.max_y() + info.padding.bottom);
            (lo, hi)
        } else {
            (pad_lo_y, pad_hi_y)
        };
        let sw = (eff_hi_x - eff_lo_x).max(info.abs_padding.size.width);
        let sh = (eff_hi_y - eff_lo_y).max(info.abs_padding.size.height);
        (sw, sh)
    }

    /// Reads CSSOM View `scrollLeft` / `scrollTop` for `node`.
    pub fn scroll_offset_for_node(&mut self, node: NodeId) -> Point {
        let is_root = self.document.borrow().document_element() == Some(node);
        if is_root {
            return self.scroll_offset;
        }
        self.element_scroll_offsets
            .get(&node)
            .copied()
            .unwrap_or(Point::ZERO)
    }

    /// Updates `scrollLeft` / `scrollTop` for `node` (clamped to scrollable bounds) and
    /// invalidates cached layout so `position: sticky` offsets recompute live.
    pub fn set_scroll_offset_for_node(
        &mut self,
        node: NodeId,
        x: Option<f32>,
        y: Option<f32>,
        relative: bool,
    ) {
        let is_root = self.document.borrow().document_element() == Some(node);
        let (cw, ch) = self.client_size_for_node(node);
        let (sw, sh) = self.scroll_size_for_node(node);
        let max_x = (sw - cw).max(0.0);
        let max_y = (sh - ch).max(0.0);
        if is_root {
            let cur = self.scroll_offset;
            let nx = x
                .map(|v| if relative { cur.x + v } else { v })
                .unwrap_or(cur.x)
                .clamp(0.0, max_x);
            let ny = y
                .map(|v| if relative { cur.y + v } else { v })
                .unwrap_or(cur.y)
                .clamp(0.0, max_y);
            if (nx - cur.x).abs() > 1e-4 || (ny - cur.y).abs() > 1e-4 {
                self.scroll_offset = Point::new(nx, ny);
                self.last_layout_revs = None;
            }
            return;
        }
        let Some(info) = self.element_geometry_for_node(node) else {
            return;
        };
        let can_x = rix_layout::is_scroll_axis(info.overflow_x);
        let can_y = rix_layout::is_scroll_axis(info.overflow_y);
        if !can_x && !can_y {
            return;
        }
        let cur = self
            .element_scroll_offsets
            .get(&node)
            .copied()
            .unwrap_or(Point::ZERO);
        let nx = if can_x {
            x.map(|v| if relative { cur.x + v } else { v })
                .unwrap_or(cur.x)
                .clamp(0.0, max_x)
        } else {
            0.0
        };
        let ny = if can_y {
            y.map(|v| if relative { cur.y + v } else { v })
                .unwrap_or(cur.y)
                .clamp(0.0, max_y)
        } else {
            0.0
        };
        if (nx - cur.x).abs() > 1e-4 || (ny - cur.y).abs() > 1e-4 {
            self.element_scroll_offsets.insert(node, Point::new(nx, ny));
            self.last_layout_revs = None;
        }
    }

    /// Updates the viewport scroll offset (for `window.scrollTo` / `window.scrollBy`),
    /// clamping to scrollable bounds and invalidating cached sticky layout.
    pub fn set_root_scroll(&mut self, x: Option<f32>, y: Option<f32>, relative: bool) {
        let root_el = self.document.borrow().document_element();
        if let Some(el) = root_el {
            self.set_scroll_offset_for_node(el, x, y, relative);
        } else {
            let cur = self.scroll_offset;
            let nx = x
                .map(|v| if relative { cur.x + v } else { v })
                .unwrap_or(cur.x)
                .max(0.0);
            let ny = y
                .map(|v| if relative { cur.y + v } else { v })
                .unwrap_or(cur.y)
                .max(0.0);
            self.scroll_offset = Point::new(nx, ny);
            self.last_layout_revs = None;
        }
    }

    /// Returns layout-resolved box metrics (`content_size`, `border_size`, `margin`, `padding`)
    /// for CSSOM View §7.1 `getComputedStyle` resolved values (`width`, `height`, `margin-*`, `padding-*`).
    pub fn resolved_layout_box_for_node(&mut self, node: NodeId) -> Option<ResolvedLayoutBox> {
        let tree = self.ensure_layout()?;
        let root = tree.root.as_ref()?;
        find_fragment_metrics(root, root.content_rect.size, node)
    }

    /// Resolves the CSS Values L3 §5.1.1 `1ch` advance measure (`"0"` glyph width) for `style`.
    pub fn ch_advance_px(&mut self, style: &ComputedStyle) -> f32 {
        self.layout_engine.ch_advance_px(style, &self.font_db)
    }

    /// Returns `true` if `node`'s parent element has `display: flex | inline-flex | grid | inline-grid`.
    pub fn parent_is_flex_or_grid(&mut self, node: NodeId) -> bool {
        let parent_id = {
            let doc = self.document.borrow();
            doc.parent(node)
        };
        let Some(pid) = parent_id else {
            return false;
        };
        let ctx = self.ensure_styles();
        ctx.style_for(pid).is_some_and(|ps| {
            matches!(
                ps.display,
                DisplayMode::Flex
                    | DisplayMode::InlineFlex
                    | DisplayMode::Grid
                    | DisplayMode::InlineGrid
                    | DisplayMode::GridLanes
                    | DisplayMode::InlineGridLanes
                    | DisplayMode::InlineGridLanesMulti
            )
        })
    }

    /// Preserves an unquantized CSS Color 4/5 AST for an inline `element.style` declaration.
    pub fn set_inline_color_expr(
        &mut self,
        node: NodeId,
        prop: rix_css::PropertyIdKind,
        expr: Option<rix_css::CssColorExpr>,
    ) {
        self.style_ctx.set_inline_color_expr(node, prop, expr);
    }

    /// Clears all preserved unquantized inline color expressions for `node`.
    pub fn clear_inline_color_exprs(&mut self, node: NodeId) {
        self.style_ctx.clear_inline_color_exprs(node);
    }

    /// Attaches a Web Animation (`Element.prototype.animate`) to `node` and marks it dirty.
    pub fn add_web_animation(&mut self, node: NodeId, mut anim: rix_css::WebAnimationSpec) -> u64 {
        let id = self.next_web_anim_id;
        self.next_web_anim_id += 1;
        anim.id = id;
        self.style_ctx.add_web_animation(node, anim);
        self.mark_node_style_dirty(node);
        id
    }

    /// Mutates a Web Animation on `node` by `id` and marks `node` dirty.
    pub fn update_web_animation(
        &mut self,
        node: NodeId,
        id: u64,
        f: impl FnOnce(&mut rix_css::WebAnimationSpec),
    ) -> bool {
        let changed = self.style_ctx.update_web_animation(node, id, f);
        if changed {
            self.mark_node_style_dirty(node);
        }
        changed
    }

    /// Lists all active Web Animations in creation order (`document.getAnimations()`).
    #[must_use]
    pub fn all_web_animations(&self) -> Vec<(NodeId, rix_css::WebAnimationSpec)> {
        self.style_ctx.all_web_animations()
    }

    /// Marks `node` and its ancestors dirty for style/layout recalculation.
    pub fn mark_node_style_dirty(&mut self, node: NodeId) {
        let mut doc = self.document.borrow_mut();
        doc.bump_style_revision_for(node);
        doc.mark_node_dirty(node, rix_dom::DirtyFlags::SELF_STYLE_DIRTY);
        let mut cur = doc.parent(node);
        while let Some(anc) = cur {
            doc.mark_node_dirty(anc, rix_dom::DirtyFlags::DESCENDANT_STYLE_DIRTY);
            cur = doc.parent(anc);
        }
        self.last_style_revs = None;
        self.last_layout_revs = None;
    }

    /// Returns the cached `ComputedStyle` for `node`, running dirty-checked cascade if needed.
    pub fn computed_style_for_node(&mut self, node: NodeId) -> Option<Arc<ComputedStyle>> {
        let ctx = self.ensure_styles();
        ctx.style_for(node).cloned()
    }

    /// Returns the cached `ComputedStyle` for `node` or its pseudo-element (`::before`, `::after`, etc.).
    pub fn computed_style_for_node_pseudo(
        &mut self,
        node: NodeId,
        pseudo: Option<RixPseudoElement>,
    ) -> Option<Arc<ComputedStyle>> {
        let ctx = self.ensure_styles();
        match pseudo {
            Some(pe) => ctx.pseudo_style_for(node, pe).cloned().or_else(|| {
                ctx.style_for(node)
                    .map(|orig| Arc::new(ComputedStyle::inherit_from(orig)))
            }),
            None => ctx.style_for(node).cloned(),
        }
    }

    /// Rebuilds the page [`rix_render::DisplayList`] ONLY when the DOM or CSS revisions changed,
    /// returning `(DisplayList, style_ms, layout_ms, paint_ms)` for live telemetry.
    pub fn paint_if_dirty_with_metrics(
        &mut self,
        pipeline: rix_ipc::PipelineId,
        resources: &mut rix_render::ResourceRegistry,
    ) -> Option<(rix_render::DisplayList, f32, f32, f32)> {
        let revs = {
            let doc = self.document.borrow();
            (doc.revision(), doc.style_revision())
        };
        if !self.sheets_dirty && self.last_paint_revs == Some(revs) {
            return None;
        }
        let t_style = std::time::Instant::now();
        let _ = self.ensure_styles();
        let style_ms = t_style.elapsed().as_secs_f32() * 1000.0;

        let t_layout = std::time::Instant::now();
        let _ = self.ensure_layout();
        let layout_ms = t_layout.elapsed().as_secs_f32() * 1000.0;

        let post_revs = {
            let doc = self.document.borrow();
            (doc.revision(), doc.style_revision())
        };
        let tree = self.cached_fragment_tree.as_ref()?;
        let t_paint = std::time::Instant::now();
        let keyframes = self.style_ctx.all_keyframes();
        let dl = rix_layout::paint_fragment_tree_with_keyframes(
            tree,
            &self.font_db,
            &self.layout_engine.assets,
            &keyframes,
            self.viewport,
            pipeline,
            resources,
            self.layout_engine.dark_mode,
        );
        let paint_ms = t_paint.elapsed().as_secs_f32() * 1000.0;
        self.last_paint_revs = Some(post_revs);
        self.display_list_build_count += 1;
        Some((dl, style_ms, layout_ms, paint_ms))
    }

    /// Rebuilds the page [`rix_render::DisplayList`] ONLY when the DOM or CSS revisions changed.
    /// During 60 FPS Canvas2D animations this returns `None` (zero DisplayList rebuilds!).
    pub fn paint_if_dirty(
        &mut self,
        pipeline: rix_ipc::PipelineId,
        resources: &mut rix_render::ResourceRegistry,
    ) -> Option<rix_render::DisplayList> {
        self.paint_if_dirty_with_metrics(pipeline, resources)
            .map(|(dl, _, _, _)| dl)
    }

    /// Forces a fresh layout + paint of the current document into a [`rix_render::DisplayList`].
    pub fn paint_now(
        &mut self,
        pipeline: rix_ipc::PipelineId,
        resources: &mut rix_render::ResourceRegistry,
    ) -> rix_render::DisplayList {
        self.last_paint_revs = None;
        self.paint_if_dirty(pipeline, resources).unwrap_or_else(|| {
            rix_render::DisplayList::new(Rect::new(
                0.0,
                0.0,
                self.viewport.width,
                self.viewport.height,
            ))
        })
    }
}

/// Layout-resolved box geometry for CSSOM View §7.1 `getComputedStyle` resolved values.
#[derive(Debug, Clone)]
pub struct ResolvedLayoutBox {
    pub content_size: Size,
    pub border_size: Size,
    pub padding: EdgeInsets,
    pub margin: EdgeInsets,
    pub parent_content_size: Size,
    pub resolved_grid_tracks: Option<Arc<rix_layout::ResolvedGridTracks>>,
}

fn find_fragment_metrics(
    frag: &Fragment,
    parent_content_size: Size,
    target: NodeId,
) -> Option<ResolvedLayoutBox> {
    if frag.dom_node == Some(target) {
        return Some(ResolvedLayoutBox {
            content_size: frag.content_rect.size,
            border_size: frag.border_rect.size,
            padding: frag.padding,
            margin: frag.margin,
            parent_content_size,
            resolved_grid_tracks: frag.resolved_grid_tracks.clone(),
        });
    }
    let next_parent = frag.content_rect.size;
    for child in frag.children.iter() {
        if let Some(found) = find_fragment_metrics(child, next_parent, target) {
            return Some(found);
        }
    }
    None
}

/// Detailed CSSOM View box geometry for a DOM element.
#[derive(Debug, Clone, Copy)]
pub struct ElementGeometryInfo {
    pub abs_border: Rect,
    pub abs_padding: Rect,
    pub abs_content: Rect,
    pub border: EdgeInsets,
    pub padding: EdgeInsets,
    pub overflow_rect: Rect,
    pub children_bounds: Rect,
    pub has_children: bool,
    pub scroll_neg_x: bool,
    pub scroll_neg_y: bool,
    pub ancestor_scroll: Point,
    pub is_root_elem: bool,
    pub is_inline_non_replaced: bool,
    pub overflow_x: rix_css::OverflowMode,
    pub overflow_y: rix_css::OverflowMode,
}

fn find_element_geometry(
    frag: &Fragment,
    parent_origin: Point,
    scroll_acc: Point,
    scroll_map: &HashMap<NodeId, Point>,
    target: NodeId,
    is_root: bool,
) -> Option<ElementGeometryInfo> {
    let abs_border = frag.absolute_border_rect(parent_origin);
    // Viewport-fixed fragments attached directly to root do not scroll with the root viewport.
    let eff_scroll = if !is_root && frag.style.position == rix_css::PositionMode::Fixed {
        Point::ZERO
    } else {
        scroll_acc
    };
    if frag.dom_node == Some(target) {
        let abs_padding = Rect::new(
            abs_border.origin.x + frag.border.left,
            abs_border.origin.y + frag.border.top,
            (abs_border.size.width - frag.border.horizontal()).max(0.0),
            (abs_border.size.height - frag.border.vertical()).max(0.0),
        );
        let abs_content = Rect::new(
            abs_border.origin.x + frag.content_rect.origin.x,
            abs_border.origin.y + frag.content_rect.origin.y,
            frag.content_rect.size.width,
            frag.content_rect.size.height,
        );
        let is_inline_non_replaced =
            matches!(frag.kind, rix_layout::FragmentKind::InlineBox { .. });
        let (has_children, children_bounds) = if frag.children.is_empty() {
            (false, Rect::ZERO)
        } else {
            let mut min_x = f32::INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            for ch in frag.children.iter() {
                let cx = ch.border_rect.origin.x;
                let cy = ch.border_rect.origin.y;
                min_x = min_x.min(cx + ch.overflow_rect.origin.x);
                min_y = min_y.min(cy + ch.overflow_rect.origin.y);
                max_x = max_x.max(cx + ch.overflow_rect.max_x());
                max_y = max_y.max(cy + ch.overflow_rect.max_y());
            }
            (
                true,
                Rect::new(
                    min_x,
                    min_y,
                    (max_x - min_x).max(0.0),
                    (max_y - min_y).max(0.0),
                ),
            )
        };
        let (scroll_neg_x, scroll_neg_y) = if matches!(
            frag.style.display,
            DisplayMode::Flex | DisplayMode::InlineFlex
        ) {
            let fg = frag.style.flex_grid.as_deref();
            let fdir = fg.map_or(rix_css::FlexDirectionMode::Row, |f| f.flex_direction);
            let fwrap = fg.map_or(rix_css::FlexWrapMode::NoWrap, |f| f.flex_wrap);
            let axes = rix_layout::FlexAxes::compute(
                frag.style.text.writing_mode,
                frag.style.text.direction,
                fdir,
                fwrap,
            );
            let nx = if axes.main_is_x {
                axes.main_reversed
            } else {
                axes.cross_reversed
            };
            let ny = if axes.main_is_x {
                axes.cross_reversed
            } else {
                axes.main_reversed
            };
            (nx, ny)
        } else {
            let wm = frag.style.text.writing_mode;
            let is_rtl = frag.style.text.direction == rix_css::DirectionMode::Rtl;
            let nx = (wm == rix_css::WritingMode::HorizontalTb && is_rtl)
                || matches!(
                    wm,
                    rix_css::WritingMode::VerticalRl | rix_css::WritingMode::SidewaysRl
                );
            let ny = (wm == rix_css::WritingMode::SidewaysLr && !is_rtl)
                || (wm != rix_css::WritingMode::HorizontalTb
                    && wm != rix_css::WritingMode::SidewaysLr
                    && is_rtl);
            (nx, ny)
        };
        return Some(ElementGeometryInfo {
            abs_border,
            abs_padding,
            abs_content,
            border: frag.border,
            padding: frag.padding,
            overflow_rect: frag.overflow_rect,
            children_bounds,
            has_children,
            scroll_neg_x,
            scroll_neg_y,
            ancestor_scroll: eff_scroll,
            is_root_elem: is_root,
            is_inline_non_replaced,
            overflow_x: frag.style.overflow.x,
            overflow_y: frag.style.overflow.y,
        });
    }
    let mut child_scroll = eff_scroll;
    if !is_root
        && (rix_layout::is_scroll_axis(frag.style.overflow.x)
            || rix_layout::is_scroll_axis(frag.style.overflow.y))
    {
        if let Some(nid) = frag.dom_node {
            if let Some(s) = scroll_map.get(&nid) {
                child_scroll.x += s.x;
                child_scroll.y += s.y;
            }
        }
    }
    let child_origin = abs_border.origin;
    for child in frag.children.iter() {
        if let Some(found) =
            find_element_geometry(child, child_origin, child_scroll, scroll_map, target, false)
        {
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
        let proto = h.proto_for_node(node_id);
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
    let make_proto = crate::dom_interfaces::make_proto;

    let event_target_proto = make_proto(vm, vm.object_proto);
    let node_proto = make_proto(vm, event_target_proto);
    let element_proto = make_proto(vm, node_proto);
    let html_element_proto = make_proto(vm, element_proto);
    let svg_element_proto = make_proto(vm, element_proto);
    let mathml_element_proto = make_proto(vm, element_proto);

    let interface_protos = crate::dom_interfaces::build_element_interface_protos(
        vm,
        html_element_proto,
        svg_element_proto,
    );
    let html_canvas_element_proto = interface_protos["HTMLCanvasElement"];
    let html_iframe_element_proto = interface_protos["HTMLIFrameElement"];
    let html_anchor_element_proto = interface_protos["HTMLAnchorElement"];
    let html_area_element_proto = interface_protos["HTMLAreaElement"];
    let html_base_element_proto = interface_protos["HTMLBaseElement"];
    let html_form_element_proto = interface_protos["HTMLFormElement"];
    let html_media_element_proto = interface_protos["HTMLMediaElement"];
    let html_video_element_proto = make_proto(vm, html_media_element_proto);
    let html_audio_element_proto = make_proto(vm, html_media_element_proto);

    let document_proto = make_proto(vm, node_proto);
    let html_document_proto = make_proto(vm, document_proto);
    let xml_document_proto = make_proto(vm, document_proto);
    let fragment_proto = make_proto(vm, node_proto);
    let shadow_root_proto = make_proto(vm, fragment_proto);
    let character_data_proto = make_proto(vm, node_proto);
    let text_proto = make_proto(vm, character_data_proto);
    let cdata_section_proto = make_proto(vm, text_proto);
    let comment_proto = make_proto(vm, character_data_proto);
    let processing_instruction_proto = make_proto(vm, character_data_proto);
    let document_type_proto = make_proto(vm, node_proto);
    let attr_proto = make_proto(vm, node_proto);

    let dom_implementation_proto = make_proto(vm, vm.object_proto);
    let dom_token_list_proto = make_proto(vm, vm.object_proto);
    let named_node_map_proto = make_proto(vm, vm.object_proto);
    let node_list_proto = make_proto(vm, vm.object_proto);
    let html_collection_proto = make_proto(vm, vm.object_proto);
    let dom_string_map_proto = make_proto(vm, vm.object_proto);
    let tree_walker_proto = make_proto(vm, vm.object_proto);
    let node_iterator_proto = make_proto(vm, vm.object_proto);

    let protos = DomPrototypes {
        event_target_proto,
        node_proto,
        element_proto,
        html_element_proto,
        html_canvas_element_proto,
        html_iframe_element_proto,
        html_anchor_element_proto,
        html_area_element_proto,
        html_base_element_proto,
        html_form_element_proto,
        html_media_element_proto,
        html_video_element_proto,
        html_audio_element_proto,
        svg_element_proto,
        mathml_element_proto,
        document_proto,
        html_document_proto,
        xml_document_proto,
        shadow_root_proto,
        character_data_proto,
        text_proto,
        cdata_section_proto,
        comment_proto,
        processing_instruction_proto,
        document_type_proto,
        attr_proto,
        fragment_proto,
        dom_implementation_proto,
        dom_token_list_proto,
        named_node_map_proto,
        node_list_proto,
        html_collection_proto,
        dom_string_map_proto,
        tree_walker_proto,
        node_iterator_proto,
    };

    let host = Rc::new(RefCell::new(HostState::new(
        document,
        protos,
        interface_protos,
        viewport,
        font_db,
    )));

    // HTML Living Standard §7.3.3.1: `Window` inherits from `WindowProperties` (named access
    // for element `id` / `name` attributes) -> `EventTarget.prototype` -> `Object.prototype`.
    install_window_properties_proxy(vm, &host, event_target_proto);

    install_node_prototype(vm, &host);
    install_element_prototype(vm, &host);
    install_document_prototype(vm, &host);
    install_shadow_root_prototype(vm, &host);
    install_fragment_prototype(vm, &host);
    crate::dom_interfaces::install_all_dom_interfaces(vm, &host);
    install_window_and_cssom(vm, &host);
    crate::selection::install_selection_and_range(vm, &host);

    // Pin root document wrapper on global `document`.
    let doc_root = host.borrow().document.borrow().root();
    let doc_val = wrap_node(vm, &host, doc_root);
    host.borrow_mut().wrappers.set_pinned(&doc_root, true);
    define_global(vm, "document", doc_val);
    let gv = JsValue::from_object(vm.global);
    define_global(vm, "window", gv);
    define_global(vm, "self", gv);
    define_global(vm, "globalThis", gv);

    // Global `DOMParser` and `XMLSerializer` constructors (WHATWG HTML §8.4).
    let dom_parser_proto = make_proto(vm, vm.object_proto);
    let h_dp = Rc::clone(&host);
    define_host_method(
        vm,
        dom_parser_proto,
        "parseFromString",
        move |vm, _, args| {
            let markup = vm.value_to_rust_string(arg(args, 0));
            let parsed = rix_html::parse_html(&markup);
            let sub_root = {
                let doc_rc = Rc::clone(&h_dp.borrow().document);
                let mut doc = doc_rc.borrow_mut();
                let r = doc.new_node(NodeData::Document);
                let pr = parsed.root();
                for child in parsed.children(pr) {
                    let cloned = doc.clone_subtree_from(&parsed, child);
                    doc.append_child(r, cloned);
                }
                r
            };
            Ok(wrap_node(vm, &h_dp, sub_root))
        },
    );
    let dom_parser_ctor = vm.alloc_host_fn("DOMParser", move |vm, _, _| {
        let inst = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(dom_parser_proto),
        ));
        Ok(JsValue::from_object(inst))
    });
    let k_proto = vm.key_prototype;
    vm.heap.object_define_data(
        dom_parser_ctor,
        k_proto,
        JsValue::from_object(dom_parser_proto),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.mark_constructor(dom_parser_ctor);
    define_global(vm, "DOMParser", JsValue::from_object(dom_parser_ctor));

    let xml_ser_proto = make_proto(vm, vm.object_proto);
    let h_xs = Rc::clone(&host);
    define_host_method(
        vm,
        xml_ser_proto,
        "serializeToString",
        move |vm, _, args| {
            let nid = require_node(vm, &h_xs, arg(args, 0))?;
            let html = h_xs.borrow().document.borrow().to_html_string(nid);
            Ok(JsValue::from_string(vm.heap.alloc_string(html)))
        },
    );
    let xml_ser_ctor = vm.alloc_host_fn("XMLSerializer", move |vm, _, _| {
        let inst = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(xml_ser_proto),
        ));
        Ok(JsValue::from_object(inst))
    });
    vm.heap.object_define_data(
        xml_ser_ctor,
        k_proto,
        JsValue::from_object(xml_ser_proto),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.mark_constructor(xml_ser_ctor);
    define_global(vm, "XMLSerializer", JsValue::from_object(xml_ser_ctor));

    host
}

pub(crate) fn define_global(vm: &mut JsVm, name: &str, val: JsValue) {
    let k = vm.heap.intern_key(name);
    let g = vm.global;
    vm.heap
        .object_define_data(g, k, val, PropertyAttrs::METHOD, &mut vm.shapes);
}

pub(crate) fn define_host_method(
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

pub(crate) fn define_host_accessor(
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

pub(crate) fn extract_node(vm: &JsVm, host: &SharedHost, val: JsValue) -> Option<NodeId> {
    host.borrow().node_for_value(vm, val)
}

pub(crate) fn require_node(
    vm: &mut JsVm,
    host: &SharedHost,
    val: JsValue,
) -> Result<NodeId, JsException> {
    host.borrow()
        .node_for_value(vm, val)
        .ok_or_else(|| vm.type_error("Illegal invocation: receiver is not a DOM Node"))
}

pub(crate) fn arg(args: &[JsValue], i: usize) -> JsValue {
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
            Some(NodeData::CDataSection(_)) => 4,
            Some(NodeData::ProcessingInstruction { .. }) => 7,
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
        let is_html_doc = !h.borrow().is_xml_doc_for_node(nid);
        let name = match h.borrow().document.borrow().get(nid) {
            Some(NodeData::Element(e)) => {
                let qn = match &e.name.prefix {
                    Some(p) if !p.is_empty() => format!("{p}:{}", e.name.local),
                    _ => e.name.local.as_str().to_string(),
                };
                if is_html_doc && &*e.name.ns == crate::dom_interfaces::HTML_NS {
                    qn.to_ascii_uppercase()
                } else {
                    qn
                }
            }
            Some(NodeData::Text(_)) => "#text".into(),
            Some(NodeData::CDataSection(_)) => "#cdata-section".into(),
            Some(NodeData::Comment(_)) => "#comment".into(),
            Some(NodeData::ProcessingInstruction { target, .. }) => target.clone(),
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

    // ownerDocument (WHATWG DOM §4.4: null for any Document node, else owning Document)
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get ownerDocument", move |vm, this_val, _| {
        let nid = require_node(vm, &h, this_val)?;
        let is_doc = matches!(
            h.borrow().document.borrow().get(nid),
            Some(NodeData::Document)
        );
        if is_doc {
            Ok(JsValue::NULL)
        } else {
            let owner = h.borrow().owner_doc_of(nid);
            Ok(wrap_node(vm, &h, owner))
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
        Ok(crate::dom_collections::create_live_child_nodes(vm, &h, nid))
    });
    define_host_accessor(vm, np, "childNodes", Some(g), None);

    // nodeValue (WHATWG DOM §4.4: data on CharacterData, null on Document/DocumentType/Element/Fragment)
    let h1 = Rc::clone(host);
    let g_nv = vm.alloc_host_fn("get nodeValue", move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        let val = match h1.borrow().document.borrow().get(nid) {
            Some(
                NodeData::Text(s)
                | NodeData::CDataSection(s)
                | NodeData::Comment(s)
                | NodeData::ProcessingInstruction { data: s, .. },
            ) => Some(s.clone()),
            _ => None,
        };
        Ok(val.map_or(JsValue::NULL, |s| {
            JsValue::from_string(vm.heap.alloc_string(s))
        }))
    });
    let h2 = Rc::clone(host);
    let s_nv = vm.alloc_host_fn("set nodeValue", move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let cur_len = match h2.borrow().document.borrow().get(nid) {
            Some(
                NodeData::Text(s)
                | NodeData::CDataSection(s)
                | NodeData::Comment(s)
                | NodeData::ProcessingInstruction { data: s, .. },
            ) => Some(s.encode_utf16().count()),
            _ => None,
        };
        if let Some(len) = cur_len {
            let new_text = if arg(args, 0).is_null() {
                String::new()
            } else {
                vm.value_to_rust_string(arg(args, 0))
            };
            crate::dom_interfaces::character_data_replace(vm, &h2, nid, 0, len, &new_text)?;
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, np, "nodeValue", Some(g_nv), Some(s_nv));

    // textContent (WHATWG DOM §4.4: null on Document/DocumentType, data on CharacterData, descendant text on Element/Fragment)
    let h1 = Rc::clone(host);
    let g_tc = vm.alloc_host_fn("get textContent", move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        let txt = {
            let doc_rc = Rc::clone(&h1.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(nid) {
                Some(NodeData::Document | NodeData::DocumentType { .. }) => None,
                Some(
                    NodeData::Text(s)
                    | NodeData::CDataSection(s)
                    | NodeData::Comment(s)
                    | NodeData::ProcessingInstruction { data: s, .. },
                ) => Some(s.clone()),
                _ => Some(doc.inner_text(nid)),
            }
        };
        Ok(txt.map_or(JsValue::NULL, |s| {
            JsValue::from_string(vm.heap.alloc_string(s))
        }))
    });
    let h2 = Rc::clone(host);
    let s_tc = vm.alloc_host_fn("set textContent", move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let v = arg(args, 0);
        let new_text = if v.is_null() || v.is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(v)
        };
        let (kind, cd_len) = match h2.borrow().document.borrow().get(nid) {
            Some(NodeData::Document | NodeData::DocumentType { .. }) => (0u8, 0usize),
            Some(
                NodeData::Text(s)
                | NodeData::CDataSection(s)
                | NodeData::Comment(s)
                | NodeData::ProcessingInstruction { data: s, .. },
            ) => (1u8, s.encode_utf16().count()),
            _ => (2u8, 0usize),
        };
        if kind == 1 {
            crate::dom_interfaces::character_data_replace(vm, &h2, nid, 0, cd_len, &new_text)?;
            return Ok(JsValue::UNDEFINED);
        }
        if kind == 0 {
            return Ok(JsValue::UNDEFINED);
        }
        let owner_doc = h2.borrow().owner_doc_of(nid);
        let removed: Vec<NodeId> = h2.borrow().document.borrow().children(nid).collect();
        for &child in &removed {
            crate::dom_collections::adjust_node_iterators_for_remove(&h2, child);
            crate::selection::adjust_ranges_for_remove_node(&h2, child, nid, 0);
            h2.borrow().document.borrow_mut().remove_child(child);
        }
        let mut added = Vec::new();
        if !new_text.is_empty() {
            let tn = h2.borrow().document.borrow_mut().create_text(&new_text);
            h2.borrow_mut().set_owner_doc(tn, owner_doc);
            h2.borrow().document.borrow_mut().append_child(nid, tn);
            crate::selection::adjust_ranges_for_insert_nodes(&h2, nid, 0, 1);
            added.push(tn);
        }
        if !removed.is_empty() || !added.is_empty() {
            h2.borrow_mut().invalidate_dom_mutation(nid, None);
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
    define_host_accessor(vm, np, "textContent", Some(g_tc), Some(s_tc));

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
        remove_node_from_parent(vm, &h, parent, child)?;
        Ok(child_val)
    });

    // replaceChild(newChild, oldChild)
    let h = Rc::clone(host);
    define_host_method(vm, np, "replaceChild", move |vm, t, args| {
        let parent = require_node(vm, &h, t)?;
        let new_child = require_node(vm, &h, arg(args, 0))?;
        let old_val = arg(args, 1);
        let old_child = require_node(vm, &h, old_val)?;
        {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            // WHATWG DOM §4.2.4 "replace": step 1 checks parent type BEFORE checking whether oldChild is a child of parent.
            if !matches!(
                doc.get(parent),
                Some(
                    NodeData::Document
                        | NodeData::DocumentFragment
                        | NodeData::ShadowRoot { .. }
                        | NodeData::Element(_)
                )
            ) {
                return Err(crate::web_apis::create_dom_exception(
                    vm,
                    "HierarchyRequestError",
                    "Parent node cannot have children",
                ));
            }
            if doc.parent(old_child) != Some(parent) {
                return Err(crate::web_apis::create_dom_exception(
                    vm,
                    "NotFoundError",
                    "The node to be replaced is not a child of this parent",
                ));
            }
            if new_child != old_child {
                crate::dom_interfaces::validate_pre_insert(
                    vm,
                    &doc,
                    parent,
                    new_child,
                    Some(old_child),
                )?;
            }
        }
        if new_child == old_child {
            return Ok(old_val);
        }
        let next_after_old = h.borrow().document.borrow().next_sibling(old_child);
        remove_node_from_parent(vm, &h, parent, old_child)?;
        let ref_node = if next_after_old == Some(new_child) {
            h.borrow().document.borrow().next_sibling(new_child)
        } else {
            next_after_old
        };
        insert_node_into_parent(vm, &h, parent, new_child, ref_node)?;
        Ok(old_val)
    });

    // cloneNode(deep?)
    let h = Rc::clone(host);
    define_host_method(vm, np, "cloneNode", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        if matches!(
            h.borrow().document.borrow().get(nid),
            Some(NodeData::ShadowRoot { .. })
        ) {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "NotSupportedError",
                "ShadowRoot nodes cannot be cloned",
            ));
        }
        let deep = vm.to_boolean(arg(args, 0));
        let owner_doc = {
            let hb = h.borrow();
            if matches!(hb.document.borrow().get(nid), Some(NodeData::Document)) {
                None
            } else {
                Some(hb.owner_doc_of(nid))
            }
        };
        let cloned = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let mut doc = _doc_rc.borrow_mut();
            if deep {
                let snapshot = doc.clone();
                doc.clone_subtree_with_options(&snapshot, nid, true)
            } else {
                let clonable_sr = match doc.get(nid) {
                    Some(NodeData::Element(el)) => {
                        el.shadow_root.and_then(|sr| match doc.get(sr) {
                            Some(NodeData::ShadowRoot {
                                mode,
                                delegates_focus,
                                clonable: true,
                                serializable,
                                slot_assignment,
                                ..
                            }) => Some((
                                sr,
                                mode.clone(),
                                *delegates_focus,
                                *serializable,
                                slot_assignment.clone(),
                            )),
                            _ => None,
                        })
                    }
                    _ => None,
                };
                let mut data = doc.get(nid).cloned().unwrap_or(NodeData::DocumentFragment);
                if let NodeData::Element(ref mut el) = data {
                    el.shadow_root = None;
                    el.content_document = None;
                    if el.template_contents.is_some() {
                        el.template_contents = Some(doc.new_node(NodeData::DocumentFragment));
                    }
                }
                let copy = doc.new_node(data);
                if let Some((src_sr, mode, delegates_focus, serializable, slot_assignment)) =
                    clonable_sr
                {
                    let snapshot = doc.clone();
                    let copy_sr = doc.new_node(NodeData::ShadowRoot {
                        host: copy,
                        mode,
                        delegates_focus,
                        clonable: true,
                        serializable,
                        slot_assignment,
                        declarative: false,
                    });
                    if let Some(NodeData::Element(el)) = doc.get_mut(copy) {
                        el.shadow_root = Some(copy_sr);
                    }
                    for ch in snapshot.children(src_sr) {
                        let cloned_ch = doc.clone_subtree_with_options(&snapshot, ch, true);
                        doc.append_child(copy_sr, cloned_ch);
                    }
                }
                copy
            }
        };
        if let Some(od) = owner_doc {
            h.borrow_mut().adopt_subtree(cloned, od);
        } else {
            let meta_opt = h.borrow().doc_meta.get(&nid).cloned();
            if let Some(meta) = meta_opt {
                h.borrow_mut().doc_meta.insert(cloned, meta);
                h.borrow_mut().adopt_subtree(cloned, cloned);
            }
        }
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

pub(crate) fn remove_node_from_parent(
    vm: &mut JsVm,
    host: &SharedHost,
    parent: NodeId,
    child: NodeId,
) -> Result<(), JsException> {
    let (was_connected, prev_sib, next_sib, old_index) = {
        let _doc_rc = Rc::clone(&host.borrow().document);
        let doc = _doc_rc.borrow();
        if doc.parent(child) != Some(parent) {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "NotFoundError",
                "The node to be removed is not a child of this node",
            ));
        }
        let idx = doc.children(parent).position(|c| c == child).unwrap_or(0);
        (
            doc.is_connected(child),
            doc.previous_sibling(child),
            doc.next_sibling(child),
            idx,
        )
    };
    crate::dom_collections::adjust_node_iterators_for_remove(host, child);
    crate::selection::adjust_ranges_for_remove_node(host, child, parent, old_index);
    host.borrow().document.borrow_mut().remove_child(child);
    host.borrow_mut().invalidate_dom_mutation(parent, None);
    host.borrow().emit_mutation(DomMutation::ChildList {
        target: parent,
        added: Vec::new(),
        removed: vec![child],
        previous_sibling: prev_sib,
        next_sibling: next_sib,
    });
    if was_connected {
        notify_custom_element_subtree(vm, host, child, false);
    }
    Ok(())
}

pub(crate) fn insert_node_into_parent(
    vm: &mut JsVm,
    host: &SharedHost,
    parent: NodeId,
    child: NodeId,
    before: Option<NodeId>,
) -> Result<(), JsException> {
    {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        crate::dom_interfaces::validate_pre_insert(vm, &doc, parent, child, before)?;
    }

    // WHATWG DOM §4.2.4 step 4: if `before == Some(child)`, advance `before` to `child.nextSibling`.
    let before = if before == Some(child) {
        host.borrow().document.borrow().next_sibling(child)
    } else {
        before
    };

    let is_frag = matches!(
        host.borrow().document.borrow().get(child),
        Some(NodeData::DocumentFragment | NodeData::ShadowRoot { .. })
    );
    let to_insert: Vec<NodeId> = if is_frag {
        host.borrow().document.borrow().children(child).collect()
    } else {
        vec![child]
    };

    // Adopt into parent's ownerDocument (or parent itself if parent is a Document).
    let target_doc = {
        let hb = host.borrow();
        if matches!(hb.document.borrow().get(parent), Some(NodeData::Document)) {
            parent
        } else {
            hb.owner_doc_of(parent)
        }
    };

    if is_frag {
        if to_insert.is_empty() {
            return Ok(());
        }
        // Remove all children from the fragment (firing live range updates & MutationObserver on fragment).
        for &n in &to_insert {
            crate::dom_collections::adjust_node_iterators_for_remove(host, n);
            crate::selection::adjust_ranges_for_remove_node(host, n, child, 0);
            host.borrow().document.borrow_mut().remove_child(n);
        }
        host.borrow().emit_mutation(DomMutation::ChildList {
            target: child,
            added: Vec::new(),
            removed: to_insert.clone(),
            previous_sibling: None,
            next_sibling: None,
        });
    } else {
        // If `child` currently has a parent, properly remove it first (adjusts ranges & emits removal mutation!).
        let old_parent = host.borrow().document.borrow().parent(child);
        if let Some(op) = old_parent {
            remove_node_from_parent(vm, host, op, child)?;
        }
    }

    for &n in &to_insert {
        host.borrow_mut().adopt_subtree(n, target_doc);
    }

    let parent_connected = host.borrow().document.borrow().is_connected(parent);
    let (prev_sib, next_sib, insert_idx) = {
        let _doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = _doc_rc.borrow_mut();
        let insert_idx = match before {
            Some(ref_n) => doc
                .children(parent)
                .position(|c| c == ref_n)
                .unwrap_or_else(|| doc.children(parent).count()),
            None => doc.children(parent).count(),
        };
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
        (prev, before, insert_idx)
    };
    crate::selection::adjust_ranges_for_insert_nodes(host, parent, insert_idx, to_insert.len());
    host.borrow_mut()
        .invalidate_dom_mutation(parent, to_insert.first().copied());
    host.borrow().emit_mutation(DomMutation::ChildList {
        target: parent,
        added: to_insert.clone(),
        removed: Vec::new(),
        previous_sibling: prev_sib,
        next_sibling: next_sib,
    });
    if parent_connected {
        let iframe_hook = host.borrow().iframe_connect_hook.clone();
        for n in to_insert {
            notify_custom_element_subtree(vm, host, n, true);
            // Execute any `<script>` nodes created by `Range.prototype.createContextualFragment()`
            // or disconnected `*HTMLUnsafe({ runScripts: true })` / `stream*HTMLUnsafe({ runScripts: true })`.
            let script_hook = host.borrow().script_connect_hook.clone();
            let scripts_to_run: Vec<(NodeId, String)> = {
                let mut hb = host.borrow_mut();
                if hb.contextual_scripts.is_empty() {
                    Vec::new()
                } else {
                    let doc_rc = Rc::clone(&hb.document);
                    let doc = doc_rc.borrow();
                    let found: Vec<(NodeId, String)> = doc
                        .descendants(n)
                        .filter(|id| hb.contextual_scripts.contains(id))
                        .map(|id| {
                            let mut code = String::new();
                            for c in doc.descendants(id) {
                                if let Some(NodeData::Text(t)) = doc.get(c) {
                                    code.push_str(t);
                                }
                            }
                            (id, code)
                        })
                        .collect();
                    for (id, _) in &found {
                        hb.contextual_scripts.remove(id);
                    }
                    found
                }
            };
            for (sid, code) in scripts_to_run {
                if let Some(ref hook) = script_hook {
                    hook(vm, sid);
                } else if !code.trim().is_empty() {
                    let prev = host.borrow().current_script;
                    host.borrow_mut().current_script = Some(sid);
                    let _ = vm.eval_script(&code);
                    host.borrow_mut().current_script = prev;
                }
            }
            if let Some(ref hook) = iframe_hook {
                let iframes: Vec<NodeId> = {
                    let doc_rc = Rc::clone(&host.borrow().document);
                    let doc = doc_rc.borrow();
                    doc.descendants(n)
                        .filter(|&id| {
                            matches!(
                                doc.get(id),
                                Some(NodeData::Element(e))
                                    if e.name.local.as_str().eq_ignore_ascii_case("iframe")
                            )
                        })
                        .collect()
                };
                for fid in iframes {
                    hook(vm, fid);
                }
            }
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
        let is_html_doc = !h.borrow().is_xml_doc_for_node(nid);
        let tag = match h.borrow().document.borrow().get(nid) {
            Some(NodeData::Element(e)) => {
                let qn = match &e.name.prefix {
                    Some(p) if !p.is_empty() => format!("{p}:{}", e.name.local),
                    _ => e.name.local.as_str().to_string(),
                };
                if is_html_doc && &*e.name.ns == crate::dom_interfaces::HTML_NS {
                    qn.to_ascii_uppercase()
                } else {
                    qn
                }
            }
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

    // classList (live DOMTokenList + [PutForwards=value] setter)
    let h1 = Rc::clone(host);
    let g = vm.alloc_host_fn("get classList", move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        Ok(JsValue::from_object(
            crate::dom_collections::get_or_create_token_list(vm, &h1, nid, "class", None),
        ))
    });
    let h2 = Rc::clone(host);
    let s = vm.alloc_host_fn("set classList", move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let val = vm.value_to_rust_string(arg(args, 0));
        set_element_attr(vm, &h2, nid, "class", &val);
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, ep, "classList", Some(g), Some(s));

    // style (live CSSStyleDeclaration Proxy + [PutForwards=cssText] setter)
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get style", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        Ok(crate::cssom::create_inline_style_proxy(vm, &h, nid))
    });
    let h2 = Rc::clone(host);
    let s = vm.alloc_host_fn("set style", move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let raw = if arg(args, 0).is_null() || arg(args, 0).is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(arg(args, 0))
        };
        crate::cssom::set_inline_style_css_text(vm, &h2, nid, &raw);
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, ep, "style", Some(g), Some(s));

    // innerText (getter + setter, HTML §4.4 / CSSOM View)
    let h1 = Rc::clone(host);
    let g_it = vm.alloc_host_fn("get innerText", move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        let txt = crate::selection::compute_inner_text(&h1, nid);
        Ok(JsValue::from_string(vm.heap.alloc_string(txt)))
    });
    let h2 = Rc::clone(host);
    let s_it = vm.alloc_host_fn("set innerText", move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let new_text = if arg(args, 0).is_null() || arg(args, 0).is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(arg(args, 0))
        };
        let (removed, added) = {
            let doc_rc = Rc::clone(&h2.borrow().document);
            let mut doc = doc_rc.borrow_mut();
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
        h2.borrow_mut().invalidate_dom_mutation(nid, None);
        h2.borrow().emit_mutation(DomMutation::ChildList {
            target: nid,
            added,
            removed,
            previous_sibling: None,
            next_sibling: None,
        });
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, ep, "innerText", Some(g_it), Some(s_it));

    // dataset (live Proxy over `data-*` attributes)
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get dataset", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        Ok(create_dataset_proxy(vm, &h, nid))
    });
    define_host_accessor(vm, ep, "dataset", Some(g), None);

    // namespaceURI / localName / prefix (WHATWG DOM §4.9)
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get namespaceURI", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let ns = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(nid) {
                Some(NodeData::Element(e)) if !e.name.ns.is_empty() => Some(e.name.ns.to_string()),
                _ => None,
            }
        };
        Ok(match ns {
            Some(s) => JsValue::from_string(vm.heap.alloc_string(s)),
            None => JsValue::NULL,
        })
    });
    define_host_accessor(vm, ep, "namespaceURI", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get localName", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let local = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(nid) {
                Some(NodeData::Element(e)) => e.name.local.to_string(),
                _ => String::new(),
            }
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(local)))
    });
    define_host_accessor(vm, ep, "localName", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get prefix", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let pref = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(nid) {
                Some(NodeData::Element(e)) => e.name.prefix.as_ref().map(|p| p.to_string()),
                _ => None,
            }
        };
        Ok(match pref {
            Some(s) => JsValue::from_string(vm.heap.alloc_string(s)),
            None => JsValue::NULL,
        })
    });
    define_host_accessor(vm, ep, "prefix", Some(g), None);

    // WHATWG DOM §4.9: `Element.prototype.attributes` (live NamedNodeMap Proxy)
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get attributes", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        Ok(crate::dom_collections::create_named_node_map_proxy(
            vm, &h, nid,
        ))
    });
    define_host_accessor(vm, ep, "attributes", Some(g), None);

    // Attribute methods
    let h = Rc::clone(host);
    define_host_method(vm, ep, "hasAttributes", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let has = matches!(
            h.borrow().document.borrow().get(nid),
            Some(NodeData::Element(e)) if !e.attributes.is_empty()
        );
        Ok(JsValue::from_bool(has))
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "getAttributeNames", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let names: Vec<String> = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(nid) {
                Some(NodeData::Element(e)) => e
                    .attributes
                    .iter()
                    .map(|(q, _)| match &q.prefix {
                        Some(p) if !p.is_empty() => format!("{p}:{}", q.local),
                        _ => q.local.to_string(),
                    })
                    .collect(),
                _ => Vec::new(),
            }
        };
        let arr = vm.alloc_array();
        for n in names {
            let v = JsValue::from_string(vm.heap.alloc_string(n));
            vm.heap.object_push_element(arr, v);
        }
        Ok(JsValue::from_object(arr))
    });
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
        if args.len() < 2 {
            return Err(vm.type_error("setAttribute requires 2 arguments"));
        }
        let nid = require_node(vm, &h, t)?;
        let name = vm.value_to_rust_string(args[0]);
        if !crate::dom_interfaces::is_valid_attribute_local_name(&name) {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "InvalidCharacterError",
                "The attribute name contains an invalid character",
            ));
        }
        let val = vm.value_to_rust_string(args[1]);
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
    let h = Rc::clone(host);
    define_host_method(vm, ep, "getAttributeNode", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let map = crate::dom_collections::create_named_node_map_proxy(vm, &h, nid);
        let mh = map.as_object().unwrap();
        let k = vm.heap.intern_key("getNamedItem");
        let f = vm.get_property_by_key(mh, k)?;
        vm.call_value(f, map, &[arg(args, 0)])
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "getAttributeNodeNS", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let map = crate::dom_collections::create_named_node_map_proxy(vm, &h, nid);
        let mh = map.as_object().unwrap();
        let k = vm.heap.intern_key("getNamedItemNS");
        let f = vm.get_property_by_key(mh, k)?;
        vm.call_value(f, map, &[arg(args, 0), arg(args, 1)])
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "setAttributeNode", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        crate::dom_collections::set_attribute_node_on_element(vm, &h, nid, arg(args, 0))
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "setAttributeNodeNS", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        crate::dom_collections::set_attribute_node_on_element(vm, &h, nid, arg(args, 0))
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "removeAttributeNode", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let Some(ah) = arg(args, 0).as_object() else {
            return Err(vm.type_error("Argument must be an Attr"));
        };
        let Some(entry) = h.borrow().attrs.get(&ah).cloned() else {
            return Err(vm.type_error("Argument must be an Attr"));
        };
        if entry.owner_element != Some(nid) {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "NotFoundError",
                "Attribute does not belong to this element",
            ));
        }
        crate::dom_collections::remove_attribute_node_from_element(
            vm,
            &h,
            nid,
            &entry.namespace,
            &entry.local_name,
        )
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "getAttributeNS", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let ns_arg = arg(args, 0);
        let ns = if ns_arg.is_null() || ns_arg.is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(ns_arg)
        };
        let local = vm.value_to_rust_string(arg(args, 1));
        let val = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(nid) {
                Some(NodeData::Element(e)) => e.get_attribute_ns(&ns, &local).map(str::to_string),
                _ => None,
            }
        };
        Ok(match val {
            Some(v) => JsValue::from_string(vm.heap.alloc_string(v)),
            None => JsValue::NULL,
        })
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "setAttributeNS", move |vm, t, args| {
        if args.len() < 3 {
            return Err(vm.type_error("setAttributeNS requires 3 arguments"));
        }
        let nid = require_node(vm, &h, t)?;
        let ns_arg = args[0];
        let ns = if ns_arg.is_null() || ns_arg.is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(ns_arg)
        };
        let qname = vm.value_to_rust_string(args[1]);
        let val = vm.value_to_rust_string(args[2]);
        let (ns, prefix, local) =
            crate::dom_interfaces::validate_and_extract(vm, &ns, &qname, false)?;
        let old_val = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            match doc.get_mut(nid) {
                Some(NodeData::Element(e)) => e
                    .set_attribute_ns(&ns, prefix.as_deref(), &local, &val)
                    .map(|(old, _)| old),
                _ => return Ok(JsValue::UNDEFINED),
            }
        };
        h.borrow_mut()
            .invalidate_attr_mutation(nid, &local, old_val.as_deref(), Some(&val));
        h.borrow().emit_mutation(DomMutation::Attributes {
            target: nid,
            attribute_name: local,
            attribute_namespace: if ns.is_empty() { None } else { Some(ns) },
            old_value: old_val,
        });
        Ok(JsValue::UNDEFINED)
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "removeAttributeNS", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let ns_arg = arg(args, 0);
        let ns = if ns_arg.is_null() || ns_arg.is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(ns_arg)
        };
        let local = vm.value_to_rust_string(arg(args, 1));
        let old_val = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            match doc.get_mut(nid) {
                Some(NodeData::Element(e)) => e.remove_attribute_ns(&ns, &local),
                _ => None,
            }
        };
        h.borrow_mut()
            .invalidate_attr_mutation(nid, &local, old_val.as_deref(), None);
        if old_val.is_some() {
            h.borrow().emit_mutation(DomMutation::Attributes {
                target: nid,
                attribute_name: local,
                attribute_namespace: if ns.is_empty() { None } else { Some(ns) },
                old_value: old_val,
            });
        }
        Ok(JsValue::UNDEFINED)
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "hasAttributeNS", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let ns_arg = arg(args, 0);
        let ns = if ns_arg.is_null() || ns_arg.is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(ns_arg)
        };
        let local = vm.value_to_rust_string(arg(args, 1));
        let has = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(nid) {
                Some(NodeData::Element(e)) => e.get_attribute_ns(&ns, &local).is_some(),
                _ => false,
            }
        };
        Ok(JsValue::from_bool(has))
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "toggleAttribute", move |vm, t, args| {
        if args.is_empty() {
            return Err(vm.type_error("toggleAttribute requires at least 1 argument"));
        }
        let nid = require_node(vm, &h, t)?;
        let name = vm.value_to_rust_string(args[0]);
        if !crate::dom_interfaces::is_valid_attribute_local_name(&name) {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "InvalidCharacterError",
                "The attribute name contains an invalid character",
            ));
        }
        let has = get_attr_string(&h, nid, &name).is_some();
        let force = args.get(1).copied();
        let want = match force {
            Some(v) if !v.is_undefined() => vm.to_boolean(v),
            _ => !has,
        };
        if want {
            if !has {
                set_element_attr(vm, &h, nid, &name, "");
            }
            Ok(JsValue::TRUE)
        } else {
            if has {
                remove_element_attr(vm, &h, nid, &name);
            }
            Ok(JsValue::FALSE)
        }
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "insertAdjacentHTML", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let pos = vm.value_to_rust_string(arg(args, 0)).to_ascii_lowercase();
        let html = vm.value_to_rust_string(arg(args, 1));
        insert_adjacent_html(vm, &h, nid, &pos, &html);
        Ok(JsValue::UNDEFINED)
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "insertAdjacentElement", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let pos = vm.value_to_rust_string(arg(args, 0)).to_ascii_lowercase();
        let el_val = arg(args, 1);
        let new_el = require_node(vm, &h, el_val)?;
        match pos.as_str() {
            "beforebegin" => {
                let parent_opt = h.borrow().document.borrow().parent(nid);
                if let Some(p) = parent_opt {
                    insert_node_into_parent(vm, &h, p, new_el, Some(nid))?;
                    return Ok(el_val);
                }
                Ok(JsValue::NULL)
            }
            "afterbegin" => {
                let first = h.borrow().document.borrow().first_child(nid);
                insert_node_into_parent(vm, &h, nid, new_el, first)?;
                Ok(el_val)
            }
            "beforeend" => {
                insert_node_into_parent(vm, &h, nid, new_el, None)?;
                Ok(el_val)
            }
            "afterend" => {
                let info = {
                    let doc_rc = Rc::clone(&h.borrow().document);
                    let doc = doc_rc.borrow();
                    doc.parent(nid).map(|p| (p, doc.next_sibling(nid)))
                };
                if let Some((p, next)) = info {
                    insert_node_into_parent(vm, &h, p, new_el, next)?;
                    return Ok(el_val);
                }
                Ok(JsValue::NULL)
            }
            _ => Err(crate::web_apis::create_dom_exception(
                vm,
                "SyntaxError",
                "Invalid insertAdjacentElement position",
            )),
        }
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "insertAdjacentText", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let pos = vm.value_to_rust_string(arg(args, 0)).to_ascii_lowercase();
        let data = vm.value_to_rust_string(arg(args, 1));
        let txt = h.borrow().document.borrow_mut().create_text(&data);
        match pos.as_str() {
            "beforebegin" => {
                let parent_opt = h.borrow().document.borrow().parent(nid);
                if let Some(p) = parent_opt {
                    insert_node_into_parent(vm, &h, p, txt, Some(nid))?;
                }
            }
            "afterbegin" => {
                let first = h.borrow().document.borrow().first_child(nid);
                insert_node_into_parent(vm, &h, nid, txt, first)?;
            }
            "beforeend" => {
                insert_node_into_parent(vm, &h, nid, txt, None)?;
            }
            "afterend" => {
                let info = {
                    let doc_rc = Rc::clone(&h.borrow().document);
                    let doc = doc_rc.borrow();
                    doc.parent(nid).map(|p| (p, doc.next_sibling(nid)))
                };
                if let Some((p, next)) = info {
                    insert_node_into_parent(vm, &h, p, txt, next)?;
                }
            }
            _ => {
                return Err(crate::web_apis::create_dom_exception(
                    vm,
                    "SyntaxError",
                    "Invalid insertAdjacentText position",
                ));
            }
        }
        Ok(JsValue::UNDEFINED)
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "focus", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let can_focus = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(nid) {
                Some(NodeData::Element(e)) => {
                    let tag = e.name.local.as_str();
                    doc.is_connected(nid)
                        && !e.has_attribute("disabled")
                        && (e.has_attribute("tabindex")
                            || e.has_attribute("contenteditable")
                            || matches!(
                                tag,
                                "a" | "area"
                                    | "button"
                                    | "input"
                                    | "select"
                                    | "textarea"
                                    | "iframe"
                            ))
                }
                _ => false,
            }
        };
        if can_focus {
            h.borrow_mut().set_focused_element(Some(nid), true);
        }
        Ok(JsValue::UNDEFINED)
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "blur", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        if h.borrow().active_element == Some(nid) {
            h.borrow_mut().set_focused_element(None, false);
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_method(vm, ep, "scrollIntoView", |_, _, _| Ok(JsValue::UNDEFINED));

    // Element traversal getters (live HTMLCollection + childElementCount)
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get children", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        Ok(crate::dom_collections::create_live_html_collection(
            vm,
            &h,
            crate::dom_interfaces::HtmlCollectionKind::Children(nid),
        ))
    });
    define_host_accessor(vm, ep, "children", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get childElementCount", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let cnt = h.borrow().document.borrow().child_element_count(nid);
        Ok(JsValue::from_f64(cnt as f64))
    });
    define_host_accessor(vm, ep, "childElementCount", Some(g), None);

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

    // Selector queries: querySelector, querySelectorAll, matches, closest (Selectors Level 4)
    let h = Rc::clone(host);
    define_host_method(vm, ep, "querySelector", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let sel = vm.value_to_rust_string(arg(args, 0));
        let parsed = parse_selector_list_strict(&sel)
            .map_err(|e| crate::web_apis::create_dom_exception(vm, "SyntaxError", &e))?;
        let found = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            query_selector_in_subtree(&doc, nid, &parsed, Some(nid), None)
        };
        Ok(found.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "querySelectorAll", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let sel = vm.value_to_rust_string(arg(args, 0));
        let parsed = parse_selector_list_strict(&sel)
            .map_err(|e| crate::web_apis::create_dom_exception(vm, "SyntaxError", &e))?;
        let list = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            query_selector_all_in_subtree(&doc, nid, &parsed, Some(nid), None)
        };
        Ok(crate::dom_collections::create_static_node_list(
            vm, &h, list,
        ))
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "matches", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let sel = vm.value_to_rust_string(arg(args, 0));
        let parsed = parse_selector_list_strict(&sel)
            .map_err(|e| crate::web_apis::create_dom_exception(vm, "SyntaxError", &e))?;
        let _doc_rc = Rc::clone(&h.borrow().document);
        let doc = _doc_rc.borrow();
        let ok = matches_element_with_scope(&doc, nid, &parsed, Some(nid));
        Ok(JsValue::from_bool(ok))
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "closest", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let sel = vm.value_to_rust_string(arg(args, 0));
        let parsed = parse_selector_list_strict(&sel)
            .map_err(|e| crate::web_apis::create_dom_exception(vm, "SyntaxError", &e))?;
        let found = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let doc = _doc_rc.borrow();
            let mut cur = Some(nid);
            let mut matched = None;
            while let Some(n) = cur {
                if matches!(doc.get(n), Some(NodeData::Element(_)))
                    && matches_element_with_scope(&doc, n, &parsed, Some(nid))
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
        let is_html_doc = !h.borrow().is_xml_doc_for_node(nid);
        Ok(crate::dom_collections::create_live_html_collection(
            vm,
            &h,
            crate::dom_interfaces::HtmlCollectionKind::ByTagName {
                root: nid,
                qname: tag,
                is_html_doc,
            },
        ))
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "getElementsByTagNameNS", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let ns_arg = arg(args, 0);
        let ns = if ns_arg.is_null() || ns_arg.is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(ns_arg)
        };
        let local = vm.value_to_rust_string(arg(args, 1));
        Ok(crate::dom_collections::create_live_html_collection(
            vm,
            &h,
            crate::dom_interfaces::HtmlCollectionKind::ByTagNameNS {
                root: nid,
                ns,
                local,
            },
        ))
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "getElementsByClassName", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let cls = vm.value_to_rust_string(arg(args, 0));
        let classes: Vec<String> = cls
            .split(['\t', '\n', '\x0C', '\r', ' '])
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let quirks = h.borrow().document.borrow().quirks_mode() == 2;
        Ok(crate::dom_collections::create_live_html_collection(
            vm,
            &h,
            crate::dom_interfaces::HtmlCollectionKind::ByClassName {
                root: nid,
                classes,
                quirks,
            },
        ))
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

    // Modern DOM ChildNode & ParentNode mixins: remove(), before(), after(), replaceWith(), append(), prepend(), replaceChildren()
    install_child_node_mixin(vm, host, ep);
    install_parent_node_mixin(vm, host, ep);

    // Shadow DOM: attachShadow({ mode, delegatesFocus, slotAssignment, clonable, serializable }) & shadowRoot getter
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
        let k_sa = vm.heap.intern_key("slotAssignment");
        let sa_val = vm.get_property_by_key(opts_h, k_sa)?;
        let slot_assignment = if sa_val.is_undefined() {
            "named".to_string()
        } else {
            let s = vm.value_to_rust_string(sa_val);
            if s != "named" && s != "manual" {
                return Err(vm.type_error("ShadowRoot slotAssignment must be 'named' or 'manual'"));
            }
            s
        };
        let k_df = vm.heap.intern_key("delegatesFocus");
        let delegates_focus = vm
            .get_property_by_key(opts_h, k_df)
            .is_ok_and(|v| vm.to_boolean(v));
        let k_cl = vm.heap.intern_key("clonable");
        let clonable = vm
            .get_property_by_key(opts_h, k_cl)
            .is_ok_and(|v| vm.to_boolean(v));
        let k_ser = vm.heap.intern_key("serializable");
        let serializable = vm
            .get_property_by_key(opts_h, k_ser)
            .is_ok_and(|v| vm.to_boolean(v));
        let owner_doc = h.borrow().owner_doc_of(host_id);
        let sr_id = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let mut doc = _doc_rc.borrow_mut();
            let (is_html_valid, existing_sr) = match doc.get(host_id) {
                Some(NodeData::Element(e)) => (
                    &*e.name.ns == crate::dom_interfaces::HTML_NS
                        && is_valid_shadow_host_name(&e.name.local),
                    e.shadow_root,
                ),
                _ => (false, None),
            };
            if !is_html_valid {
                return Err(crate::web_apis::create_dom_exception(
                    vm,
                    "NotSupportedError",
                    "Element cannot attach a shadow root",
                ));
            }
            if let Some(ex_sr) = existing_sr {
                // WHATWG DOM §4.9: if host already has a declarative shadow root with matching mode,
                // clear its children, mark it non-declarative, and return the same ShadowRoot!
                let can_adopt_declarative = matches!(
                    doc.get(ex_sr),
                    Some(NodeData::ShadowRoot {
                        declarative: true,
                        mode: ref ex_mode,
                        ..
                    }) if ex_mode == &mode
                );
                if !can_adopt_declarative {
                    return Err(crate::web_apis::create_dom_exception(
                        vm,
                        "NotSupportedError",
                        "Shadow root cannot be created on a host which already hosts a shadow tree",
                    ));
                }
                let old_children: Vec<NodeId> = doc.children(ex_sr).collect();
                for ch in old_children {
                    doc.remove_child(ch);
                }
                if let Some(NodeData::ShadowRoot { declarative, .. }) = doc.get_mut(ex_sr) {
                    *declarative = false;
                }
                ex_sr
            } else {
                let sr = doc.new_node(NodeData::ShadowRoot {
                    host: host_id,
                    mode,
                    delegates_focus,
                    clonable,
                    serializable,
                    slot_assignment,
                    declarative: false,
                });
                if let Some(NodeData::Element(e)) = doc.get_mut(host_id) {
                    e.shadow_root = Some(sr);
                }
                sr
            }
        };
        h.borrow_mut().set_owner_doc(sr_id, owner_doc);
        crate::shadow_dom::refresh_slots_for_shadow_root(&h, sr_id);
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

    // W3C CSSOM View §7.1: `offsetParent`, `offsetLeft`, `offsetTop`, `offsetWidth`, `offsetHeight`,
    // `clientLeft`, `clientTop`, `clientWidth`, `clientHeight`, `scrollWidth`, `scrollHeight`,
    // `scrollLeft`, `scrollTop`, `scrollTo`, `scroll`, `scrollBy`.
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get offsetParent", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let op = h.borrow_mut().offset_parent_for_node(nid);
        Ok(op.map_or(JsValue::NULL, |p| wrap_node(vm, &h, p)))
    });
    define_host_accessor(vm, ep, "offsetParent", Some(g), None);

    for (prop, field) in [
        ("offsetLeft", 0u8),
        ("offsetTop", 1),
        ("offsetWidth", 2),
        ("offsetHeight", 3),
        ("clientLeft", 4),
        ("clientTop", 5),
        ("clientWidth", 6),
        ("clientHeight", 7),
        ("scrollWidth", 8),
        ("scrollHeight", 9),
    ] {
        let h = Rc::clone(host);
        let g = vm.alloc_host_fn(prop, move |vm, t, _| {
            let nid = require_node(vm, &h, t)?;
            let mut hs = h.borrow_mut();
            let val = match field {
                0 | 1 => {
                    let Some(info) = hs.element_geometry_for_node(nid) else {
                        return Ok(JsValue::from_f64(0.0));
                    };
                    let is_root_or_body = {
                        let doc = hs.document.borrow();
                        matches!(
                            doc.get(nid),
                            Some(NodeData::Element(e))
                                if e.name.local.as_str().eq_ignore_ascii_case("html")
                                    || e.name.local.as_str().eq_ignore_ascii_case("body")
                        )
                    };
                    if is_root_or_body {
                        0.0
                    } else if let Some(op) = hs.offset_parent_for_node(nid) {
                        let op_is_static_body = {
                            let doc = hs.document.borrow();
                            let is_body = matches!(
                                doc.get(op),
                                Some(NodeData::Element(e))
                                    if e.name.local.as_str().eq_ignore_ascii_case("body")
                            );
                            is_body
                                && hs
                                    .style_ctx
                                    .style_for(op)
                                    .is_none_or(|s| s.position == rix_css::PositionMode::Static)
                        };
                        if op_is_static_body {
                            if field == 0 {
                                info.abs_border.origin.x
                            } else {
                                info.abs_border.origin.y
                            }
                        } else if let Some(op_info) = hs.element_geometry_for_node(op) {
                            if field == 0 {
                                info.abs_border.origin.x - op_info.abs_padding.origin.x
                            } else {
                                info.abs_border.origin.y - op_info.abs_padding.origin.y
                            }
                        } else if field == 0 {
                            info.abs_border.origin.x
                        } else {
                            info.abs_border.origin.y
                        }
                    } else if field == 0 {
                        info.abs_border.origin.x
                    } else {
                        info.abs_border.origin.y
                    }
                }
                2 => hs
                    .element_geometry_for_node(nid)
                    .map_or(0.0, |i| i.abs_border.size.width),
                3 => hs
                    .element_geometry_for_node(nid)
                    .map_or(0.0, |i| i.abs_border.size.height),
                4 => hs
                    .element_geometry_for_node(nid)
                    .map_or(0.0, |i| i.border.left),
                5 => hs
                    .element_geometry_for_node(nid)
                    .map_or(0.0, |i| i.border.top),
                6 => hs.client_size_for_node(nid).0,
                7 => hs.client_size_for_node(nid).1,
                8 => hs.scroll_size_for_node(nid).0,
                _ => hs.scroll_size_for_node(nid).1,
            };
            Ok(JsValue::from_f64(f64::from(val.round())))
        });
        define_host_accessor(vm, ep, prop, Some(g), None);
    }

    for (prop, is_y) in [("scrollLeft", false), ("scrollTop", true)] {
        let hg = Rc::clone(host);
        let g = vm.alloc_host_fn(prop, move |vm, t, _| {
            let nid = require_node(vm, &hg, t)?;
            let off = hg.borrow_mut().scroll_offset_for_node(nid);
            Ok(JsValue::from_f64(f64::from(if is_y {
                off.y
            } else {
                off.x
            })))
        });
        let hs_set = Rc::clone(host);
        let s = vm.alloc_host_fn(prop, move |vm, t, args| {
            let nid = require_node(vm, &hs_set, t)?;
            let v = vm.to_number(arg(args, 0))?;
            if v.is_finite() {
                if is_y {
                    hs_set.borrow_mut().set_scroll_offset_for_node(
                        nid,
                        None,
                        Some(v as f32),
                        false,
                    );
                } else {
                    hs_set.borrow_mut().set_scroll_offset_for_node(
                        nid,
                        Some(v as f32),
                        None,
                        false,
                    );
                }
            }
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, ep, prop, Some(g), Some(s));
    }

    for (name, relative) in [("scrollTo", false), ("scroll", false), ("scrollBy", true)] {
        let h = Rc::clone(host);
        define_host_method(vm, ep, name, move |vm, t, args| {
            let nid = require_node(vm, &h, t)?;
            let first = arg(args, 0);
            let (mut x_opt, mut y_opt) = (None, None);
            if let Some(obj) = first.as_object() {
                let k_left = vm.heap.intern_key("left");
                let k_top = vm.heap.intern_key("top");
                if let Ok(v) = vm.get_property_by_key(obj, k_left) {
                    if !v.is_undefined() {
                        let n = vm.to_number(v)?;
                        if n.is_finite() {
                            x_opt = Some(n as f32);
                        }
                    }
                }
                if let Ok(v) = vm.get_property_by_key(obj, k_top) {
                    if !v.is_undefined() {
                        let n = vm.to_number(v)?;
                        if n.is_finite() {
                            y_opt = Some(n as f32);
                        }
                    }
                }
            } else {
                let nx = vm.to_number(first)?;
                let ny = vm.to_number(arg(args, 1))?;
                if nx.is_finite() {
                    x_opt = Some(nx as f32);
                }
                if ny.is_finite() {
                    y_opt = Some(ny as f32);
                }
            }
            h.borrow_mut()
                .set_scroll_offset_for_node(nid, x_opt, y_opt, relative);
            Ok(JsValue::UNDEFINED)
        });
    }

    install_html5_element_semantics(vm, host, ep);
}

/// Installs WHATWG HTML5 element semantics on `Element.prototype`:
/// `<template>.content`, `<dialog>`, `<details>`, `<form>` / Constraint Validation API,
/// `<select>`/`<option>`/`<label>`, Popover API, and standard boolean/string attribute reflections.
fn install_html5_element_semantics(vm: &mut JsVm, host: &SharedHost, ep: HeapHandle) {
    // `<template>.content` -> inert `DocumentFragment` (HTMLTemplateElement only, not SVG `<template>`)
    let template_proto = host
        .borrow()
        .interface_protos
        .get("HTMLTemplateElement")
        .copied()
        .unwrap_or(ep);
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get content", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let frag = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(nid) {
                Some(NodeData::Element(el)) => el.template_contents,
                _ => None,
            }
        };
        Ok(frag.map_or(JsValue::UNDEFINED, |f| wrap_node(vm, &h, f)))
    });
    define_host_accessor(vm, template_proto, "content", Some(g), None);

    // Reflected boolean attributes (`open`, `hidden`, `disabled`, `required`, `readOnly`,
    // `checked`, `multiple`, `noValidate`, `selected`).
    for (prop, attr) in [
        ("open", "open"),
        ("hidden", "hidden"),
        ("disabled", "disabled"),
        ("required", "required"),
        ("readOnly", "readonly"),
        ("checked", "checked"),
        ("multiple", "multiple"),
        ("noValidate", "novalidate"),
        ("selected", "selected"),
    ] {
        let h1 = Rc::clone(host);
        let g =
            vm.alloc_host_fn(prop, move |vm, t, _| {
                let nid = require_node(vm, &h1, t)?;
                let present =
                    h1.borrow().document.borrow().get(nid).is_some_and(
                        |d| matches!(d, NodeData::Element(e) if e.has_attribute(attr)),
                    );
                Ok(JsValue::from_bool(present))
            });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn(prop, move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            let on = vm.to_boolean(arg(args, 0));
            if on {
                set_element_attr(vm, &h2, nid, attr, "");
            } else {
                remove_element_attr(vm, &h2, nid, attr);
            }
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, ep, prop, Some(g), Some(s));
    }

    // Reflected string attributes (`name`, `placeholder`, `title`, `lang`, `dir`, `src`).
    for (prop, attr) in [
        ("name", "name"),
        ("placeholder", "placeholder"),
        ("title", "title"),
        ("lang", "lang"),
        ("dir", "dir"),
        ("src", "src"),
    ] {
        let h1 = Rc::clone(host);
        let g = vm.alloc_host_fn(prop, move |vm, t, _| {
            let nid = require_node(vm, &h1, t)?;
            let val = h1
                .borrow()
                .document
                .borrow()
                .get(nid)
                .and_then(|d| match d {
                    NodeData::Element(e) => e.get_attribute(attr).map(str::to_string),
                    _ => None,
                })
                .unwrap_or_default();
            Ok(JsValue::from_string(vm.heap.alloc_string(val)))
        });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn(prop, move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            let val = vm.value_to_rust_string(arg(args, 0));
            set_element_attr(vm, &h2, nid, attr, &val);
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, ep, prop, Some(g), Some(s));
    }

    // `HTMLLabelElement.prototype.htmlFor` reflects the `for` attribute (HTMLOutputElement uses DOMTokenList).
    if let Some(&label_proto) = host.borrow().interface_protos.get("HTMLLabelElement") {
        let h1 = Rc::clone(host);
        let g = vm.alloc_host_fn("get htmlFor", move |vm, t, _| {
            let nid = require_node(vm, &h1, t)?;
            let val = h1
                .borrow()
                .document
                .borrow()
                .get(nid)
                .and_then(|d| match d {
                    NodeData::Element(e) => e.get_attribute("for").map(str::to_string),
                    _ => None,
                })
                .unwrap_or_default();
            Ok(JsValue::from_string(vm.heap.alloc_string(val)))
        });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn("set htmlFor", move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            let val = vm.value_to_rust_string(arg(args, 0));
            set_element_attr(vm, &h2, nid, "for", &val);
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, label_proto, "htmlFor", Some(g), Some(s));
    }

    // `popover` enumerated attribute (HTML5 Popover API: "auto" | "manual" | null).
    let h1 = Rc::clone(host);
    let g = vm.alloc_host_fn("get popover", move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        let val = {
            let doc_rc = Rc::clone(&h1.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(nid) {
                Some(NodeData::Element(e)) => e.get_attribute("popover").map(|raw| {
                    if raw.eq_ignore_ascii_case("manual") {
                        "manual".to_string()
                    } else {
                        "auto".to_string()
                    }
                }),
                _ => None,
            }
        };
        Ok(match val {
            Some(s) => JsValue::from_string(vm.heap.alloc_string(s)),
            None => JsValue::NULL,
        })
    });
    let h2 = Rc::clone(host);
    let s = vm.alloc_host_fn("set popover", move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let a = arg(args, 0);
        if a.is_null() {
            remove_element_attr(vm, &h2, nid, "popover");
        } else {
            let v = vm.value_to_rust_string(a);
            set_element_attr(vm, &h2, nid, "popover", &v);
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, ep, "popover", Some(g), Some(s));

    // `showPopover()`, `hidePopover()`, `togglePopover(force?)`
    let h = Rc::clone(host);
    define_host_method(vm, ep, "showPopover", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let is_popover = h
            .borrow()
            .document
            .borrow()
            .get(nid)
            .is_some_and(|d| matches!(d, NodeData::Element(e) if e.has_attribute("popover")));
        if !is_popover {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "NotSupportedError",
                "Element does not have popover attribute",
            ));
        }
        let mut hs = h.borrow_mut();
        if !hs.open_popovers.contains(&nid) {
            hs.open_popovers.push(nid);
        }
        Ok(JsValue::UNDEFINED)
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "hidePopover", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        h.borrow_mut().open_popovers.retain(|&p| p != nid);
        Ok(JsValue::UNDEFINED)
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "togglePopover", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let is_popover = h
            .borrow()
            .document
            .borrow()
            .get(nid)
            .is_some_and(|d| matches!(d, NodeData::Element(e) if e.has_attribute("popover")));
        if !is_popover {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "NotSupportedError",
                "Element does not have popover attribute",
            ));
        }
        let mut hs = h.borrow_mut();
        let was_open = hs.open_popovers.contains(&nid);
        let next = if args.first().is_some_and(|v| !v.is_undefined()) {
            vm.to_boolean(args[0])
        } else {
            !was_open
        };
        if next && !was_open {
            hs.open_popovers.push(nid);
        } else if !next && was_open {
            hs.open_popovers.retain(|&p| p != nid);
        }
        Ok(JsValue::from_bool(next))
    });

    // `tabIndex`
    let h1 = Rc::clone(host);
    let g = vm.alloc_host_fn("get tabIndex", move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        let idx = {
            let doc_rc = Rc::clone(&h1.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(nid) {
                Some(NodeData::Element(e)) => {
                    if let Some(raw) = e.get_attribute("tabindex") {
                        raw.trim().parse::<i32>().unwrap_or(-1)
                    } else if matches!(
                        e.name.local.as_ref(),
                        "a" | "button" | "input" | "select" | "textarea" | "summary"
                    ) {
                        0
                    } else {
                        -1
                    }
                }
                _ => -1,
            }
        };
        Ok(JsValue::from_i32(idx))
    });
    let h2 = Rc::clone(host);
    let s = vm.alloc_host_fn("set tabIndex", move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let v = vm.to_i32(arg(args, 0))?;
        set_element_attr(vm, &h2, nid, "tabindex", &v.to_string());
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, ep, "tabIndex", Some(g), Some(s));

    // `<input>` / `<button>` / `<select>` / `<textarea>` `type`, `value`, `defaultValue`
    let h1 = Rc::clone(host);
    let g = vm.alloc_host_fn("get type", move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        let ty = {
            let doc_rc = Rc::clone(&h1.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(nid) {
                Some(NodeData::Element(e)) => match e.name.local.as_ref() {
                    "input" => e
                        .get_attribute("type")
                        .unwrap_or("text")
                        .trim()
                        .to_ascii_lowercase(),
                    "button" => e
                        .get_attribute("type")
                        .unwrap_or("submit")
                        .trim()
                        .to_ascii_lowercase(),
                    "textarea" => "textarea".to_string(),
                    "select" => {
                        if e.has_attribute("multiple") {
                            "select-multiple".to_string()
                        } else {
                            "select-one".to_string()
                        }
                    }
                    _ => e.get_attribute("type").unwrap_or("").to_string(),
                },
                _ => String::new(),
            }
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(ty)))
    });
    let h2 = Rc::clone(host);
    let s = vm.alloc_host_fn("set type", move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let v = vm.value_to_rust_string(arg(args, 0));
        set_element_attr(vm, &h2, nid, "type", &v);
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, ep, "type", Some(g), Some(s));

    let h1 = Rc::clone(host);
    let g = vm.alloc_host_fn("get value", move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        let val = {
            let doc_rc = Rc::clone(&h1.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(nid) {
                Some(NodeData::Element(e)) => match e.name.local.as_ref() {
                    "textarea" => e
                        .get_attribute("value")
                        .map(str::to_string)
                        .unwrap_or_else(|| doc.inner_text(nid)),
                    "option" => e
                        .get_attribute("value")
                        .map(str::to_string)
                        .unwrap_or_else(|| doc.inner_text(nid)),
                    "select" => {
                        let opts: Vec<NodeId> = doc
                            .descendants(nid)
                            .filter(|&n| {
                                matches!(doc.get(n), Some(NodeData::Element(o)) if &*o.name.local == "option")
                            })
                            .collect();
                        let chosen = opts
                            .iter()
                            .copied()
                            .find(|&n| {
                                matches!(doc.get(n), Some(NodeData::Element(o)) if o.has_attribute("selected"))
                            })
                            .or_else(|| opts.first().copied());
                        chosen
                            .and_then(|opt| match doc.get(opt) {
                                Some(NodeData::Element(o)) => Some(
                                    o.get_attribute("value")
                                        .map(str::to_string)
                                        .unwrap_or_else(|| doc.inner_text(opt)),
                                ),
                                _ => None,
                            })
                            .unwrap_or_default()
                    }
                    _ => e.get_attribute("value").unwrap_or("").to_string(),
                },
                _ => String::new(),
            }
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(val)))
    });
    let h2 = Rc::clone(host);
    let s = vm.alloc_host_fn("set value", move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let val = vm.value_to_rust_string(arg(args, 0));
        set_element_attr(vm, &h2, nid, "value", &val);
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, ep, "value", Some(g), Some(s));

    // `<dialog>` `returnValue`, `show()`, `showModal()`, `close(returnValue?)`
    let h1 = Rc::clone(host);
    let g = vm.alloc_host_fn("get returnValue", move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        let rv = h1
            .borrow()
            .dialog_return_values
            .get(&nid)
            .cloned()
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(rv)))
    });
    let h2 = Rc::clone(host);
    let s = vm.alloc_host_fn("set returnValue", move |vm, t, args| {
        let nid = require_node(vm, &h2, t)?;
        let rv = vm.value_to_rust_string(arg(args, 0));
        h2.borrow_mut().dialog_return_values.insert(nid, rv);
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, ep, "returnValue", Some(g), Some(s));

    let h = Rc::clone(host);
    define_host_method(vm, ep, "show", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        set_element_attr(vm, &h, nid, "open", "");
        Ok(JsValue::UNDEFINED)
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "showModal", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let is_open = h
            .borrow()
            .document
            .borrow()
            .get(nid)
            .is_some_and(|d| matches!(d, NodeData::Element(e) if e.has_attribute("open")));
        if is_open {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "InvalidStateError",
                "Dialog is already open",
            ));
        }
        set_element_attr(vm, &h, nid, "open", "");
        {
            let doc_rc = Rc::clone(&h.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            if let Some(NodeData::Element(e)) = doc.get_mut(nid) {
                e.pseudo_state.insert(rix_dom::ElementState::MODAL);
            }
        }
        h.borrow_mut().mark_node_style_dirty(nid);
        Ok(JsValue::UNDEFINED)
    });
    let h = Rc::clone(host);
    define_host_method(vm, ep, "close", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        if let Some(&rv) = args.first() {
            if !rv.is_undefined() {
                let s = vm.value_to_rust_string(rv);
                h.borrow_mut().dialog_return_values.insert(nid, s);
            }
        }
        {
            let doc_rc = Rc::clone(&h.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            if let Some(NodeData::Element(e)) = doc.get_mut(nid) {
                e.pseudo_state.remove(rix_dom::ElementState::MODAL);
            }
        }
        remove_element_attr(vm, &h, nid, "open");
        h.borrow_mut().mark_node_style_dirty(nid);
        Ok(JsValue::UNDEFINED)
    });

    // HTML5 Constraint Validation API (`validity`, `validationMessage`, `willValidate`,
    // `setCustomValidity`, `checkValidity`, `reportValidity`) and `<form>` (`elements`, `reset`).
    let h = Rc::clone(host);
    define_host_method(vm, ep, "setCustomValidity", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let msg = vm.value_to_rust_string(arg(args, 0));
        if msg.is_empty() {
            h.borrow_mut().custom_validity.remove(&nid);
        } else {
            h.borrow_mut().custom_validity.insert(nid, msg);
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get validity", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let (valid, value_missing, custom_error, _msg) = compute_element_validity(&h, nid);
        let obj = vm.alloc_ordinary_object();
        for (k, v) in [
            ("valid", valid),
            ("valueMissing", value_missing),
            ("typeMismatch", false),
            ("patternMismatch", false),
            ("tooLong", false),
            ("tooShort", false),
            ("rangeUnderflow", false),
            ("rangeOverflow", false),
            ("stepMismatch", false),
            ("badInput", false),
            ("customError", custom_error),
        ] {
            let key = vm.heap.intern_key(k);
            vm.heap.object_define_data(
                obj,
                key,
                JsValue::from_bool(v),
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
        }
        Ok(JsValue::from_object(obj))
    });
    define_host_accessor(vm, ep, "validity", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get validationMessage", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let (_, _, _, msg) = compute_element_validity(&h, nid);
        Ok(JsValue::from_string(vm.heap.alloc_string(msg)))
    });
    define_host_accessor(vm, ep, "validationMessage", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get willValidate", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let will = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(nid) {
                Some(NodeData::Element(e)) => {
                    let is_ctrl = matches!(
                        e.name.local.as_ref(),
                        "input" | "select" | "textarea" | "button"
                    );
                    let is_hidden_input = &*e.name.local == "input"
                        && e.get_attribute("type")
                            .is_some_and(|t| t.eq_ignore_ascii_case("hidden"));
                    is_ctrl
                        && !is_hidden_input
                        && !e.has_attribute("disabled")
                        && !e.has_attribute("readonly")
                }
                _ => false,
            }
        };
        Ok(JsValue::from_bool(will))
    });
    define_host_accessor(vm, ep, "willValidate", Some(g), None);

    for mname in ["checkValidity", "reportValidity"] {
        let h = Rc::clone(host);
        define_host_method(vm, ep, mname, move |vm, t, _| {
            let nid = require_node(vm, &h, t)?;
            let is_form = h
                .borrow()
                .document
                .borrow()
                .get(nid)
                .is_some_and(|d| matches!(d, NodeData::Element(e) if &*e.name.local == "form"));
            if is_form {
                let controls: Vec<NodeId> = {
                    let doc_rc = Rc::clone(&h.borrow().document);
                    let doc = doc_rc.borrow();
                    doc.descendants(nid)
                        .filter(|&n| {
                            matches!(
                                doc.get(n),
                                Some(NodeData::Element(e))
                                    if matches!(e.name.local.as_ref(), "input" | "select" | "textarea")
                            )
                        })
                        .collect()
                };
                for c in controls {
                    if !compute_element_validity(&h, c).0 {
                        return Ok(JsValue::FALSE);
                    }
                }
                Ok(JsValue::TRUE)
            } else {
                Ok(JsValue::from_bool(compute_element_validity(&h, nid).0))
            }
        });
    }

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get form", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let form_id = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            let mut cur = doc.parent(nid);
            let mut found = None;
            while let Some(p) = cur {
                if matches!(doc.get(p), Some(NodeData::Element(e)) if &*e.name.local == "form") {
                    found = Some(p);
                    break;
                }
                cur = doc.parent(p);
            }
            found
        };
        Ok(form_id.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, ep, "form", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get elements", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let list: Vec<NodeId> = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            doc.descendants(nid)
                .filter(|&n| {
                    n != nid
                        && matches!(
                            doc.get(n),
                            Some(NodeData::Element(e))
                                if matches!(
                                    e.name.local.as_ref(),
                                    "button" | "fieldset" | "input" | "object" | "output" | "select" | "textarea"
                                )
                        )
                })
                .collect()
        };
        Ok(wrap_node_list(vm, &h, list))
    });
    define_host_accessor(vm, ep, "elements", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get options", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let list: Vec<NodeId> = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            doc.descendants(nid)
                .filter(|&n| {
                    n != nid
                        && matches!(doc.get(n), Some(NodeData::Element(e)) if &*e.name.local == "option")
                })
                .collect()
        };
        Ok(wrap_node_list(vm, &h, list))
    });
    define_host_accessor(vm, ep, "options", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get selectedIndex", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let idx = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            let opts: Vec<NodeId> = doc
                .descendants(nid)
                .filter(|&n| {
                    matches!(doc.get(n), Some(NodeData::Element(e)) if &*e.name.local == "option")
                })
                .collect();
            if opts.is_empty() {
                -1
            } else {
                opts.iter()
                    .position(|&n| {
                        matches!(doc.get(n), Some(NodeData::Element(e)) if e.has_attribute("selected"))
                    })
                    .unwrap_or(0) as i32
            }
        };
        Ok(JsValue::from_i32(idx))
    });
    define_host_accessor(vm, ep, "selectedIndex", Some(g), None);
}

fn compute_element_validity(host: &SharedHost, nid: NodeId) -> (bool, bool, bool, String) {
    let custom = host
        .borrow()
        .custom_validity
        .get(&nid)
        .cloned()
        .unwrap_or_default();
    if !custom.is_empty() {
        return (false, false, true, custom);
    }
    let doc_rc = Rc::clone(&host.borrow().document);
    let doc = doc_rc.borrow();
    if let Some(NodeData::Element(e)) = doc.get(nid) {
        if !e.has_attribute("disabled") && e.has_attribute("required") {
            let empty = match e.name.local.as_ref() {
                "input" => {
                    let ty = e.get_attribute("type").unwrap_or("text");
                    if ty.eq_ignore_ascii_case("checkbox") || ty.eq_ignore_ascii_case("radio") {
                        !e.has_attribute("checked")
                    } else {
                        e.get_attribute("value").unwrap_or("").is_empty()
                    }
                }
                "textarea" => e
                    .get_attribute("value")
                    .map(|s| s.is_empty())
                    .unwrap_or_else(|| doc.inner_text(nid).is_empty()),
                _ => false,
            };
            if empty {
                return (
                    false,
                    true,
                    false,
                    "Please fill out this field.".to_string(),
                );
            }
        }
    }
    (true, false, false, String::new())
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
    host.borrow_mut().invalidate_dom_mutation(nid, None);
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

fn insert_adjacent_html(vm: &mut JsVm, host: &SharedHost, nid: NodeId, pos: &str, html: &str) {
    let ctx_name = match host.borrow().document.borrow().get(nid) {
        Some(NodeData::Element(e)) => Some(e.name.clone()),
        _ => None,
    };
    let (frag_doc, frag_roots) = parse_html_fragment(html, ctx_name.as_ref());
    if frag_roots.is_empty() {
        return;
    }
    let (parent_target, added, is_conn) = {
        let _doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = _doc_rc.borrow_mut();
        let mut added = Vec::with_capacity(frag_roots.len());
        match pos {
            "afterbegin" => {
                let first = doc.first_child(nid);
                for src_root in frag_roots {
                    let cloned = doc.clone_subtree_from(&frag_doc, src_root);
                    if let Some(ref_node) = first {
                        doc.insert_before(ref_node, cloned);
                    } else {
                        doc.append_child(nid, cloned);
                    }
                    added.push(cloned);
                }
                (nid, added, doc.is_connected(nid))
            }
            "beforeend" => {
                for src_root in frag_roots {
                    let cloned = doc.clone_subtree_from(&frag_doc, src_root);
                    doc.append_child(nid, cloned);
                    added.push(cloned);
                }
                (nid, added, doc.is_connected(nid))
            }
            "beforebegin" => {
                let Some(parent) = doc.parent(nid) else {
                    return;
                };
                for src_root in frag_roots {
                    let cloned = doc.clone_subtree_from(&frag_doc, src_root);
                    doc.insert_before(nid, cloned);
                    added.push(cloned);
                }
                (parent, added, doc.is_connected(parent))
            }
            "afterend" => {
                let Some(parent) = doc.parent(nid) else {
                    return;
                };
                let next_sib = doc.next_sibling(nid);
                for src_root in frag_roots {
                    let cloned = doc.clone_subtree_from(&frag_doc, src_root);
                    if let Some(ref_node) = next_sib {
                        doc.insert_before(ref_node, cloned);
                    } else {
                        doc.append_child(parent, cloned);
                    }
                    added.push(cloned);
                }
                (parent, added, doc.is_connected(parent))
            }
            _ => return,
        }
    };
    host.borrow_mut()
        .invalidate_dom_mutation(parent_target, None);
    host.borrow().emit_mutation(DomMutation::ChildList {
        target: parent_target,
        added: added.clone(),
        removed: Vec::new(),
        previous_sibling: None,
        next_sibling: None,
    });
    if is_conn {
        for n in added {
            notify_custom_element_subtree(vm, host, n, true);
        }
    }
}

fn fnv_mix(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h ^ 0xff
}

// ---------------------------------------------------------------------------
// Document.prototype & ShadowRoot.prototype
// ---------------------------------------------------------------------------

fn install_document_prototype(vm: &mut JsVm, host: &SharedHost) {
    let dp = host.borrow().prototypes.document_proto;

    // documentElement
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get documentElement", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let el = h.borrow().document.borrow().document_element_of(doc_id);
        Ok(el.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, dp, "documentElement", Some(g), None);

    // children / childElementCount / firstElementChild / lastElementChild (ParentNode mixin)
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get children", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        Ok(crate::dom_collections::create_live_html_collection(
            vm,
            &h,
            crate::dom_interfaces::HtmlCollectionKind::Children(doc_id),
        ))
    });
    define_host_accessor(vm, dp, "children", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get childElementCount", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let cnt = h.borrow().document.borrow().child_element_count(doc_id);
        Ok(JsValue::from_f64(cnt as f64))
    });
    define_host_accessor(vm, dp, "childElementCount", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get firstElementChild", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let c = h.borrow().document.borrow().first_element_child(doc_id);
        Ok(c.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, dp, "firstElementChild", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get lastElementChild", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let c = h.borrow().document.borrow().last_element_child(doc_id);
        Ok(c.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, dp, "lastElementChild", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get scrollingElement", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let el = h.borrow().document.borrow().document_element_of(doc_id);
        Ok(el.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, dp, "scrollingElement", Some(g), None);

    // Document.prototype.defaultView & Document.prototype.location (HTML §7.2):
    // non-browsing-context documents (createDocument, createHTMLDocument, new Document, DOMParser)
    // MUST return `null` for both `defaultView` and `location`.
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get defaultView", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let main_root = h.borrow().document.borrow().root();
        if doc_id == main_root {
            return Ok(JsValue::from_object(vm.global));
        }
        let dv = h
            .borrow()
            .doc_meta
            .get(&doc_id)
            .and_then(|m| m.default_view);
        Ok(dv.map_or(JsValue::NULL, JsValue::from_object))
    });
    define_host_accessor(vm, dp, "defaultView", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get location", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let main_root = h.borrow().document.borrow().root();
        if doc_id == main_root {
            let kl = vm.heap.intern_key("location");
            return vm.get_property_by_key(vm.global, kl).map(|v| {
                if v.is_undefined() {
                    JsValue::NULL
                } else {
                    v
                }
            });
        }
        let dv = h
            .borrow()
            .doc_meta
            .get(&doc_id)
            .and_then(|m| m.default_view);
        if let Some(proxy) = dv {
            let kl = vm.heap.intern_key("location");
            return vm.get_property_by_key(proxy, kl).map(|v| {
                if v.is_undefined() {
                    JsValue::NULL
                } else {
                    v
                }
            });
        }
        Ok(JsValue::NULL)
    });
    define_host_accessor(vm, dp, "location", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get head", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let el = find_elements_by_tag(&h, doc_id, "head").into_iter().next();
        Ok(el.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, dp, "head", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get body", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let el = find_elements_by_tag(&h, doc_id, "body").into_iter().next();
        Ok(el.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, dp, "body", Some(g), None);

    let h1 = Rc::clone(host);
    let g = vm.alloc_host_fn("get title", move |vm, t, _| {
        let doc_id = require_node(vm, &h1, t)?;
        let title = {
            let _doc_rc = Rc::clone(&h1.borrow().document);
            let doc = _doc_rc.borrow();
            doc.document_title(doc_id).trim().to_string()
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(title)))
    });
    let h2 = Rc::clone(host);
    let s = vm.alloc_host_fn("set title", move |vm, t, args| {
        let doc_id = require_node(vm, &h2, t)?;
        let new_title = vm.value_to_rust_string(arg(args, 0));
        let _doc_rc = Rc::clone(&h2.borrow().document);
        let mut doc = _doc_rc.borrow_mut();
        if let Some(title_el) = doc.document_title_element(doc_id) {
            let kids: Vec<NodeId> = doc.children(title_el).collect();
            for c in kids {
                doc.remove_child(c);
            }
            if !new_title.is_empty() {
                let tn = doc.create_text(&new_title);
                doc.append_child(title_el, tn);
            }
        } else {
            let head_opt = doc.descendants(doc_id).find(|&n| {
                matches!(
                    doc.get(n),
                    Some(NodeData::Element(e)) if &*e.name.ns == crate::dom_interfaces::HTML_NS && &*e.name.local == "head"
                )
            });
            if let Some(head) = head_opt {
                let tn = doc.create_element("title");
                if !new_title.is_empty() {
                    let text_node = doc.create_text(&new_title);
                    doc.append_child(tn, text_node);
                }
                doc.append_child(head, tn);
            }
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, dp, "title", Some(g), Some(s));

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get readyState", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let rs = {
            let hb = h.borrow();
            if doc_id == hb.document.borrow().root() {
                hb.ready_state.clone()
            } else {
                hb.doc_meta
                    .get(&doc_id)
                    .map(|m| m.ready_state.clone())
                    .unwrap_or_else(|| "complete".to_string())
            }
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(rs)))
    });
    define_host_accessor(vm, dp, "readyState", Some(g), None);

    // document.characterSet / charset / inputEncoding / contentType / URL / documentURI / compatMode
    for prop in ["characterSet", "charset", "inputEncoding"] {
        let h = Rc::clone(host);
        let g = vm.alloc_host_fn(prop, move |vm, t, _| {
            let doc_id = require_node(vm, &h, t)?;
            let hb = h.borrow();
            let cs = hb
                .doc_meta
                .get(&doc_id)
                .map(|m| m.character_set.clone())
                .unwrap_or_else(|| hb.document.borrow().character_set().to_string());
            Ok(JsValue::from_string(vm.heap.alloc_string(cs)))
        });
        define_host_accessor(vm, dp, prop, Some(g), None);
    }

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get contentType", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let ct = h
            .borrow()
            .doc_meta
            .get(&doc_id)
            .map(|m| m.content_type.clone())
            .unwrap_or_else(|| "text/html".to_string());
        Ok(JsValue::from_string(vm.heap.alloc_string(ct)))
    });
    define_host_accessor(vm, dp, "contentType", Some(g), None);

    for prop in ["URL", "documentURI"] {
        let h = Rc::clone(host);
        let g = vm.alloc_host_fn(prop, move |vm, t, _| {
            let doc_id = require_node(vm, &h, t)?;
            let url = h.borrow().doc_url_for_node(doc_id);
            Ok(JsValue::from_string(vm.heap.alloc_string(url)))
        });
        define_host_accessor(vm, dp, prop, Some(g), None);
    }

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get compatMode", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let hb = h.borrow();
        let cm = hb
            .doc_meta
            .get(&doc_id)
            .map(|m| {
                if m.quirks_mode == 2 {
                    "BackCompat"
                } else {
                    "CSS1Compat"
                }
            })
            .unwrap_or_else(|| hb.document.borrow().compat_mode());
        Ok(JsValue::from_string(vm.heap.alloc_string(cm.to_string())))
    });
    define_host_accessor(vm, dp, "compatMode", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get currentScript", move |vm, _, _| {
        let cs = h.borrow().current_script;
        Ok(cs.map_or(JsValue::NULL, |nid| wrap_node(vm, &h, nid)))
    });
    define_host_accessor(vm, dp, "currentScript", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get doctype", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let dt_id = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            let found = doc
                .children(doc_id)
                .find(|&n| matches!(doc.get(n), Some(NodeData::DocumentType { .. })));
            found
        };
        Ok(dt_id.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, dp, "doctype", Some(g), None);

    let h1 = Rc::clone(host);
    let g = vm.alloc_host_fn("get dir", move |vm, t, _| {
        let doc_id = require_node(vm, &h1, t)?;
        let dir = {
            let doc_rc = Rc::clone(&h1.borrow().document);
            let doc = doc_rc.borrow();
            doc.document_element_of(doc_id)
                .and_then(|de| match doc.get(de) {
                    Some(NodeData::Element(e)) => e.get_attribute("dir").map(str::to_string),
                    _ => None,
                })
                .unwrap_or_default()
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(dir)))
    });
    let h2 = Rc::clone(host);
    let s = vm.alloc_host_fn("set dir", move |vm, t, args| {
        let doc_id = require_node(vm, &h2, t)?;
        let next = vm.value_to_rust_string(arg(args, 0));
        let de = h2.borrow().document.borrow().document_element_of(doc_id);
        if let Some(de) = de {
            set_element_attr(vm, &h2, de, "dir", &next);
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, dp, "dir", Some(g), Some(s));

    // Node factory methods (WHATWG DOM §4.5, per-document HTML vs XML semantics)
    let h = Rc::clone(host);
    define_host_method(vm, dp, "createElement", move |vm, t, args| {
        if args.is_empty() {
            return Err(vm.type_error("createElement requires at least 1 argument"));
        }
        let doc_id = require_node(vm, &h, t)?;
        let raw_tag = vm.value_to_rust_string(args[0]);
        if !crate::dom_interfaces::is_valid_element_local_name(&raw_tag) {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "InvalidCharacterError",
                "The tag name provided is not a valid element name",
            ));
        }
        let (is_xml, content_type) = {
            let hb = h.borrow();
            match hb.doc_meta.get(&doc_id) {
                Some(m) => (m.is_xml, m.content_type.clone()),
                None => (false, "text/html".to_string()),
            }
        };
        let local = if is_xml {
            raw_tag
        } else {
            raw_tag.to_ascii_lowercase()
        };
        let ns = if !is_xml || content_type == "application/xhtml+xml" {
            crate::dom_interfaces::HTML_NS
        } else {
            ""
        };
        let is_opt: Option<String> = arg(args, 1).as_object().and_then(|oh| {
            let k_is = vm.heap.intern_key("is");
            vm.get_property_by_key(oh, k_is).ok().and_then(|v| {
                if v.is_undefined() {
                    None
                } else {
                    Some(vm.value_to_rust_string(v))
                }
            })
        });
        let nid = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            let n = doc.create_element_ns(ns, None, &local);
            if let Some(ref is_val) = is_opt {
                if let Some(NodeData::Element(el)) = doc.get_mut(n) {
                    el.set_attribute("is", is_val);
                }
            }
            n
        };
        h.borrow_mut().set_owner_doc(nid, doc_id);
        let wrapper = wrap_node(vm, &h, nid);
        if ns == crate::dom_interfaces::HTML_NS {
            let ce_key = is_opt.as_deref().unwrap_or(&local);
            upgrade_if_custom_element(vm, &h, nid, ce_key, &local);
        }
        Ok(wrapper)
    });

    let h = Rc::clone(host);
    define_host_method(vm, dp, "createElementNS", move |vm, t, args| {
        if args.len() < 2 {
            return Err(vm.type_error("createElementNS requires 2 arguments"));
        }
        let doc_id = require_node(vm, &h, t)?;
        let ns_arg = args[0];
        let ns = if ns_arg.is_null() || ns_arg.is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(ns_arg)
        };
        let raw_qname = vm.value_to_rust_string(args[1]);
        let (ns, prefix, local) =
            crate::dom_interfaces::validate_and_extract(vm, &ns, &raw_qname, true)?;
        let nid =
            h.borrow()
                .document
                .borrow_mut()
                .create_element_ns(&ns, prefix.as_deref(), &local);
        h.borrow_mut().set_owner_doc(nid, doc_id);
        let wrapper = wrap_node(vm, &h, nid);
        if ns == crate::dom_interfaces::HTML_NS {
            upgrade_if_custom_element(vm, &h, nid, &local, &local);
        }
        Ok(wrapper)
    });

    let h = Rc::clone(host);
    define_host_method(vm, dp, "createTextNode", move |vm, t, args| {
        let doc_id = require_node(vm, &h, t)?;
        let text = vm.value_to_rust_string(arg(args, 0));
        let nid = h.borrow().document.borrow_mut().create_text(&text);
        h.borrow_mut().set_owner_doc(nid, doc_id);
        Ok(wrap_node(vm, &h, nid))
    });

    let h = Rc::clone(host);
    define_host_method(vm, dp, "createCDATASection", move |vm, t, args| {
        if args.is_empty() {
            return Err(vm.type_error("createCDATASection requires 1 argument"));
        }
        let doc_id = require_node(vm, &h, t)?;
        if !h.borrow().is_xml_doc(doc_id) {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "NotSupportedError",
                "CDATASection nodes are only supported in XML documents",
            ));
        }
        let data = vm.value_to_rust_string(args[0]);
        if data.contains("]]>") {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "InvalidCharacterError",
                "CDATASection data cannot contain ']]>'",
            ));
        }
        let nid = h.borrow().document.borrow_mut().create_cdata_section(&data);
        h.borrow_mut().set_owner_doc(nid, doc_id);
        Ok(wrap_node(vm, &h, nid))
    });

    let h = Rc::clone(host);
    define_host_method(vm, dp, "createComment", move |vm, t, args| {
        let doc_id = require_node(vm, &h, t)?;
        let text = vm.value_to_rust_string(arg(args, 0));
        let nid = h.borrow().document.borrow_mut().create_comment(&text);
        h.borrow_mut().set_owner_doc(nid, doc_id);
        Ok(wrap_node(vm, &h, nid))
    });

    let h = Rc::clone(host);
    define_host_method(vm, dp, "createProcessingInstruction", move |vm, t, args| {
        if args.len() < 2 {
            return Err(vm.type_error("createProcessingInstruction requires 2 arguments"));
        }
        let doc_id = require_node(vm, &h, t)?;
        let target = vm.value_to_rust_string(args[0]);
        let data = vm.value_to_rust_string(args[1]);
        if !crate::dom_interfaces::is_valid_xml_name(&target) {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "InvalidCharacterError",
                "ProcessingInstruction target is not a valid XML Name",
            ));
        }
        if data.contains("?>") {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "InvalidCharacterError",
                "ProcessingInstruction data cannot contain '?>'",
            ));
        }
        let nid = h
            .borrow()
            .document
            .borrow_mut()
            .create_processing_instruction(&target, &data);
        h.borrow_mut().set_owner_doc(nid, doc_id);
        Ok(wrap_node(vm, &h, nid))
    });

    let h = Rc::clone(host);
    define_host_method(vm, dp, "createDocumentFragment", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let nid = h
            .borrow()
            .document
            .borrow_mut()
            .new_node(NodeData::DocumentFragment);
        h.borrow_mut().set_owner_doc(nid, doc_id);
        Ok(wrap_node(vm, &h, nid))
    });

    let h = Rc::clone(host);
    define_host_method(vm, dp, "createAttribute", move |vm, t, args| {
        if args.is_empty() {
            return Err(vm.type_error("createAttribute requires 1 argument"));
        }
        let doc_id = require_node(vm, &h, t)?;
        let raw_name = vm.value_to_rust_string(args[0]);
        if !crate::dom_interfaces::is_valid_attribute_local_name(&raw_name) {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "InvalidCharacterError",
                "The attribute name contains an invalid character",
            ));
        }
        let local = if h.borrow().is_xml_doc(doc_id) {
            raw_name
        } else {
            raw_name.to_ascii_lowercase()
        };
        let ah = crate::dom_collections::alloc_attr_object(
            vm,
            &h,
            crate::dom_interfaces::AttrEntry {
                owner_element: None,
                owner_doc: doc_id,
                namespace: String::new(),
                prefix: None,
                local_name: local,
                detached_value: String::new(),
            },
        );
        Ok(JsValue::from_object(ah))
    });

    let h = Rc::clone(host);
    define_host_method(vm, dp, "createAttributeNS", move |vm, t, args| {
        if args.len() < 2 {
            return Err(vm.type_error("createAttributeNS requires 2 arguments"));
        }
        let doc_id = require_node(vm, &h, t)?;
        let ns_arg = args[0];
        let ns = if ns_arg.is_null() || ns_arg.is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(ns_arg)
        };
        let qname = vm.value_to_rust_string(args[1]);
        let (ns, prefix, local) =
            crate::dom_interfaces::validate_and_extract(vm, &ns, &qname, false)?;
        let ah = crate::dom_collections::alloc_attr_object(
            vm,
            &h,
            crate::dom_interfaces::AttrEntry {
                owner_element: None,
                owner_doc: doc_id,
                namespace: ns,
                prefix,
                local_name: local,
                detached_value: String::new(),
            },
        );
        Ok(JsValue::from_object(ah))
    });

    // adoptNode(node) / importNode(node, deep = false)
    let h = Rc::clone(host);
    define_host_method(vm, dp, "adoptNode", move |vm, t, args| {
        let doc_id = require_node(vm, &h, t)?;
        let node_val = arg(args, 0);
        if let Some(ah) = node_val.as_object() {
            if h.borrow().attrs.contains_key(&ah) {
                let owner_el = h.borrow().attrs.get(&ah).and_then(|e| e.owner_element);
                if let Some(el) = owner_el {
                    let entry = h.borrow().attrs.get(&ah).cloned().unwrap();
                    let _ = crate::dom_collections::remove_attribute_node_from_element(
                        vm,
                        &h,
                        el,
                        &entry.namespace,
                        &entry.local_name,
                    );
                }
                if let Some(e) = h.borrow_mut().attrs.get_mut(&ah) {
                    e.owner_doc = doc_id;
                }
                return Ok(node_val);
            }
        }
        let nid = require_node(vm, &h, node_val)?;
        match h.borrow().document.borrow().get(nid) {
            Some(NodeData::Document) => {
                return Err(crate::web_apis::create_dom_exception(
                    vm,
                    "NotSupportedError",
                    "Document nodes cannot be adopted",
                ));
            }
            Some(NodeData::ShadowRoot { .. }) => {
                return Err(crate::web_apis::create_dom_exception(
                    vm,
                    "HierarchyRequestError",
                    "ShadowRoot nodes cannot be adopted",
                ));
            }
            _ => {}
        }
        let old_parent = h.borrow().document.borrow().parent(nid);
        if let Some(op) = old_parent {
            remove_node_from_parent(vm, &h, op, nid)?;
        }
        h.borrow_mut().adopt_subtree(nid, doc_id);
        Ok(node_val)
    });

    let h = Rc::clone(host);
    define_host_method(vm, dp, "importNode", move |vm, t, args| {
        let doc_id = require_node(vm, &h, t)?;
        let node_val = arg(args, 0);
        let deep = vm.to_boolean(arg(args, 1));
        if let Some(ah) = node_val.as_object() {
            let entry_opt = h.borrow().attrs.get(&ah).cloned();
            if let Some(entry) = entry_opt {
                let val = if let Some(el) = entry.owner_element {
                    h.borrow()
                        .document
                        .borrow()
                        .get(el)
                        .and_then(|d| match d {
                            NodeData::Element(e) => e
                                .get_attribute_ns(&entry.namespace, &entry.local_name)
                                .map(str::to_string),
                            _ => None,
                        })
                        .unwrap_or(entry.detached_value)
                } else {
                    entry.detached_value
                };
                let cloned = crate::dom_collections::alloc_attr_object(
                    vm,
                    &h,
                    crate::dom_interfaces::AttrEntry {
                        owner_element: None,
                        owner_doc: doc_id,
                        namespace: entry.namespace,
                        prefix: entry.prefix,
                        local_name: entry.local_name,
                        detached_value: val,
                    },
                );
                return Ok(JsValue::from_object(cloned));
            }
        }
        let nid = require_node(vm, &h, node_val)?;
        if matches!(
            h.borrow().document.borrow().get(nid),
            Some(NodeData::Document | NodeData::ShadowRoot { .. })
        ) {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "NotSupportedError",
                "Document or ShadowRoot nodes cannot be imported",
            ));
        }
        let cloned = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            if deep {
                let snap = doc.clone();
                doc.clone_subtree_from(&snap, nid)
            } else {
                let mut data = doc.get(nid).cloned().unwrap_or(NodeData::DocumentFragment);
                if let NodeData::Element(ref mut el) = data {
                    el.shadow_root = None;
                    el.content_document = None;
                    if el.template_contents.is_some() {
                        el.template_contents = Some(doc.new_node(NodeData::DocumentFragment));
                    }
                }
                doc.new_node(data)
            }
        };
        h.borrow_mut().adopt_subtree(cloned, doc_id);
        Ok(wrap_node(vm, &h, cloned))
    });

    // document.implementation (cached SameObject DOMImplementation per Document)
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get implementation", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let imp = crate::dom_interfaces::create_dom_implementation_obj(vm, &h, doc_id);
        Ok(JsValue::from_object(imp))
    });
    define_host_accessor(vm, dp, "implementation", Some(g), None);

    // Document.createTreeWalker & Document.createNodeIterator (WHATWG DOM §6)
    for (mname, is_iter) in [("createTreeWalker", false), ("createNodeIterator", true)] {
        let h = Rc::clone(host);
        define_host_method(vm, dp, mname, move |vm, _, args| {
            let root = require_node(vm, &h, arg(args, 0))?;
            let what_to_show = if args.get(1).is_some_and(|v| !v.is_undefined()) {
                let n = vm.to_number(args[1])?;
                if n.is_finite() {
                    n.trunc().rem_euclid(4_294_967_296.0) as u32
                } else {
                    0
                }
            } else {
                0xFFFF_FFFF
            };
            let filter_arg = arg(args, 2);
            let filter = if filter_arg.is_null() || filter_arg.is_undefined() {
                None
            } else if let Some(fh) = filter_arg.as_object() {
                Some(fh)
            } else {
                return Err(vm.type_error("NodeFilter must be a function, object, or null"));
            };
            let obj = crate::dom_collections::create_traversal_object(
                vm,
                &h,
                root,
                what_to_show,
                filter,
                is_iter,
            );
            Ok(JsValue::from_object(obj))
        });
    }

    // Document.activeElement
    let h = Rc::clone(host);
    let g_ae = vm.alloc_host_fn("get activeElement", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let active = h.borrow().active_element;
        if let Some(nid) = active {
            if h.borrow().document.borrow().is_connected(nid)
                && h.borrow().owner_doc_of(nid) == doc_id
            {
                return Ok(wrap_node(vm, &h, nid));
            }
        }
        let body = find_elements_by_tag(&h, doc_id, "body").into_iter().next();
        Ok(body.map_or(JsValue::NULL, |b| wrap_node(vm, &h, b)))
    });
    define_host_accessor(vm, dp, "activeElement", Some(g_ae), None);

    // HTML §3.1.3 Document tree accessors (`images`, `embeds`, `plugins`, `links`,
    // `forms`, `scripts`, `applets`, `anchors`) — light tree only (never crossing into ShadowRoots).
    for (prop, tags) in [
        ("images", &["img"][..]),
        ("embeds", &["embed"][..]),
        ("plugins", &["embed"][..]),
        ("forms", &["form"][..]),
        ("scripts", &["script"][..]),
        ("applets", &[][..]),
        ("links", &["a", "area"][..]),
        ("anchors", &["a"][..]),
    ] {
        let h = Rc::clone(host);
        let prop_name = prop;
        let g = vm.alloc_host_fn(prop, move |vm, t, _| {
            let doc_id = require_node(vm, &h, t)?;
            let matched: Vec<NodeId> = {
                let doc_rc = Rc::clone(&h.borrow().document);
                let doc = doc_rc.borrow();
                doc.descendants(doc_id)
                    .filter(|&id| match doc.get(id) {
                        Some(NodeData::Element(e))
                            if &*e.name.ns == crate::dom_interfaces::HTML_NS
                                && tags.contains(&e.name.local.as_str()) =>
                        {
                            match prop_name {
                                "links" => e.has_attribute("href"),
                                "anchors" => e.has_attribute("name"),
                                _ => true,
                            }
                        }
                        _ => false,
                    })
                    .collect()
            };
            Ok(wrap_node_list(vm, &h, matched))
        });
        define_host_accessor(vm, dp, prop, Some(g), None);
    }

    install_parent_node_mixin(vm, host, dp);

    // Document queries (per-document!)
    let h = Rc::clone(host);
    define_host_method(vm, dp, "getElementById", move |vm, t, args| {
        let doc_id = require_node(vm, &h, t)?;
        let id = vm.value_to_rust_string(arg(args, 0));
        let found = h
            .borrow()
            .document
            .borrow()
            .get_element_by_id_in(doc_id, &id);
        Ok(found.map_or(JsValue::NULL, |nid| wrap_node(vm, &h, nid)))
    });
    let h = Rc::clone(host);
    define_host_method(vm, dp, "querySelector", move |vm, t, args| {
        let doc_id = require_node(vm, &h, t)?;
        let sel = vm.value_to_rust_string(arg(args, 0));
        let parsed = parse_selector_list_strict(&sel)
            .map_err(|e| crate::web_apis::create_dom_exception(vm, "SyntaxError", &e))?;
        let found = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let doc = _doc_rc.borrow();
            query_selector_in_subtree(&doc, doc_id, &parsed, None, None)
        };
        Ok(found.map_or(JsValue::NULL, |nid| wrap_node(vm, &h, nid)))
    });
    let h = Rc::clone(host);
    define_host_method(vm, dp, "querySelectorAll", move |vm, t, args| {
        let doc_id = require_node(vm, &h, t)?;
        let sel = vm.value_to_rust_string(arg(args, 0));
        let parsed = parse_selector_list_strict(&sel)
            .map_err(|e| crate::web_apis::create_dom_exception(vm, "SyntaxError", &e))?;
        let list = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let doc = _doc_rc.borrow();
            query_selector_all_in_subtree(&doc, doc_id, &parsed, None, None)
        };
        Ok(crate::dom_collections::create_static_node_list(
            vm, &h, list,
        ))
    });
    let h = Rc::clone(host);
    define_host_method(vm, dp, "getElementsByTagName", move |vm, t, args| {
        let doc_id = require_node(vm, &h, t)?;
        let tag = vm.value_to_rust_string(arg(args, 0));
        let is_html_doc = !h.borrow().is_xml_doc(doc_id);
        Ok(crate::dom_collections::create_live_html_collection(
            vm,
            &h,
            crate::dom_interfaces::HtmlCollectionKind::ByTagName {
                root: doc_id,
                qname: tag,
                is_html_doc,
            },
        ))
    });
    let h = Rc::clone(host);
    define_host_method(vm, dp, "getElementsByTagNameNS", move |vm, t, args| {
        let doc_id = require_node(vm, &h, t)?;
        let ns_arg = arg(args, 0);
        let ns = if ns_arg.is_null() || ns_arg.is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(ns_arg)
        };
        let local = vm.value_to_rust_string(arg(args, 1));
        Ok(crate::dom_collections::create_live_html_collection(
            vm,
            &h,
            crate::dom_interfaces::HtmlCollectionKind::ByTagNameNS {
                root: doc_id,
                ns,
                local,
            },
        ))
    });
    let h = Rc::clone(host);
    define_host_method(vm, dp, "getElementsByClassName", move |vm, t, args| {
        let doc_id = require_node(vm, &h, t)?;
        let cls = vm.value_to_rust_string(arg(args, 0));
        let classes: Vec<String> = cls
            .split(['\t', '\n', '\x0C', '\r', ' '])
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let quirks = h
            .borrow()
            .doc_meta
            .get(&doc_id)
            .map(|m| m.quirks_mode == 2)
            .unwrap_or_else(|| h.borrow().document.borrow().quirks_mode() == 2);
        Ok(crate::dom_collections::create_live_html_collection(
            vm,
            &h,
            crate::dom_interfaces::HtmlCollectionKind::ByClassName {
                root: doc_id,
                classes,
                quirks,
            },
        ))
    });

    // document.open() / document.close() / document.write(...) / document.writeln(...)
    fn open_doc_impl(h: &SharedHost, did: NodeId) {
        let main_root = h.borrow().document.borrow().root();
        let removed: Vec<NodeId> = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            let children: Vec<NodeId> = doc.children(did).collect();
            for &c in &children {
                doc.remove_child(c);
            }
            children
        };
        {
            let mut hb = h.borrow_mut();
            if did == main_root {
                hb.ready_state = "loading".to_string();
            } else {
                let url = hb.doc_url_for_node(did);
                let meta = hb
                    .doc_meta
                    .entry(did)
                    .or_insert_with(|| crate::dom_interfaces::DocumentMeta::html(url));
                meta.ready_state = "loading".to_string();
            }
            hb.invalidate_dom_mutation(did, None);
        }
        if !removed.is_empty() {
            h.borrow().emit_mutation(DomMutation::ChildList {
                target: did,
                added: Vec::new(),
                removed,
                previous_sibling: None,
                next_sibling: None,
            });
        }
    }
    fn write_to_doc_impl(h: &SharedHost, did: NodeId, html: &str) {
        let main_root = h.borrow().document.borrow().root();
        let parser_opt = if did == main_root {
            h.borrow().active_parser.clone()
        } else {
            None
        };
        if let Some(parser) = parser_opt {
            parser.borrow_mut().insert_document_write(html);
            return;
        }
        let is_loading = {
            let hb = h.borrow();
            if did == main_root {
                hb.ready_state == "loading"
            } else {
                hb.doc_meta
                    .get(&did)
                    .is_some_and(|m| m.ready_state == "loading")
            }
        };
        if !is_loading {
            open_doc_impl(h, did);
        }
        // Ensure `<html><head></head><body></body></html>` skeleton exists after implied `open()`.
        let target_body = {
            let existing_body = find_elements_by_tag(h, did, "body").into_iter().next();
            if let Some(b) = existing_body {
                b
            } else {
                let (html_el, body_el) = {
                    let doc_rc = Rc::clone(&h.borrow().document);
                    let mut doc = doc_rc.borrow_mut();
                    let html_el = doc.document_element_of(did).unwrap_or_else(|| {
                        let he =
                            doc.create_element_ns(crate::dom_interfaces::HTML_NS, None, "html");
                        doc.append_child(did, he);
                        he
                    });
                    let head_el =
                        doc.create_element_ns(crate::dom_interfaces::HTML_NS, None, "head");
                    let body_el =
                        doc.create_element_ns(crate::dom_interfaces::HTML_NS, None, "body");
                    doc.append_child(html_el, head_el);
                    doc.append_child(html_el, body_el);
                    (html_el, body_el)
                };
                h.borrow_mut().adopt_subtree(html_el, did);
                body_el
            }
        };
        let (frag_doc, roots) = parse_html_fragment(html, None);
        let mut added = Vec::new();
        {
            let doc_rc = Rc::clone(&h.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            for c in roots {
                let cloned = doc.clone_subtree_from(&frag_doc, c);
                doc.append_child(target_body, cloned);
                added.push(cloned);
            }
        }
        for &n in &added {
            h.borrow_mut().adopt_subtree(n, did);
        }
        h.borrow_mut().invalidate_dom_mutation(target_body, None);
    }
    {
        let h = Rc::clone(host);
        define_host_method(vm, dp, "open", move |vm, t, _| {
            let did = require_node(vm, &h, t)?;
            open_doc_impl(&h, did);
            Ok(t)
        });
    }
    {
        let h = Rc::clone(host);
        define_host_method(vm, dp, "close", move |vm, t, _| {
            let did = require_node(vm, &h, t)?;
            let main_root = h.borrow().document.borrow().root();
            let mut hb = h.borrow_mut();
            if did == main_root {
                if hb.ready_state == "loading" {
                    hb.ready_state = "interactive".to_string();
                }
            } else if let Some(meta) = hb.doc_meta.get_mut(&did) {
                if meta.ready_state == "loading" {
                    meta.ready_state = "interactive".to_string();
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let h = Rc::clone(host);
        define_host_method(vm, dp, "write", move |vm, t, args| {
            let mut html = String::new();
            for &a in args {
                html.push_str(&vm.value_to_rust_string(a));
            }
            if let Ok(did) = require_node(vm, &h, t) {
                write_to_doc_impl(&h, did, &html);
            }
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let h = Rc::clone(host);
        define_host_method(vm, dp, "writeln", move |vm, t, args| {
            let mut html = String::new();
            for &a in args {
                html.push_str(&vm.value_to_rust_string(a));
            }
            html.push('\n');
            if let Ok(did) = require_node(vm, &h, t) {
                write_to_doc_impl(&h, did, &html);
            }
            Ok(JsValue::UNDEFINED)
        });
    }
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

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get delegatesFocus", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let v = matches!(
            h.borrow().document.borrow().get(nid),
            Some(NodeData::ShadowRoot {
                delegates_focus: true,
                ..
            })
        );
        Ok(JsValue::from_bool(v))
    });
    define_host_accessor(vm, sp, "delegatesFocus", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get clonable", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let v = matches!(
            h.borrow().document.borrow().get(nid),
            Some(NodeData::ShadowRoot { clonable: true, .. })
        );
        Ok(JsValue::from_bool(v))
    });
    define_host_accessor(vm, sp, "clonable", Some(g), None);

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get serializable", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let v = matches!(
            h.borrow().document.borrow().get(nid),
            Some(NodeData::ShadowRoot {
                serializable: true,
                ..
            })
        );
        Ok(JsValue::from_bool(v))
    });
    define_host_accessor(vm, sp, "serializable", Some(g), None);

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
        let parsed = parse_selector_list_strict(&sel)
            .map_err(|e| crate::web_apis::create_dom_exception(vm, "SyntaxError", &e))?;
        let found = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            let shadow_host = match doc.get(nid) {
                Some(NodeData::ShadowRoot { host: he, .. }) => Some(*he),
                _ => None,
            };
            query_selector_in_subtree(&doc, nid, &parsed, None, shadow_host)
        };
        Ok(found.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    let h = Rc::clone(host);
    define_host_method(vm, sp, "querySelectorAll", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let sel = vm.value_to_rust_string(arg(args, 0));
        let parsed = parse_selector_list_strict(&sel)
            .map_err(|e| crate::web_apis::create_dom_exception(vm, "SyntaxError", &e))?;
        let list = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            let shadow_host = match doc.get(nid) {
                Some(NodeData::ShadowRoot { host: he, .. }) => Some(*he),
                _ => None,
            };
            query_selector_all_in_subtree(&doc, nid, &parsed, None, shadow_host)
        };
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
    // window.customElements (`CustomElementRegistry`)
    let ce_reg_proto = crate::dom_interfaces::make_proto(vm, vm.object_proto);
    let _ = crate::dom_interfaces::register_dom_interface(
        vm,
        "CustomElementRegistry",
        ce_reg_proto,
        None,
        false,
    );
    let ce_reg = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(ce_reg_proto),
    ));
    let h = Rc::clone(host);
    define_host_method(vm, ce_reg_proto, "define", move |vm, _, args| {
        let name = vm.value_to_rust_string(arg(args, 0));
        if !is_valid_custom_element_name(&name) {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "SyntaxError",
                &format!("'{name}' is not a valid custom element name"),
            ));
        }
        let Some(ctor) = arg(args, 1).as_object() else {
            return Err(vm.type_error("customElements.define requires a constructor function"));
        };
        if !vm.is_callable_handle(ctor) {
            return Err(vm.type_error("customElements.define requires a constructor function"));
        }
        if h.borrow().custom_elements.contains_key(&name) {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "NotSupportedError",
                &format!("Custom element '{name}' is already defined"),
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
        // Read optional `{ extends: "..." }` third argument for customized built-in elements.
        let extends_opt: Option<String> = arg(args, 2).as_object().and_then(|oh| {
            let k_ext = vm.heap.intern_key("extends");
            vm.get_property_by_key(oh, k_ext).ok().and_then(|v| {
                if v.is_undefined() {
                    None
                } else {
                    Some(vm.value_to_rust_string(v).to_ascii_lowercase())
                }
            })
        });
        let waiters = {
            let mut hs = h.borrow_mut();
            hs.custom_elements.insert(
                name.clone(),
                CustomElementDef {
                    constructor: ctor,
                    observed_attributes,
                    extends: extends_opt.clone(),
                },
            );
            hs.when_defined_waiters.remove(&name).unwrap_or_default()
        };
        for p in waiters {
            resolve_promise(vm, p, JsValue::from_object(ctor));
        }
        // Upgrade any already-connected matching elements in the document.
        let root = h.borrow().document.borrow().root();
        let search_tag = extends_opt.as_deref().unwrap_or(&name);
        let matching = find_elements_by_tag(&h, root, search_tag);
        for nid in matching {
            let matches_ce = if extends_opt.is_some() {
                let doc_rc = Rc::clone(&h.borrow().document);
                let doc = doc_rc.borrow();
                matches!(
                    doc.get(nid),
                    Some(NodeData::Element(e)) if e.get_attribute("is") == Some(name.as_str())
                )
            } else {
                true
            };
            if matches_ce {
                upgrade_if_custom_element(vm, &h, nid, &name, search_tag);
                invoke_custom_element_lifecycle(vm, &h, nid, "connectedCallback", &[]);
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, ce_reg_proto, "get", move |_vm, _, args| {
        let name = _vm.value_to_rust_string(arg(args, 0));
        Ok(h.borrow()
            .custom_elements
            .get(&name)
            .map_or(JsValue::UNDEFINED, |d| JsValue::from_object(d.constructor)))
    });

    let h = Rc::clone(host);
    define_host_method(vm, ce_reg_proto, "whenDefined", move |vm, _, args| {
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
}

/// Installs HTML Living Standard §7.3.3.1 named access on `Window` via a `WindowProperties`
/// Proxy sitting between `window` (`vm.global`) and `EventTarget.prototype`.
fn install_window_properties_proxy(
    vm: &mut JsVm,
    host: &SharedHost,
    event_target_proto: HeapHandle,
) {
    let target = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(event_target_proto),
    ));
    let handler = vm.alloc_ordinary_object();

    let h_has = Rc::clone(host);
    define_host_method(vm, handler, "has", move |vm, _, args| {
        let target_h = arg(args, 0).as_object();
        let prop_val = arg(args, 1);
        if prop_val.is_symbol() {
            if let Some(th) = target_h {
                let k_str = vm.to_property_key(prop_val)?;
                let key = vm.heap.intern_key(&k_str);
                return Ok(JsValue::from_bool(vm.has_property_by_key(th, key)?));
            }
            return Ok(JsValue::FALSE);
        }
        let name = vm.value_to_rust_string(prop_val);
        if let Some(th) = target_h {
            let k = vm.heap.intern_key(&name);
            if vm.has_property_by_key(th, k)? {
                return Ok(JsValue::TRUE);
            }
        }
        if name.is_empty() {
            return Ok(JsValue::FALSE);
        }
        let found = {
            let doc_rc = Rc::clone(&h_has.borrow().document);
            let doc = doc_rc.borrow();
            !find_window_named_elements(&doc, &name).is_empty()
        };
        Ok(JsValue::from_bool(found))
    });

    let h_get = Rc::clone(host);
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let target_h = arg(args, 0).as_object();
        let prop_val = arg(args, 1);
        let receiver = arg(args, 2);
        if prop_val.is_symbol() {
            if let Some(th) = target_h {
                let k_str = vm.to_property_key(prop_val)?;
                let key = vm.heap.intern_key(&k_str);
                return vm.get_property_with_receiver(th, key, receiver);
            }
            return Ok(JsValue::UNDEFINED);
        }
        let name = vm.value_to_rust_string(prop_val);
        if let Some(th) = target_h {
            let k = vm.heap.intern_key(&name);
            if vm.has_property_by_key(th, k)? {
                return vm.get_property_with_receiver(th, k, receiver);
            }
        }
        if name.is_empty() {
            return Ok(JsValue::UNDEFINED);
        }
        let matches = {
            let doc_rc = Rc::clone(&h_get.borrow().document);
            let doc = doc_rc.borrow();
            find_window_named_elements(&doc, &name)
        };
        match matches.as_slice() {
            [] => Ok(JsValue::UNDEFINED),
            [only] => Ok(wrap_node(vm, &h_get, *only)),
            _ => Ok(wrap_node_list(vm, &h_get, matches)),
        }
    });

    let proxy = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(rix_js::ProxyObject {
            target,
            handler,
            revoked: false,
        }),
        Some(event_target_proto),
    ));
    let g = vm.global;
    vm.heap.object_set_prototype(g, Some(proxy), &mut vm.shapes);
}

fn find_window_named_elements(doc: &Document, name: &str) -> Vec<NodeId> {
    let mut out = Vec::new();
    for nid in doc.descendants(doc.root()) {
        let Some(NodeData::Element(el)) = doc.get(nid) else {
            continue;
        };
        if el.cached_id.as_deref() == Some(name) {
            out.push(nid);
            continue;
        }
        let tag = el.name.local.as_str();
        if matches!(
            tag,
            "a" | "applet" | "area" | "embed" | "form" | "frameset" | "iframe" | "img" | "object"
        ) && el.get_attribute("name") == Some(name)
        {
            out.push(nid);
        }
    }
    out
}

fn install_fragment_prototype(vm: &mut JsVm, host: &SharedHost) {
    let fp = host.borrow().prototypes.fragment_proto;
    install_parent_node_mixin(vm, host, fp);
    let sp = host.borrow().prototypes.shadow_root_proto;
    install_parent_node_mixin(vm, host, sp);
    let h = Rc::clone(host);
    define_host_method(vm, fp, "querySelector", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let sel = vm.value_to_rust_string(arg(args, 0));
        let parsed = parse_selector_list_strict(&sel)
            .map_err(|e| crate::web_apis::create_dom_exception(vm, "SyntaxError", &e))?;
        let found = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            query_selector_in_subtree(&doc, nid, &parsed, None, None)
        };
        Ok(found.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    let h = Rc::clone(host);
    define_host_method(vm, fp, "querySelectorAll", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let sel = vm.value_to_rust_string(arg(args, 0));
        let parsed = parse_selector_list_strict(&sel)
            .map_err(|e| crate::web_apis::create_dom_exception(vm, "SyntaxError", &e))?;
        let list = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            query_selector_all_in_subtree(&doc, nid, &parsed, None, None)
        };
        Ok(wrap_node_list(vm, &h, list))
    });
    let h = Rc::clone(host);
    define_host_method(vm, fp, "getElementById", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let id = vm.value_to_rust_string(arg(args, 0));
        let found = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            let res = doc.descendants(nid).find(|&n| {
                matches!(doc.get(n), Some(NodeData::Element(e)) if e.get_attribute("id") == Some(id.as_str()))
            });
            res
        };
        Ok(found.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
}

pub(crate) fn install_child_node_mixin(vm: &mut JsVm, host: &SharedHost, proto: HeapHandle) {
    let h = Rc::clone(host);
    define_host_method(vm, proto, "remove", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let parent_opt = h.borrow().document.borrow().parent(nid);
        if let Some(parent) = parent_opt {
            remove_node_from_parent(vm, &h, parent, nid)?;
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, proto, "before", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let Some(parent) = h.borrow().document.borrow().parent(nid) else {
            return Ok(JsValue::UNDEFINED);
        };
        for &a in args {
            let child = coerce_node_or_text(vm, &h, a);
            insert_node_into_parent(vm, &h, parent, child, Some(nid))?;
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, proto, "after", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let info = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            doc.parent(nid).map(|p| (p, doc.next_sibling(nid)))
        };
        let Some((parent, next_sib)) = info else {
            return Ok(JsValue::UNDEFINED);
        };
        for &a in args {
            let child = coerce_node_or_text(vm, &h, a);
            insert_node_into_parent(vm, &h, parent, child, next_sib)?;
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, proto, "replaceWith", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let info = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            doc.parent(nid).map(|p| (p, doc.next_sibling(nid)))
        };
        let Some((parent, next_sib)) = info else {
            return Ok(JsValue::UNDEFINED);
        };
        let nodes: Vec<NodeId> = args
            .iter()
            .map(|&a| coerce_node_or_text(vm, &h, a))
            .collect();
        let mut viable_next = next_sib;
        while let Some(vn) = viable_next {
            if nodes.contains(&vn) {
                viable_next = h.borrow().document.borrow().next_sibling(vn);
            } else {
                break;
            }
        }
        if !nodes.contains(&nid) {
            remove_node_from_parent(vm, &h, parent, nid)?;
        }
        for child in nodes {
            insert_node_into_parent(vm, &h, parent, child, viable_next)?;
        }
        Ok(JsValue::UNDEFINED)
    });
}

fn install_parent_node_mixin(vm: &mut JsVm, host: &SharedHost, proto: HeapHandle) {
    // W3C DOM ParentNode element-child getters (`children`, `firstElementChild`,
    // `lastElementChild`, `childElementCount`) on Element, Document, DocumentFragment, ShadowRoot.
    let h = Rc::clone(host);
    let g_ch = vm.alloc_host_fn("get children", move |vm, t, _| {
        let parent = require_node(vm, &h, t)?;
        Ok(crate::dom_collections::create_live_html_collection(
            vm,
            &h,
            crate::dom_interfaces::HtmlCollectionKind::Children(parent),
        ))
    });
    define_host_accessor(vm, proto, "children", Some(g_ch), None);

    let h = Rc::clone(host);
    let g_fec = vm.alloc_host_fn("get firstElementChild", move |vm, t, _| {
        let parent = require_node(vm, &h, t)?;
        let found = h.borrow().document.borrow().children(parent).find(|&c| {
            matches!(
                h.borrow().document.borrow().get(c),
                Some(NodeData::Element(_))
            )
        });
        Ok(found.map_or(JsValue::NULL, |n| wrap_node(vm, &h, n)))
    });
    define_host_accessor(vm, proto, "firstElementChild", Some(g_fec), None);

    let h = Rc::clone(host);
    let g_lec = vm.alloc_host_fn("get lastElementChild", move |vm, t, _| {
        let parent = require_node(vm, &h, t)?;
        let found = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            let kids: Vec<NodeId> = doc.children(parent).collect();
            kids.into_iter()
                .rev()
                .find(|&c| matches!(doc.get(c), Some(NodeData::Element(_))))
        };
        Ok(found.map_or(JsValue::NULL, |n| wrap_node(vm, &h, n)))
    });
    define_host_accessor(vm, proto, "lastElementChild", Some(g_lec), None);

    let h = Rc::clone(host);
    let g_cec = vm.alloc_host_fn("get childElementCount", move |vm, t, _| {
        let parent = require_node(vm, &h, t)?;
        let cnt = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            doc.children(parent)
                .filter(|&c| matches!(doc.get(c), Some(NodeData::Element(_))))
                .count()
        };
        Ok(JsValue::from_i32(cnt as i32))
    });
    define_host_accessor(vm, proto, "childElementCount", Some(g_cec), None);

    let h = Rc::clone(host);
    define_host_method(vm, proto, "append", move |vm, t, args| {
        let parent = require_node(vm, &h, t)?;
        for &a in args {
            let child = coerce_node_or_text(vm, &h, a);
            insert_node_into_parent(vm, &h, parent, child, None)?;
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, proto, "prepend", move |vm, t, args| {
        let parent = require_node(vm, &h, t)?;
        let first = h.borrow().document.borrow().first_child(parent);
        for &a in args {
            let child = coerce_node_or_text(vm, &h, a);
            insert_node_into_parent(vm, &h, parent, child, first)?;
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, proto, "replaceChildren", move |vm, t, args| {
        let parent = require_node(vm, &h, t)?;
        let kids: Vec<NodeId> = h.borrow().document.borrow().children(parent).collect();
        for c in kids {
            remove_node_from_parent(vm, &h, parent, c)?;
        }
        for &a in args {
            let child = coerce_node_or_text(vm, &h, a);
            insert_node_into_parent(vm, &h, parent, child, None)?;
        }
        Ok(JsValue::UNDEFINED)
    });
}

pub(crate) fn upgrade_if_custom_element(
    vm: &mut JsVm,
    host: &SharedHost,
    nid: NodeId,
    ce_name: &str,
    local_tag: &str,
) {
    if host.borrow().upgraded_custom_elements.contains(&nid) {
        return;
    }
    let def_opt = host.borrow().custom_elements.get(ce_name).cloned();
    let Some(def) = def_opt else {
        return;
    };
    // Verify autonomous vs customized built-in match.
    match &def.extends {
        Some(ext) => {
            if !ext.eq_ignore_ascii_case(local_tag) {
                return;
            }
        }
        None => {
            if !ce_name.eq_ignore_ascii_case(local_tag) {
                return;
            }
        }
    }
    {
        let doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = doc_rc.borrow_mut();
        if let Some(NodeData::Element(e)) = doc.get_mut(nid) {
            e.pseudo_state.insert(rix_dom::ElementState::DEFINED);
        }
    }
    {
        let mut hb = host.borrow_mut();
        hb.upgraded_custom_elements.insert(nid);
        hb.invalidate_element_state(nid, rix_dom::ElementState::DEFINED);
        hb.pending_ce_upgrade_node = Some(nid);
    }
    let wrapper = wrap_node(vm, host, nid);
    if let Some(wh) = wrapper.as_object() {
        if let Ok(ctor_proto) = vm.get_property_by_key(def.constructor, vm.key_prototype) {
            if let Some(ph) = ctor_proto.as_object() {
                vm.heap.object_set_prototype(wh, Some(ph), &mut vm.shapes);
            }
        }
    }
    let ctor_val = JsValue::from_object(def.constructor);
    let _ = vm.construct_value(ctor_val, &[], ctor_val);
    host.borrow_mut().pending_ce_upgrade_node = None;
}

pub(crate) fn notify_custom_element_subtree(
    vm: &mut JsVm,
    host: &SharedHost,
    root: NodeId,
    connected: bool,
) {
    if host.borrow().custom_elements.is_empty() {
        return;
    }
    let nodes: Vec<(NodeId, String, String)> = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        let mut out = Vec::new();
        for n in doc.descendants(root) {
            if let Some(NodeData::Element(e)) = doc.get(n) {
                if &*e.name.ns != crate::dom_interfaces::HTML_NS {
                    continue;
                }
                let tag = e.name.local.as_str().to_ascii_lowercase();
                if let Some(def) = hb.custom_elements.get(&tag) {
                    if def.extends.is_none() {
                        out.push((n, tag.clone(), tag));
                        continue;
                    }
                }
                if let Some(is_val) = e.get_attribute("is") {
                    if let Some(def) = hb.custom_elements.get(is_val) {
                        if def.extends.as_deref() == Some(tag.as_str()) {
                            out.push((n, is_val.to_string(), tag));
                        }
                    }
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
    for (nid, ce_name, tag) in nodes {
        if connected {
            upgrade_if_custom_element(vm, host, nid, &ce_name, &tag);
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

pub(crate) fn get_attr_string(host: &SharedHost, nid: NodeId, name: &str) -> Option<String> {
    let is_html_doc = !host.borrow().is_xml_doc_for_node(nid);
    let _doc_rc = Rc::clone(&host.borrow().document);
    let doc = _doc_rc.borrow();
    match doc.get(nid) {
        Some(NodeData::Element(e)) => {
            let fold = is_html_doc && &*e.name.ns == crate::dom_interfaces::HTML_NS;
            let qn = if fold {
                name.to_ascii_lowercase()
            } else {
                name.to_string()
            };
            e.get_attribute_by_qname(&qn).map(|(_, v)| v.to_string())
        }
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
    let is_html_doc = !host.borrow().is_xml_doc_for_node(nid);
    let (old_val, tag_lower, attr_ns, attr_local) = {
        let _doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = _doc_rc.borrow_mut();
        match doc.get_mut(nid) {
            Some(NodeData::Element(e)) => {
                // Per WHATWG DOM §4.9 setAttribute: HTML elements in HTML documents lowercase the name.
                let fold = is_html_doc && &*e.name.ns == crate::dom_interfaces::HTML_NS;
                let effective_name = if fold && name.bytes().any(|b| b.is_ascii_uppercase()) {
                    std::borrow::Cow::Owned(name.to_ascii_lowercase())
                } else {
                    std::borrow::Cow::Borrowed(name)
                };
                let tag = e.name.local.as_str().to_ascii_lowercase();
                let (old, ns, local) = e.set_attribute_by_qname(&effective_name, val);
                (old, tag, ns, local)
            }
            _ => return,
        }
    };
    host.borrow_mut()
        .invalidate_attr_mutation(nid, &attr_local, old_val.as_deref(), Some(val));
    if matches!(tag_lower.as_str(), "body" | "frameset")
        && attr_local.starts_with("on")
        && attr_ns.is_empty()
    {
        let hook = host.borrow().body_attr_handler_hook.clone();
        if let Some(f) = hook {
            f(vm, nid, &attr_local, val);
        }
    }
    host.borrow().emit_mutation(DomMutation::Attributes {
        target: nid,
        attribute_name: attr_local.clone(),
        attribute_namespace: if attr_ns.is_empty() {
            None
        } else {
            Some(attr_ns)
        },
        old_value: old_val.clone(),
    });
    let observed = host
        .borrow()
        .custom_elements
        .get(&tag_lower)
        .is_some_and(|d| d.observed_attributes.iter().any(|a| a == &attr_local));
    if observed {
        let n_val = JsValue::from_string(vm.heap.alloc_string(attr_local));
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

pub(crate) fn remove_element_attr(vm: &mut JsVm, host: &SharedHost, nid: NodeId, name: &str) {
    let is_html_doc = !host.borrow().is_xml_doc_for_node(nid);
    let removed = {
        let _doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = _doc_rc.borrow_mut();
        match doc.get_mut(nid) {
            Some(NodeData::Element(e)) => {
                let fold = is_html_doc && &*e.name.ns == crate::dom_interfaces::HTML_NS;
                let effective_name = if fold && name.bytes().any(|b| b.is_ascii_uppercase()) {
                    std::borrow::Cow::Owned(name.to_ascii_lowercase())
                } else {
                    std::borrow::Cow::Borrowed(name)
                };
                let tag = e.name.local.as_str().to_ascii_lowercase();
                e.remove_attribute_by_qname(&effective_name)
                    .map(|(old, ns, local)| (old, ns, local, tag))
            }
            _ => return,
        }
    };
    if let Some((old_val, ns, local, tag_lower)) = removed {
        {
            let mut hb = host.borrow_mut();
            if let Some(ah) = hb
                .elem_attr_handles
                .remove(&(nid, ns.clone(), local.clone()))
            {
                if let Some(entry) = hb.attrs.get_mut(&ah) {
                    entry.owner_element = None;
                    entry.detached_value = old_val.clone();
                }
            }
            hb.invalidate_attr_mutation(nid, &local, Some(&old_val), None);
        }
        host.borrow().emit_mutation(DomMutation::Attributes {
            target: nid,
            attribute_name: local.clone(),
            attribute_namespace: if ns.is_empty() { None } else { Some(ns) },
            old_value: Some(old_val.clone()),
        });
        let observed = host
            .borrow()
            .custom_elements
            .get(&tag_lower)
            .is_some_and(|d| d.observed_attributes.iter().any(|a| a == &local));
        if observed {
            let n_val = JsValue::from_string(vm.heap.alloc_string(local));
            let o_val = JsValue::from_string(vm.heap.alloc_string(old_val));
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
    let tag_lower = tag.to_ascii_lowercase();
    doc.descendants(root)
        .filter(|&n| {
            n != root
                && matches!(doc.get(n), Some(NodeData::Element(e)) if {
                    if wildcard {
                        true
                    } else if &*e.name.ns == "http://www.w3.org/1999/xhtml" {
                        e.name.local.as_str() == tag_lower
                    } else {
                        e.name.local.as_str() == tag
                    }
                })
        })
        .collect()
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

fn kebab_to_camel(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '-' {
            if let Some(&next) = chars.peek() {
                if next.is_ascii_lowercase() {
                    chars.next();
                    out.push(next.to_ascii_uppercase());
                    continue;
                }
            }
        }
        out.push(ch);
    }
    out
}

fn dataset_keys(host: &SharedHost, nid: NodeId) -> Vec<(String, String)> {
    let doc_rc = Rc::clone(&host.borrow().document);
    let doc = doc_rc.borrow();
    let Some(NodeData::Element(e)) = doc.get(nid) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (q, val) in &e.attributes {
        if q.ns.is_empty() && q.prefix.is_none() {
            if let Some(rest) = q.local.as_str().strip_prefix("data-") {
                if !rest.bytes().any(|b| b.is_ascii_uppercase()) {
                    out.push((kebab_to_camel(rest), val.clone()));
                }
            }
        }
    }
    out
}

fn create_dataset_proxy(vm: &mut JsVm, host: &SharedHost, nid: NodeId) -> JsValue {
    let proto = host.borrow().prototypes.dom_string_map_proto;
    let target = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let handler = vm.alloc_ordinary_object();
    let h1 = Rc::clone(host);
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let prop = arg(args, 1);
        if prop.is_symbol() {
            if let Some(th) = arg(args, 0).as_object() {
                let ks = vm.to_property_key(prop)?;
                let k = vm.heap.intern_key(&ks);
                return vm.get_property_with_receiver(th, k, arg(args, 2));
            }
            return Ok(JsValue::UNDEFINED);
        }
        let key = vm.value_to_rust_string(prop);
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
        if !crate::dom_interfaces::is_valid_attribute_local_name(&attr) {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "InvalidCharacterError",
                "Invalid data-* attribute name",
            ));
        }
        set_element_attr(vm, &h2, nid, &attr, &val);
        Ok(JsValue::TRUE)
    });
    let h3 = Rc::clone(host);
    define_host_method(vm, handler, "deleteProperty", move |vm, _, args| {
        let key = vm.value_to_rust_string(arg(args, 1));
        let attr = format!("data-{}", camel_to_kebab(&key));
        remove_element_attr(vm, &h3, nid, &attr);
        Ok(JsValue::TRUE)
    });
    let h4 = Rc::clone(host);
    define_host_method(vm, handler, "has", move |vm, _, args| {
        let prop = arg(args, 1);
        if prop.is_symbol() {
            return Ok(JsValue::FALSE);
        }
        let key = vm.value_to_rust_string(prop);
        let attr = format!("data-{}", camel_to_kebab(&key));
        Ok(JsValue::from_bool(
            get_attr_string(&h4, nid, &attr).is_some(),
        ))
    });
    let h5 = Rc::clone(host);
    define_host_method(vm, handler, "ownKeys", move |vm, _, _| {
        let arr = vm.alloc_array();
        for (k, _) in dataset_keys(&h5, nid) {
            let v = JsValue::from_string(vm.heap.alloc_string(k));
            vm.heap.object_push_element(arr, v);
        }
        Ok(JsValue::from_object(arr))
    });
    let h6 = Rc::clone(host);
    define_host_method(
        vm,
        handler,
        "getOwnPropertyDescriptor",
        move |vm, _, args| {
            let prop = arg(args, 1);
            if prop.is_symbol() {
                return Ok(JsValue::UNDEFINED);
            }
            let key = vm.value_to_rust_string(prop);
            let Some((_, val)) = dataset_keys(&h6, nid).into_iter().find(|(k, _)| k == &key) else {
                return Ok(JsValue::UNDEFINED);
            };
            let desc = vm.alloc_ordinary_object();
            let v = JsValue::from_string(vm.heap.alloc_string(val));
            for (ks, dv) in [
                ("value", v),
                ("writable", JsValue::TRUE),
                ("enumerable", JsValue::TRUE),
                ("configurable", JsValue::TRUE),
            ] {
                let dk = vm.heap.intern_key(ks);
                vm.heap
                    .object_define_data(desc, dk, dv, PropertyAttrs::DEFAULT, &mut vm.shapes);
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
    JsValue::from_object(proxy)
}
