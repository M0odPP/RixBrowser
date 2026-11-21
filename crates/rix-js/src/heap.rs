//! Safe generational arena heap, tri-color incremental mark-sweep GC,
//! write barriers, root stack discipline, and leak-free weak handle tables
//! (`WeakHeapHandle` / `WeakTable`) ready for DOM wrappers.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::Instant;

use smallvec::SmallVec;

use crate::bigint::JsBigInt;
use crate::string::{JsString, PropertyKey, StringInterner};
use crate::value::{HeapHandle, JsValue, SymbolId};

/// Number of minor GC survivals before a Young object is promoted to Old generation.
const PROMOTE_AGE: u8 = 2;

/// Default nursery object budget before automatic minor collection.
pub const DEFAULT_NURSERY_LIMIT: usize = 8_192;

/// Default old-generation object growth threshold before major collection.
pub const DEFAULT_OLD_GC_THRESHOLD: usize = 32_768;

/// Generational residence of a heap allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generation {
    Young,
    Old,
}

/// Tri-color incremental marking phase for major GC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MajorGcPhase {
    Idle,
    Marking,
}

/// Mutable JS object backing storage inside the GC arena.
#[derive(Debug, Clone)]
pub struct JsObject {
    pub shape_id: u32,
    pub prototype: Option<HeapHandle>,
    pub slots: Vec<JsValue>,
    pub elements: Vec<JsValue>,
    pub extra_refs: SmallVec<[HeapHandle; 2]>,
    pub host_tag: Option<u64>,
    pub extensible: bool,
}

impl Default for JsObject {
    fn default() -> Self {
        Self {
            shape_id: 0,
            prototype: None,
            slots: Vec::new(),
            elements: Vec::new(),
            extra_refs: SmallVec::new(),
            host_tag: None,
            extensible: true,
        }
    }
}

impl JsObject {
    #[must_use]
    pub fn with_shape(shape_id: u32, prototype: Option<HeapHandle>) -> Self {
        Self {
            shape_id,
            prototype,
            ..Self::default()
        }
    }

    pub fn trace_children(&self, mut visit: impl FnMut(HeapHandle)) {
        if let Some(proto) = self.prototype {
            visit(proto);
        }
        for &val in &self.slots {
            if let Some(h) = val.as_heap_handle() {
                visit(h);
            }
        }
        for &val in &self.elements {
            if let Some(h) = val.as_heap_handle() {
                visit(h);
            }
        }
        for &extra in &self.extra_refs {
            visit(extra);
        }
    }
}

/// Payload stored inside a GC arena slot.
#[derive(Debug, Clone)]
pub enum HeapCell {
    String(JsString),
    BigInt(JsBigInt),
    Object(JsObject),
}

impl HeapCell {
    pub fn trace_children(&self, visit: impl FnMut(HeapHandle)) {
        match self {
            Self::String(s) => s.trace_children(visit),
            Self::Object(o) => o.trace_children(visit),
            Self::BigInt(_) => {}
        }
    }
}

struct ArenaSlot {
    generation: u16,
    age: u8,
    gen_space: Generation,
    remembered: bool,
    mark_epoch: u32,
    cell: Option<HeapCell>,
}

/// Weak non-owning reference into [`GcHeap`] validated via generational index.
///
/// Used by DOM wrapper tables (`WeakMap<NodeId, WeakHeapHandle>`): when `pinned_by_host`
/// is `false`, a detached unreachable DOM wrapper is automatically collected by GC;
/// when `pinned_by_host` is `true` (e.g., active JS event listener or expando property),
/// the GC traces it as a root until unpinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WeakHeapHandle {
    pub handle: HeapHandle,
    pub pinned_by_host: bool,
}

impl WeakHeapHandle {
    #[must_use]
    pub const fn new(handle: HeapHandle) -> Self {
        Self {
            handle,
            pinned_by_host: false,
        }
    }

