//! Stylo-style bucketed [`RuleIndex`] and scoped [`InvalidationMap`].
//!
//! Every compiled rule is indexed into exactly one fast rightmost bucket
//! (ID > class > tag > universal, or pseudo-element) with inline `AncestorHashes`
//! for cache-local `BloomFilter` rejection before full selector matching.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Arc;

use rix_dom::{hash_ident, hash_ident_ascii_lower, ElementData, ElementState};
use selectors::bloom::BloomFilter;
use selectors::matching::selector_may_match;
use selectors::parser::{AncestorHashes, Combinator, Component, Selector};
use smallvec::SmallVec;

use crate::selector::{RixPseudoClass, RixPseudoElement, RixSelectorImpl};
use crate::stylesheet::{CompiledRule, Stylesheet};

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

/// Bucketed rule index for O(1) candidate lookup per element.
#[derive(Debug, Default)]
pub struct RuleIndex {
    pub rules: Vec<CompiledRule>,
    by_id: FastU64Map<Vec<RuleBucketEntry>>,
    by_class: FastU64Map<Vec<RuleBucketEntry>>,
    by_tag: FastU64Map<Vec<RuleBucketEntry>>,
    universal: Vec<RuleBucketEntry>,
    pseudo_by_kind: HashMap<RixPseudoElement, Vec<RuleBucketEntry>>,
    /// Global flag indicating whether any rule contains a `:has(...)` selector.
    pub has_relative_selectors: bool,
    /// True when no rule uses sibling combinators, `:nth-*` / `:empty`, or `:has()`,
    /// enabling O(1) Stylo-style sibling style sharing for attribute-free siblings.
    pub allows_sibling_sharing: bool,
}

impl RuleIndex {
    /// Builds a [`RuleIndex`] and matching [`InvalidationMap`] from ordered stylesheets.
    #[must_use]
    pub fn build(stylesheets: &[Arc<Stylesheet>]) -> (Self, InvalidationMap) {
        let total_rules: usize = stylesheets.iter().map(|s| s.rules.len()).sum();
        let mut index = Self {
            rules: Vec::with_capacity(total_rules),
            by_id: FastU64Map::default(),
            by_class: FastU64Map::default(),
            by_tag: FastU64Map::default(),
            universal: Vec::new(),
            pseudo_by_kind: HashMap::new(),
            has_relative_selectors: false,
            allows_sibling_sharing: true,
        };
        let mut inv = InvalidationMap::default();

        let mut global_order: u32 = 0;
        for sheet in stylesheets {
            for rule in &sheet.rules {
                let mut compiled = rule.clone();
                compiled.source_order = global_order;
                global_order = global_order.saturating_add(1);

                let rule_idx = index.rules.len() as u32;
                let entry = RuleBucketEntry {
                    rule_idx,
                    ancestor_hashes: compiled.ancestor_hashes.clone(),
                };
                index.insert_bucket(&compiled.selector, entry);
                if selector_prevents_sibling_sharing(&compiled.selector) {
                    index.allows_sibling_sharing = false;
                }
                if inv.record_selector(&compiled.selector) {
                    index.has_relative_selectors = true;
                    index.allows_sibling_sharing = false;
                }
                index.rules.push(compiled);
            }
        }
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
        if !self.universal.is_empty() {
            push_bucket(&self.universal);
        }
    }

    /// Collects candidate rule indices for a pseudo-element on an originating element.
    pub fn collect_pseudo_candidates(
        &self,
        pseudo: RixPseudoElement,
        bloom: Option<&BloomFilter>,
        out: &mut SmallVec<[u32; 16]>,
    ) {
        out.clear();
        let Some(entries) = self.pseudo_by_kind.get(&pseudo) else {
            return;
        };
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
    }

    /// Returns true if any rule targets `pseudo`.
    #[must_use]
    pub fn has_pseudo_rules(&self, pseudo: RixPseudoElement) -> bool {
        self.pseudo_by_kind
            .get(&pseudo)
            .is_some_and(|v| !v.is_empty())
    }

    fn insert_bucket(&mut self, selector: &Selector<RixSelectorImpl>, entry: RuleBucketEntry) {
        let mut iter = selector.iter();
        let mut best_id: Option<u64> = None;
        let mut best_class: Option<u64> = None;
        let mut best_tag: Option<u64> = None;

        for comp in iter.by_ref() {
            match comp {
                Component::PseudoElement(pe) => {
                    self.pseudo_by_kind.entry(*pe).or_default().push(entry);
                    return;
                }
                Component::ID(id) if best_id.is_none() => best_id = Some(id.hash64),
                Component::Class(cls) if best_class.is_none() => best_class = Some(cls.hash64),
                Component::LocalName(ln) if best_tag.is_none() => {
                    best_tag = Some(ln.lower_name.hash64);
                }
                Component::Is(list) | Component::Where(list) if list.slice().len() == 1 => {
                    extract_simple_key(
                        &list.slice()[0],
                        &mut best_id,
                        &mut best_class,
                        &mut best_tag,
                    );
                }
                _ => {}
            }
        }

        if let Some(id_hash) = best_id {
            self.by_id.entry(id_hash).or_default().push(entry);
        } else if let Some(cls_hash) = best_class {
            self.by_class.entry(cls_hash).or_default().push(entry);
        } else if let Some(tag_hash) = best_tag {
            self.by_tag.entry(tag_hash).or_default().push(entry);
        } else {
            self.universal.push(entry);
        }
    }
}

