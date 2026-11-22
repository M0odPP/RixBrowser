//! Per-document [`StyleContext`], `StyleMemo` sharing cache, parallel `rayon`
//! top-down cascade, and incremental dirty-bit restyling.
//!
//! Zero global mutable locks: every document/tab owns its own [`StyleContext`].

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Arc;

use rayon::prelude::*;
use rix_dom::{DirtyFlags, Document, ElementData, ElementState, NodeData, NodeId};
use selectors::bloom::BloomFilter;
use selectors::context::SelectorCaches;
use selectors::Element as _;
use smallvec::SmallVec;

use crate::computed::{resolve_element_style, resolve_element_style_with_hints, ComputedStyle};
use crate::properties::{parse_inline_style, MissingCssFeature, SpecifiedDeclaration};
use crate::rule_index::{InvalidationMap, InvalidationScope, RuleIndex};
use crate::selector::{matches_single_selector, DomElement, RixPseudoElement};
use crate::stylesheet::{CompiledRule, MediaContext, Stylesheet};
use crate::ua::default_ua_stylesheet;

const PARALLEL_CHILD_THRESHOLD: usize = 32;
const MIN_PARALLEL_CHUNK_SIZE: usize = 8;
const SIBLING_CACHE_SLOTS: usize = 32;

/// Fast FNV-1a hasher for internal per-document `NodeId` and memo maps.
#[derive(Default)]
struct FastFnvHasher(u64);

impl Hasher for FastFnvHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = if self.0 == 0 {
            0xcbf2_9ce4_8422_2325
        } else {
            self.0
        };
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        self.0 = h;
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.0 =
            self.0.wrapping_mul(0x0100_0000_01b3) ^ (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.0 = self.0.wrapping_mul(0x0100_0000_01b3) ^ i;
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.0 = self.0.wrapping_mul(0x0100_0000_01b3) ^ u64::from(i);
    }
}

type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FastFnvHasher>>;

/// Summary metrics produced by [`StyleContext::restyle_dirty`] or [`StyleContext::cascade_full`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestyleReport {
    pub restyled_elements: usize,
    pub shared_memo_hits: usize,
    pub layout_dirty_count: usize,
    pub paint_only_count: usize,
}

/// Key for sharing computed styles across sibling/subtree elements with identical
/// parent style pointer and matched rule sequence.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StyleMemoKey {
    parent_ptr: usize,
    rule_indices: SmallVec<[u32; 8]>,
}

/// Direct-mapped 32-slot L1-resident sibling style sharing slot.
struct SiblingSlot {
    parent_node: NodeId,
    tag_hash: u64,
    class_sig: u64,
    pseudo_state: ElementState,
    class_hashes: SmallVec<[u64; 4]>,
    style: Arc<ComputedStyle>,
}

/// Thread-local / per-worker style sharing memo cache.
pub struct StyleMemo {
    entries: FastMap<StyleMemoKey, Arc<ComputedStyle>>,
    sibling_slots: [Option<SiblingSlot>; SIBLING_CACHE_SLOTS],
}

impl Default for StyleMemo {
    fn default() -> Self {
        Self::with_capacity(64)
    }
}

#[inline]
fn sibling_signature(tag_hash: u64, pseudo: ElementState, classes: &[u64]) -> u64 {
    let mut sig = tag_hash ^ (u64::from(pseudo.bits()) << 48);
    for &c in classes {
        sig = sig.rotate_left(7) ^ c;
    }
    sig
}

impl StyleMemo {
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            entries: FastMap::with_capacity_and_hasher(cap, BuildHasherDefault::default()),
            sibling_slots: std::array::from_fn(|_| None),
        }
    }

    #[inline]
    fn lookup_sibling(
        &self,
        parent_node: NodeId,
        elem: &ElementData,
    ) -> Option<Arc<ComputedStyle>> {
        let sig = sibling_signature(elem.tag_hash, elem.pseudo_state, &elem.class_hashes);
        let idx = (sig as usize) & (SIBLING_CACHE_SLOTS - 1);
        let slot = self.sibling_slots[idx].as_ref()?;
        if slot.parent_node == parent_node
            && slot.class_sig == sig
            && slot.tag_hash == elem.tag_hash
            && slot.pseudo_state == elem.pseudo_state
            && slot.class_hashes == elem.class_hashes
        {
            Some(Arc::clone(&slot.style))
        } else {
            None
        }
    }

    #[inline]
    fn insert_sibling(
        &mut self,
        parent_node: NodeId,
        elem: &ElementData,
        style: Arc<ComputedStyle>,
    ) {
        let sig = sibling_signature(elem.tag_hash, elem.pseudo_state, &elem.class_hashes);
        let idx = (sig as usize) & (SIBLING_CACHE_SLOTS - 1);
        self.sibling_slots[idx] = Some(SiblingSlot {
            parent_node,
            tag_hash: elem.tag_hash,
            class_sig: sig,
            pseudo_state: elem.pseudo_state,
            class_hashes: elem.class_hashes.clone(),
            style,
        });
    }

    fn lookup(&self, parent_ptr: usize, matched: &[u32]) -> Option<Arc<ComputedStyle>> {
        let key = StyleMemoKey {
            parent_ptr,
            rule_indices: SmallVec::from_slice(matched),
        };
        self.entries.get(&key).map(Arc::clone)
    }

    fn insert(&mut self, parent_ptr: usize, matched: &[u32], style: Arc<ComputedStyle>) {
        if self.entries.len() < 4096 {
            let key = StyleMemoKey {
                parent_ptr,
                rule_indices: SmallVec::from_slice(matched),
            };
            self.entries.insert(key, style);
        }
    }
}