    #[must_use]
    pub const fn pinned(handle: HeapHandle) -> Self {
        Self {
            handle,
            pinned_by_host: true,
        }
    }
}

/// Weak association table mapping host keys (such as DOM `NodeId`s) to JS heap objects
/// without leaking unreachable detached wrappers.
#[derive(Debug)]
pub struct WeakTable<K: Eq + Hash> {
    entries: HashMap<K, WeakHeapHandle>,
}

impl<K: Eq + Hash> Default for WeakTable<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash> WeakTable<K> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub fn insert(&mut self, key: K, handle: HeapHandle, pinned_by_host: bool) {
        self.entries.insert(
            key,
            WeakHeapHandle {
                handle,
                pinned_by_host,
            },
        );
    }

    pub fn set_pinned(&mut self, key: &K, pinned: bool) -> bool {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.pinned_by_host = pinned;
            true
        } else {
            false
        }
    }

    #[must_use]
    pub fn get_live(&self, key: &K, heap: &GcHeap) -> Option<HeapHandle> {
        let entry = self.entries.get(key)?;
        heap.is_alive(entry.handle).then_some(entry.handle)
    }

    /// Collects all host-pinned wrapper handles as GC roots.
    pub fn collect_pinned_roots(&self, out: &mut Vec<JsValue>) {
        for entry in self.entries.values() {
            if entry.pinned_by_host {
                out.push(JsValue::from_object(entry.handle));
            }
        }
    }

    /// Sweeps entries whose JS heap objects have been reclaimed by GC.
    pub fn sweep_dead(&mut self, heap: &GcHeap) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, entry| heap.is_alive(entry.handle));
        before - self.entries.len()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Explicit root stack preserving temporary [`JsValue`] roots across allocations.
#[derive(Debug, Default)]
pub struct RootStack {
    roots: Vec<JsValue>,
}

impl RootStack {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn push(&mut self, val: JsValue) -> usize {
        let idx = self.roots.len();
        self.roots.push(val);
        idx
    }

    #[inline]
    pub fn truncate(&mut self, mark: usize) {
        self.roots.truncate(mark);
    }

    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.roots.len()
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    #[inline]
    #[must_use]
    pub fn as_slice(&self) -> &[JsValue] {
        &self.roots
    }
}

/// Metrics returned by a minor or major GC cycle.
#[derive(Debug, Clone, Copy, Default)]
pub struct GcCycleStats {
    pub is_major: bool,
    pub reclaimed: usize,
    pub promoted: usize,
    pub live_after: usize,
    pub pause_us: u64,
}

/// Safe generational mark-sweep arena heap (`#![deny(unsafe_code)]`).
pub struct GcHeap {
    slots: Vec<ArenaSlot>,
    free_list: Vec<u32>,
    nursery: Vec<u32>,
    remembered_set: Vec<u32>,
    gray_worklist: Vec<u32>,
    interner: StringInterner,
    interned_string_handles: HashMap<PropertyKey, HeapHandle>,
    symbols: Vec<Option<String>>,
    mark_epoch: u32,
    major_phase: MajorGcPhase,
    major_epoch: u32,
    live_young: usize,
    live_old: usize,
    nursery_limit: usize,
    old_gc_threshold: usize,
    minor_collections: u64,
    major_collections: u64,
    total_allocated: u64,
}

impl Default for GcHeap {
    fn default() -> Self {
        Self::new()
    }
}

