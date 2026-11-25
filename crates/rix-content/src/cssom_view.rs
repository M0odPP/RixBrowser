//! W3C CSSOM View Module scrolling, visibility, media query, and screen APIs:
//! - `Element.prototype.{checkVisibility, currentCSSZoom, scrollParent}`
//! - `HTMLImageElement.prototype.{x, y}`
//! - Promise-returning `scroll`, `scrollTo`, `scrollBy`, `scrollIntoView` (with `scroll-behavior: smooth`,
//!   writing-mode/RTL alignment, rAF stepping, and interruption semantics)
//! - `MediaQueryList` (extends `EventTarget`), `MediaQueryListEvent`, `window.matchMedia`, `window.screen`
//! - Same-origin child `WindowProxy` CSSOM View bindings

use std::collections::HashMap;
use std::rc::Rc;

use rix_core::{Point, Rect};
use rix_css::{
    ComputedStyle, ContentVisibilityMode, DirectionMode, DisplayMode, MediaContext, OverflowMode,
    PositionMode, ScrollBehaviorMode, VisibilityMode, WritingMode,
};
use rix_dom::{NodeData, NodeId};
use rix_js::promise::{alloc_promise, reject_promise, resolve_promise};
use rix_js::{HeapHandle, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::cssom_geometry::{
    alloc_host_ctor, collect_boxes_for_node, define_host_getter, set_f64_prop,
    transformed_bounding_rect_for_node,
};
use crate::dom_bridge::{
    arg, define_global, define_host_accessor, define_host_method, require_node, wrap_node,
    DomMutation, SharedHost,
};
use crate::event_loop::{SharedEventLoop, TaskPriority};
use crate::events::SharedEvents;

// ---------------------------------------------------------------------------
// State & GC Root Tracking
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ScrollTargetKey {
    Window(NodeId),
    Element(NodeId),
}

struct ActiveSmoothScroll {
    id: u64,
    promise: HeapHandle,
    cancelled: bool,
}

struct MqlRecord {
    handle: HeapHandle,
    owner_doc: NodeId,
    raw_query: String,
    normalized_media: String,
    last_matches: bool,
    onchange: JsValue,
    onchange_wrapper: Option<HeapHandle>,
}

#[derive(Default)]
pub(crate) struct CssomViewState {
    pub(crate) mql_proto: Option<HeapHandle>,
    pub(crate) mql_event_proto: Option<HeapHandle>,
    active_smooth_scrolls: HashMap<ScrollTargetKey, ActiveSmoothScroll>,
    next_scroll_id: u64,
    media_query_lists: Vec<MqlRecord>,
    mql_check_scheduled: bool,
    extra_roots: Vec<HeapHandle>,
}

impl CssomViewState {
    pub(crate) fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for &h in &self.extra_roots {
            out.push(JsValue::from_object(h));
        }
        for p in [self.mql_proto, self.mql_event_proto].into_iter().flatten() {
            out.push(JsValue::from_object(p));
        }
        for s in self.active_smooth_scrolls.values() {
            out.push(JsValue::from_object(s.promise));
        }
        for m in &self.media_query_lists {
            out.push(JsValue::from_object(m.handle));
            out.push(m.onchange);
            if let Some(w) = m.onchange_wrapper {
                out.push(JsValue::from_object(w));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// checkVisibility, currentCSSZoom, scrollParent
// ---------------------------------------------------------------------------

fn flat_tree_parent(doc: &rix_dom::Document, n: NodeId) -> Option<NodeId> {
    if let Some(p) = doc.parent(n) {
        if let Some(NodeData::Element(pel)) = doc.get(p) {
            if let Some(sr) = pel.shadow_root {
                // Element `n` is light-DOM child of shadow host `p`: must be assigned to a `<slot>`.
                let slot_name = match doc.get(n) {
                    Some(NodeData::Element(el)) => el.get_attribute("slot").unwrap_or(""),
                    _ => "",
                };
                for sn in doc.descendants(sr) {
                    if let Some(NodeData::Element(sel)) = doc.get(sn) {
                        if sel.name.local.as_str().eq_ignore_ascii_case("slot")
                            && sel.get_attribute("name").unwrap_or("") == slot_name
                        {
                            return Some(sn);
                        }
                    }
                }
                return None; // Unslotted!
            }
        }
        if let Some(NodeData::ShadowRoot { host, .. }) = doc.get(p) {
            return Some(*host);
        }
        Some(p)
    } else {
        None
    }
}

fn check_visibility_impl(
    host: &SharedHost,
    nid: NodeId,
    check_opacity: bool,
    check_vis_css: bool,
    check_cv_auto: bool,
) -> bool {
    {
        let mut hb = host.borrow_mut();
        let _ = hb.ensure_layout();
    }
    let (is_connected, self_tag, flat_chain, in_closed_details, is_listbox_opt) = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        if !doc.is_connected(nid) {
            return false;
        }
        let self_tag = match doc.get(nid) {
            Some(NodeData::Element(el)) => el.name.local.as_str().to_ascii_lowercase(),
            _ => return false,
        };
        // Walk flat tree up to document root; if unslotted light-DOM child of shadow host, returns None.
        let mut chain = vec![nid];
        let mut cur = nid;
        let mut reached_doc = false;
        let mut closed_details = false;
        while let Some(p) = flat_tree_parent(&doc, cur) {
            match doc.get(p) {
                Some(NodeData::Document) => {
                    reached_doc = true;
                    break;
                }
                Some(NodeData::Element(pel)) => {
                    if pel.name.local.as_str().eq_ignore_ascii_case("details")
                        && pel.get_attribute("open").is_none()
                    {
                        // Only the first `<summary>` direct child of `<details>` is rendered when closed.
                        let first_summary = doc.children(p).find(|&c| {
                            matches!(
                                doc.get(c),
                                Some(NodeData::Element(ce)) if ce.name.local.as_str().eq_ignore_ascii_case("summary")
                            )
                        });
                        if first_summary != Some(cur) {
                            closed_details = true;
                        }
                    }
                    chain.push(p);
                    cur = p;
                }
                _ => {
                    cur = p;
                }
            }
        }
        if !reached_doc {
            return false;
        }
        let mut listbox_opt = false;
        if self_tag == "option" || self_tag == "optgroup" {
            for &anc in chain.iter().skip(1) {
                if let Some(NodeData::Element(el)) = doc.get(anc) {
                    if el.name.local.as_str().eq_ignore_ascii_case("select") {
                        let is_multi = el.get_attribute("multiple").is_some();
                        let size_gt1 = el
                            .get_attribute("size")
                            .and_then(|s| s.trim().parse::<u32>().ok())
                            .is_some_and(|n| n > 1);
                        if is_multi || size_gt1 {
                            listbox_opt = true;
                        }
                        break;
                    }
                }
            }
        }
        (true, self_tag, chain, closed_details, listbox_opt)
    };
    if !is_connected || in_closed_details {
        return false;
    }

    let vp = {
        let mut hb = host.borrow_mut();
        let owner = hb.owner_doc_of(nid);
        hb.viewport_for_doc(owner)
    };
    let vp_rect = Rect::new(0.0, 0.0, vp.width, vp.height);

    for (idx, &node) in flat_chain.iter().enumerate() {
        let Some(st) = host.borrow_mut().computed_style_for_node(node) else {
            return false;
        };
        if st.display == DisplayMode::None {
            return false;
        }
        if idx == 0 && st.display == DisplayMode::Contents && !is_listbox_opt {
            return false;
        }
        if idx > 0 {
            match st.visual.content_visibility {
                ContentVisibilityMode::Hidden => return false,
                ContentVisibilityMode::Auto if check_cv_auto => {
                    let r = transformed_bounding_rect_for_node(host, node);
                    let intersects = r.max_x() >= vp_rect.origin.x
                        && r.origin.x <= vp_rect.max_x()
                        && r.max_y() >= vp_rect.origin.y
                        && r.origin.y <= vp_rect.max_y();
                    if !intersects {
                        return false;
                    }
                }
                _ => {}
            }
        }
        if check_opacity && st.opacity <= 0.0 {
            return false;
        }
        if idx == 0 && check_vis_css && st.visibility != VisibilityMode::Visible {
            return false;
        }
        if idx == 0 && self_tag == "option" {
            let hidden = matches!(
                host.borrow().document.borrow().get(node),
                Some(NodeData::Element(el)) if el.get_attribute("hidden").is_some()
            );
            if hidden {
                return false;
            }
        }
    }
    true
}

fn current_css_zoom_impl(host: &SharedHost, nid: NodeId) -> f64 {
    let chain = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        if !doc.is_connected(nid) {
            return 1.0;
        }
        let mut chain = vec![nid];
        let mut cur = nid;
        while let Some(p) = flat_tree_parent(&doc, cur) {
            if matches!(doc.get(p), Some(NodeData::Element(_))) {
                chain.push(p);
            }
            cur = p;
        }
        chain
    };
    let mut product = 1.0f64;
    for node in chain {
        if let Some(st) = host.borrow_mut().computed_style_for_node(node) {
            if st.display == DisplayMode::None {
                return 1.0;
            }
            product *= f64::from(st.visual.zoom);
        }
    }
    product
}

fn is_scroll_container_style(st: &ComputedStyle) -> bool {
    matches!(
        st.overflow.x,
        OverflowMode::Scroll | OverflowMode::Auto | OverflowMode::Hidden
    ) || matches!(
        st.overflow.y,
        OverflowMode::Scroll | OverflowMode::Auto | OverflowMode::Hidden
    )
}

fn establishes_fixed_cb(st: &ComputedStyle) -> bool {
    if let Some(fx) = st.effects.as_deref() {
        if !fx.transform.is_empty()
            || !matches!(fx.translate, rix_css::values_math::TranslateSpec::None)
            || !matches!(fx.rotate, rix_css::values_math::RotateSpec::None)
            || !matches!(fx.scale, rix_css::values_math::ScaleSpec::None)
            || !fx.filter.is_empty()
            || (fx.contain & (0x04 | 0x10)) != 0
        {
            return true;
        }
    }
    false
}

fn scroll_parent_for_node(host: &SharedHost, nid: NodeId) -> Option<NodeId> {
    let owner_doc = host.borrow().owner_doc_of(nid);
    let doc_el = host
        .borrow()
        .document
        .borrow()
        .document_element_of(owner_doc);
    let scrolling_el = host.borrow_mut().scrolling_element_of(owner_doc);
    if doc_el == Some(nid) || scrolling_el == Some(nid) {
        return None;
    }
    let self_style = host.borrow_mut().computed_style_for_node(nid)?;
    if self_style.display == DisplayMode::None || self_style.display == DisplayMode::Contents {
        return None;
    }
    let connected = host.borrow().document.borrow().is_connected(nid);
    if !connected {
        return None;
    }

    // Collect flat-tree element ancestors.
    let ancestors: Vec<NodeId> = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        let mut out = Vec::new();
        let mut cur = nid;
        while let Some(p) = flat_tree_parent(&doc, cur) {
            if matches!(doc.get(p), Some(NodeData::Element(_))) {
                out.push(p);
            }
            cur = p;
        }
        out
    };

    // Determine starting ancestor in the containing-block chain for abspos / fixedpos.
    let start_idx = match self_style.position {
        PositionMode::Fixed => {
            let mut found = None;
            for (i, &anc) in ancestors.iter().enumerate() {
                if let Some(ast) = host.borrow_mut().computed_style_for_node(anc) {
                    if ast.display != DisplayMode::Contents && establishes_fixed_cb(&ast) {
                        found = Some(i);
                        break;
                    }
                }
            }
            // Fixed to initial containing block (root viewport): scrollParent is `null`!
            found?
        }
        PositionMode::Absolute => {
            let mut found = None;
            for (i, &anc) in ancestors.iter().enumerate() {
                if let Some(ast) = host.borrow_mut().computed_style_for_node(anc) {
                    if ast.display != DisplayMode::Contents
                        && (ast.position != PositionMode::Static || establishes_fixed_cb(&ast))
                    {
                        found = Some(i);
                        break;
                    }
                }
            }
            found.unwrap_or(ancestors.len())
        }
        _ => 0,
    };

    let root_overflow_visible = doc_el
        .and_then(|de| host.borrow_mut().computed_style_for_node(de))
        .is_some_and(|st| {
            st.overflow.x == OverflowMode::Visible && st.overflow.y == OverflowMode::Visible
        });

    for &anc in ancestors.iter().skip(start_idx) {
        if doc_el == Some(anc) || scrolling_el == Some(anc) {
            continue;
        }
        // CSS Overflow L3 §3.1: when the root element has `overflow: visible`, `<body>`'s
        // overflow propagates to the viewport and `<body>` itself is NOT a scroll container.
        if root_overflow_visible {
            let is_body = matches!(
                host.borrow().document.borrow().get(anc),
                Some(NodeData::Element(el)) if el.name.local.as_str().eq_ignore_ascii_case("body")
            );
            if is_body {
                continue;
            }
        }
        if let Some(ast) = host.borrow_mut().computed_style_for_node(anc) {
            if ast.display == DisplayMode::Contents {
                continue;
            }
            if is_scroll_container_style(&ast) {
                return Some(anc);
            }
        }
    }
    scrolling_el
}

// ---------------------------------------------------------------------------
// Scroll Promises, Smooth Scroll rAF Driver, and Full scrollIntoView
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollBehaviorOpt {
    Auto,
    Instant,
    Smooth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollLogicalPos {
    Start,
    Center,
    End,
    Nearest,
}

fn make_scroll_result_obj(vm: &mut JsVm, interrupted: bool) -> JsValue {
    let o = vm.alloc_ordinary_object();
    let k = vm.heap.intern_key("interrupted");
    vm.heap.object_define_data(
        o,
        k,
        JsValue::from_bool(interrupted),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    JsValue::from_object(o)
}

fn make_resolved_scroll_promise(vm: &mut JsVm, interrupted: bool) -> JsValue {
    let p = alloc_promise(vm);
    let res = make_scroll_result_obj(vm, interrupted);
    resolve_promise(vm, p, res);
    JsValue::from_object(p)
}

fn make_rejected_type_error_promise(vm: &mut JsVm, msg: &str) -> JsValue {
    let p = alloc_promise(vm);
    let err = vm.type_error(msg);
    reject_promise(vm, p, err.value);
    JsValue::from_object(p)
}

fn scroll_key_for_node(host: &SharedHost, nid: NodeId) -> ScrollTargetKey {
    let owner_doc = host.borrow().owner_doc_of(nid);
    let scroll_el = host.borrow_mut().scrolling_element_of(owner_doc);
    if scroll_el == Some(nid) {
        ScrollTargetKey::Window(owner_doc)
    } else {
        ScrollTargetKey::Element(nid)
    }
}

fn interrupt_smooth_scroll_by_key(vm: &mut JsVm, host: &SharedHost, key: ScrollTargetKey) {
    let old = host
        .borrow()
        .cssom_view_state
        .borrow_mut()
        .active_smooth_scrolls
        .remove(&key);
    if let Some(mut act) = old {
        if !act.cancelled {
            act.cancelled = true;
            let res = make_scroll_result_obj(vm, true);
            resolve_promise(vm, act.promise, res);
        }
    }
}

/// Cancels any active smooth scroll on `nid` (or viewport when `nid` is `scrollingElement`),
/// resolving its pending promise with `{ interrupted: true }`.
pub(crate) fn interrupt_smooth_scroll_for_node(vm: &mut JsVm, host: &SharedHost, nid: NodeId) {
    let key = scroll_key_for_node(host, nid);
    interrupt_smooth_scroll_by_key(vm, host, key);
}

fn dispatch_scroll_event(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    key: ScrollTargetKey,
    ev_type: &str,
) {
    let target_obj = match key {
        ScrollTargetKey::Window(doc_id) => {
            let main_root = host.borrow().document.borrow().root();
            if doc_id == main_root {
                vm.global
            } else if let Some(dv) = host
                .borrow()
                .doc_meta
                .get(&doc_id)
                .and_then(|m| m.default_view)
            {
                dv
            } else if let Some(el) = wrap_node(vm, host, doc_id).as_object() {
                el
            } else {
                vm.global
            }
        }
        ScrollTargetKey::Element(nid) => {
            let Some(el) = wrap_node(vm, host, nid).as_object() else {
                return;
            };
            el
        }
    };
    let ev_proto = events.borrow().event_proto.unwrap_or(vm.object_proto);
    let ev_h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(ev_proto)));
    let bubbles = matches!(key, ScrollTargetKey::Window(_));
    crate::events::register_constructed_event(
        events,
        ev_h,
        ev_type.to_string(),
        bubbles,
        false,
        false,
    );
    let _ = event_loop;
    let k_de = vm.heap.intern_key("dispatchEvent");
    if let Ok(de_v) = vm.get_property_by_key(target_obj, k_de) {
        if let Some(de_fn) = de_v.as_object() {
            let _ = vm.call_value(
                JsValue::from_object(de_fn),
                JsValue::from_object(target_obj),
                &[JsValue::from_object(ev_h)],
            );
        }
    }
}

fn current_scroll_for_key(host: &SharedHost, key: ScrollTargetKey) -> Point {
    let mut hb = host.borrow_mut();
    match key {
        ScrollTargetKey::Window(doc_id) => {
            let main_root = hb.document.borrow().root();
            if doc_id == main_root {
                hb.scroll_offset
            } else {
                hb.subdoc_scroll_offsets
                    .get(&doc_id)
                    .copied()
                    .unwrap_or(Point::ZERO)
            }
        }
        ScrollTargetKey::Element(nid) => hb.scroll_offset_for_node(nid),
    }
}

fn clamp_scroll_for_key(
    host: &SharedHost,
    key: ScrollTargetKey,
    x: Option<f32>,
    y: Option<f32>,
    relative: bool,
) -> Point {
    let cur = current_scroll_for_key(host, key);
    let mut hb = host.borrow_mut();
    let target_node = match key {
        ScrollTargetKey::Window(doc_id) => hb.scrolling_element_of(doc_id),
        ScrollTargetKey::Element(nid) => Some(nid),
    };
    let Some(node) = target_node else {
        let nx = x
            .map(|v| if relative { cur.x + v } else { v })
            .unwrap_or(cur.x)
            .max(0.0);
        let ny = y
            .map(|v| if relative { cur.y + v } else { v })
            .unwrap_or(cur.y)
            .max(0.0);
        return Point::new(nx, ny);
    };
    let (cw, ch) = hb.client_size_for_node(node);
    let (sw, sh) = hb.scroll_size_for_node(node);
    let max_x = (sw - cw).max(0.0);
    let max_y = (sh - ch).max(0.0);
    let info_opt = hb.element_geometry_for_node(node);
    let (neg_x, neg_y) = info_opt.map_or((false, false), |i| (i.scroll_neg_x, i.scroll_neg_y));
    let is_viewport = matches!(key, ScrollTargetKey::Window(_));
    let (can_x, can_y) = if is_viewport {
        (true, true)
    } else if let Some(info) = info_opt {
        (
            rix_layout::is_scroll_axis(info.overflow_x),
            rix_layout::is_scroll_axis(info.overflow_y),
        )
    } else {
        (false, false)
    };
    let rx = x
        .map(|v| if relative { cur.x + v } else { v })
        .unwrap_or(cur.x);
    let ry = y
        .map(|v| if relative { cur.y + v } else { v })
        .unwrap_or(cur.y);
    let nx = if !can_x {
        cur.x
    } else if neg_x {
        rx.clamp(-max_x, 0.0)
    } else {
        rx.clamp(0.0, max_x)
    };
    let ny = if !can_y {
        cur.y
    } else if neg_y {
        ry.clamp(-max_y, 0.0)
    } else {
        ry.clamp(0.0, max_y)
    };
    Point::new(nx, ny)
}

fn apply_scroll_for_key(host: &SharedHost, key: ScrollTargetKey, dest: Point) -> bool {
    let mut hb = host.borrow_mut();
    match key {
        ScrollTargetKey::Window(doc_id) => {
            hb.set_doc_scroll(doc_id, Some(dest.x), Some(dest.y), false)
        }
        ScrollTargetKey::Element(nid) => {
            hb.set_scroll_offset_for_node(nid, Some(dest.x), Some(dest.y), false)
        }
    }
}

fn effective_scroll_behavior(
    host: &SharedHost,
    key: ScrollTargetKey,
    requested: ScrollBehaviorOpt,
) -> bool {
    match requested {
        ScrollBehaviorOpt::Smooth => true,
        ScrollBehaviorOpt::Instant => false,
        ScrollBehaviorOpt::Auto => {
            let mut hb = host.borrow_mut();
            let node_opt = match key {
                ScrollTargetKey::Window(doc_id) => hb.document.borrow().document_element_of(doc_id),
                ScrollTargetKey::Element(nid) => Some(nid),
            };
            node_opt
                .and_then(|n| hb.computed_style_for_node(n))
                .is_some_and(|st| st.visual.scroll_behavior == ScrollBehaviorMode::Smooth)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_scroll_with_promise(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    key: ScrollTargetKey,
    x_opt: Option<f32>,
    y_opt: Option<f32>,
    relative: bool,
    requested_behavior: ScrollBehaviorOpt,
) -> JsValue {
    interrupt_smooth_scroll_by_key(vm, host, key);
    let cur = current_scroll_for_key(host, key);
    let dest = clamp_scroll_for_key(host, key, x_opt, y_opt, relative);
    let dist = (dest.x - cur.x).abs() + (dest.y - cur.y).abs();
    let is_smooth = effective_scroll_behavior(host, key, requested_behavior);

    if !is_smooth || dist < 1e-3 {
        let changed = apply_scroll_for_key(host, key, dest);
        if changed {
            dispatch_scroll_event(vm, host, events, event_loop, key, "scroll");
            dispatch_scroll_event(vm, host, events, event_loop, key, "scrollend");
        }
        return make_resolved_scroll_promise(vm, false);
    }

    // Smooth scroll: advance incrementally across 2 rAF ticks so `scrollTop` stays < target
    // synchronously, `observeScrolling` detects intermediate movement, and any overlapping
    // scroll call cancels this animation and resolves its promise with `{ interrupted: true }`.
    let promise = alloc_promise(vm);
    let scroll_id = {
        let st = host.borrow();
        let mut cv = st.cssom_view_state.borrow_mut();
        cv.next_scroll_id += 1;
        let id = cv.next_scroll_id;
        cv.active_smooth_scrolls.insert(
            key,
            ActiveSmoothScroll {
                id,
                promise,
                cancelled: false,
            },
        );
        id
    };
    let mid = Point::new((cur.x + dest.x) * 0.5, (cur.y + dest.y) * 0.5);
    let h1 = Rc::clone(host);
    let ev1 = Rc::clone(events);
    let el1 = Rc::clone(event_loop);
    let cb1 = vm.alloc_host_fn("smooth_scroll_step1", move |vm, _, _| {
        let active = h1
            .borrow()
            .cssom_view_state
            .borrow()
            .active_smooth_scrolls
            .get(&key)
            .is_some_and(|a| a.id == scroll_id && !a.cancelled);
        if !active {
            return Ok(JsValue::UNDEFINED);
        }
        if apply_scroll_for_key(&h1, key, mid) {
            dispatch_scroll_event(vm, &h1, &ev1, &el1, key, "scroll");
        }
        let h2 = Rc::clone(&h1);
        let ev2 = Rc::clone(&ev1);
        let el2 = Rc::clone(&el1);
        let cb2 = vm.alloc_host_fn("smooth_scroll_step2", move |vm, _, _| {
            let removed = {
                let st = h2.borrow();
                let mut cv = st.cssom_view_state.borrow_mut();
                if cv
                    .active_smooth_scrolls
                    .get(&key)
                    .is_some_and(|a| a.id == scroll_id && !a.cancelled)
                {
                    cv.active_smooth_scrolls.remove(&key)
                } else {
                    None
                }
            };
            if let Some(act) = removed {
                if apply_scroll_for_key(&h2, key, dest) {
                    dispatch_scroll_event(vm, &h2, &ev2, &el2, key, "scroll");
                    dispatch_scroll_event(vm, &h2, &ev2, &el2, key, "scrollend");
                }
                let res = make_scroll_result_obj(vm, false);
                resolve_promise(vm, act.promise, res);
            }
            Ok(JsValue::UNDEFINED)
        });
        el1.borrow_mut().request_animation_frame(cb2);
        Ok(JsValue::UNDEFINED)
    });
    event_loop.borrow_mut().request_animation_frame(cb1);
    JsValue::from_object(promise)
}

fn parse_scroll_to_args(
    vm: &mut JsVm,
    args: &[JsValue],
) -> Result<(Option<f32>, Option<f32>, ScrollBehaviorOpt), String> {
    if args.is_empty() {
        return Ok((None, None, ScrollBehaviorOpt::Auto));
    }
    if args.len() == 1 {
        let first = args[0];
        if first.is_undefined() || first.is_null() {
            return Ok((None, None, ScrollBehaviorOpt::Auto));
        }
        let Some(obj) = first.as_object() else {
            return Err("Failed to execute scroll: parameter 1 is not an object".to_string());
        };
        let mut x_opt = None;
        let mut y_opt = None;
        let mut behavior = ScrollBehaviorOpt::Auto;
        let k_left = vm.heap.intern_key("left");
        if let Ok(v) = vm.get_property_by_key(obj, k_left) {
            if !v.is_undefined() {
                if let Ok(n) = vm.to_number(v) {
                    if n.is_finite() {
                        x_opt = Some(n as f32);
                    }
                }
            }
        }
        let k_top = vm.heap.intern_key("top");
        if let Ok(v) = vm.get_property_by_key(obj, k_top) {
            if !v.is_undefined() {
                if let Ok(n) = vm.to_number(v) {
                    if n.is_finite() {
                        y_opt = Some(n as f32);
                    }
                }
            }
        }
        let k_beh = vm.heap.intern_key("behavior");
        if let Ok(v) = vm.get_property_by_key(obj, k_beh) {
            if !v.is_undefined() {
                let s = vm.value_to_rust_string(v);
                behavior = match s.as_str() {
                    "auto" => ScrollBehaviorOpt::Auto,
                    "instant" => ScrollBehaviorOpt::Instant,
                    "smooth" => ScrollBehaviorOpt::Smooth,
                    _ => {
                        return Err(format!(
                            "The provided value '{s}' is not a valid enum value of type ScrollBehavior."
                        ));
                    }
                };
            }
        }
        return Ok((x_opt, y_opt, behavior));
    }
    let nx = vm.to_number(args[0]).unwrap_or(f64::NAN);
    let ny = vm.to_number(args[1]).unwrap_or(f64::NAN);
    let x_opt = nx.is_finite().then_some(nx as f32);
    let y_opt = ny.is_finite().then_some(ny as f32);
    Ok((x_opt, y_opt, ScrollBehaviorOpt::Auto))
}

fn align_axis_delta(
    el_lo: f32,
    el_hi: f32,
    view_lo: f32,
    view_hi: f32,
    pos: ScrollLogicalPos,
    reversed: bool,
) -> f32 {
    let eff_pos = if reversed {
        match pos {
            ScrollLogicalPos::Start => ScrollLogicalPos::End,
            ScrollLogicalPos::End => ScrollLogicalPos::Start,
            other => other,
        }
    } else {
        pos
    };
    match eff_pos {
        ScrollLogicalPos::Start => el_lo - view_lo,
        ScrollLogicalPos::End => el_hi - view_hi,
        ScrollLogicalPos::Center => (el_lo + el_hi) * 0.5 - (view_lo + view_hi) * 0.5,
        ScrollLogicalPos::Nearest => {
            if (el_lo >= view_lo && el_hi <= view_hi) || (el_lo < view_lo && el_hi > view_hi) {
                0.0
            } else if el_lo < view_lo {
                el_lo - view_lo
            } else {
                el_hi - view_hi
            }
        }
    }
}

fn execute_scroll_into_view(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    nid: NodeId,
    args: &[JsValue],
) -> JsValue {
    let mut block_pos = ScrollLogicalPos::Start;
    let mut inline_pos = ScrollLogicalPos::Nearest;
    let mut behavior = ScrollBehaviorOpt::Auto;
    let mut nearest_only = false;

    if let Some(&first) = args.first() {
        if first.is_undefined() || first.is_null() {
            // default: block=start, inline=nearest
        } else if let Some(obj) = first.as_object() {
            let parse_pos = |vm: &mut JsVm,
                             key: &str,
                             default: ScrollLogicalPos|
             -> Result<ScrollLogicalPos, String> {
                let k = vm.heap.intern_key(key);
                let v = vm.get_property_by_key(obj, k).unwrap_or(JsValue::UNDEFINED);
                if v.is_undefined() {
                    return Ok(default);
                }
                match vm.value_to_rust_string(v).as_str() {
                    "start" => Ok(ScrollLogicalPos::Start),
                    "center" => Ok(ScrollLogicalPos::Center),
                    "end" => Ok(ScrollLogicalPos::End),
                    "nearest" => Ok(ScrollLogicalPos::Nearest),
                    other => Err(format!("Invalid ScrollLogicalPosition '{other}'")),
                }
            };
            match parse_pos(vm, "block", ScrollLogicalPos::Start) {
                Ok(p) => block_pos = p,
                Err(e) => return make_rejected_type_error_promise(vm, &e),
            }
            match parse_pos(vm, "inline", ScrollLogicalPos::Nearest) {
                Ok(p) => inline_pos = p,
                Err(e) => return make_rejected_type_error_promise(vm, &e),
            }
            let k_beh = vm.heap.intern_key("behavior");
            if let Ok(v) = vm.get_property_by_key(obj, k_beh) {
                if !v.is_undefined() {
                    match vm.value_to_rust_string(v).as_str() {
                        "auto" => behavior = ScrollBehaviorOpt::Auto,
                        "instant" => behavior = ScrollBehaviorOpt::Instant,
                        "smooth" => behavior = ScrollBehaviorOpt::Smooth,
                        other => {
                            return make_rejected_type_error_promise(
                                vm,
                                &format!("Invalid ScrollBehavior '{other}'"),
                            );
                        }
                    }
                }
            }
            let k_cnt = vm.heap.intern_key("container");
            if let Ok(v) = vm.get_property_by_key(obj, k_cnt) {
                if !v.is_undefined() {
                    match vm.value_to_rust_string(v).as_str() {
                        "all" => nearest_only = false,
                        "nearest" => nearest_only = true,
                        other => {
                            return make_rejected_type_error_promise(
                                vm,
                                &format!("Invalid ScrollIntoViewContainer '{other}'"),
                            );
                        }
                    }
                }
            }
        } else if !vm.to_boolean(first) {
            block_pos = ScrollLogicalPos::End;
            inline_pos = ScrollLogicalPos::Nearest;
        }
    }

    // Walk ancestor scroll containers from nearest to root viewport.
    let owner_doc = host.borrow().owner_doc_of(nid);
    let scrolling_el = host.borrow_mut().scrolling_element_of(owner_doc);
    let mut containers: Vec<NodeId> = Vec::new();
    let mut cur = nid;
    while let Some(sp) = scroll_parent_for_node(host, cur) {
        containers.push(sp);
        if nearest_only || Some(sp) == scrolling_el {
            break;
        }
        cur = sp;
    }
    if containers.is_empty() {
        return make_resolved_scroll_promise(vm, false);
    }

    let mut primary_promise: Option<JsValue> = None;
    for sc in containers {
        let el_rect = transformed_bounding_rect_for_node(host, nid);
        if el_rect.size.width == 0.0 && el_rect.size.height == 0.0 {
            let has_box = !collect_boxes_for_node(host, nid).is_empty();
            if !has_box {
                break;
            }
        }
        let is_viewport = Some(sc) == scrolling_el;
        let view_rect = if is_viewport {
            let vp = host.borrow_mut().viewport_for_doc(owner_doc);
            Rect::new(0.0, 0.0, vp.width, vp.height)
        } else if let Some(info) = host.borrow_mut().element_geometry_for_node(sc) {
            Rect::new(
                info.abs_padding.origin.x - info.ancestor_scroll.x,
                info.abs_padding.origin.y - info.ancestor_scroll.y,
                info.abs_padding.size.width,
                info.abs_padding.size.height,
            )
        } else {
            continue;
        };

        let (wm, dir) = host
            .borrow_mut()
            .computed_style_for_node(sc)
            .map_or((WritingMode::HorizontalTb, DirectionMode::Ltr), |st| {
                (st.text.writing_mode, st.text.direction)
            });
        let (dx, dy) = match wm {
            WritingMode::HorizontalTb => {
                let x_rev = dir == DirectionMode::Rtl;
                let dx = align_axis_delta(
                    el_rect.origin.x,
                    el_rect.max_x(),
                    view_rect.origin.x,
                    view_rect.max_x(),
                    inline_pos,
                    x_rev,
                );
                let dy = align_axis_delta(
                    el_rect.origin.y,
                    el_rect.max_y(),
                    view_rect.origin.y,
                    view_rect.max_y(),
                    block_pos,
                    false,
                );
                (dx, dy)
            }
            WritingMode::VerticalRl | WritingMode::SidewaysRl => {
                // Block axis = X (right-to-left => reversed), Inline axis = Y (top-to-bottom in LTR, bottom-to-top in RTL)
                let y_rev = dir == DirectionMode::Rtl;
                let dx = align_axis_delta(
                    el_rect.origin.x,
                    el_rect.max_x(),
                    view_rect.origin.x,
                    view_rect.max_x(),
                    block_pos,
                    true,
                );
                let dy = align_axis_delta(
                    el_rect.origin.y,
                    el_rect.max_y(),
                    view_rect.origin.y,
                    view_rect.max_y(),
                    inline_pos,
                    y_rev,
                );
                (dx, dy)
            }
            WritingMode::VerticalLr => {
                let y_rev = dir == DirectionMode::Rtl;
                let dx = align_axis_delta(
                    el_rect.origin.x,
                    el_rect.max_x(),
                    view_rect.origin.x,
                    view_rect.max_x(),
                    block_pos,
                    false,
                );
                let dy = align_axis_delta(
                    el_rect.origin.y,
                    el_rect.max_y(),
                    view_rect.origin.y,
                    view_rect.max_y(),
                    inline_pos,
                    y_rev,
                );
                (dx, dy)
            }
            WritingMode::SidewaysLr => {
                // Block axis = X (left-to-right), Inline axis = Y (bottom-to-top in LTR, top-to-bottom in RTL)
                let y_rev = dir == DirectionMode::Ltr;
                let dx = align_axis_delta(
                    el_rect.origin.x,
                    el_rect.max_x(),
                    view_rect.origin.x,
                    view_rect.max_x(),
                    block_pos,
                    false,
                );
                let dy = align_axis_delta(
                    el_rect.origin.y,
                    el_rect.max_y(),
                    view_rect.origin.y,
                    view_rect.max_y(),
                    inline_pos,
                    y_rev,
                );
                (dx, dy)
            }
        };
        let key = if is_viewport {
            ScrollTargetKey::Window(owner_doc)
        } else {
            ScrollTargetKey::Element(sc)
        };
        let cur_sc = current_scroll_for_key(host, key);
        let clamped_dest = clamp_scroll_for_key(host, key, Some(dx), Some(dy), true);
        if (clamped_dest.x - cur_sc.x).abs() + (clamped_dest.y - cur_sc.y).abs() < 1e-3 {
            continue;
        }
        let p = execute_scroll_with_promise(
            vm,
            host,
            events,
            event_loop,
            key,
            Some(dx),
            Some(dy),
            true,
            behavior,
        );
        if primary_promise.is_none() {
            primary_promise = Some(p);
        }
    }
    primary_promise.unwrap_or_else(|| make_resolved_scroll_promise(vm, false))
}

// ---------------------------------------------------------------------------
// MediaQueryList, MediaQueryListEvent, matchMedia, Screen
// ---------------------------------------------------------------------------

fn normalize_match_media_query(raw: &str) -> String {
    if raw.trim().is_empty() {
        return String::new();
    }
    let parts = crate::cssom::parse_media_query_list(raw);
    let cleaned: Vec<String> = parts
        .into_iter()
        .map(|p| {
            let low = p.to_ascii_lowercase();
            if let Some(rest) = low.strip_prefix("all and ") {
                p[p.len() - rest.len()..].to_string()
            } else {
                p
            }
        })
        .collect();
    cleaned.join(", ")
}

fn evaluate_mql_for_doc(
    host: &SharedHost,
    doc_id: NodeId,
    raw_query: &str,
    normalized: &str,
) -> bool {
    let trimmed = raw_query.trim();
    if trimmed.is_empty() {
        return true;
    }
    if normalized == "not all" {
        return false;
    }
    let (vp, dark, dpr) = {
        let mut hb = host.borrow_mut();
        (
            hb.viewport_for_doc(doc_id),
            hb.dark_mode,
            hb.device_pixel_ratio,
        )
    };
    let media = MediaContext {
        viewport_width: vp.width,
        viewport_height: vp.height,
        dark_mode: dark,
        dpi: dpr * 96.0,
        ..MediaContext::default()
    };
    media.evaluate_media_query_str(normalized)
}

#[allow(clippy::too_many_arguments)]
fn alloc_mql_event(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    ev_type: &str,
    media: &str,
    matches: bool,
    bubbles: bool,
    cancelable: bool,
    composed: bool,
) -> HeapHandle {
    let proto = host
        .borrow()
        .cssom_view_state
        .borrow()
        .mql_event_proto
        .unwrap_or(vm.object_proto);
    let ev_h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    crate::events::register_constructed_event(
        events,
        ev_h,
        ev_type.to_string(),
        bubbles,
        cancelable,
        composed,
    );
    let k_media = vm.heap.intern_key("media");
    let k_matches = vm.heap.intern_key("matches");
    let ms = JsValue::from_string(vm.heap.alloc_string(media));
    vm.heap.object_define_data(
        ev_h,
        k_media,
        ms,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        ev_h,
        k_matches,
        JsValue::from_bool(matches),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    ev_h
}

pub(crate) fn create_media_query_list_for_doc(
    vm: &mut JsVm,
    host: &SharedHost,
    doc_id: NodeId,
    raw_query: &str,
) -> HeapHandle {
    let normalized = normalize_match_media_query(raw_query);
    let matches = evaluate_mql_for_doc(host, doc_id, raw_query, &normalized);
    let proto = host
        .borrow()
        .cssom_view_state
        .borrow()
        .mql_proto
        .unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    host.borrow()
        .cssom_view_state
        .borrow_mut()
        .media_query_lists
        .push(MqlRecord {
            handle: obj,
            owner_doc: doc_id,
            raw_query: raw_query.to_string(),
            normalized_media: normalized,
            last_matches: matches,
            onchange: JsValue::NULL,
            onchange_wrapper: None,
        });
    obj
}

fn check_mql_changes(vm: &mut JsVm, host: &SharedHost, events: &SharedEvents) {
    let snapshot: Vec<(usize, HeapHandle, NodeId, String, String, bool)> = {
        let st = host.borrow();
        let mut cv = st.cssom_view_state.borrow_mut();
        cv.mql_check_scheduled = false;
        cv.media_query_lists
            .iter()
            .enumerate()
            .map(|(i, m)| {
                (
                    i,
                    m.handle,
                    m.owner_doc,
                    m.raw_query.clone(),
                    m.normalized_media.clone(),
                    m.last_matches,
                )
            })
            .collect()
    };
    let mut to_fire = Vec::new();
    for (idx, handle, doc_id, raw_q, norm, prev) in snapshot {
        let now = evaluate_mql_for_doc(host, doc_id, &raw_q, &norm);
        if now != prev {
            if let Some(rec) = host
                .borrow()
                .cssom_view_state
                .borrow_mut()
                .media_query_lists
                .get_mut(idx)
            {
                if rec.handle == handle {
                    rec.last_matches = now;
                }
            }
            to_fire.push((handle, norm, now));
        }
    }
    for (mql_h, media, matches) in to_fire {
        let ev_h = alloc_mql_event(
            vm, host, events, "change", &media, matches, false, false, false,
        );
        let k_de = vm.heap.intern_key("dispatchEvent");
        if let Ok(de_v) = vm.get_property_by_key(mql_h, k_de) {
            if let Some(de_fn) = de_v.as_object() {
                let _ = vm.call_value(
                    JsValue::from_object(de_fn),
                    JsValue::from_object(mql_h),
                    &[JsValue::from_object(ev_h)],
                );
            }
        }
    }
}

fn install_media_query_and_screen(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
) {
    let etp = host.borrow().prototypes.event_target_proto;
    let ev_proto = events.borrow().event_proto.unwrap_or(vm.object_proto);

    // MediaQueryListEvent extends Event
    let mqle_proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(ev_proto)));
    let h_mqle = Rc::clone(host);
    let ev_mqle = Rc::clone(events);
    let mqle_ctor = alloc_host_ctor(vm, "MediaQueryListEvent", move |vm, _, args| {
        if args.is_empty() {
            return Err(vm.type_error("MediaQueryListEvent requires a type argument"));
        }
        let ev_type = vm.value_to_rust_string(args[0]);
        let (mut media, mut matches, mut bubbles, mut cancelable, mut composed) =
            (String::new(), false, false, false, false);
        if let Some(oh) = arg(args, 1).as_object() {
            let k_m = vm.heap.intern_key("media");
            if let Ok(v) = vm.get_property_by_key(oh, k_m) {
                if !v.is_undefined() {
                    media = vm.value_to_rust_string(v);
                }
            }
            let k_ma = vm.heap.intern_key("matches");
            if let Ok(v) = vm.get_property_by_key(oh, k_ma) {
                matches = vm.to_boolean(v);
            }
            let k_b = vm.heap.intern_key("bubbles");
            if let Ok(v) = vm.get_property_by_key(oh, k_b) {
                bubbles = vm.to_boolean(v);
            }
            let k_c = vm.heap.intern_key("cancelable");
            if let Ok(v) = vm.get_property_by_key(oh, k_c) {
                cancelable = vm.to_boolean(v);
            }
            let k_co = vm.heap.intern_key("composed");
            if let Ok(v) = vm.get_property_by_key(oh, k_co) {
                composed = vm.to_boolean(v);
            }
        }
        let h = alloc_mql_event(
            vm, &h_mqle, &ev_mqle, &ev_type, &media, matches, bubbles, cancelable, composed,
        );
        Ok(JsValue::from_object(h))
    });
    let k_p = vm.heap.intern_key("prototype");
    let k_c = vm.heap.intern_key("constructor");
    vm.heap.object_define_data(
        mqle_ctor,
        k_p,
        JsValue::from_object(mqle_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        mqle_proto,
        k_c,
        JsValue::from_object(mqle_ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    define_global(vm, "MediaQueryListEvent", JsValue::from_object(mqle_ctor));

    // MediaQueryList extends EventTarget
    let mql_proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(etp)));
    let h_med = Rc::clone(host);
    define_host_getter(vm, mql_proto, "media", move |vm, t, _| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let s = h_med
            .borrow()
            .cssom_view_state
            .borrow()
            .media_query_lists
            .iter()
            .find(|m| m.handle == oh)
            .map(|m| m.normalized_media.clone())
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });
    let h_mat = Rc::clone(host);
    define_host_getter(vm, mql_proto, "matches", move |vm, t, _| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let rec = h_mat
            .borrow()
            .cssom_view_state
            .borrow()
            .media_query_lists
            .iter()
            .find(|m| m.handle == oh)
            .map(|m| (m.owner_doc, m.raw_query.clone(), m.normalized_media.clone()))
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let m = evaluate_mql_for_doc(&h_mat, rec.0, &rec.1, &rec.2);
        Ok(JsValue::from_bool(m))
    });

    // onchange EventHandler attribute on MediaQueryList.prototype
    let h_og = Rc::clone(host);
    let g_oc = vm.alloc_host_fn("get onchange", move |vm, t, _| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let val = h_og
            .borrow()
            .cssom_view_state
            .borrow()
            .media_query_lists
            .iter()
            .find(|m| m.handle == oh)
            .map(|m| m.onchange)
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(val)
    });
    let h_os = Rc::clone(host);
    let s_oc = vm.alloc_host_fn("set onchange", move |vm, t, args| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let v = arg(args, 0);
        let new_handler = if v.as_object().is_some_and(|ch| vm.is_callable_handle(ch)) {
            v
        } else {
            JsValue::NULL
        };
        let (old_wrapper, is_valid) = {
            let st = h_os.borrow();
            let mut cv = st.cssom_view_state.borrow_mut();
            if let Some(rec) = cv.media_query_lists.iter_mut().find(|m| m.handle == oh) {
                rec.onchange = new_handler;
                (rec.onchange_wrapper, true)
            } else {
                (None, false)
            }
        };
        if !is_valid {
            return Err(vm.type_error("Illegal invocation"));
        }
        if !new_handler.is_null() && old_wrapper.is_none() {
            let h_cb = Rc::clone(&h_os);
            let wrapper = vm.alloc_host_fn("mql_onchange_wrapper", move |vm, this_v, ev_args| {
                let Some(mh) = this_v.as_object() else {
                    return Ok(JsValue::UNDEFINED);
                };
                let handler = h_cb
                    .borrow()
                    .cssom_view_state
                    .borrow()
                    .media_query_lists
                    .iter()
                    .find(|m| m.handle == mh)
                    .map(|m| m.onchange)
                    .unwrap_or(JsValue::NULL);
                if let Some(fn_h) = handler.as_object() {
                    if vm.is_callable_handle(fn_h) {
                        let _ = vm.call_value(JsValue::from_object(fn_h), this_v, ev_args);
                    }
                }
                Ok(JsValue::UNDEFINED)
            });
            {
                let st = h_os.borrow();
                let mut cv = st.cssom_view_state.borrow_mut();
                if let Some(rec) = cv.media_query_lists.iter_mut().find(|m| m.handle == oh) {
                    rec.onchange_wrapper = Some(wrapper);
                }
            }
            let k_add = vm.heap.intern_key("addEventListener");
            if let Ok(add_v) = vm.get_property_by_key(oh, k_add) {
                if let Some(add_fn) = add_v.as_object() {
                    let change_s = JsValue::from_string(vm.heap.alloc_string("change"));
                    let _ = vm.call_value(
                        JsValue::from_object(add_fn),
                        JsValue::from_object(oh),
                        &[change_s, JsValue::from_object(wrapper), JsValue::FALSE],
                    );
                }
            }
        } else if new_handler.is_null() {
            if let Some(w) = old_wrapper {
                {
                    let st = h_os.borrow();
                    let mut cv = st.cssom_view_state.borrow_mut();
                    if let Some(rec) = cv.media_query_lists.iter_mut().find(|m| m.handle == oh) {
                        rec.onchange_wrapper = None;
                    }
                }
                let k_rm = vm.heap.intern_key("removeEventListener");
                if let Ok(rm_v) = vm.get_property_by_key(oh, k_rm) {
                    if let Some(rm_fn) = rm_v.as_object() {
                        let change_s = JsValue::from_string(vm.heap.alloc_string("change"));
                        let _ = vm.call_value(
                            JsValue::from_object(rm_fn),
                            JsValue::from_object(oh),
                            &[change_s, JsValue::from_object(w), JsValue::FALSE],
                        );
                    }
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, mql_proto, "onchange", Some(g_oc), Some(s_oc));

    // addListener / removeListener directly delegate to EventTarget's ("change", cb, false)!
    define_host_method(vm, mql_proto, "addListener", |vm, t, args| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let cb = arg(args, 0);
        if cb.is_null() || cb.is_undefined() {
            return Ok(JsValue::UNDEFINED);
        }
        let k_add = vm.heap.intern_key("addEventListener");
        if let Ok(add_v) = vm.get_property_by_key(oh, k_add) {
            if let Some(add_fn) = add_v.as_object() {
                let change_s = JsValue::from_string(vm.heap.alloc_string("change"));
                vm.call_value(
                    JsValue::from_object(add_fn),
                    t,
                    &[change_s, cb, JsValue::FALSE],
                )?;
            }
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_method(vm, mql_proto, "removeListener", |vm, t, args| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let cb = arg(args, 0);
        if cb.is_null() || cb.is_undefined() {
            return Ok(JsValue::UNDEFINED);
        }
        let k_rm = vm.heap.intern_key("removeEventListener");
        if let Ok(rm_v) = vm.get_property_by_key(oh, k_rm) {
            if let Some(rm_fn) = rm_v.as_object() {
                let change_s = JsValue::from_string(vm.heap.alloc_string("change"));
                vm.call_value(
                    JsValue::from_object(rm_fn),
                    t,
                    &[change_s, cb, JsValue::FALSE],
                )?;
            }
        }
        Ok(JsValue::UNDEFINED)
    });
    let mql_ctor = vm.alloc_host_fn("MediaQueryList", |vm, _, _| {
        Err(vm.type_error("Illegal constructor"))
    });
    vm.heap.object_define_data(
        mql_ctor,
        k_p,
        JsValue::from_object(mql_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        mql_proto,
        k_c,
        JsValue::from_object(mql_ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    define_global(vm, "MediaQueryList", JsValue::from_object(mql_ctor));

    {
        let st = host.borrow();
        let mut cv = st.cssom_view_state.borrow_mut();
        cv.mql_proto = Some(mql_proto);
        cv.mql_event_proto = Some(mqle_proto);
    }

    // Hook DOM attribute mutations (e.g. `iframe.width = "250"` or `style` change) to check MQLs.
    let h_sink = Rc::clone(host);
    let ev_sink = Rc::clone(events);
    let el_sink = Rc::clone(event_loop);
    let sink: crate::dom_bridge::MutationSink = Rc::new(std::cell::RefCell::new(Box::new(
        move |mut_ev: &DomMutation| {
            let is_iframe_resize = match mut_ev {
                DomMutation::Attributes {
                    target,
                    attribute_name,
                    ..
                } => {
                    let attr_l = attribute_name.to_ascii_lowercase();
                    if attr_l == "width" || attr_l == "height" || attr_l == "style" {
                        matches!(
                            h_sink.borrow().document.borrow().get(*target),
                            Some(NodeData::Element(el)) if el.name.local.as_str().eq_ignore_ascii_case("iframe")
                        )
                    } else {
                        false
                    }
                }
                _ => false,
            };
            if !is_iframe_resize {
                return;
            }
            let should_schedule = {
                let st = h_sink.borrow();
                let mut cv = st.cssom_view_state.borrow_mut();
                if cv.media_query_lists.is_empty() || cv.mql_check_scheduled {
                    false
                } else {
                    cv.mql_check_scheduled = true;
                    true
                }
            };
            if should_schedule {
                let h_t = Rc::clone(&h_sink);
                let ev_t = Rc::clone(&ev_sink);
                el_sink
                    .borrow_mut()
                    .post_host_task(TaskPriority::Rendering, move |vm| {
                        check_mql_changes(vm, &h_t, &ev_t);
                    });
            }
        },
    )));
    host.borrow_mut().mutation_sinks.push(sink);

    // window.matchMedia(query)
    let h_mm = Rc::clone(host);
    let global = vm.global;
    define_host_method(vm, global, "matchMedia", move |vm, _, args| {
        let query = vm.value_to_rust_string(arg(args, 0));
        let main_doc = h_mm.borrow().document.borrow().root();
        let mql = create_media_query_list_for_doc(vm, &h_mm, main_doc, &query);
        Ok(JsValue::from_object(mql))
    });

    // window.screen (CSSOM View §4.2 Screen interface)
    let screen = alloc_screen_object(vm, host);
    define_global(vm, "screen", JsValue::from_object(screen));
}

fn alloc_screen_object(vm: &mut JsVm, host: &SharedHost) -> HeapHandle {
    let screen = vm.alloc_ordinary_object();
    let h1 = Rc::clone(host);
    define_host_getter(vm, screen, "width", move |_, _, _| {
        Ok(JsValue::from_f64(f64::from(h1.borrow().viewport.width)))
    });
    let h2 = Rc::clone(host);
    define_host_getter(vm, screen, "height", move |_, _, _| {
        Ok(JsValue::from_f64(f64::from(h2.borrow().viewport.height)))
    });
    let h3 = Rc::clone(host);
    define_host_getter(vm, screen, "availWidth", move |_, _, _| {
        Ok(JsValue::from_f64(f64::from(h3.borrow().viewport.width)))
    });
    let h4 = Rc::clone(host);
    define_host_getter(vm, screen, "availHeight", move |_, _, _| {
        Ok(JsValue::from_f64(f64::from(h4.borrow().viewport.height)))
    });
    for (k, v) in [
        ("availLeft", 0.0),
        ("availTop", 0.0),
        ("colorDepth", 24.0),
        ("pixelDepth", 24.0),
    ] {
        define_host_getter(vm, screen, k, move |_, _, _| Ok(JsValue::from_f64(v)));
    }
    let orient = vm.alloc_ordinary_object();
    set_f64_prop(vm, orient, "angle", 0.0);
    let kt = vm.heap.intern_key("type");
    let ts = JsValue::from_string(vm.heap.alloc_string("landscape-primary"));
    vm.heap
        .object_define_data(orient, kt, ts, PropertyAttrs::CONFIGURABLE, &mut vm.shapes);
    let ko = vm.heap.intern_key("orientation");
    vm.heap.object_define_data(
        screen,
        ko,
        JsValue::from_object(orient),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    {
        let st = host.borrow();
        let mut cv = st.cssom_view_state.borrow_mut();
        cv.extra_roots.push(screen);
        cv.extra_roots.push(orient);
    }
    screen
}

// ---------------------------------------------------------------------------
// Public Entry Points (Main Window & Same-Origin Child WindowProxy)
// ---------------------------------------------------------------------------

pub fn install_cssom_view(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
) {
    install_media_query_and_screen(vm, host, events, event_loop);
    let ep = host.borrow().prototypes.element_proto;

    // Element.prototype.checkVisibility(options?)
    let h = Rc::clone(host);
    define_host_method(vm, ep, "checkVisibility", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        let (mut check_op, mut check_vis, mut check_cv_auto) = (false, false, false);
        if let Some(oh) = arg(args, 0).as_object() {
            let flag = |vm: &mut JsVm, kname: &str| -> bool {
                let k = vm.heap.intern_key(kname);
                vm.get_property_by_key(oh, k)
                    .is_ok_and(|v| vm.to_boolean(v))
            };
            check_op = flag(vm, "checkOpacity") || flag(vm, "opacityProperty");
            check_vis = flag(vm, "checkVisibilityCSS") || flag(vm, "visibilityProperty");
            check_cv_auto = flag(vm, "contentVisibilityAuto");
        }
        Ok(JsValue::from_bool(check_visibility_impl(
            &h,
            nid,
            check_op,
            check_vis,
            check_cv_auto,
        )))
    });

    // Element.prototype.currentCSSZoom
    let h = Rc::clone(host);
    define_host_getter(vm, ep, "currentCSSZoom", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        Ok(JsValue::from_f64(current_css_zoom_impl(&h, nid)))
    });

    // Element.prototype.scrollParent()
    let h = Rc::clone(host);
    define_host_method(vm, ep, "scrollParent", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        Ok(scroll_parent_for_node(&h, nid).map_or(JsValue::NULL, |p| wrap_node(vm, &h, p)))
    });

    // HTMLImageElement.prototype.{x, y} (CSSOM View §7.2)
    if let Some(&img_proto) = host.borrow().interface_protos.get("HTMLImageElement") {
        for (prop, is_y) in [("x", false), ("y", true)] {
            let h = Rc::clone(host);
            define_host_getter(vm, img_proto, prop, move |vm, t, _| {
                let nid = require_node(vm, &h, t)?;
                let rect = transformed_bounding_rect_for_node(&h, nid);
                let v = if is_y { rect.origin.y } else { rect.origin.x };
                Ok(JsValue::from_f64(f64::from(v.round())))
            });
        }
    }

    // Override Element.prototype.{scrollLeft, scrollTop} setters to honor CSS `scroll-behavior: smooth`
    // while preserving immediate synchronous interruption when `scroll-behavior: auto`.
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
        let hs = Rc::clone(host);
        let evs = Rc::clone(events);
        let el = Rc::clone(event_loop);
        let s = vm.alloc_host_fn(prop, move |vm, t, args| {
            let nid = require_node(vm, &hs, t)?;
            let v = vm.to_number(arg(args, 0))?;
            if v.is_finite() {
                let owner_doc = hs.borrow().owner_doc_of(nid);
                let is_quirks = hs.borrow().is_doc_quirks_mode(owner_doc);
                let doc_el = hs.borrow().document.borrow().document_element_of(owner_doc);
                let scroll_el = hs.borrow_mut().scrolling_element_of(owner_doc);
                if !(is_quirks && doc_el == Some(nid) && scroll_el != Some(nid)) {
                    let key = scroll_key_for_node(&hs, nid);
                    let (x_opt, y_opt) = if is_y {
                        (None, Some(v as f32))
                    } else {
                        (Some(v as f32), None)
                    };
                    let _ = execute_scroll_with_promise(
                        vm,
                        &hs,
                        &evs,
                        &el,
                        key,
                        x_opt,
                        y_opt,
                        false,
                        ScrollBehaviorOpt::Auto,
                    );
                }
            }
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, ep, prop, Some(g), Some(s));
    }

    // Element.prototype.{scroll, scrollTo, scrollBy, scrollIntoView}
    for (name, relative) in [("scroll", false), ("scrollTo", false), ("scrollBy", true)] {
        let h = Rc::clone(host);
        let ev = Rc::clone(events);
        let el = Rc::clone(event_loop);
        define_host_method(vm, ep, name, move |vm, t, args| {
            let nid = require_node(vm, &h, t)?;
            match parse_scroll_to_args(vm, args) {
                Ok((x, y, beh)) => {
                    let key = scroll_key_for_node(&h, nid);
                    Ok(execute_scroll_with_promise(
                        vm, &h, &ev, &el, key, x, y, relative, beh,
                    ))
                }
                Err(msg) => Ok(make_rejected_type_error_promise(vm, &msg)),
            }
        });
    }
    let h = Rc::clone(host);
    let ev = Rc::clone(events);
    let el = Rc::clone(event_loop);
    define_host_method(vm, ep, "scrollIntoView", move |vm, t, args| {
        let nid = require_node(vm, &h, t)?;
        Ok(execute_scroll_into_view(vm, &h, &ev, &el, nid, args))
    });

    // Window.{scroll, scrollTo, scrollBy}
    let global = vm.global;
    for (name, relative) in [("scroll", false), ("scrollTo", false), ("scrollBy", true)] {
        let h = Rc::clone(host);
        let ev = Rc::clone(events);
        let el = Rc::clone(event_loop);
        define_host_method(vm, global, name, move |vm, _, args| {
            let main_doc = h.borrow().document.borrow().root();
            match parse_scroll_to_args(vm, args) {
                Ok((x, y, beh)) => Ok(execute_scroll_with_promise(
                    vm,
                    &h,
                    &ev,
                    &el,
                    ScrollTargetKey::Window(main_doc),
                    x,
                    y,
                    relative,
                    beh,
                )),
                Err(msg) => Ok(make_rejected_type_error_promise(vm, &msg)),
            }
        });
    }
}

/// Installs per-iframe CSSOM View APIs (`innerWidth`, `innerHeight`, `scrollX`/`scrollY`,
/// `scroll`/`scrollTo`/`scrollBy`, `matchMedia`, `screen`, `requestAnimationFrame`) onto a
/// same-origin child `WindowProxy`.
pub(crate) fn install_child_window_cssom_view(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    proxy_h: HeapHandle,
    doc_id: NodeId,
) {
    let h1 = Rc::clone(host);
    define_host_getter(vm, proxy_h, "innerWidth", move |_, _, _| {
        Ok(JsValue::from_f64(f64::from(
            h1.borrow_mut().viewport_for_doc(doc_id).width,
        )))
    });
    let h2 = Rc::clone(host);
    define_host_getter(vm, proxy_h, "innerHeight", move |_, _, _| {
        Ok(JsValue::from_f64(f64::from(
            h2.borrow_mut().viewport_for_doc(doc_id).height,
        )))
    });
    let h3 = Rc::clone(host);
    define_host_getter(vm, proxy_h, "outerWidth", move |_, _, _| {
        Ok(JsValue::from_f64(f64::from(
            h3.borrow_mut().viewport_for_doc(doc_id).width,
        )))
    });
    let h4 = Rc::clone(host);
    define_host_getter(vm, proxy_h, "outerHeight", move |_, _, _| {
        Ok(JsValue::from_f64(f64::from(
            h4.borrow_mut().viewport_for_doc(doc_id).height,
        )))
    });
    for (prop, is_y) in [
        ("scrollX", false),
        ("pageXOffset", false),
        ("scrollY", true),
        ("pageYOffset", true),
    ] {
        let h = Rc::clone(host);
        define_host_getter(vm, proxy_h, prop, move |_, _, _| {
            let off = current_scroll_for_key(&h, ScrollTargetKey::Window(doc_id));
            Ok(JsValue::from_f64(f64::from(if is_y {
                off.y
            } else {
                off.x
            })))
        });
    }
    for (name, relative) in [("scroll", false), ("scrollTo", false), ("scrollBy", true)] {
        let h = Rc::clone(host);
        let ev = Rc::clone(events);
        let el = Rc::clone(event_loop);
        define_host_method(
            vm,
            proxy_h,
            name,
            move |vm, _, args| match parse_scroll_to_args(vm, args) {
                Ok((x, y, beh)) => Ok(execute_scroll_with_promise(
                    vm,
                    &h,
                    &ev,
                    &el,
                    ScrollTargetKey::Window(doc_id),
                    x,
                    y,
                    relative,
                    beh,
                )),
                Err(msg) => Ok(make_rejected_type_error_promise(vm, &msg)),
            },
        );
    }
    let h_mm = Rc::clone(host);
    define_host_method(vm, proxy_h, "matchMedia", move |vm, _, args| {
        let query = vm.value_to_rust_string(arg(args, 0));
        let mql = create_media_query_list_for_doc(vm, &h_mm, doc_id, &query);
        Ok(JsValue::from_object(mql))
    });
    let screen = alloc_screen_object(vm, host);
    let k_sc = vm.heap.intern_key("screen");
    vm.heap.object_define_data(
        proxy_h,
        k_sc,
        JsValue::from_object(screen),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
}