#[inline]
fn elem_eligible_for_sibling_sharing(elem: &ElementData) -> bool {
    if elem.cached_id.is_some() {
        return false;
    }
    match elem.attributes.as_slice() {
        [] => true,
        [(q, _)] => q.local.eq_str_ignore_ascii_case("class"),
        _ => false,
    }
}

/// Per-document CSS style engine holding compiled stylesheets, bucketed [`RuleIndex`],
/// [`InvalidationMap`], resolved [`ComputedStyle`] map, and deduplicated missing-feature telemetry.
pub struct StyleContext {
    media: MediaContext,
    ua_sheet: Arc<Stylesheet>,
    author_sheets: Vec<Arc<Stylesheet>>,
    rule_index: RuleIndex,
    invalidation_map: InvalidationMap,
    styles: FastMap<NodeId, Arc<ComputedStyle>>,
    pseudo_styles: FastMap<(NodeId, RixPseudoElement), Arc<ComputedStyle>>,
    missing_seen: HashSet<MissingCssFeature>,
    missing_features: Vec<MissingCssFeature>,
    index_dirty: bool,
}

impl StyleContext {
    /// Creates a new per-document [`StyleContext`] preloaded with the HTML5 UA stylesheet.
    #[must_use]
    pub fn new(media: MediaContext) -> Self {
        let ua_sheet = default_ua_stylesheet();
        let mut ctx = Self {
            media,
            ua_sheet,
            author_sheets: Vec::new(),
            rule_index: RuleIndex::default(),
            invalidation_map: InvalidationMap::default(),
            styles: FastMap::default(),
            pseudo_styles: FastMap::default(),
            missing_seen: HashSet::new(),
            missing_features: Vec::new(),
            index_dirty: true,
        };
        ctx.rebuild_index();
        ctx
    }

    /// Creates a [`StyleContext`] without the default UA stylesheet (for isolated tests/benchmarks).
    #[must_use]
    pub fn without_ua(media: MediaContext) -> Self {
        Self {
            media,
            ua_sheet: Arc::new(Stylesheet::parse(
                "",
                crate::stylesheet::CascadeOrigin::UserAgent,
                &media,
            )),
            author_sheets: Vec::new(),
            rule_index: RuleIndex::default(),
            invalidation_map: InvalidationMap::default(),
            styles: FastMap::default(),
            pseudo_styles: FastMap::default(),
            missing_seen: HashSet::new(),
            missing_features: Vec::new(),
            index_dirty: true,
        }
    }

    /// Appends a compiled author or user stylesheet and marks the rule index dirty.
    pub fn add_stylesheet(&mut self, sheet: Arc<Stylesheet>) {
        for feat in &sheet.missing_features {
            if self.missing_seen.insert(feat.clone()) {
                self.missing_features.push(feat.clone());
            }
        }
        self.author_sheets.push(sheet);
        self.index_dirty = true;
    }

    /// Clears all author stylesheets and resolved styles (e.g. on navigation).
    pub fn clear_author_stylesheets(&mut self) {
        self.author_sheets.clear();
        self.styles.clear();
        self.pseudo_styles.clear();
        self.missing_seen.clear();
        self.missing_features.clear();
        self.index_dirty = true;
    }

    /// Updates viewport/media context (re-indexing is caller's responsibility if sheets re-parsed).
    pub fn set_media(&mut self, media: MediaContext) {
        self.media = media;
    }

    /// Looks up the latest `@keyframes <name>` rule across author & UA stylesheets.
    #[must_use]
    pub fn keyframes(&self, name: &str) -> Option<&crate::stylesheet::KeyframesRule> {
        for sheet in self.author_sheets.iter().rev() {
            if let Some(kf) = sheet
                .keyframes
                .iter()
                .rev()
                .find(|k| k.name.as_str() == name)
            {
                return Some(kf);
            }
        }
        self.ua_sheet
            .keyframes
            .iter()
            .rev()
            .find(|k| k.name.as_str() == name)
    }

    /// Returns all compiled `@keyframes` rules across attached stylesheets.
    #[must_use]
    pub fn all_keyframes(&self) -> Vec<&crate::stylesheet::KeyframesRule> {
        let mut out = Vec::new();
        for kf in &self.ua_sheet.keyframes {
            out.push(kf);
        }
        for sheet in &self.author_sheets {
            for kf in &sheet.keyframes {
                out.push(kf);
            }
        }
        out
    }