impl GcHeap {
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_NURSERY_LIMIT, DEFAULT_OLD_GC_THRESHOLD)
    }

    #[must_use]
    pub fn with_limits(nursery_limit: usize, old_gc_threshold: usize) -> Self {
        Self {
            slots: Vec::with_capacity(nursery_limit.min(16_384)),
            free_list: Vec::new(),
            nursery: Vec::with_capacity(nursery_limit.min(16_384)),
            remembered_set: Vec::new(),
            gray_worklist: Vec::new(),
            interner: StringInterner::new(),
            interned_string_handles: HashMap::new(),
            symbols: Vec::new(),
            mark_epoch: 1,
            major_phase: MajorGcPhase::Idle,
            major_epoch: 0,
            live_young: 0,
            live_old: 0,
            nursery_limit: nursery_limit.max(64),
            old_gc_threshold: old_gc_threshold.max(256),
            minor_collections: 0,
            major_collections: 0,
            total_allocated: 0,
        }
    }

    #[must_use]
    pub fn live_count(&self) -> usize {
        self.live_young + self.live_old
    }

    #[must_use]
    pub fn live_young(&self) -> usize {
        self.live_young
    }

    #[must_use]
    pub fn live_old(&self) -> usize {
        self.live_old
    }

    #[must_use]
    pub fn minor_collections(&self) -> u64 {
        self.minor_collections
    }

    #[must_use]
    pub fn major_collections(&self) -> u64 {
        self.major_collections
    }

    #[must_use]
    pub fn total_allocated(&self) -> u64 {
        self.total_allocated
    }

    #[must_use]
    pub fn major_phase(&self) -> MajorGcPhase {
        self.major_phase
    }

    #[must_use]
    pub fn should_collect_minor(&self) -> bool {
        self.nursery.len() >= self.nursery_limit
    }

    #[must_use]
    pub fn should_collect_major(&self) -> bool {
        self.live_old >= self.old_gc_threshold
    }

    pub fn intern_key(&mut self, s: &str) -> PropertyKey {
        self.interner.intern(s)
    }

    #[must_use]
    pub fn resolve_key(&self, key: PropertyKey) -> Option<&str> {
        self.interner.get(key).map(|a| a.as_ref())
    }

    pub fn alloc_symbol(&mut self, description: Option<String>) -> SymbolId {
        let id = SymbolId(self.symbols.len() as u32);
        self.symbols.push(description);
        id
    }

    #[must_use]
    pub fn symbol_description(&self, id: SymbolId) -> Option<&str> {
        self.symbols.get(id.0 as usize).and_then(|d| d.as_deref())
    }

    fn alloc_cell(&mut self, cell: HeapCell, gen_space: Generation) -> HeapHandle {
        self.total_allocated = self.total_allocated.wrapping_add(1);
        let mark_epoch = if self.major_phase == MajorGcPhase::Marking {
            // Allocate-black during active incremental major marking so fresh objects survive.
            self.major_epoch
        } else {
            0
        };
        let (index, generation) = if let Some(idx) = self.free_list.pop() {
            let slot = &mut self.slots[idx as usize];
            slot.age = 0;
            slot.gen_space = gen_space;
            slot.remembered = false;
            slot.mark_epoch = mark_epoch;
            slot.cell = Some(cell);
            (idx, slot.generation)
        } else {
            let idx = self.slots.len() as u32;
            self.slots.push(ArenaSlot {
                generation: 1,
                age: 0,
                gen_space,
                remembered: false,
                mark_epoch,
                cell: Some(cell),
            });
            (idx, 1)
        };
        match gen_space {
            Generation::Young => {
                self.nursery.push(index);
                self.live_young += 1;
            }
            Generation::Old => {
                self.live_old += 1;
            }
        }
        HeapHandle::new(index, generation)
    }

    pub fn alloc_object(&mut self, obj: JsObject) -> HeapHandle {
        self.alloc_cell(HeapCell::Object(obj), Generation::Young)
    }

    pub fn alloc_empty_object(&mut self) -> HeapHandle {
        self.alloc_object(JsObject::default())
    }

    pub fn alloc_string(&mut self, text: impl Into<String>) -> HeapHandle {
        self.alloc_cell(
            HeapCell::String(JsString::from_owned(text.into())),
            Generation::Young,
        )
    }

    /// Allocates or retrieves an Old-generation interned string handle for property keys.
    pub fn alloc_interned_string(&mut self, text: &str) -> HeapHandle {
        let key = self.interner.intern(text);
        if let Some(&h) = self.interned_string_handles.get(&key) {
            if self.is_alive(h) {
                return h;
            }
        }
        let arc = self
            .interner
            .get(key)
            .cloned()
            .expect("key was just interned");
        let h = self.alloc_cell(
            HeapCell::String(JsString::from_interned(arc, key)),
            Generation::Old,
        );
        self.interned_string_handles.insert(key, h);
        h
    }

    /// Allocates an O(1) rope string referencing `left` and `right` heap string handles.
    pub fn alloc_rope(&mut self, left: HeapHandle, right: HeapHandle) -> Option<HeapHandle> {
        let (lb, lu) = {
            let s = self.get_string(left)?;
            (s.byte_len(), s.utf16_len())
        };
        let (rb, ru) = {
            let s = self.get_string(right)?;
            (s.byte_len(), s.utf16_len())
        };
        Some(self.alloc_cell(
            HeapCell::String(JsString::Rope {
                left,
                right,
                byte_len: lb + rb,
                utf16_len: lu + ru,
            }),
            Generation::Young,
        ))
    }

    /// Flattens any [`JsString`] (including ropes and slices) into an owned Rust `String`.
    #[must_use]
    pub fn materialize_string(&self, handle: HeapHandle) -> Option<String> {
        let s = self.get_string(handle)?;
        let mut out = String::with_capacity(s.byte_len());
        self.append_string_into(handle, &mut out)?;
        Some(out)
    }

    fn append_string_into(&self, handle: HeapHandle, out: &mut String) -> Option<()> {
        match self.get_string(handle)? {
            JsString::Flat { data, .. } => {
                out.push_str(data);
                Some(())
            }
            JsString::Interned { data, .. } => {
                out.push_str(data);
                Some(())
            }
            JsString::Rope { left, right, .. } => {
                self.append_string_into(*left, out)?;
                self.append_string_into(*right, out)
            }
            JsString::Slice {
                parent,
                byte_start,
                byte_end,
                ..
            } => {
                let flat = self.materialize_string(*parent)?;
                let slice = flat.get(*byte_start..*byte_end)?;
                out.push_str(slice);
                Some(())
            }
        }
    }

    pub fn alloc_bigint(&mut self, value: JsBigInt) -> HeapHandle {
        self.alloc_cell(HeapCell::BigInt(value), Generation::Young)
    }

    #[inline]
    #[must_use]
    pub fn is_alive(&self, handle: HeapHandle) -> bool {
        self.slots
            .get(handle.index as usize)
            .is_some_and(|s| s.cell.is_some() && s.generation == handle.generation)
    }

    #[inline]
    #[must_use]
    pub fn generation_of(&self, handle: HeapHandle) -> Option<Generation> {
        let slot = self.slots.get(handle.index as usize)?;
        if slot.cell.is_some() && slot.generation == handle.generation {
            Some(slot.gen_space)
        } else {
            None
        }
    }

    #[must_use]
    pub fn get_object(&self, handle: HeapHandle) -> Option<&JsObject> {
        let slot = self.slots.get(handle.index as usize)?;
        if slot.generation != handle.generation {
            return None;
        }
        match slot.cell.as_ref()? {
            HeapCell::Object(obj) => Some(obj),
            _ => None,
        }
    }

    pub fn get_object_mut(&mut self, handle: HeapHandle) -> Option<&mut JsObject> {
        let slot = self.slots.get_mut(handle.index as usize)?;
        if slot.generation != handle.generation {
            return None;
        }
        match slot.cell.as_mut()? {
            HeapCell::Object(obj) => Some(obj),
            _ => None,
        }
    }

    #[must_use]
    pub fn get_string(&self, handle: HeapHandle) -> Option<&JsString> {
        let slot = self.slots.get(handle.index as usize)?;
        if slot.generation != handle.generation {
            return None;
        }
        match slot.cell.as_ref()? {
            HeapCell::String(s) => Some(s),
            _ => None,
        }
    }

    #[must_use]
    pub fn get_bigint(&self, handle: HeapHandle) -> Option<&JsBigInt> {
        let slot = self.slots.get(handle.index as usize)?;
        if slot.generation != handle.generation {
            return None;
        }
        match slot.cell.as_ref()? {
            HeapCell::BigInt(b) => Some(b),
            _ => None,
        }
    }

    /// Generational & incremental tri-color write barrier.
    ///
    /// MUST be invoked whenever a field of `owner` is mutated to reference `value`.
    pub fn write_barrier(&mut self, owner: HeapHandle, value: JsValue) {
        let Some(target) = value.as_heap_handle() else {
            return;
        };
        let owner_is_old = self.slots.get(owner.index as usize).is_some_and(|s| {
            s.cell.is_some() && s.generation == owner.generation && s.gen_space == Generation::Old
        });
        let target_is_young = self.slots.get(target.index as usize).is_some_and(|s| {
            s.cell.is_some()
                && s.generation == target.generation
                && s.gen_space == Generation::Young
        });

        if owner_is_old && target_is_young {
            let slot = &mut self.slots[owner.index as usize];
            if !slot.remembered {
                slot.remembered = true;
                self.remembered_set.push(owner.index);
            }
        }

        // Dijkstra incremental marking write barrier: shade unmarked target gray when marking.
        if self.major_phase == MajorGcPhase::Marking {
            let epoch = self.major_epoch;
            if let Some(t_slot) = self.slots.get_mut(target.index as usize) {
                if t_slot.cell.is_some()
                    && t_slot.generation == target.generation
                    && t_slot.mark_epoch != epoch
                {
                    t_slot.mark_epoch = epoch;
                    self.gray_worklist.push(target.index);
                }
            }
        }
    }

    /// Writes `value` into `owner.slots[slot_idx]` and invokes [`Self::write_barrier`].
    pub fn object_set_slot(&mut self, owner: HeapHandle, slot_idx: usize, value: JsValue) -> bool {
        let Some(obj) = self.get_object_mut(owner) else {
            return false;
        };
        if slot_idx >= obj.slots.len() {
            obj.slots.resize(slot_idx + 1, JsValue::UNDEFINED);
        }
        obj.slots[slot_idx] = value;
        self.write_barrier(owner, value);
        true
    }

    /// Pushes `value` onto `owner.elements` and invokes [`Self::write_barrier`].
    pub fn object_push_element(&mut self, owner: HeapHandle, value: JsValue) -> bool {
        let Some(obj) = self.get_object_mut(owner) else {
            return false;
        };
        obj.elements.push(value);
        self.write_barrier(owner, value);
        true
    }

    /// Runs a fast generational **Minor GC** strictly over the Young nursery + remembered set.
    pub fn collect_minor(&mut self, roots: &[JsValue]) -> GcCycleStats {
        let start = Instant::now();
        self.mark_epoch = self.mark_epoch.wrapping_add(1).max(1);
        let epoch = self.mark_epoch;
        let mut worklist: Vec<u32> = Vec::with_capacity(64);

        // 1. Enqueue young objects directly reachable from roots.
        for &val in roots {
            if let Some(h) = val.as_heap_handle() {
                self.mark_if_young(h, epoch, &mut worklist);
            }
        }

        // 2. Scan remembered Old->Young objects and prune entries that no longer point to Young.
        let mut kept_remembered = Vec::with_capacity(self.remembered_set.len());
        let remembered = std::mem::take(&mut self.remembered_set);
        let mut scratch: Vec<HeapHandle> = Vec::new();
        for old_idx in remembered {
            let is_live_old = self
                .slots
                .get(old_idx as usize)
                .is_some_and(|s| s.cell.is_some() && s.gen_space == Generation::Old);
            if !is_live_old {
                continue;
            }
            scratch.clear();
            if let Some(cell) = &self.slots[old_idx as usize].cell {
                cell.trace_children(|h| scratch.push(h));
            }
            let mut still_points_young = false;
            for &child in &scratch {
                if self.mark_if_young(child, epoch, &mut worklist) {
                    still_points_young = true;
                }
            }
            if still_points_young {
                kept_remembered.push(old_idx);
            } else {
                self.slots[old_idx as usize].remembered = false;
            }
        }

        // 3. Trace transitive Young survivors.
        while let Some(idx) = worklist.pop() {
            scratch.clear();
            if let Some(cell) = &self.slots[idx as usize].cell {
                cell.trace_children(|h| scratch.push(h));
            }
            for &child in &scratch {
                self.mark_if_young(child, epoch, &mut worklist);
            }
        }

        // 4. Sweep ONLY the nursery: promote aged survivors to Old, reclaim dead Young slots.
        let nursery = std::mem::take(&mut self.nursery);
        let mut next_nursery = Vec::with_capacity(nursery.len() / 4);
        let mut reclaimed = 0usize;
        let mut promoted = 0usize;

        for idx in nursery {
            let slot = &mut self.slots[idx as usize];
            if slot.cell.is_none() || slot.gen_space != Generation::Young {
                continue;
            }
            if slot.mark_epoch == epoch {
                slot.age = slot.age.saturating_add(1);
                if slot.age >= PROMOTE_AGE {
                    slot.gen_space = Generation::Old;
                    self.live_young = self.live_young.saturating_sub(1);
                    self.live_old += 1;
                    promoted += 1;
                    // A newly promoted Old object might still reference younger survivors -> remember it!
                    slot.remembered = true;
                    kept_remembered.push(idx);
                } else {
                    next_nursery.push(idx);
                }
            } else {
                slot.cell = None;
                slot.generation = slot.generation.wrapping_add(1).max(1);
                slot.age = 0;
                slot.remembered = false;
                self.free_list.push(idx);
                self.live_young = self.live_young.saturating_sub(1);
                reclaimed += 1;
            }
        }

        self.nursery = next_nursery;
        self.remembered_set = kept_remembered;
        self.minor_collections += 1;

        GcCycleStats {
            is_major: false,
            reclaimed,
            promoted,
            live_after: self.live_count(),
            pause_us: start.elapsed().as_micros() as u64,
        }
    }

    fn mark_if_young(&mut self, h: HeapHandle, epoch: u32, worklist: &mut Vec<u32>) -> bool {
        let Some(slot) = self.slots.get_mut(h.index as usize) else {
            return false;
        };
        if slot.cell.is_none()
            || slot.generation != h.generation
            || slot.gen_space != Generation::Young
        {
            return false;
        }
        if slot.mark_epoch != epoch {
            slot.mark_epoch = epoch;
            worklist.push(h.index);
        }
        true
    }

    /// Starts an incremental tri-color **Major GC** marking cycle from `roots`.
    pub fn start_major_marking(&mut self, roots: &[JsValue]) {
        self.mark_epoch = self.mark_epoch.wrapping_add(1).max(1);
        self.major_epoch = self.mark_epoch;
        self.major_phase = MajorGcPhase::Marking;
        self.gray_worklist.clear();

        let epoch = self.major_epoch;
        // Interned property strings are always pinned as VM roots.
        let interned: Vec<HeapHandle> = self.interned_string_handles.values().copied().collect();
        for h in interned {
            self.mark_any(h, epoch);
        }
        for &val in roots {
            if let Some(h) = val.as_heap_handle() {
                self.mark_any(h, epoch);
            }
        }
    }

    /// Advances incremental tri-color major marking by up to `work_budget` objects.
    /// Returns `true` when marking is complete and ready for [`Self::finish_major_sweep`].
    pub fn step_major_marking(&mut self, work_budget: usize) -> bool {
        if self.major_phase != MajorGcPhase::Marking {
            return true;
        }
        let epoch = self.major_epoch;
        let mut scratch = Vec::new();
        let mut steps = 0usize;
        while steps < work_budget {
            let Some(idx) = self.gray_worklist.pop() else {
                return true;
            };
            scratch.clear();
            if let Some(cell) = &self.slots[idx as usize].cell {
                cell.trace_children(|h| scratch.push(h));
            }
            for &child in &scratch {
                self.mark_any(child, epoch);
            }
            steps += 1;
        }
        self.gray_worklist.is_empty()
    }

    /// Completes an active incremental major mark cycle by draining remaining gray work and sweeping.
    pub fn finish_major_sweep(&mut self, extra_roots: &[JsValue]) -> GcCycleStats {
        let start = Instant::now();
        if self.major_phase != MajorGcPhase::Marking {
            return self.collect_major(extra_roots);
        }
        let epoch = self.major_epoch;
        for &val in extra_roots {
            if let Some(h) = val.as_heap_handle() {
                self.mark_any(h, epoch);
            }
        }
        while !self.step_major_marking(usize::MAX) {}

        let mut reclaimed = 0usize;
        let mut promoted = 0usize;
        let mut next_nursery = Vec::new();
        self.remembered_set.clear();

        for idx in 0..self.slots.len() {
            let slot = &mut self.slots[idx];
            if slot.cell.is_none() {
                continue;
            }
            if slot.mark_epoch == epoch {
                slot.remembered = false;
                if slot.gen_space == Generation::Young {
                    slot.gen_space = Generation::Old;
                    self.live_young = self.live_young.saturating_sub(1);
                    self.live_old += 1;
                    promoted += 1;
                }
            } else {
                let was_young = slot.gen_space == Generation::Young;
                slot.cell = None;
                slot.generation = slot.generation.wrapping_add(1).max(1);
                slot.age = 0;
                slot.remembered = false;
                self.free_list.push(idx as u32);
                if was_young {
                    self.live_young = self.live_young.saturating_sub(1);
                } else {
                    self.live_old = self.live_old.saturating_sub(1);
                }
                reclaimed += 1;
            }
        }
        next_nursery.clear();
        self.nursery = next_nursery;
        self.major_phase = MajorGcPhase::Idle;
        self.major_collections += 1;
        self.old_gc_threshold = (self.live_old * 2).max(DEFAULT_OLD_GC_THRESHOLD);

        GcCycleStats {
            is_major: true,
            reclaimed,
            promoted,
            live_after: self.live_count(),
            pause_us: start.elapsed().as_micros() as u64,
        }
    }

    /// Runs a full stop-the-world tri-color **Major GC** (reclaims cyclic Old+Young graphs).
    pub fn collect_major(&mut self, roots: &[JsValue]) -> GcCycleStats {
        let start = Instant::now();
        self.start_major_marking(roots);
        let mut stats = self.finish_major_sweep(&[]);
        stats.pause_us = start.elapsed().as_micros() as u64;
        stats
    }

    fn mark_any(&mut self, h: HeapHandle, epoch: u32) {
        let Some(slot) = self.slots.get_mut(h.index as usize) else {
            return;
        };
        if slot.cell.is_none() || slot.generation != h.generation || slot.mark_epoch == epoch {
            return;
        }
        slot.mark_epoch = epoch;
        self.gray_worklist.push(h.index);
    }

    /// Runs automatic minor/major GC if nursery or old-generation thresholds are exceeded.
    pub fn maybe_collect(&mut self, roots: &[JsValue]) -> Option<GcCycleStats> {
        if self.should_collect_minor() {
            let stats = self.collect_minor(roots);
            if self.should_collect_major() {
                return Some(self.collect_major(roots));
            }
            return Some(stats);
        }
        if self.should_collect_major() {
            return Some(self.collect_major(roots));
        }
        None
    }
}
