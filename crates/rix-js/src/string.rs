//! Heap-allocated [`JsString`] supporting flat, interned, rope concatenation,
//! and zero-copy substring slices, plus a per-heap [`StringInterner`] for O(1)
//! property key and identifier lookup.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::value::HeapHandle;

/// Interned property key / identifier ID (`u32`) for O(1) Shape & IC lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PropertyKey(pub u32);

/// GC-managed ECMAScript string representation.
#[derive(Debug, Clone)]
pub enum JsString {
    Flat {
        data: String,
        utf16_len: usize,
    },
    Interned {
        data: Arc<str>,
        key: PropertyKey,
        utf16_len: usize,
    },
    Rope {
        left: HeapHandle,
        right: HeapHandle,
        byte_len: usize,
        utf16_len: usize,
    },
    Slice {
        parent: HeapHandle,
        byte_start: usize,
        byte_end: usize,
        utf16_len: usize,
    },
}

impl JsString {
    #[must_use]
    pub fn from_owned(data: String) -> Self {
        let utf16_len = data.encode_utf16().count();
        Self::Flat { data, utf16_len }
    }

    #[must_use]
    pub fn from_interned(data: Arc<str>, key: PropertyKey) -> Self {
        let utf16_len = data.encode_utf16().count();
        Self::Interned {
            data,
            key,
            utf16_len,
        }
    }

    #[inline]
    #[must_use]
    pub fn utf16_len(&self) -> usize {
        match self {
            Self::Flat { utf16_len, .. }
            | Self::Interned { utf16_len, .. }
            | Self::Rope { utf16_len, .. }
            | Self::Slice { utf16_len, .. } => *utf16_len,
        }
    }

    #[inline]
    #[must_use]
    pub fn byte_len(&self) -> usize {
        match self {
            Self::Flat { data, .. } => data.len(),
            Self::Interned { data, .. } => data.len(),
            Self::Rope { byte_len, .. } => *byte_len,
            Self::Slice {
                byte_start,
                byte_end,
                ..
            } => byte_end.saturating_sub(*byte_start),
        }
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.byte_len() == 0
    }

    #[inline]
    #[must_use]
    pub fn as_flat_str(&self) -> Option<&str> {
        match self {
            Self::Flat { data, .. } => Some(data.as_str()),
            Self::Interned { data, .. } => Some(data.as_ref()),
            Self::Rope { .. } | Self::Slice { .. } => None,
        }
    }

    /// Visits child [`HeapHandle`] references owned by rope/slice strings for GC tracing.
    pub fn trace_children(&self, mut visit: impl FnMut(HeapHandle)) {
        match self {
            Self::Rope { left, right, .. } => {
                visit(*left);
                visit(*right);
            }
            Self::Slice { parent, .. } => visit(*parent),
            Self::Flat { .. } | Self::Interned { .. } => {}
        }
    }
}

/// Per-realm string interner assigning dense [`PropertyKey`] indices.
#[derive(Debug, Default)]
pub struct StringInterner {
    by_str: HashMap<Arc<str>, PropertyKey>,
    entries: Vec<Arc<str>>,
}

impl StringInterner {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn intern(&mut self, text: &str) -> PropertyKey {
        if let Some(&key) = self.by_str.get(text) {
            return key;
        }
        let arc: Arc<str> = Arc::from(text);
        let key = PropertyKey(self.entries.len() as u32);
        self.entries.push(Arc::clone(&arc));
        self.by_str.insert(arc, key);
        key
    }

    #[must_use]
    pub fn get(&self, key: PropertyKey) -> Option<&Arc<str>> {
        self.entries.get(key.0 as usize)
    }

    #[must_use]
    pub fn lookup(&self, text: &str) -> Option<PropertyKey> {
        self.by_str.get(text).copied()
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
