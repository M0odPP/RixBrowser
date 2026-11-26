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

use crate::animations::{
    apply_animations_and_transitions, ActiveTransition, AnimationStateSnapshot, WebAnimationSpec,
};
use crate::color::CssColorExpr;
use crate::computed::{resolve_element_style_with_context, ComputedStyle, MatchedRule};
use crate::properties::{
    parse_inline_style, MissingCssFeature, PropertyIdKind, PropertyValue, SpecifiedDeclaration,
};
use crate::rule_index::{InvalidationMap, InvalidationScope, RuleIndex};
use crate::selector::{
    matches_selector_list_with_scope, matches_single_selector, matches_single_selector_with_scope,
    DomElement, RixPseudoElement,
};
use crate::stylesheet::{
    CompiledRule, CompiledScope, MediaContext, RegisteredProperty, Stylesheet,
};
use crate::ua::default_ua_stylesheet;

const PARALLEL_CHILD_THRESHOLD: usize = 32;
const MIN_PARALLEL_CHUNK_SIZE: usize = 8;
const SIBLING_CACHE_SLOTS: usize = 32;

/// Fast FNV-1a hasher for internal per-document `NodeId`, custom property, and memo maps.
#[derive(Default)]
pub(crate) struct FastFnvHasher(u64);

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

