//! Pure-Rust W3C Selection API (`Selection`, `window.getSelection`,
//! `document.getSelection`), DOM `Range` (`document.createRange`), and
//! CSS-aware HTML §4.4 `Element.innerText`.

use std::rc::Rc;

use rix_css::{ComputedStyle, DisplayMode, WhiteSpaceCollapseMode};
use rix_dom::{Document, LocalName, Namespace, NodeData, NodeId, QualName};
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
    pub selected_range: Option<HeapHandle>,
    pub abstract_range_proto: Option<HeapHandle>,
    pub range_proto: Option<HeapHandle>,
    pub static_range_proto: Option<HeapHandle>,
    pub ranges: std::collections::HashMap<HeapHandle, RangeData>,
}

/// Live or static DOM `Range` boundary endpoints.
#[derive(Debug, Clone, Copy)]
pub struct RangeData {
    pub start_container: NodeId,
    pub start_offset: usize,
    pub end_container: NodeId,
    pub end_offset: usize,
    pub is_live: bool,
}

// ---------------------------------------------------------------------------
// WHATWG DOM §5.2 Live Range Mutation Hooks
// ---------------------------------------------------------------------------

/// Adjusts all live `Range` boundary points after `CharacterData` "replace data"
/// (`offset`, `count` removed, `inserted_len` added, all in UTF-16 code units).
pub fn adjust_ranges_for_replace_data(
    host: &SharedHost,
    node: NodeId,
    offset: usize,
    count: usize,
    inserted_len: usize,
) {
    let mut hb = host.borrow_mut();
    let end_removed = offset.saturating_add(count);
    for r in hb.selection.ranges.values_mut() {
        if !r.is_live {
            continue;
        }
        if r.start_container == node {
            if r.start_offset > offset && r.start_offset <= end_removed {
                r.start_offset = offset;
            } else if r.start_offset > end_removed {
                r.start_offset = r.start_offset + inserted_len - count;
            }
        }
        if r.end_container == node {
            if r.end_offset > offset && r.end_offset <= end_removed {
                r.end_offset = offset;
            } else if r.end_offset > end_removed {
                r.end_offset = r.end_offset + inserted_len - count;
            }
        }
    }
}

/// Adjusts all live `Range` boundary points during `Text.prototype.splitText(offset)`
/// when `old_node` has a parent `parent` at child index `node_index` (WHATWG DOM §4.11).
pub fn adjust_ranges_for_split_text(
    host: &SharedHost,
    old_node: NodeId,
    new_node: NodeId,
    parent: NodeId,
    node_index: usize,
    offset: usize,
) {
    let mut hb = host.borrow_mut();
    for r in hb.selection.ranges.values_mut() {
        if !r.is_live {
            continue;
        }
        if r.start_container == old_node && r.start_offset > offset {
            r.start_container = new_node;
            r.start_offset -= offset;
        } else if r.start_container == parent && r.start_offset > node_index {
            r.start_offset += 1;
        }
        if r.end_container == old_node && r.end_offset > offset {
            r.end_container = new_node;
            r.end_offset -= offset;
        } else if r.end_container == parent && r.end_offset > node_index {
            r.end_offset += 1;
        }
    }
}

/// Adjusts all live `Range` boundary points when `count` nodes are inserted into
/// `parent` at child index `insert_index` (WHATWG DOM §4.2.4 "insert").
pub fn adjust_ranges_for_insert_nodes(
    host: &SharedHost,
    parent: NodeId,
    insert_index: usize,
    count: usize,
) {
    if count == 0 {
        return;
    }
    let mut hb = host.borrow_mut();
    for r in hb.selection.ranges.values_mut() {
        if !r.is_live {
            continue;
        }
        if r.start_container == parent && r.start_offset > insert_index {
            r.start_offset += count;
        }
        if r.end_container == parent && r.end_offset > insert_index {
            r.end_offset += count;
        }
    }
}

/// Adjusts all live `Range` boundary points immediately BEFORE `node` is detached from
/// `old_parent` at child index `old_index` (WHATWG DOM §4.2.4 "remove").
pub fn adjust_ranges_for_remove_node(
    host: &SharedHost,
    node: NodeId,
    old_parent: NodeId,
    old_index: usize,
) {
    let doc_rc = Rc::clone(&host.borrow().document);
    let doc = doc_rc.borrow();
    let mut hb = host.borrow_mut();
    for r in hb.selection.ranges.values_mut() {
        if !r.is_live {
            continue;
        }
        if r.start_container == node || doc.ancestors(r.start_container).any(|a| a == node) {
            r.start_container = old_parent;
            r.start_offset = old_index;
        } else if r.start_container == old_parent && r.start_offset > old_index {
            r.start_offset -= 1;
        }
        if r.end_container == node || doc.ancestors(r.end_container).any(|a| a == node) {
            r.end_container = old_parent;
            r.end_offset = old_index;
        } else if r.end_container == old_parent && r.end_offset > old_index {
            r.end_offset -= 1;
        }
    }
}

