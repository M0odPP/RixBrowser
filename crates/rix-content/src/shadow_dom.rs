//! WHATWG DOM §4.2.2.3 Shadow DOM Slotting, `HTMLSlotElement`, `Slottable`,
//! `slotchange` microtask signaling, `delegatesFocus`, `ShadowRoot.activeElement`,
//! `ShadowRoot.elementFromPoint`, and per-ShadowRoot `adoptedStyleSheets`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use rix_dom::{Document, NodeData, NodeId};
use rix_js::{JsValue, JsVm};

use crate::dom_bridge::{
    arg, define_host_accessor, define_host_method, require_node, wrap_node, DomMutation, SharedHost,
};
use crate::dom_interfaces::HTML_NS;
use crate::event_loop::SharedEventLoop;
use crate::events::{fire_dom_node_event, SharedEvents};

/// Per-document/runtime Shadow DOM slot assignment & adopted stylesheet state.
#[derive(Default)]
pub struct ShadowDomState {
    /// Manual slot assignments populated by `HTMLSlotElement.prototype.assign(...nodes)`.
    pub manual_assignments: HashMap<NodeId, Vec<NodeId>>,
    /// Snapshot of last computed assigned nodes per `<slot>`, used to detect `slotchange`.
    pub last_assigned: HashMap<NodeId, Vec<NodeId>>,
    /// Slots queued for the next microtask `slotchange` delivery.
    pub pending_slotchange: Vec<NodeId>,
    /// Per-ShadowRoot `adoptedStyleSheets` backing store.
    pub adopted_stylesheets: HashMap<NodeId, Vec<crate::cssom::SharedStyleSheet>>,
}

/// Returns the tree root (`Document`, `ShadowRoot`, `DocumentFragment`, or detached ancestor) of `node`.
pub(crate) fn tree_root(doc: &Document, node: NodeId) -> NodeId {
    let mut cur = node;
    for _ in 0..512 {
        match doc.parent(cur) {
            Some(p) => cur = p,
            None => return cur,
        }
    }
    cur
}

/// WHATWG DOM §4.2.2: returns `true` iff `a` is a shadow-including inclusive ancestor of `b`.
pub(crate) fn is_shadow_including_inclusive_ancestor(doc: &Document, a: NodeId, b: NodeId) -> bool {
    let mut cur = Some(b);
    for _ in 0..512 {
        let Some(nid) = cur else {
            return false;
        };
        if nid == a {
            return true;
        }
        cur = match doc.parent(nid) {
            Some(p) => Some(p),
            None => match doc.get(nid) {
                Some(NodeData::ShadowRoot { host, .. }) => Some(*host),
                _ => None,
            },
        };
    }
    false
}

/// WHATWG DOM §2.4 `retarget(a, b)`: retargets node `a` against reference node `b`.
pub(crate) fn retarget_node(doc: &Document, a: NodeId, b: NodeId) -> NodeId {
    let mut cur = a;
    for _ in 0..128 {
        let root = tree_root(doc, cur);
        match doc.get(root) {
            Some(NodeData::ShadowRoot { host, .. }) => {
                if is_shadow_including_inclusive_ancestor(doc, root, b) {
                    return cur;
                }
                cur = *host;
            }
            _ => return cur,
        }
    }
    cur
}

/// Returns `true` iff `nid` is an HTML `<slot>` element.
pub(crate) fn is_html_slot(doc: &Document, nid: NodeId) -> bool {
    matches!(
        doc.get(nid),
        Some(NodeData::Element(e)) if &*e.name.ns == HTML_NS && e.name.local.as_str() == "slot"
    )
}

/// Returns `true` iff `nid` is a WHATWG DOM `Slottable` (`Element` or `Text`).
pub(crate) fn is_slottable(doc: &Document, nid: NodeId) -> bool {
    matches!(doc.get(nid), Some(NodeData::Element(_) | NodeData::Text(_)))
}

/// WHATWG DOM §4.2.2.3 "find a slot" for `slottable_id`.
/// When `open_only` is `true`, returns `None` if the host's shadow root has `mode != "open"`.
pub(crate) fn find_slot_for_slottable(
    doc: &Document,
    shadow_state: &ShadowDomState,
    slottable_id: NodeId,
    open_only: bool,
) -> Option<NodeId> {
    if !is_slottable(doc, slottable_id) {
        return None;
    }
    let parent = doc.parent(slottable_id)?;
    let sr_id = match doc.get(parent) {
        Some(NodeData::Element(e)) => e.shadow_root?,
        _ => return None,
    };
    let (mode, slot_assignment) = match doc.get(sr_id) {
        Some(NodeData::ShadowRoot {
            mode,
            slot_assignment,
            ..
        }) => (mode.as_str(), slot_assignment.as_str()),
        _ => return None,
    };
    if open_only && mode != "open" {
        return None;
    }
    if slot_assignment == "manual" {
        for cand in doc.descendants(sr_id) {
            if is_html_slot(doc, cand) {
                if let Some(list) = shadow_state.manual_assignments.get(&cand) {
                    if list.contains(&slottable_id) {
                        return Some(cand);
                    }
                }
            }
        }
        return None;
    }
    let slot_name = match doc.get(slottable_id) {
        Some(NodeData::Element(e)) => e.get_attribute("slot").unwrap_or(""),
        Some(NodeData::Text(_)) => "",
        _ => return None,
    };
    for cand in doc.descendants(sr_id) {
        if let Some(NodeData::Element(se)) = doc.get(cand) {
            if &*se.name.ns == HTML_NS && se.name.local.as_str() == "slot" {
                let cand_name = se.get_attribute("name").unwrap_or("");
                if cand_name == slot_name {
                    return Some(cand);
                }
            }
        }
    }
    None
}