pub(crate) type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FastFnvHasher>>;

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
    layout_parent_flex_grid: bool,
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

    fn lookup(
        &self,
        parent_ptr: usize,
        layout_parent_flex_grid: bool,
        matched: &[u32],
    ) -> Option<Arc<ComputedStyle>> {
        let key = StyleMemoKey {
            parent_ptr,
            layout_parent_flex_grid,
            rule_indices: SmallVec::from_slice(matched),
        };
        self.entries.get(&key).map(Arc::clone)
    }

    fn insert(
        &mut self,
        parent_ptr: usize,
        layout_parent_flex_grid: bool,
        matched: &[u32],
        style: Arc<ComputedStyle>,
    ) {
        if self.entries.len() < 4096 {
            let key = StyleMemoKey {
                parent_ptr,
                layout_parent_flex_grid,
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
    shadow_adopted_sheets: HashMap<NodeId, Vec<Arc<Stylesheet>>>,
    js_registered_properties: Vec<RegisteredProperty>,
    rule_index: RuleIndex,
    invalidation_map: InvalidationMap,
    styles: FastMap<NodeId, Arc<ComputedStyle>>,
    pseudo_styles: FastMap<(NodeId, RixPseudoElement), Arc<ComputedStyle>>,
    inline_color_exprs: FastMap<(NodeId, PropertyIdKind), Arc<CssColorExpr>>,
    before_change_styles: FastMap<NodeId, Arc<ComputedStyle>>,
    pseudo_before_change_styles: FastMap<(NodeId, RixPseudoElement), Arc<ComputedStyle>>,
    pre_anim_styles: FastMap<NodeId, Arc<ComputedStyle>>,
    active_transitions: FastMap<NodeId, Vec<ActiveTransition>>,
    pseudo_active_transitions: FastMap<(NodeId, RixPseudoElement), Vec<ActiveTransition>>,
    managed_css_animations: FastMap<NodeId, Vec<WebAnimationSpec>>,
    web_animations: FastMap<NodeId, Vec<WebAnimationSpec>>,
    transition_overrides: HashMap<u64, crate::animations::TransitionControlOverride>,
    cancelled_transition_ids: Vec<u64>,
    next_transition_id: u64,
    transition_generation: u64,
    css_animations_managed: bool,
    animation_clock_ms: f32,
    random_doc_seed: u64,
    /// Laid-out `(content_width, content_height)` of CSS Containment L3 query containers.
    container_sizes: HashMap<NodeId, (f32, f32)>,
    missing_seen: HashSet<MissingCssFeature>,
    missing_features: Vec<MissingCssFeature>,
    index_dirty: bool,
}

fn next_random_doc_seed() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let mut x = NEXT
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
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
            shadow_adopted_sheets: HashMap::new(),
            js_registered_properties: Vec::new(),
            rule_index: RuleIndex::default(),
            invalidation_map: InvalidationMap::default(),
            styles: FastMap::default(),
            pseudo_styles: FastMap::default(),
            inline_color_exprs: FastMap::default(),
            before_change_styles: FastMap::default(),
            pseudo_before_change_styles: FastMap::default(),
            pre_anim_styles: FastMap::default(),
            active_transitions: FastMap::default(),
            pseudo_active_transitions: FastMap::default(),
            managed_css_animations: FastMap::default(),
            web_animations: FastMap::default(),
            transition_overrides: HashMap::new(),
            cancelled_transition_ids: Vec::new(),
            next_transition_id: 1,
            transition_generation: 1,
            css_animations_managed: false,
            animation_clock_ms: 0.0,
            random_doc_seed: next_random_doc_seed(),
            container_sizes: HashMap::new(),
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
            shadow_adopted_sheets: HashMap::new(),
            js_registered_properties: Vec::new(),
            rule_index: RuleIndex::default(),
            invalidation_map: InvalidationMap::default(),
            styles: FastMap::default(),
            pseudo_styles: FastMap::default(),
            inline_color_exprs: FastMap::default(),
            before_change_styles: FastMap::default(),
            pseudo_before_change_styles: FastMap::default(),
            pre_anim_styles: FastMap::default(),
            active_transitions: FastMap::default(),
            pseudo_active_transitions: FastMap::default(),
            managed_css_animations: FastMap::default(),
            web_animations: FastMap::default(),
            transition_overrides: HashMap::new(),
            cancelled_transition_ids: Vec::new(),
            next_transition_id: 1,
            transition_generation: 1,
            css_animations_managed: false,
            animation_clock_ms: 0.0,
            random_doc_seed: next_random_doc_seed(),
            container_sizes: HashMap::new(),
            missing_seen: HashSet::new(),
            missing_features: Vec::new(),
            index_dirty: true,
        }
    }

    /// Extracts portable animation/transition state so it survives full stylesheet rebuilds.
    pub fn take_animation_state(&mut self) -> AnimationStateSnapshot {
        AnimationStateSnapshot {
            before_change: self.before_change_styles.drain().collect(),
            pseudo_before_change: self.pseudo_before_change_styles.drain().collect(),
            active_transitions: self.active_transitions.drain().collect(),
            pseudo_active_transitions: self.pseudo_active_transitions.drain().collect(),
            managed_css_animations: self.managed_css_animations.drain().collect(),
            web_animations: self.web_animations.drain().collect(),
            transition_overrides: std::mem::take(&mut self.transition_overrides),
            next_transition_id: self.next_transition_id,
            transition_generation: self.transition_generation,
            css_animations_managed: self.css_animations_managed,
            random_doc_seed: Some(self.random_doc_seed),
        }
    }

    /// Restores portable animation/transition state onto a newly constructed [`StyleContext`].
    pub fn restore_animation_state(&mut self, snap: AnimationStateSnapshot) {
        self.before_change_styles.extend(snap.before_change);
        self.pseudo_before_change_styles
            .extend(snap.pseudo_before_change);
        self.active_transitions.extend(snap.active_transitions);
        self.pseudo_active_transitions
            .extend(snap.pseudo_active_transitions);
        self.managed_css_animations
            .extend(snap.managed_css_animations);
        self.web_animations.extend(snap.web_animations);
        self.transition_overrides.extend(snap.transition_overrides);
        if snap.next_transition_id > 0 {
            self.next_transition_id = snap.next_transition_id;
        }
        if snap.transition_generation > 0 {
            self.transition_generation = snap.transition_generation;
        }
        self.css_animations_managed = snap.css_animations_managed;
        if let Some(seed) = snap.random_doc_seed {
            self.random_doc_seed = seed;
            self.rule_index.random_doc_seed = seed;
        }
    }

    /// Enables or disables `WebAnimationSystem` management of CSS `@keyframes` animations.
    pub fn set_css_animations_managed(&mut self, managed: bool) {
        self.css_animations_managed = managed;
    }

    /// Replaces the list of `WebAnimationSystem`-managed CSS `@keyframes` animations on `node`.
    pub fn set_managed_css_animations_for_node(
        &mut self,
        node: NodeId,
        anims: Vec<WebAnimationSpec>,
    ) {
        if anims.is_empty() {
            self.managed_css_animations.remove(&node);
        } else {
            self.managed_css_animations.insert(node, anims);
        }
    }

    /// Sets or clears a script-driven control override (`currentTime`, `cancelled`, `disassociated`)
    /// for the CSS transition identified by `transition_id`.
    pub fn set_transition_override(
        &mut self,
        transition_id: u64,
        ov: Option<crate::animations::TransitionControlOverride>,
    ) {
        match ov {
            Some(v) => {
                self.transition_overrides.insert(transition_id, v);
                for list in self.active_transitions.values_mut() {
                    if let Some(tr) = list.iter_mut().find(|t| t.id == transition_id) {
                        tr.current_time_override_ms = v.current_time_ms;
                        tr.cancelled = v.cancelled;
                        tr.disassociated = v.disassociated;
                        if let Some(ct) = v.current_time_ms {
                            if crate::animations::compute_transition_eased_progress_local(
                                &tr.spec, ct,
                            )
                            .is_some()
                            {
                                tr.completed = false;
                            }
                        }
                    }
                }
                for list in self.pseudo_active_transitions.values_mut() {
                    if let Some(tr) = list.iter_mut().find(|t| t.id == transition_id) {
                        tr.current_time_override_ms = v.current_time_ms;
                        tr.cancelled = v.cancelled;
                        tr.disassociated = v.disassociated;
                        if let Some(ct) = v.current_time_ms {
                            if crate::animations::compute_transition_eased_progress_local(
                                &tr.spec, ct,
                            )
                            .is_some()
                            {
                                tr.completed = false;
                            }
                        }
                    }
                }
            }
            None => {
                self.transition_overrides.remove(&transition_id);
            }
        }
    }

    /// Returns all currently active (non-cancelled, non-disassociated, non-completed) CSS transitions across
    /// elements and pseudo-elements.
    #[must_use]
    pub fn all_active_transitions(&self) -> Vec<(NodeId, ActiveTransition)> {
        let mut out = Vec::new();
        for (&node, list) in &self.active_transitions {
            for tr in list {
                if !tr.cancelled && !tr.disassociated && !tr.completed {
                    out.push((node, tr.clone()));
                }
            }
        }
        for (&(node, _), list) in &self.pseudo_active_transitions {
            for tr in list {
                if !tr.cancelled && !tr.disassociated && !tr.completed {
                    out.push((node, tr.clone()));
                }
            }
        }
        out.sort_by_key(|(_, tr)| tr.id);
        out
    }

    /// Drains IDs of CSS transitions cancelled by style changes since the previous sync.
    pub fn drain_cancelled_transition_ids(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.cancelled_transition_ids)
    }

    /// Returns all styled elements and pseudo-elements that declare CSS `animation-name` specs.
    #[must_use]
    pub fn all_css_animation_candidates(
        &self,
    ) -> Vec<(NodeId, Option<String>, Vec<crate::values::AnimationSpec>)> {
        let mut out = Vec::new();
        for (&node, st) in &self.styles {
            if st.display == crate::values::DisplayMode::None {
                continue;
            }
            if let Some(eff) = &st.effects {
                if !eff.animations.is_empty() {
                    out.push((node, None, eff.animations.clone()));
                }
            }
        }
        for (&(node, pe), st) in &self.pseudo_styles {
            if st.display == crate::values::DisplayMode::None {
                continue;
            }
            if let Some(eff) = &st.effects {
                if !eff.animations.is_empty() {
                    let pe_str = match pe {
                        RixPseudoElement::Before => "::before",
                        RixPseudoElement::After => "::after",
                        RixPseudoElement::Marker => "::marker",
                        RixPseudoElement::FirstLine => "::first-line",
                        RixPseudoElement::FirstLetter => "::first-letter",
                        RixPseudoElement::Backdrop => "::backdrop",
                        RixPseudoElement::Placeholder => "::placeholder",
                        RixPseudoElement::Selection => "::selection",
                        RixPseudoElement::FileSelectorButton => "::file-selector-button",
                        RixPseudoElement::DetailsContent => "::details-content",
                        RixPseudoElement::Cue => "::cue",
                        RixPseudoElement::TargetText => "::target-text",
                    };
                    out.push((node, Some(pe_str.to_string()), eff.animations.clone()));
                }
            }
        }
        out
    }

    /// Sets the document animation clock in milliseconds (synchronized with `DocumentTimeline`).
    pub fn set_animation_clock_ms(&mut self, clock_ms: f32) {
        if clock_ms.is_finite() && clock_ms >= 0.0 {
            self.animation_clock_ms = clock_ms;
        }
    }

    /// Replaces the entire list of Web Animations attached to `node`.
    pub fn set_web_animations_for_node(&mut self, node: NodeId, anims: Vec<WebAnimationSpec>) {
        if anims.is_empty() {
            self.web_animations.remove(&node);
        } else {
            self.web_animations.insert(node, anims);
        }
    }

    /// Attaches a Web Animation (`Element.prototype.animate`) to `node`.
    pub fn add_web_animation(&mut self, node: NodeId, anim: WebAnimationSpec) {
        self.web_animations.entry(node).or_default().push(anim);
    }

    /// Mutates a Web Animation on `node` by `id`.
    pub fn update_web_animation(
        &mut self,
        node: NodeId,
        id: u64,
        f: impl FnOnce(&mut WebAnimationSpec),
    ) -> bool {
        if let Some(list) = self.web_animations.get_mut(&node) {
            if let Some(anim) = list.iter_mut().find(|a| a.id == id) {
                f(anim);
                return true;
            }
        }
        false
    }

    /// Returns all active (non-cancelled) Web Animations attached across the document.
    #[must_use]
    pub fn all_web_animations(&self) -> Vec<(NodeId, WebAnimationSpec)> {
        let mut out = Vec::new();
        for (&node, list) in &self.web_animations {
            for a in list {
                if !a.cancelled {
                    out.push((node, a.clone()));
                }
            }
        }
        out.sort_by_key(|(_, a)| a.id);
        out
    }

    /// Returns the current document animation clock in milliseconds.
    #[must_use]
    pub const fn animation_clock_ms(&self) -> f32 {
        self.animation_clock_ms
    }

    /// Advances the document animation clock (e.g. per animation frame).
    pub fn advance_animation_clock_ms(&mut self, delta_ms: f32) {
        if delta_ms > 0.0 {
            self.animation_clock_ms += delta_ms;
        }
    }

    /// Returns `true` if any element has active transitions or animations.
    #[must_use]
    pub fn has_active_animations(&self) -> bool {
        !self.active_transitions.is_empty()
            || !self.pseudo_active_transitions.is_empty()
            || !self.managed_css_animations.is_empty()
            || !self.web_animations.is_empty()
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

    /// Sets the compiled per-`ShadowRoot` adopted stylesheets (Constructable Stylesheets).
    pub fn set_shadow_adopted_stylesheets(&mut self, map: HashMap<NodeId, Vec<Arc<Stylesheet>>>) {
        self.shadow_adopted_sheets = map;
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

    /// Returns the active viewport/media query evaluation context.
    #[must_use]
    pub const fn media(&self) -> &MediaContext {
        &self.media
    }

    /// Looks up the winning `@keyframes <name>` rule across author & UA stylesheets
    /// respecting CSS Cascade Level 5 `@layer` precedence.
    #[must_use]
    pub fn keyframes(&self, name: &str) -> Option<&crate::stylesheet::KeyframesRule> {
        let key = rix_core::intern_str(name);
        if let Some(kf) = self.rule_index.keyframes.get(&key) {
            return Some(kf);
        }
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

    /// Registers a JS `CSS.registerProperty` definition (overrides `@property` rules).
    pub fn register_property(&mut self, prop: RegisteredProperty) {
        self.js_registered_properties.push(prop);
        self.index_dirty = true;
    }

    fn rebuild_index(&mut self) {
        let mut all = Vec::with_capacity(1 + self.author_sheets.len());
        if !self.ua_sheet.rules.is_empty() {
            all.push(Arc::clone(&self.ua_sheet));
        }
        all.extend(self.author_sheets.iter().cloned());
        let (mut index, inv) = RuleIndex::build(&all);
        index.random_doc_seed = self.random_doc_seed;
        for rp in &self.js_registered_properties {
            index
                .registered_properties
                .insert(rp.name.clone(), rp.clone());
        }
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

    /// Resolves a pseudo-element's [`ComputedStyle`] on demand (for `getComputedStyle(el, '::before')`
    /// when a non-generated `::before`/`::after` without `content` was skipped during bulk cascade).
    #[must_use]
    pub fn resolve_pseudo_style_on_demand(
        &self,
        doc: &Document,
        node: NodeId,
        pseudo: RixPseudoElement,
    ) -> Option<Arc<ComputedStyle>> {
        if let Some(cached) = self.pseudo_styles.get(&(node, pseudo)) {
            return Some(Arc::clone(cached));
        }
        let orig_style = self.styles.get(&node)?;
        let Some(NodeData::Element(orig_elem)) = doc.get(node) else {
            return None;
        };
        if !self.rule_index.has_pseudo_rules(pseudo) {
            return None;
        }
        let mut cands = SmallVec::<[u32; 16]>::new();
        self.rule_index
            .collect_pseudo_candidates(pseudo, orig_elem, None, &mut cands);
        if cands.is_empty() {
            return None;
        }
        let pseudo_el = DomElement::with_pseudo(doc, node, pseudo);
        let mut caches = SelectorCaches::default();
        let mut matched: SmallVec<[MatchedRule<'_>; 8]> = SmallVec::new();
        for &idx in &cands {
            let rule = &self.rule_index.rules[idx as usize];
            if let Some(scope_proximity) =
                match_rule_with_scopes(doc, node, &pseudo_el, rule, None, &mut caches, None)
            {
                matched.push(MatchedRule {
                    rule,
                    scope_proximity,
                    is_shadow_host_rule: false,
                });
            }
        }
        if matched.is_empty() {
            return None;
        }
        let root_font_size = doc
            .children(doc.root())
            .find_map(|c| self.styles.get(&c).map(|s| s.text.font_size_px))
            .unwrap_or(16.0);
        let (si, sc) = element_sibling_position(doc, node);
        let pseudo_nid = (usize::from(node) as u64) ^ (((pseudo as u64) + 1) << 48);
        let mut pstyle = resolve_element_style_with_context(
            &matched,
            &[],
            &[],
            Some(orig_style.as_ref()),
            root_font_size,
            &self.media,
            Some(&self.rule_index.registered_properties),
            si,
            sc,
            pseudo_nid,
            self.rule_index.random_doc_seed,
            &orig_elem.attributes,
        );
        if matches!(
            orig_style.display,
            crate::values::DisplayMode::Flex
                | crate::values::DisplayMode::InlineFlex
                | crate::values::DisplayMode::Grid
                | crate::values::DisplayMode::InlineGrid
        ) && matches!(pseudo, RixPseudoElement::Before | RixPseudoElement::After)
        {
            pstyle.display = pstyle.display.blockify();
        }
        Some(Arc::new(pstyle))
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
        }
        let inline_has_attr = matches!(
            doc.get(node),
            Some(NodeData::Element(e))
                if e.attributes.iter().any(|(q, v)| {
                    q.local.eq_str_ignore_ascii_case("style")
                        && crate::substitution::contains_values5_subst_fn_str(v)
                })
        );
        if inline_has_attr {
            mark_node_and_ancestors_dirty(doc, node);
        }
        let scope = self.invalidation_map.scope_for_attribute(attr_name);
        apply_invalidation_scope(doc, node, scope);
    }

    /// Marks affected nodes dirty when children/text are inserted into or removed from `parent`.
    pub fn invalidate_subtree_mutation(
        &mut self,
        doc: &mut Document,
        parent: NodeId,
        mutated_child: Option<NodeId>,
    ) {
        self.ensure_index();
        mark_node_and_ancestors_dirty(doc, parent);
        if let Some(child) = mutated_child {
            mark_subtree_style_dirty(doc, child);
        }
        let sibling_inline_math = doc.children(parent).any(|c| {
            matches!(doc.get(c), Some(NodeData::Element(e)) if e.attributes.iter().any(|(q, v)| {
                q.local.eq_str_ignore_ascii_case("style") && v.to_ascii_lowercase().contains("sibling-")
            }))
        });
        if !self.rule_index.allows_sibling_sharing
            || self.rule_index.has_relative_selectors
            || sibling_inline_math
        {
            // Structural pseudo-classes (`:first-child`, `:last-child`, `:nth-child`, `:only-child`,
            // `:empty`), sibling combinators (`+`, `~`), or `:has(...)`: invalidate siblings & subtrees.
            let children: Vec<NodeId> = doc
                .children(parent)
                .filter(|&c| matches!(doc.get(c), Some(NodeData::Element(_))))
                .collect();
            for c in children {
                mark_subtree_style_dirty(doc, c);
            }
        }
        if self.rule_index.has_relative_selectors {
            let mut rel_scope = InvalidationScope::ANCESTORS
                | InvalidationScope::EARLIER_SIBLINGS
                | InvalidationScope::ANCESTOR_EARLIER_SIBLINGS;
            if self.rule_index.has_non_subject_has {
                rel_scope |= InvalidationScope::ANCESTOR_SUBTREES;
            }
            apply_invalidation_scope(doc, parent, rel_scope);
        }
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

    /// Preserves an unquantized CSS Color 4/5 AST for an inline `element.style` declaration.
    pub fn set_inline_color_expr(
        &mut self,
        node: NodeId,
        prop: PropertyIdKind,
        expr: Option<CssColorExpr>,
    ) {
        if let Some(e) = expr {
            self.inline_color_exprs.insert((node, prop), Arc::new(e));
        } else {
            self.inline_color_exprs.remove(&(node, prop));
        }
    }

    /// Clears all preserved unquantized inline color expressions for `node`.
    pub fn clear_inline_color_exprs(&mut self, node: NodeId) {
        self.inline_color_exprs.retain(|&(n, _), _| n != node);
    }

    /// Returns `true` if any author rule uses CSS Containment Level 3 `@container` queries.
    #[must_use]
    pub fn has_container_queries(&self) -> bool {
        self.rule_index.has_container_queries
    }

    /// Returns `true` if at least one styled element establishes a CSS Containment L3 size
    /// query container (`container-type: size | inline-size`).
    #[must_use]
    pub fn has_size_containers(&self) -> bool {
        self.styles.values().any(|st| {
            st.effects
                .as_ref()
                .is_some_and(|e| e.container_type & 0x03 != 0)
        })
    }

    /// Updates laid-out `(content_width, content_height)` measurements for query container nodes
    /// and returns all container [`NodeId`]s whose dimensions changed by `> 0.5px`.
    pub fn set_container_sizes(&mut self, sizes: HashMap<NodeId, (f32, f32)>) -> Vec<NodeId> {
        let mut changed = Vec::new();
        for (&nid, &(w, h)) in &sizes {
            let prev = self.container_sizes.get(&nid).copied();
            let differs = prev.is_none_or(|(pw, ph)| (pw - w).abs() > 0.5 || (ph - h).abs() > 0.5);
            if differs {
                changed.push(nid);
            }
        }
        for &old_nid in self.container_sizes.keys() {
            if !sizes.contains_key(&old_nid) {
                changed.push(old_nid);
            }
        }
        self.container_sizes = sizes;
        changed
    }

    /// Appends a new author stylesheet and incrementally restyles ONLY affected DOM elements
    /// when safe (e.g. CSS-in-JS / runtime `<style>` appends on large documents), falling back
    /// to `cascade_full` only when the stylesheet contains universal/root/layer rules.
    pub fn append_stylesheet_incremental(
        &mut self,
        doc: &mut Document,
        sheet: Arc<Stylesheet>,
    ) -> RestyleReport {
        self.add_stylesheet(Arc::clone(&sheet));
        self.ensure_index();
        if self.styles.is_empty() || !RuleIndex::mark_affected_nodes_dirty_for_sheet(doc, &sheet) {
            return self.cascade_full(doc);
        }
        self.restyle_dirty(doc)
    }

    /// Performs a full parallel top-down cascade over the entire document.
    pub fn cascade_full(&mut self, doc: &mut Document) -> RestyleReport {
        let root = doc.root();
        self.cascade_full_from(doc, root)
    }

    /// Performs a full parallel top-down cascade starting at `doc_root` (supports sub-documents
    /// such as same-origin `iframe.contentDocument`).
    pub fn cascade_full_from(&mut self, doc: &mut Document, doc_root: NodeId) -> RestyleReport {
        self.ensure_index();
        self.styles.clear();
        self.pseudo_styles.clear();

        let root_children: Vec<NodeId> = doc
            .children(doc_root)
            .filter(|&c| matches!(doc.get(c), Some(NodeData::Element(_))))
            .collect();

        let mut results = Vec::new();
        let mut pseudo_results = Vec::new();
        let mut missing_local = Vec::new();
        let mut report = RestyleReport::default();

        let mut bloom = BloomFilter::new();
        let mut caches = SelectorCaches::default();
        let mut memo = StyleMemo::with_capacity(256);
        let mut active_containers: Vec<ActiveContainer> = Vec::new();

        for child in root_children {
            cascade_subtree_par(
                doc,
                child,
                None,
                false,
                16.0,
                true,
                None,
                &self.ua_sheet,
                &self.shadow_adopted_sheets,
                &self.rule_index,
                &self.inline_color_exprs,
                &self.media,
                &self.container_sizes,
                &mut active_containers,
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
        let mut styled_ids = Vec::with_capacity(results.len());
        for (id, style) in results {
            styled_ids.push(id);
            self.styles.insert(id, style);
            doc.clear_dirty_flags(
                id,
                DirtyFlags::SELF_STYLE_DIRTY | DirtyFlags::DESCENDANT_STYLE_DIRTY,
            );
            doc.mark_node_dirty(id, DirtyFlags::SELF_LAYOUT_DIRTY | DirtyFlags::PAINT_DIRTY);
        }
        for ((id, pe), style) in pseudo_results {
            self.pseudo_styles.insert((id, pe), style);
        }
        self.apply_post_cascade_animations(doc, &styled_ids, None);
        doc.clear_dirty_flags(
            doc_root,
            DirtyFlags::SELF_STYLE_DIRTY | DirtyFlags::DESCENDANT_STYLE_DIRTY,
        );
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

        let mut anim_ctx = AnimWalkCtx {
            dark_mode: self.media.dark_mode,
            elapsed_ms: self.animation_clock_ms,
            generation: self.transition_generation,
            css_animations_managed: self.css_animations_managed,
            rule_index: &self.rule_index,
            before_change_styles: &mut self.before_change_styles,
            pseudo_before_change_styles: &mut self.pseudo_before_change_styles,
            pre_anim_styles: &mut self.pre_anim_styles,
            active_transitions: &mut self.active_transitions,
            pseudo_active_transitions: &mut self.pseudo_active_transitions,
            managed_css_animations: &self.managed_css_animations,
            web_animations: &self.web_animations,
            transition_overrides: &self.transition_overrides,
            next_transition_id: &mut self.next_transition_id,
            started_any: false,
            cancelled_local: Vec::new(),
        };

        let mut active_containers: Vec<ActiveContainer> = Vec::new();
        for child in root_children {
            restyle_dirty_walk(
                doc,
                child,
                None,
                false,
                root_font_size,
                false,
                false,
                &[],
                None,
                &self.ua_sheet,
                &self.shadow_adopted_sheets,
                &self.rule_index,
                &self.inline_color_exprs,
                &self.media,
                &self.container_sizes,
                &mut active_containers,
                &mut bloom,
                &mut caches,
                &mut memo,
                &mut self.styles,
                &mut self.pseudo_styles,
                &mut anim_ctx,
                &mut missing_local,
                &mut cleared_nodes,
                &mut layout_dirty_nodes,
                &mut paint_dirty_nodes,
                &mut report,
            );
        }

        let started_any = anim_ctx.started_any;
        let cancelled_local = anim_ctx.cancelled_local;
        if started_any {
            self.transition_generation = self.transition_generation.wrapping_add(1).max(1);
        }
        self.cancelled_transition_ids.extend(cancelled_local);

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

    #[rustfmt::skip]
    fn apply_post_cascade_animations(
        &mut self, doc: &Document, node_ids: &[NodeId], mut extra_layout_dirty: Option<&mut Vec<NodeId>>,
    ) {
        let mut anim_ctx = AnimWalkCtx {
            dark_mode: self.media.dark_mode,
            elapsed_ms: self.animation_clock_ms,
            generation: self.transition_generation,
            css_animations_managed: self.css_animations_managed,
            rule_index: &self.rule_index,
            before_change_styles: &mut self.before_change_styles,
            pseudo_before_change_styles: &mut self.pseudo_before_change_styles,
            pre_anim_styles: &mut self.pre_anim_styles,
            active_transitions: &mut self.active_transitions,
            pseudo_active_transitions: &mut self.pseudo_active_transitions,
            managed_css_animations: &self.managed_css_animations,
            web_animations: &self.web_animations,
            transition_overrides: &self.transition_overrides,
            next_transition_id: &mut self.next_transition_id,
            started_any: false,
            cancelled_local: Vec::new(),
        };
        for &id in node_ids {
            let mut hidden_ancestor = false;
            let mut anc_trs: Vec<(PropertyIdKind, Option<rix_core::InternedStr>)> = Vec::new();
            let mut anc = doc.parent(id);
            while let Some(a) = anc {
                if self.styles.get(&a).is_some_and(|s| s.display == crate::values::DisplayMode::None) {
                    hidden_ancestor = true;
                }
                if let Some(list) = anim_ctx.active_transitions.get(&a) {
                    for tr in list {
                        if !tr.cancelled && !tr.disassociated && !tr.completed {
                            anc_trs.push((tr.prop, tr.custom_name.clone()));
                        }
                    }
                }
                anc = doc.parent(a);
            }
            let _ = anim_ctx.apply_node(
                doc, id, &mut self.styles, &mut self.pseudo_styles,
                hidden_ancestor, &anc_trs, extra_layout_dirty.as_deref_mut(),
            );
        }
        let started_any = anim_ctx.started_any;
        let cancelled_local = anim_ctx.cancelled_local;
        if started_any {
            self.transition_generation = self.transition_generation.wrapping_add(1).max(1);
        }
        self.cancelled_transition_ids.extend(cancelled_local);
    }

    /// Evaluates the computed style of `id` applying only Web Animations up to and including
    /// `commit_anim_id` (with W3C Web Animations §4.4.17 `commitStyles` inclusive boundary fill).
    #[must_use]
    pub fn compute_style_for_web_anim_commit(
        &self,
        doc: &Document,
        id: NodeId,
        commit_anim_id: u64,
    ) -> Option<Arc<ComputedStyle>> {
        let base_style = self
            .pre_anim_styles
            .get(&id)
            .or_else(|| self.styles.get(&id))
            .cloned()?;
        let parent_style = doc
            .parent(id)
            .and_then(|pid| self.styles.get(&pid))
            .cloned();
        let web_slice = self
            .web_animations
            .get(&id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        Some(crate::animations::apply_web_animations_for_commit(
            base_style,
            parent_style.as_deref(),
            self.media.dark_mode,
            web_slice,
            commit_anim_id,
            &self.rule_index.registered_properties,
        ))
    }
}

#[inline]
fn next_layout_parent_flex_grid(style: &ComputedStyle, inherited: bool) -> bool {
    match style.display {
        crate::values::DisplayMode::Contents => inherited,
        crate::values::DisplayMode::Flex
        | crate::values::DisplayMode::InlineFlex
        | crate::values::DisplayMode::Grid
        | crate::values::DisplayMode::InlineGrid
        | crate::values::DisplayMode::GridLanes
        | crate::values::DisplayMode::InlineGridLanes
        | crate::values::DisplayMode::InlineGridLanesMulti => true,
        _ => false,
    }
}

/// Builds a tree-scoped [`RuleIndex`] for a `ShadowRoot` containing the UA stylesheet,
/// any `<style>` elements inside that shadow tree, and its `adoptedStyleSheets` in cascade order.
fn build_shadow_rule_index(
    doc: &Document,
    sr_id: NodeId,
    ua_sheet: &Arc<Stylesheet>,
    shadow_adopted: &HashMap<NodeId, Vec<Arc<Stylesheet>>>,
    media: &MediaContext,
) -> RuleIndex {
    let mut sheets = Vec::new();
    if !ua_sheet.rules.is_empty() {
        sheets.push(Arc::clone(ua_sheet));
    }
    for nid in doc.descendants(sr_id) {
        if let Some(NodeData::Element(el)) = doc.get(nid) {
            if el.name.local.as_str().eq_ignore_ascii_case("style") {
                let css = doc.inner_text(nid);
                if !css.trim().is_empty() {
                    sheets.push(Arc::new(Stylesheet::parse(
                        &css,
                        crate::stylesheet::CascadeOrigin::Author,
                        media,
                    )));
                }
            }
        }
    }
    if let Some(adopted) = shadow_adopted.get(&sr_id) {
        sheets.extend(adopted.iter().cloned());
    }
    let (index, _) = RuleIndex::build(&sheets);
    index
}

/// Active ancestor CSS Containment L3 query container pushed during top-down cascade.
#[derive(Debug, Clone)]
struct ActiveContainer {
    container_type: u8,
    names: Vec<rix_core::InternedStr>,
    width: f32,
    height: f32,
    custom_props: Arc<crate::computed::CustomPropertyMap>,
}

fn maybe_push_active_container(
    node: NodeId,
    style: &ComputedStyle,
    media: &MediaContext,
    container_sizes: &HashMap<NodeId, (f32, f32)>,
    active_containers: &mut Vec<ActiveContainer>,
) -> bool {
    let Some(ref eff) = style.effects else {
        return false;
    };
    if eff.container_type == 0 && eff.container_name.is_empty() {
        return false;
    }
    let (w, h) = container_sizes
        .get(&node)
        .copied()
        .unwrap_or((media.viewport_width, media.viewport_height));
    active_containers.push(ActiveContainer {
        container_type: eff.container_type,
        names: eff.container_name.clone(),
        width: w,
        height: h,
        custom_props: Arc::clone(&style.custom_properties),
    });
    true
}

fn nearest_size_container(active: &[ActiveContainer]) -> Option<(f32, f32)> {
    active
        .iter()
        .rev()
        .find(|c| c.container_type & 0x03 != 0)
        .map(|c| (c.width, c.height))
}

fn matches_container_queries(
    queries: &[crate::stylesheet::CompiledContainerQuery],
    active: &[ActiveContainer],
    root_font_size: f32,
    elem_font_size: f32,
) -> bool {
    for q in queries {
        let req_size = q.requires_size_container();
        let matched_container = active.iter().rev().find(|c| {
            let type_ok = if req_size {
                c.container_type & 0x03 != 0
            } else {
                true
            };
            let name_ok = match &q.name {
                Some(req_name) => c.names.iter().any(|n| n == req_name),
                None => true,
            };
            type_ok && name_ok
        });
        let Some(container) = matched_container else {
            return false;
        };
        if !q.matches(
            container.width,
            container.height,
            root_font_size,
            elem_font_size,
            &container.custom_props,
        ) {
            return false;
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn cascade_subtree_par(
    doc: &Document,
    node: NodeId,
    parent_style: Option<&Arc<ComputedStyle>>,
    layout_parent_flex_grid: bool,
    root_font_size: f32,
    allow_parallel: bool,
    shadow_host: Option<selectors::OpaqueElement>,
    ua_sheet: &Arc<Stylesheet>,
    shadow_adopted: &HashMap<NodeId, Vec<Arc<Stylesheet>>>,
    rule_index: &RuleIndex,
    inline_color_exprs: &FastMap<(NodeId, PropertyIdKind), Arc<CssColorExpr>>,
    media: &MediaContext,
    container_sizes: &HashMap<NodeId, (f32, f32)>,
    active_containers: &mut Vec<ActiveContainer>,
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

    let shadow_root_id = elem.shadow_root;
    let own_shadow_index = shadow_root_id
        .map(|sr_id| build_shadow_rule_index(doc, sr_id, ua_sheet, shadow_adopted, media));

    let (style, hit) = compute_single_element_style(
        doc,
        node,
        elem,
        parent_style,
        layout_parent_flex_grid,
        root_font_size,
        shadow_host,
        own_shadow_index.as_ref(),
        rule_index,
        inline_color_exprs,
        media,
        active_containers,
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
    let next_lp_fg = next_layout_parent_flex_grid(&style, layout_parent_flex_grid);

    collect_pseudo_element_styles(
        doc,
        node,
        &style,
        next_lp_fg,
        next_root_font,
        shadow_host,
        rule_index,
        media,
        Some(bloom),
        caches,
        memo,
        out_pseudos,
    );
    out_styles.push((node, Arc::clone(&style)));

    let pushed_container =
        maybe_push_active_container(node, &style, media, container_sizes, active_containers);

    let elem_children: SmallVec<[NodeId; 16]> = doc
        .children(node)
        .filter(|&c| matches!(doc.get(c), Some(NodeData::Element(_))))
        .collect();

    if let (Some(sr_id), Some(ref shadow_index)) = (shadow_root_id, own_shadow_index) {
        let shadow_children: SmallVec<[NodeId; 16]> = doc
            .children(sr_id)
            .filter(|&c| matches!(doc.get(c), Some(NodeData::Element(_))))
            .collect();
        if !shadow_children.is_empty() {
            let host_dom_el = DomElement::new(doc, node);
            let host_opaque = Some(host_dom_el.opaque());
            let mut shadow_bloom = BloomFilter::new();
            push_bloom_hashes(&host_dom_el, &mut shadow_bloom);
            let mut shadow_memo = StyleMemo::with_capacity(32);
            for &sc in &shadow_children {
                cascade_subtree_par(
                    doc,
                    sc,
                    Some(&style),
                    next_lp_fg,
                    next_root_font,
                    false,
                    host_opaque,
                    ua_sheet,
                    shadow_adopted,
                    shadow_index,
                    inline_color_exprs,
                    media,
                    container_sizes,
                    active_containers,
                    &mut shadow_bloom,
                    caches,
                    &mut shadow_memo,
                    out_styles,
                    out_pseudos,
                    out_missing,
                    report,
                );
            }
        }
    }

    if elem_children.is_empty() {
        if pushed_container {
            active_containers.pop();
        }
        return;
    }

    let dom_el = DomElement::new(doc, node);
    push_bloom_hashes(&dom_el, bloom);

    if allow_parallel && elem_children.len() >= PARALLEL_CHILD_THRESHOLD {
        let parent_arc = Arc::clone(&style);
        let bloom_snapshot = bloom.clone();
        let container_snapshot = active_containers.clone();
        let threads = rayon::current_num_threads().max(1);
        let chunk_size = elem_children
            .len()
            .div_ceil(threads)
            .max(MIN_PARALLEL_CHUNK_SIZE);
        let chunk_outputs: Vec<_> = elem_children
            .par_chunks(chunk_size)
            .map(|chunk| {
                let mut local_bloom = bloom_snapshot.clone();
                let mut local_containers = container_snapshot.clone();
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
                        next_lp_fg,
                        next_root_font,
                        false,
                        shadow_host,
                        ua_sheet,
                        shadow_adopted,
                        rule_index,
                        inline_color_exprs,
                        media,
                        container_sizes,
                        &mut local_containers,
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
                next_lp_fg,
                next_root_font,
                allow_parallel,
                shadow_host,
                ua_sheet,
                shadow_adopted,
                rule_index,
                inline_color_exprs,
                media,
                container_sizes,
                active_containers,
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
    if pushed_container {
        active_containers.pop();
    }
}

struct AnimWalkCtx<'a> {
    dark_mode: bool,
    elapsed_ms: f32,
    generation: u64,
    css_animations_managed: bool,
    rule_index: &'a RuleIndex,
    before_change_styles: &'a mut FastMap<NodeId, Arc<ComputedStyle>>,
    pseudo_before_change_styles: &'a mut FastMap<(NodeId, RixPseudoElement), Arc<ComputedStyle>>,
    pre_anim_styles: &'a mut FastMap<NodeId, Arc<ComputedStyle>>,
    active_transitions: &'a mut FastMap<NodeId, Vec<ActiveTransition>>,
    pseudo_active_transitions: &'a mut FastMap<(NodeId, RixPseudoElement), Vec<ActiveTransition>>,
    managed_css_animations: &'a FastMap<NodeId, Vec<WebAnimationSpec>>,
    web_animations: &'a FastMap<NodeId, Vec<WebAnimationSpec>>,
    transition_overrides: &'a HashMap<u64, crate::animations::TransitionControlOverride>,
    next_transition_id: &'a mut u64,
    started_any: bool,
    cancelled_local: Vec<u64>,
}

impl AnimWalkCtx<'_> {
    #[allow(clippy::too_many_arguments)]
    #[rustfmt::skip]
    fn apply_node(
        &mut self,
        doc: &Document,
        id: NodeId,
        styles: &mut FastMap<NodeId, Arc<ComputedStyle>>,
        pseudo_styles: &mut FastMap<(NodeId, RixPseudoElement), Arc<ComputedStyle>>,
        hidden_ancestor: bool,
        ancestor_transitioning: &[(PropertyIdKind, Option<rix_core::InternedStr>)],
        extra_layout_dirty: Option<&mut Vec<NodeId>>,
    ) -> Option<Arc<ComputedStyle>> {
        let mut base_style = styles.get(&id).cloned()?;
        let eff = base_style.effects.as_ref();
        let has_css_anims = eff.is_some_and(|e| !e.animations.is_empty());
        let has_css_tr = eff.is_some_and(|e| !e.transitions.is_empty());
        let has_managed = self.managed_css_animations.contains_key(&id);
        let has_web = self.web_animations.contains_key(&id);
        let has_active_tr = self.active_transitions.contains_key(&id);
        let has_pseudo_work = [
            RixPseudoElement::Before,
            RixPseudoElement::After,
            RixPseudoElement::Marker,
        ]
        .iter()
        .any(|&pe| {
            self.pseudo_active_transitions.contains_key(&(id, pe))
                || pseudo_styles
                    .get(&(id, pe))
                    .and_then(|s| s.effects.as_ref())
                    .is_some_and(|e| !e.transitions.is_empty() || !e.animations.is_empty())
        });
        if !has_css_anims && !has_css_tr && !has_managed && !has_web && !has_active_tr && !has_pseudo_work {
            self.before_change_styles.insert(id, Arc::clone(&base_style));
            return Some(base_style);
        }
        if has_css_anims || has_managed || has_web {
            let (sib_idx, sib_cnt) = element_sibling_position(doc, id);
            let mut cloned = (*base_style).clone();
            let t = Arc::make_mut(&mut cloned.text);
            t.eval_ctx.sibling_index = sib_idx;
            t.eval_ctx.sibling_count = sib_cnt;
            base_style = Arc::new(cloned);
        }
        let parent_style = doc.parent(id).and_then(|pid| styles.get(&pid)).cloned();
        let before = self.before_change_styles.get(&id).cloned();
        let managed_slice = self.managed_css_animations.get(&id).cloned();
        let web_slice = self.web_animations.get(&id).cloned();
        if has_web || has_managed {
            self.pre_anim_styles.insert(id, Arc::clone(&base_style));
        } else {
            self.pre_anim_styles.remove(&id);
        }
        let mut active_trs = self.active_transitions.remove(&id).unwrap_or_default();
        let final_style = apply_animations_and_transitions(
            Arc::clone(&base_style),
            parent_style.as_deref(),
            self.dark_mode,
            self.elapsed_ms,
            &|name| self.rule_index.keyframes.get(&rix_core::intern_str(name)),
            self.css_animations_managed,
            managed_slice.as_deref(),
            web_slice.as_deref(),
            before.as_ref(),
            &mut active_trs,
            &self.rule_index.registered_properties,
            None,
            hidden_ancestor,
            ancestor_transitioning,
            self.transition_overrides,
            self.next_transition_id,
            self.generation,
            &mut self.started_any,
            &mut self.cancelled_local,
        );
        if !active_trs.is_empty() {
            self.active_transitions.insert(id, active_trs);
        }
        if let Some(dirty_out) = extra_layout_dirty {
            if before.as_deref() != Some(&*final_style) || final_style != base_style {
                dirty_out.push(id);
            }
        }
        let pe_hidden = hidden_ancestor || final_style.display == crate::values::DisplayMode::None;
        for &(pe_name, pe_kind) in &[
            ("::before", RixPseudoElement::Before),
            ("::after", RixPseudoElement::After),
            ("::marker", RixPseudoElement::Marker),
            ("::first-line", RixPseudoElement::FirstLine),
            ("::first-letter", RixPseudoElement::FirstLetter),
            ("::backdrop", RixPseudoElement::Backdrop),
            ("::placeholder", RixPseudoElement::Placeholder),
        ] {
            let has_pe_web = web_slice.as_ref().is_some_and(|w| w.iter().any(|a| a.pseudo_element.as_deref() == Some(pe_name) && !a.cancelled && !a.removed));
            let has_pe_man = managed_slice.as_ref().is_some_and(|w| w.iter().any(|a| a.pseudo_element.as_deref() == Some(pe_name) && !a.cancelled && !a.removed));
            let has_pe_tr = self.pseudo_active_transitions.contains_key(&(id, pe_kind))
                || pseudo_styles.get(&(id, pe_kind)).and_then(|s| s.effects.as_ref()).is_some_and(|e| !e.transitions.is_empty() || !e.animations.is_empty());
            if !has_pe_web && !has_pe_man && !has_pe_tr {
                if let Some(pe_st) = pseudo_styles.get(&(id, pe_kind)).cloned() {
                    if matches!(pe_kind, RixPseudoElement::Before | RixPseudoElement::After | RixPseudoElement::Marker) {
                        self.pseudo_before_change_styles.insert((id, pe_kind), pe_st);
                    }
                }
                continue;
            }
            let base_pe = pseudo_styles
                .get(&(id, pe_kind))
                .cloned()
                .unwrap_or_else(|| Arc::new(ComputedStyle::inherit_from(&final_style)));
            let pe_before = self.pseudo_before_change_styles.get(&(id, pe_kind)).cloned();
            let mut pe_trs = self.pseudo_active_transitions.remove(&(id, pe_kind)).unwrap_or_default();
            let animated_pe = apply_animations_and_transitions(
                Arc::clone(&base_pe),
                Some(&final_style),
                self.dark_mode,
                self.elapsed_ms,
                &|name| self.rule_index.keyframes.get(&rix_core::intern_str(name)),
                self.css_animations_managed,
                managed_slice.as_deref(),
                web_slice.as_deref(),
                pe_before.as_ref(),
                &mut pe_trs,
                &self.rule_index.registered_properties,
                Some(pe_name),
                pe_hidden,
                &[],
                self.transition_overrides,
                self.next_transition_id,
                self.generation,
                &mut self.started_any,
                &mut self.cancelled_local,
            );
            if !pe_trs.is_empty() {
                self.pseudo_active_transitions.insert((id, pe_kind), pe_trs);
            }
            self.pseudo_before_change_styles.insert((id, pe_kind), Arc::clone(&animated_pe));
            pseudo_styles.insert((id, pe_kind), animated_pe);
        }
        self.before_change_styles.insert(id, Arc::clone(&final_style));
        styles.insert(id, Arc::clone(&final_style));
        Some(final_style)
    }
}

#[allow(clippy::too_many_arguments)]
fn restyle_dirty_walk(
    doc: &Document,
    node: NodeId,
    parent_style: Option<&Arc<ComputedStyle>>,
    layout_parent_flex_grid: bool,
    root_font_size: f32,
    force_inherit_restyle: bool,
    hidden_ancestor: bool,
    ancestor_transitioning: &[(PropertyIdKind, Option<rix_core::InternedStr>)],
    shadow_host: Option<selectors::OpaqueElement>,
    ua_sheet: &Arc<Stylesheet>,
    shadow_adopted: &HashMap<NodeId, Vec<Arc<Stylesheet>>>,
    rule_index: &RuleIndex,
    inline_color_exprs: &FastMap<(NodeId, PropertyIdKind), Arc<CssColorExpr>>,
    media: &MediaContext,
    container_sizes: &HashMap<NodeId, (f32, f32)>,
    active_containers: &mut Vec<ActiveContainer>,
    bloom: &mut BloomFilter,
    caches: &mut SelectorCaches,
    memo: &mut StyleMemo,
    styles: &mut FastMap<NodeId, Arc<ComputedStyle>>,
    pseudo_styles: &mut FastMap<(NodeId, RixPseudoElement), Arc<ComputedStyle>>,
    anim_ctx: &mut AnimWalkCtx<'_>,
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

    let own_shadow_index = elem
        .shadow_root
        .map(|sr_id| build_shadow_rule_index(doc, sr_id, ua_sheet, shadow_adopted, media));

    let mut propagate_inherited = false;
    let mut style_changed = false;
    let current_style = if needs_self {
        let old_style_opt = styles.get(&node).cloned();
        let (new_style, hit) = compute_single_element_style(
            doc,
            node,
            elem,
            parent_style,
            layout_parent_flex_grid,
            root_font_size,
            shadow_host,
            own_shadow_index.as_ref(),
            rule_index,
            inline_color_exprs,
            media,
            active_containers,
            Some(bloom),
            caches,
            memo,
            missing_out,
        );
        report.restyled_elements += 1;
        if hit {
            report.shared_memo_hits += 1;
        }

        let next_lp_fg = next_layout_parent_flex_grid(&new_style, layout_parent_flex_grid);
        let mut pseudos = Vec::new();
        collect_pseudo_element_styles(
            doc,
            node,
            &new_style,
            next_lp_fg,
            root_font_size,
            shadow_host,
            rule_index,
            media,
            Some(bloom),
            caches,
            memo,
            &mut pseudos,
        );
        for pe in [
            RixPseudoElement::Before,
            RixPseudoElement::After,
            RixPseudoElement::FirstLine,
            RixPseudoElement::FirstLetter,
            RixPseudoElement::Marker,
            RixPseudoElement::Placeholder,
            RixPseudoElement::Backdrop,
            RixPseudoElement::FileSelectorButton,
            RixPseudoElement::DetailsContent,
        ] {
            pseudo_styles.remove(&(node, pe));
        }
        for ((nid, pe), pstyle) in pseudos {
            pseudo_styles.insert((nid, pe), pstyle);
        }

        styles.insert(node, Arc::clone(&new_style));
        let mut final_style = anim_ctx
            .apply_node(
                doc,
                node,
                styles,
                pseudo_styles,
                hidden_ancestor,
                ancestor_transitioning,
                Some(layout_dirty),
            )
            .unwrap_or(new_style);

        if let Some(old_style) = old_style_opt {
            if !old_style.layout_eq(&final_style) {
                layout_dirty.push(node);
                report.layout_dirty_count += 1;
                style_changed = true;
            } else if *old_style != *final_style {
                paint_dirty.push(node);
                report.paint_only_count += 1;
                style_changed = true;
            }
            if !style_changed {
                // Restore the identical previous `Arc<ComputedStyle>` so descendants retain
                // `Arc::ptr_eq` on `parent_style` and `custom_properties`, preserving `StyleMemo`
                // hits and O(1) `CustomPropertyMap::eq` across incremental restyles.
                styles.insert(node, Arc::clone(&old_style));
                final_style = old_style;
            } else {
                let text_changed = !Arc::ptr_eq(&old_style.text, &final_style.text)
                    && old_style.text != final_style.text;
                let vars_changed =
                    !Arc::ptr_eq(&old_style.custom_properties, &final_style.custom_properties)
                        && old_style.custom_properties != final_style.custom_properties;
                if !vars_changed
                    && !Arc::ptr_eq(&old_style.custom_properties, &final_style.custom_properties)
                {
                    let mut shared = (*final_style).clone();
                    shared.custom_properties = Arc::clone(&old_style.custom_properties);
                    final_style = Arc::new(shared);
                    styles.insert(node, Arc::clone(&final_style));
                }
                let inherited_scalar_changed = old_style.visibility != final_style.visibility
                    || old_style.pointer_events != final_style.pointer_events
                    || old_style.cursor != final_style.cursor;
                let display_none_toggled = (old_style.display == crate::values::DisplayMode::None)
                    != (final_style.display == crate::values::DisplayMode::None);
                let blockify_changed = old_style.display != final_style.display
                    && (matches!(
                        old_style.display,
                        crate::values::DisplayMode::Flex
                            | crate::values::DisplayMode::InlineFlex
                            | crate::values::DisplayMode::Grid
                            | crate::values::DisplayMode::InlineGrid
                            | crate::values::DisplayMode::GridLanes
                            | crate::values::DisplayMode::InlineGridLanes
                            | crate::values::DisplayMode::InlineGridLanesMulti
                            | crate::values::DisplayMode::Contents
                    ) || matches!(
                        final_style.display,
                        crate::values::DisplayMode::Flex
                            | crate::values::DisplayMode::InlineFlex
                            | crate::values::DisplayMode::Grid
                            | crate::values::DisplayMode::InlineGrid
                            | crate::values::DisplayMode::GridLanes
                            | crate::values::DisplayMode::InlineGridLanes
                            | crate::values::DisplayMode::InlineGridLanesMulti
                            | crate::values::DisplayMode::Contents
                    ));
                propagate_inherited = text_changed
                    || vars_changed
                    || inherited_scalar_changed
                    || blockify_changed
                    || display_none_toggled;
            }
        } else {
            layout_dirty.push(node);
            report.layout_dirty_count += 1;
            propagate_inherited = true;
            style_changed = true;
        }
        final_style
    } else if let Some(existing) = styles.get(&node) {
        Arc::clone(existing)
    } else {
        return;
    };

    if flags.intersects(DirtyFlags::SELF_STYLE_DIRTY | DirtyFlags::DESCENDANT_STYLE_DIRTY) {
        cleared_nodes.push(node);
    }

    let propagate_through_contents =
        force_inherit_restyle && current_style.display == crate::values::DisplayMode::Contents;
    let any_child_explicit_inherit = style_changed
        && doc
            .children(node)
            .any(|c| styles.get(&c).is_some_and(|s| s.uses_explicit_inherit));
    if !needs_desc
        && !propagate_inherited
        && !propagate_through_contents
        && !any_child_explicit_inherit
    {
        return;
    }

    let next_hidden_ancestor =
        hidden_ancestor || current_style.display == crate::values::DisplayMode::None;
    let mut next_anc_trs: SmallVec<[(PropertyIdKind, Option<rix_core::InternedStr>); 8]> =
        ancestor_transitioning.iter().cloned().collect();
    if let Some(list) = anim_ctx.active_transitions.get(&node) {
        for tr in list {
            if !tr.cancelled && !tr.disassociated && !tr.completed {
                let key = (tr.prop, tr.custom_name.clone());
                if !next_anc_trs.contains(&key) {
                    next_anc_trs.push(key);
                }
            }
        }
    }

    let dom_el = DomElement::new(doc, node);
    push_bloom_hashes(&dom_el, bloom);
    let next_root_font = if parent_style.is_none() {
        current_style.text.font_size_px
    } else {
        root_font_size
    };
    let next_lp_fg = next_layout_parent_flex_grid(&current_style, layout_parent_flex_grid);
    let force_children = propagate_inherited || propagate_through_contents;

    if let (Some(sr_id), Some(ref shadow_index)) = (elem.shadow_root, own_shadow_index) {
        let host_opaque = Some(dom_el.opaque());
        let mut shadow_bloom = BloomFilter::new();
        push_bloom_hashes(&dom_el, &mut shadow_bloom);
        let mut shadow_memo = StyleMemo::default();
        for sc in doc.children(sr_id) {
            if matches!(doc.get(sc), Some(NodeData::Element(_))) {
                let sc_force = force_children
                    || !styles.contains_key(&sc)
                    || (style_changed && styles.get(&sc).is_some_and(|s| s.uses_explicit_inherit));
                restyle_dirty_walk(
                    doc,
                    sc,
                    Some(&current_style),
                    next_lp_fg,
                    next_root_font,
                    sc_force,
                    next_hidden_ancestor,
                    &next_anc_trs,
                    host_opaque,
                    ua_sheet,
                    shadow_adopted,
                    shadow_index,
                    inline_color_exprs,
                    media,
                    container_sizes,
                    active_containers,
                    &mut shadow_bloom,
                    caches,
                    &mut shadow_memo,
                    styles,
                    pseudo_styles,
                    anim_ctx,
                    missing_out,
                    cleared_nodes,
                    layout_dirty,
                    paint_dirty,
                    report,
                );
            }
        }
    }

    let pushed_container = maybe_push_active_container(
        node,
        &current_style,
        media,
        container_sizes,
        active_containers,
    );
    for child in doc.children(node) {
        if matches!(doc.get(child), Some(NodeData::Element(_))) {
            let child_force = force_children
                || (style_changed && styles.get(&child).is_some_and(|s| s.uses_explicit_inherit));
            restyle_dirty_walk(
                doc,
                child,
                Some(&current_style),
                next_lp_fg,
                next_root_font,
                child_force,
                next_hidden_ancestor,
                &next_anc_trs,
                shadow_host,
                ua_sheet,
                shadow_adopted,
                rule_index,
                inline_color_exprs,
                media,
                container_sizes,
                active_containers,
                bloom,
                caches,
                memo,
                styles,
                pseudo_styles,
                anim_ctx,
                missing_out,
                cleared_nodes,
                layout_dirty,
                paint_dirty,
                report,
            );
        }
    }
    if pushed_container {
        active_containers.pop();
    }
    pop_bloom_hashes(&dom_el, bloom);
}

#[allow(clippy::too_many_arguments)]
fn compute_single_element_style(
    doc: &Document,
    node: NodeId,
    elem: &ElementData,
    parent_style: Option<&Arc<ComputedStyle>>,
    layout_parent_flex_grid: bool,
    root_font_size: f32,
    shadow_host: Option<selectors::OpaqueElement>,
    own_shadow_index: Option<&RuleIndex>,
    rule_index: &RuleIndex,
    inline_color_exprs: &FastMap<(NodeId, PropertyIdKind), Arc<CssColorExpr>>,
    media: &MediaContext,
    active_containers: &[ActiveContainer],
    bloom: Option<&BloomFilter>,
    caches: &mut SelectorCaches,
    memo: &mut StyleMemo,
    missing_out: &mut Vec<MissingCssFeature>,
) -> (Arc<ComputedStyle>, bool) {
    let has_own_shadow_host_rules = own_shadow_index.is_some_and(RuleIndex::has_host_rules);
    let parent_ptr = parent_style.map_or(0, |p| Arc::as_ptr(p) as usize);
    let has_active_containers = !active_containers.is_empty();
    let sibling_parent = if rule_index.allows_sibling_sharing
        && !has_own_shadow_host_rules
        && !has_active_containers
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

    let elem_fs_basis = parent_style.map_or(root_font_size, |p| p.text.font_size_px);
    let dom_el = DomElement::new(doc, node);
    let mut matched_indices = SmallVec::<[u32; 16]>::new();
    let mut matched_prox = SmallVec::<[u32; 16]>::new();
    let mut any_scoped = false;
    for &idx in &candidate_indices {
        let rule = &rule_index.rules[idx as usize];
        if let Some(ref cqs) = rule.container_queries {
            if !matches_container_queries(cqs, active_containers, root_font_size, elem_fs_basis) {
                continue;
            }
        }
        if let Some(prox) =
            match_rule_with_scopes(doc, node, &dom_el, rule, bloom, caches, shadow_host)
        {
            matched_indices.push(idx);
            matched_prox.push(prox);
            if prox != u32::MAX {
                any_scoped = true;
            }
        }
    }

    let mut host_matched = SmallVec::<[(u32, u32); 8]>::new();
    if let Some(s_idx) = own_shadow_index {
        if s_idx.has_host_rules() {
            let mut host_cands = SmallVec::<[u32; 16]>::new();
            s_idx.collect_host_candidates(&mut host_cands);
            let self_host = Some(dom_el.opaque());
            for &idx in &host_cands {
                let rule = &s_idx.rules[idx as usize];
                if let Some(prox) =
                    match_rule_with_scopes(doc, node, &dom_el, rule, None, caches, self_host)
                {
                    host_matched.push((idx, prox));
                }
            }
        }
    }

    let inline_attr = elem
        .attributes
        .iter()
        .find(|(q, _)| q.local.eq_str_ignore_ascii_case("style"))
        .map(|(_, v)| v.as_str());
    let has_pres_hints = has_presentational_hint_attrs(doc, node, elem);

    let any_sibling_math = rule_index.has_sibling_or_random_rules
        && matched_indices.iter().any(|&idx| {
            rule_index.rules[idx as usize].declarations.iter().any(|d| {
                d.value.contains_sibling_fn()
                    || (rule_index.has_sibling_or_random_custom_props && d.value.contains_var_fn())
            })
        });
    let uses_attr = rule_index.has_attr_fn_rules
        && matched_indices.iter().any(|&idx| {
            rule_index.rules[idx as usize].declarations.iter().any(|d| {
                d.value.contains_attr_fn()
                    || (rule_index.has_attr_custom_props && d.value.contains_var_fn())
            })
        });

    let is_mathml = &*elem.name.ns == "http://www.w3.org/1998/Math/MathML";
    let is_top_layer = elem
        .pseudo_state
        .intersects(ElementState::MODAL | ElementState::POPOVER_OPEN);

    if inline_attr.is_none()
        && !has_pres_hints
        && !any_scoped
        && !has_active_containers
        && !any_sibling_math
        && !uses_attr
        && !is_mathml
        && !is_top_layer
        && host_matched.is_empty()
    {
        if let Some(cached) = memo.lookup(parent_ptr, layout_parent_flex_grid, &matched_indices) {
            if let Some(p_node) = sibling_parent {
                memo.insert_sibling(p_node, elem, Arc::clone(&cached));
            }
            return (cached, true);
        }
    }

    let pres_decls = if has_pres_hints {
        collect_presentational_hint_decls(doc, node, elem, missing_out)
    } else {
        Vec::new()
    };

    let mut inline_decls: Vec<SpecifiedDeclaration> = match inline_attr {
        Some(attr) if !attr.trim().is_empty() => parse_inline_style(attr, missing_out),
        _ => Vec::new(),
    };
    if !inline_color_exprs.is_empty() && !inline_decls.is_empty() {
        for decl in &mut inline_decls {
            if let Some(expr) = inline_color_exprs.get(&(node, decl.property)) {
                decl.value = PropertyValue::Color4(Box::new((**expr).clone()));
            }
        }
    }

    let mut matched_refs: SmallVec<[MatchedRule<'_>; 16]> = matched_indices
        .iter()
        .zip(matched_prox.iter())
        .map(|(&idx, &scope_proximity)| MatchedRule {
            rule: &rule_index.rules[idx as usize],
            scope_proximity,
            is_shadow_host_rule: false,
        })
        .collect();
    if let Some(s_idx) = own_shadow_index {
        for &(idx, scope_proximity) in &host_matched {
            matched_refs.push(MatchedRule {
                rule: &s_idx.rules[idx as usize],
                scope_proximity,
                is_shadow_host_rule: true,
            });
        }
    }

    let inline_uses_sibling = inline_decls.iter().any(|d| d.value.contains_sibling_fn());
    let (sib_idx, sib_cnt) = if any_sibling_math || inline_uses_sibling {
        element_sibling_position(doc, node)
    } else {
        (1, 1)
    };
    let mut eff_media = *media;
    if let Some((cw, ch)) = nearest_size_container(active_containers) {
        eff_media.container_width = Some(cw);
        eff_media.container_height = Some(ch);
    }
    let mut resolved_val = resolve_element_style_with_context(
        &matched_refs,
        &pres_decls,
        &inline_decls,
        parent_style.map(Arc::as_ref),
        root_font_size,
        &eff_media,
        Some(&rule_index.registered_properties),
        sib_idx,
        sib_cnt,
        usize::from(node) as u64,
        rule_index.random_doc_seed,
        &elem.attributes,
    );
    // CSS Display L3 §2.7: Blockify items whose layout parent (skipping `display: contents`)
    // is a flex or grid container.
    if layout_parent_flex_grid {
        resolved_val.display = resolved_val.display.blockify();
    }
    // CSS Display L3 / MathML Core: `display: math` / `block math` on non-MathML elements
    // computes to `inline` / `block` (unless already blockified).
    if !is_mathml {
        match resolved_val.display {
            crate::values::DisplayMode::Math => {
                resolved_val.display = crate::values::DisplayMode::Inline;
            }
            crate::values::DisplayMode::BlockMath => {
                resolved_val.display = crate::values::DisplayMode::Block;
            }
            _ => {}
        }
    }
    // CSS Position L4 / Fullscreen Top Layer: elements in the top layer compute
    // `position: static | relative | sticky` to `absolute` and blockify their `display`.
    if is_top_layer {
        if matches!(
            resolved_val.position,
            crate::values::PositionMode::Static
                | crate::values::PositionMode::Relative
                | crate::values::PositionMode::Sticky
        ) {
            resolved_val.position = crate::values::PositionMode::Absolute;
        }
        resolved_val.float_mode = crate::values::FloatMode::None;
        resolved_val.display = resolved_val.display.blockify();
    }
    let resolved = Arc::new(resolved_val);

    if inline_attr.is_none()
        && !has_pres_hints
        && !any_scoped
        && !has_active_containers
        && !any_sibling_math
        && !uses_attr
        && !is_mathml
        && !is_top_layer
        && host_matched.is_empty()
    {
        memo.insert(
            parent_ptr,
            layout_parent_flex_grid,
            &matched_indices,
            Arc::clone(&resolved),
        );
        if let Some(p_node) = sibling_parent {
            memo.insert_sibling(p_node, elem, Arc::clone(&resolved));
        }
    }
    (resolved, false)
}

fn element_sibling_position(doc: &Document, node: NodeId) -> (u32, u32) {
    let Some(parent) = doc.parent(node) else {
        return (1, 1);
    };
    let (mut idx, mut count) = (0u32, 0u32);
    for child in doc.children(parent) {
        if matches!(doc.get(child), Some(NodeData::Element(_))) {
            count = count.saturating_add(1);
            if child == node {
                idx = count;
            }
        }
    }
    if idx == 0 {
        (1, count.max(1))
    } else {
        (idx, count)
    }
}

/// Evaluates `rule` against `dom_el`, enforcing any enclosing `@scope` boundaries and
/// returning the closest scoping proximity (`0` = scoping root itself, `u32::MAX` = unscoped).
fn match_rule_with_scopes(
    doc: &Document,
    node: NodeId,
    dom_el: &DomElement<'_>,
    rule: &CompiledRule,
    bloom: Option<&BloomFilter>,
    caches: &mut SelectorCaches,
    shadow_host: Option<selectors::OpaqueElement>,
) -> Option<u32> {
    let Some(ref scopes) = rule.scopes else {
        return matches_single_selector(&rule.selector, dom_el, bloom, caches, shadow_host)
            .then_some(u32::MAX);
    };
    let mut chain = SmallVec::<[NodeId; 16]>::new();
    let mut cur = Some(node);
    while let Some(id) = cur {
        if matches!(doc.get(id), Some(NodeData::Element(_))) {
            chain.push(id);
        }
        cur = doc.parent(id);
    }
    if chain.is_empty() {
        return None;
    }
    eval_nested_scopes(
        doc,
        &chain,
        0,
        scopes,
        chain.len() - 1,
        None,
        dom_el,
        rule,
        bloom,
        caches,
        shadow_host,
    )
}

#[allow(clippy::too_many_arguments)]
fn eval_nested_scopes(
    doc: &Document,
    chain: &[NodeId],
    scope_idx: usize,
    scopes: &[CompiledScope],
    max_root_idx: usize,
    outer_scope_opaque: Option<selectors::OpaqueElement>,
    dom_el: &DomElement<'_>,
    rule: &CompiledRule,
    bloom: Option<&BloomFilter>,
    caches: &mut SelectorCaches,
    shadow_host: Option<selectors::OpaqueElement>,
) -> Option<u32> {
    let sc = &scopes[scope_idx];
    let mut best_prox: Option<u32> = None;

    for r_idx in 0..=max_root_idx {
        let root_id = chain[r_idx];
        let root_el = DomElement::new(doc, root_id);
        let is_root = if let Some(ref start_list) = sc.start {
            matches_selector_list_with_scope(
                start_list,
                &root_el,
                outer_scope_opaque,
                None,
                caches,
                shadow_host,
            )
        } else if let Some(imp) = sc.implicit_root {
            imp == root_id
        } else {
            // Constructed / adopted stylesheet without an owner `<style>` element:
            // CSS Cascade L6 §2.1 scopes implicitly to the document root element.
            r_idx == chain.len() - 1
        };
        if !is_root {
            continue;
        }
        let root_opaque = root_el.opaque();
        // CSS Cascade L6 §2.5: `<scope-end>` excludes any element on the path from
        // subject `chain[0]` up to scoping root `chain[r_idx]` inclusive.
        if let Some(ref end_list) = sc.end {
            let mut excluded = false;
            for &path_id in &chain[..=r_idx] {
                let path_el = DomElement::new(doc, path_id);
                if matches_selector_list_with_scope(
                    end_list,
                    &path_el,
                    Some(root_opaque),
                    None,
                    caches,
                    shadow_host,
                ) {
                    excluded = true;
                    break;
                }
            }
            if excluded {
                continue;
            }
        }
        let candidate_prox = if scope_idx + 1 < scopes.len() {
            eval_nested_scopes(
                doc,
                chain,
                scope_idx + 1,
                scopes,
                r_idx,
                Some(root_opaque),
                dom_el,
                rule,
                bloom,
                caches,
                shadow_host,
            )
        } else if matches_single_selector_with_scope(
            &rule.selector,
            dom_el,
            Some(root_opaque),
            bloom,
            caches,
            shadow_host,
        ) {
            Some(r_idx as u32)
        } else {
            None
        };
        if let Some(p) = candidate_prox {
            best_prox = Some(best_prox.map_or(p, |cur| cur.min(p)));
        }
    }
    best_prox
}

fn owning_html_table<'a>(doc: &'a Document, node: NodeId, tag: &str) -> Option<&'a ElementData> {
    if tag.eq_ignore_ascii_case("td") || tag.eq_ignore_ascii_case("th") {
        let tr = doc.parent(node)?;
        let tr_el = match doc.get(tr)? {
            NodeData::Element(e) if e.name.local.eq_str_ignore_ascii_case("tr") => tr,
            _ => return None,
        };
        let p = doc.parent(tr_el)?;
        match doc.get(p)? {
            NodeData::Element(e) if e.name.local.eq_str_ignore_ascii_case("table") => Some(e),
            NodeData::Element(e)
                if e.name.local.eq_str_ignore_ascii_case("tbody")
                    || e.name.local.eq_str_ignore_ascii_case("thead")
                    || e.name.local.eq_str_ignore_ascii_case("tfoot") =>
            {
                let tbl = doc.parent(p)?;
                match doc.get(tbl)? {
                    NodeData::Element(te) if te.name.local.eq_str_ignore_ascii_case("table") => {
                        Some(te)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    } else if tag.eq_ignore_ascii_case("thead")
        || tag.eq_ignore_ascii_case("tbody")
        || tag.eq_ignore_ascii_case("tfoot")
        || tag.eq_ignore_ascii_case("colgroup")
    {
        let p = doc.parent(node)?;
        match doc.get(p)? {
            NodeData::Element(te) if te.name.local.eq_str_ignore_ascii_case("table") => Some(te),
            _ => None,
        }
    } else {
        None
    }
}

fn parse_html_table_border_attr(raw: &str) -> f32 {
    let trimmed = raw.trim_start_matches(|c: char| c.is_ascii_whitespace());
    if trimmed.is_empty() {
        return 1.0;
    }
    let bytes = trimmed.as_bytes();
    let mut end = 0usize;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == 0 {
        return 1.0;
    }
    trimmed[..end].parse::<f32>().unwrap_or(1.0).max(0.0)
}

fn has_presentational_hint_attrs(doc: &Document, node: NodeId, elem: &ElementData) -> bool {
    let tag: &str = elem.name.local.as_ref();
    if doc.quirks_mode() == 2 && tag.eq_ignore_ascii_case("table") {
        return true;
    }
    if elem.attributes.iter().any(|(q, _)| {
        let k: &str = q.local.as_ref();
        k.eq_ignore_ascii_case("bgcolor")
            || k.eq_ignore_ascii_case("width")
            || k.eq_ignore_ascii_case("height")
            || k.eq_ignore_ascii_case("align")
            || k.eq_ignore_ascii_case("valign")
            || k.eq_ignore_ascii_case("cellspacing")
            || k.eq_ignore_ascii_case("cellpadding")
            || k.eq_ignore_ascii_case("border")
            || k.eq_ignore_ascii_case("rules")
            || k.eq_ignore_ascii_case("frame")
            || k.eq_ignore_ascii_case("nowrap")
            || k.eq_ignore_ascii_case("color")
    }) {
        return true;
    }
    if let Some(tbl) = owning_html_table(doc, node, tag) {
        return tbl.attributes.iter().any(|(q, _)| {
            let k: &str = q.local.as_ref();
            k.eq_ignore_ascii_case("border")
                || k.eq_ignore_ascii_case("rules")
                || k.eq_ignore_ascii_case("cellpadding")
        });
    }
    false
}

/// Translates HTML5 §15.3 presentational attributes (`bgcolor`, `width`, `height`, `align`,
/// `valign`, `cellspacing`, `cellpadding`, `border`, `rules`, `frame`, `nowrap`, `<font color>`)
/// and Quirks Mode table resets into zero-specificity CSS declarations overridden by author CSS.
fn collect_presentational_hint_decls(
    doc: &Document,
    node: NodeId,
    elem: &ElementData,
    missing_out: &mut Vec<MissingCssFeature>,
) -> Vec<SpecifiedDeclaration> {
    let tag: &str = elem.name.local.as_ref();
    let mut css = String::new();

    // WHATWG Quirks Mode §3.4 / HTML §15.3.9: in Quirks Mode (not Limited-Quirks),
    // `<table>` resets inherited font & text properties to their initial values.
    if doc.quirks_mode() == 2 && tag.eq_ignore_ascii_case("table") {
        css.push_str(
            "font-weight:initial;font-style:initial;font-size:initial;line-height:initial;white-space:initial;text-align:initial;",
        );
    }

    // HTML §15.3.9 `<table>` border / rules / frame presentational hint mapping.
    if tag.eq_ignore_ascii_case("table") {
        let border_attr = elem.get_attribute("border");
        let rules_attr = elem
            .get_attribute("rules")
            .map(|s| s.trim().to_ascii_lowercase());
        let frame_attr = elem
            .get_attribute("frame")
            .map(|s| s.trim().to_ascii_lowercase());
        let has_valid_rules = matches!(
            rules_attr.as_deref(),
            Some("none" | "groups" | "rows" | "cols" | "all")
        );
        let frame_sides: Option<(bool, bool, bool, bool)> = match frame_attr.as_deref() {
            Some("void") => Some((false, false, false, false)),
            Some("above") => Some((true, false, false, false)),
            Some("below") => Some((false, false, true, false)),
            Some("hsides") => Some((true, false, true, false)),
            Some("lhs") => Some((false, false, false, true)),
            Some("rhs") => Some((false, true, false, false)),
            Some("vsides") => Some((false, true, false, true)),
            Some("box" | "border") => Some((true, true, true, true)),
            _ => None,
        };
        let border_w = border_attr.map(parse_html_table_border_attr);
        if has_valid_rules {
            css.push_str("border-style:hidden;border-collapse:collapse;");
        }
        if let Some((t, r, b, l)) = frame_sides {
            let st = |on: bool| if on { "outset" } else { "hidden" };
            css.push_str(&format!(
                "border-top-style:{};border-right-style:{};border-bottom-style:{};border-left-style:{};",
                st(t),
                st(r),
                st(b),
                st(l)
            ));
            if border_w.is_none_or(|w| w <= 0.0) {
                let wd = |on: bool| if on { "1px" } else { "0px" };
                css.push_str(&format!(
                    "border-top-width:{};border-right-width:{};border-bottom-width:{};border-left-width:{};border-color:currentcolor;",
                    wd(t),
                    wd(r),
                    wd(b),
                    wd(l)
                ));
            }
        }
        if let Some(w) = border_w {
            if w > 0.0 {
                css.push_str(&format!("border-width:{w}px;border-color:gray;"));
                if frame_sides.is_none() {
                    css.push_str("border-style:outset;");
                }
            } else if frame_sides.is_none() {
                css.push_str("border-width:0px;");
            }
        }
    }

    // Inherited HTML table presentational hints from owning `<table>` (`border`, `rules`, `cellpadding`).
    if let Some(tbl) = owning_html_table(doc, node, tag) {
        if tag.eq_ignore_ascii_case("td") || tag.eq_ignore_ascii_case("th") {
            if let Some(cp) = tbl.get_attribute("cellpadding") {
                if let Some(dim) = normalize_html_dimension(cp.trim()) {
                    css.push_str("padding:");
                    css.push_str(&dim);
                    css.push(';');
                }
            }
            let rules_lc = tbl
                .get_attribute("rules")
                .map(|s| s.trim().to_ascii_lowercase());
            let has_nonzero_border = tbl
                .get_attribute("border")
                .is_some_and(|v| parse_html_table_border_attr(v) > 0.0);
            match rules_lc.as_deref() {
                Some("all") => {
                    css.push_str("border-width:1px;border-style:solid;border-color:inherit;");
                }
                Some("rows") => {
                    css.push_str("border-top-width:1px;border-bottom-width:1px;border-top-style:solid;border-bottom-style:solid;border-left-style:none;border-right-style:none;border-color:inherit;");
                }
                Some("cols") => {
                    css.push_str("border-left-width:1px;border-right-width:1px;border-left-style:solid;border-right-style:solid;border-top-style:none;border-bottom-style:none;border-color:inherit;");
                }
                Some("none") => {
                    css.push_str("border-style:none;");
                }
                _ => {
                    if has_nonzero_border {
                        css.push_str("border-width:1px;border-style:inset;border-color:inherit;");
                    }
                }
            }
        } else if matches!(
            tbl.get_attribute("rules")
                .map(|s| s.trim().to_ascii_lowercase())
                .as_deref(),
            Some("groups")
        ) {
            if tag.eq_ignore_ascii_case("thead")
                || tag.eq_ignore_ascii_case("tbody")
                || tag.eq_ignore_ascii_case("tfoot")
            {
                css.push_str(
                    "border-top-width:1px;border-bottom-width:1px;border-top-style:solid;border-bottom-style:solid;",
                );
            } else if tag.eq_ignore_ascii_case("colgroup") {
                css.push_str(
                    "border-left-width:1px;border-right-width:1px;border-left-style:solid;border-right-style:solid;",
                );
            }
        }
    }

    for (q, val) in &elem.attributes {
        let attr: &str = q.local.as_ref();
        if attr.eq_ignore_ascii_case("nowrap")
            && (tag.eq_ignore_ascii_case("td") || tag.eq_ignore_ascii_case("th"))
        {
            css.push_str("white-space:nowrap;");
            continue;
        }
        let trimmed = val.trim();
        if trimmed.is_empty() {
            continue;
        }
        if attr.eq_ignore_ascii_case("bgcolor")
            && (tag.eq_ignore_ascii_case("body")
                || tag.eq_ignore_ascii_case("table")
                || tag.eq_ignore_ascii_case("thead")
                || tag.eq_ignore_ascii_case("tbody")
                || tag.eq_ignore_ascii_case("tfoot")
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
                || tag.eq_ignore_ascii_case("col")
                || tag.eq_ignore_ascii_case("colgroup")
                || tag.eq_ignore_ascii_case("img")
                || tag.eq_ignore_ascii_case("svg")
                || tag.eq_ignore_ascii_case("canvas")
                || tag.eq_ignore_ascii_case("iframe")
                || tag.eq_ignore_ascii_case("hr"))
        {
            let dim = if tag.eq_ignore_ascii_case("svg") {
                normalize_svg_dimension(trimmed)
            } else {
                normalize_html_dimension(trimmed)
            };
            if let Some(dim) = dim {
                css.push_str("width:");
                css.push_str(&dim);
                css.push(';');
            }
        } else if attr.eq_ignore_ascii_case("height")
            && (tag.eq_ignore_ascii_case("table")
                || tag.eq_ignore_ascii_case("thead")
                || tag.eq_ignore_ascii_case("tbody")
                || tag.eq_ignore_ascii_case("tfoot")
                || tag.eq_ignore_ascii_case("tr")
                || tag.eq_ignore_ascii_case("td")
                || tag.eq_ignore_ascii_case("th")
                || tag.eq_ignore_ascii_case("img")
                || tag.eq_ignore_ascii_case("svg")
                || tag.eq_ignore_ascii_case("canvas")
                || tag.eq_ignore_ascii_case("iframe"))
        {
            let dim = if tag.eq_ignore_ascii_case("svg") {
                normalize_svg_dimension(trimmed)
            } else {
                normalize_html_dimension(trimmed)
            };
            if let Some(dim) = dim {
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
        } else if attr.eq_ignore_ascii_case("valign")
            && (tag.eq_ignore_ascii_case("thead")
                || tag.eq_ignore_ascii_case("tbody")
                || tag.eq_ignore_ascii_case("tfoot")
                || tag.eq_ignore_ascii_case("tr")
                || tag.eq_ignore_ascii_case("td")
                || tag.eq_ignore_ascii_case("th"))
        {
            let lower = trimmed.to_ascii_lowercase();
            if matches!(lower.as_str(), "top" | "middle" | "bottom" | "baseline") {
                css.push_str("vertical-align:");
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

/// Normalizes an SVG 2 §4.2 `width`/`height` presentation attribute (accepting unitless user
/// units, `%`, and CSS `<length>` units such as `em`, `rem`, `ex`, `ch`, `pt`, `pc`, `cm`, `mm`,
/// `in`, `vw`, `vh`) into a CSS `<length-percentage>` presentational hint.
fn normalize_svg_dimension(raw: &str) -> Option<String> {
    if let Some(basic) = normalize_html_dimension(raw) {
        return Some(basic);
    }
    let trimmed = raw.trim();
    let split = trimmed
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '+' || c == '-'))
        .unwrap_or(trimmed.len());
    let num = trimmed[..split].trim();
    if !num
        .parse::<f32>()
        .ok()
        .is_some_and(|v| v.is_finite() && v >= 0.0)
    {
        return None;
    }
    let unit = trimmed[split..].trim().to_ascii_lowercase();
    match unit.as_str() {
        "em" | "rem" | "ex" | "ch" | "pt" | "pc" | "cm" | "mm" | "in" | "q" | "vw" | "vh"
        | "vmin" | "vmax" | "svw" | "svh" | "lvw" | "lvh" | "dvw" | "dvh" => {
            Some(format!("{num}{unit}"))
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_pseudo_element_styles(
    doc: &Document,
    node: NodeId,
    originating_style: &Arc<ComputedStyle>,
    layout_parent_flex_grid: bool,
    root_font_size: f32,
    shadow_host: Option<selectors::OpaqueElement>,
    rule_index: &RuleIndex,
    media: &MediaContext,
    bloom: Option<&BloomFilter>,
    caches: &mut SelectorCaches,
    memo: &mut StyleMemo,
    out_pseudos: &mut Vec<((NodeId, RixPseudoElement), Arc<ComputedStyle>)>,
) {
    let Some(NodeData::Element(orig_elem)) = doc.get(node) else {
        return;
    };
    let elem_attrs: &[(rix_dom::QualName, String)] = &orig_elem.attributes;
    let tag: &str = orig_elem.name.local.as_ref();
    let is_input = tag.eq_ignore_ascii_case("input");
    let is_textarea = tag.eq_ignore_ascii_case("textarea");
    let is_details = tag.eq_ignore_ascii_case("details");
    let is_top_layer = orig_elem
        .pseudo_state
        .intersects(ElementState::MODAL | ElementState::POPOVER_OPEN | ElementState::FULLSCREEN);
    let is_list_item = matches!(
        originating_style.display,
        crate::values::DisplayMode::ListItem
            | crate::values::DisplayMode::InlineListItem
            | crate::values::DisplayMode::FlowRootListItem
            | crate::values::DisplayMode::InlineFlowRootListItem
    );
    let orig_ptr = Arc::as_ptr(originating_style) as usize;

    for pseudo in [
        RixPseudoElement::Before,
        RixPseudoElement::After,
        RixPseudoElement::FirstLine,
        RixPseudoElement::FirstLetter,
        RixPseudoElement::Marker,
        RixPseudoElement::Placeholder,
        RixPseudoElement::Backdrop,
        RixPseudoElement::FileSelectorButton,
        RixPseudoElement::DetailsContent,
    ] {
        // Skip pseudo-elements that cannot exist on this originating element per CSS Pseudo-Elements L4.
        match pseudo {
            RixPseudoElement::Placeholder if !is_input && !is_textarea => continue,
            RixPseudoElement::FileSelectorButton if !is_input => continue,
            RixPseudoElement::DetailsContent if !is_details => continue,
            RixPseudoElement::Backdrop if !is_top_layer => continue,
            RixPseudoElement::Marker if !is_list_item => continue,
            _ => {}
        }
        if !rule_index.has_pseudo_rules(pseudo) {
            continue;
        }
        let mut cands = SmallVec::<[u32; 16]>::new();
        rule_index.collect_pseudo_candidates(pseudo, orig_elem, bloom, &mut cands);
        if cands.is_empty() {
            continue;
        }
        let pseudo_el = DomElement::with_pseudo(doc, node, pseudo);
        let mut matched: SmallVec<[MatchedRule<'_>; 8]> = SmallVec::new();
        let mut matched_indices: SmallVec<[u32; 8]> = SmallVec::new();
        let mut any_scoped = false;
        for &idx in &cands {
            let rule = &rule_index.rules[idx as usize];
            if let Some(scope_proximity) =
                match_rule_with_scopes(doc, node, &pseudo_el, rule, bloom, caches, shadow_host)
            {
                matched.push(MatchedRule {
                    rule,
                    scope_proximity,
                    is_shadow_host_rule: false,
                });
                matched_indices.push(idx);
                if scope_proximity != u32::MAX {
                    any_scoped = true;
                }
            }
        }
        if !matched.is_empty() {
            // CSS Pseudo-Elements L4 §2.2 / CSS Display L3: `::before` and `::after` do not
            // generate boxes unless a matched rule declares `content` (or `all`). Skipping
            // universal non-generated pseudo-element resets (`*, ::before, ::after { ... }`,
            // specificity <= 1) during bulk cascade avoids 2x full-DOM style work while
            // `getComputedStyle(el, '::before')` resolves them lazily via
            // `resolve_pseudo_style_on_demand`.
            if matches!(pseudo, RixPseudoElement::Before | RixPseudoElement::After)
                && matched.iter().all(|m| m.rule.specificity <= 1)
                && !matched.iter().any(|m| {
                    m.rule.declarations.iter().any(|d| {
                        matches!(
                            d.property,
                            crate::properties::PropertyIdKind::Content
                                | crate::properties::PropertyIdKind::All
                        )
                    })
                })
            {
                continue;
            }
            let needs_sib = rule_index.has_sibling_or_random_rules
                && matched.iter().any(|m| {
                    m.rule.declarations.iter().any(|d| {
                        d.value.contains_sibling_fn()
                            || (rule_index.has_sibling_or_random_custom_props
                                && d.value.contains_var_fn())
                    })
                });
            let uses_attr = rule_index.has_attr_fn_rules
                && matched.iter().any(|m| {
                    m.rule.declarations.iter().any(|d| {
                        d.value.contains_attr_fn()
                            || (rule_index.has_attr_custom_props && d.value.contains_var_fn())
                    })
                });
            let memo_parent_ptr = orig_ptr ^ (((pseudo as usize) + 1) << 56);
            if !any_scoped && !needs_sib && !uses_attr {
                if let Some(cached) =
                    memo.lookup(memo_parent_ptr, layout_parent_flex_grid, &matched_indices)
                {
                    out_pseudos.push(((node, pseudo), cached));
                    continue;
                }
            }
            let (si, sc) = if needs_sib {
                element_sibling_position(doc, node)
            } else {
                (1, 1)
            };
            let pseudo_nid = (usize::from(node) as u64) ^ (((pseudo as u64) + 1) << 48);
            let mut pstyle = resolve_element_style_with_context(
                &matched,
                &[],
                &[],
                Some(originating_style.as_ref()),
                root_font_size,
                media,
                Some(&rule_index.registered_properties),
                si,
                sc,
                pseudo_nid,
                rule_index.random_doc_seed,
                elem_attrs,
            );
            if layout_parent_flex_grid
                && matches!(pseudo, RixPseudoElement::Before | RixPseudoElement::After)
            {
                pstyle.display = pstyle.display.blockify();
            }
            // CSS Pseudo-Elements L4: `display` does not apply to `::first-line` or `::first-letter`.
            match pseudo {
                RixPseudoElement::FirstLine => {
                    pstyle.display = crate::values::DisplayMode::Inline;
                }
                RixPseudoElement::FirstLetter => {
                    pstyle.display = if pstyle.float_mode != crate::values::FloatMode::None {
                        crate::values::DisplayMode::Block
                    } else {
                        crate::values::DisplayMode::Inline
                    };
                }
                _ => {}
            }
            // Pseudo-elements are never in the MathML namespace.
            match pstyle.display {
                crate::values::DisplayMode::Math => {
                    pstyle.display = crate::values::DisplayMode::Inline;
                }
                crate::values::DisplayMode::BlockMath => {
                    pstyle.display = crate::values::DisplayMode::Block;
                }
                _ => {}
            }
            let style = Arc::new(pstyle);
            if !any_scoped && !needs_sib && !uses_attr {
                memo.insert(
                    memo_parent_ptr,
                    layout_parent_flex_grid,
                    &matched_indices,
                    Arc::clone(&style),
                );
            }
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
    if scope.contains(InvalidationScope::EARLIER_SIBLINGS) {
        if let Some(parent) = doc.parent(node) {
            let siblings: Vec<NodeId> = doc.children(parent).collect();
            for sib in siblings {
                if sib == node {
                    break;
                }
                if matches!(doc.get(sib), Some(NodeData::Element(_))) {
                    mark_node_and_ancestors_dirty(doc, sib);
                }
            }
        }
    }
    if scope.contains(InvalidationScope::ANCESTOR_EARLIER_SIBLINGS) {
        let ancestors: Vec<NodeId> = doc.ancestors(node).collect();
        for anc in ancestors {
            if let Some(parent) = doc.parent(anc) {
                let siblings: Vec<NodeId> = doc.children(parent).collect();
                for sib in siblings {
                    if sib == anc {
                        break;
                    }
                    if matches!(doc.get(sib), Some(NodeData::Element(_))) {
                        mark_node_and_ancestors_dirty(doc, sib);
                    }
                }
            }
        }
    }
    if scope.contains(InvalidationScope::ANCESTOR_SUBTREES) {
        let ancestors: Vec<NodeId> = doc.ancestors(node).collect();
        if let Some(top_elem) = ancestors
            .into_iter()
            .rev()
            .find(|&n| matches!(doc.get(n), Some(NodeData::Element(_))))
        {
            mark_subtree_style_dirty(doc, top_elem);
        } else {
            mark_subtree_style_dirty(doc, node);
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

#[cfg(test)]
mod stage2_container_and_delta_tests {
    use super::*;
    use crate::stylesheet::{CascadeOrigin, MediaContext, Stylesheet};

    #[test]
    fn container_queries_and_cqw_resolve_against_query_container_size() {
        let mut doc = Document::new();
        let root = doc.root();
        let html = doc.create_element("html");
        let body = doc.create_element("body");
        let card = doc.create_element("div");
        if let Some(NodeData::Element(e)) = doc.get_mut(card) {
            e.set_attribute("class", "card");
        }
        let heading = doc.create_element("h1");
        if let Some(NodeData::Element(e)) = doc.get_mut(heading) {
            e.set_attribute("class", "title");
        }
        doc.append_child(root, html);
        doc.append_child(html, body);
        doc.append_child(body, card);
        doc.append_child(card, heading);

        let media = MediaContext {
            viewport_width: 1280.0,
            viewport_height: 800.0,
            ..MediaContext::default()
        };
        let css = r#"
            .card { container: promo / inline-size; }
            .title { font-size: 10cqw; color: rgb(10, 20, 30); }
            @container promo (min-width: 500px) {
                .title { color: rgb(200, 10, 10); }
            }
            @container promo (width < 500px) {
                .title { color: rgb(10, 180, 20); }
            }
        "#;
        let sheet = Arc::new(Stylesheet::parse(css, CascadeOrigin::Author, &media));
        let mut ctx = StyleContext::new(media);
        ctx.add_stylesheet(sheet);
        let _ = ctx.cascade_full(&mut doc);
        assert!(ctx.has_size_containers());
        assert!(ctx.has_container_queries());

        // Narrow container (400px): 10cqw = 40px, matches `(width < 500px)` -> green!
        let mut sizes = HashMap::new();
        sizes.insert(card, (400.0, 300.0));
        let changed = ctx.set_container_sizes(sizes);
        assert!(changed.contains(&card));
        mark_node_and_ancestors_dirty(&mut doc, heading);
        let _ = ctx.restyle_dirty(&mut doc);
        let st_narrow = ctx.style_for(heading).unwrap();
        assert!((st_narrow.text.font_size_px - 40.0).abs() < 0.2);
        assert_eq!(
            st_narrow.text.color,
            crate::values::ColorRgba::rgb(10, 180, 20)
        );

        // Wide container (600px): 10cqw = 60px, matches `(min-width: 500px)` -> red!
        let mut sizes2 = HashMap::new();
        sizes2.insert(card, (600.0, 300.0));
        let _ = ctx.set_container_sizes(sizes2);
        mark_node_and_ancestors_dirty(&mut doc, heading);
        let _ = ctx.restyle_dirty(&mut doc);
        let st_wide = ctx.style_for(heading).unwrap();
        assert!((st_wide.text.font_size_px - 60.0).abs() < 0.2);
        assert_eq!(
            st_wide.text.color,
            crate::values::ColorRgba::rgb(200, 10, 10)
        );
    }

    #[test]
    fn incremental_stylesheet_append_restyles_only_matching_nodes() {
        let mut doc = Document::new();
        let root = doc.root();
        let html = doc.create_element("html");
        let body = doc.create_element("body");
        doc.append_child(root, html);
        doc.append_child(html, body);
        let mut target_node = body;
        for i in 0..300 {
            let d = doc.create_element("div");
            if let Some(NodeData::Element(e)) = doc.get_mut(d) {
                e.set_attribute("class", if i == 42 { "target-only" } else { "other" });
            }
            doc.append_child(body, d);
            if i == 42 {
                target_node = d;
            }
        }
        let media = MediaContext::default();
        let mut ctx = StyleContext::new(media);
        let _ = ctx.cascade_full(&mut doc);

        let delta = Arc::new(Stylesheet::parse(
            ".target-only { color: rgb(255, 0, 128); }",
            CascadeOrigin::Author,
            &media,
        ));
        let rep = ctx.append_stylesheet_incremental(&mut doc, delta);
        // Only the 1 matching `.target-only` node (out of 302 elements!) should be restyled!
        assert_eq!(rep.restyled_elements, 1);
        assert_eq!(
            ctx.style_for(target_node).unwrap().text.color,
            crate::values::ColorRgba::rgb(255, 0, 128)
        );
    }

    #[test]
    fn color4_preserves_oklch_and_lab_and_ua_li_text_align_resolves_left() {
        let mut doc = Document::new();
        let root = doc.root();
        let html = doc.create_element("html");
        let body = doc.create_element("body");
        let ul = doc.create_element("ul");
        let li = doc.create_element("li");
        doc.append_child(root, html);
        doc.append_child(html, body);
        doc.append_child(body, ul);
        doc.append_child(ul, li);

        let css = r#"
            body { color: oklch(0.707 0.022 261.325); background-color: lab(88.8292 0 0); }
        "#;
        let media = MediaContext::default();
        let mut ctx = StyleContext::new(media);
        ctx.add_stylesheet(Arc::new(Stylesheet::parse(
            css,
            CascadeOrigin::Author,
            &media,
        )));
        let _ = ctx.cascade_full(&mut doc);

        let body_st = ctx.style_for(body).unwrap();
        assert!(
            body_st.text.color_computed.is_some(),
            "oklch() must retain CSS Color 4 computed value"
        );
        assert!(
            body_st.visual.background_color_computed.is_some(),
            "lab() must retain CSS Color 4 computed value"
        );
        assert!(
            body_st.visual.background_color.r > 200,
            "lab(88.8 0 0) must resolve to light gray, got {:?}",
            body_st.visual.background_color
        );
        let li_st = ctx.style_for(li).unwrap();
        assert_eq!(li_st.text.text_align, crate::values::TextAlignMode::Left);
    }
}
