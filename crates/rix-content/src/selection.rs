//! Pure-Rust W3C Selection API (`Selection`, `window.getSelection`,
//! `document.getSelection`), DOM `Range` (`document.createRange`), and
//! CSS-aware HTML §4.4 `Element.innerText`.

use std::rc::Rc;

use rix_css::{ComputedStyle, DisplayMode, WhiteSpaceCollapseMode};
use rix_dom::{Document, NodeData, NodeId};
use rix_js::{HeapHandle, JsValue, JsVm, PropertyAttrs};

use crate::dom_bridge::{
    arg, define_global, define_host_accessor, define_host_method, require_node, wrap_node,
    SharedHost,
};

fn extract_node(vm: &mut JsVm, host: &SharedHost, val: JsValue) -> Option<NodeId> {
    require_node(vm, host, val).ok()
}

/// Active selection endpoints stored on [`crate::dom_bridge::HostState`].
#[derive(Debug, Clone, Default)]
pub struct SelectionState {
    pub anchor_node: Option<NodeId>,
    pub anchor_offset: usize,
    pub focus_node: Option<NodeId>,
    pub focus_offset: usize,
    pub selection_handle: Option<HeapHandle>,
    pub ranges: std::collections::HashMap<HeapHandle, RangeData>,
}

/// Live DOM `Range` boundary endpoints.
#[derive(Debug, Clone, Copy)]
pub struct RangeData {
    pub start_container: NodeId,
    pub start_offset: usize,
    pub end_container: NodeId,
    pub end_offset: usize,
}

/// Resolves the effective HTML §3.2.6.2 language for `node` (stopping on explicit `lang=""`).
fn effective_lang_for_node(doc: &Document, node: NodeId) -> Option<String> {
    let mut cur = Some(node);
    while let Some(id) = cur {
        if let Some(NodeData::Element(elem)) = doc.get(id) {
            if let Some(raw) = elem
                .get_attribute("lang")
                .or_else(|| elem.get_attribute("xml:lang"))
            {
                let trimmed = raw.trim();
                return if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
            }
        }
        cur = doc.parent(id);
    }
    None
}

fn is_block_display(d: DisplayMode) -> bool {
    !matches!(
        d,
        DisplayMode::Inline
            | DisplayMode::InlineBlock
            | DisplayMode::InlineFlex
            | DisplayMode::InlineGrid
            | DisplayMode::InlineGridLanes
            | DisplayMode::InlineGridLanesMulti
            | DisplayMode::InlineTable
            | DisplayMode::InlineFlowRootListItem
            | DisplayMode::Contents
    )
}

fn is_skipped_element(tag: &str) -> bool {
    matches!(
        tag,
        "script" | "style" | "noscript" | "template" | "head" | "meta" | "link" | "title"
    )
}

fn style_for_node(
    host: &crate::dom_bridge::HostState,
    doc: &Document,
    id: NodeId,
) -> ComputedStyle {
    let mut cur = Some(id);
    while let Some(n) = cur {
        if let Some(st) = host.style_ctx.style_for(n) {
            return (**st).clone();
        }
        cur = doc.parent(n);
    }
    ComputedStyle::default()
}

