#![deny(unsafe_code)]
//! Custom pure-Rust NaN-boxed JavaScript value representation, arena heap,
//! generational tri-color mark-sweep GC, and weak handle tables for RixBrowser.

pub mod bigint;
pub mod heap;
pub mod string;
pub mod value;

pub use bigint::JsBigInt;
pub use heap::{
    GcCycleStats, GcHeap, Generation, HeapCell, JsObject, MajorGcPhase, RootStack, WeakHeapHandle,
    WeakTable, DEFAULT_NURSERY_LIMIT, DEFAULT_OLD_GC_THRESHOLD,
};
pub use string::{JsString, PropertyKey, StringInterner};
pub use value::{HeapHandle, JsValue, JsValueKind, SymbolId, CANONICAL_NAN_BITS};
