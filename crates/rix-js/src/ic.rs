//! Monomorphic -> Polymorphic (4-entry) -> Megamorphic Inline Caches (IC).
//!
//! Reference: Spec 05 §3.2.
//! - **Monomorphic:** Single `obj.shape == entry.shape` check -> direct `obj.slots[offset]` access.
//! - **Polymorphic:** Linear scan over up to 4 [`IcEntry`] slots.
//! - **Megamorphic:** Shared bounded global `(ShapeId, PropertyKey) -> MegamorphicEntry` cache.
//!
//! Prototype-chain hits additionally validate `entry.proto_epoch == shapes.prototype_epoch()`
//! so mutating any prototype object invalidates all prototype-chain ICs in O(1).

use crate::shape::ShapeId;
use crate::string::PropertyKey;
use crate::value::HeapHandle;
use std::collections::HashMap;

/// Maximum distinct shapes cached per site before transitioning to Megamorphic.
pub const POLYMORPHIC_MAX_ENTRIES: usize = 4;

/// Maximum entries in the shared megamorphic fallback cache before periodic clear.
pub const MEGAMORPHIC_CACHE_LIMIT: usize = 16_384;

/// Single cached shape-to-slot mapping inside an inline cache site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IcEntry {
    /// Expected receiver object shape.
    pub shape: ShapeId,
    /// Fixed slot offset on the receiver (or on `holder` for prototype-chain loads).
    pub offset: u32,
    /// For property-adding `SetProp`/`DefineProp` transitions, the resulting new shape.
    /// Equals `shape` for in-place slot writes and loads.
    pub target_shape: ShapeId,
    /// `None` for own-property access; `Some(proto)` for prototype-chain loads/method calls.
    pub holder: Option<HeapHandle>,
    /// Prototype epoch snapshot at cache fill time (0 for own-property access).
    pub proto_epoch: u64,
    /// True if the cached property is an accessor (must slow-path invoke getter/setter).
    pub is_accessor: bool,
}

impl IcEntry {
    #[inline]
    pub const fn empty() -> Self {
        Self {
            shape: ShapeId::DICTIONARY,
            offset: 0,
            target_shape: ShapeId::DICTIONARY,
            holder: None,
            proto_epoch: 0,
            is_accessor: false,
        }
    }

    #[inline]
    pub const fn own(shape: ShapeId, offset: u32) -> Self {
        Self {
            shape,
            offset,
            target_shape: shape,
            holder: None,
            proto_epoch: 0,
            is_accessor: false,
        }
    }

    #[inline]
    pub const fn transition(from_shape: ShapeId, target_shape: ShapeId, offset: u32) -> Self {
        Self {
            shape: from_shape,
            offset,
            target_shape,
            holder: None,
            proto_epoch: 0,
            is_accessor: false,
        }
    }

    #[inline]
    pub const fn proto(
        receiver_shape: ShapeId,
        holder: HeapHandle,
        offset: u32,
        proto_epoch: u64,
    ) -> Self {
        Self {
            shape: receiver_shape,
            offset,
            target_shape: receiver_shape,
            holder: Some(holder),
            proto_epoch,
            is_accessor: false,
        }
    }

    #[inline]
    #[must_use]
    pub fn matches(&self, receiver_shape: ShapeId, current_proto_epoch: u64) -> bool {
        self.shape == receiver_shape
            && (self.holder.is_none() || self.proto_epoch == current_proto_epoch)
    }
}

/// Runtime state of a single bytecode inline cache site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IcState {
    Uninitialized,
    Monomorphic(IcEntry),
    Polymorphic {
        len: u8,
        entries: [IcEntry; POLYMORPHIC_MAX_ENTRIES],
    },
    Megamorphic,
}

impl Default for IcState {
    #[inline]
    fn default() -> Self {
        Self::Uninitialized
    }
}

/// Global megamorphic lookup entry shared across all functions in a VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MegamorphicEntry {
    pub offset: u32,
    pub target_shape: ShapeId,
    pub holder: Option<HeapHandle>,
    pub proto_epoch: u64,
}

/// Shared global `(ShapeId, PropertyKey, is_store)` cache for megamorphic sites.
#[derive(Debug, Default)]
pub struct MegamorphicCache {
    loads: HashMap<(ShapeId, PropertyKey), MegamorphicEntry>,
    stores: HashMap<(ShapeId, PropertyKey), MegamorphicEntry>,
}

