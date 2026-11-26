//! Stylo-style bucketed [`RuleIndex`] and scoped [`InvalidationMap`].
//!
//! Every compiled rule is indexed into exactly one fast rightmost bucket
//! (ID > class > tag > universal, or pseudo-element) with inline `AncestorHashes`
//! for cache-local `BloomFilter` rejection before full selector matching.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Arc;

use rix_core::InternedStr;
use rix_dom::{hash_ident, hash_ident_ascii_lower, ElementData, ElementState};
use selectors::bloom::BloomFilter;
use selectors::matching::selector_may_match;
use selectors::parser::{AncestorHashes, Combinator, Component, Selector};
use smallvec::SmallVec;

use crate::selector::{RixPseudoClass, RixPseudoElement, RixSelectorImpl};
use crate::stylesheet::{
    CompiledRule, FontFaceRule, KeyframesRule, LayerId, LayerSeg, RegisteredProperty, Stylesheet,
};

/// Zero-overhead identity hasher for pre-hashed `u64` FNV-1a keys.
#[derive(Default)]
pub(crate) struct U64IdentityHasher(u64);

impl Hasher for U64IdentityHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        self.0 = h;
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.0 = i;
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.0 = (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

pub(crate) type FastU64Map<V> = HashMap<u64, V, BuildHasherDefault<U64IdentityHasher>>;

/// Cache-local rule bucket entry pairing a flat rule index with its ancestor hashes.
#[derive(Debug, Clone)]
pub struct RuleBucketEntry {
    pub rule_idx: u32,
    pub ancestor_hashes: AncestorHashes,
}

/// Rightmost selector hash buckets (`ID > class > tag > attribute > universal`) shared by
/// both originating element rules and per-pseudo-element rule tables.
#[derive(Debug, Default)]
struct SelectorBuckets {
    by_id: FastU64Map<Vec<RuleBucketEntry>>,
    by_class: FastU64Map<Vec<RuleBucketEntry>>,
    by_tag: FastU64Map<Vec<RuleBucketEntry>>,
    by_attr: FastU64Map<Vec<RuleBucketEntry>>,
    universal: Vec<RuleBucketEntry>,
}

impl SelectorBuckets {
    #[inline]
    fn is_empty(&self) -> bool {
        self.by_id.is_empty()
            && self.by_class.is_empty()
            && self.by_tag.is_empty()
            && self.by_attr.is_empty()
            && self.universal.is_empty()
    }

    fn insert(
        &mut self,
        best_id: Option<u64>,
        best_class: Option<u64>,
        best_tag: Option<u64>,
        best_attr: Option<u64>,
        entry: RuleBucketEntry,
    ) {
        if let Some(id_hash) = best_id {
            self.by_id.entry(id_hash).or_default().push(entry);
        } else if let Some(cls_hash) = best_class {
            self.by_class.entry(cls_hash).or_default().push(entry);
        } else if let Some(tag_hash) = best_tag {
            self.by_tag.entry(tag_hash).or_default().push(entry);
        } else if let Some(attr_hash) = best_attr {
            self.by_attr.entry(attr_hash).or_default().push(entry);
        } else {
            self.universal.push(entry);
        }
    }

    fn collect<const N: usize>(
        &self,
        elem: &ElementData,
        bloom: Option<&BloomFilter>,
        out: &mut SmallVec<[u32; N]>,
    ) {
        let mut push_bucket = |entries: &[RuleBucketEntry]| {
            if let Some(bf) = bloom {
                for entry in entries {
                    if selector_may_match(&entry.ancestor_hashes, bf) {
                        out.push(entry.rule_idx);
                    }
                }
            } else {
                for entry in entries {
                    out.push(entry.rule_idx);
                }
            }
        };

        if let Some(ref id) = elem.cached_id {
            let id_hash = hash_ident(id.as_str());
            if let Some(bucket) = self.by_id.get(&id_hash) {
                push_bucket(bucket);
            }
        }
        for &cls_hash in &elem.class_hashes {
            if let Some(bucket) = self.by_class.get(&cls_hash) {
                push_bucket(bucket);
            }
        }
        if let Some(bucket) = self.by_tag.get(&elem.tag_hash) {
            push_bucket(bucket);
        }
        if !self.by_attr.is_empty() && !elem.attributes.is_empty() {
            for (q, _) in &elem.attributes {
                let attr_hash = hash_ident_ascii_lower(q.local.as_str());
                if let Some(bucket) = self.by_attr.get(&attr_hash) {
                    push_bucket(bucket);
                }
            }
        }
        if !self.universal.is_empty() {
            push_bucket(&self.universal);
        }
    }
}

/// Bucketed rule index for O(1) candidate lookup per element.
#[derive(Debug, Default)]
pub struct RuleIndex {
    pub rules: Vec<CompiledRule>,
    elements: SelectorBuckets,
    host_rules: Vec<RuleBucketEntry>,
    pseudo_by_kind: HashMap<RixPseudoElement, SelectorBuckets>,
    /// Global flag indicating whether any rule contains a `:has(...)` selector.
    pub has_relative_selectors: bool,
    /// True when any rule contains a `:has(...)` selector in a non-subject compound
    /// (e.g. `.parent:has(.a) .child` or `:has(.a) ~ .b`), requiring ancestor subtree
    /// invalidation on DOM tree mutations.
    pub has_non_subject_has: bool,
    /// True when no rule uses sibling combinators, `:nth-*` / `:empty`, `:has()`, or `@scope`,
    /// enabling O(1) Stylo-style sibling style sharing for attribute-free siblings.
    pub allows_sibling_sharing: bool,
    /// True when any rule or keyframe declaration uses `sibling-index()`, `sibling-count()`,
    /// `random()`, or `random-item()`.
    pub has_sibling_or_random_rules: bool,
    /// True when any custom property (`--*`) declaration uses `sibling-index()`, `sibling-count()`,
    /// `random()`, or `random-item()`.
    pub has_sibling_or_random_custom_props: bool,
    /// True when any rule declaration invokes CSS Values L5 `attr(...)`.
    pub has_attr_fn_rules: bool,
    /// True when any custom property (`--*`) declaration invokes `attr(...)`.
    pub has_attr_custom_props: bool,
    /// Per-document deterministic seed for CSS Values L5 `random()`.
    pub random_doc_seed: u64,
    /// True when any rule is enclosed in an `@scope` block.
    pub has_scope_rules: bool,
    /// True when any rule is enclosed in an `@container` block.
    pub has_container_queries: bool,
    /// Merged `@property` / `CSS.registerProperty` custom property registrations (layer-ordered).
    pub registered_properties: HashMap<InternedStr, RegisteredProperty>,
    /// Merged `@keyframes` rules by name (layer-ordered per CSS Cascade L5).
    pub keyframes: HashMap<InternedStr, KeyframesRule>,
    /// Layer-ordered `@font-face` rules (ascending priority; highest-priority last).
    pub font_faces: Vec<FontFaceRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum GlobalLayerKey {
    Named(InternedStr),
    Anon(u32, u32),
}

#[derive(Default)]
struct LayerTreeNode {
    children: Vec<(GlobalLayerKey, usize)>,
    rank: u32,
}

fn assign_layer_postorder(nodes: &mut [LayerTreeNode], idx: usize, next_rank: &mut u32) {
    let child_ids: SmallVec<[usize; 8]> = nodes[idx].children.iter().map(|&(_, id)| id).collect();
    for cid in child_ids {
        assign_layer_postorder(nodes, cid, next_rank);
        nodes[cid].rank = *next_rank;
        *next_rank = next_rank.saturating_add(1);
    }
}

impl RuleIndex {
    /// Builds a [`RuleIndex`] and matching [`InvalidationMap`] from ordered stylesheets,
    /// resolving a global post-order hierarchical `@layer` tree across all stylesheets.
    #[must_use]
    pub fn build(stylesheets: &[Arc<Stylesheet>]) -> (Self, InvalidationMap) {
        let total_rules: usize = stylesheets.iter().map(|s| s.rules.len()).sum();
        let mut index = Self {
            rules: Vec::with_capacity(total_rules),
            elements: SelectorBuckets::default(),
            host_rules: Vec::new(),
            pseudo_by_kind: HashMap::new(),
            has_relative_selectors: false,
            has_non_subject_has: false,
            allows_sibling_sharing: true,
            has_sibling_or_random_rules: false,
            has_sibling_or_random_custom_props: false,
            has_attr_fn_rules: false,
            has_attr_custom_props: false,
            random_doc_seed: crate::values::EvalContext::default().random_doc_seed,
            has_scope_rules: false,
            has_container_queries: false,
            registered_properties: HashMap::new(),
            keyframes: HashMap::new(),
            font_faces: Vec::new(),
        };
        let mut inv = InvalidationMap::default();

        // Build global hierarchical layer tree (3 origin roots: UA=0, User=1, Author=2).
        let mut layer_nodes: Vec<LayerTreeNode> =
            (0..3).map(|_| LayerTreeNode::default()).collect();
        let mut sheet_layer_nodes: Vec<Vec<usize>> = Vec::with_capacity(stylesheets.len());
        for (sheet_idx, sheet) in stylesheets.iter().enumerate() {
            let root = (sheet.origin as usize).min(2);
            let mut local_map = Vec::with_capacity(sheet.layer_paths.len());
            for path in &sheet.layer_paths {
                let mut cur = root;
                for seg in path {
                    let key = match seg {
                        LayerSeg::Named(n) => GlobalLayerKey::Named(n.clone()),
                        LayerSeg::Anon(a) => GlobalLayerKey::Anon(sheet_idx as u32, *a),
                    };
                    if let Some(&(_, child_idx)) =
                        layer_nodes[cur].children.iter().find(|(k, _)| k == &key)
                    {
                        cur = child_idx;
                    } else {
                        let new_idx = layer_nodes.len();
                        layer_nodes.push(LayerTreeNode::default());
                        layer_nodes[cur].children.push((key, new_idx));
                        cur = new_idx;
                    }
                }
                local_map.push(cur);
            }
            sheet_layer_nodes.push(local_map);
        }
        for root in 0..3 {
            let mut next_rank = 0u32;
            assign_layer_postorder(&mut layer_nodes, root, &mut next_rank);
        }

        let remap_layer = |sheet_idx: usize, local: LayerId| -> LayerId {
            if local.is_unlayered() {
                return LayerId::UNLAYERED;
            }
            sheet_layer_nodes
                .get(sheet_idx)
                .and_then(|m| m.get(local.0 as usize))
                .map_or(local, |&node_idx| LayerId(layer_nodes[node_idx].rank))
        };

        let mut reg_layers: HashMap<InternedStr, u32> = HashMap::new();
        let mut kf_layers: HashMap<InternedStr, (u8, u32)> = HashMap::new();
        let mut ff_ranked: Vec<((u8, u32, u32), FontFaceRule)> = Vec::new();
        let mut ff_order = 0u32;

        let mut global_order: u32 = 0;
        for (sheet_idx, sheet) in stylesheets.iter().enumerate() {
            let origin_rank = sheet.origin as u8;
            for rp in &sheet.registered_properties {
                let remapped = remap_layer(sheet_idx, rp.layer_id);
                let rank = remapped.0;
                let wins = reg_layers
                    .get(&rp.name)
                    .is_none_or(|&prev_rank| rank >= prev_rank);
                if wins {
                    reg_layers.insert(rp.name.clone(), rank);
                    let mut rp_copy = rp.clone();
                    rp_copy.layer_id = remapped;
                    index.registered_properties.insert(rp.name.clone(), rp_copy);
                }
            }
            for kf in &sheet.keyframes {
                let remapped = remap_layer(sheet_idx, kf.layer_id);
                let key_rank = (origin_rank, remapped.0);
                let wins = kf_layers.get(&kf.name).is_none_or(|&prev| key_rank >= prev);
                if wins {
                    kf_layers.insert(kf.name.clone(), key_rank);
                    let mut kf_copy = kf.clone();
                    kf_copy.layer_id = remapped;
                    if kf_copy.steps.iter().any(|st| {
                        st.declarations
                            .iter()
                            .any(|d| d.value.contains_sibling_fn())
                    }) {
                        index.allows_sibling_sharing = false;
                        index.has_sibling_or_random_rules = true;
                    }
                    index.keyframes.insert(kf.name.clone(), kf_copy);
                }
            }
            for ff in &sheet.font_faces {
                let remapped = remap_layer(sheet_idx, ff.layer_id);
                let mut ff_copy = ff.clone();
                ff_copy.layer_id = remapped;
                ff_ranked.push(((origin_rank, remapped.0, ff_order), ff_copy));
                ff_order = ff_order.saturating_add(1);
            }
            for rule in &sheet.rules {
                let mut compiled = rule.clone();
                compiled.layer_id = remap_layer(sheet_idx, compiled.layer_id);
                compiled.source_order = global_order;
                global_order = global_order.saturating_add(1);

                let rule_idx = index.rules.len() as u32;
                let entry = RuleBucketEntry {
                    rule_idx,
                    ancestor_hashes: compiled.ancestor_hashes.clone(),
                };
                index.insert_bucket(&compiled.selector, entry);
                if compiled.container_queries.is_some() {
                    index.has_container_queries = true;
                    index.allows_sibling_sharing = false;
                }
                if let Some(ref scopes) = compiled.scopes {
                    index.has_scope_rules = true;
                    index.allows_sibling_sharing = false;
                    let scope_inv = InvalidationScope::SELF | InvalidationScope::DESCENDANTS;
                    for sc in scopes.iter() {
                        if let Some(ref start_list) = sc.start {
                            for sel in start_list.slice() {
                                let _ = inv.record_selector_with_scope(sel, scope_inv);
                            }
                        }
                        if let Some(ref end_list) = sc.end {
                            for sel in end_list.slice() {
                                let _ = inv.record_selector_with_scope(sel, scope_inv);
                            }
                        }
                    }
                }
                let mut rule_has_sib = false;
                for d in compiled.declarations.iter() {
                    if d.value.contains_sibling_fn() {
                        rule_has_sib = true;
                        index.has_sibling_or_random_rules = true;
                        if d.property == crate::properties::PropertyIdKind::Custom {
                            index.has_sibling_or_random_custom_props = true;
                        }
                    }
                    if d.value.contains_attr_fn() {
                        index.has_attr_fn_rules = true;
                        if d.property == crate::properties::PropertyIdKind::Custom {
                            index.has_attr_custom_props = true;
                        }
                    }
                }
                if selector_prevents_sibling_sharing(&compiled.selector) || rule_has_sib {
                    index.allows_sibling_sharing = false;
                }
                let mut attr_refs = Vec::new();
                for d in compiled.declarations.iter() {
                    if let crate::properties::PropertyValue::Unresolved(toks)
                    | crate::properties::PropertyValue::Custom(toks) = &d.value
                    {
                        attr_refs.clear();
                        crate::substitution::collect_attr_names_from_tokens(
                            toks.as_slice(),
                            &mut attr_refs,
                        );
                        let attr_scope = if d.property == crate::properties::PropertyIdKind::Custom
                        {
                            InvalidationScope::SELF | InvalidationScope::DESCENDANTS
                        } else {
                            InvalidationScope::SELF
                        };
                        for a in &attr_refs {
                            if a == "*" {
                                inv.any_attr_dep |= attr_scope;
                            } else {
                                *inv.attr_deps.entry(hash_ident_ascii_lower(a)).or_default() |=
                                    attr_scope;
                            }
                        }
                    }
                }
                let (has_rel, has_non_subject) = inv.record_selector(&compiled.selector);
                if has_rel {
                    index.has_relative_selectors = true;
                    index.allows_sibling_sharing = false;
                }
                if has_non_subject {
                    index.has_non_subject_has = true;
                }
                index.rules.push(compiled);
            }
        }
        ff_ranked.sort_by_key(|(k, _)| *k);
        index.font_faces = ff_ranked.into_iter().map(|(_, ff)| ff).collect();
        (index, inv)
    }

    /// Collects candidate rule indices for `elem`, applying `BloomFilter` ancestor pruning.
    pub fn collect_candidates(
        &self,
        elem: &ElementData,
        bloom: Option<&BloomFilter>,
        out: &mut SmallVec<[u32; 32]>,
    ) {
        out.clear();
        self.elements.collect(elem, bloom, out);
    }

    /// Collects candidate rule indices for a pseudo-element on an originating element,
    /// using the originating element's ID/class/tag/attribute buckets + BloomFilter pruning.
    pub fn collect_pseudo_candidates(
        &self,
        pseudo: RixPseudoElement,
        elem: &ElementData,
        bloom: Option<&BloomFilter>,
        out: &mut SmallVec<[u32; 16]>,
    ) {
        out.clear();
        let Some(buckets) = self.pseudo_by_kind.get(&pseudo) else {
            return;
        };
        buckets.collect(elem, bloom, out);
    }

    /// Returns true if any rule targets `:host` or `:host(...)`.
    #[must_use]
    pub fn has_host_rules(&self) -> bool {
        !self.host_rules.is_empty()
    }

    /// Collects candidate `:host` / `:host(...)` rule indices for a shadow host element.
    pub fn collect_host_candidates(&self, out: &mut SmallVec<[u32; 16]>) {
        out.clear();
        for entry in &self.host_rules {
            out.push(entry.rule_idx);
        }
    }

    /// Returns true if any rule targets `pseudo`.
    #[must_use]
    pub fn has_pseudo_rules(&self, pseudo: RixPseudoElement) -> bool {
        self.pseudo_by_kind
            .get(&pseudo)
            .is_some_and(|b| !b.is_empty())
    }

    /// Marks ONLY DOM elements potentially affected by a newly appended `sheet` as
    /// `SELF_STYLE_DIRTY` (propagating `DESCENDANT_STYLE_DIRTY` to the document root).
    /// Returns `true` if incremental marking succeeded, or `false` if a full restyle is required
    /// (e.g. `sheet` contains universal `*` / `:root` rules, `@property`, `@keyframes`, or `@layer`).
    pub fn mark_affected_nodes_dirty_for_sheet(
        doc: &mut rix_dom::Document,
        sheet: &Stylesheet,
    ) -> bool {
        if !sheet.registered_properties.is_empty()
            || !sheet.keyframes.is_empty()
            || !sheet.layers.is_empty()
            || !sheet.font_faces.is_empty()
        {
            return false;
        }
        let mut target_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut target_classes: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut target_tags: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut target_attrs: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let html_hash = hash_ident_ascii_lower("html");
        let body_hash = hash_ident_ascii_lower("body");

        for rule in &sheet.rules {
            if rule.scopes.is_some() || rule.container_queries.is_some() {
                return false;
            }
            let mut iter = rule.selector.iter();
            let mut best_id: Option<u64> = None;
            let mut best_class: Option<u64> = None;
            let mut best_tag: Option<u64> = None;
            let mut best_attr: Option<u64> = None;
            let mut pseudo_kind: Option<RixPseudoElement> = None;
            for comp in iter.by_ref() {
                match comp {
                    Component::PseudoElement(pe) => pseudo_kind = Some(*pe),
                    Component::ID(id) if best_id.is_none() => best_id = Some(id.hash64),
                    Component::Class(cls) if best_class.is_none() => best_class = Some(cls.hash64),
                    Component::LocalName(ln) if best_tag.is_none() => {
                        best_tag = Some(ln.lower_name.hash64);
                    }
                    Component::AttributeInNoNamespace { local_name, .. } if best_attr.is_none() => {
                        best_attr = Some(local_name.hash64);
                    }
                    Component::AttributeInNoNamespaceExists {
                        local_name_lower, ..
                    } if best_attr.is_none() => {
                        best_attr = Some(local_name_lower.hash64);
                    }
                    Component::AttributeOther(attr) if best_attr.is_none() => {
                        best_attr = Some(attr.local_name_lower.hash64);
                    }
                    Component::Is(list) | Component::Where(list) if list.slice().len() == 1 => {
                        extract_simple_key(
                            &list.slice()[0],
                            &mut best_id,
                            &mut best_class,
                            &mut best_tag,
                            &mut best_attr,
                        );
                    }
                    _ => {}
                }
            }
            if pseudo_kind.is_some()
                && matches!(iter.next_sequence(), Some(Combinator::PseudoElement))
            {
                for comp in iter.by_ref() {
                    match comp {
                        Component::ID(id) if best_id.is_none() => best_id = Some(id.hash64),
                        Component::Class(cls) if best_class.is_none() => {
                            best_class = Some(cls.hash64);
                        }
                        Component::LocalName(ln) if best_tag.is_none() => {
                            best_tag = Some(ln.lower_name.hash64);
                        }
                        _ => {}
                    }
                }
            }
            if let Some(h) = best_id {
                target_ids.insert(h);
            } else if let Some(h) = best_class {
                target_classes.insert(h);
            } else if let Some(h) = best_tag {
                if h == html_hash || h == body_hash {
                    // Inherited/custom properties on html/body affect the entire document.
                    return false;
                }
                target_tags.insert(h);
            } else if let Some(h) = best_attr {
                target_attrs.insert(h);
            } else {
                // Universal selector (`*`, `:root`, `:is(...)`) -> fall back to full restyle.
                return false;
            }
        }

        let affected: Vec<rix_dom::NodeId> = doc
            .descendants(doc.root())
            .filter(|&nid| {
                let Some(rix_dom::NodeData::Element(el)) = doc.get(nid) else {
                    return false;
                };
                if !target_ids.is_empty() {
                    if let Some(ref id) = el.cached_id {
                        if target_ids.contains(&hash_ident(id.as_str())) {
                            return true;
                        }
                    }
                }
                if !target_classes.is_empty()
                    && el.class_hashes.iter().any(|h| target_classes.contains(h))
                {
                    return true;
                }
                if !target_tags.is_empty() && target_tags.contains(&el.tag_hash) {
                    return true;
                }
                if !target_attrs.is_empty()
                    && el.attributes.iter().any(|(q, _)| {
                        target_attrs.contains(&hash_ident_ascii_lower(q.local.as_str()))
                    })
                {
                    return true;
                }
                false
            })
            .collect();

        for nid in affected {
            doc.mark_node_dirty(nid, rix_dom::DirtyFlags::SELF_STYLE_DIRTY);
            let mut cur = doc.parent(nid);
            while let Some(anc) = cur {
                if doc
                    .dirty_flags(anc)
                    .contains(rix_dom::DirtyFlags::DESCENDANT_STYLE_DIRTY)
                {
                    break;
                }
                doc.mark_node_dirty(anc, rix_dom::DirtyFlags::DESCENDANT_STYLE_DIRTY);
                cur = doc.parent(anc);
            }
        }
        true
    }

    fn insert_bucket(&mut self, selector: &Selector<RixSelectorImpl>, entry: RuleBucketEntry) {
        let mut iter = selector.iter();
        let mut best_id: Option<u64> = None;
        let mut best_class: Option<u64> = None;
        let mut best_tag: Option<u64> = None;
        let mut best_attr: Option<u64> = None;
        let mut is_host_subject = false;
        let mut pseudo_kind: Option<RixPseudoElement> = None;

        for comp in iter.by_ref() {
            match comp {
                Component::PseudoElement(pe) => {
                    pseudo_kind = Some(*pe);
                }
                Component::Host(_) => {
                    is_host_subject = true;
                }
                Component::ID(id) if best_id.is_none() => best_id = Some(id.hash64),
                Component::Class(cls) if best_class.is_none() => best_class = Some(cls.hash64),
                Component::LocalName(ln) if best_tag.is_none() => {
                    best_tag = Some(ln.lower_name.hash64);
                }
                Component::AttributeInNoNamespace { local_name, .. } if best_attr.is_none() => {
                    best_attr = Some(local_name.hash64);
                }
                Component::AttributeInNoNamespaceExists {
                    local_name_lower, ..
                } if best_attr.is_none() => {
                    best_attr = Some(local_name_lower.hash64);
                }
                Component::AttributeOther(attr) if best_attr.is_none() => {
                    best_attr = Some(attr.local_name_lower.hash64);
                }
                Component::Is(list) | Component::Where(list) if list.slice().len() == 1 => {
                    extract_simple_key(
                        &list.slice()[0],
                        &mut best_id,
                        &mut best_class,
                        &mut best_tag,
                        &mut best_attr,
                    );
                }
                _ => {}
            }
        }

        // In the `selectors` crate, `.foo::before` separates `[PseudoElement(Before)]` from the
        // originating element's compound (`[Class("foo")]`) with `Combinator::PseudoElement`.
        if let Some(pe) = pseudo_kind {
            if matches!(iter.next_sequence(), Some(Combinator::PseudoElement)) {
                for comp in iter.by_ref() {
                    match comp {
                        Component::ID(id) if best_id.is_none() => best_id = Some(id.hash64),
                        Component::Class(cls) if best_class.is_none() => {
                            best_class = Some(cls.hash64);
                        }
                        Component::LocalName(ln) if best_tag.is_none() => {
                            best_tag = Some(ln.lower_name.hash64);
                        }
                        Component::AttributeInNoNamespace { local_name, .. }
                            if best_attr.is_none() =>
                        {
                            best_attr = Some(local_name.hash64);
                        }
                        Component::AttributeInNoNamespaceExists {
                            local_name_lower, ..
                        } if best_attr.is_none() => {
                            best_attr = Some(local_name_lower.hash64);
                        }
                        Component::AttributeOther(attr) if best_attr.is_none() => {
                            best_attr = Some(attr.local_name_lower.hash64);
                        }
                        Component::Is(list) | Component::Where(list) if list.slice().len() == 1 => {
                            extract_simple_key(
                                &list.slice()[0],
                                &mut best_id,
                                &mut best_class,
                                &mut best_tag,
                                &mut best_attr,
                            );
                        }
                        _ => {}
                    }
                }
            }
            self.pseudo_by_kind
                .entry(pe)
                .or_default()
                .insert(best_id, best_class, best_tag, best_attr, entry);
            return;
        }

        if is_host_subject {
            self.host_rules.push(entry);
        } else {
            self.elements
                .insert(best_id, best_class, best_tag, best_attr, entry);
        }
    }
}

fn extract_simple_key(
    sel: &Selector<RixSelectorImpl>,
    best_id: &mut Option<u64>,
    best_class: &mut Option<u64>,
    best_tag: &mut Option<u64>,
    best_attr: &mut Option<u64>,
) {
    for comp in sel.iter() {
        match comp {
            Component::ID(id) if best_id.is_none() => *best_id = Some(id.hash64),
            Component::Class(cls) if best_class.is_none() => *best_class = Some(cls.hash64),
            Component::LocalName(ln) if best_tag.is_none() => {
                *best_tag = Some(ln.lower_name.hash64);
            }
            Component::AttributeInNoNamespace { local_name, .. } if best_attr.is_none() => {
                *best_attr = Some(local_name.hash64);
            }
            Component::AttributeInNoNamespaceExists {
                local_name_lower, ..
            } if best_attr.is_none() => {
                *best_attr = Some(local_name_lower.hash64);
            }
            Component::AttributeOther(attr) if best_attr.is_none() => {
                *best_attr = Some(attr.local_name_lower.hash64);
            }
            _ => {}
        }
    }
}

fn selector_prevents_sibling_sharing(sel: &Selector<RixSelectorImpl>) -> bool {
    let mut iter = sel.iter();
    loop {
        for comp in iter.by_ref() {
            match comp {
                Component::Nth(_)
                | Component::NthOf(_)
                | Component::Empty
                | Component::Has(_)
                | Component::Host(_)
                | Component::Slotted(_) => return true,
                Component::Is(list) | Component::Where(list) | Component::Negation(list)
                    if list.slice().iter().any(selector_prevents_sibling_sharing) =>
                {
                    return true;
                }
                _ => {}
            }
        }
        match iter.next_sequence() {
            Some(Combinator::NextSibling | Combinator::LaterSibling) => return true,
            Some(_) => {}
            None => return false,
        }
    }
}

bitflags::bitflags! {
    /// Scoped set of DOM elements that must be invalidated when a dependency mutates.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct InvalidationScope: u8 {
        const SELF                      = 1 << 0;
        const DESCENDANTS               = 1 << 1;
        const LATER_SIBLINGS            = 1 << 2;
        const LATER_SIBLING_DESCENDANTS = 1 << 3;
        const ANCESTORS                 = 1 << 4;
        const EARLIER_SIBLINGS          = 1 << 5;
        const ANCESTOR_EARLIER_SIBLINGS = 1 << 6;
        const ANCESTOR_SUBTREES         = 1 << 7;
    }
}

/// Dependency index mapping mutated classes, IDs, attributes, and pseudo-states
/// to the minimal [`InvalidationScope`] affected by the active stylesheets.
#[derive(Debug, Default, Clone)]
pub struct InvalidationMap {
    class_deps: HashMap<u64, InvalidationScope>,
    id_deps: HashMap<u64, InvalidationScope>,
    attr_deps: HashMap<u64, InvalidationScope>,
    any_attr_dep: InvalidationScope,
    state_deps: HashMap<ElementState, InvalidationScope>,
}

impl InvalidationMap {
    /// Returns the invalidation scope for mutating `class_name`.
    #[must_use]
    pub fn scope_for_class(&self, class_name: &str) -> InvalidationScope {
        self.class_deps
            .get(&hash_ident(class_name))
            .copied()
            .unwrap_or(InvalidationScope::empty())
    }

    /// Returns the invalidation scope for mutating `class_hash`.
    #[must_use]
    pub fn scope_for_class_hash(&self, class_hash: u64) -> InvalidationScope {
        self.class_deps
            .get(&class_hash)
            .copied()
            .unwrap_or(InvalidationScope::empty())
    }

    /// Returns the invalidation scope for mutating `id`.
    #[must_use]
    pub fn scope_for_id(&self, id: &str) -> InvalidationScope {
        self.id_deps
            .get(&hash_ident(id))
            .copied()
            .unwrap_or(InvalidationScope::empty())
    }

    /// Returns the invalidation scope for mutating attribute `attr_name`.
    #[must_use]
    pub fn scope_for_attribute(&self, attr_name: &str) -> InvalidationScope {
        self.any_attr_dep
            | self
                .attr_deps
                .get(&hash_ident_ascii_lower(attr_name))
                .copied()
                .unwrap_or(InvalidationScope::empty())
    }

    /// Returns the combined invalidation scope for toggling `changed_states`.
    #[must_use]
    pub fn scope_for_state(&self, changed_states: ElementState) -> InvalidationScope {
        let mut out = InvalidationScope::empty();
        for (&state, &scope) in &self.state_deps {
            if changed_states.intersects(state) {
                out |= scope;
            }
        }
        out
    }

    /// Records all selector dependencies; returns `(has_relative, has_non_subject_has)`.
    fn record_selector(&mut self, selector: &Selector<RixSelectorImpl>) -> (bool, bool) {
        self.record_selector_with_scope(selector, InvalidationScope::SELF)
    }

    fn record_selector_with_scope(
        &mut self,
        selector: &Selector<RixSelectorImpl>,
        initial_scope: InvalidationScope,
    ) -> (bool, bool) {
        let mut iter = selector.iter();
        let mut scope = initial_scope;
        let mut has_relative = false;
        let mut has_non_subject = false;

        loop {
            for comp in iter.by_ref() {
                let (rel, non_sub) = self.record_component(comp, scope);
                has_relative |= rel;
                has_non_subject |= non_sub;
            }
            let Some(comb) = iter.next_sequence() else {
                break;
            };
            scope = advance_scope(scope, comb);
        }
        (has_relative, has_non_subject)
    }

    fn record_component(
        &mut self,
        comp: &Component<RixSelectorImpl>,
        scope: InvalidationScope,
    ) -> (bool, bool) {
        match comp {
            Component::ID(id) => {
                *self.id_deps.entry(id.hash64).or_default() |= scope;
                (false, false)
            }
            Component::Class(cls) => {
                *self.class_deps.entry(cls.hash64).or_default() |= scope;
                (false, false)
            }
            Component::AttributeInNoNamespace { local_name, .. } => {
                *self.attr_deps.entry(local_name.hash64).or_default() |= scope;
                (false, false)
            }
            Component::AttributeInNoNamespaceExists {
                local_name_lower, ..
            } => {
                *self.attr_deps.entry(local_name_lower.hash64).or_default() |= scope;
                (false, false)
            }
            Component::AttributeOther(attr) => {
                *self
                    .attr_deps
                    .entry(attr.local_name_lower.hash64)
                    .or_default() |= scope;
                (false, false)
            }
            Component::NonTSPseudoClass(pc) => {
                if let Some(state) = pseudo_to_element_state(pc) {
                    *self.state_deps.entry(state).or_default() |= scope;
                }
                if matches!(
                    pc,
                    RixPseudoClass::Disabled
                        | RixPseudoClass::Enabled
                        | RixPseudoClass::Checked
                        | RixPseudoClass::ReadOnly
                        | RixPseudoClass::ReadWrite
                        | RixPseudoClass::Required
                        | RixPseudoClass::Optional
                        | RixPseudoClass::Link
                        | RixPseudoClass::AnyLink
                        | RixPseudoClass::PlaceholderShown
                        | RixPseudoClass::Open
                        | RixPseudoClass::Dir(_)
                        | RixPseudoClass::Lang(_)
                ) {
                    let attr = match pc {
                        RixPseudoClass::Disabled | RixPseudoClass::Enabled => "disabled",
                        RixPseudoClass::Checked => "checked",
                        RixPseudoClass::ReadOnly | RixPseudoClass::ReadWrite => "readonly",
                        RixPseudoClass::Required | RixPseudoClass::Optional => "required",
                        RixPseudoClass::Link | RixPseudoClass::AnyLink => "href",
                        RixPseudoClass::Open => "open",
                        RixPseudoClass::Dir(_) => "dir",
                        RixPseudoClass::Lang(_) => "lang",
                        _ => "placeholder",
                    };
                    let attr_scope =
                        if matches!(pc, RixPseudoClass::Dir(_) | RixPseudoClass::Lang(_)) {
                            scope | InvalidationScope::DESCENDANTS
                        } else {
                            scope
                        };
                    *self
                        .attr_deps
                        .entry(hash_ident_ascii_lower(attr))
                        .or_default() |= attr_scope;
                    if matches!(pc, RixPseudoClass::Dir(_)) {
                        // `<input type="tel">` and `<input dir="auto" value="...">` affect `:dir()`.
                        *self
                            .attr_deps
                            .entry(hash_ident_ascii_lower("type"))
                            .or_default() |= scope;
                        *self
                            .attr_deps
                            .entry(hash_ident_ascii_lower("value"))
                            .or_default() |= scope;
                    }
                }
                (false, false)
            }
            Component::Is(list) | Component::Where(list) | Component::Negation(list) => {
                let mut rel = false;
                let mut non_sub = false;
                for sel in list.slice() {
                    let (r, ns) = self.record_selector_with_scope(sel, scope);
                    rel |= r;
                    non_sub |= ns;
                }
                (rel, non_sub)
            }
            Component::Has(rel_list) => {
                let mut has_scope = InvalidationScope::ANCESTORS
                    | InvalidationScope::EARLIER_SIBLINGS
                    | InvalidationScope::ANCESTOR_EARLIER_SIBLINGS
                    | InvalidationScope::SELF;
                let is_non_subject = scope.intersects(
                    InvalidationScope::DESCENDANTS
                        | InvalidationScope::LATER_SIBLINGS
                        | InvalidationScope::LATER_SIBLING_DESCENDANTS,
                );
                // When `:has(...)` appears in a non-subject compound (e.g. `.p:has(.a) .c`
                // or `:has(.a) ~ .b`), invalidating the `:has` subject must also invalidate
                // its descendants / later siblings!
                if is_non_subject {
                    has_scope |= InvalidationScope::ANCESTOR_SUBTREES;
                }
                for rel in rel_list.iter() {
                    let _ = self.record_selector_with_scope(&rel.selector, has_scope);
                }
                (true, is_non_subject)
            }
            Component::Host(Some(sel)) | Component::Slotted(sel) => {
                self.record_selector_with_scope(sel, scope)
            }
            _ => (false, false),
        }
    }
}

fn advance_scope(current: InvalidationScope, combinator: Combinator) -> InvalidationScope {
    let mut next = InvalidationScope::empty();
    let carry = current
        & (InvalidationScope::ANCESTORS
            | InvalidationScope::EARLIER_SIBLINGS
            | InvalidationScope::ANCESTOR_EARLIER_SIBLINGS
            | InvalidationScope::ANCESTOR_SUBTREES);
    next |= carry;
    match combinator {
        Combinator::Child | Combinator::Descendant => {
            if current.intersects(
                InvalidationScope::LATER_SIBLINGS | InvalidationScope::LATER_SIBLING_DESCENDANTS,
            ) {
                next |=
                    InvalidationScope::DESCENDANTS | InvalidationScope::LATER_SIBLING_DESCENDANTS;
            } else {
                next |= InvalidationScope::DESCENDANTS;
            }
        }
        Combinator::NextSibling | Combinator::LaterSibling => {
            if current.intersects(
                InvalidationScope::DESCENDANTS | InvalidationScope::LATER_SIBLING_DESCENDANTS,
            ) {
                next |= InvalidationScope::LATER_SIBLING_DESCENDANTS;
            } else {
                next |= InvalidationScope::LATER_SIBLINGS;
            }
        }
        Combinator::PseudoElement | Combinator::Part | Combinator::SlotAssignment => {
            next |= current;
        }
    }
    next
}

fn pseudo_to_element_state(pc: &RixPseudoClass) -> Option<ElementState> {
    match pc {
        RixPseudoClass::Hover => Some(ElementState::HOVER),
        RixPseudoClass::Active => Some(ElementState::ACTIVE),
        RixPseudoClass::Focus => Some(ElementState::FOCUS),
        RixPseudoClass::FocusVisible => Some(ElementState::FOCUS_VISIBLE),
        RixPseudoClass::FocusWithin => Some(ElementState::FOCUS_WITHIN),
        RixPseudoClass::Disabled | RixPseudoClass::Enabled => Some(ElementState::DISABLED),
        RixPseudoClass::Checked => Some(ElementState::CHECKED),
        RixPseudoClass::Indeterminate => Some(ElementState::INDETERMINATE),
        RixPseudoClass::Visited | RixPseudoClass::Link => Some(ElementState::VISITED),
        RixPseudoClass::Defined => Some(ElementState::DEFINED),
        RixPseudoClass::Target => Some(ElementState::TARGET),
        RixPseudoClass::Modal => Some(ElementState::MODAL),
        RixPseudoClass::PopoverOpen => Some(ElementState::POPOVER_OPEN),
        RixPseudoClass::Open => Some(ElementState::OPEN),
        RixPseudoClass::Fullscreen => Some(ElementState::FULLSCREEN),
        RixPseudoClass::Valid | RixPseudoClass::Invalid => Some(ElementState::INVALID),
        RixPseudoClass::UserValid => Some(ElementState::USER_VALID),
        RixPseudoClass::UserInvalid => Some(ElementState::USER_INVALID),
        RixPseudoClass::Autofill => Some(ElementState::AUTOFILL),
        _ => None,
    }
}
