#![deny(unsafe_code)]
//! Custom pure-Rust NaN-boxed JavaScript value representation, arena heap,
//! generational tri-color mark-sweep GC, and weak handle tables for RixBrowser.

pub mod ast;
pub mod bigint;
pub mod bytecode;
pub mod heap;
pub mod lexer;
pub mod parser;
pub mod string;
pub mod value;

pub use ast::{Program, SourceType, Span, SyntaxError};
pub use bigint::JsBigInt;
pub use bytecode::{
    CompiledProgram, ConstantEntry, ExceptionHandler, FunctionBytecode, IcKind, IcSlotDescriptor,
    Instruction, ModuleBlueprint, Opcode, UpvalueDescriptor, BYTECODE_VERSION,
};
pub use heap::{
    GcCycleStats, GcHeap, Generation, HeapCell, JsObject, MajorGcPhase, RootStack, WeakHeapHandle,
    WeakTable, DEFAULT_NURSERY_LIMIT, DEFAULT_OLD_GC_THRESHOLD,
};
pub use parser::parse;
pub use string::{JsString, PropertyKey, StringInterner};
pub use value::{HeapHandle, JsValue, JsValueKind, SymbolId, CANONICAL_NAN_BITS};
