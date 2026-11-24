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
    pub html_iframe_element_proto: HeapHandle,
    pub html_media_element_proto: HeapHandle,
    pub html_video_element_proto: HeapHandle,
    pub html_audio_element_proto: HeapHandle,
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
            self.html_iframe_element_proto,
            self.html_media_element_proto,
            self.html_video_element_proto,
            self.html_audio_element_proto,
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
    pub scroll_offset: Point,
    pub device_pixel_ratio: f32,
    pub dark_mode: bool,
    pub font_db: Arc<FontDatabase>,
    pub layout_engine: LayoutEngine,
    style_ctx: StyleContext,
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
            scroll_offset: Point::ZERO,
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
        if let Some(proto) = self.css_stylesheet_proto {
            out.push(JsValue::from_object(proto));
        }
        for &proto in self.css_rule_protos.values() {
            out.push(JsValue::from_object(proto));
        }
    }

    /// Sweeps dead unpinned weak DOM wrappers and releases their tag table entries (zero leaks!).
    pub fn sweep_dead_wrappers(&mut self, heap: &GcHeap) -> usize {
        self.stylesheet_handle_map
            .retain(|h, _| heap.get_object(*h).is_some());
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
            Some(pe) => ctx.pseudo_style_for(node, pe).cloned(),
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
#[derive(Debug, Clone, Copy)]
pub struct ResolvedLayoutBox {
    pub content_size: Size,
    pub border_size: Size,
    pub padding: EdgeInsets,
    pub margin: EdgeInsets,
    pub parent_content_size: Size,
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
                    } else if el.name.local.as_str().eq_ignore_ascii_case("iframe") {
                        h.prototypes.html_iframe_element_proto
                    } else if el.name.local.as_str().eq_ignore_ascii_case("video") {
                        h.prototypes.html_video_element_proto
                    } else if el.name.local.as_str().eq_ignore_ascii_case("audio") {
                        h.prototypes.html_audio_element_proto
                    } else {
                        h.prototypes.html_element_proto
                    }
                }
                Some(
                    NodeData::Text(_)
                    | NodeData::Comment(_)
                    | NodeData::ProcessingInstruction { .. },
                ) => h.prototypes.text_proto,
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
    let html_iframe_element_proto = make_proto(vm, html_element_proto);
    let html_media_element_proto = make_proto(vm, html_element_proto);
    let html_video_element_proto = make_proto(vm, html_media_element_proto);
    let html_audio_element_proto = make_proto(vm, html_media_element_proto);
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
        html_iframe_element_proto,
        html_media_element_proto,
        html_video_element_proto,
        html_audio_element_proto,
        document_proto,
        shadow_root_proto,
        text_proto,
        fragment_proto,
    };

    let host = Rc::new(RefCell::new(HostState::new(
        document, protos, viewport, font_db,
    )));

    // HTML Living Standard §7.3.3.1: `Window` inherits from `WindowProperties` (named access
    // for element `id` / `name` attributes) -> `EventTarget.prototype` -> `Object.prototype`.
    install_window_properties_proxy(vm, &host, event_target_proto);

    install_node_prototype(vm, &host);
    install_element_prototype(vm, &host);
    install_document_prototype(vm, &host);
    install_shadow_root_prototype(vm, &host);
    install_fragment_prototype(vm, &host);
    install_text_prototype(vm, &host);
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
        let name = match h.borrow().document.borrow().get(nid) {
            Some(NodeData::Element(e)) => {
                if &*e.name.ns == "http://www.w3.org/1999/xhtml" {
                    e.name.local.as_str().to_ascii_uppercase()
                } else {
                    e.name.local.as_str().to_string()
                }
            }
            Some(NodeData::Text(_)) => "#text".into(),
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
            Some(NodeData::Text(_) | NodeData::Comment(_) | NodeData::ProcessingInstruction { .. })
        );
        if is_text {
            let (old, parent_opt) = {
                let _doc_rc = Rc::clone(&h2.borrow().document);
                let mut doc = _doc_rc.borrow_mut();
                let parent_opt = doc.parent(nid);
                let old = match doc.get_mut(nid) {
                    Some(
                        NodeData::Text(s)
                        | NodeData::Comment(s)
                        | NodeData::ProcessingInstruction { data: s, .. },
                    ) => std::mem::replace(s, new_text),
                    _ => String::new(),
                };
                (old, parent_opt)
            };
            if let Some(parent) = parent_opt {
                h2.borrow_mut().invalidate_dom_mutation(parent, None);
            }
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
        h.borrow_mut().invalidate_dom_mutation(parent, None);
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
        h.borrow_mut()
            .invalidate_dom_mutation(parent, Some(new_child));
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
        let txt = h1.borrow().document.borrow().inner_text(nid);
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
        let nid = require_node(vm, &h, t)?;
        let ns_arg = arg(args, 0);
        let ns = if ns_arg.is_null() || ns_arg.is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(ns_arg)
        };
        let qname = vm.value_to_rust_string(arg(args, 1));
        let val = vm.value_to_rust_string(arg(args, 2));
        let (prefix, local) = match qname.split_once(':') {
            Some((p, l)) => (Some(p), l),
            None => (None, qname.as_str()),
        };
        let old_val = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            match doc.get_mut(nid) {
                Some(NodeData::Element(e)) => e
                    .set_attribute_ns(&ns, prefix, local, &val)
                    .map(|(old, _)| old),
                _ => return Ok(JsValue::UNDEFINED),
            }
        };
        h.borrow_mut()
            .invalidate_attr_mutation(nid, local, old_val.as_deref(), Some(&val));
        h.borrow().emit_mutation(DomMutation::Attributes {
            target: nid,
            attribute_name: local.to_string(),
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
        let nid = require_node(vm, &h, t)?;
        let name = vm.value_to_rust_string(arg(args, 0));
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
                if let Some(p) = h.borrow().document.borrow().parent(nid) {
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
                if let Some(p) = h.borrow().document.borrow().parent(nid) {
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
        Ok(wrap_node_list(vm, &h, list))
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

    // Modern DOM ChildNode & ParentNode mixins: remove(), before(), after(), replaceWith(), append(), prepend(), replaceChildren()
    install_child_node_mixin(vm, host, ep);
    install_parent_node_mixin(vm, host, ep);

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

    install_html5_element_semantics(vm, host, ep);
}

/// Installs WHATWG HTML5 element semantics on `Element.prototype`:
/// `<template>.content`, `<dialog>`, `<details>`, `<form>` / Constraint Validation API,
/// `<select>`/`<option>`/`<label>`, Popover API, and standard boolean/string attribute reflections.
fn install_html5_element_semantics(vm: &mut JsVm, host: &SharedHost, ep: HeapHandle) {
    // `<template>.content` -> inert `DocumentFragment`
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
    define_host_accessor(vm, ep, "content", Some(g), None);

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

    // Reflected string attributes (`name`, `placeholder`, `title`, `lang`, `dir`, `htmlFor`, `popover`).
    for (prop, attr) in [
        ("name", "name"),
        ("placeholder", "placeholder"),
        ("title", "title"),
        ("lang", "lang"),
        ("dir", "dir"),
        ("htmlFor", "for"),
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
            return Err(vm.make_error(
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
            return Err(vm.make_error(
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
            return Err(vm.make_error("InvalidStateError", "Dialog is already open"));
        }
        set_element_attr(vm, &h, nid, "open", "");
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
        remove_element_attr(vm, &h, nid, "open");
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

    // document.characterSet / charset / inputEncoding / compatMode / currentScript / doctype / dir
    for prop in ["characterSet", "charset", "inputEncoding"] {
        let h = Rc::clone(host);
        let g = vm.alloc_host_fn(prop, move |vm, _, _| {
            let cs = h.borrow().document.borrow().character_set().to_string();
            Ok(JsValue::from_string(vm.heap.alloc_string(cs)))
        });
        define_host_accessor(vm, dp, prop, Some(g), None);
    }

    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get compatMode", move |vm, _, _| {
        let cm = h.borrow().document.borrow().compat_mode();
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
    let g = vm.alloc_host_fn("get doctype", move |vm, _, _| {
        let dt_id = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            let found = doc
                .children(doc.root())
                .find(|&n| matches!(doc.get(n), Some(NodeData::DocumentType { .. })));
            found
        };
        let Some(dt_id) = dt_id else {
            return Ok(JsValue::NULL);
        };
        let wrapper = wrap_node(vm, &h, dt_id);
        if let Some(wh) = wrapper.as_object() {
            let dt_fields = {
                let doc_rc = Rc::clone(&h.borrow().document);
                let doc = doc_rc.borrow();
                match doc.get(dt_id) {
                    Some(NodeData::DocumentType {
                        name,
                        public_id,
                        system_id,
                    }) => Some((name.clone(), public_id.clone(), system_id.clone())),
                    _ => None,
                }
            };
            if let Some((name, pub_id, sys_id)) = dt_fields {
                for (k, v) in [("name", name), ("publicId", pub_id), ("systemId", sys_id)] {
                    let key = vm.heap.intern_key(k);
                    let s = vm.heap.alloc_string(v);
                    vm.heap.object_define_data(
                        wh,
                        key,
                        JsValue::from_string(s),
                        PropertyAttrs::DEFAULT,
                        &mut vm.shapes,
                    );
                }
            }
        }
        Ok(wrapper)
    });
    define_host_accessor(vm, dp, "doctype", Some(g), None);

    let h1 = Rc::clone(host);
    let g = vm.alloc_host_fn("get dir", move |vm, _, _| {
        let dir = {
            let doc_rc = Rc::clone(&h1.borrow().document);
            let doc = doc_rc.borrow();
            doc.document_element()
                .and_then(|de| match doc.get(de) {
                    Some(NodeData::Element(e)) => e.get_attribute("dir").map(str::to_string),
                    _ => None,
                })
                .unwrap_or_default()
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(dir)))
    });
    let h2 = Rc::clone(host);
    let s = vm.alloc_host_fn("set dir", move |vm, _, args| {
        let next = vm.value_to_rust_string(arg(args, 0));
        let de = h2.borrow().document.borrow().document_element();
        if let Some(de) = de {
            set_element_attr(vm, &h2, de, "dir", &next);
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, dp, "dir", Some(g), Some(s));

    // document.title getter/setter
    let h_get = Rc::clone(host);
    let g = vm.alloc_host_fn("get title", move |vm, _, _| {
        let title = {
            let doc_rc = Rc::clone(&h_get.borrow().document);
            let doc = doc_rc.borrow();
            let found = doc
                .descendants(doc.root())
                .find(|&n| {
                    matches!(
                        doc.get(n),
                        Some(NodeData::Element(e)) if e.name.local.as_str().eq_ignore_ascii_case("title")
                    )
                })
                .map(|n| doc.inner_text(n).trim().to_string())
                .unwrap_or_default();
            found
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(title)))
    });
    let h_set = Rc::clone(host);
    let s = vm.alloc_host_fn("set title", move |vm, _, args| {
        let next = vm.value_to_rust_string(arg(args, 0));
        let doc_rc = Rc::clone(&h_set.borrow().document);
        let mut doc = doc_rc.borrow_mut();
        let existing = doc.descendants(doc.root()).find(|&n| {
            matches!(
                doc.get(n),
                Some(NodeData::Element(e)) if e.name.local.as_str().eq_ignore_ascii_case("title")
            )
        });
        let head = doc.descendants(doc.root()).find(|&n| {
            matches!(
                doc.get(n),
                Some(NodeData::Element(e)) if e.name.local.as_str().eq_ignore_ascii_case("head")
            )
        });
        if let Some(tn) = existing {
            let kids: Vec<NodeId> = doc.children(tn).collect();
            for k in kids {
                doc.remove_child(k);
            }
            let text_node = doc.create_text(&next);
            doc.append_child(tn, text_node);
        } else if let Some(head) = head {
            let tn = doc.create_element("title");
            let text_node = doc.create_text(&next);
            doc.append_child(tn, text_node);
            doc.append_child(head, tn);
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, dp, "title", Some(g), Some(s));

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
    define_host_method(vm, dp, "createElementNS", move |vm, _, args| {
        let ns_arg = arg(args, 0);
        let ns = if ns_arg.is_null() || ns_arg.is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(ns_arg)
        };
        let raw_qname = vm.value_to_rust_string(arg(args, 1));
        let (prefix, local) = match raw_qname.split_once(':') {
            Some((p, l)) => (Some(p), l),
            None => (None, raw_qname.as_str()),
        };
        let nid = h
            .borrow()
            .document
            .borrow_mut()
            .create_element_ns(&ns, prefix, local);
        let wrapper = wrap_node(vm, &h, nid);
        if ns == "http://www.w3.org/1999/xhtml" {
            upgrade_if_custom_element(vm, &h, nid, local);
        }
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
    define_host_method(vm, dp, "createProcessingInstruction", move |vm, _, args| {
        let target = vm.value_to_rust_string(arg(args, 0));
        let data = vm.value_to_rust_string(arg(args, 1));
        if data.contains("?>") {
            return Err(vm.make_error(
                "InvalidCharacterError",
                "ProcessingInstruction data cannot contain '?>'",
            ));
        }
        let nid = h
            .borrow()
            .document
            .borrow_mut()
            .create_processing_instruction(&target, &data);
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

    // document.implementation (WHATWG DOM §4.5.1 DOMImplementation)
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get implementation", move |vm, _, _| {
        let imp = vm.alloc_ordinary_object();
        let h_html = Rc::clone(&h);
        define_host_method(vm, imp, "createHTMLDocument", move |vm, _, args| {
            let title = if args.first().is_some_and(|v| !v.is_undefined()) {
                Some(vm.value_to_rust_string(args[0]))
            } else {
                None
            };
            let html = match title {
                Some(t) => format!(
                    "<!DOCTYPE html><html><head><title>{t}</title></head><body></body></html>"
                ),
                None => "<!DOCTYPE html><html><head></head><body></body></html>".to_string(),
            };
            let parsed = rix_html::parse_html(&html);
            let sub_root = {
                let doc_rc = Rc::clone(&h_html.borrow().document);
                let mut doc = doc_rc.borrow_mut();
                let r = doc.new_node(NodeData::Document);
                let pr = parsed.root();
                for child in parsed.children(pr) {
                    let cloned = doc.clone_subtree_from(&parsed, child);
                    doc.append_child(r, cloned);
                }
                r
            };
            Ok(wrap_node(vm, &h_html, sub_root))
        });
        let h_dt = Rc::clone(&h);
        define_host_method(vm, imp, "createDocumentType", move |vm, _, args| {
            let name = vm.value_to_rust_string(arg(args, 0));
            let pub_id = vm.value_to_rust_string(arg(args, 1));
            let sys_id = vm.value_to_rust_string(arg(args, 2));
            let nid = h_dt
                .borrow()
                .document
                .borrow_mut()
                .new_node(NodeData::DocumentType {
                    name,
                    public_id: pub_id,
                    system_id: sys_id,
                });
            Ok(wrap_node(vm, &h_dt, nid))
        });
        Ok(JsValue::from_object(imp))
    });
    define_host_accessor(vm, dp, "implementation", Some(g), None);

    // Document.activeElement
    let h = Rc::clone(host);
    let g_ae = vm.alloc_host_fn("get activeElement", move |vm, _, _| {
        let active = h.borrow().active_element;
        if let Some(nid) = active {
            if h.borrow().document.borrow().is_connected(nid) {
                return Ok(wrap_node(vm, &h, nid));
            }
        }
        let root = h.borrow().document.borrow().root();
        let body = find_elements_by_tag(&h, root, "body").into_iter().next();
        Ok(body.map_or(JsValue::NULL, |b| wrap_node(vm, &h, b)))
    });
    define_host_accessor(vm, dp, "activeElement", Some(g_ae), None);
    install_parent_node_mixin(vm, host, dp);

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
        let parsed = parse_selector_list_strict(&sel)
            .map_err(|e| crate::web_apis::create_dom_exception(vm, "SyntaxError", &e))?;
        let found = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let doc = _doc_rc.borrow();
            query_selector_in_subtree(&doc, doc.root(), &parsed, None, None)
        };
        Ok(found.map_or(JsValue::NULL, |nid| wrap_node(vm, &h, nid)))
    });
    let h = Rc::clone(host);
    define_host_method(vm, dp, "querySelectorAll", move |vm, _, args| {
        let sel = vm.value_to_rust_string(arg(args, 0));
        let parsed = parse_selector_list_strict(&sel)
            .map_err(|e| crate::web_apis::create_dom_exception(vm, "SyntaxError", &e))?;
        let list = {
            let _doc_rc = Rc::clone(&h.borrow().document);
            let doc = _doc_rc.borrow();
            query_selector_all_in_subtree(&doc, doc.root(), &parsed, None, None)
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
    // Expose base constructors (`HTMLElement`, `Element`, `Node`, `EventTarget`, `Document`).
    let protos = host.borrow().prototypes;
    for (name, proto) in [
        ("EventTarget", protos.event_target_proto),
        ("Node", protos.node_proto),
        ("Element", protos.element_proto),
        ("HTMLElement", protos.html_element_proto),
        ("HTMLIFrameElement", protos.html_iframe_element_proto),
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
        vm.mark_constructor(ctor);
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

fn install_child_node_mixin(vm: &mut JsVm, host: &SharedHost, proto: HeapHandle) {
    let h = Rc::clone(host);
    define_host_method(vm, proto, "remove", move |vm, t, _| {
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
            h.borrow_mut().invalidate_dom_mutation(parent, None);
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
            doc.parent(nid)
                .map(|p| (p, doc.is_connected(nid), doc.next_sibling(nid)))
        };
        let Some((parent, was_conn, next_sib)) = info else {
            return Ok(JsValue::UNDEFINED);
        };
        for &a in args {
            let child = coerce_node_or_text(vm, &h, a);
            insert_node_into_parent(vm, &h, parent, child, next_sib)?;
        }
        h.borrow().document.borrow_mut().remove_child(nid);
        h.borrow_mut().invalidate_dom_mutation(parent, None);
        if was_conn {
            notify_custom_element_subtree(vm, &h, nid, false);
        }
        Ok(JsValue::UNDEFINED)
    });
}

fn install_parent_node_mixin(vm: &mut JsVm, host: &SharedHost, proto: HeapHandle) {
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
        let removed: Vec<NodeId> = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            let kids: Vec<NodeId> = doc.children(parent).collect();
            for &c in &kids {
                doc.remove_child(c);
            }
            kids
        };
        if !removed.is_empty() {
            h.borrow_mut().invalidate_dom_mutation(parent, None);
            h.borrow().emit_mutation(DomMutation::ChildList {
                target: parent,
                added: Vec::new(),
                removed,
                previous_sibling: None,
                next_sibling: None,
            });
        }
        for &a in args {
            let child = coerce_node_or_text(vm, &h, a);
            insert_node_into_parent(vm, &h, parent, child, None)?;
        }
        Ok(JsValue::UNDEFINED)
    });
}

fn install_text_prototype(vm: &mut JsVm, host: &SharedHost) {
    let tp = host.borrow().prototypes.text_proto;
    install_child_node_mixin(vm, host, tp);

    let h1 = Rc::clone(host);
    let g_data = vm.alloc_host_fn("get data", move |vm, t, _| {
        let nid = require_node(vm, &h1, t)?;
        let text = match h1.borrow().document.borrow().get(nid) {
            Some(
                NodeData::Text(s)
                | NodeData::Comment(s)
                | NodeData::ProcessingInstruction { data: s, .. },
            ) => s.clone(),
            _ => String::new(),
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
        let (old, parent_opt) = {
            let doc_rc = Rc::clone(&h2.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            let parent_opt = doc.parent(nid);
            let old = match doc.get_mut(nid) {
                Some(
                    NodeData::Text(s)
                    | NodeData::Comment(s)
                    | NodeData::ProcessingInstruction { data: s, .. },
                ) => std::mem::replace(s, new_val),
                _ => String::new(),
            };
            (old, parent_opt)
        };
        if let Some(parent) = parent_opt {
            h2.borrow_mut().invalidate_dom_mutation(parent, None);
        }
        h2.borrow().emit_mutation(DomMutation::CharacterData {
            target: nid,
            old_value: old,
        });
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, tp, "data", Some(g_data), Some(s_data));

    let h_len = Rc::clone(host);
    let g_len = vm.alloc_host_fn("get length", move |vm, t, _| {
        let nid = require_node(vm, &h_len, t)?;
        let len = match h_len.borrow().document.borrow().get(nid) {
            Some(
                NodeData::Text(s)
                | NodeData::Comment(s)
                | NodeData::ProcessingInstruction { data: s, .. },
            ) => s.encode_utf16().count(),
            _ => 0,
        };
        Ok(JsValue::from_i32(len as i32))
    });
    define_host_accessor(vm, tp, "length", Some(g_len), None);
}

fn upgrade_if_custom_element(vm: &mut JsVm, host: &SharedHost, nid: NodeId, tag: &str) {
    let def_opt = host.borrow().custom_elements.get(tag).cloned();
    let Some(def) = def_opt else {
        return;
    };
    {
        let doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = doc_rc.borrow_mut();
        if let Some(NodeData::Element(e)) = doc.get_mut(nid) {
            e.pseudo_state.insert(rix_dom::ElementState::DEFINED);
        }
    }
    host.borrow_mut()
        .invalidate_element_state(nid, rix_dom::ElementState::DEFINED);
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

pub(crate) fn get_attr_string(host: &SharedHost, nid: NodeId, name: &str) -> Option<String> {
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
    let (old_val, tag_lower, attr_name) = {
        let _doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = _doc_rc.borrow_mut();
        match doc.get_mut(nid) {
            Some(NodeData::Element(e)) => {
                // Per WHATWG DOM §4.9 setAttribute: HTML elements in HTML documents lowercase the name.
                let effective_name: std::borrow::Cow<'_, str> = if &*e.name.ns
                    == "http://www.w3.org/1999/xhtml"
                    && name.bytes().any(|b| b.is_ascii_uppercase())
                {
                    std::borrow::Cow::Owned(name.to_ascii_lowercase())
                } else {
                    std::borrow::Cow::Borrowed(name)
                };
                let old = e.get_attribute(&effective_name).map(str::to_string);
                let tag = e.name.local.as_str().to_ascii_lowercase();
                e.set_attribute(&effective_name, val);
                (old, tag, effective_name.into_owned())
            }
            _ => return,
        }
    };
    let name = attr_name.as_str();
    host.borrow_mut()
        .invalidate_attr_mutation(nid, name, old_val.as_deref(), Some(val));
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
        match doc.get_mut(nid) {
            Some(NodeData::Element(e)) => {
                let old = e.get_attribute(name).map(str::to_string);
                let tag = e.name.local.as_str().to_ascii_lowercase();
                e.remove_attribute(name);
                (old, tag)
            }
            _ => return,
        }
    };
    host.borrow_mut()
        .invalidate_attr_mutation(nid, name, old_val.as_deref(), None);
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
    let tag_lower = tag.to_ascii_lowercase();
    doc.descendants(root)
        .filter(|&n| {
            n != root
                && matches!(doc.get(n), Some(NodeData::Element(e)) if {
                    if wildcard {
                        true
                    } else if &*e.name.ns == "http://www.w3.org/1999/xhtml" {
                        // WHATWG DOM §4.2.6.1: HTML elements in HTML documents match ASCII-lowercased qualifiedName.
                        e.name.local.as_str() == tag_lower
                    } else {
                        // Foreign elements (SVG, MathML, XML) match exact case-sensitive qualifiedName.
                        e.name.local.as_str() == tag
                    }
                })
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