/// Adjusts all live `Range` boundary points during `Node.prototype.normalize()`
/// when merging `removed_text` (at `removed_index` in `parent`) into `kept_text`
/// at `current_len` (WHATWG DOM §4.4).
pub fn adjust_ranges_for_normalize_merge(
    host: &SharedHost,
    kept_text: NodeId,
    removed_text: NodeId,
    parent: NodeId,
    removed_index: usize,
    current_len: usize,
) {
    let mut hb = host.borrow_mut();
    for r in hb.selection.ranges.values_mut() {
        if !r.is_live {
            continue;
        }
        if r.start_container == removed_text {
            r.start_container = kept_text;
            r.start_offset += current_len;
        } else if r.start_container == parent && r.start_offset == removed_index {
            r.start_container = kept_text;
            r.start_offset = current_len;
        } else if r.start_container == parent && r.start_offset > removed_index {
            r.start_offset -= 1;
        }
        if r.end_container == removed_text {
            r.end_container = kept_text;
            r.end_offset += current_len;
        } else if r.end_container == parent && r.end_offset == removed_index {
            r.end_container = kept_text;
            r.end_offset = current_len;
        } else if r.end_container == parent && r.end_offset > removed_index {
            r.end_offset -= 1;
        }
    }
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
        if let Some(NodeData::Text(t) | NodeData::CDataSection(t)) = doc.get(start_node) {
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

fn node_length(doc: &Document, nid: NodeId) -> usize {
    match doc.get(nid) {
        Some(
            NodeData::Text(t)
            | NodeData::CDataSection(t)
            | NodeData::Comment(t)
            | NodeData::ProcessingInstruction { data: t, .. },
        ) => t.encode_utf16().count(),
        Some(NodeData::DocumentType { .. }) => 0,
        _ => doc.children(nid).count(),
    }
}

fn root_of(doc: &Document, mut nid: NodeId) -> NodeId {
    while let Some(p) = doc.parent(nid) {
        nid = p;
    }
    nid
}

/// Compares two boundary points `(a_node, a_off)` and `(b_node, b_off)` in the same tree.
/// Returns `None` if in different trees, otherwise `Some(Ordering)`.
fn compare_bp(
    doc: &Document,
    a_node: NodeId,
    a_off: usize,
    b_node: NodeId,
    b_off: usize,
) -> Option<std::cmp::Ordering> {
    if a_node == b_node {
        return Some(a_off.cmp(&b_off));
    }
    if root_of(doc, a_node) != root_of(doc, b_node) {
        return None;
    }
    let pos = crate::dom_interfaces::compare_document_position(doc, a_node, b_node);
    // If b_node is a descendant of a_node (`CONTAINED_BY` = 0x10):
    if (pos & 0x10) != 0 {
        let mut child = b_node;
        while let Some(p) = doc.parent(child) {
            if p == a_node {
                break;
            }
            child = p;
        }
        let child_idx = doc.children(a_node).position(|c| c == child).unwrap_or(0);
        return Some(if a_off <= child_idx {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        });
    }
    // If a_node is a descendant of b_node (`CONTAINS` = 0x08):
    if (pos & 0x08) != 0 {
        let mut child = a_node;
        while let Some(p) = doc.parent(child) {
            if p == b_node {
                break;
            }
            child = p;
        }
        let child_idx = doc.children(b_node).position(|c| c == child).unwrap_or(0);
        return Some(if child_idx < b_off {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        });
    }
    if (pos & 0x04) != 0 {
        Some(std::cmp::Ordering::Less)
    } else {
        Some(std::cmp::Ordering::Greater)
    }
}

fn to_u32_offset(vm: &mut JsVm, val: JsValue) -> Result<usize, rix_js::JsException> {
    let n = vm.to_number(val)?;
    if !n.is_finite() || n == 0.0 {
        return Ok(0);
    }
    Ok(n.trunc().rem_euclid(4_294_967_296.0) as u32 as usize)
}

fn alloc_range_object(vm: &mut JsVm, host: &SharedHost, data: RangeData) -> HeapHandle {
    let proto = if data.is_live {
        host.borrow()
            .selection
            .range_proto
            .unwrap_or(vm.object_proto)
    } else {
        host.borrow()
            .selection
            .static_range_proto
            .unwrap_or(vm.object_proto)
    };
    let range_obj = vm.heap.alloc_object(rix_js::JsObject::with_kind(
        rix_js::ObjectKind::Ordinary,
        Some(proto),
    ));
    host.borrow_mut().selection.ranges.insert(range_obj, data);
    range_obj
}

fn set_range_boundary(
    vm: &mut JsVm,
    host: &SharedHost,
    rh: HeapHandle,
    nid: NodeId,
    off: usize,
    is_start: bool,
) -> Result<(), rix_js::JsException> {
    {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        if matches!(doc.get(nid), Some(NodeData::DocumentType { .. })) {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "InvalidNodeTypeError",
                "Range boundary node cannot be a DocumentType",
            ));
        }
        let len = node_length(&doc, nid);
        if off > len {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "IndexSizeError",
                "Range offset exceeds node length",
            ));
        }
    }
    let doc_rc = Rc::clone(&host.borrow().document);
    let doc = doc_rc.borrow();
    let mut hb = host.borrow_mut();
    let Some(r) = hb.selection.ranges.get_mut(&rh) else {
        return Err(vm.type_error("Illegal invocation: receiver is not a Range"));
    };
    if is_start {
        let collapse = matches!(
            compare_bp(&doc, nid, off, r.end_container, r.end_offset),
            None | Some(std::cmp::Ordering::Greater)
        );
        r.start_container = nid;
        r.start_offset = off;
        if collapse {
            r.end_container = nid;
            r.end_offset = off;
        }
    } else {
        let collapse = matches!(
            compare_bp(&doc, r.start_container, r.start_offset, nid, off),
            None | Some(std::cmp::Ordering::Greater)
        );
        r.end_container = nid;
        r.end_offset = off;
        if collapse {
            r.start_container = nid;
            r.start_offset = off;
        }
    }
    Ok(())
}