    /// Ensures the bucketed [`RuleIndex`] and [`InvalidationMap`] are up to date.
    pub fn ensure_index(&mut self) {
        if self.index_dirty {
            self.rebuild_index();
        }
    }

    fn rebuild_index(&mut self) {
        let mut all = Vec::with_capacity(1 + self.author_sheets.len());
        if !self.ua_sheet.rules.is_empty() {
            all.push(Arc::clone(&self.ua_sheet));
        }
        all.extend(self.author_sheets.iter().cloned());
        let (index, inv) = RuleIndex::build(&all);
        self.rule_index = index;
        self.invalidation_map = inv;
        self.index_dirty = false;
    }

    /// Returns the resolved [`ComputedStyle`] for `node`, if styled.
    #[must_use]
    pub fn style_for(&self, node: NodeId) -> Option<&Arc<ComputedStyle>> {
        self.styles.get(&node)
    }

    /// Returns the resolved pseudo-element style (`::before`, `::after`, etc.) for `node`.
    #[must_use]
    pub fn pseudo_style_for(
        &self,
        node: NodeId,
        pseudo: RixPseudoElement,
    ) -> Option<&Arc<ComputedStyle>> {
        self.pseudo_styles.get(&(node, pseudo))
    }

    /// Returns the deduplicated unsupported CSS features recorded for F12 telemetry.
    #[must_use]
    pub fn missing_features(&self) -> &[MissingCssFeature] {
        &self.missing_features
    }

    /// Returns a reference to the active [`InvalidationMap`].
    #[must_use]
    pub fn invalidation_map(&self) -> &InvalidationMap {
        &self.invalidation_map
    }

    /// Returns a reference to the active [`RuleIndex`].
    #[must_use]
    pub fn rule_index(&self) -> &RuleIndex {
        &self.rule_index
    }

    /// Marks the minimal set of DOM nodes dirty when `class_name` is added/removed on `node`.
    pub fn invalidate_class_change(&mut self, doc: &mut Document, node: NodeId, class_name: &str) {
        self.ensure_index();
        let scope = self.invalidation_map.scope_for_class(class_name);
        apply_invalidation_scope(doc, node, scope);
    }

    /// Marks the minimal set of DOM nodes dirty when `old_id` / `new_id` changes on `node`.
    pub fn invalidate_id_change(
        &mut self,
        doc: &mut Document,
        node: NodeId,
        old_id: Option<&str>,
        new_id: Option<&str>,
    ) {
        self.ensure_index();
        let mut scope = InvalidationScope::empty();
        if let Some(old) = old_id {
            scope |= self.invalidation_map.scope_for_id(old);
        }
        if let Some(new) = new_id {
            scope |= self.invalidation_map.scope_for_id(new);
        }
        apply_invalidation_scope(doc, node, scope);
    }

    /// Marks the minimal set of DOM nodes dirty when attribute `attr_name` mutates on `node`.
    pub fn invalidate_attribute_change(
        &mut self,
        doc: &mut Document,
        node: NodeId,
        attr_name: &str,
    ) {
        self.ensure_index();
        if attr_name.eq_ignore_ascii_case("style") {
            mark_node_and_ancestors_dirty(doc, node);
            return;
        }
        let scope = self.invalidation_map.scope_for_attribute(attr_name);
        apply_invalidation_scope(doc, node, scope);
    }

    /// Marks the minimal set of DOM nodes dirty when pseudo-state bits `changed` toggle on `node`.
    pub fn invalidate_state_change(
        &mut self,
        doc: &mut Document,
        node: NodeId,
        changed: ElementState,
    ) {
        self.ensure_index();
        let scope = self.invalidation_map.scope_for_state(changed);
        apply_invalidation_scope(doc, node, scope);
    }

    /// Convenience helper combining `doc.set_hovered_diff` with scoped `InvalidationMap` marking.
    pub fn set_hovered(&mut self, doc: &mut Document, target: Option<NodeId>) {
        self.ensure_index();
        let scope = self.invalidation_map.scope_for_state(ElementState::HOVER);
        let changed = doc.set_hovered_diff(target);
        for node in changed {
            apply_invalidation_scope(doc, node, scope);
        }
    }

    /// Convenience helper combining `doc.set_focused_diff` with scoped `InvalidationMap` marking.
    pub fn set_focused(&mut self, doc: &mut Document, target: Option<NodeId>, visible: bool) {
        self.ensure_index();
        let scope = self.invalidation_map.scope_for_state(
            ElementState::FOCUS | ElementState::FOCUS_VISIBLE | ElementState::FOCUS_WITHIN,
        );
        let changed = doc.set_focused_diff(target, visible);
        for node in changed {
            apply_invalidation_scope(doc, node, scope);
        }
    }