/// Finds the last visible character immediately preceding `target` inside the same
/// enclosing block formatting context (for CSS Text L3 §2.1 `capitalize` across spans).
fn preceding_char_in_block(
    host: &crate::dom_bridge::HostState,
    doc: &Document,
    target: NodeId,
) -> Option<char> {
    let target_style = style_for_node(host, doc, target);
    if is_block_display(target_style.display) {
        return None;
    }
    let mut blk = doc.parent(target)?;
    loop {
        if let Some(NodeData::Element(_)) = doc.get(blk) {
            let st = style_for_node(host, doc, blk);
            if is_block_display(st.display) {
                break;
            }
        }
        match doc.parent(blk) {
            Some(p) => blk = p,
            None => break,
        }
    }

    fn walk_until(
        host: &crate::dom_bridge::HostState,
        doc: &Document,
        cur: NodeId,
        target: NodeId,
        last_ch: &mut Option<char>,
    ) -> bool {
        if cur == target {
            return true;
        }
        match doc.get(cur) {
            Some(NodeData::Text(t)) => {
                for ch in t.chars() {
                    *last_ch = Some(ch);
                }
                false
            }
            Some(NodeData::Element(e)) => {
                let tag = e.name.local.as_str();
                if is_skipped_element(tag) {
                    return false;
                }
                let st = style_for_node(host, doc, cur);
                if st.display == DisplayMode::None {
                    return false;
                }
                if tag == "br" {
                    *last_ch = Some('\n');
                    return false;
                }
                let is_blk = is_block_display(st.display);
                if is_blk {
                    *last_ch = None;
                }
                let kids: Vec<NodeId> = doc.children(cur).collect();
                for child in kids {
                    if walk_until(host, doc, child, target, last_ch) {
                        return true;
                    }
                }
                if is_blk {
                    *last_ch = None;
                }
                false
            }
            Some(NodeData::Document | NodeData::DocumentFragment | NodeData::ShadowRoot { .. }) => {
                let kids: Vec<NodeId> = doc.children(cur).collect();
                for child in kids {
                    if walk_until(host, doc, child, target, last_ch) {
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    }

    let mut last_ch = None;
    let kids: Vec<NodeId> = doc.children(blk).collect();
    for child in kids {
        if walk_until(host, doc, child, target, &mut last_ch) {
            break;
        }
    }
    last_ch
}

/// Computes CSS-aware HTML §4.4 `Element.innerText` honoring `display: none`,
/// `white-space-collapse`, `<br>`, block boundaries, and `text-transform` + `lang`.
#[must_use]
pub fn compute_inner_text(host: &SharedHost, nid: NodeId) -> String {
    host.borrow_mut().ensure_styles();
    let hb = host.borrow();
    let doc = hb.document.borrow();

    let root_style = style_for_node(&hb, &doc, nid);
    if root_style.display == DisplayMode::None {
        return doc.inner_text(nid);
    }

    let mut out = String::new();
    let mut prev_char = preceding_char_in_block(&hb, &doc, nid);
    let mut pending_space = false;
    let mut at_line_start = true;

    #[allow(clippy::too_many_arguments)]
    fn collect(
        hb: &crate::dom_bridge::HostState,
        doc: &Document,
        id: NodeId,
        is_root: bool,
        out: &mut String,
        prev_char: &mut Option<char>,
        pending_space: &mut bool,
        at_line_start: &mut bool,
    ) {
        match doc.get(id) {
            Some(NodeData::Text(raw)) => {
                let parent_id = doc.parent(id).unwrap_or(id);
                let style = style_for_node(hb, doc, parent_id);
                let lang = effective_lang_for_node(doc, parent_id);
                let transformed = rix_layout::text_transform::apply_text_transform_flags(
                    raw,
                    style.text.text_transform_flags,
                    lang.as_deref(),
                    *prev_char,
                );
                let collapse = style.text.white_space_collapse;
                let preserve_spaces = matches!(
                    collapse,
                    WhiteSpaceCollapseMode::Preserve
                        | WhiteSpaceCollapseMode::PreserveSpaces
                        | WhiteSpaceCollapseMode::BreakSpaces
                );
                let preserve_breaks = matches!(
                    collapse,
                    WhiteSpaceCollapseMode::Preserve
                        | WhiteSpaceCollapseMode::PreserveBreaks
                        | WhiteSpaceCollapseMode::BreakSpaces
                );
                if preserve_spaces {
                    if *pending_space && !*at_line_start {
                        out.push(' ');
                    }
                    *pending_space = false;
                    out.push_str(&transformed);
                    if let Some(last) = transformed.chars().last() {
                        *prev_char = Some(last);
                        *at_line_start = last == '\n';
                    }
                } else {
                    for ch in transformed.chars() {
                        if preserve_breaks && ch == '\n' {
                            *pending_space = false;
                            out.push('\n');
                            *prev_char = Some('\n');
                            *at_line_start = true;
                        } else if matches!(ch, ' ' | '\t' | '\n' | '\r' | '\x0C') {
                            if collapse != WhiteSpaceCollapseMode::Discard {
                                *pending_space = true;
                            }
                            *prev_char = Some(' ');
                        } else {
                            if *pending_space && !*at_line_start {
                                out.push(' ');
                            }
                            *pending_space = false;
                            out.push(ch);
                            *prev_char = Some(ch);
                            *at_line_start = false;
                        }
                    }
                }
            }
            Some(NodeData::Element(elem)) => {
                let tag = elem.name.local.as_str();
                if !is_root && is_skipped_element(tag) {
                    return;
                }
                let style = style_for_node(hb, doc, id);
                if !is_root && style.display == DisplayMode::None {
                    return;
                }
                if tag == "br" {
                    *pending_space = false;
                    out.push('\n');
                    *prev_char = Some('\n');
                    *at_line_start = true;
                    return;
                }
                let is_blk = !is_root && is_block_display(style.display);
                if is_blk && !out.is_empty() && !out.ends_with('\n') {
                    *pending_space = false;
                    out.push('\n');
                    *prev_char = None;
                    *at_line_start = true;
                }
                let kids: Vec<NodeId> = doc.children(id).collect();
                for child in kids {
                    collect(
                        hb,
                        doc,
                        child,
                        false,
                        out,
                        prev_char,
                        pending_space,
                        at_line_start,
                    );
                }
                if is_blk && !out.is_empty() && !out.ends_with('\n') {
                    *pending_space = false;
                    out.push('\n');
                    *prev_char = None;
                    *at_line_start = true;
                }
            }
            Some(NodeData::Document | NodeData::DocumentFragment | NodeData::ShadowRoot { .. }) => {
                let kids: Vec<NodeId> = doc.children(id).collect();
                for child in kids {
                    collect(
                        hb,
                        doc,
                        child,
                        false,
                        out,
                        prev_char,
                        pending_space,
                        at_line_start,
                    );
                }
            }
            _ => {}
        }
    }

    collect(
        &hb,
        &doc,
        nid,
        true,
        &mut out,
        &mut prev_char,
        &mut pending_space,
        &mut at_line_start,
    );
    while out.ends_with('\n') && is_block_display(root_style.display) {
        out.pop();
    }
    out
}

/// Slices a UTF-8 string by UTF-16 code unit `[start_u16, end_u16)` offsets.
fn slice_utf16(s: &str, start_u16: usize, end_u16: usize) -> &str {
    if start_u16 >= end_u16 {
        return "";
    }
    let mut u16_pos = 0usize;
    let mut byte_start = s.len();
    let mut byte_end = s.len();
    for (byte_idx, ch) in s.char_indices() {
        if u16_pos <= start_u16 {
            byte_start = byte_idx;
        }
        let next_u16 = u16_pos + ch.len_utf16();
        if u16_pos < end_u16 {
            byte_end = byte_idx + ch.len_utf8();
        }
        u16_pos = next_u16;
    }
    if start_u16 >= u16_pos {
        return "";
    }
    &s[byte_start.min(s.len())..byte_end.min(s.len())]
}

/// Returns the flattened DOM tree order of all nodes under the root containing `node`.
fn tree_order(doc: &Document, node: NodeId) -> Vec<NodeId> {
    let mut root = node;
    while let Some(p) = doc.parent(root) {
        root = p;
    }
    let mut order = Vec::new();
    fn dfs(doc: &Document, cur: NodeId, out: &mut Vec<NodeId>) {
        out.push(cur);
        for child in doc.children(cur) {
            dfs(doc, child, out);
        }
    }
    dfs(doc, root, &mut order);
    order
}

/// Extracts text between two DOM boundary points `(a_node, a_off)` and `(b_node, b_off)`,
/// applying CSS `text-transform` + `lang` when `apply_css_transform` is `true`
/// (`Selection.toString()`).
fn extract_range_text(
    host: &SharedHost,
    a_node: NodeId,
    a_off: usize,
    b_node: NodeId,
    b_off: usize,
    apply_css_transform: bool,
) -> String {
    if apply_css_transform {
        host.borrow_mut().ensure_styles();
    }
    let hb = host.borrow();
    let doc = hb.document.borrow();

    // Normalize endpoint order in DOM tree order.
    let order = tree_order(&doc, a_node);
    let pos_a = order.iter().position(|&n| n == a_node);
    let pos_b = order.iter().position(|&n| n == b_node);
    let (start_node, start_off, end_node, end_off) = match (pos_a, pos_b) {
        (Some(pa), Some(pb)) if pa > pb || (pa == pb && a_off > b_off) => {
            (b_node, b_off, a_node, a_off)
        }
        _ => (a_node, a_off, b_node, b_off),
    };

    if start_node == end_node && start_off == end_off {
        return String::new();
    }

    let transform_slice = |text_node: NodeId, raw_slice: &str, prev_ch: Option<char>| -> String {
        if !apply_css_transform {
            return raw_slice.to_string();
        }
        let parent_id = doc.parent(text_node).unwrap_or(text_node);
        let style = style_for_node(&hb, &doc, parent_id);
        let lang = effective_lang_for_node(&doc, parent_id);
        rix_layout::text_transform::apply_text_transform_flags(
            raw_slice,
            style.text.text_transform_flags,
            lang.as_deref(),
            prev_ch,
        )
    };

    // Fast path: selection within a single Text node.
    if start_node == end_node {
        if let Some(NodeData::Text(t)) = doc.get(start_node) {
            let slice = slice_utf16(t, start_off, end_off);
            let prev_ch = if start_off > 0 {
                slice_utf16(t, 0, start_off).chars().last()
            } else {
                preceding_char_in_block(&hb, &doc, start_node)
            };
            return transform_slice(start_node, slice, prev_ch);
        }
        // Selection across a child range of a single Element container:
        // e.g. `setBaseAndExtent(target, 0, target, 1)`.
        let kids: Vec<NodeId> = doc.children(start_node).collect();
        let lo = start_off.min(kids.len());
        let hi = end_off.min(kids.len());
        let mut out = String::new();
        let mut prev_ch = preceding_char_in_block(&hb, &doc, start_node);
        for &child in &kids[lo..hi] {
            collect_subtree_text(
                &hb,
                &doc,
                child,
                apply_css_transform,
                &mut prev_ch,
                &mut out,
            );
        }
        return out;
    }

    // General path: walk tree order between start boundary and end boundary.
    let resolve_boundary = |node: NodeId, off: usize| -> (usize, Option<usize>) {
        if matches!(doc.get(node), Some(NodeData::Text(_))) {
            let idx = order.iter().position(|&n| n == node).unwrap_or(0);
            return (idx, Some(off));
        }
        let kids: Vec<NodeId> = doc.children(node).collect();
        if off < kids.len() {
            let child = kids[off];
            let idx = order.iter().position(|&n| n == child).unwrap_or(0);
            (idx, None)
        } else {
            // After last child of `node`: jump past entire subtree of `node`.
            let node_idx = order.iter().position(|&n| n == node).unwrap_or(0);
            let mut end_idx = node_idx + 1;
            while end_idx < order.len() && is_descendant(&doc, order[end_idx], node) {
                end_idx += 1;
            }
            (end_idx, None)
        }
    };

    let (s_idx, s_text_off) = resolve_boundary(start_node, start_off);
    let (e_idx, e_text_off) = resolve_boundary(end_node, end_off);
    let last_inclusive = if e_text_off.is_some() {
        e_idx.min(order.len().saturating_sub(1))
    } else {
        e_idx.saturating_sub(1)
    };
    if s_idx > last_inclusive || s_idx >= order.len() {
        return String::new();
    }

    let mut out = String::new();
    let mut prev_ch = preceding_char_in_block(&hb, &doc, order[s_idx]);
    for (idx, &nid) in order
        .iter()
        .enumerate()
        .take(last_inclusive + 1)
        .skip(s_idx)
    {
        if let Some(NodeData::Text(t)) = doc.get(nid) {
            let lo = if idx == s_idx {
                s_text_off.unwrap_or(0)
            } else {
                0
            };
            let hi = if idx == e_idx {
                e_text_off.unwrap_or(usize::MAX)
            } else {
                usize::MAX
            };
            let slice = slice_utf16(t, lo, hi);
            let transformed = transform_slice(nid, slice, prev_ch);
            if let Some(last) = transformed.chars().last() {
                prev_ch = Some(last);
            }
            out.push_str(&transformed);
        }
    }
    out
}

fn is_descendant(doc: &Document, candidate: NodeId, ancestor: NodeId) -> bool {
    let mut cur = doc.parent(candidate);
    while let Some(p) = cur {
        if p == ancestor {
            return true;
        }
        cur = doc.parent(p);
    }
    false
}

fn collect_subtree_text(
    hb: &crate::dom_bridge::HostState,
    doc: &Document,
    id: NodeId,
    apply_css_transform: bool,
    prev_ch: &mut Option<char>,
    out: &mut String,
) {
    match doc.get(id) {
        Some(NodeData::Text(t)) => {
            if apply_css_transform {
                let parent_id = doc.parent(id).unwrap_or(id);
                let style = style_for_node(hb, doc, parent_id);
                let lang = effective_lang_for_node(doc, parent_id);
                let tr = rix_layout::text_transform::apply_text_transform_flags(
                    t,
                    style.text.text_transform_flags,
                    lang.as_deref(),
                    *prev_ch,
                );
                if let Some(last) = tr.chars().last() {
                    *prev_ch = Some(last);
                }
                out.push_str(&tr);
            } else {
                if let Some(last) = t.chars().last() {
                    *prev_ch = Some(last);
                }
                out.push_str(t);
            }
        }
        Some(NodeData::Element(e)) => {
            if is_skipped_element(e.name.local.as_str()) {
                return;
            }
            if apply_css_transform && style_for_node(hb, doc, id).display == DisplayMode::None {
                return;
            }
            for child in doc.children(id) {
                collect_subtree_text(hb, doc, child, apply_css_transform, prev_ch, out);
            }
        }
        Some(NodeData::Document | NodeData::DocumentFragment | NodeData::ShadowRoot { .. }) => {
            for child in doc.children(id) {
                collect_subtree_text(hb, doc, child, apply_css_transform, prev_ch, out);
            }
        }
        _ => {}
    }
}

fn alloc_range_object(vm: &mut JsVm, host: &SharedHost, data: RangeData) -> HeapHandle {
    let range_obj = vm.alloc_ordinary_object();
    host.borrow_mut().selection.ranges.insert(range_obj, data);

    let h = Rc::clone(host);
    let g_sc = vm.alloc_host_fn("get startContainer", move |vm, this_val, _| {
        let Some(rh) = this_val.as_object() else {
            return Ok(JsValue::NULL);
        };
        let n = h
            .borrow()
            .selection
            .ranges
            .get(&rh)
            .map(|r| r.start_container);
        Ok(n.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, range_obj, "startContainer", Some(g_sc), None);

    let h = Rc::clone(host);
    let g_so = vm.alloc_host_fn("get startOffset", move |_vm, this_val, _| {
        let Some(rh) = this_val.as_object() else {
            return Ok(JsValue::from_number(0.0));
        };
        let o = h
            .borrow()
            .selection
            .ranges
            .get(&rh)
            .map_or(0, |r| r.start_offset);
        Ok(JsValue::from_number(o as f64))
    });
    define_host_accessor(vm, range_obj, "startOffset", Some(g_so), None);

    let h = Rc::clone(host);
    let g_ec = vm.alloc_host_fn("get endContainer", move |vm, this_val, _| {
        let Some(rh) = this_val.as_object() else {
            return Ok(JsValue::NULL);
        };
        let n = h
            .borrow()
            .selection
            .ranges
            .get(&rh)
            .map(|r| r.end_container);
        Ok(n.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, range_obj, "endContainer", Some(g_ec), None);

    let h = Rc::clone(host);
    let g_eo = vm.alloc_host_fn("get endOffset", move |_vm, this_val, _| {
        let Some(rh) = this_val.as_object() else {
            return Ok(JsValue::from_number(0.0));
        };
        let o = h
            .borrow()
            .selection
            .ranges
            .get(&rh)
            .map_or(0, |r| r.end_offset);
        Ok(JsValue::from_number(o as f64))
    });
    define_host_accessor(vm, range_obj, "endOffset", Some(g_eo), None);

    let h = Rc::clone(host);
    let g_col = vm.alloc_host_fn("get collapsed", move |_vm, this_val, _| {
        let Some(rh) = this_val.as_object() else {
            return Ok(JsValue::from_bool(true));
        };
        let col =
            h.borrow().selection.ranges.get(&rh).is_none_or(|r| {
                r.start_container == r.end_container && r.start_offset == r.end_offset
            });
        Ok(JsValue::from_bool(col))
    });
    define_host_accessor(vm, range_obj, "collapsed", Some(g_col), None);

    let h = Rc::clone(host);
    define_host_method(vm, range_obj, "setStart", move |vm, this_val, args| {
        let Some(rh) = this_val.as_object() else {
            return Ok(JsValue::UNDEFINED);
        };
        let Some(nid) = extract_node(vm, &h, arg(args, 0)) else {
            return Err(vm.type_error("Range.setStart requires a Node"));
        };
        let off = arg(args, 1).to_number_f64().unwrap_or(0.0).max(0.0) as usize;
        if let Some(r) = h.borrow_mut().selection.ranges.get_mut(&rh) {
            r.start_container = nid;
            r.start_offset = off;
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, range_obj, "setEnd", move |vm, this_val, args| {
        let Some(rh) = this_val.as_object() else {
            return Ok(JsValue::UNDEFINED);
        };
        let Some(nid) = extract_node(vm, &h, arg(args, 0)) else {
            return Err(vm.type_error("Range.setEnd requires a Node"));
        };
        let off = arg(args, 1).to_number_f64().unwrap_or(0.0).max(0.0) as usize;
        if let Some(r) = h.borrow_mut().selection.ranges.get_mut(&rh) {
            r.end_container = nid;
            r.end_offset = off;
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(
        vm,
        range_obj,
        "selectNodeContents",
        move |vm, this_val, args| {
            let Some(rh) = this_val.as_object() else {
                return Ok(JsValue::UNDEFINED);
            };
            let Some(nid) = extract_node(vm, &h, arg(args, 0)) else {
                return Err(vm.type_error("Range.selectNodeContents requires a Node"));
            };
            let end_off = {
                let hb = h.borrow();
                let doc = hb.document.borrow();
                match doc.get(nid) {
                    Some(NodeData::Text(t)) => t.encode_utf16().count(),
                    _ => doc.children(nid).count(),
                }
            };
            if let Some(r) = h.borrow_mut().selection.ranges.get_mut(&rh) {
                r.start_container = nid;
                r.start_offset = 0;
                r.end_container = nid;
                r.end_offset = end_off;
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let h = Rc::clone(host);
    define_host_method(vm, range_obj, "selectNode", move |vm, this_val, args| {
        let Some(rh) = this_val.as_object() else {
            return Ok(JsValue::UNDEFINED);
        };
        let Some(nid) = extract_node(vm, &h, arg(args, 0)) else {
            return Err(vm.type_error("Range.selectNode requires a Node"));
        };
        let (parent, idx) = {
            let hb = h.borrow();
            let doc = hb.document.borrow();
            let Some(p) = doc.parent(nid) else {
                return Ok(JsValue::UNDEFINED);
            };
            let idx = doc.children(p).position(|c| c == nid).unwrap_or(0);
            (p, idx)
        };
        if let Some(r) = h.borrow_mut().selection.ranges.get_mut(&rh) {
            r.start_container = parent;
            r.start_offset = idx;
            r.end_container = parent;
            r.end_offset = idx + 1;
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, range_obj, "collapse", move |vm, this_val, args| {
        let Some(rh) = this_val.as_object() else {
            return Ok(JsValue::UNDEFINED);
        };
        let to_start = vm.to_boolean(arg(args, 0));
        if let Some(r) = h.borrow_mut().selection.ranges.get_mut(&rh) {
            if to_start {
                r.end_container = r.start_container;
                r.end_offset = r.start_offset;
            } else {
                r.start_container = r.end_container;
                r.start_offset = r.end_offset;
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, range_obj, "toString", move |vm, this_val, _| {
        let Some(rh) = this_val.as_object() else {
            return Ok(JsValue::from_string(vm.heap.alloc_string(String::new())));
        };
        let data = h.borrow().selection.ranges.get(&rh).copied();
        let text = match data {
            Some(r) => extract_range_text(
                &h,
                r.start_container,
                r.start_offset,
                r.end_container,
                r.end_offset,
                false,
            ),
            None => String::new(),
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(text)))
    });

    range_obj
}

fn get_or_create_selection(vm: &mut JsVm, host: &SharedHost) -> HeapHandle {
    if let Some(h) = host.borrow().selection.selection_handle {
        return h;
    }
    let sel = vm.alloc_ordinary_object();
    host.borrow_mut().selection.selection_handle = Some(sel);

    let h = Rc::clone(host);
    let g_an = vm.alloc_host_fn("get anchorNode", move |vm, _, _| {
        let n = h.borrow().selection.anchor_node;
        Ok(n.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, sel, "anchorNode", Some(g_an), None);

    let h = Rc::clone(host);
    let g_ao = vm.alloc_host_fn("get anchorOffset", move |_vm, _, _| {
        Ok(JsValue::from_number(
            h.borrow().selection.anchor_offset as f64,
        ))
    });
    define_host_accessor(vm, sel, "anchorOffset", Some(g_ao), None);

    let h = Rc::clone(host);
    let g_fn = vm.alloc_host_fn("get focusNode", move |vm, _, _| {
        let n = h.borrow().selection.focus_node;
        Ok(n.map_or(JsValue::NULL, |id| wrap_node(vm, &h, id)))
    });
    define_host_accessor(vm, sel, "focusNode", Some(g_fn), None);

    let h = Rc::clone(host);
    let g_fo = vm.alloc_host_fn("get focusOffset", move |_vm, _, _| {
        Ok(JsValue::from_number(
            h.borrow().selection.focus_offset as f64,
        ))
    });
    define_host_accessor(vm, sel, "focusOffset", Some(g_fo), None);

    let h = Rc::clone(host);
    let g_col = vm.alloc_host_fn("get isCollapsed", move |_vm, _, _| {
        let s = &h.borrow().selection;
        let col = s.anchor_node.is_none()
            || (s.anchor_node == s.focus_node && s.anchor_offset == s.focus_offset);
        Ok(JsValue::from_bool(col))
    });
    define_host_accessor(vm, sel, "isCollapsed", Some(g_col), None);

    let h = Rc::clone(host);
    let g_rc = vm.alloc_host_fn("get rangeCount", move |_vm, _, _| {
        let rc = if h.borrow().selection.anchor_node.is_some() {
            1.0
        } else {
            0.0
        };
        Ok(JsValue::from_number(rc))
    });
    define_host_accessor(vm, sel, "rangeCount", Some(g_rc), None);

    let h = Rc::clone(host);
    let g_type = vm.alloc_host_fn("get type", move |vm, _, _| {
        let ty = {
            let hb = h.borrow();
            let s = &hb.selection;
            if s.anchor_node.is_none() {
                "None"
            } else if s.anchor_node == s.focus_node && s.anchor_offset == s.focus_offset {
                "Caret"
            } else {
                "Range"
            }
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(ty.to_string())))
    });
    define_host_accessor(vm, sel, "type", Some(g_type), None);

    let h = Rc::clone(host);
    define_host_method(vm, sel, "setBaseAndExtent", move |vm, _, args| {
        let Some(a_node) = extract_node(vm, &h, arg(args, 0)) else {
            return Err(vm.type_error("Selection.setBaseAndExtent requires anchorNode"));
        };
        let a_off = arg(args, 1).to_number_f64().unwrap_or(0.0).max(0.0) as usize;
        let Some(f_node) = extract_node(vm, &h, arg(args, 2)) else {
            return Err(vm.type_error("Selection.setBaseAndExtent requires focusNode"));
        };
        let f_off = arg(args, 3).to_number_f64().unwrap_or(0.0).max(0.0) as usize;
        let mut hb = h.borrow_mut();
        hb.selection.anchor_node = Some(a_node);
        hb.selection.anchor_offset = a_off;
        hb.selection.focus_node = Some(f_node);
        hb.selection.focus_offset = f_off;
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sel, "collapse", move |vm, _, args| {
        let node = extract_node(vm, &h, arg(args, 0));
        let off = arg(args, 1).to_number_f64().unwrap_or(0.0).max(0.0) as usize;
        let mut hb = h.borrow_mut();
        hb.selection.anchor_node = node;
        hb.selection.anchor_offset = off;
        hb.selection.focus_node = node;
        hb.selection.focus_offset = off;
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sel, "extend", move |vm, _, args| {
        let Some(f_node) = extract_node(vm, &h, arg(args, 0)) else {
            return Err(vm.type_error("Selection.extend requires a Node"));
        };
        let f_off = arg(args, 1).to_number_f64().unwrap_or(0.0).max(0.0) as usize;
        let mut hb = h.borrow_mut();
        hb.selection.focus_node = Some(f_node);
        hb.selection.focus_offset = f_off;
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sel, "selectAllChildren", move |vm, _, args| {
        let Some(nid) = extract_node(vm, &h, arg(args, 0)) else {
            return Err(vm.type_error("Selection.selectAllChildren requires a Node"));
        };
        let count = {
            let hb = h.borrow();
            let doc = hb.document.borrow();
            match doc.get(nid) {
                Some(NodeData::Text(t)) => t.encode_utf16().count(),
                _ => doc.children(nid).count(),
            }
        };
        let mut hb = h.borrow_mut();
        hb.selection.anchor_node = Some(nid);
        hb.selection.anchor_offset = 0;
        hb.selection.focus_node = Some(nid);
        hb.selection.focus_offset = count;
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sel, "removeAllRanges", move |_vm, _, _| {
        let mut hb = h.borrow_mut();
        hb.selection.anchor_node = None;
        hb.selection.focus_node = None;
        hb.selection.anchor_offset = 0;
        hb.selection.focus_offset = 0;
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sel, "empty", move |_vm, _, _| {
        let mut hb = h.borrow_mut();
        hb.selection.anchor_node = None;
        hb.selection.focus_node = None;
        hb.selection.anchor_offset = 0;
        hb.selection.focus_offset = 0;
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sel, "addRange", move |_vm, _, args| {
        let Some(rh) = arg(args, 0).as_object() else {
            return Ok(JsValue::UNDEFINED);
        };
        let data = h.borrow().selection.ranges.get(&rh).copied();
        if let Some(r) = data {
            let mut hb = h.borrow_mut();
            hb.selection.anchor_node = Some(r.start_container);
            hb.selection.anchor_offset = r.start_offset;
            hb.selection.focus_node = Some(r.end_container);
            hb.selection.focus_offset = r.end_offset;
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sel, "getRangeAt", move |vm, _, args| {
        let idx = arg(args, 0).to_number_f64().unwrap_or(0.0) as isize;
        let pair = {
            let hb = h.borrow();
            let s = &hb.selection;
            match (s.anchor_node, s.focus_node) {
                (Some(a), Some(f)) if idx == 0 => Some((a, s.anchor_offset, f, s.focus_offset)),
                _ => None,
            }
        };
        let Some((a, ao, f, fo)) = pair else {
            return Err(vm.make_error("IndexSizeError", "Selection range index out of bounds"));
        };
        let rh = alloc_range_object(
            vm,
            &h,
            RangeData {
                start_container: a,
                start_offset: ao,
                end_container: f,
                end_offset: fo,
            },
        );
        Ok(JsValue::from_object(rh))
    });

    let h = Rc::clone(host);
    define_host_method(vm, sel, "toString", move |vm, _, _| {
        let pair = {
            let hb = h.borrow();
            let s = &hb.selection;
            match (s.anchor_node, s.focus_node) {
                (Some(a), Some(f)) => Some((a, s.anchor_offset, f, s.focus_offset)),
                _ => None,
            }
        };
        let text = match pair {
            Some((a, ao, f, fo)) => extract_range_text(&h, a, ao, f, fo, true),
            None => String::new(),
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(text)))
    });

    sel
}

/// Installs `window.getSelection()`, `document.getSelection()`, `document.createRange()`,
/// and `Range` constructor on `vm`.
pub fn install_selection_and_range(vm: &mut JsVm, host: &SharedHost) {
    let dp = host.borrow().prototypes.document_proto;

    let h = Rc::clone(host);
    let get_sel_fn = vm.alloc_host_fn("getSelection", move |vm, _, _| {
        let sel = get_or_create_selection(vm, &h);
        Ok(JsValue::from_object(sel))
    });
    define_global(vm, "getSelection", JsValue::from_object(get_sel_fn));

    let h = Rc::clone(host);
    define_host_method(vm, dp, "getSelection", move |vm, _, _| {
        let sel = get_or_create_selection(vm, &h);
        Ok(JsValue::from_object(sel))
    });

    let h = Rc::clone(host);
    define_host_method(vm, dp, "createRange", move |vm, _, _| {
        let root = h.borrow().document.borrow().root();
        let rh = alloc_range_object(
            vm,
            &h,
            RangeData {
                start_container: root,
                start_offset: 0,
                end_container: root,
                end_offset: 0,
            },
        );
        Ok(JsValue::from_object(rh))
    });

    let h = Rc::clone(host);
    let range_ctor = vm.alloc_host_fn("Range", move |vm, _, _| {
        let root = h.borrow().document.borrow().root();
        let rh = alloc_range_object(
            vm,
            &h,
            RangeData {
                start_container: root,
                start_offset: 0,
                end_container: root,
                end_offset: 0,
            },
        );
        Ok(JsValue::from_object(rh))
    });
    let range_proto = vm.alloc_ordinary_object();
    vm.heap.object_define_data(
        range_ctor,
        vm.key_prototype,
        JsValue::from_object(range_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.mark_constructor(range_ctor);
    define_global(vm, "Range", JsValue::from_object(range_ctor));
}