/// WHATWG DOM §4.2.2.3 "find slottables" for `slot_id`.
pub(crate) fn find_slottables_for_slot(
    doc: &Document,
    shadow_state: &ShadowDomState,
    slot_id: NodeId,
) -> Vec<NodeId> {
    if !is_html_slot(doc, slot_id) {
        return Vec::new();
    }
    let root = tree_root(doc, slot_id);
    let (host_el, slot_assignment) = match doc.get(root) {
        Some(NodeData::ShadowRoot {
            host,
            slot_assignment,
            ..
        }) => (*host, slot_assignment.as_str()),
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    if slot_assignment == "manual" {
        if let Some(assigned) = shadow_state.manual_assignments.get(&slot_id) {
            for &nid in assigned {
                if is_slottable(doc, nid) && doc.parent(nid) == Some(host_el) && !out.contains(&nid)
                {
                    out.push(nid);
                }
            }
        }
        return out;
    }
    for child in doc.children(host_el) {
        if find_slot_for_slottable(doc, shadow_state, child, false) == Some(slot_id) {
            out.push(child);
        }
    }
    out
}

/// WHATWG DOM §4.2.2.3 "find flattened slottables" for `slot_id`.
pub(crate) fn find_flattened_slottables(
    doc: &Document,
    shadow_state: &ShadowDomState,
    slot_id: NodeId,
) -> Vec<NodeId> {
    let mut visited = HashSet::new();
    find_flattened_slottables_inner(doc, shadow_state, slot_id, &mut visited)
}

fn find_flattened_slottables_inner(
    doc: &Document,
    shadow_state: &ShadowDomState,
    slot_id: NodeId,
    visited: &mut HashSet<NodeId>,
) -> Vec<NodeId> {
    if !visited.insert(slot_id) || !is_html_slot(doc, slot_id) {
        return Vec::new();
    }
    let root = tree_root(doc, slot_id);
    if !matches!(doc.get(root), Some(NodeData::ShadowRoot { .. })) {
        return Vec::new();
    }
    let mut slottables = find_slottables_for_slot(doc, shadow_state, slot_id);
    if slottables.is_empty() {
        for child in doc.children(slot_id) {
            if is_slottable(doc, child) {
                slottables.push(child);
            }
        }
    }
    let mut result = Vec::new();
    for node in slottables {
        if is_html_slot(doc, node)
            && matches!(
                doc.get(tree_root(doc, node)),
                Some(NodeData::ShadowRoot { .. })
            )
        {
            result.extend(find_flattened_slottables_inner(
                doc,
                shadow_state,
                node,
                visited,
            ));
        } else {
            result.push(node);
        }
    }
    result
}

/// Recomputes slot assignments for all `<slot>` descendants of `sr_id` and records
/// any slot whose assigned nodes changed into `pending_slotchange`.
pub(crate) fn refresh_slots_for_shadow_root(host: &SharedHost, sr_id: NodeId) {
    let hb = host.borrow();
    let doc = hb.document.borrow();
    if !matches!(doc.get(sr_id), Some(NodeData::ShadowRoot { .. })) {
        return;
    }
    let slots: Vec<NodeId> = doc
        .descendants(sr_id)
        .filter(|&id| is_html_slot(&doc, id))
        .collect();
    let mut sd = hb.shadow_dom.borrow_mut();
    for slot_id in slots {
        let now = find_slottables_for_slot(&doc, &sd, slot_id);
        let prev = sd.last_assigned.get(&slot_id).cloned().unwrap_or_default();
        if now != prev {
            sd.last_assigned.insert(slot_id, now);
            if !sd.pending_slotchange.contains(&slot_id) {
                sd.pending_slotchange.push(slot_id);
            }
        }
    }
}

/// Called from `HostState::mutation_sinks` on every DOM mutation to detect slot changes.
fn on_dom_mutation_for_slots(host: &SharedHost, mutation: &DomMutation) {
    let roots_to_refresh: Vec<NodeId> = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        let mut srs = Vec::new();
        match mutation {
            DomMutation::ChildList { target, .. } => {
                if let Some(NodeData::Element(e)) = doc.get(*target) {
                    if let Some(sr) = e.shadow_root {
                        srs.push(sr);
                    }
                }
                let troot = tree_root(&doc, *target);
                if matches!(doc.get(troot), Some(NodeData::ShadowRoot { .. }))
                    && !srs.contains(&troot)
                {
                    srs.push(troot);
                }
            }
            DomMutation::Attributes {
                target,
                attribute_name,
                ..
            } => {
                if attribute_name == "slot" {
                    if let Some(parent) = doc.parent(*target) {
                        if let Some(NodeData::Element(pe)) = doc.get(parent) {
                            if let Some(sr) = pe.shadow_root {
                                srs.push(sr);
                            }
                        }
                    }
                } else if attribute_name == "name" && is_html_slot(&doc, *target) {
                    let troot = tree_root(&doc, *target);
                    if matches!(doc.get(troot), Some(NodeData::ShadowRoot { .. })) {
                        srs.push(troot);
                    }
                }
            }
            DomMutation::CharacterData { .. } => {}
        }
        srs
    };

    // Also handle `<slot>` elements that were removed from a shadow root and lost their assigned nodes.
    // Fast-path: skip descendant walk entirely when no `<slot>` has tracked assignments!
    if let DomMutation::ChildList { removed, .. } = mutation {
        let hb = host.borrow();
        let mut sd = hb.shadow_dom.borrow_mut();
        if !sd.last_assigned.is_empty() {
            let doc = hb.document.borrow();
            for &rem in removed {
                for nid in doc.descendants(rem) {
                    if is_html_slot(&doc, nid) {
                        let prev = sd.last_assigned.get(&nid).cloned().unwrap_or_default();
                        if !prev.is_empty() {
                            sd.last_assigned.insert(nid, Vec::new());
                            if !sd.pending_slotchange.contains(&nid) {
                                sd.pending_slotchange.push(nid);
                            }
                        }
                    }
                }
            }
        }
    }

    for sr_id in roots_to_refresh {
        refresh_slots_for_shadow_root(host, sr_id);
    }
}