    /// Performs a full parallel top-down cascade over the entire document.
    pub fn cascade_full(&mut self, doc: &mut Document) -> RestyleReport {
        self.ensure_index();
        self.styles.clear();
        self.pseudo_styles.clear();

        let root_children: Vec<NodeId> = doc
            .children(doc.root())
            .filter(|&c| matches!(doc.get(c), Some(NodeData::Element(_))))
            .collect();

        let mut results = Vec::new();
        let mut pseudo_results = Vec::new();
        let mut missing_local = Vec::new();
        let mut report = RestyleReport::default();

        let mut bloom = BloomFilter::new();
        let mut caches = SelectorCaches::default();
        let mut memo = StyleMemo::with_capacity(256);

        for child in root_children {
            cascade_subtree_par(
                doc,
                child,
                None,
                16.0,
                true,
                &self.rule_index,
                &self.media,
                &mut bloom,
                &mut caches,
                &mut memo,
                &mut results,
                &mut pseudo_results,
                &mut missing_local,
                &mut report,
            );
        }

        self.styles.reserve(results.len());
        report.layout_dirty_count += results.len();
        for (id, style) in results {
            self.styles.insert(id, style);
            if let Some(NodeData::Element(el)) = doc.get_mut(id) {
                el.dirty_flags
                    .remove(DirtyFlags::SELF_STYLE_DIRTY | DirtyFlags::DESCENDANT_STYLE_DIRTY);
                el.dirty_flags
                    .insert(DirtyFlags::SELF_LAYOUT_DIRTY | DirtyFlags::PAINT_DIRTY);
            }
        }
        doc.clear_dirty_flags(
            doc.root(),
            DirtyFlags::SELF_STYLE_DIRTY | DirtyFlags::DESCENDANT_STYLE_DIRTY,
        );

        for ((id, pe), style) in pseudo_results {
            self.pseudo_styles.insert((id, pe), style);
        }
        for feat in missing_local {
            if self.missing_seen.insert(feat.clone()) {
                self.missing_features.push(feat);
            }
        }
        report
    }

    /// Incrementally restyles ONLY dirty subtrees/elements marked with
    /// `DirtyFlags::SELF_STYLE_DIRTY` or `DirtyFlags::DESCENDANT_STYLE_DIRTY`.
    pub fn restyle_dirty(&mut self, doc: &mut Document) -> RestyleReport {
        self.ensure_index();
        let mut report = RestyleReport::default();
        if self.styles.is_empty() {
            return self.cascade_full(doc);
        }

        let root_flags = doc.dirty_flags(doc.root());
        if !root_flags.intersects(DirtyFlags::SELF_STYLE_DIRTY | DirtyFlags::DESCENDANT_STYLE_DIRTY)
        {
            return report;
        }

        let root_children: Vec<NodeId> = doc
            .children(doc.root())
            .filter(|&c| matches!(doc.get(c), Some(NodeData::Element(_))))
            .collect();

        let mut bloom = BloomFilter::new();
        let mut caches = SelectorCaches::default();
        let mut memo = StyleMemo::default();
        let mut missing_local = Vec::new();
        let mut cleared_nodes = Vec::new();
        let mut layout_dirty_nodes = Vec::new();
        let mut paint_dirty_nodes = Vec::new();

        let root_font_size = root_children
            .first()
            .and_then(|id| self.styles.get(id))
            .map_or(16.0, |s| s.text.font_size_px);

        for child in root_children {
            restyle_dirty_walk(
                doc,
                child,
                None,
                root_font_size,
                false,
                &self.rule_index,
                &self.media,
                &mut bloom,
                &mut caches,
                &mut memo,
                &mut self.styles,
                &mut self.pseudo_styles,
                &mut missing_local,
                &mut cleared_nodes,
                &mut layout_dirty_nodes,
                &mut paint_dirty_nodes,
                &mut report,
            );
        }

        for id in cleared_nodes {
            doc.clear_dirty_flags(
                id,
                DirtyFlags::SELF_STYLE_DIRTY | DirtyFlags::DESCENDANT_STYLE_DIRTY,
            );
        }
        doc.clear_dirty_flags(
            doc.root(),
            DirtyFlags::SELF_STYLE_DIRTY | DirtyFlags::DESCENDANT_STYLE_DIRTY,
        );
        for id in layout_dirty_nodes {
            doc.mark_node_dirty(id, DirtyFlags::SELF_LAYOUT_DIRTY | DirtyFlags::PAINT_DIRTY);
        }
        for id in paint_dirty_nodes {
            doc.mark_node_dirty(id, DirtyFlags::PAINT_DIRTY);
        }
        for feat in missing_local {
            if self.missing_seen.insert(feat.clone()) {
                self.missing_features.push(feat);
            }
        }
        report
    }
}