impl MegamorphicCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    #[must_use]
    pub fn get_load(
        &self,
        shape: ShapeId,
        key: PropertyKey,
        proto_epoch: u64,
    ) -> Option<MegamorphicEntry> {
        let entry = *self.loads.get(&(shape, key))?;
        if entry.holder.is_some() && entry.proto_epoch != proto_epoch {
            return None;
        }
        Some(entry)
    }

    #[inline]
    #[must_use]
    pub fn get_store(&self, shape: ShapeId, key: PropertyKey) -> Option<MegamorphicEntry> {
        self.stores.get(&(shape, key)).copied()
    }

    pub fn insert_load(&mut self, shape: ShapeId, key: PropertyKey, entry: MegamorphicEntry) {
        if self.loads.len() >= MEGAMORPHIC_CACHE_LIMIT {
            self.loads.clear();
        }
        self.loads.insert((shape, key), entry);
    }

    pub fn insert_store(&mut self, shape: ShapeId, key: PropertyKey, entry: MegamorphicEntry) {
        if self.stores.len() >= MEGAMORPHIC_CACHE_LIMIT {
            self.stores.clear();
        }
        self.stores.insert((shape, key), entry);
    }

    pub fn invalidate_prototypes(&mut self) {
        self.loads.retain(|_, e| e.holder.is_none());
    }

    pub fn clear(&mut self) {
        self.loads.clear();
        self.stores.clear();
    }
}

/// Live telemetry counters for inline cache efficiency & JIT type feedback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IcStats {
    pub monomorphic_hits: u64,
    pub polymorphic_hits: u64,
    pub megamorphic_hits: u64,
    pub misses: u64,
}

impl IcStats {
    #[inline]
    #[must_use]
    pub fn total(&self) -> u64 {
        self.monomorphic_hits + self.polymorphic_hits + self.megamorphic_hits + self.misses
    }

    #[inline]
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.monomorphic_hits + self.polymorphic_hits + self.megamorphic_hits
    }

    /// Returns cache hit ratio in `[0.0, 1.0]`.
    #[inline]
    #[must_use]
    pub fn hit_rate(&self) -> f64 {
        let total = self.total();
        if total == 0 {
            1.0
        } else {
            self.hits() as f64 / total as f64
        }
    }
}

/// Per-function runtime inline cache table parallel to `FunctionBytecode::ic_slots`.
#[derive(Debug, Clone, Default)]
pub struct IcTable {
    slots: Vec<IcState>,
}

impl IcTable {
    #[must_use]
    pub fn with_len(len: usize) -> Self {
        Self {
            slots: vec![IcState::Uninitialized; len],
        }
    }

    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    #[inline]
    #[must_use]
    pub fn state(&self, ic_idx: usize) -> IcState {
        self.slots
            .get(ic_idx)
            .copied()
            .unwrap_or(IcState::Uninitialized)
    }

    /// Probes an IC site for a load/method lookup.
    #[inline]
    pub fn probe_load(
        &self,
        ic_idx: usize,
        receiver_shape: ShapeId,
        key: PropertyKey,
        proto_epoch: u64,
        mega: &MegamorphicCache,
        stats: &mut IcStats,
    ) -> Option<IcEntry> {
        if receiver_shape.is_dictionary() {
            stats.misses += 1;
            return None;
        }
        let state = *self.slots.get(ic_idx)?;
        match state {
            IcState::Monomorphic(entry) => {
                if entry.matches(receiver_shape, proto_epoch) {
                    stats.monomorphic_hits += 1;
                    return Some(entry);
                }
            }
            IcState::Polymorphic { len, entries } => {
                for entry in entries.iter().take(len as usize) {
                    if entry.matches(receiver_shape, proto_epoch) {
                        stats.polymorphic_hits += 1;
                        return Some(*entry);
                    }
                }
            }
            IcState::Megamorphic => {
                if let Some(m) = mega.get_load(receiver_shape, key, proto_epoch) {
                    stats.megamorphic_hits += 1;
                    return Some(IcEntry {
                        shape: receiver_shape,
                        offset: m.offset,
                        target_shape: m.target_shape,
                        holder: m.holder,
                        proto_epoch: m.proto_epoch,
                        is_accessor: false,
                    });
                }
            }
            IcState::Uninitialized => {}
        }
        stats.misses += 1;
        None
    }