/// Drains queued `slotchange` signals during a WHATWG microtask checkpoint
/// (runs AFTER `MutationObserver` callbacks!).
pub(crate) fn deliver_slotchange_signals(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
) {
    loop {
        let pending: Vec<NodeId> = {
            let hb = host.borrow();
            let mut sd = hb.shadow_dom.borrow_mut();
            if sd.pending_slotchange.is_empty() {
                break;
            }
            std::mem::take(&mut sd.pending_slotchange)
        };
        for slot_id in pending {
            let _ = fire_dom_node_event(
                vm,
                host,
                events,
                event_loop,
                slot_id,
                "slotchange",
                true,
                false,
                &[],
            );
        }
    }
}

/// Returns whether element `nid` is natively focusable or has a valid integer `tabindex`.
pub(crate) fn is_element_focusable(doc: &Document, nid: NodeId) -> bool {
    let Some(NodeData::Element(e)) = doc.get(nid) else {
        return false;
    };
    if !doc.is_connected(nid) || e.has_attribute("disabled") || e.has_attribute("inert") {
        return false;
    }
    if let Some(ti) = e.get_attribute("tabindex") {
        if ti.trim().parse::<i32>().is_ok() {
            return true;
        }
    }
    if e.has_attribute("contenteditable") {
        return true;
    }
    matches!(
        e.name.local.as_str(),
        "a" | "area" | "button" | "input" | "select" | "textarea" | "iframe"
    )
}

/// Finds the first focusable descendant inside `shadow_root` for `delegatesFocus`,
/// descending into nested `delegatesFocus` shadow hosts and slotted elements.
fn find_first_focusable_in_shadow(
    doc: &Document,
    shadow_state: &ShadowDomState,
    sr_id: NodeId,
    depth: usize,
) -> Option<NodeId> {
    if depth > 32 {
        return None;
    }
    fn walk_children(
        doc: &Document,
        shadow_state: &ShadowDomState,
        parent: NodeId,
        depth: usize,
    ) -> Option<NodeId> {
        for child in doc.children(parent) {
            if is_html_slot(doc, child) {
                let assigned = find_flattened_slottables(doc, shadow_state, child);
                for sn in assigned {
                    if let Some(found) = check_candidate(doc, shadow_state, sn, depth) {
                        return Some(found);
                    }
                }
                continue;
            }
            if let Some(found) = check_candidate(doc, shadow_state, child, depth) {
                return Some(found);
            }
        }
        None
    }
    fn check_candidate(
        doc: &Document,
        shadow_state: &ShadowDomState,
        nid: NodeId,
        depth: usize,
    ) -> Option<NodeId> {
        let Some(NodeData::Element(e)) = doc.get(nid) else {
            return None;
        };
        if let Some(nested_sr) = e.shadow_root {
            let nested_df = matches!(
                doc.get(nested_sr),
                Some(NodeData::ShadowRoot {
                    delegates_focus: true,
                    ..
                })
            );
            if nested_df {
                if let Some(found) =
                    find_first_focusable_in_shadow(doc, shadow_state, nested_sr, depth + 1)
                {
                    return Some(found);
                }
            } else if is_element_focusable(doc, nid) {
                return Some(nid);
            }
            if let Some(found) =
                find_first_focusable_in_shadow(doc, shadow_state, nested_sr, depth + 1)
            {
                return Some(found);
            }
            return None;
        }
        if is_element_focusable(doc, nid) {
            return Some(nid);
        }
        walk_children(doc, shadow_state, nid, depth)
    }
    walk_children(doc, shadow_state, sr_id, depth)
}