#[allow(clippy::too_many_arguments)]
fn cascade_subtree_par(
    doc: &Document,
    node: NodeId,
    parent_style: Option<&Arc<ComputedStyle>>,
    root_font_size: f32,
    allow_parallel: bool,
    rule_index: &RuleIndex,
    media: &MediaContext,
    bloom: &mut BloomFilter,
    caches: &mut SelectorCaches,
    memo: &mut StyleMemo,
    out_styles: &mut Vec<(NodeId, Arc<ComputedStyle>)>,
    out_pseudos: &mut Vec<((NodeId, RixPseudoElement), Arc<ComputedStyle>)>,
    out_missing: &mut Vec<MissingCssFeature>,
    report: &mut RestyleReport,
) {
    let Some(NodeData::Element(elem)) = doc.get(node) else {
        return;
    };

    let (style, hit) = compute_single_element_style(
        doc,
        node,
        elem,
        parent_style,
        root_font_size,
        rule_index,
        media,
        Some(bloom),
        caches,
        memo,
        out_missing,
    );
    report.restyled_elements += 1;
    if hit {
        report.shared_memo_hits += 1;
    }

    let next_root_font = if parent_style.is_none() {
        style.text.font_size_px
    } else {
        root_font_size
    };

    collect_pseudo_element_styles(
        doc,
        node,
        &style,
        next_root_font,
        rule_index,
        media,
        Some(bloom),
        caches,
        out_pseudos,
    );

    let elem_children: SmallVec<[NodeId; 16]> = doc
        .children(node)
        .filter(|&c| matches!(doc.get(c), Some(NodeData::Element(_))))
        .collect();

    if elem_children.is_empty() {
        out_styles.push((node, style));
        return;
    }

    let dom_el = DomElement::new(doc, node);
    push_bloom_hashes(&dom_el, bloom);

    if allow_parallel && elem_children.len() >= PARALLEL_CHILD_THRESHOLD {
        let parent_arc = Arc::clone(&style);
        let bloom_snapshot = bloom.clone();
        let threads = rayon::current_num_threads().max(1);
        let chunk_size = elem_children
            .len()
            .div_ceil(threads)
            .max(MIN_PARALLEL_CHUNK_SIZE);
        let chunk_outputs: Vec<_> = elem_children
            .par_chunks(chunk_size)
            .map(|chunk| {
                let mut local_bloom = bloom_snapshot.clone();
                let mut local_caches = SelectorCaches::default();
                let mut local_memo = StyleMemo::with_capacity(64);
                let mut local_styles = Vec::with_capacity(chunk.len() * 16);
                let mut local_pseudos = Vec::new();
                let mut local_missing = Vec::new();
                let mut local_report = RestyleReport::default();

                for &child in chunk {
                    cascade_subtree_par(
                        doc,
                        child,
                        Some(&parent_arc),
                        next_root_font,
                        false,
                        rule_index,
                        media,
                        &mut local_bloom,
                        &mut local_caches,
                        &mut local_memo,
                        &mut local_styles,
                        &mut local_pseudos,
                        &mut local_missing,
                        &mut local_report,
                    );
                }
                (local_styles, local_pseudos, local_missing, local_report)
            })
            .collect();

        for (ls, lp, lm, lr) in chunk_outputs {
            out_styles.extend(ls);
            out_pseudos.extend(lp);
            out_missing.extend(lm);
            report.restyled_elements += lr.restyled_elements;
            report.shared_memo_hits += lr.shared_memo_hits;
        }
    } else {
        for &child in &elem_children {
            cascade_subtree_par(
                doc,
                child,
                Some(&style),
                next_root_font,
                allow_parallel,
                rule_index,
                media,
                bloom,
                caches,
                memo,
                out_styles,
                out_pseudos,
                out_missing,
                report,
            );
        }
    }

    pop_bloom_hashes(&dom_el, bloom);
    out_styles.push((node, style));
}