    /// Probes an IC site for an own-property store or shape-transition store.
    #[inline]
    pub fn probe_store(
        &self,
        ic_idx: usize,
        receiver_shape: ShapeId,
        key: PropertyKey,
        mega: &MegamorphicCache,
        stats: &mut IcStats,
    ) -> Option<IcEntry> {
        if receiver_shape.is_dictionary() {
            stats.misses += 1;
            return None;
        }
        let state = *self.slots.get(ic_idx)?;
        match state {
            IcState::Monomorphic(entry) => {
                if entry.shape == receiver_shape {
                    stats.monomorphic_hits += 1;
                    return Some(entry);
                }
            }
            IcState::Polymorphic { len, entries } => {
                for entry in entries.iter().take(len as usize) {
                    if entry.shape == receiver_shape {
                        stats.polymorphic_hits += 1;
                        return Some(*entry);
                    }
                }
            }
            IcState::Megamorphic => {
                if let Some(m) = mega.get_store(receiver_shape, key) {
                    stats.megamorphic_hits += 1;
                    return Some(IcEntry {
                        shape: receiver_shape,
                        offset: m.offset,
                        target_shape: m.target_shape,
                        holder: None,
                        proto_epoch: 0,
                        is_accessor: false,
                    });
                }
            }
            IcState::Uninitialized => {}
        }
        stats.misses += 1;
        None
    }

    /// Updates an IC site after a slow-path lookup, advancing
    /// `Uninitialized -> Monomorphic -> Polymorphic(2..=4) -> Megamorphic`.
    pub fn update(
        &mut self,
        ic_idx: usize,
        key: PropertyKey,
        entry: IcEntry,
        is_store: bool,
        mega: &mut MegamorphicCache,
    ) {
        if entry.shape.is_dictionary() || entry.target_shape.is_dictionary() || entry.is_accessor {
            return;
        }
        let Some(slot) = self.slots.get_mut(ic_idx) else {
            return;
        };
        match *slot {
            IcState::Uninitialized => {
                *slot = IcState::Monomorphic(entry);
            }
            IcState::Monomorphic(existing) => {
                if existing.shape == entry.shape {
                    // Refresh stale prototype-epoch entry in-place without widening to polymorphic.
                    *slot = IcState::Monomorphic(entry);
                } else {
                    let mut entries = [IcEntry::empty(); POLYMORPHIC_MAX_ENTRIES];
                    entries[0] = existing;
                    entries[1] = entry;
                    *slot = IcState::Polymorphic { len: 2, entries };
                }
            }
            IcState::Polymorphic {
                mut len,
                mut entries,
            } => {
                // Refresh existing shape entry in-place if present (e.g. after proto invalidation).
                for existing in entries.iter_mut().take(len as usize) {
                    if existing.shape == entry.shape {
                        *existing = entry;
                        *slot = IcState::Polymorphic { len, entries };
                        return;
                    }
                }
                if (len as usize) < POLYMORPHIC_MAX_ENTRIES {
                    entries[len as usize] = entry;
                    len += 1;
                    *slot = IcState::Polymorphic { len, entries };
                } else {
                    // Promote all 4 polymorphic entries + new entry into the global megamorphic cache.
                    for e in entries.iter().copied().chain(std::iter::once(entry)) {
                        let m = MegamorphicEntry {
                            offset: e.offset,
                            target_shape: e.target_shape,
                            holder: e.holder,
                            proto_epoch: e.proto_epoch,
                        };
                        if is_store {
                            mega.insert_store(e.shape, key, m);
                        } else {
                            mega.insert_load(e.shape, key, m);
                        }
                    }
                    *slot = IcState::Megamorphic;
                }
            }
            IcState::Megamorphic => {
                let m = MegamorphicEntry {
                    offset: entry.offset,
                    target_shape: entry.target_shape,
                    holder: entry.holder,
                    proto_epoch: entry.proto_epoch,
                };
                if is_store {
                    mega.insert_store(entry.shape, key, m);
                } else {
                    mega.insert_load(entry.shape, key, m);
                }
            }
        }
    }
}