/// Computes `Document.prototype.activeElement` or `ShadowRoot.prototype.activeElement`
/// retargeted to `root_id` per WHATWG Shadow DOM focus scoping.
fn active_element_for_root(host: &SharedHost, root_id: NodeId) -> Option<NodeId> {
    let hb = host.borrow();
    let doc = hb.document.borrow();
    let is_shadow = matches!(doc.get(root_id), Some(NodeData::ShadowRoot { .. }));
    if let Some(active) = hb.active_element {
        if doc.is_connected(active) && is_shadow_including_inclusive_ancestor(&doc, root_id, active)
        {
            let retargeted = retarget_node(&doc, active, root_id);
            if retargeted != root_id {
                return Some(retargeted);
            }
        }
    }
    if is_shadow {
        None
    } else {
        // Document fallback: `document.body` (or `documentElement`).
        for n in doc.descendants(root_id) {
            if matches!(
                doc.get(n),
                Some(NodeData::Element(e)) if e.name.local.as_str().eq_ignore_ascii_case("body")
            ) {
                return Some(n);
            }
        }
        doc.document_element_of(root_id)
    }
}

/// Performs CSSOM View hit-testing at viewport `(x, y)` directly over the layout `FragmentTree`
/// in reverse paint order, resolving `Text` hits to their parent element (or shadow host when a
/// direct child of a `ShadowRoot`) and retargeting every hit against `scope_root`.
fn elements_from_point_for_root(
    host: &SharedHost,
    scope_root: NodeId,
    x: f64,
    y: f64,
) -> Vec<NodeId> {
    let mut hb = host.borrow_mut();
    let owner_doc = hb.owner_doc_of(scope_root);
    let main_root = hb.document.borrow().root();
    let vp = hb.viewport_for_doc(owner_doc);
    if !x.is_finite()
        || !y.is_finite()
        || x < 0.0
        || y < 0.0
        || x >= f64::from(vp.width)
        || y >= f64::from(vp.height)
    {
        return Vec::new();
    }
    let root_scroll = if owner_doc == main_root {
        hb.scroll_offset
    } else {
        hb.subdoc_scroll_offsets
            .get(&owner_doc)
            .copied()
            .unwrap_or(rix_core::Point::ZERO)
    };
    let scroll_map = hb.element_scroll_offsets.clone();
    let doc_rc = Rc::clone(&hb.document);
    let mut raw_hits: Vec<NodeId> = Vec::new();
    let sub_tree_owned = if owner_doc != main_root {
        hb.layout_subdoc(owner_doc).map(|(t, _, _)| t)
    } else {
        None
    };
    let tree_opt = if owner_doc != main_root {
        sub_tree_owned.as_ref()
    } else {
        hb.ensure_layout()
    };
    if let Some(tree) = tree_opt {
        if let Some(root_frag) = tree.root.as_ref() {
            fn z_order(frag: &rix_layout::Fragment) -> i32 {
                match frag.style.z_index {
                    rix_css::ZIndexValue::Integer(n) => n,
                    rix_css::ZIndexValue::Auto => 0,
                }
            }
            fn hit_walk(
                frag: &rix_layout::Fragment,
                parent_origin: rix_core::Point,
                scroll_acc: rix_core::Point,
                scroll_map: &HashMap<NodeId, rix_core::Point>,
                pt: (f64, f64),
                is_root: bool,
                out: &mut Vec<NodeId>,
            ) {
                if matches!(frag.kind, rix_layout::FragmentKind::TextRun(_)) {
                    return;
                }
                let (x, y) = pt;
                let abs_border = frag.absolute_border_rect(parent_origin);
                let eff_scroll = if !is_root && frag.style.position == rix_css::PositionMode::Fixed
                {
                    rix_core::Point::ZERO
                } else {
                    scroll_acc
                };
                let child_scroll = if let Some(nid) = frag.dom_node {
                    if let Some(&s) = scroll_map.get(&nid) {
                        rix_core::Point::new(eff_scroll.x + s.x, eff_scroll.y + s.y)
                    } else {
                        eff_scroll
                    }
                } else {
                    eff_scroll
                };
                // Paint order: positive/zero z-index children (front-to-back), then self,
                // then negative z-index children (front-to-back).
                let mut pos_children: Vec<&rix_layout::Fragment> = Vec::new();
                let mut neg_children: Vec<&rix_layout::Fragment> = Vec::new();
                for ch in frag.children.iter() {
                    if z_order(ch) < 0 {
                        neg_children.push(ch);
                    } else {
                        pos_children.push(ch);
                    }
                }
                pos_children.sort_by_key(|c| z_order(c));
                neg_children.sort_by_key(|c| z_order(c));
                for ch in pos_children.into_iter().rev() {
                    hit_walk(
                        ch,
                        abs_border.origin,
                        child_scroll,
                        scroll_map,
                        pt,
                        false,
                        out,
                    );
                }
                if let Some(nid) = frag.dom_node {
                    let hit_self = frag.style.pointer_events != rix_css::PointerEventsMode::None
                        && frag.style.visibility == rix_css::VisibilityMode::Visible;
                    if hit_self {
                        let vx = f64::from(abs_border.origin.x - eff_scroll.x);
                        let vy = f64::from(abs_border.origin.y - eff_scroll.y);
                        let w = f64::from(abs_border.size.width);
                        let h = f64::from(abs_border.size.height);
                        if w > 0.0 && h > 0.0 && x >= vx && x < vx + w && y >= vy && y < vy + h {
                            out.push(nid);
                        }
                    }
                }
                for ch in neg_children.into_iter().rev() {
                    hit_walk(
                        ch,
                        abs_border.origin,
                        child_scroll,
                        scroll_map,
                        pt,
                        false,
                        out,
                    );
                }
            }
            hit_walk(
                root_frag,
                rix_core::Point::ZERO,
                root_scroll,
                &scroll_map,
                (x, y),
                true,
                &mut raw_hits,
            );
        }
    }
    let doc = doc_rc.borrow();
    let mut hits = Vec::new();
    for nid in raw_hits {
        let el_node = match doc.get(nid) {
            Some(NodeData::Element(_)) => Some(nid),
            Some(NodeData::Text(_)) => match doc.parent(nid) {
                Some(p) => match doc.get(p) {
                    Some(NodeData::Element(_)) => Some(p),
                    Some(NodeData::ShadowRoot { host: sh, .. }) => Some(*sh),
                    _ => None,
                },
                None => None,
            },
            _ => None,
        };
        if let Some(el) = el_node {
            let adjusted = retarget_node(&doc, el, scope_root);
            if matches!(doc.get(adjusted), Some(NodeData::Element(_))) && !hits.contains(&adjusted)
            {
                hits.push(adjusted);
            }
        }
    }
    // CSSOM View: viewport root element (`<html>`) is always the final hit when inside viewport.
    if let Some(doc_el) = doc.document_element_of(main_root) {
        if !hits.contains(&doc_el) {
            hits.push(doc_el);
        }
    }
    hits
}