#[allow(clippy::too_many_arguments)]
fn restyle_dirty_walk(
    doc: &Document,
    node: NodeId,
    parent_style: Option<&Arc<ComputedStyle>>,
    root_font_size: f32,
    force_inherit_restyle: bool,
    rule_index: &RuleIndex,
    media: &MediaContext,
    bloom: &mut BloomFilter,
    caches: &mut SelectorCaches,
    memo: &mut StyleMemo,
    styles: &mut FastMap<NodeId, Arc<ComputedStyle>>,
    pseudo_styles: &mut FastMap<(NodeId, RixPseudoElement), Arc<ComputedStyle>>,
    missing_out: &mut Vec<MissingCssFeature>,
    cleared_nodes: &mut Vec<NodeId>,
    layout_dirty: &mut Vec<NodeId>,
    paint_dirty: &mut Vec<NodeId>,
    report: &mut RestyleReport,
) {
    let Some(NodeData::Element(elem)) = doc.get(node) else {
        return;
    };
    let flags = elem.dirty_flags;
    let needs_self = force_inherit_restyle || flags.contains(DirtyFlags::SELF_STYLE_DIRTY);
    let needs_desc = flags.contains(DirtyFlags::DESCENDANT_STYLE_DIRTY);

    if !needs_self && !needs_desc {
        return;
    }

    let mut propagate_inherited = false;
    let current_style = if needs_self {
        let (new_style, hit) = compute_single_element_style(
            doc,
            node,
            elem,
            parent_style,
            root_font_size,
            rule_index,
            media,
            Some(bloom),
            caches,
            memo,
            missing_out,
        );
        report.restyled_elements += 1;
        if hit {
            report.shared_memo_hits += 1;
        }

        if let Some(old_style) = styles.get(&node) {
            if !old_style.layout_eq(&new_style) {
                layout_dirty.push(node);
                report.layout_dirty_count += 1;
            } else if **old_style != *new_style {
                paint_dirty.push(node);
                report.paint_only_count += 1;
            }
            let text_changed =
                !Arc::ptr_eq(&old_style.text, &new_style.text) && old_style.text != new_style.text;
            let vars_changed =
                !Arc::ptr_eq(&old_style.custom_properties, &new_style.custom_properties)
                    && old_style.custom_properties != new_style.custom_properties;
            let inherited_scalar_changed = old_style.visibility != new_style.visibility
                || old_style.pointer_events != new_style.pointer_events
                || old_style.cursor != new_style.cursor;
            propagate_inherited = text_changed || vars_changed || inherited_scalar_changed;
        } else {
            layout_dirty.push(node);
            report.layout_dirty_count += 1;
            propagate_inherited = true;
        }

        let mut pseudos = Vec::new();
        collect_pseudo_element_styles(
            doc,
            node,
            &new_style,
            root_font_size,
            rule_index,
            media,
            Some(bloom),
            caches,
            &mut pseudos,
        );
        for pe in [RixPseudoElement::Before, RixPseudoElement::After] {
            pseudo_styles.remove(&(node, pe));
        }
        for ((nid, pe), pstyle) in pseudos {
            pseudo_styles.insert((nid, pe), pstyle);
        }

        styles.insert(node, Arc::clone(&new_style));
        new_style
    } else if let Some(existing) = styles.get(&node) {
        Arc::clone(existing)
    } else {
        return;
    };

    if flags.intersects(DirtyFlags::SELF_STYLE_DIRTY | DirtyFlags::DESCENDANT_STYLE_DIRTY) {
        cleared_nodes.push(node);
    }

    if !needs_desc && !propagate_inherited {
        return;
    }

    let dom_el = DomElement::new(doc, node);
    push_bloom_hashes(&dom_el, bloom);
    let next_root_font = if parent_style.is_none() {
        current_style.text.font_size_px
    } else {
        root_font_size
    };

    for child in doc.children(node) {
        if matches!(doc.get(child), Some(NodeData::Element(_))) {
            restyle_dirty_walk(
                doc,
                child,
                Some(&current_style),
                next_root_font,
                propagate_inherited,
                rule_index,
                media,
                bloom,
                caches,
                memo,
                styles,
                pseudo_styles,
                missing_out,
                cleared_nodes,
                layout_dirty,
                paint_dirty,
                report,
            );
        }
    }
    pop_bloom_hashes(&dom_el, bloom);
}

#[allow(clippy::too_many_arguments)]
fn compute_single_element_style(
    doc: &Document,
    node: NodeId,
    elem: &ElementData,
    parent_style: Option<&Arc<ComputedStyle>>,
    root_font_size: f32,
    rule_index: &RuleIndex,
    media: &MediaContext,
    bloom: Option<&BloomFilter>,
    caches: &mut SelectorCaches,
    memo: &mut StyleMemo,
    missing_out: &mut Vec<MissingCssFeature>,
) -> (Arc<ComputedStyle>, bool) {
    let parent_ptr = parent_style.map_or(0, |p| Arc::as_ptr(p) as usize);
    let sibling_parent = if rule_index.allows_sibling_sharing
        && parent_ptr != 0
        && elem_eligible_for_sibling_sharing(elem)
    {
        doc.parent(node)
    } else {
        None
    };
    if let Some(p_node) = sibling_parent {
        if let Some(cached) = memo.lookup_sibling(p_node, elem) {
            return (cached, true);
        }
    }

    let mut candidate_indices = SmallVec::<[u32; 32]>::new();
    rule_index.collect_candidates(elem, bloom, &mut candidate_indices);

    let dom_el = DomElement::new(doc, node);
    let mut matched_indices = SmallVec::<[u32; 16]>::new();
    for &idx in &candidate_indices {
        let rule = &rule_index.rules[idx as usize];
        if matches_single_selector(&rule.selector, &dom_el, bloom, caches, None) {
            matched_indices.push(idx);
        }
    }

    let inline_attr = elem
        .attributes
        .iter()
        .find(|(q, _)| q.local.eq_str_ignore_ascii_case("style"))
        .map(|(_, v)| v.as_str());
    let has_pres_hints = has_presentational_hint_attrs(elem);

    if inline_attr.is_none() && !has_pres_hints {
        if let Some(cached) = memo.lookup(parent_ptr, &matched_indices) {
            if let Some(p_node) = sibling_parent {
                memo.insert_sibling(p_node, elem, Arc::clone(&cached));
            }
            return (cached, true);
        }
    }

    let pres_decls = if has_pres_hints {
        collect_presentational_hint_decls(elem, missing_out)
    } else {
        Vec::new()
    };

    let inline_decls: Vec<SpecifiedDeclaration> = match inline_attr {
        Some(attr) if !attr.trim().is_empty() => parse_inline_style(attr, missing_out),
        _ => Vec::new(),
    };

    let matched_refs: SmallVec<[&CompiledRule; 16]> = matched_indices
        .iter()
        .map(|&idx| &rule_index.rules[idx as usize])
        .collect();

    let resolved = Arc::new(resolve_element_style_with_hints(
        &matched_refs,
        &pres_decls,
        &inline_decls,
        parent_style.map(Arc::as_ref),
        root_font_size,
        media,
    ));

    if inline_attr.is_none() && !has_pres_hints {
        memo.insert(parent_ptr, &matched_indices, Arc::clone(&resolved));
        if let Some(p_node) = sibling_parent {
            memo.insert_sibling(p_node, elem, Arc::clone(&resolved));
        }
    }
    (resolved, false)
}

