//! Typed dense and sparse indexed element storage for JavaScript Arrays and objects.
//!
//! Follows the V8/SpiderMonkey/JSC elements-kind hierarchy:
//! `PackedInt32` -> `PackedFloat64` -> `PackedValues` -> `SparseDict`.
//! Arrays of integers pay zero GC tracing overhead and 4 bytes per element,
//! widening automatically on floats, objects, holes, or huge sparse gaps.

use crate::value::{HeapHandle, JsValue, JsValueKind};
use std::collections::HashMap;

/// Maximum empty gap allowed before transitioning a dense array into `SparseDict`.
pub const SPARSE_GAP_THRESHOLD: usize = 1024;

/// Discriminant for inspecting an object's current elements storage kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementsKind {
    PackedInt32,
    PackedFloat64,
    PackedValues,
    SparseDict,
}

/// Out-of-line sparse backing for arrays with large gaps.
#[derive(Debug, Clone)]
pub struct SparseElements {
    pub len: u32,
    pub entries: HashMap<u32, JsValue>,
}

/// Typed storage for numeric indexed properties (`0..u32::MAX - 1`).
#[derive(Debug, Clone)]
pub enum ElementsStorage {
    PackedInt32(Vec<i32>),
    PackedFloat64(Vec<f64>),
    PackedValues(Vec<JsValue>),
    SparseDict(Box<SparseElements>),
}

impl Default for ElementsStorage {
    #[inline]
    fn default() -> Self {
        Self::PackedInt32(Vec::new())
    }
}

impl ElementsStorage {
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    #[must_use]
    pub fn kind(&self) -> ElementsKind {
        match self {
            Self::PackedInt32(_) => ElementsKind::PackedInt32,
            Self::PackedFloat64(_) => ElementsKind::PackedFloat64,
            Self::PackedValues(_) => ElementsKind::PackedValues,
            Self::SparseDict(_) => ElementsKind::SparseDict,
        }
    }