fn install_range_prototypes(
    vm: &mut JsVm,
    host: &SharedHost,
    abs_proto: HeapHandle,
    range_proto: HeapHandle,
) {
    // AbstractRange.prototype getters
    let h = Rc::clone(host);
    let g_sc = vm.alloc_host_fn("get startContainer", move |vm, this_val, _| {
        let rh = this_val
            .as_object()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let n = h
            .borrow()
            .selection
            .ranges
            .get(&rh)
            .map(|r| r.start_container)
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(wrap_node(vm, &h, n))
    });
    define_host_accessor(vm, abs_proto, "startContainer", Some(g_sc), None);

    let h = Rc::clone(host);
    let g_so = vm.alloc_host_fn("get startOffset", move |vm, this_val, _| {
        let rh = this_val
            .as_object()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let o = h
            .borrow()
            .selection
            .ranges
            .get(&rh)
            .map(|r| r.start_offset)
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(JsValue::from_f64(o as f64))
    });
    define_host_accessor(vm, abs_proto, "startOffset", Some(g_so), None);

    let h = Rc::clone(host);
    let g_ec = vm.alloc_host_fn("get endContainer", move |vm, this_val, _| {
        let rh = this_val
            .as_object()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let n = h
            .borrow()
            .selection
            .ranges
            .get(&rh)
            .map(|r| r.end_container)
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(wrap_node(vm, &h, n))
    });
    define_host_accessor(vm, abs_proto, "endContainer", Some(g_ec), None);

    let h = Rc::clone(host);
    let g_eo = vm.alloc_host_fn("get endOffset", move |vm, this_val, _| {
        let rh = this_val
            .as_object()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let o = h
            .borrow()
            .selection
            .ranges
            .get(&rh)
            .map(|r| r.end_offset)
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(JsValue::from_f64(o as f64))
    });
    define_host_accessor(vm, abs_proto, "endOffset", Some(g_eo), None);

    let h = Rc::clone(host);
    let g_col = vm.alloc_host_fn("get collapsed", move |vm, this_val, _| {
        let rh = this_val
            .as_object()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let col = h
            .borrow()
            .selection
            .ranges
            .get(&rh)
            .map(|r| r.start_container == r.end_container && r.start_offset == r.end_offset)
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(JsValue::from_bool(col))
    });
    define_host_accessor(vm, abs_proto, "collapsed", Some(g_col), None);

    // Range.prototype.commonAncestorContainer
    let h = Rc::clone(host);
    let g_cac = vm.alloc_host_fn("get commonAncestorContainer", move |vm, this_val, _| {
        let rh = this_val
            .as_object()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let r = h
            .borrow()
            .selection
            .ranges
            .get(&rh)
            .copied()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let cac = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            let end_ancs: Vec<NodeId> = doc.ancestors(r.end_container).collect();
            let mut cur = r.start_container;
            while !end_ancs.contains(&cur) {
                match doc.parent(cur) {
                    Some(p) => cur = p,
                    None => break,
                }
            }
            cur
        };
        Ok(wrap_node(vm, &h, cac))
    });
    define_host_accessor(
        vm,
        range_proto,
        "commonAncestorContainer",
        Some(g_cac),
        None,
    );

    // setStart / setEnd
    let h = Rc::clone(host);
    define_host_method(vm, range_proto, "setStart", move |vm, this_val, args| {
        let rh = this_val
            .as_object()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let Some(nid) = extract_node(vm, &h, arg(args, 0)) else {
            return Err(vm.type_error("Range.setStart requires a Node"));
        };
        let off = to_u32_offset(vm, arg(args, 1))?;
        set_range_boundary(vm, &h, rh, nid, off, true)?;
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, range_proto, "setEnd", move |vm, this_val, args| {
        let rh = this_val
            .as_object()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let Some(nid) = extract_node(vm, &h, arg(args, 0)) else {
            return Err(vm.type_error("Range.setEnd requires a Node"));
        };
        let off = to_u32_offset(vm, arg(args, 1))?;
        set_range_boundary(vm, &h, rh, nid, off, false)?;
        Ok(JsValue::UNDEFINED)
    });

    // setStartBefore / setStartAfter / setEndBefore / setEndAfter
    for (mname, is_start, delta) in [
        ("setStartBefore", true, 0usize),
        ("setStartAfter", true, 1),
        ("setEndBefore", false, 0),
        ("setEndAfter", false, 1),
    ] {
        let h = Rc::clone(host);
        define_host_method(vm, range_proto, mname, move |vm, this_val, args| {
            let rh = this_val
                .as_object()
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            let Some(nid) = extract_node(vm, &h, arg(args, 0)) else {
                return Err(vm.type_error("Argument must be a Node"));
            };
            let (parent, idx) = {
                let hb = h.borrow();
                let doc = hb.document.borrow();
                let Some(p) = doc.parent(nid) else {
                    return Err(crate::web_apis::create_dom_exception(
                        vm,
                        "InvalidNodeTypeError",
                        "Node has no parent",
                    ));
                };
                let idx = doc.children(p).position(|c| c == nid).unwrap_or(0);
                (p, idx)
            };
            set_range_boundary(vm, &h, rh, parent, idx + delta, is_start)?;
            Ok(JsValue::UNDEFINED)
        });
    }

    // selectNodeContents
    let h = Rc::clone(host);
    define_host_method(
        vm,
        range_proto,
        "selectNodeContents",
        move |vm, this_val, args| {
            let rh = this_val
                .as_object()
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            let Some(nid) = extract_node(vm, &h, arg(args, 0)) else {
                return Err(vm.type_error("Range.selectNodeContents requires a Node"));
            };
            let end_off = {
                let hb = h.borrow();
                let doc = hb.document.borrow();
                if matches!(doc.get(nid), Some(NodeData::DocumentType { .. })) {
                    return Err(crate::web_apis::create_dom_exception(
                        vm,
                        "InvalidNodeTypeError",
                        "Cannot select contents of a DocumentType",
                    ));
                }
                node_length(&doc, nid)
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

    // selectNode
    let h = Rc::clone(host);
    define_host_method(vm, range_proto, "selectNode", move |vm, this_val, args| {
        let rh = this_val
            .as_object()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let Some(nid) = extract_node(vm, &h, arg(args, 0)) else {
            return Err(vm.type_error("Range.selectNode requires a Node"));
        };
        let (parent, idx) = {
            let hb = h.borrow();
            let doc = hb.document.borrow();
            let Some(p) = doc.parent(nid) else {
                return Err(crate::web_apis::create_dom_exception(
                    vm,
                    "InvalidNodeTypeError",
                    "Node has no parent",
                ));
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

    // collapse
    let h = Rc::clone(host);
    define_host_method(vm, range_proto, "collapse", move |vm, this_val, args| {
        let rh = this_val
            .as_object()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
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

    // cloneRange / detach
    let h = Rc::clone(host);
    define_host_method(vm, range_proto, "cloneRange", move |vm, this_val, _| {
        let rh = this_val
            .as_object()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let r = h
            .borrow()
            .selection
            .ranges
            .get(&rh)
            .copied()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let cloned = alloc_range_object(vm, &h, RangeData { is_live: true, ..r });
        Ok(JsValue::from_object(cloned))
    });
    define_host_method(vm, range_proto, "detach", |_vm, _, _| {
        Ok(JsValue::UNDEFINED)
    });

    // compareBoundaryPoints(how, sourceRange)
    let h = Rc::clone(host);
    define_host_method(
        vm,
        range_proto,
        "compareBoundaryPoints",
        move |vm, this_val, args| {
            let rh = this_val
                .as_object()
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            let how = to_u32_offset(vm, arg(args, 0))? & 0xFFFF;
            if how > 3 {
                return Err(crate::web_apis::create_dom_exception(
                    vm,
                    "NotSupportedError",
                    "Invalid compareBoundaryPoints mode",
                ));
            }
            let Some(src_h) = arg(args, 1).as_object() else {
                return Err(vm.type_error("sourceRange must be a Range"));
            };
            let (r1, r2) = {
                let hb = h.borrow();
                let r1 = hb
                    .selection
                    .ranges
                    .get(&rh)
                    .copied()
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                let r2 = hb
                    .selection
                    .ranges
                    .get(&src_h)
                    .copied()
                    .ok_or_else(|| vm.type_error("sourceRange must be a Range"))?;
                (r1, r2)
            };
            let (p1, p2) = match how {
                0 => (
                    (r1.start_container, r1.start_offset),
                    (r2.start_container, r2.start_offset),
                ),
                1 => (
                    (r1.end_container, r1.end_offset),
                    (r2.start_container, r2.start_offset),
                ),
                2 => (
                    (r1.end_container, r1.end_offset),
                    (r2.end_container, r2.end_offset),
                ),
                _ => (
                    (r1.start_container, r1.start_offset),
                    (r2.end_container, r2.end_offset),
                ),
            };
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            let Some(ord) = compare_bp(&doc, p1.0, p1.1, p2.0, p2.1) else {
                return Err(crate::web_apis::create_dom_exception(
                    vm,
                    "WrongDocumentError",
                    "Ranges are in different trees",
                ));
            };
            let res = match ord {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            };
            Ok(JsValue::from_i32(res))
        },
    );

    // comparePoint(node, offset) & isPointInRange(node, offset)
    for (mname, as_bool) in [("comparePoint", false), ("isPointInRange", true)] {
        let h = Rc::clone(host);
        define_host_method(vm, range_proto, mname, move |vm, this_val, args| {
            let rh = this_val
                .as_object()
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            let Some(nid) = extract_node(vm, &h, arg(args, 0)) else {
                return Err(vm.type_error("Argument 1 must be a Node"));
            };
            let off = to_u32_offset(vm, arg(args, 1))?;
            let r = h
                .borrow()
                .selection
                .ranges
                .get(&rh)
                .copied()
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            if root_of(&doc, nid) != root_of(&doc, r.start_container) {
                if as_bool {
                    return Ok(JsValue::FALSE);
                }
                return Err(crate::web_apis::create_dom_exception(
                    vm,
                    "WrongDocumentError",
                    "Node is not in the same tree as Range",
                ));
            }
            if matches!(doc.get(nid), Some(NodeData::DocumentType { .. })) {
                return Err(crate::web_apis::create_dom_exception(
                    vm,
                    "InvalidNodeTypeError",
                    "Node cannot be a DocumentType",
                ));
            }
            if off > node_length(&doc, nid) {
                return Err(crate::web_apis::create_dom_exception(
                    vm,
                    "IndexSizeError",
                    "Offset exceeds node length",
                ));
            }
            let cmp_s = compare_bp(&doc, nid, off, r.start_container, r.start_offset);
            let cmp_e = compare_bp(&doc, nid, off, r.end_container, r.end_offset);
            let res = if cmp_s == Some(std::cmp::Ordering::Less) {
                -1
            } else if cmp_e == Some(std::cmp::Ordering::Greater) {
                1
            } else {
                0
            };
            if as_bool {
                Ok(JsValue::from_bool(res == 0))
            } else {
                Ok(JsValue::from_i32(res))
            }
        });
    }

    // intersectsNode(node)
    let h = Rc::clone(host);
    define_host_method(
        vm,
        range_proto,
        "intersectsNode",
        move |vm, this_val, args| {
            let rh = this_val
                .as_object()
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            let Some(nid) = extract_node(vm, &h, arg(args, 0)) else {
                return Err(vm.type_error("intersectsNode requires a Node"));
            };
            let r = h
                .borrow()
                .selection
                .ranges
                .get(&rh)
                .copied()
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            if root_of(&doc, nid) != root_of(&doc, r.start_container) {
                return Ok(JsValue::FALSE);
            }
            let Some(parent) = doc.parent(nid) else {
                return Ok(JsValue::TRUE);
            };
            let idx = doc.children(parent).position(|c| c == nid).unwrap_or(0);
            let start_before_end = matches!(
                compare_bp(&doc, parent, idx, r.end_container, r.end_offset),
                Some(std::cmp::Ordering::Less)
            );
            let end_after_start = matches!(
                compare_bp(&doc, parent, idx + 1, r.start_container, r.start_offset),
                Some(std::cmp::Ordering::Greater)
            );
            Ok(JsValue::from_bool(start_before_end && end_after_start))
        },
    );

    // createContextualFragment(string)
    let h = Rc::clone(host);
    define_host_method(
        vm,
        range_proto,
        "createContextualFragment",
        move |vm, this_val, args| {
            let rh = this_val
                .as_object()
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            let html = vm.value_to_rust_string(arg(args, 0));
            let r = h
                .borrow()
                .selection
                .ranges
                .get(&rh)
                .copied()
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            let (ctx_name, owner_doc) = {
                let hb = h.borrow();
                let doc = hb.document.borrow();
                let owner_doc = if matches!(doc.get(r.start_container), Some(NodeData::Document)) {
                    r.start_container
                } else {
                    hb.owner_doc_of(r.start_container)
                };
                let ctx_el = match doc.get(r.start_container) {
                    Some(NodeData::Element(e)) => Some(e.name.clone()),
                    Some(
                        NodeData::Text(_)
                        | NodeData::CDataSection(_)
                        | NodeData::Comment(_)
                        | NodeData::ProcessingInstruction { .. },
                    ) => doc
                        .parent(r.start_container)
                        .and_then(|p| match doc.get(p) {
                            Some(NodeData::Element(e)) => Some(e.name.clone()),
                            _ => None,
                        }),
                    _ => None,
                };
                let ctx_name = match ctx_el {
                    Some(q)
                        if &*q.ns == crate::dom_interfaces::HTML_NS
                            && matches!(&*q.local, "html" | "body") =>
                    {
                        Some(QualName::new(
                            None,
                            Namespace::from(crate::dom_interfaces::HTML_NS),
                            LocalName::from("body"),
                        ))
                    }
                    Some(q) => Some(q),
                    None => Some(QualName::new(
                        None,
                        Namespace::from(crate::dom_interfaces::HTML_NS),
                        LocalName::from("body"),
                    )),
                };
                (ctx_name, owner_doc)
            };
            let (frag_doc, frag_roots) = rix_html::parse_html_fragment(&html, ctx_name.as_ref());
            let frag_id = {
                let doc_rc = Rc::clone(&h.borrow().document);
                let mut doc = doc_rc.borrow_mut();
                let f = doc.new_node(NodeData::DocumentFragment);
                for src in frag_roots {
                    let cloned = doc.clone_subtree_from(&frag_doc, src);
                    doc.append_child(f, cloned);
                }
                f
            };
            h.borrow_mut().adopt_subtree(frag_id, owner_doc);
            Ok(wrap_node(vm, &h, frag_id))
        },
    );

    // insertNode(node) (WHATWG DOM §5.5)
    let h = Rc::clone(host);
    define_host_method(vm, range_proto, "insertNode", move |vm, this_val, args| {
        let rh = this_val
            .as_object()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let Some(node) = extract_node(vm, &h, arg(args, 0)) else {
            return Err(vm.type_error("insertNode requires a Node"));
        };
        let r = h
            .borrow()
            .selection
            .ranges
            .get(&rh)
            .copied()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let (is_char_data, parent_opt, ref_child) = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(r.start_container) {
                Some(NodeData::ProcessingInstruction { .. } | NodeData::Comment(_)) => {
                    return Err(crate::web_apis::create_dom_exception(
                        vm,
                        "HierarchyRequestError",
                        "Cannot insert into Comment or ProcessingInstruction",
                    ));
                }
                Some(NodeData::Text(_) | NodeData::CDataSection(_)) => {
                    let Some(p) = doc.parent(r.start_container) else {
                        return Err(crate::web_apis::create_dom_exception(
                            vm,
                            "HierarchyRequestError",
                            "Text container has no parent",
                        ));
                    };
                    (true, Some(p), None)
                }
                _ => {
                    let ref_c = doc.children(r.start_container).nth(r.start_offset);
                    (false, Some(r.start_container), ref_c)
                }
            }
        };
        if is_char_data {
            let parent = parent_opt.unwrap();
            let text_val = wrap_node(vm, &h, r.start_container);
            let split_k = vm.heap.intern_key("splitText");
            let split_fn = vm.get_property_by_key(text_val.as_object().unwrap(), split_k)?;
            let new_text_val = vm.call_value(
                split_fn,
                text_val,
                &[JsValue::from_f64(r.start_offset as f64)],
            )?;
            let new_text_id = extract_node(vm, &h, new_text_val);
            crate::dom_bridge::insert_node_into_parent(vm, &h, parent, node, new_text_id)?;
        } else if let Some(parent) = parent_opt {
            crate::dom_bridge::insert_node_into_parent(vm, &h, parent, node, ref_child)?;
        }
        Ok(JsValue::UNDEFINED)
    });

    // deleteContents() / extractContents() / cloneContents()
    let h = Rc::clone(host);
    define_host_method(vm, range_proto, "deleteContents", move |vm, this_val, _| {
        let rh = this_val
            .as_object()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let r = h
            .borrow()
            .selection
            .ranges
            .get(&rh)
            .copied()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        if r.start_container == r.end_container && r.start_offset == r.end_offset {
            return Ok(JsValue::UNDEFINED);
        }
        if r.start_container == r.end_container {
            let is_cd = matches!(
                h.borrow().document.borrow().get(r.start_container),
                Some(
                    NodeData::Text(_)
                        | NodeData::CDataSection(_)
                        | NodeData::Comment(_)
                        | NodeData::ProcessingInstruction { .. }
                )
            );
            if is_cd {
                let count = r.end_offset.saturating_sub(r.start_offset);
                crate::dom_interfaces::character_data_replace(
                    vm,
                    &h,
                    r.start_container,
                    r.start_offset,
                    count,
                    "",
                )?;
                return Ok(JsValue::UNDEFINED);
            }
            let kids: Vec<NodeId> = h
                .borrow()
                .document
                .borrow()
                .children(r.start_container)
                .skip(r.start_offset)
                .take(r.end_offset.saturating_sub(r.start_offset))
                .collect();
            for kid in kids {
                crate::dom_bridge::remove_node_from_parent(vm, &h, r.start_container, kid)?;
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, range_proto, "cloneContents", move |vm, this_val, _| {
        let rh = this_val
            .as_object()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let r = h
            .borrow()
            .selection
            .ranges
            .get(&rh)
            .copied()
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        let owner_doc = h.borrow().owner_doc_of(r.start_container);
        let frag = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            let f = doc.new_node(NodeData::DocumentFragment);
            if r.start_container == r.end_container {
                if let Some(NodeData::Text(t) | NodeData::CDataSection(t)) =
                    doc.get(r.start_container)
                {
                    let sub = slice_utf16(t, r.start_offset, r.end_offset).to_string();
                    let tn = doc.create_text(&sub);
                    doc.append_child(f, tn);
                } else {
                    let kids: Vec<NodeId> = doc
                        .children(r.start_container)
                        .skip(r.start_offset)
                        .take(r.end_offset.saturating_sub(r.start_offset))
                        .collect();
                    let snap = doc.clone();
                    for kid in kids {
                        let c = doc.clone_subtree_from(&snap, kid);
                        doc.append_child(f, c);
                    }
                }
            }
            f
        };
        h.borrow_mut().adopt_subtree(frag, owner_doc);
        Ok(wrap_node(vm, &h, frag))
    });

    let h = Rc::clone(host);
    define_host_method(
        vm,
        range_proto,
        "extractContents",
        move |vm, this_val, _| {
            let clone_k = vm.heap.intern_key("cloneContents");
            let clone_fn = vm.get_property_by_key(this_val.as_object().unwrap(), clone_k)?;
            let frag = vm.call_value(clone_fn, this_val, &[])?;
            let del_k = vm.heap.intern_key("deleteContents");
            let del_fn = vm.get_property_by_key(this_val.as_object().unwrap(), del_k)?;
            let _ = vm.call_value(del_fn, this_val, &[])?;
            let _ = &h;
            Ok(frag)
        },
    );

    // surroundContents(newParent) — WHATWG DOM §5.5
    let h = Rc::clone(host);
    define_host_method(
        vm,
        range_proto,
        "surroundContents",
        move |vm, this_val, args| {
            let rh = this_val
                .as_object()
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            let r = h
                .borrow()
                .selection
                .ranges
                .get(&rh)
                .copied()
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            let Some(new_parent) = extract_node(vm, &h, arg(args, 0)) else {
                return Err(vm.type_error("Range.surroundContents requires a Node"));
            };
            // Step 1: If a non-Text node is partially contained in this range, throw InvalidStateError.
            {
                let doc_rc = Rc::clone(&h.borrow().document);
                let doc = doc_rc.borrow();
                let mut start_ancs = Vec::new();
                let mut cur = Some(r.start_container);
                while let Some(n) = cur {
                    start_ancs.push(n);
                    cur = doc.parent(n);
                }
                let mut end_ancs = Vec::new();
                let mut cur = Some(r.end_container);
                while let Some(n) = cur {
                    end_ancs.push(n);
                    cur = doc.parent(n);
                }
                let partially_non_text = start_ancs
                    .iter()
                    .filter(|n| !end_ancs.contains(n))
                    .chain(end_ancs.iter().filter(|n| !start_ancs.contains(n)))
                    .any(|&n| !matches!(doc.get(n), Some(NodeData::Text(_))));
                if partially_non_text {
                    return Err(crate::web_apis::create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "Cannot surroundContents when a non-Text node is partially contained",
                    ));
                }
                // Step 2: If newParent is Document, DocumentType, or DocumentFragment, throw InvalidNodeTypeError.
                if matches!(
                    doc.get(new_parent),
                    Some(
                        NodeData::Document
                            | NodeData::DocumentType { .. }
                            | NodeData::DocumentFragment
                            | NodeData::ShadowRoot { .. }
                    )
                ) {
                    return Err(crate::web_apis::create_dom_exception(
                        vm,
                        "InvalidNodeTypeError",
                        "Cannot surroundContents with Document, DocumentType, or DocumentFragment",
                    ));
                }
            }
            let _ = rh;
            // Step 3: Extract contents into fragment.
            let this_h = this_val.as_object().unwrap();
            let k_ext = vm.heap.intern_key("extractContents");
            let ext_fn = vm.get_property_by_key(this_h, k_ext)?;
            let frag_val = vm.call_value(ext_fn, this_val, &[])?;
            // Step 4: Remove all children of newParent.
            let existing_kids: Vec<NodeId> =
                h.borrow().document.borrow().children(new_parent).collect();
            let np_val = wrap_node(vm, &h, new_parent);
            if !existing_kids.is_empty() {
                let k_rem = vm.heap.intern_key("removeChild");
                if let Some(np_h) = np_val.as_object() {
                    let rem_fn = vm.get_property_by_key(np_h, k_rem)?;
                    for kid in existing_kids {
                        let kv = wrap_node(vm, &h, kid);
                        let _ = vm.call_value(rem_fn, np_val, &[kv])?;
                    }
                }
            }
            // Step 5: Insert newParent into this range.
            let k_ins = vm.heap.intern_key("insertNode");
            let ins_fn = vm.get_property_by_key(this_h, k_ins)?;
            let _ = vm.call_value(ins_fn, this_val, &[np_val])?;
            // Step 6: Append fragment to newParent.
            let k_app = vm.heap.intern_key("appendChild");
            if let Some(np_h) = np_val.as_object() {
                let app_fn = vm.get_property_by_key(np_h, k_app)?;
                let _ = vm.call_value(app_fn, np_val, &[frag_val])?;
            }
            // Step 7: Select newParent in this range.
            let k_sel = vm.heap.intern_key("selectNode");
            let sel_fn = vm.get_property_by_key(this_h, k_sel)?;
            let _ = vm.call_value(sel_fn, this_val, &[np_val])?;
            Ok(JsValue::UNDEFINED)
        },
    );

    // toString()
    let h = Rc::clone(host);
    define_host_method(vm, range_proto, "toString", move |vm, this_val, _| {
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

    // Range boundary comparison constants on Range.prototype
    for (cname, cval) in [
        ("START_TO_START", 0),
        ("START_TO_END", 1),
        ("END_TO_END", 2),
        ("END_TO_START", 3),
    ] {
        let k = vm.heap.intern_key(cname);
        vm.heap.object_define_data(
            range_proto,
            k,
            JsValue::from_i32(cval),
            PropertyAttrs::ENUMERABLE,
            &mut vm.shapes,
        );
    }
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
        hb.selection.selected_range = None;
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sel, "empty", move |_vm, _, _| {
        let mut hb = h.borrow_mut();
        hb.selection.anchor_node = None;
        hb.selection.focus_node = None;
        hb.selection.anchor_offset = 0;
        hb.selection.focus_offset = 0;
        hb.selection.selected_range = None;
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sel, "addRange", move |_vm, _, args| {
        let Some(rh) = arg(args, 0).as_object() else {
            return Ok(JsValue::UNDEFINED);
        };
        let data = h.borrow().selection.ranges.get(&rh).copied();
        if let Some(r) = data {
            // Only add ranges whose root is this document (matches browser Selection rules).
            let main_root = h.borrow().document.borrow().root();
            let same_doc = root_of(&h.borrow().document.borrow(), r.start_container) == main_root;
            if same_doc {
                let mut hb = h.borrow_mut();
                hb.selection.anchor_node = Some(r.start_container);
                hb.selection.anchor_offset = r.start_offset;
                hb.selection.focus_node = Some(r.end_container);
                hb.selection.focus_offset = r.end_offset;
                hb.selection.selected_range = Some(rh);
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    define_host_method(vm, sel, "getRangeAt", move |vm, _, args| {
        let idx = arg(args, 0).to_number_f64().unwrap_or(0.0) as isize;
        if idx == 0 {
            if let Some(rh) = h.borrow().selection.selected_range {
                if vm.heap.get_object(rh).is_some() {
                    return Ok(JsValue::from_object(rh));
                }
            }
        }
        let pair = {
            let hb = h.borrow();
            let s = &hb.selection;
            match (s.anchor_node, s.focus_node) {
                (Some(a), Some(f)) if idx == 0 => Some((a, s.anchor_offset, f, s.focus_offset)),
                _ => None,
            }
        };
        let Some((a, ao, f, fo)) = pair else {
            return Err(crate::web_apis::create_dom_exception(
                vm,
                "IndexSizeError",
                "Selection range index out of bounds",
            ));
        };
        let rh = alloc_range_object(
            vm,
            &h,
            RangeData {
                start_container: a,
                start_offset: ao,
                end_container: f,
                end_offset: fo,
                is_live: true,
            },
        );
        Ok(JsValue::from_object(rh))
    });

    let h = Rc::clone(host);
    define_host_method(vm, sel, "toString", move |vm, _, _| {
        let pair = {
            let hb = h.borrow();
            let s = &hb.selection;
            if let Some(rh) = s.selected_range {
                s.ranges.get(&rh).map(|r| {
                    (
                        r.start_container,
                        r.start_offset,
                        r.end_container,
                        r.end_offset,
                    )
                })
            } else {
                match (s.anchor_node, s.focus_node) {
                    (Some(a), Some(f)) => Some((a, s.anchor_offset, f, s.focus_offset)),
                    _ => None,
                }
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
/// `AbstractRange`, `StaticRange`, and `Range` constructors on `vm`.
pub fn install_selection_and_range(vm: &mut JsVm, host: &SharedHost) {
    let dp = host.borrow().prototypes.document_proto;

    let abs_proto = crate::dom_interfaces::make_proto(vm, vm.object_proto);
    let range_proto = crate::dom_interfaces::make_proto(vm, abs_proto);
    let static_proto = crate::dom_interfaces::make_proto(vm, abs_proto);
    {
        let mut hb = host.borrow_mut();
        hb.selection.abstract_range_proto = Some(abs_proto);
        hb.selection.range_proto = Some(range_proto);
        hb.selection.static_range_proto = Some(static_proto);
    }

    install_range_prototypes(vm, host, abs_proto, range_proto);

    let abs_ctor =
        crate::dom_interfaces::register_dom_interface(vm, "AbstractRange", abs_proto, None, false);

    // StaticRange constructor (`new StaticRange({ startContainer, startOffset, endContainer, endOffset })`)
    crate::dom_interfaces::define_to_string_tag(vm, static_proto, "StaticRange");
    let h_sr = Rc::clone(host);
    let sr_ctor = vm.alloc_host_fn("StaticRange", move |vm, _, args| {
        let Some(init) = arg(args, 0).as_object() else {
            return Err(vm.type_error("StaticRange requires a StaticRangeInit dictionary"));
        };
        let k_sc = vm.heap.intern_key("startContainer");
        let k_so = vm.heap.intern_key("startOffset");
        let k_ec = vm.heap.intern_key("endContainer");
        let k_eo = vm.heap.intern_key("endOffset");
        let sc_v = vm.get_property_by_key(init, k_sc)?;
        let so_v = vm.get_property_by_key(init, k_so)?;
        let ec_v = vm.get_property_by_key(init, k_ec)?;
        let eo_v = vm.get_property_by_key(init, k_eo)?;
        let Some(sc) = extract_node(vm, &h_sr, sc_v) else {
            return Err(vm.type_error("startContainer must be a Node"));
        };
        let Some(ec) = extract_node(vm, &h_sr, ec_v) else {
            return Err(vm.type_error("endContainer must be a Node"));
        };
        {
            let hb = h_sr.borrow();
            let doc = hb.document.borrow();
            if matches!(doc.get(sc), Some(NodeData::DocumentType { .. }))
                || matches!(doc.get(ec), Some(NodeData::DocumentType { .. }))
            {
                return Err(crate::web_apis::create_dom_exception(
                    vm,
                    "InvalidNodeTypeError",
                    "StaticRange container cannot be a DocumentType",
                ));
            }
        }
        let so = to_u32_offset(vm, so_v)?;
        let eo = to_u32_offset(vm, eo_v)?;
        let rh = alloc_range_object(
            vm,
            &h_sr,
            RangeData {
                start_container: sc,
                start_offset: so,
                end_container: ec,
                end_offset: eo,
                is_live: false,
            },
        );
        Ok(JsValue::from_object(rh))
    });
    vm.heap
        .object_set_prototype(sr_ctor, Some(abs_ctor), &mut vm.shapes);
    vm.heap.object_define_data(
        sr_ctor,
        vm.key_prototype,
        JsValue::from_object(static_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        static_proto,
        vm.key_constructor,
        JsValue::from_object(sr_ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.mark_constructor(sr_ctor);
    define_global(vm, "StaticRange", JsValue::from_object(sr_ctor));

    // Selection getters & methods
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

    // document.createRange() — uses receiver document as initial start/end container!
    let h = Rc::clone(host);
    define_host_method(vm, dp, "createRange", move |vm, this_val, _| {
        let doc_id =
            require_node(vm, &h, this_val).unwrap_or_else(|_| h.borrow().document.borrow().root());
        let rh = alloc_range_object(
            vm,
            &h,
            RangeData {
                start_container: doc_id,
                start_offset: 0,
                end_container: doc_id,
                end_offset: 0,
                is_live: true,
            },
        );
        Ok(JsValue::from_object(rh))
    });

    // Global `new Range()`
    crate::dom_interfaces::define_to_string_tag(vm, range_proto, "Range");
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
                is_live: true,
            },
        );
        Ok(JsValue::from_object(rh))
    });
    vm.heap
        .object_set_prototype(range_ctor, Some(abs_ctor), &mut vm.shapes);
    for (cname, cval) in [
        ("START_TO_START", 0),
        ("START_TO_END", 1),
        ("END_TO_END", 2),
        ("END_TO_START", 3),
    ] {
        let k = vm.heap.intern_key(cname);
        vm.heap.object_define_data(
            range_ctor,
            k,
            JsValue::from_i32(cval),
            PropertyAttrs::ENUMERABLE,
            &mut vm.shapes,
        );
    }
    vm.heap.object_define_data(
        range_ctor,
        vm.key_prototype,
        JsValue::from_object(range_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        range_proto,
        vm.key_constructor,
        JsValue::from_object(range_ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.mark_constructor(range_ctor);
    define_global(vm, "Range", JsValue::from_object(range_ctor));
}