fn has_presentational_hint_attrs(elem: &ElementData) -> bool {
    elem.attributes.iter().any(|(q, _)| {
        let k: &str = q.local.as_ref();
        k.eq_ignore_ascii_case("bgcolor")
            || k.eq_ignore_ascii_case("width")
            || k.eq_ignore_ascii_case("height")
            || k.eq_ignore_ascii_case("align")
            || k.eq_ignore_ascii_case("cellspacing")
            || k.eq_ignore_ascii_case("color")
    })
}

/// Translates HTML5 §15.3 presentational attributes (`bgcolor`, `width`, `height`, `align`,
/// `cellspacing`, `<font color>`) into zero-specificity CSS declarations overridden by author CSS.
fn collect_presentational_hint_decls(
    elem: &ElementData,
    missing_out: &mut Vec<MissingCssFeature>,
) -> Vec<SpecifiedDeclaration> {
    let tag: &str = elem.name.local.as_ref();
    let mut css = String::new();

    for (q, val) in &elem.attributes {
        let attr: &str = q.local.as_ref();
        let trimmed = val.trim();
        if trimmed.is_empty() {
            continue;
        }
        if attr.eq_ignore_ascii_case("bgcolor")
            && (tag.eq_ignore_ascii_case("body")
                || tag.eq_ignore_ascii_case("table")
                || tag.eq_ignore_ascii_case("tr")
                || tag.eq_ignore_ascii_case("td")
                || tag.eq_ignore_ascii_case("th"))
        {
            let norm = normalize_legacy_color(trimmed);
            css.push_str("background-color:");
            css.push_str(&norm);
            css.push(';');
        } else if attr.eq_ignore_ascii_case("color") && tag.eq_ignore_ascii_case("font") {
            let norm = normalize_legacy_color(trimmed);
            css.push_str("color:");
            css.push_str(&norm);
            css.push(';');
        } else if attr.eq_ignore_ascii_case("width")
            && (tag.eq_ignore_ascii_case("table")
                || tag.eq_ignore_ascii_case("td")
                || tag.eq_ignore_ascii_case("th")
                || tag.eq_ignore_ascii_case("img")
                || tag.eq_ignore_ascii_case("svg")
                || tag.eq_ignore_ascii_case("canvas")
                || tag.eq_ignore_ascii_case("iframe")
                || tag.eq_ignore_ascii_case("hr"))
        {
            if let Some(dim) = normalize_html_dimension(trimmed) {
                css.push_str("width:");
                css.push_str(&dim);
                css.push(';');
            }
        } else if attr.eq_ignore_ascii_case("height")
            && (tag.eq_ignore_ascii_case("table")
                || tag.eq_ignore_ascii_case("td")
                || tag.eq_ignore_ascii_case("th")
                || tag.eq_ignore_ascii_case("img")
                || tag.eq_ignore_ascii_case("svg")
                || tag.eq_ignore_ascii_case("canvas")
                || tag.eq_ignore_ascii_case("iframe"))
        {
            if let Some(dim) = normalize_html_dimension(trimmed) {
                css.push_str("height:");
                css.push_str(&dim);
                css.push(';');
            }
        } else if attr.eq_ignore_ascii_case("align") {
            let lower = trimmed.to_ascii_lowercase();
            if tag.eq_ignore_ascii_case("table") && lower == "center" {
                css.push_str("margin-left:auto;margin-right:auto;");
            } else if matches!(lower.as_str(), "left" | "center" | "right" | "justify") {
                css.push_str("text-align:");
                css.push_str(&lower);
                css.push(';');
            }
        } else if attr.eq_ignore_ascii_case("cellspacing") && tag.eq_ignore_ascii_case("table") {
            if let Some(dim) = normalize_html_dimension(trimmed) {
                css.push_str("border-spacing:");
                css.push_str(&dim);
                css.push(';');
            }
        }
    }

    if css.is_empty() {
        Vec::new()
    } else {
        parse_inline_style(&css, missing_out)
    }
}

fn normalize_legacy_color(raw: &str) -> String {
    if raw.len() == 6 && raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        format!("#{raw}")
    } else {
        raw.to_string()
    }
}