/// Installs Shadow DOM slotting, `HTMLSlotElement`, `Slottable`, `delegatesFocus`,
/// `ShadowRoot.activeElement`/`slotAssignment`/`elementFromPoint`/`adoptedStyleSheets`,
/// and microtask `slotchange` delivery.
pub fn install_shadow_dom_apis(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
) {
    // Hook DOM mutations -> slot assignment diffing (Weak ref -> zero Rc cycles!).
    {
        let wh = Rc::downgrade(host);
        host.borrow_mut()
            .mutation_sinks
            .push(Rc::new(RefCell::new(Box::new(
                move |mutation: &DomMutation| {
                    if let Some(h) = wh.upgrade() {
                        on_dom_mutation_for_slots(&h, mutation);
                    }
                },
            ))));
    }

    // Hook microtask checkpoint -> deliver `slotchange` AFTER MutationObservers (Weak refs!).
    {
        let wh = Rc::downgrade(host);
        let wev = Rc::downgrade(events);
        let wel = Rc::downgrade(event_loop);
        event_loop
            .borrow_mut()
            .add_checkpoint_hook(Rc::new(RefCell::new(Box::new(move |vm: &mut JsVm| {
                let (Some(h), Some(evs), Some(el)) = (wh.upgrade(), wev.upgrade(), wel.upgrade())
                else {
                    return;
                };
                deliver_slotchange_signals(vm, &h, &evs, &el);
            }))));
    }

    let ep = host.borrow().prototypes.element_proto;
    let tp = host.borrow().prototypes.text_proto;
    let dp = host.borrow().prototypes.document_proto;
    let srp = host.borrow().prototypes.shadow_root_proto;
    let slot_proto = host
        .borrow()
        .interface_protos
        .get("HTMLSlotElement")
        .copied();

    // `Slottable.prototype.assignedSlot` on `Element.prototype` and `Text.prototype`
    for proto in [ep, tp] {
        let h = Rc::clone(host);
        let g = vm.alloc_host_fn("get assignedSlot", move |vm, t, _| {
            let nid = require_node(vm, &h, t)?;
            let slot = {
                let hb = h.borrow();
                let doc = hb.document.borrow();
                let sd = hb.shadow_dom.borrow();
                find_slot_for_slottable(&doc, &sd, nid, true)
            };
            Ok(slot.map_or(JsValue::NULL, |s| wrap_node(vm, &h, s)))
        });
        define_host_accessor(vm, proto, "assignedSlot", Some(g), None);
    }

    // `Element.prototype.slot` (reflects `slot` content attribute)
    {
        let h1 = Rc::clone(host);
        let g = vm.alloc_host_fn("get slot", move |vm, t, _| {
            let nid = require_node(vm, &h1, t)?;
            let val = match h1.borrow().document.borrow().get(nid) {
                Some(NodeData::Element(e)) => e.get_attribute("slot").unwrap_or("").to_string(),
                _ => String::new(),
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(val)))
        });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn("set slot", move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            let val = crate::dom_parsing::to_dom_string(vm, arg(args, 0))?;
            let old_val = {
                let doc_rc = Rc::clone(&h2.borrow().document);
                let mut doc = doc_rc.borrow_mut();
                match doc.get_mut(nid) {
                    Some(NodeData::Element(e)) => {
                        let old = e.get_attribute("slot").map(str::to_string);
                        e.set_attribute("slot", &val);
                        old
                    }
                    _ => None,
                }
            };
            h2.borrow().emit_mutation(DomMutation::Attributes {
                target: nid,
                attribute_name: "slot".to_string(),
                attribute_namespace: None,
                old_value: old_val,
            });
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, ep, "slot", Some(g), Some(s));
    }

    // `HTMLSlotElement.prototype.{name, assignedNodes, assignedElements, assign}`
    if let Some(sp) = slot_proto {
        let h1 = Rc::clone(host);
        let g = vm.alloc_host_fn("get name", move |vm, t, _| {
            let nid = require_node(vm, &h1, t)?;
            let val = match h1.borrow().document.borrow().get(nid) {
                Some(NodeData::Element(e)) => e.get_attribute("name").unwrap_or("").to_string(),
                _ => String::new(),
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(val)))
        });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn("set name", move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            let val = crate::dom_parsing::to_dom_string(vm, arg(args, 0))?;
            let old_val = {
                let doc_rc = Rc::clone(&h2.borrow().document);
                let mut doc = doc_rc.borrow_mut();
                match doc.get_mut(nid) {
                    Some(NodeData::Element(e)) => {
                        let old = e.get_attribute("name").map(str::to_string);
                        e.set_attribute("name", &val);
                        old
                    }
                    _ => None,
                }
            };
            h2.borrow().emit_mutation(DomMutation::Attributes {
                target: nid,
                attribute_name: "name".to_string(),
                attribute_namespace: None,
                old_value: old_val,
            });
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, sp, "name", Some(g), Some(s));

        for (mname, elements_only) in [("assignedNodes", false), ("assignedElements", true)] {
            let h = Rc::clone(host);
            define_host_method(vm, sp, mname, move |vm, t, args| {
                let slot_id = require_node(vm, &h, t)?;
                let mut flatten = false;
                if let Some(oh) = arg(args, 0).as_object() {
                    let kf = vm.heap.intern_key("flatten");
                    if let Ok(v) = vm.get_property_by_key(oh, kf) {
                        flatten = vm.to_boolean(v);
                    }
                }
                let nodes: Vec<NodeId> = {
                    let hb = h.borrow();
                    let doc = hb.document.borrow();
                    let sd = hb.shadow_dom.borrow();
                    let raw = if flatten {
                        find_flattened_slottables(&doc, &sd, slot_id)
                    } else {
                        find_slottables_for_slot(&doc, &sd, slot_id)
                    };
                    if elements_only {
                        raw.into_iter()
                            .filter(|&n| matches!(doc.get(n), Some(NodeData::Element(_))))
                            .collect()
                    } else {
                        raw
                    }
                };
                let arr = vm.alloc_array();
                for n in nodes {
                    let w = wrap_node(vm, &h, n);
                    vm.heap.object_push_element(arr, w);
                }
                Ok(JsValue::from_object(arr))
            });
        }

        let h = Rc::clone(host);
        define_host_method(vm, sp, "assign", move |vm, t, args| {
            let slot_id = require_node(vm, &h, t)?;
            // Per WHATWG HTML §4.12.4: `assign(...nodes)` takes variadic `(Element or Text)`.
            // Non-node arguments (including arrays/sequences) MUST throw `TypeError`.
            let mut new_nodes: Vec<NodeId> = Vec::with_capacity(args.len());
            for &a in args {
                let Some(nid) = h.borrow().node_for_value(vm, a) else {
                    return Err(vm.type_error(
                        "Failed to execute 'assign' on 'HTMLSlotElement': argument is not an Element or Text node",
                    ));
                };
                let ok = is_slottable(&h.borrow().document.borrow(), nid);
                if !ok {
                    return Err(vm.type_error(
                        "Failed to execute 'assign' on 'HTMLSlotElement': argument is not an Element or Text node",
                    ));
                }
                if !new_nodes.contains(&nid) {
                    new_nodes.push(nid);
                }
            }
            let affected_roots: Vec<NodeId> = {
                let hb = h.borrow();
                let doc = hb.document.borrow();
                let mut sd = hb.shadow_dom.borrow_mut();
                let mut roots = Vec::new();
                // Remove newly assigned nodes from any other slot's manual list.
                let mut other_slots_modified = Vec::new();
                for (&other_slot, list) in &mut sd.manual_assignments {
                    if other_slot != slot_id {
                        let before = list.len();
                        list.retain(|n| !new_nodes.contains(n));
                        if list.len() != before {
                            other_slots_modified.push(other_slot);
                        }
                    }
                }
                for os in other_slots_modified {
                    let r = tree_root(&doc, os);
                    if matches!(doc.get(r), Some(NodeData::ShadowRoot { .. }))
                        && !roots.contains(&r)
                    {
                        roots.push(r);
                    }
                }
                sd.manual_assignments.insert(slot_id, new_nodes);
                let my_root = tree_root(&doc, slot_id);
                if matches!(doc.get(my_root), Some(NodeData::ShadowRoot { .. }))
                    && !roots.contains(&my_root)
                {
                    roots.push(my_root);
                }
                roots
            };
            for r in affected_roots {
                refresh_slots_for_shadow_root(&h, r);
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // `ShadowRoot.prototype.slotAssignment`
    {
        let h = Rc::clone(host);
        let g = vm.alloc_host_fn("get slotAssignment", move |vm, t, _| {
            let nid = require_node(vm, &h, t)?;
            let sa = match h.borrow().document.borrow().get(nid) {
                Some(NodeData::ShadowRoot {
                    slot_assignment, ..
                }) => slot_assignment.clone(),
                _ => "named".to_string(),
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(sa)))
        });
        define_host_accessor(vm, srp, "slotAssignment", Some(g), None);
    }

    // `ShadowRoot.prototype.activeElement` & retargeted `Document.prototype.activeElement`
    for proto in [srp, dp] {
        let h = Rc::clone(host);
        let g = vm.alloc_host_fn("get activeElement", move |vm, t, _| {
            let root_id = require_node(vm, &h, t)?;
            let ae = active_element_for_root(&h, root_id);
            Ok(ae.map_or(JsValue::NULL, |n| wrap_node(vm, &h, n)))
        });
        define_host_accessor(vm, proto, "activeElement", Some(g), None);
    }

    // `Element.prototype.focus()` with Shadow DOM `delegatesFocus` + `focus`/`blur`/`focusin`/`focusout` events
    {
        let h = Rc::clone(host);
        let evs = Rc::clone(events);
        let el = Rc::clone(event_loop);
        define_host_method(vm, ep, "focus", move |vm, t, _| {
            let nid = require_node(vm, &h, t)?;
            let target_to_focus: Option<NodeId> = {
                let hb = h.borrow();
                let doc = hb.document.borrow();
                if !doc.is_connected(nid) {
                    None
                } else if let Some(NodeData::Element(e)) = doc.get(nid) {
                    if let Some(sr_id) = e.shadow_root {
                        let delegates = matches!(
                            doc.get(sr_id),
                            Some(NodeData::ShadowRoot {
                                delegates_focus: true,
                                ..
                            })
                        );
                        if delegates {
                            // If focus is already inside this shadow tree, keep it!
                            if hb.active_element.is_some_and(|ae| {
                                is_shadow_including_inclusive_ancestor(&doc, sr_id, ae)
                            }) {
                                return Ok(JsValue::UNDEFINED);
                            }
                            let sd = hb.shadow_dom.borrow();
                            find_first_focusable_in_shadow(&doc, &sd, sr_id, 0)
                        } else if is_element_focusable(&doc, nid) {
                            Some(nid)
                        } else {
                            None
                        }
                    } else if is_element_focusable(&doc, nid) {
                        Some(nid)
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some(new_focus) = target_to_focus {
                let prev_focus = h.borrow().active_element;
                if prev_focus != Some(new_focus) {
                    h.borrow_mut().set_focused_element(Some(new_focus), true);
                    if let Some(old_el) = prev_focus {
                        let new_val = wrap_node(vm, &h, new_focus);
                        let _ = fire_dom_node_event(
                            vm,
                            &h,
                            &evs,
                            &el,
                            old_el,
                            "blur",
                            false,
                            false,
                            &[("relatedTarget", new_val)],
                        );
                        let _ = fire_dom_node_event(
                            vm,
                            &h,
                            &evs,
                            &el,
                            old_el,
                            "focusout",
                            true,
                            false,
                            &[("relatedTarget", new_val)],
                        );
                    }
                    let old_val = prev_focus.map_or(JsValue::NULL, |o| wrap_node(vm, &h, o));
                    let _ = fire_dom_node_event(
                        vm,
                        &h,
                        &evs,
                        &el,
                        new_focus,
                        "focus",
                        false,
                        false,
                        &[("relatedTarget", old_val)],
                    );
                    let _ = fire_dom_node_event(
                        vm,
                        &h,
                        &evs,
                        &el,
                        new_focus,
                        "focusin",
                        true,
                        false,
                        &[("relatedTarget", old_val)],
                    );
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // `DocumentOrShadowRoot`: `elementFromPoint(x, y)` and `elementsFromPoint(x, y)`
    for proto in [dp, srp] {
        let h1 = Rc::clone(host);
        define_host_method(vm, proto, "elementFromPoint", move |vm, t, args| {
            let root_id = require_node(vm, &h1, t)?;
            if args.len() < 2 {
                return Err(vm.type_error("elementFromPoint requires 2 arguments (x, y)"));
            }
            let x = vm.to_number(args[0])?;
            let y = vm.to_number(args[1])?;
            if !x.is_finite() || !y.is_finite() {
                return Err(vm.type_error("elementFromPoint coordinates must be finite numbers"));
            }
            let hits = elements_from_point_for_root(&h1, root_id, x, y);
            Ok(hits
                .first()
                .map_or(JsValue::NULL, |&n| wrap_node(vm, &h1, n)))
        });
        let h2 = Rc::clone(host);
        define_host_method(vm, proto, "elementsFromPoint", move |vm, t, args| {
            let root_id = require_node(vm, &h2, t)?;
            if args.len() < 2 {
                return Err(vm.type_error("elementsFromPoint requires 2 arguments (x, y)"));
            }
            let x = vm.to_number(args[0])?;
            let y = vm.to_number(args[1])?;
            if !x.is_finite() || !y.is_finite() {
                return Err(vm.type_error("elementsFromPoint coordinates must be finite numbers"));
            }
            let hits = elements_from_point_for_root(&h2, root_id, x, y);
            let arr = vm.alloc_array();
            for n in hits {
                let w = wrap_node(vm, &h2, n);
                vm.heap.object_push_element(arr, w);
            }
            Ok(JsValue::from_object(arr))
        });
    }

    // `HTMLTemplateElement.prototype.{shadowRootMode, shadowRootDelegatesFocus, shadowRootClonable, shadowRootSerializable}`
    if let Some(&tmpl_proto) = host.borrow().interface_protos.get("HTMLTemplateElement") {
        let h1 = Rc::clone(host);
        let g = vm.alloc_host_fn("get shadowRootMode", move |vm, t, _| {
            let nid = require_node(vm, &h1, t)?;
            let val = match h1.borrow().document.borrow().get(nid) {
                Some(NodeData::Element(e)) => {
                    match e
                        .get_attribute("shadowrootmode")
                        .map(|s| s.to_ascii_lowercase())
                        .as_deref()
                    {
                        Some("open") => "open",
                        Some("closed") => "closed",
                        _ => "",
                    }
                }
                _ => "",
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(val)))
        });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn("set shadowRootMode", move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            let val = crate::dom_parsing::to_dom_string(vm, arg(args, 0))?;
            if let Some(NodeData::Element(e)) = h2.borrow().document.borrow_mut().get_mut(nid) {
                e.set_attribute("shadowrootmode", &val);
            }
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, tmpl_proto, "shadowRootMode", Some(g), Some(s));

        let h1 = Rc::clone(host);
        let g = vm.alloc_host_fn("get shadowRootSlotAssignment", move |vm, t, _| {
            let nid = require_node(vm, &h1, t)?;
            let val = match h1.borrow().document.borrow().get(nid) {
                Some(NodeData::Element(e))
                    if e.get_attribute("shadowrootslotassignment")
                        .is_some_and(|s| s.eq_ignore_ascii_case("manual")) =>
                {
                    "manual"
                }
                _ => "named",
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(val)))
        });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn("set shadowRootSlotAssignment", move |vm, t, args| {
            let nid = require_node(vm, &h2, t)?;
            let val = crate::dom_parsing::to_dom_string(vm, arg(args, 0))?;
            if let Some(NodeData::Element(e)) = h2.borrow().document.borrow_mut().get_mut(nid) {
                e.set_attribute("shadowrootslotassignment", &val);
            }
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, tmpl_proto, "shadowRootSlotAssignment", Some(g), Some(s));

        for (prop, attr) in [
            ("shadowRootDelegatesFocus", "shadowrootdelegatesfocus"),
            ("shadowRootClonable", "shadowrootclonable"),
            ("shadowRootSerializable", "shadowrootserializable"),
        ] {
            let h1 = Rc::clone(host);
            let g = vm.alloc_host_fn(prop, move |vm, t, _| {
                let nid = require_node(vm, &h1, t)?;
                let has = matches!(
                    h1.borrow().document.borrow().get(nid),
                    Some(NodeData::Element(e)) if e.has_attribute(attr)
                );
                Ok(JsValue::from_bool(has))
            });
            let h2 = Rc::clone(host);
            let s = vm.alloc_host_fn(prop, move |vm, t, args| {
                let nid = require_node(vm, &h2, t)?;
                let on = vm.to_boolean(arg(args, 0));
                if let Some(NodeData::Element(e)) = h2.borrow().document.borrow_mut().get_mut(nid) {
                    if on {
                        e.set_attribute(attr, "");
                    } else {
                        e.remove_attribute(attr);
                    }
                }
                Ok(JsValue::UNDEFINED)
            });
            define_host_accessor(vm, tmpl_proto, prop, Some(g), Some(s));
        }
    }

    // `ShadowRoot.prototype.adoptedStyleSheets` (live ObservableArray Proxy)
    {
        let h1 = Rc::clone(host);
        let g = vm.alloc_host_fn("get adoptedStyleSheets", move |vm, t, _| {
            let sr_id = require_node(vm, &h1, t)?;
            Ok(crate::cssom::create_adopted_stylesheets_proxy(
                vm,
                &h1,
                Some(sr_id),
            ))
        });
        let h2 = Rc::clone(host);
        let s = vm.alloc_host_fn("set adoptedStyleSheets", move |vm, t, args| {
            let sr_id = require_node(vm, &h2, t)?;
            let items = vm.collect_iterable(arg(args, 0))?;
            let mut sheets = Vec::with_capacity(items.len());
            {
                let hb = h2.borrow();
                for item in items {
                    let Some(oh) = item.as_object() else {
                        return Err(
                            vm.type_error("adoptedStyleSheets must contain CSSStyleSheet objects")
                        );
                    };
                    let Some(sh) = hb.stylesheet_handle_map.get(&oh) else {
                        return Err(
                            vm.type_error("adoptedStyleSheets must contain CSSStyleSheet objects")
                        );
                    };
                    if !sh.borrow().is_constructed {
                        return Err(crate::web_apis::create_dom_exception(
                            vm,
                            "NotAllowedError",
                            "Cannot adopt a non-constructed CSSStyleSheet",
                        ));
                    }
                    sheets.push(Rc::clone(sh));
                }
            }
            h2.borrow()
                .shadow_dom
                .borrow_mut()
                .adopted_stylesheets
                .insert(sr_id, sheets);
            h2.borrow_mut().mark_sheets_dirty();
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, srp, "adoptedStyleSheets", Some(g), Some(s));
    }
}