    #[inline]
    #[must_use]
    pub fn len(&self) -> u32 {
        match self {
            Self::PackedInt32(v) => v.len() as u32,
            Self::PackedFloat64(v) => v.len() as u32,
            Self::PackedValues(v) => v.len() as u32,
            Self::SparseDict(s) => s.len,
        }
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reads the element at `index`, returning `None` for out-of-bounds or holes.
    #[inline]
    #[must_use]
    pub fn get(&self, index: u32) -> Option<JsValue> {
        let idx = index as usize;
        match self {
            Self::PackedInt32(v) => v.get(idx).map(|&i| JsValue::from_i32(i)),
            Self::PackedFloat64(v) => v.get(idx).map(|&f| JsValue::from_f64(f)),
            Self::PackedValues(v) => {
                let val = *v.get(idx)?;
                (!val.is_empty()).then_some(val)
            }
            Self::SparseDict(s) => s.entries.get(&index).copied(),
        }
    }

    #[inline]
    #[must_use]
    pub fn has(&self, index: u32) -> bool {
        self.get(index).is_some()
    }

    /// Appends `value` to the end of the array, widening storage only when required.
    pub fn push(&mut self, value: JsValue) {
        let idx = self.len();
        self.set(idx, value);
    }

    /// Appends a sparse hole (increments `length` without storing a value).
    pub fn push_hole(&mut self) {
        match self {
            Self::PackedInt32(_) | Self::PackedFloat64(_) => {
                self.widen_to_values();
                if let Self::PackedValues(v) = self {
                    v.push(JsValue::EMPTY);
                }
            }
            Self::PackedValues(v) => v.push(JsValue::EMPTY),
            Self::SparseDict(s) => {
                s.len = s.len.saturating_add(1);
            }
        }
    }

    /// Sets element `index` to `value`, updating `length` if `index >= len` and
    /// widening representation if the type or sparsity requires it.
    pub fn set(&mut self, index: u32, value: JsValue) {
        let idx = index as usize;
        // Guard against huge sparse allocations before touching dense vectors.
        if !matches!(self, Self::SparseDict(_))
            && idx > (self.len() as usize).saturating_add(SPARSE_GAP_THRESHOLD)
        {
            self.widen_to_sparse();
        }

        match self {
            Self::PackedInt32(v) => {
                if let JsValueKind::Int32(i) = value.kind() {
                    if idx == v.len() {
                        v.push(i);
                        return;
                    } else if idx < v.len() {
                        v[idx] = i;
                        return;
                    }
                } else if let JsValueKind::Float(f) = value.kind() {
                    if idx <= v.len() {
                        self.widen_to_float64();
                        self.set(index, JsValue::from_f64(f));
                        return;
                    }
                }
                self.widen_to_values();
                self.set(index, value);
            }
            Self::PackedFloat64(v) => {
                let as_f64 = match value.kind() {
                    JsValueKind::Int32(i) => Some(f64::from(i)),
                    JsValueKind::Float(f) => Some(f),
                    _ => None,
                };
                if let Some(f) = as_f64 {
                    if idx == v.len() {
                        v.push(f);
                        return;
                    } else if idx < v.len() {
                        v[idx] = f;
                        return;
                    }
                }
                self.widen_to_values();
                self.set(index, value);
            }
            Self::PackedValues(v) => {
                if idx >= v.len() {
                    v.resize(idx + 1, JsValue::EMPTY);
                }
                v[idx] = value;
            }
            Self::SparseDict(s) => {
                if value.is_empty() {
                    s.entries.remove(&index);
                } else {
                    s.entries.insert(index, value);
                }
                if index >= s.len {
                    s.len = index.saturating_add(1);
                }
            }
        }
    }

    /// Removes and returns the last element (or `None` if empty/hole).
    pub fn pop(&mut self) -> Option<JsValue> {
        match self {
            Self::PackedInt32(v) => v.pop().map(JsValue::from_i32),
            Self::PackedFloat64(v) => v.pop().map(JsValue::from_f64),
            Self::PackedValues(v) => v.pop().filter(|val| !val.is_empty()),
            Self::SparseDict(s) => {
                if s.len == 0 {
                    return None;
                }
                s.len -= 1;
                s.entries.remove(&s.len)
            }
        }
    }

    /// Deletes element at `index`, creating a hole without changing `length`.
    pub fn delete(&mut self, index: u32) -> bool {
        let idx = index as usize;
        match self {
            Self::PackedInt32(v) => {
                if idx >= v.len() {
                    return true;
                }
                self.widen_to_values();
                self.delete(index)
            }
            Self::PackedFloat64(v) => {
                if idx >= v.len() {
                    return true;
                }
                self.widen_to_values();
                self.delete(index)
            }
            Self::PackedValues(v) => {
                if idx < v.len() {
                    v[idx] = JsValue::EMPTY;
                }
                true
            }
            Self::SparseDict(s) => {
                s.entries.remove(&index);
                true
            }
        }
    }

    /// Truncates or extends array length cleanly.
    pub fn set_len(&mut self, new_len: u32) {
        let cur = self.len();
        if new_len == cur {
            return;
        }
        if new_len < cur {
            match self {
                Self::PackedInt32(v) => v.truncate(new_len as usize),
                Self::PackedFloat64(v) => v.truncate(new_len as usize),
                Self::PackedValues(v) => v.truncate(new_len as usize),
                Self::SparseDict(s) => {
                    s.entries.retain(|&k, _| k < new_len);
                    s.len = new_len;
                }
            }
        } else {
            let gap = (new_len - cur) as usize;
            if gap > SPARSE_GAP_THRESHOLD {
                self.widen_to_sparse();
            } else if matches!(self, Self::PackedInt32(_) | Self::PackedFloat64(_)) {
                self.widen_to_values();
            }
            match self {
                Self::PackedValues(v) => v.resize(new_len as usize, JsValue::EMPTY),
                Self::SparseDict(s) => s.len = new_len,
                _ => unreachable!(),
            }
        }
    }

    /// Returns all present element indices in ascending numeric order (per ES `[[OwnPropertyKeys]]`).
    pub fn present_indices(&self) -> Vec<u32> {
        match self {
            Self::PackedInt32(v) => (0..v.len() as u32).collect(),
            Self::PackedFloat64(v) => (0..v.len() as u32).collect(),
            Self::PackedValues(v) => v
                .iter()
                .enumerate()
                .filter_map(|(i, val)| (!val.is_empty()).then_some(i as u32))
                .collect(),
            Self::SparseDict(s) => {
                let mut keys: Vec<u32> = s.entries.keys().copied().collect();
                keys.sort_unstable();
                keys
            }
        }
    }

    /// Visits all GC heap handles stored inside elements (no-op for packed numeric arrays!).
    #[inline]
    pub fn trace_children(&self, mut visit: impl FnMut(HeapHandle)) {
        match self {
            Self::PackedInt32(_) | Self::PackedFloat64(_) => {}
            Self::PackedValues(v) => {
                for &val in v {
                    if let Some(h) = val.as_heap_handle() {
                        visit(h);
                    }
                }
            }
            Self::SparseDict(s) => {
                for &val in s.entries.values() {
                    if let Some(h) = val.as_heap_handle() {
                        visit(h);
                    }
                }
            }
        }
    }

    fn widen_to_float64(&mut self) {
        if let Self::PackedInt32(v) = self {
            let floats: Vec<f64> = v.iter().map(|&i| f64::from(i)).collect();
            *self = Self::PackedFloat64(floats);
        }
    }

    fn widen_to_values(&mut self) {
        let values: Vec<JsValue> = match self {
            Self::PackedInt32(v) => v.iter().map(|&i| JsValue::from_i32(i)).collect(),
            Self::PackedFloat64(v) => v.iter().map(|&f| JsValue::from_f64(f)).collect(),
            Self::PackedValues(_) | Self::SparseDict(_) => return,
        };
        *self = Self::PackedValues(values);
    }

    fn widen_to_sparse(&mut self) {
        let len = self.len();
        let mut entries = HashMap::new();
        match self {
            Self::PackedInt32(v) => {
                for (i, &n) in v.iter().enumerate() {
                    entries.insert(i as u32, JsValue::from_i32(n));
                }
            }
            Self::PackedFloat64(v) => {
                for (i, &f) in v.iter().enumerate() {
                    entries.insert(i as u32, JsValue::from_f64(f));
                }
            }
            Self::PackedValues(v) => {
                for (i, &val) in v.iter().enumerate() {
                    if !val.is_empty() {
                        entries.insert(i as u32, val);
                    }
                }
            }
            Self::SparseDict(_) => return,
        }
        *self = Self::SparseDict(Box::new(SparseElements { len, entries }));
    }
}