fn normalize_html_dimension(raw: &str) -> Option<String> {
    if let Some(stripped) = raw.strip_suffix('%') {
        let num = stripped.trim();
        if num
            .parse::<f32>()
            .ok()
            .is_some_and(|v| v.is_finite() && v >= 0.0)
        {
            return Some(format!("{num}%"));
        }
        return None;
    }
    let stripped = raw
        .strip_suffix("px")
        .or_else(|| raw.strip_suffix("PX"))
        .unwrap_or(raw)
        .trim();
    if stripped
        .parse::<f32>()
        .ok()
        .is_some_and(|v| v.is_finite() && v >= 0.0)
    {
        return Some(format!("{stripped}px"));
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn collect_pseudo_element_styles(
    doc: &Document,
    node: NodeId,
    originating_style: &Arc<ComputedStyle>,
    root_font_size: f32,
    rule_index: &RuleIndex,
    media: &MediaContext,
    bloom: Option<&BloomFilter>,
    caches: &mut SelectorCaches,
    out_pseudos: &mut Vec<((NodeId, RixPseudoElement), Arc<ComputedStyle>)>,
) {
    for pseudo in [RixPseudoElement::Before, RixPseudoElement::After] {
        if !rule_index.has_pseudo_rules(pseudo) {
            continue;
        }
        let mut cands = SmallVec::<[u32; 16]>::new();
        rule_index.collect_pseudo_candidates(pseudo, bloom, &mut cands);
        if cands.is_empty() {
            continue;
        }
        let pseudo_el = DomElement::with_pseudo(doc, node, pseudo);
        let mut matched: SmallVec<[&CompiledRule; 8]> = SmallVec::new();
        for &idx in &cands {
            let rule = &rule_index.rules[idx as usize];
            if matches_single_selector(&rule.selector, &pseudo_el, bloom, caches, None) {
                matched.push(rule);
            }
        }
        if !matched.is_empty() {
            let style = Arc::new(resolve_element_style(
                &matched,
                &[],
                Some(originating_style.as_ref()),
                root_font_size,
                media,
            ));
            out_pseudos.push(((node, pseudo), style));
        }
    }
}

fn push_bloom_hashes(el: &DomElement<'_>, bloom: &mut BloomFilter) {
    let _ = el.add_element_unique_hashes(bloom);
}

fn pop_bloom_hashes(el: &DomElement<'_>, bloom: &mut BloomFilter) {
    let Some(NodeData::Element(elem)) = el.doc.get(el.node_id) else {
        return;
    };
    bloom.remove_hash(rix_dom::fold_hash_u32(elem.tag_hash));
    if let Some(ref id) = elem.cached_id {
        bloom.remove_hash(rix_dom::fold_hash_u32(rix_dom::hash_ident(id.as_str())));
    }
    for &cls_hash in &elem.class_hashes {
        bloom.remove_hash(rix_dom::fold_hash_u32(cls_hash));
    }
}

fn apply_invalidation_scope(doc: &mut Document, node: NodeId, scope: InvalidationScope) {
    if scope.is_empty() {
        return;
    }
    if scope.contains(InvalidationScope::SELF) {
        mark_node_and_ancestors_dirty(doc, node);
    }
    if scope.contains(InvalidationScope::DESCENDANTS) {
        mark_subtree_style_dirty(doc, node);
    }
    if scope.intersects(
        InvalidationScope::LATER_SIBLINGS | InvalidationScope::LATER_SIBLING_DESCENDANTS,
    ) {
        let deep = scope.contains(InvalidationScope::LATER_SIBLING_DESCENDANTS);
        if let Some(parent) = doc.parent(node) {
            let siblings: Vec<NodeId> = doc.children(parent).collect();
            let mut seen = false;
            for sib in siblings {
                if seen {
                    if matches!(doc.get(sib), Some(NodeData::Element(_))) {
                        if deep {
                            mark_subtree_style_dirty(doc, sib);
                        } else {
                            mark_node_and_ancestors_dirty(doc, sib);
                        }
                    }
                } else if sib == node {
                    seen = true;
                }
            }
        }
    }
    if scope.contains(InvalidationScope::ANCESTORS) {
        let ancestors: Vec<NodeId> = doc.ancestors(node).collect();
        for anc in ancestors {
            if matches!(doc.get(anc), Some(NodeData::Element(_))) {
                mark_node_and_ancestors_dirty(doc, anc);
            }
        }
    }
}

fn mark_node_and_ancestors_dirty(doc: &mut Document, node: NodeId) {
    doc.mark_node_dirty(node, DirtyFlags::SELF_STYLE_DIRTY);
    let mut cur = doc.parent(node);
    while let Some(anc) = cur {
        if doc
            .dirty_flags(anc)
            .contains(DirtyFlags::DESCENDANT_STYLE_DIRTY)
        {
            break;
        }
        doc.mark_node_dirty(anc, DirtyFlags::DESCENDANT_STYLE_DIRTY);
        cur = doc.parent(anc);
    }
}

fn mark_subtree_style_dirty(doc: &mut Document, root: NodeId) {
    mark_node_and_ancestors_dirty(doc, root);
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let children: Vec<NodeId> = doc
            .children(id)
            .filter(|&c| matches!(doc.get(c), Some(NodeData::Element(_))))
            .collect();
        if !children.is_empty() {
            doc.mark_node_dirty(id, DirtyFlags::DESCENDANT_STYLE_DIRTY);
            for c in children {
                doc.mark_node_dirty(c, DirtyFlags::SELF_STYLE_DIRTY);
                stack.push(c);
            }
        }
    }
}
