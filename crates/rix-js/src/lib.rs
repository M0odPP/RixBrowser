#![deny(unsafe_code)]
//! Custom pure-Rust NaN-boxed JavaScript value representation, arena heap,
//! generational tri-color mark-sweep GC, and weak handle tables for RixBrowser.

pub mod ast;
pub mod bigint;
pub mod builtins_atomics;
pub mod builtins_collections;
pub mod builtins_date;
pub mod builtins_primitives;
pub mod builtins_regexp;
pub mod bytecode;
pub mod cache;
pub mod compiler;
pub mod elements;
pub mod heap;
pub mod ic;
pub mod intl;
pub mod lexer;
pub mod modules;
pub mod parser;
pub mod promise;
pub mod shape;
pub mod string;
pub mod value;
pub mod vm;
pub mod vm_builtins;
pub mod webcrypto;

pub use ast::{Program, SourceType, Span, SyntaxError};
pub use bigint::JsBigInt;
pub use bytecode::{
    CompiledProgram, ConstantEntry, ExceptionHandler, FunctionBytecode, IcKind, IcSlotDescriptor,
    Instruction, ModuleBlueprint, Opcode, UpvalueDescriptor, BYTECODE_VERSION,
};
pub use cache::{BytecodeCache, CacheStatus};
pub use compiler::{compile, compile_program, CompileError};
pub use elements::{ElementsKind, ElementsStorage, SPARSE_GAP_THRESHOLD};
pub use heap::{
    BoundFunctionObject, ClosureObject, GcCycleStats, GcHeap, Generation, GeneratorObject,
    GeneratorState, HeapCell, JsObject, MajorGcPhase, ObjectKind, PromiseObject, PromiseReaction,
    PromiseState, ProxyObject, RootStack, SuspendedFrame, WeakHeapHandle, WeakTable,
    DEFAULT_NURSERY_LIMIT, DEFAULT_OLD_GC_THRESHOLD,
};
pub use ic::{IcEntry, IcState, IcStats, IcTable, MegamorphicCache, POLYMORPHIC_MAX_ENTRIES};
pub use modules::{
    ImportMap, MemoryModuleProvider, ModuleEnv, ModuleError, ModuleLoader, ModuleSourceProvider,
};
pub use parser::parse;
pub use promise::MicrotaskJob;
pub use shape::{
    DictProperty, PropertyAttrs, Shape, ShapeId, ShapeSlot, ShapeTable, DICTIONARY_TRANSITION_LIMIT,
};
pub use string::{JsString, PropertyKey, StringInterner};
pub use value::{HeapHandle, JsValue, JsValueKind, SymbolId, CANONICAL_NAN_BITS};
pub use vm::{
    CallSiteFeedback, EvalError, JitEngine, JitOutcome, JsException, JsVm, NativeFnPtr,
    BASELINE_THRESHOLD, OPT_THRESHOLD, OSR_THRESHOLD,
};