fn extract_simple_key(
    sel: &Selector<RixSelectorImpl>,
    best_id: &mut Option<u64>,
    best_class: &mut Option<u64>,
    best_tag: &mut Option<u64>,
) {
    for comp in sel.iter() {
        match comp {
            Component::ID(id) if best_id.is_none() => *best_id = Some(id.hash64),
            Component::Class(cls) if best_class.is_none() => *best_class = Some(cls.hash64),
            Component::LocalName(ln) if best_tag.is_none() => {
                *best_tag = Some(ln.lower_name.hash64);
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
        const SELF                     = 1 << 0;
        const DESCENDANTS              = 1 << 1;
        const LATER_SIBLINGS           = 1 << 2;
        const LATER_SIBLING_DESCENDANTS = 1 << 3;
        const ANCESTORS                = 1 << 4;
    }
}

/// Dependency index mapping mutated classes, IDs, attributes, and pseudo-states
/// to the minimal [`InvalidationScope`] affected by the active stylesheets.
#[derive(Debug, Default, Clone)]
pub struct InvalidationMap {
    class_deps: HashMap<u64, InvalidationScope>,
    id_deps: HashMap<u64, InvalidationScope>,
    attr_deps: HashMap<u64, InvalidationScope>,
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
        self.attr_deps
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

    /// Records all selector dependencies; returns `true` if `:has(...)` was encountered.
    fn record_selector(&mut self, selector: &Selector<RixSelectorImpl>) -> bool {
        self.record_selector_with_scope(selector, InvalidationScope::SELF)
    }

    fn record_selector_with_scope(
        &mut self,
        selector: &Selector<RixSelectorImpl>,
        initial_scope: InvalidationScope,
    ) -> bool {
        let mut iter = selector.iter();
        let mut scope = initial_scope;
        let mut has_relative = false;

        loop {
            for comp in iter.by_ref() {
                if self.record_component(comp, scope) {
                    has_relative = true;
                }
            }
            let Some(comb) = iter.next_sequence() else {
                break;
            };
            scope = advance_scope(scope, comb);
        }
        has_relative
    }

    fn record_component(
        &mut self,
        comp: &Component<RixSelectorImpl>,
        scope: InvalidationScope,
    ) -> bool {
        match comp {
            Component::ID(id) => {
                *self.id_deps.entry(id.hash64).or_default() |= scope;
                false
            }
            Component::Class(cls) => {
                *self.class_deps.entry(cls.hash64).or_default() |= scope;
                false
            }
            Component::AttributeInNoNamespace { local_name, .. } => {
                *self.attr_deps.entry(local_name.hash64).or_default() |= scope;
                false
            }
            Component::AttributeInNoNamespaceExists {
                local_name_lower, ..
            } => {
                *self.attr_deps.entry(local_name_lower.hash64).or_default() |= scope;
                false
            }
            Component::AttributeOther(attr) => {
                *self
                    .attr_deps
                    .entry(attr.local_name_lower.hash64)
                    .or_default() |= scope;
                false
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
                ) {
                    let attr = match pc {
                        RixPseudoClass::Disabled | RixPseudoClass::Enabled => "disabled",
                        RixPseudoClass::Checked => "checked",
                        RixPseudoClass::ReadOnly | RixPseudoClass::ReadWrite => "readonly",
                        RixPseudoClass::Required | RixPseudoClass::Optional => "required",
                        RixPseudoClass::Link | RixPseudoClass::AnyLink => "href",
                        _ => "placeholder",
                    };
                    *self
                        .attr_deps
                        .entry(hash_ident_ascii_lower(attr))
                        .or_default() |= scope;
                }
                false
            }
            Component::Is(list) | Component::Where(list) | Component::Negation(list) => {
                let mut rel = false;
                for sel in list.slice() {
                    if self.record_selector_with_scope(sel, scope) {
                        rel = true;
                    }
                }
                rel
            }
            Component::Has(rel_list) => {
                let has_scope = InvalidationScope::ANCESTORS | InvalidationScope::SELF;
                for rel in rel_list.iter() {
                    let _ = self.record_selector_with_scope(&rel.selector, has_scope);
                }
                true
            }
            Component::Host(Some(sel)) | Component::Slotted(sel) => {
                self.record_selector_with_scope(sel, scope)
            }
            _ => false,
        }
    }
}

fn advance_scope(current: InvalidationScope, combinator: Combinator) -> InvalidationScope {
    let mut next = InvalidationScope::empty();
    if current.contains(InvalidationScope::ANCESTORS) {
        next |= InvalidationScope::ANCESTORS;
    }
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
        _ => None,
    }
}
