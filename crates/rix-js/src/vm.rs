//! Tier-1 Register-Window Bytecode Virtual Machine (`JsVm`).
//!
//! Reference: Spec 05 §3 & §4.2.
//! Executes compact 32-bit fixed-width register bytecode on a reusable flat
//! `Vec<JsValue>` register stack with Monomorphic -> Polymorphic -> Megamorphic
//! Inline Caches, open-upvalue closures, classes/`super`, `try`/`catch`/`finally`
//! exception unwinding, `Proxy`/`Reflect`, and cooperative loop-interrupt budgets.

use crate::ast::SourceType;
use crate::bigint::JsBigInt;
use crate::bytecode::{
    CompiledProgram, ConstantEntry, FunctionBytecode, IcSlotDescriptor, Instruction, Opcode,
    SerializableMethodKind, WIDE_IC_SENTINEL,
};
use crate::compiler::{compile, CompileError};
use crate::heap::{
    ClassInstanceInit, ClosureObject, GcCycleStats, GcHeap, JsObject, ObjectKind, PrivateEntry,
    ProxyObject, RegExpObject, SuspendedFrame,
};
use crate::ic::{IcEntry, IcStats, IcTable, MegamorphicCache};
use crate::modules::ModuleEnv;
use crate::promise::MicrotaskJob;
use crate::shape::{PropertyAttrs, ShapeId, ShapeTable};
use crate::string::PropertyKey;
use crate::value::{HeapHandle, JsValue, JsValueKind, SymbolId};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use thiserror::Error;

/// Runtime JavaScript exception thrown across VM frames.
#[derive(Debug, Clone)]
pub struct JsException {
    pub value: JsValue,
    pub message: String,
}

impl fmt::Display for JsException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Uncaught {}", self.message)
    }
}

impl std::error::Error for JsException {}

/// Error returned by [`JsVm::eval_script`].
#[derive(Debug, Error)]
pub enum EvalError {
    #[error("{0}")]
    Compile(#[from] CompileError),
    #[error("{0}")]
    Runtime(#[from] JsException),
}

/// Native function callback signature.
pub type NativeFnPtr = fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException>;

/// Shared closure-backed host callback.
pub type NativeClosure = Rc<dyn Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException>>;

/// Dynamic `import(specifier)` handler callback `(vm, referrer_url, specifier) -> promise_handle`.
pub type DynamicImportHandler = Rc<dyn Fn(&mut JsVm, Option<String>, String) -> HeapHandle>;

/// Outcome of running a potentially suspendable (`async` / generator) bytecode frame.
#[derive(Debug, Clone)]
pub enum RunOutcome {
    Completed(JsValue),
    SuspendedAwait {
        frame: SuspendedFrame,
        awaited: JsValue,
    },
    SuspendedYield {
        frame: SuspendedFrame,
        yielded: JsValue,
    },
    SuspendedYieldStar {
        frame: SuspendedFrame,
        iterable: JsValue,
    },
    SuspendedInitial {
        frame: SuspendedFrame,
    },
}

/// Tier-2 Baseline Cranelift JIT invocation threshold (Spec 05 §4.3).
pub const BASELINE_THRESHOLD: u32 = 32;
/// Tier-2 On-Stack Replacement (OSR) loop back-edge threshold (Spec 05 §4.3).
pub const OSR_THRESHOLD: u32 = 256;
/// Tier-3 Speculative Optimizing JIT invocation threshold (Spec 05 §4.4).
pub const OPT_THRESHOLD: u32 = 1_000;

/// Result returned from a native JIT entry invocation back to [`JsVm`].
#[derive(Debug, Clone)]
pub enum JitOutcome {
    /// Compiled frame completed normally and produced `val`.
    Returned(JsValue),
    /// Compiled frame raised a JavaScript exception to be unwound by `JsVm`.
    Threw(JsException),
    /// Compiled frame deoptimized cleanly back to interpreter at `resume_pc`
    /// with the live register window already materialized.
    Deopt { resume_pc: usize },
    /// Function/entry is not JIT-compiled — continue execution in Tier-1 interpreter.
    Fallback,
}

/// Monomorphic/polymorphic callee feedback collected at `Opcode::Call` sites for Tier-3 inlining.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CallSiteFeedback {
    #[default]
    Uninitialized,
    Monomorphic {
        callee: HeapHandle,
        program_id: u32,
        func_idx: u32,
    },
    Polymorphic,
}

/// Pluggable JIT compilation & execution backend attached to [`JsVm`]
/// (implemented in `rix-jit` while keeping `rix-js` 100% `#![deny(unsafe_code)]`).
pub trait JitEngine {
    /// Minimum call count before attempting Tier-2 function-entry JIT compilation.
    fn baseline_threshold(&self) -> u32 {
        BASELINE_THRESHOLD
    }

    /// Minimum loop back-edge count before attempting Tier-2 OSR compilation.
    fn osr_threshold(&self) -> u32 {
        OSR_THRESHOLD
    }

    /// Attempts to compile (if hot) and execute `(program_id, func_idx)` starting at
    /// `entry_pc` (`0` for normal function entry, loop-header PC for OSR).
    fn try_enter(
        &self,
        vm: &mut JsVm,
        program_id: u32,
        func_idx: u32,
        entry_pc: usize,
        call_count: u32,
        back_edge_count: u32,
    ) -> JitOutcome;
}

/// Native callable backing stored in [`JsVm::native_fns`].
pub enum NativeCallable {
    Static(NativeFnPtr),
    Closure(NativeClosure),
}

#[derive(Debug, Clone)]
struct CallFrame {
    program_id: u32,
    func_idx: u32,
    base_reg: usize,
    pc: usize,
    this_val: JsValue,
    callee: HeapHandle,
    ret_dst: Option<usize>,
    construct_obj: Option<HeapHandle>,
    new_target: JsValue,
    call_args: Vec<JsValue>,
    arguments_obj: Option<HeapHandle>,
}

#[derive(Debug, Clone)]
enum IteratorState {
    Array {
        arr: HeapHandle,
        index: u32,
    },
    String {
        chars: Vec<char>,
        index: usize,
    },
    Keys {
        keys: Vec<String>,
        index: usize,
    },
    Custom {
        iter_obj: HeapHandle,
        exhausted: bool,
    },
    AsyncCustom {
        iter_obj: HeapHandle,
    },
}

/// Core pure-Rust Register Bytecode Virtual Machine (`#![deny(unsafe_code)]`).
pub struct JsVm {
    pub heap: GcHeap,
    pub shapes: ShapeTable,
    pub mega_ic: MegamorphicCache,
    pub ic_stats: IcStats,
    pub global: HeapHandle,
    pub object_proto: HeapHandle,
    pub function_proto: HeapHandle,
    pub array_proto: HeapHandle,
    pub error_proto: HeapHandle,
    pub type_error_proto: HeapHandle,
    pub reference_error_proto: HeapHandle,
    pub range_error_proto: HeapHandle,
    pub syntax_error_proto: HeapHandle,
    pub uri_error_proto: HeapHandle,
    pub eval_error_proto: HeapHandle,
    pub aggregate_error_proto: HeapHandle,
    pub suppressed_error_proto: HeapHandle,
    pub disposable_stack_proto: HeapHandle,
    pub async_disposable_stack_proto: HeapHandle,
    pub string_proto: HeapHandle,
    pub number_proto: HeapHandle,
    pub boolean_proto: HeapHandle,
    pub bigint_proto: HeapHandle,
    pub symbol_proto: HeapHandle,
    pub regexp_proto: HeapHandle,
    pub date_proto: HeapHandle,
    pub map_proto: HeapHandle,
    pub set_proto: HeapHandle,
    pub weakmap_proto: HeapHandle,
    pub weakset_proto: HeapHandle,
    pub weakref_proto: HeapHandle,
    pub array_buffer_proto: HeapHandle,
    pub shared_array_buffer_proto: HeapHandle,
    pub data_view_proto: HeapHandle,
    pub typed_array_proto: HeapHandle,
    pub typed_array_protos: [HeapHandle; 12],
    pub promise_proto: HeapHandle,
    pub promise_ctor: Option<HeapHandle>,
    pub iterator_proto: HeapHandle,
    pub iterator_helper_proto: HeapHandle,
    pub wrap_iterator_proto: HeapHandle,
    pub async_iterator_proto: HeapHandle,
    pub string_iter_proto: HeapHandle,
    pub regexp_iter_proto: HeapHandle,
    pub generator_proto: HeapHandle,
    pub async_generator_proto: HeapHandle,
    pub sym_iterator: SymbolId,
    pub key_symbol_iterator: PropertyKey,
    pub sym_async_iterator: SymbolId,
    pub key_symbol_async_iterator: PropertyKey,
    pub sym_dispose: SymbolId,
    pub key_symbol_dispose: PropertyKey,
    pub sym_async_dispose: SymbolId,
    pub key_symbol_async_dispose: PropertyKey,
    pub sym_to_primitive: SymbolId,
    pub key_symbol_to_primitive: PropertyKey,
    pub sym_species: SymbolId,
    pub key_symbol_species: PropertyKey,
    pub sym_has_instance: SymbolId,
    pub key_symbol_has_instance: PropertyKey,
    pub sym_is_concat_spreadable: SymbolId,
    pub key_symbol_is_concat_spreadable: PropertyKey,
    pub sym_to_string_tag: SymbolId,
    pub key_symbol_to_string_tag: PropertyKey,
    pub sym_unscopables: SymbolId,
    pub key_symbol_unscopables: PropertyKey,
    pub sym_match: SymbolId,
    pub key_symbol_match: PropertyKey,
    pub sym_match_all: SymbolId,
    pub key_symbol_match_all: PropertyKey,
    pub sym_replace: SymbolId,
    pub key_symbol_replace: PropertyKey,
    pub sym_search: SymbolId,
    pub key_symbol_search: PropertyKey,
    pub sym_split: SymbolId,
    pub key_symbol_split: PropertyKey,
    pub default_array_iter_fn: Option<HeapHandle>,
    pub eval_fn: Option<HeapHandle>,
    pub symbol_registry: HashMap<String, SymbolId>,
    pub symbol_descriptions: HashMap<SymbolId, String>,
    pub regexp_statics: crate::builtins_regexp::RegExpLegacyStatics,
    /// Pending same-agent `Atomics.waitAsync` waiters: `(sab_backing_ptr, byte_offset, promise)`.
    pub atomic_async_waiters: Vec<(usize, usize, HeapHandle)>,
    pub rng_state: u64,
    microtasks: VecDeque<MicrotaskJob>,
    module_envs: HashMap<u32, ModuleEnv>,
    dynamic_import_handler: Option<DynamicImportHandler>,
    programs: Vec<CompiledProgram>,
    ic_tables: Vec<Vec<IcTable>>,
    const_keys: Vec<Vec<Vec<Option<PropertyKey>>>>,
    native_fns: Vec<NativeCallable>,
    registers: Vec<JsValue>,
    frames: Vec<CallFrame>,
    open_upvalues: Vec<(usize, HeapHandle)>,
    with_stack: Vec<HeapHandle>,
    iterators: HashMap<HeapHandle, IteratorState>,
    /// Optional cooperative loop back-edge budget (`None` = unlimited).
    pub max_loop_iterations: Option<u64>,
    pub loop_iterations: u64,
    native_call_depth: u32,
    /// Active `new.target` during a `NativeFunction` constructor invocation (`undefined` if called).
    pub native_new_target: JsValue,
    /// Shared atomic flag checked on every `Opcode::LoopBack`.
    pub interrupt_flag: Arc<AtomicBool>,
    /// Per-function invocation counters (used for Tier-2/3 JIT tier-up in Phase 3).
    pub call_counts: Vec<Vec<u32>>,
    /// Per-function loop back-edge counters (used for Tier-2 OSR in Phase 3).
    pub back_edge_counts: Vec<Vec<u32>>,
    /// Optional attached Tier-2 / Tier-3 Cranelift JIT backend.
    jit: Option<Rc<dyn JitEngine>>,
    /// Per-call-site `(prog_id, func_idx, pc)` monomorphic callee feedback for Tier-3 inlining.
    call_feedback: HashMap<(u32, u32, usize), CallSiteFeedback>,
    /// Per-element-site `(prog_id, func_idx, pc)` observed array `ElementsKind` for Tier-3.
    elem_feedback: HashMap<(u32, u32, usize), crate::elements::ElementsKind>,
    /// Stashed JS exception raised inside an `extern "C"` JIT trampoline.
    pub pending_jit_exception: Option<JsException>,
    /// Frequently interned well-known property keys.
    pub key_length: PropertyKey,
    pub key_prototype: PropertyKey,
    pub key_constructor: PropertyKey,
    pub key_name: PropertyKey,
    pub key_message: PropertyKey,
    pub key_to_string: PropertyKey,
    pub key_value_of: PropertyKey,
    pub key_next: PropertyKey,
    pub key_return: PropertyKey,
    pub key_done: PropertyKey,
    pub key_value: PropertyKey,
    pub key_get: PropertyKey,
    pub key_set: PropertyKey,
    pub key_has: PropertyKey,
    pub key_delete_property: PropertyKey,
    pub key_apply: PropertyKey,
    pub key_construct: PropertyKey,
    pub key_get_prototype_of: PropertyKey,
    pub key_set_prototype_of: PropertyKey,
    pub key_own_keys: PropertyKey,
}

impl Default for JsVm {
    fn default() -> Self {
        Self::new()
    }
}

impl JsVm {
    #[must_use]
    pub fn new() -> Self {
        let mut heap = GcHeap::new();
        let mut shapes = ShapeTable::new();

        let object_proto = heap.alloc_empty_object();
        heap.get_object_mut(object_proto).unwrap().is_prototype = true;

        let function_proto = heap.alloc_object(JsObject::with_kind(
            ObjectKind::NativeFunction(0),
            Some(object_proto),
        ));
        heap.get_object_mut(function_proto).unwrap().is_prototype = true;

        let array_proto =
            heap.alloc_object(JsObject::with_kind(ObjectKind::Array, Some(object_proto)));
        heap.get_object_mut(array_proto).unwrap().is_prototype = true;

        let error_proto = heap.alloc_empty_object();
        heap.object_set_prototype(error_proto, Some(object_proto), &mut shapes);
        heap.get_object_mut(error_proto).unwrap().is_prototype = true;

        let string_proto = heap.alloc_empty_object();
        heap.object_set_prototype(string_proto, Some(object_proto), &mut shapes);
        let empty_str_val = JsValue::from_string(heap.alloc_string(""));
        {
            let o = heap.get_object_mut(string_proto).unwrap();
            o.is_prototype = true;
            o.rare_mut().boxed_value = Some(empty_str_val);
        }
        let k_len_init = heap.intern_key("length");
        heap.object_define_data(
            string_proto,
            k_len_init,
            JsValue::from_i32(0),
            PropertyAttrs::empty(),
            &mut shapes,
        );

        let number_proto = heap.alloc_empty_object();
        heap.object_set_prototype(number_proto, Some(object_proto), &mut shapes);
        {
            let o = heap.get_object_mut(number_proto).unwrap();
            o.is_prototype = true;
            o.rare_mut().boxed_value = Some(JsValue::from_f64(0.0));
        }

        let boolean_proto = heap.alloc_empty_object();
        heap.object_set_prototype(boolean_proto, Some(object_proto), &mut shapes);
        {
            let o = heap.get_object_mut(boolean_proto).unwrap();
            o.is_prototype = true;
            o.rare_mut().boxed_value = Some(JsValue::FALSE);
        }

        let regexp_proto = heap.alloc_empty_object();
        heap.object_set_prototype(regexp_proto, Some(object_proto), &mut shapes);
        heap.get_object_mut(regexp_proto).unwrap().is_prototype = true;

        let date_proto = heap.alloc_empty_object();
        heap.object_set_prototype(date_proto, Some(object_proto), &mut shapes);
        heap.get_object_mut(date_proto).unwrap().is_prototype = true;

        let make_proto = |heap: &mut GcHeap, shapes: &mut ShapeTable, parent: HeapHandle| {
            let p = heap.alloc_empty_object();
            heap.object_set_prototype(p, Some(parent), shapes);
            heap.get_object_mut(p).unwrap().is_prototype = true;
            p
        };
        let bigint_proto = make_proto(&mut heap, &mut shapes, object_proto);
        let symbol_proto = make_proto(&mut heap, &mut shapes, object_proto);
        let type_error_proto = make_proto(&mut heap, &mut shapes, error_proto);
        let reference_error_proto = make_proto(&mut heap, &mut shapes, error_proto);
        let range_error_proto = make_proto(&mut heap, &mut shapes, error_proto);
        let syntax_error_proto = make_proto(&mut heap, &mut shapes, error_proto);
        let uri_error_proto = make_proto(&mut heap, &mut shapes, error_proto);
        let eval_error_proto = make_proto(&mut heap, &mut shapes, error_proto);
        let aggregate_error_proto = make_proto(&mut heap, &mut shapes, error_proto);
        let suppressed_error_proto = make_proto(&mut heap, &mut shapes, error_proto);
        let disposable_stack_proto = make_proto(&mut heap, &mut shapes, object_proto);
        let async_disposable_stack_proto = make_proto(&mut heap, &mut shapes, object_proto);
        let map_proto = make_proto(&mut heap, &mut shapes, object_proto);
        let set_proto = make_proto(&mut heap, &mut shapes, object_proto);
        let weakmap_proto = make_proto(&mut heap, &mut shapes, object_proto);
        let weakset_proto = make_proto(&mut heap, &mut shapes, object_proto);
        let weakref_proto = make_proto(&mut heap, &mut shapes, object_proto);
        let array_buffer_proto = make_proto(&mut heap, &mut shapes, object_proto);
        let shared_array_buffer_proto = make_proto(&mut heap, &mut shapes, object_proto);
        let data_view_proto = make_proto(&mut heap, &mut shapes, object_proto);
        let typed_array_proto = make_proto(&mut heap, &mut shapes, object_proto);
        let typed_array_protos =
            std::array::from_fn(|_| make_proto(&mut heap, &mut shapes, typed_array_proto));
        let promise_proto = make_proto(&mut heap, &mut shapes, object_proto);
        let iterator_proto = make_proto(&mut heap, &mut shapes, object_proto);
        let iterator_helper_proto = make_proto(&mut heap, &mut shapes, iterator_proto);
        let wrap_iterator_proto = make_proto(&mut heap, &mut shapes, iterator_proto);
        let async_iterator_proto = make_proto(&mut heap, &mut shapes, object_proto);
        let string_iter_proto = make_proto(&mut heap, &mut shapes, iterator_proto);
        let regexp_iter_proto = make_proto(&mut heap, &mut shapes, iterator_proto);
        let generator_proto = make_proto(&mut heap, &mut shapes, iterator_proto);
        let async_generator_proto = make_proto(&mut heap, &mut shapes, async_iterator_proto);

        let alloc_wk = |heap: &mut GcHeap, desc: &str| {
            let s = heap.alloc_symbol(Some(desc.into()));
            let k = heap.intern_key(&format!("Symbol({})", s.0));
            (s, k)
        };
        let (sym_iterator, key_symbol_iterator) = alloc_wk(&mut heap, "Symbol.iterator");
        let (sym_async_iterator, key_symbol_async_iterator) =
            alloc_wk(&mut heap, "Symbol.asyncIterator");
        let (sym_dispose, key_symbol_dispose) = alloc_wk(&mut heap, "Symbol.dispose");
        let (sym_async_dispose, key_symbol_async_dispose) =
            alloc_wk(&mut heap, "Symbol.asyncDispose");
        let (sym_to_primitive, key_symbol_to_primitive) = alloc_wk(&mut heap, "Symbol.toPrimitive");
        let (sym_species, key_symbol_species) = alloc_wk(&mut heap, "Symbol.species");
        let (sym_has_instance, key_symbol_has_instance) = alloc_wk(&mut heap, "Symbol.hasInstance");
        let (sym_is_concat_spreadable, key_symbol_is_concat_spreadable) =
            alloc_wk(&mut heap, "Symbol.isConcatSpreadable");
        let (sym_to_string_tag, key_symbol_to_string_tag) =
            alloc_wk(&mut heap, "Symbol.toStringTag");
        let (sym_unscopables, key_symbol_unscopables) = alloc_wk(&mut heap, "Symbol.unscopables");
        let (sym_match, key_symbol_match) = alloc_wk(&mut heap, "Symbol.match");
        let (sym_match_all, key_symbol_match_all) = alloc_wk(&mut heap, "Symbol.matchAll");
        let (sym_replace, key_symbol_replace) = alloc_wk(&mut heap, "Symbol.replace");
        let (sym_search, key_symbol_search) = alloc_wk(&mut heap, "Symbol.search");
        let (sym_split, key_symbol_split) = alloc_wk(&mut heap, "Symbol.split");

        let global = heap.alloc_empty_object();
        heap.object_set_prototype(global, Some(object_proto), &mut shapes);
        heap.get_object_mut(global).unwrap().is_global = true;

        let key_length = heap.intern_key("length");
        let key_prototype = heap.intern_key("prototype");
        let key_constructor = heap.intern_key("constructor");
        let key_name = heap.intern_key("name");
        let key_message = heap.intern_key("message");
        let key_to_string = heap.intern_key("toString");
        let key_value_of = heap.intern_key("valueOf");
        let key_next = heap.intern_key("next");
        let key_return = heap.intern_key("return");
        let key_done = heap.intern_key("done");
        let key_value = heap.intern_key("value");
        let key_get = heap.intern_key("get");
        let key_set = heap.intern_key("set");
        let key_has = heap.intern_key("has");
        let key_delete_property = heap.intern_key("deleteProperty");
        let key_apply = heap.intern_key("apply");
        let key_construct = heap.intern_key("construct");
        let key_get_prototype_of = heap.intern_key("getPrototypeOf");
        let key_set_prototype_of = heap.intern_key("setPrototypeOf");
        let key_own_keys = heap.intern_key("ownKeys");

        let mut vm = Self {
            heap,
            shapes,
            mega_ic: MegamorphicCache::new(),
            ic_stats: IcStats::default(),
            global,
            object_proto,
            function_proto,
            array_proto,
            error_proto,
            type_error_proto,
            reference_error_proto,
            range_error_proto,
            syntax_error_proto,
            uri_error_proto,
            eval_error_proto,
            aggregate_error_proto,
            suppressed_error_proto,
            disposable_stack_proto,
            async_disposable_stack_proto,
            string_proto,
            number_proto,
            boolean_proto,
            bigint_proto,
            symbol_proto,
            regexp_proto,
            date_proto,
            map_proto,
            set_proto,
            weakmap_proto,
            weakset_proto,
            weakref_proto,
            array_buffer_proto,
            shared_array_buffer_proto,
            data_view_proto,
            typed_array_proto,
            typed_array_protos,
            promise_proto,
            promise_ctor: None,
            iterator_proto,
            iterator_helper_proto,
            wrap_iterator_proto,
            async_iterator_proto,
            string_iter_proto,
            regexp_iter_proto,
            generator_proto,
            async_generator_proto,
            sym_iterator,
            key_symbol_iterator,
            sym_async_iterator,
            key_symbol_async_iterator,
            sym_dispose,
            key_symbol_dispose,
            sym_async_dispose,
            key_symbol_async_dispose,
            sym_to_primitive,
            key_symbol_to_primitive,
            sym_species,
            key_symbol_species,
            sym_has_instance,
            key_symbol_has_instance,
            sym_is_concat_spreadable,
            key_symbol_is_concat_spreadable,
            sym_to_string_tag,
            key_symbol_to_string_tag,
            sym_unscopables,
            key_symbol_unscopables,
            sym_match,
            key_symbol_match,
            sym_match_all,
            key_symbol_match_all,
            sym_replace,
            key_symbol_replace,
            sym_search,
            key_symbol_search,
            sym_split,
            key_symbol_split,
            default_array_iter_fn: None,
            eval_fn: None,
            symbol_registry: HashMap::new(),
            symbol_descriptions: HashMap::new(),
            regexp_statics: crate::builtins_regexp::RegExpLegacyStatics::default(),
            atomic_async_waiters: Vec::new(),
            rng_state: 0x9E37_79B9_7F4A_7C15,
            microtasks: VecDeque::new(),
            module_envs: HashMap::new(),
            dynamic_import_handler: None,
            programs: Vec::new(),
            ic_tables: Vec::new(),
            const_keys: Vec::new(),
            native_fns: vec![NativeCallable::Static(|_vm, _this, _args| {
                Ok(JsValue::UNDEFINED)
            })],
            registers: Vec::with_capacity(1024),
            frames: Vec::with_capacity(64),
            open_upvalues: Vec::new(),
            with_stack: Vec::new(),
            iterators: HashMap::new(),
            max_loop_iterations: None,
            loop_iterations: 0,
            native_call_depth: 0,
            native_new_target: JsValue::UNDEFINED,
            interrupt_flag: Arc::new(AtomicBool::new(false)),
            call_counts: Vec::new(),
            back_edge_counts: Vec::new(),
            jit: None,
            call_feedback: HashMap::new(),
            elem_feedback: HashMap::new(),
            pending_jit_exception: None,
            key_length,
            key_prototype,
            key_constructor,
            key_name,
            key_message,
            key_to_string,
            key_value_of,
            key_next,
            key_return,
            key_done,
            key_value,
            key_get,
            key_set,
            key_has,
            key_delete_property,
            key_apply,
            key_construct,
            key_get_prototype_of,
            key_set_prototype_of,
            key_own_keys,
        };
        crate::vm_builtins::install_core_intrinsics(&mut vm);
        crate::builtins_primitives::install_primitive_builtins(&mut vm);
        crate::builtins_collections::install_collection_builtins(&mut vm);
        crate::builtins_iterator::install_iterator(&mut vm);
        crate::promise::install_promise_and_async(&mut vm);
        crate::builtins_disposable::install_disposable(&mut vm);
        crate::intl::install_intl(&mut vm);
        crate::webcrypto::install_webcrypto(&mut vm);
        vm
    }

    /// Enqueues an ECMAScript microtask job in FIFO order.
    pub fn enqueue_microtask(&mut self, job: MicrotaskJob) {
        self.microtasks.push_back(job);
    }

    /// Pops the next queued microtask job.
    pub fn pop_microtask(&mut self) -> Option<MicrotaskJob> {
        self.microtasks.pop_front()
    }

    /// Returns `true` if any microtasks are waiting to run.
    #[must_use]
    pub fn has_pending_microtasks(&self) -> bool {
        !self.microtasks.is_empty()
    }

    /// Drains all queued microtasks FIFO (Promise reactions, `queueMicrotask`, `async`/`await`).
    pub fn drain_microtasks(&mut self) {
        crate::promise::drain_microtasks(self);
    }

    /// Registers a runtime [`ModuleEnv`] for compiled program `program_id`.
    pub fn register_module_env(&mut self, program_id: u32, env: ModuleEnv) {
        self.module_envs.insert(program_id, env);
    }

    #[must_use]
    pub fn module_env(&self, program_id: u32) -> Option<&ModuleEnv> {
        self.module_envs.get(&program_id)
    }

    pub fn module_env_mut(&mut self, program_id: u32) -> Option<&mut ModuleEnv> {
        self.module_envs.get_mut(&program_id)
    }

    /// Installs the dynamic `import(specifier)` callback used by `Opcode::DynamicImport`.
    pub fn set_dynamic_import_handler(&mut self, handler: DynamicImportHandler) {
        self.dynamic_import_handler = Some(handler);
    }

    /// Collects every active VM root into a flat vector for generational or major GC.
    #[must_use]
    pub fn collect_roots(&self) -> Vec<JsValue> {
        let mut roots = Vec::with_capacity(
            64 + self.registers.len() + self.frames.len() * 4 + self.open_upvalues.len(),
        );
        for h in [
            self.global,
            self.object_proto,
            self.function_proto,
            self.array_proto,
            self.error_proto,
            self.type_error_proto,
            self.reference_error_proto,
            self.range_error_proto,
            self.syntax_error_proto,
            self.uri_error_proto,
            self.eval_error_proto,
            self.aggregate_error_proto,
            self.suppressed_error_proto,
            self.disposable_stack_proto,
            self.async_disposable_stack_proto,
            self.string_proto,
            self.number_proto,
            self.boolean_proto,
            self.bigint_proto,
            self.symbol_proto,
            self.regexp_proto,
            self.date_proto,
            self.map_proto,
            self.set_proto,
            self.weakmap_proto,
            self.weakset_proto,
            self.weakref_proto,
            self.array_buffer_proto,
            self.shared_array_buffer_proto,
            self.data_view_proto,
            self.typed_array_proto,
            self.promise_proto,
            self.iterator_proto,
            self.iterator_helper_proto,
            self.wrap_iterator_proto,
            self.async_iterator_proto,
            self.string_iter_proto,
            self.regexp_iter_proto,
            self.generator_proto,
            self.async_generator_proto,
        ] {
            roots.push(JsValue::from_object(h));
        }
        if let Some(pc) = self.promise_ctor {
            roots.push(JsValue::from_object(pc));
        }
        for &h in &self.typed_array_protos {
            roots.push(JsValue::from_object(h));
        }
        for job in &self.microtasks {
            job.trace_roots(&mut roots);
        }
        for env in self.module_envs.values() {
            env.trace_roots(&mut roots);
        }
        roots.extend_from_slice(&self.registers);
        for f in &self.frames {
            roots.push(f.this_val);
            roots.push(JsValue::from_object(f.callee));
            if let Some(c) = f.construct_obj {
                roots.push(JsValue::from_object(c));
            }
            roots.push(f.new_target);
            roots.extend_from_slice(&f.call_args);
            if let Some(a) = f.arguments_obj {
                roots.push(JsValue::from_object(a));
            }
        }
        for &(_, uv) in &self.open_upvalues {
            roots.push(JsValue::from_object(uv));
        }
        for &w in &self.with_stack {
            roots.push(JsValue::from_object(w));
        }
        for (&iter_h, state) in &self.iterators {
            roots.push(JsValue::from_object(iter_h));
            match state {
                IteratorState::Array { arr, .. } => roots.push(JsValue::from_object(*arr)),
                IteratorState::Custom { iter_obj, .. }
                | IteratorState::AsyncCustom { iter_obj } => {
                    roots.push(JsValue::from_object(*iter_obj))
                }
                IteratorState::String { .. } | IteratorState::Keys { .. } => {}
            }
        }
        for &(_, _, p) in &self.atomic_async_waiters {
            roots.push(JsValue::from_object(p));
        }
        if let Some(h) = self.default_array_iter_fn {
            roots.push(JsValue::from_object(h));
        }
        if let Some(h) = self.eval_fn {
            roots.push(JsValue::from_object(h));
        }
        roots
    }

    /// Triggers a full stop-the-world Major GC across all live VM roots.
    pub fn collect_garbage(&mut self) -> GcCycleStats {
        let roots = self.collect_roots();
        let stats = self.heap.collect_major(&roots);
        self.iterators.retain(|&h, _| self.heap.is_alive(h));
        stats
    }

    /// Detaches an unshared mutable `ArrayBuffer` per ECMA-262 §25.1.2.3 `DetachArrayBuffer`.
    pub fn detach_array_buffer(&mut self, h: HeapHandle) -> Result<(), JsException> {
        if crate::builtins_atomics::ab_is_shared(self, h) {
            return Err(self.type_error("Cannot detach a SharedArrayBuffer"));
        }
        if crate::builtins_atomics::ab_is_immutable(self, h) {
            return Err(self.type_error("Cannot detach an immutable ArrayBuffer"));
        }
        if let Some(o) = self.heap.get_object_mut(h) {
            if let ObjectKind::ArrayBuffer(buf) = &mut o.kind {
                buf.clear();
                o.rare_mut().ab_detached = true;
                return Ok(());
            }
        }
        Err(self.type_error("Expected an ArrayBuffer"))
    }

    /// Allocates a `SharedArrayBuffer` backed by an existing cross-thread [`SharedBufferBacking`].
    pub fn alloc_shared_array_buffer_from_backing(
        &mut self,
        backing: std::sync::Arc<crate::heap::SharedBufferBacking>,
    ) -> HeapHandle {
        let h = self.heap.alloc_object(JsObject::with_kind(
            ObjectKind::ArrayBuffer(Box::default()),
            Some(self.shared_array_buffer_proto),
        ));
        if let Some(o) = self.heap.get_object_mut(h) {
            let r = o.rare_mut();
            r.ab_max_byte_length = backing.max_byte_length;
            r.shared_mem = Some(backing);
        }
        h
    }

    /// Allocates a `RegExp` object with `RegExp.prototype` and own `lastIndex` per ECMA-262 §22.2.3.
    pub fn alloc_regexp(&mut self, pattern: String, flags: String) -> HeapHandle {
        let canonical_flags = crate::builtins_regexp::canonicalize_flags(&flags);
        let compiled = crate::builtins_regexp::compile_regress_raw(&pattern, &canonical_flags)
            .ok()
            .map(std::sync::Arc::new);
        let re = self.heap.alloc_object(JsObject::with_kind(
            ObjectKind::RegExp(Box::new(RegExpObject {
                pattern,
                flags: canonical_flags,
                compiled,
            })),
            Some(self.regexp_proto),
        ));
        let k_last = self.heap.intern_key("lastIndex");
        self.heap.object_define_data(
            re,
            k_last,
            JsValue::from_i32(0),
            PropertyAttrs::WRITABLE,
            &mut self.shapes,
        );
        re
    }

    /// Allocates a `Date` object storing UTC milliseconds since Unix epoch.
    pub fn alloc_date(&mut self, timestamp_ms: f64) -> HeapHandle {
        self.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Date(timestamp_ms),
            Some(self.date_proto),
        ))
    }

    /// Marks a callable handle as constructable (`[[Construct]]` internal method present).
    pub fn mark_constructor(&mut self, handle: HeapHandle) {
        if let Some(o) = self.heap.get_object_mut(handle) {
            o.is_constructor = true;
        }
    }

    /// Sets a function object's own `length` property (`{ writable: false, enumerable: false, configurable: true }`).
    pub fn set_fn_length(&mut self, handle: HeapHandle, len: u32) {
        self.heap.object_define_data(
            handle,
            self.key_length,
            JsValue::from_i32(len as i32),
            PropertyAttrs::CONFIGURABLE,
            &mut self.shapes,
        );
    }

    /// Returns `true` if `handle` implements ECMAScript `IsConstructor(handle)`.
    pub fn is_constructor_handle(&self, handle: HeapHandle) -> bool {
        let mut cur = handle;
        for _ in 0..32 {
            let Some(obj) = self.heap.get_object(cur) else {
                return false;
            };
            match &obj.kind {
                ObjectKind::BoundFunction(b) => cur = b.target,
                ObjectKind::Proxy(p) => {
                    if p.revoked {
                        return false;
                    }
                    cur = p.target;
                }
                _ => return obj.is_constructor,
            }
        }
        false
    }

    /// Registers a static native function and returns its callable [`HeapHandle`].
    pub fn alloc_native_fn(&mut self, name: &str, f: NativeFnPtr) -> HeapHandle {
        let id = self.native_fns.len() as u32;
        self.native_fns.push(NativeCallable::Static(f));
        let obj = self.heap.alloc_object(JsObject::with_kind(
            ObjectKind::NativeFunction(id),
            Some(self.function_proto),
        ));
        self.heap.object_define_data(
            obj,
            self.key_length,
            JsValue::from_i32(0),
            PropertyAttrs::CONFIGURABLE,
            &mut self.shapes,
        );
        let name_val = JsValue::from_string(self.heap.alloc_string(name));
        self.heap.object_define_data(
            obj,
            self.key_name,
            name_val,
            PropertyAttrs::CONFIGURABLE,
            &mut self.shapes,
        );
        obj
    }

    /// Registers a closure-backed native host callback and returns its [`HeapHandle`].
    pub fn alloc_host_fn(
        &mut self,
        name: &str,
        f: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
    ) -> HeapHandle {
        let id = self.native_fns.len() as u32;
        self.native_fns.push(NativeCallable::Closure(Rc::new(f)));
        let obj = self.heap.alloc_object(JsObject::with_kind(
            ObjectKind::NativeFunction(id),
            Some(self.function_proto),
        ));
        self.heap.object_define_data(
            obj,
            self.key_length,
            JsValue::from_i32(0),
            PropertyAttrs::CONFIGURABLE,
            &mut self.shapes,
        );
        let name_val = JsValue::from_string(self.heap.alloc_string(name));
        self.heap.object_define_data(
            obj,
            self.key_name,
            name_val,
            PropertyAttrs::CONFIGURABLE,
            &mut self.shapes,
        );
        obj
    }

    /// Allocates an ordinary object with `Object.prototype`.
    pub fn alloc_ordinary_object(&mut self) -> HeapHandle {
        self.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(self.object_proto),
        ))
    }

    /// Allocates an Array object with `Array.prototype`.
    pub fn alloc_array(&mut self) -> HeapHandle {
        self.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Array,
            Some(self.array_proto),
        ))
    }

    /// Loads a [`CompiledProgram`] into the VM, pre-interning constant strings and
    /// allocating per-function inline cache tables.
    pub fn load_program(&mut self, program: CompiledProgram) -> u32 {
        let prog_id = self.programs.len() as u32;
        let mut func_ics = Vec::with_capacity(program.functions.len());
        let mut func_keys = Vec::with_capacity(program.functions.len());
        let mut calls = vec![0u32; program.functions.len()];
        let mut edges = vec![0u32; program.functions.len()];
        for func in &program.functions {
            func_ics.push(IcTable::with_len(func.ic_slots.len()));
            let mut keys = Vec::with_capacity(func.constants.len());
            for c in &func.constants {
                match c {
                    ConstantEntry::String(s) => keys.push(Some(self.heap.intern_key(s))),
                    _ => keys.push(None),
                }
            }
            func_keys.push(keys);
        }
        calls.shrink_to_fit();
        edges.shrink_to_fit();
        self.ic_tables.push(func_ics);
        self.const_keys.push(func_keys);
        self.call_counts.push(calls);
        self.back_edge_counts.push(edges);
        self.programs.push(program);
        prog_id
    }

    /// Parses, compiles, and executes an ECMAScript `Script`, returning its completion value.
    pub fn eval_script(&mut self, source: &str) -> Result<JsValue, EvalError> {
        let compiled = compile(source, SourceType::Script)?;
        let val = self.execute_program(compiled)?;
        Ok(val)
    }

    /// Executes a pre-compiled [`CompiledProgram`] entry function.
    pub fn execute_program(&mut self, program: CompiledProgram) -> Result<JsValue, JsException> {
        let global_val = JsValue::from_object(self.global);
        self.execute_program_with_this(program, global_val)
    }

    /// Executes a pre-compiled [`CompiledProgram`] entry function with an explicit `this` receiver.
    pub fn execute_program_with_this(
        &mut self,
        program: CompiledProgram,
        this_val: JsValue,
    ) -> Result<JsValue, JsException> {
        let entry_idx = program.entry_func;
        let prog_id = self.load_program(program);
        let entry_closure = self.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Closure(Box::new(ClosureObject {
                program_id: prog_id,
                func_idx: entry_idx,
                upvalues: Vec::new(),
                bound_this: None,
                super_ctor: None,
                field_init: None,
            })),
            Some(self.function_proto),
        ));
        self.call_value(JsValue::from_object(entry_closure), this_val, &[])
    }

    /// Invokes any callable [`JsValue`] (`Closure`, `NativeFunction`, `BoundFunction`, or `Proxy`).
    pub fn call_value(
        &mut self,
        callee_val: JsValue,
        this_val: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JsException> {
        let Some(callee) = callee_val.as_object() else {
            return Err(self.type_error(format!(
                "{} is not a function",
                self.value_to_rust_string(callee_val)
            )));
        };
        self.call_handle(callee, this_val, args, None, JsValue::UNDEFINED)
    }

    /// Invokes a constructor (`new callee(...args)`).
    pub fn construct_value(
        &mut self,
        callee_val: JsValue,
        args: &[JsValue],
        new_target: JsValue,
    ) -> Result<JsValue, JsException> {
        let Some(callee) = callee_val.as_object() else {
            return Err(self.type_error("Target is not a constructor"));
        };
        if !self.is_constructor_handle(callee) {
            return Err(self.type_error("Target is not a constructor"));
        }
        let nt_handle = match new_target.as_object() {
            Some(nt) => {
                if !self.is_constructor_handle(nt) {
                    return Err(self.type_error("newTarget is not a constructor"));
                }
                nt
            }
            None => return Err(self.type_error("newTarget is not a constructor")),
        };
        if let Some(ObjectKind::Proxy(p)) = self.heap.get_object(callee).map(|o| o.kind.clone()) {
            if p.revoked {
                return Err(self.type_error("Cannot perform 'construct' on a revoked Proxy"));
            }
            let trap = self.get_property_by_key(p.handler, self.key_construct)?;
            if !trap.is_undefined() && !trap.is_null() {
                let arg_arr = self.alloc_array();
                for &a in args {
                    self.heap.object_push_element(arg_arr, a);
                }
                let res = self.call_value(
                    trap,
                    JsValue::from_object(p.handler),
                    &[
                        JsValue::from_object(p.target),
                        JsValue::from_object(arg_arr),
                        new_target,
                    ],
                )?;
                if !res.is_object() {
                    return Err(self.type_error("'construct' trap on Proxy must return an object"));
                }
                return Ok(res);
            }
            return self.construct_value(JsValue::from_object(p.target), args, new_target);
        }

        // Determine instance `[[Prototype]]` from `newTarget.prototype` (fallback `callee.prototype`).
        let proto_val = self.get_property_by_key(nt_handle, self.key_prototype)?;
        let proto_handle = proto_val
            .as_object()
            .or_else(|| {
                self.get_property_by_key(callee, self.key_prototype)
                    .ok()
                    .and_then(|v| v.as_object())
            })
            .unwrap_or(self.object_proto);
        let instance = self.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(proto_handle),
        ));
        if let Some(p_obj) = self.heap.get_object_mut(proto_handle) {
            p_obj.is_prototype = true;
        }

        let res = self.call_handle(
            callee,
            JsValue::from_object(instance),
            args,
            Some(instance),
            new_target,
        )?;
        if let Some(res_h) = res.as_object() {
            if nt_handle != callee
                && matches!(
                    self.heap.get_object(callee).map(|o| &o.kind),
                    Some(ObjectKind::NativeFunction(_))
                )
            {
                if let Some(nt_proto) = proto_val.as_object() {
                    if let Some(o) = self.heap.get_object_mut(res_h) {
                        o.prototype = Some(nt_proto);
                    }
                }
            }
            Ok(res)
        } else {
            Ok(JsValue::from_object(instance))
        }
    }

    fn call_handle(
        &mut self,
        callee: HeapHandle,
        this_val: JsValue,
        args: &[JsValue],
        construct_obj: Option<HeapHandle>,
        new_target: JsValue,
    ) -> Result<JsValue, JsException> {
        const MAX_NATIVE_CALL_DEPTH: u32 = 160;
        if self.native_call_depth >= MAX_NATIVE_CALL_DEPTH {
            return Err(self.range_error("Maximum call stack size exceeded"));
        }
        self.native_call_depth += 1;
        let res = self.call_handle_inner(callee, this_val, args, construct_obj, new_target);
        self.native_call_depth -= 1;
        res
    }

    fn call_handle_inner(
        &mut self,
        callee: HeapHandle,
        this_val: JsValue,
        args: &[JsValue],
        construct_obj: Option<HeapHandle>,
        new_target: JsValue,
    ) -> Result<JsValue, JsException> {
        let (kind, class_init) = self
            .heap
            .get_object(callee)
            .map(|o| {
                (
                    o.kind.clone(),
                    o.rare.as_ref().and_then(|r| r.class_instance_init),
                )
            })
            .ok_or_else(|| self.type_error("Attempted to call dead object handle"))?;

        if let Some(init) = class_init {
            let Some(inst_h) = construct_obj else {
                return Err(self.type_error("Class constructor cannot be invoked without 'new'"));
            };
            if !init.is_derived {
                self.init_class_instance(callee, inst_h)?;
            }
        }

        match kind {
            ObjectKind::NativeFunction(id) => {
                // Execute native Rust callback cleanly via temporary borrow split.
                let callable = match self.native_fns.get(id as usize) {
                    Some(NativeCallable::Static(fp)) => NativeInv::Static(*fp),
                    Some(NativeCallable::Closure(rc)) => NativeInv::Closure(Rc::clone(rc)),
                    None => return Err(self.type_error("Invalid native function ID")),
                };
                let prev_nt = self.native_new_target;
                self.native_new_target = if construct_obj.is_some() {
                    new_target
                } else {
                    JsValue::UNDEFINED
                };
                let res = match callable {
                    NativeInv::Static(fp) => fp(self, this_val, args),
                    NativeInv::Closure(rc) => rc(self, this_val, args),
                };
                self.native_new_target = prev_nt;
                res
            }
            ObjectKind::BoundFunction(bound) => {
                let mut combined = Vec::with_capacity(bound.bound_args.len() + args.len());
                combined.extend_from_slice(&bound.bound_args);
                combined.extend_from_slice(args);
                self.call_handle(
                    bound.target,
                    bound.bound_this,
                    &combined,
                    construct_obj,
                    new_target,
                )
            }
            ObjectKind::Proxy(p) => {
                if p.revoked {
                    return Err(self.type_error("Cannot perform 'apply' on a revoked Proxy"));
                }
                let trap = self.get_property_by_key(p.handler, self.key_apply)?;
                if !trap.is_undefined() && !trap.is_null() {
                    let arg_arr = self.alloc_array();
                    for &a in args {
                        self.heap.object_push_element(arg_arr, a);
                    }
                    return self.call_value(
                        trap,
                        JsValue::from_object(p.handler),
                        &[
                            JsValue::from_object(p.target),
                            this_val,
                            JsValue::from_object(arg_arr),
                        ],
                    );
                }
                self.call_handle(p.target, this_val, args, construct_obj, new_target)
            }
            ObjectKind::Closure(closure) => {
                let (is_strict, is_async, is_generator) = {
                    let f = &self.programs[closure.program_id as usize].functions
                        [closure.func_idx as usize];
                    (f.is_strict, f.is_async, f.is_generator)
                };
                let effective_this = closure.bound_this.unwrap_or_else(|| {
                    if !is_strict && (this_val.is_undefined() || this_val.is_null()) {
                        JsValue::from_object(self.global)
                    } else {
                        this_val
                    }
                });
                if is_generator && is_async {
                    let gen_h = crate::promise::create_async_generator(
                        self,
                        callee,
                        closure.program_id,
                        closure.func_idx,
                        effective_this,
                        args,
                    )?;
                    return Ok(JsValue::from_object(gen_h));
                }
                if is_generator {
                    let gen_h = crate::promise::create_generator(
                        self,
                        callee,
                        closure.program_id,
                        closure.func_idx,
                        effective_this,
                        args,
                    )?;
                    return Ok(JsValue::from_object(gen_h));
                }
                if is_async {
                    let prom_h = crate::promise::start_async_call(
                        self,
                        callee,
                        closure.program_id,
                        closure.func_idx,
                        effective_this,
                        args,
                    );
                    return Ok(JsValue::from_object(prom_h));
                }
                let entry_depth = self.frames.len();
                self.push_closure_frame(
                    callee,
                    closure.program_id,
                    closure.func_idx,
                    effective_this,
                    args,
                    None,
                    construct_obj,
                    new_target,
                )?;
                self.run_loop(entry_depth)
            }
            _ => Err(self.type_error("Object is not callable")),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn push_closure_frame(
        &mut self,
        callee: HeapHandle,
        program_id: u32,
        func_idx: u32,
        this_val: JsValue,
        args: &[JsValue],
        ret_dst: Option<usize>,
        construct_obj: Option<HeapHandle>,
        new_target: JsValue,
    ) -> Result<(), JsException> {
        if self.frames.len() >= 1024 {
            return Err(self.range_error("Maximum call stack size exceeded"));
        }
        let (reg_count, param_count) = {
            let func = &self.programs[program_id as usize].functions[func_idx as usize];
            (func.register_count as usize, func.param_count as usize)
        };
        if let Some(counts) = self.call_counts.get_mut(program_id as usize) {
            if let Some(c) = counts.get_mut(func_idx as usize) {
                *c = c.saturating_add(1);
            }
        }
        let base_reg = self.registers.len();
        self.registers.resize(
            base_reg + reg_count.max(param_count + 1),
            JsValue::UNDEFINED,
        );
        let copy_len = args.len().min(param_count);
        for (i, &arg) in args.iter().take(copy_len).enumerate() {
            self.registers[base_reg + i] = arg;
        }
        // Stash extra args beyond `param_count` for `CollectRestArgs` if needed.
        if args.len() > param_count {
            let extra_start = base_reg + reg_count.max(param_count + 1);
            self.registers
                .resize(extra_start + (args.len() - param_count), JsValue::UNDEFINED);
            for (i, &arg) in args[param_count..].iter().enumerate() {
                self.registers[extra_start + i] = arg;
            }
        }
        self.frames.push(CallFrame {
            program_id,
            func_idx,
            base_reg,
            pc: 0,
            this_val,
            callee,
            ret_dst,
            construct_obj,
            new_target,
            call_args: args.to_vec(),
            arguments_obj: None,
        });
        Ok(())
    }

    pub(crate) fn frame_depth(&self) -> usize {
        self.frames.len()
    }

    pub(crate) fn restore_suspended_frame(&mut self, frame: &SuspendedFrame) -> usize {
        let base_reg = self.registers.len();
        self.registers.extend_from_slice(&frame.saved_regs);
        // Re-attach suspended open upvalue cells and sync any mutations made while suspended.
        for &(rel_reg, cell) in &frame.saved_upvalues {
            let abs_reg = base_reg + rel_reg;
            if let Some(val) = self
                .heap
                .get_object(cell)
                .and_then(|o| o.slots.first().copied())
            {
                if abs_reg < self.registers.len() {
                    self.registers[abs_reg] = val;
                }
            }
            self.open_upvalues.push((abs_reg, cell));
        }
        self.frames.push(CallFrame {
            program_id: frame.program_id,
            func_idx: frame.func_idx,
            base_reg,
            pc: frame.pc,
            this_val: frame.this_val,
            callee: frame.callee,
            ret_dst: None,
            construct_obj: None,
            new_target: JsValue::UNDEFINED,
            call_args: frame.call_args.clone(),
            arguments_obj: None,
        });
        base_reg
    }

    pub(crate) fn write_register(&mut self, abs_reg: usize, val: JsValue) {
        if abs_reg < self.registers.len() {
            self.registers[abs_reg] = val;
        }
    }

    pub(crate) fn discard_frames_to(&mut self, depth: usize) {
        while self.frames.len() > depth {
            if let Some(popped) = self.frames.pop() {
                self.close_upvalues_from(popped.base_reg);
                self.registers.truncate(popped.base_reg);
            }
        }
    }

    pub(crate) fn unwind_into_frame(&mut self, stop_depth: usize, exc: JsException) -> bool {
        self.handle_exception(exc, stop_depth).is_ok()
    }

    pub(crate) fn format_exception_public(&self, val: JsValue) -> String {
        self.format_exception_value(val)
    }

    pub(crate) fn create_iterator_public(
        &mut self,
        iterable: JsValue,
    ) -> Result<HeapHandle, JsException> {
        self.create_iterator(iterable)
    }

    pub(crate) fn step_iterator_public(
        &mut self,
        iter_val: JsValue,
    ) -> Result<(JsValue, bool), JsException> {
        self.step_iterator_ex(iter_val, true)
    }

    pub(crate) fn create_async_iterator_public(
        &mut self,
        iterable: JsValue,
    ) -> Result<HeapHandle, JsException> {
        self.create_async_iterator(iterable)
    }

    pub(crate) fn step_async_iterator_public(
        &mut self,
        iter_val: JsValue,
    ) -> Result<JsValue, JsException> {
        self.step_async_iterator(iter_val)
    }

    fn suspend_top_frame(&mut self, resume_dst: u8) -> SuspendedFrame {
        let top = self.frames.pop().expect("active frame to suspend");
        // Snapshot open upvalues in this frame so closures see current values while suspended
        // and re-attach seamlessly when the frame resumes.
        let mut saved_upvalues = Vec::new();
        let mut i = 0;
        while i < self.open_upvalues.len() {
            let (abs_reg, cell) = self.open_upvalues[i];
            if abs_reg >= top.base_reg {
                let val = self
                    .registers
                    .get(abs_reg)
                    .copied()
                    .unwrap_or(JsValue::UNDEFINED);
                self.heap.object_set_slot(cell, 0, val);
                saved_upvalues.push((abs_reg - top.base_reg, cell));
                self.open_upvalues.swap_remove(i);
            } else {
                i += 1;
            }
        }
        let saved_regs = self.registers.split_off(top.base_reg);
        SuspendedFrame {
            program_id: top.program_id,
            func_idx: top.func_idx,
            pc: top.pc,
            this_val: top.this_val,
            callee: top.callee,
            saved_regs,
            call_args: top.call_args,
            saved_upvalues,
            resume_dst,
        }
    }

    /// Core register-window bytecode interpreter dispatch loop (synchronous entry).
    fn run_loop(&mut self, stop_depth: usize) -> Result<JsValue, JsException> {
        match self.run_suspendable(stop_depth)? {
            RunOutcome::Completed(val) => Ok(val),
            RunOutcome::SuspendedAwait { .. }
            | RunOutcome::SuspendedYield { .. }
            | RunOutcome::SuspendedYieldStar { .. }
            | RunOutcome::SuspendedInitial { .. } => {
                Err(self.type_error("Unexpected suspension outside async or generator context"))
            }
        }
    }

    /// Attaches a Tier-2/Tier-3 [`JitEngine`] to this VM realm.
    pub fn attach_jit(&mut self, jit: Rc<dyn JitEngine>) {
        self.jit = Some(jit);
    }

    /// Detaches any active [`JitEngine`], returning to pure Tier-1 interpreter mode.
    pub fn detach_jit(&mut self) {
        self.jit = None;
    }

    /// Core register-window interpreter loop supporting `await`, `yield`, and JIT/OSR tier-up.
    pub(crate) fn run_suspendable(&mut self, stop_depth: usize) -> Result<RunOutcome, JsException> {
        let mut skip_jit_once = false;
        loop {
            if self.frames.len() <= stop_depth {
                return Ok(RunOutcome::Completed(JsValue::UNDEFINED));
            }
            let frame_idx = self.frames.len() - 1;
            let (prog_id, func_idx, base, pc) = {
                let f = &self.frames[frame_idx];
                (f.program_id as usize, f.func_idx as usize, f.base_reg, f.pc)
            };

            // Tier-2/3 JIT function entry hook when pc == 0 and function call count reached threshold.
            if pc == 0 && !skip_jit_once {
                if let Some(jit) = self.jit.clone() {
                    let call_cnt = self
                        .call_counts
                        .get(prog_id)
                        .and_then(|v| v.get(func_idx))
                        .copied()
                        .unwrap_or(0);
                    let back_cnt = self
                        .back_edge_counts
                        .get(prog_id)
                        .and_then(|v| v.get(func_idx))
                        .copied()
                        .unwrap_or(0);
                    if call_cnt >= jit.baseline_threshold() || back_cnt >= jit.osr_threshold() {
                        match jit.try_enter(
                            self,
                            prog_id as u32,
                            func_idx as u32,
                            0,
                            call_cnt,
                            back_cnt,
                        ) {
                            JitOutcome::Returned(val) => {
                                if let Some(top_ret) = self.pop_frame_with_value(val, stop_depth) {
                                    return Ok(RunOutcome::Completed(top_ret));
                                }
                                continue;
                            }
                            JitOutcome::Threw(exc) => {
                                self.handle_exception(exc, stop_depth)?;
                                continue;
                            }
                            JitOutcome::Deopt { resume_pc } => {
                                self.frames[frame_idx].pc = resume_pc;
                                skip_jit_once = true;
                                continue;
                            }
                            JitOutcome::Fallback => {}
                        }
                    }
                }
            }
            skip_jit_once = false;

            let pc = self.frames[frame_idx].pc;
            let inst = match self.programs[prog_id].functions[func_idx].code.get(pc) {
                Some(&i) => i,
                None => {
                    // Implicit return undefined.
                    if let Some(ret) = self.pop_frame_with_value(JsValue::UNDEFINED, stop_depth) {
                        return Ok(RunOutcome::Completed(ret));
                    }
                    continue;
                }
            };
            self.frames[frame_idx].pc = pc + 1;

            let Some(op) = inst.opcode() else {
                let err = self.type_error("Invalid bytecode opcode");
                self.handle_exception(err, stop_depth)?;
                continue;
            };

            let step_res =
                self.dispatch_op(op, inst, prog_id, func_idx, base, frame_idx, stop_depth);
            match step_res {
                Ok(StepOutcome::Continue) => {}
                Ok(StepOutcome::ReturnTop(val)) => return Ok(RunOutcome::Completed(val)),
                Ok(StepOutcome::SuspendAwait { dst, awaited }) => {
                    let frame = self.suspend_top_frame(dst);
                    return Ok(RunOutcome::SuspendedAwait { frame, awaited });
                }
                Ok(StepOutcome::SuspendYield { dst, yielded }) => {
                    let frame = self.suspend_top_frame(dst);
                    return Ok(RunOutcome::SuspendedYield { frame, yielded });
                }
                Ok(StepOutcome::SuspendYieldStar { dst, iterable }) => {
                    let frame = self.suspend_top_frame(dst);
                    return Ok(RunOutcome::SuspendedYieldStar { frame, iterable });
                }
                Ok(StepOutcome::SuspendInitial) => {
                    let frame = self.suspend_top_frame(0);
                    return Ok(RunOutcome::SuspendedInitial { frame });
                }
                Err(exc) => {
                    self.handle_exception(exc, stop_depth)?;
                }
            }
        }
    }

    fn read_wide_index(&mut self, prog_id: usize, func_idx: usize, frame_idx: usize) -> u32 {
        let next_pc = self.frames[frame_idx].pc;
        let extra = self.programs[prog_id].functions[func_idx].code[next_pc];
        self.frames[frame_idx].pc = next_pc + 1;
        extra.ax_val()
    }

    #[inline]
    fn resolve_ic_idx(
        &mut self,
        inst: Instruction,
        prog_id: usize,
        func_idx: usize,
        frame_idx: usize,
    ) -> usize {
        let c = inst.c();
        if c == WIDE_IC_SENTINEL {
            self.read_wide_index(prog_id, func_idx, frame_idx) as usize
        } else {
            usize::from(c)
        }
    }

    #[inline]
    fn resolve_abx_idx(
        &mut self,
        inst: Instruction,
        prog_id: usize,
        func_idx: usize,
        frame_idx: usize,
    ) -> usize {
        let bx = inst.bx();
        if bx == u16::MAX {
            self.read_wide_index(prog_id, func_idx, frame_idx) as usize
        } else {
            usize::from(bx)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch_op(
        &mut self,
        op: Opcode,
        inst: Instruction,
        prog_id: usize,
        func_idx: usize,
        base: usize,
        frame_idx: usize,
        stop_depth: usize,
    ) -> Result<StepOutcome, JsException> {
        let a = base + usize::from(inst.a());
        match op {
            Opcode::Nop | Opcode::ExtraArg | Opcode::Debugger => {}
            Opcode::Move => {
                let b = base + usize::from(inst.b());
                self.registers[a] = self.registers[b];
            }
            Opcode::LoadUndef => self.registers[a] = JsValue::UNDEFINED,
            Opcode::LoadNull => self.registers[a] = JsValue::NULL,
            Opcode::LoadTrue => self.registers[a] = JsValue::TRUE,
            Opcode::LoadFalse => self.registers[a] = JsValue::FALSE,
            Opcode::LoadTdz => self.registers[a] = JsValue::EMPTY,
            Opcode::CheckTdz => {
                let name_idx = self.resolve_abx_idx(inst, prog_id, func_idx, frame_idx);
                if self.registers[a].is_empty() {
                    let name = self.const_string(prog_id, func_idx, name_idx);
                    return Err(self
                        .reference_error(format!("Cannot access '{name}' before initialization")));
                }
            }
            Opcode::LoadInt => {
                self.registers[a] = JsValue::from_i32(i32::from(inst.sbx()));
            }
            Opcode::LoadConst => {
                let idx = self.resolve_abx_idx(inst, prog_id, func_idx, frame_idx);
                let val = self.materialize_constant(prog_id, func_idx, idx)?;
                self.registers[a] = val;
            }
            Opcode::LoadThis => {
                self.registers[a] = self.frames[frame_idx].this_val;
            }
            Opcode::LoadNewTarget => {
                self.registers[a] = self.frames[frame_idx].new_target;
            }
            Opcode::LoadImportMeta => {
                let meta = self.alloc_ordinary_object();
                self.registers[a] = JsValue::from_object(meta);
            }
            Opcode::GetGlobal => {
                let ic_idx = self.resolve_abx_idx(inst, prog_id, func_idx, frame_idx);
                let (key, name) = self.ic_key_and_name(prog_id, func_idx, ic_idx);
                // Check `with` scope chain first if active.
                for i in (0..self.with_stack.len()).rev() {
                    let with_obj = self.with_stack[i];
                    if self.has_property_by_key(with_obj, key)? {
                        let v = self.get_property_by_key(with_obj, key)?;
                        self.registers[a] = v;
                        return Ok(StepOutcome::Continue);
                    }
                }
                let val = self.get_global_cached(prog_id, func_idx, ic_idx, key, &name)?;
                self.registers[a] = val;
            }
            Opcode::TypeOfGlobal => {
                let ic_idx = self.resolve_abx_idx(inst, prog_id, func_idx, frame_idx);
                let key = self.ic_key(prog_id, func_idx, ic_idx);
                let ty = if self.has_property_by_key(self.global, key)? {
                    let v = self.get_property_by_key(self.global, key)?;
                    self.typeof_value(v)
                } else {
                    "undefined"
                };
                self.registers[a] = JsValue::from_string(self.heap.alloc_string(ty));
            }
            Opcode::SetGlobal => {
                let ic_idx = self.resolve_abx_idx(inst, prog_id, func_idx, frame_idx);
                let (key, name) = self.ic_key_and_name(prog_id, func_idx, ic_idx);
                let val = self.registers[a];
                for i in (0..self.with_stack.len()).rev() {
                    let with_obj = self.with_stack[i];
                    if self.has_property_by_key(with_obj, key)? {
                        self.set_property_by_key(with_obj, key, val)?;
                        return Ok(StepOutcome::Continue);
                    }
                }
                let is_strict = self.programs[prog_id].functions[func_idx].is_strict;
                if is_strict && !self.has_property_by_key(self.global, key)? {
                    return Err(self.reference_error(format!("{name} is not defined")));
                }
                self.set_prop_cached(self.global, prog_id, func_idx, ic_idx, key, val)?;
            }
            Opcode::DeclareGlobal => {
                let name_idx = self.resolve_abx_idx(inst, prog_id, func_idx, frame_idx);
                let key = self.const_key(prog_id, func_idx, name_idx);
                let val = self.registers[a];
                let attrs = if self.programs[prog_id].functions[func_idx].name.as_deref()
                    == Some("<script>")
                {
                    PropertyAttrs::WRITABLE | PropertyAttrs::ENUMERABLE
                } else {
                    PropertyAttrs::DEFAULT
                };
                if val.is_empty() {
                    if self
                        .heap
                        .object_get_own(self.global, key, &self.shapes)
                        .is_none()
                    {
                        self.heap.object_define_data(
                            self.global,
                            key,
                            JsValue::UNDEFINED,
                            attrs,
                            &mut self.shapes,
                        );
                    }
                } else {
                    self.heap
                        .object_define_data(self.global, key, val, attrs, &mut self.shapes);
                }
            }
            Opcode::GetUpvalue => {
                let uv_idx = usize::from(inst.bx());
                let cell = self.frame_upvalue(frame_idx, uv_idx)?;
                let val = self.read_upvalue_cell(cell);
                if val.is_empty() {
                    return Err(self.reference_error(
                        "Cannot access lexical declaration before initialization",
                    ));
                }
                self.registers[a] = val;
            }
            Opcode::SetUpvalue => {
                let uv_idx = usize::from(inst.bx());
                let cell = self.frame_upvalue(frame_idx, uv_idx)?;
                if self.read_upvalue_cell(cell).is_empty() {
                    return Err(self.reference_error(
                        "Cannot access lexical declaration before initialization",
                    ));
                }
                let val = self.registers[a];
                self.write_upvalue_cell(cell, val);
            }
            Opcode::CloseUpvalues => {
                self.close_upvalues_from(a);
            }
            Opcode::NewClosure => {
                let const_idx = self.resolve_abx_idx(inst, prog_id, func_idx, frame_idx);
                let child_fn_idx = match self.programs[prog_id].functions[func_idx]
                    .constants
                    .get(const_idx)
                {
                    Some(ConstantEntry::Function(idx)) => *idx,
                    _ => return Err(self.type_error("Expected Function constant")),
                };
                let (is_arrow, is_async, is_generator, is_method, fn_length, fn_name) = {
                    let f = &self.programs[prog_id].functions[child_fn_idx as usize];
                    (
                        f.is_arrow,
                        f.is_async,
                        f.is_generator,
                        f.is_method,
                        f.length,
                        f.name.clone().unwrap_or_default(),
                    )
                };
                let captured =
                    self.capture_function_upvalues(prog_id, child_fn_idx, frame_idx, base)?;
                let bound_this = is_arrow.then_some(self.frames[frame_idx].this_val);
                let is_ctor = !is_arrow && !is_async && !is_generator && !is_method;
                let mut js_obj = JsObject::with_kind(
                    ObjectKind::Closure(Box::new(ClosureObject {
                        program_id: prog_id as u32,
                        func_idx: child_fn_idx,
                        upvalues: captured,
                        bound_this,
                        super_ctor: None,
                        field_init: None,
                    })),
                    Some(self.function_proto),
                );
                js_obj.is_constructor = is_ctor;
                let closure_obj = self.heap.alloc_object(js_obj);
                self.heap.object_define_data(
                    closure_obj,
                    self.key_length,
                    JsValue::from_i32(i32::from(fn_length)),
                    PropertyAttrs::CONFIGURABLE,
                    &mut self.shapes,
                );
                let name_val = JsValue::from_string(self.heap.alloc_string(fn_name));
                self.heap.object_define_data(
                    closure_obj,
                    self.key_name,
                    name_val,
                    PropertyAttrs::CONFIGURABLE,
                    &mut self.shapes,
                );
                if is_ctor {
                    let proto_obj = self.alloc_ordinary_object();
                    self.heap.get_object_mut(proto_obj).unwrap().is_prototype = true;
                    self.heap.object_define_data(
                        proto_obj,
                        self.key_constructor,
                        JsValue::from_object(closure_obj),
                        PropertyAttrs::METHOD,
                        &mut self.shapes,
                    );
                    self.heap.object_define_data(
                        closure_obj,
                        self.key_prototype,
                        JsValue::from_object(proto_obj),
                        PropertyAttrs::WRITABLE,
                        &mut self.shapes,
                    );
                } else if is_generator {
                    let parent_gp = if is_async {
                        self.async_generator_proto
                    } else {
                        self.generator_proto
                    };
                    let proto_obj = self
                        .heap
                        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(parent_gp)));
                    self.heap.get_object_mut(proto_obj).unwrap().is_prototype = true;
                    self.heap.object_define_data(
                        closure_obj,
                        self.key_prototype,
                        JsValue::from_object(proto_obj),
                        PropertyAttrs::WRITABLE,
                        &mut self.shapes,
                    );
                }
                self.registers[a] = JsValue::from_object(closure_obj);
            }
            Opcode::NewObject => {
                let obj = self.alloc_ordinary_object();
                self.registers[a] = JsValue::from_object(obj);
            }
            Opcode::NewArray => {
                let elem_base = base + usize::from(inst.b());
                let count = usize::from(inst.c());
                let arr = self.alloc_array();
                for i in 0..count {
                    let v = self.registers[elem_base + i];
                    self.heap.object_push_element(arr, v);
                }
                self.registers[a] = JsValue::from_object(arr);
            }
            Opcode::ArrayPush => {
                let val = self.registers[base + usize::from(inst.b())];
                if let Some(arr) = self.registers[a].as_object() {
                    self.heap.object_push_element(arr, val);
                }
            }
            Opcode::ArrayPushHole => {
                if let Some(arr) = self.registers[a].as_object() {
                    if let Some(obj) = self.heap.get_object_mut(arr) {
                        obj.elements.push_hole();
                    }
                }
            }
            Opcode::ArrayAppendIterable => {
                let iter_src = self.registers[base + usize::from(inst.b())];
                if let Some(arr) = self.registers[a].as_object() {
                    let items = self.collect_iterable(iter_src)?;
                    for item in items {
                        self.heap.object_push_element(arr, item);
                    }
                }
            }
            Opcode::ObjectCopyProps => {
                let src_val = self.registers[base + usize::from(inst.b())];
                let excl_val = self.registers[base + usize::from(inst.c())];
                if let (Some(dst_h), Some(src_h)) =
                    (self.registers[a].as_object(), src_val.as_object())
                {
                    let mut excluded: Vec<PropertyKey> = Vec::new();
                    if let Some(ex_h) = excl_val.as_object() {
                        let len = self.heap.get_object(ex_h).map_or(0, |o| o.elements.len());
                        for i in 0..len {
                            if let Some(kv) =
                                self.heap.get_object(ex_h).and_then(|o| o.elements.get(i))
                            {
                                let s = self.value_to_rust_string(kv);
                                excluded.push(self.heap.intern_key(&s));
                            }
                        }
                    }
                    let keys = self.own_enumerable_keys(src_h);
                    for k in keys {
                        if !excluded.contains(&k) {
                            let val = self.get_property_by_key(src_h, k)?;
                            self.heap.object_define_data(
                                dst_h,
                                k,
                                val,
                                PropertyAttrs::DEFAULT,
                                &mut self.shapes,
                            );
                        }
                    }
                }
            }
            Opcode::NewRegExp => {
                let idx = self.resolve_abx_idx(inst, prog_id, func_idx, frame_idx);
                let (pattern, flags) = match self.programs[prog_id].functions[func_idx]
                    .constants
                    .get(idx)
                {
                    Some(ConstantEntry::RegExp { pattern, flags }) => {
                        (pattern.clone(), flags.clone())
                    }
                    _ => return Err(self.type_error("Expected RegExp constant")),
                };
                let re = self.alloc_regexp(pattern, flags);
                self.registers[a] = JsValue::from_object(re);
            }
            Opcode::CreateClass => {
                let super_val = self.registers[base + usize::from(inst.b())];
                let const_idx = self.resolve_ic_idx(inst, prog_id, func_idx, frame_idx);
                let cls_obj = self
                    .instantiate_class(prog_id, func_idx, const_idx, super_val, frame_idx, base)?;
                self.registers[a] = JsValue::from_object(cls_obj);
            }
            Opcode::GetProp => {
                let obj_val = self.registers[base + usize::from(inst.b())];
                let ic_idx = self.resolve_ic_idx(inst, prog_id, func_idx, frame_idx);
                let key = self.ic_key(prog_id, func_idx, ic_idx);
                let val = self.get_prop_cached(obj_val, prog_id, func_idx, ic_idx, key)?;
                self.registers[a] = val;
            }
            Opcode::SetProp => {
                let obj_val = self.registers[a];
                let val = self.registers[base + usize::from(inst.b())];
                let ic_idx = self.resolve_ic_idx(inst, prog_id, func_idx, frame_idx);
                let key = self.ic_key(prog_id, func_idx, ic_idx);
                let is_strict = self.programs[prog_id].functions[func_idx].is_strict;
                let Some(obj) = obj_val.as_object() else {
                    if obj_val.is_null() || obj_val.is_undefined() || is_strict {
                        return Err(self.type_error("Cannot set property on non-object"));
                    }
                    return Ok(StepOutcome::Continue);
                };
                self.set_prop_cached(obj, prog_id, func_idx, ic_idx, key, val)?;
            }
            Opcode::DefineProp => {
                let obj_val = self.registers[a];
                let val = self.registers[base + usize::from(inst.b())];
                let ic_idx = self.resolve_ic_idx(inst, prog_id, func_idx, frame_idx);
                let key = self.ic_key(prog_id, func_idx, ic_idx);
                let Some(obj) = obj_val.as_object() else {
                    return Err(self.type_error("Cannot define property on non-object"));
                };
                self.define_prop_cached(obj, prog_id, func_idx, ic_idx, key, val)?;
            }
            Opcode::SetProto => {
                let obj_val = self.registers[a];
                let proto_val = self.registers[base + usize::from(inst.b())];
                if let Some(obj) = obj_val.as_object() {
                    if proto_val.is_null() {
                        self.heap.object_set_prototype(obj, None, &mut self.shapes);
                        self.mega_ic.invalidate_prototypes();
                    } else if let Some(ph) = proto_val.as_object() {
                        self.heap
                            .object_set_prototype(obj, Some(ph), &mut self.shapes);
                        self.mega_ic.invalidate_prototypes();
                    }
                }
            }
            Opcode::GetElem => {
                let obj_val = self.registers[base + usize::from(inst.b())];
                let key_val = self.registers[base + usize::from(inst.c())];
                if self.jit.is_some() {
                    if let Some(obj) = obj_val.as_object() {
                        if let Some(o) = self.heap.get_object(obj) {
                            if matches!(o.kind, ObjectKind::Array) {
                                let epc = self.frames[frame_idx].pc.saturating_sub(1);
                                self.elem_feedback.insert(
                                    (prog_id as u32, func_idx as u32, epc),
                                    o.elements.kind(),
                                );
                            }
                        }
                    }
                }
                let val = self.get_elem(obj_val, key_val)?;
                self.registers[a] = val;
            }
            Opcode::SetElem => {
                let obj_val = self.registers[a];
                let key_val = self.registers[base + usize::from(inst.b())];
                let val = self.registers[base + usize::from(inst.c())];
                let is_strict = self.programs[prog_id].functions[func_idx].is_strict;
                if self.jit.is_some() {
                    if let Some(obj) = obj_val.as_object() {
                        if let Some(o) = self.heap.get_object(obj) {
                            if matches!(o.kind, ObjectKind::Array) {
                                let epc = self.frames[frame_idx].pc.saturating_sub(1);
                                self.elem_feedback.insert(
                                    (prog_id as u32, func_idx as u32, epc),
                                    o.elements.kind(),
                                );
                            }
                        }
                    }
                }
                if obj_val.is_null() || obj_val.is_undefined() {
                    return Err(self.type_error("Cannot set element on null or undefined"));
                }
                if !obj_val.is_object() {
                    if is_strict {
                        return Err(self.type_error("Cannot set property on primitive value"));
                    }
                    return Ok(StepOutcome::Continue);
                }
                let ok = self.set_elem_checked(obj_val, key_val, val)?;
                if !ok && is_strict {
                    return Err(
                        self.type_error("Cannot assign to read-only or non-extensible property")
                    );
                }
            }
            Opcode::DeleteProp | Opcode::DeleteElem => {
                let obj_val = self.registers[base + usize::from(inst.b())];
                let key_val = self.registers[base + usize::from(inst.c())];
                if obj_val.is_null() || obj_val.is_undefined() {
                    return Err(self.type_error("Cannot delete property of null or undefined"));
                }
                let deleted = self.delete_elem(obj_val, key_val)?;
                let is_strict = self.programs[prog_id].functions[func_idx].is_strict;
                if !deleted && is_strict {
                    return Err(self.type_error("Cannot delete non-configurable property"));
                }
                self.registers[a] = JsValue::from_bool(deleted);
            }
            Opcode::DefineGetter | Opcode::DefineSetter => {
                let key_val = self.registers[base + usize::from(inst.b())];
                let fn_val = self.registers[base + usize::from(inst.c())];
                let s = self.to_property_key(key_val)?;
                if let (Some(obj), Some(fn_h)) = (self.registers[a].as_object(), fn_val.as_object())
                {
                    let key = self.heap.intern_key(&s);
                    let prefix = if op == Opcode::DefineGetter {
                        "get "
                    } else {
                        "set "
                    };
                    self.set_function_name_for_key(fn_h, key_val, &s, prefix);
                    let (g, set) = if op == Opcode::DefineGetter {
                        (Some(fn_h), None)
                    } else {
                        (None, Some(fn_h))
                    };
                    self.heap.object_define_accessor(
                        obj,
                        key,
                        g,
                        set,
                        PropertyAttrs::ENUMERABLE | PropertyAttrs::CONFIGURABLE,
                        &mut self.shapes,
                    );
                }
            }
            Opcode::DefineClassMethod | Opcode::DefineClassGetter | Opcode::DefineClassSetter => {
                let key_val = self.registers[base + usize::from(inst.b())];
                let fn_val = self.registers[base + usize::from(inst.c())];
                let s = self.to_property_key(key_val)?;
                if let (Some(obj), Some(fn_h)) = (self.registers[a].as_object(), fn_val.as_object())
                {
                    let key = self.heap.intern_key(&s);
                    let prefix = match op {
                        Opcode::DefineClassGetter => "get ",
                        Opcode::DefineClassSetter => "set ",
                        _ => "",
                    };
                    self.set_function_name_for_key(fn_h, key_val, &s, prefix);
                    // Propagate class `super_ctor` onto computed class method closures.
                    let sup_ctor = self
                        .heap
                        .object_get_own(obj, self.key_constructor, &self.shapes)
                        .and_then(|p| p.value.as_object())
                        .or(Some(obj))
                        .and_then(|c| self.heap.get_object(c))
                        .and_then(|o| match &o.kind {
                            ObjectKind::Closure(cl) => cl.super_ctor,
                            _ => None,
                        });
                    if let Some(sup) = sup_ctor {
                        if let Some(ObjectKind::Closure(cl)) =
                            self.heap.get_object_mut(fn_h).map(|o| &mut o.kind)
                        {
                            cl.super_ctor = Some(sup);
                        }
                    }
                    match op {
                        Opcode::DefineClassMethod => {
                            self.heap.object_define_data(
                                obj,
                                key,
                                JsValue::from_object(fn_h),
                                PropertyAttrs::METHOD,
                                &mut self.shapes,
                            );
                        }
                        Opcode::DefineClassGetter => {
                            self.heap.object_define_accessor(
                                obj,
                                key,
                                Some(fn_h),
                                None,
                                PropertyAttrs::CONFIGURABLE,
                                &mut self.shapes,
                            );
                        }
                        Opcode::DefineClassSetter => {
                            self.heap.object_define_accessor(
                                obj,
                                key,
                                None,
                                Some(fn_h),
                                PropertyAttrs::CONFIGURABLE,
                                &mut self.shapes,
                            );
                        }
                        _ => unreachable!(),
                    }
                }
            }
            Opcode::GetPrivate => {
                let obj_val = self.registers[base + usize::from(inst.b())];
                let ic_idx = self.resolve_ic_idx(inst, prog_id, func_idx, frame_idx);
                let key = self.ic_key(prog_id, func_idx, ic_idx);
                let Some(obj) = obj_val.as_object() else {
                    return Err(self.type_error("Cannot read private member from non-object"));
                };
                let entry = self
                    .heap
                    .get_object(obj)
                    .and_then(|o| o.rare.as_ref())
                    .and_then(|r| r.private_fields.get(&key).copied())
                    .ok_or_else(|| {
                        self.type_error("Cannot read private member from object without it")
                    })?;
                let val = match entry {
                    PrivateEntry::Field(v) => v,
                    PrivateEntry::Method(h) => JsValue::from_object(h),
                    PrivateEntry::Accessor(Some(g), _) => {
                        self.call_value(JsValue::from_object(g), obj_val, &[])?
                    }
                    PrivateEntry::Accessor(None, _) => {
                        return Err(
                            self.type_error("Cannot read private accessor without a getter")
                        );
                    }
                };
                self.registers[a] = val;
            }
            Opcode::SetPrivate => {
                let obj_val = self.registers[a];
                let val = self.registers[base + usize::from(inst.b())];
                let ic_idx = self.resolve_ic_idx(inst, prog_id, func_idx, frame_idx);
                let key = self.ic_key(prog_id, func_idx, ic_idx);
                let Some(obj) = obj_val.as_object() else {
                    return Err(self.type_error("Cannot write private member on non-object"));
                };
                let entry = self
                    .heap
                    .get_object(obj)
                    .and_then(|o| o.rare.as_ref())
                    .and_then(|r| r.private_fields.get(&key).copied())
                    .ok_or_else(|| {
                        self.type_error("Cannot write private member to object without it")
                    })?;
                match entry {
                    PrivateEntry::Field(_) => {
                        if let Some(o) = self.heap.get_object_mut(obj) {
                            o.rare_mut()
                                .private_fields
                                .insert(key, PrivateEntry::Field(val));
                        }
                        self.heap.write_barrier(obj, val);
                    }
                    PrivateEntry::Method(_) => {
                        return Err(self.type_error("Private method is not writable"));
                    }
                    PrivateEntry::Accessor(_, Some(s)) => {
                        self.call_value(JsValue::from_object(s), obj_val, &[val])?;
                    }
                    PrivateEntry::Accessor(_, None) => {
                        return Err(self.type_error("Private accessor has no setter"));
                    }
                }
            }
            Opcode::DefinePrivate => {
                let obj_val = self.registers[a];
                let val = self.registers[base + usize::from(inst.b())];
                let ic_idx = self.resolve_ic_idx(inst, prog_id, func_idx, frame_idx);
                let key = self.ic_key(prog_id, func_idx, ic_idx);
                let Some(obj) = obj_val.as_object() else {
                    return Err(self.type_error("Cannot define private field on non-object"));
                };
                let already = self
                    .heap
                    .get_object(obj)
                    .and_then(|o| o.rare.as_ref())
                    .is_some_and(|r| r.private_fields.contains_key(&key));
                if already {
                    return Err(
                        self.type_error("Cannot initialize private field twice on the same object")
                    );
                }
                if let Some(o) = self.heap.get_object_mut(obj) {
                    o.rare_mut()
                        .private_fields
                        .insert(key, PrivateEntry::Field(val));
                }
                self.heap.write_barrier(obj, val);
            }
            Opcode::InPrivate => {
                let obj_val = self.registers[base + usize::from(inst.b())];
                let ic_idx = self.resolve_ic_idx(inst, prog_id, func_idx, frame_idx);
                let key = self.ic_key(prog_id, func_idx, ic_idx);
                let Some(obj) = obj_val.as_object() else {
                    return Err(self
                        .type_error("Cannot use 'in' operator with private field on non-object"));
                };
                let present = self
                    .heap
                    .get_object(obj)
                    .and_then(|o| o.rare.as_ref())
                    .is_some_and(|r| r.private_fields.contains_key(&key));
                self.registers[a] = JsValue::from_bool(present);
            }
            Opcode::GetSuperProp => {
                let key_val = self.registers[base + usize::from(inst.b())];
                let this_val = self.frames[frame_idx].this_val;
                let callee_h = self.frames[frame_idx].callee;
                let s = self.value_to_rust_string(key_val);
                let key = self.heap.intern_key(&s);
                let super_ctor = match self.heap.get_object(callee_h).map(|o| &o.kind) {
                    Some(ObjectKind::Closure(c)) => c.super_ctor,
                    _ => None,
                };
                let super_proto = if let Some(sup) = super_ctor {
                    self.get_property_by_key(sup, self.key_prototype)?
                        .as_object()
                } else {
                    this_val
                        .as_object()
                        .and_then(|h| self.heap.get_object(h))
                        .and_then(|o| o.prototype)
                        .and_then(|p| self.heap.get_object(p))
                        .and_then(|po| po.prototype)
                };
                let val = if let Some(sp) = super_proto {
                    self.get_property_with_receiver(sp, key, this_val)?
                } else {
                    JsValue::UNDEFINED
                };
                self.registers[a] = val;
            }
            Opcode::SetSuperProp => {
                let key_val = self.registers[a];
                let val = self.registers[base + usize::from(inst.b())];
                let this_val = self.frames[frame_idx].this_val;
                self.set_elem(this_val, key_val, val)?;
            }
            Opcode::Call => {
                let call_base = base + usize::from(inst.b());
                let argc = usize::from(inst.c());
                let callee = self.registers[call_base];
                if self.jit.is_some() {
                    let call_pc = self.frames[frame_idx].pc.saturating_sub(1);
                    self.record_call_feedback(prog_id as u32, func_idx as u32, call_pc, callee);
                }
                let res = if argc <= 4 {
                    let mut buf = [JsValue::UNDEFINED; 4];
                    for (i, slot) in buf.iter_mut().take(argc).enumerate() {
                        *slot = self.registers[call_base + 1 + i];
                    }
                    self.call_value(callee, JsValue::UNDEFINED, &buf[..argc])?
                } else {
                    let args: Vec<JsValue> = (0..argc)
                        .map(|i| self.registers[call_base + 1 + i])
                        .collect();
                    self.call_value(callee, JsValue::UNDEFINED, &args)?
                };
                self.registers[a] = res;
            }
            Opcode::DirectEval => {
                let call_base = base + usize::from(inst.b());
                let meta_idx = usize::from(inst.c());
                let meta = match self.programs[prog_id].functions[func_idx]
                    .constants
                    .get(meta_idx)
                {
                    Some(ConstantEntry::DirectEval(m)) => m.clone(),
                    _ => return Err(self.type_error("Invalid direct eval metadata")),
                };
                let callee = self.registers[call_base];
                let argc = usize::from(meta.argc);
                let res = if callee.as_object() != self.eval_fn {
                    let args: Vec<JsValue> = (0..argc)
                        .map(|i| self.registers[call_base + 1 + i])
                        .collect();
                    self.call_value(callee, JsValue::UNDEFINED, &args)?
                } else if argc == 0 {
                    JsValue::UNDEFINED
                } else {
                    let first = self.registers[call_base + 1];
                    if let Some(str_h) = first.as_string() {
                        let src = self.heap.materialize_string(str_h).unwrap_or_default();
                        let prog = crate::parser::parse_eval(
                            &src,
                            meta.is_strict,
                            meta.in_class_field_or_static,
                            meta.in_class_body,
                        )
                        .map_err(|e| self.make_error("SyntaxError", e.to_string()))?;
                        let eval_is_strict = meta.is_strict || prog.strict;
                        if !eval_is_strict && !meta.forbidden_vars.is_empty() {
                            let var_names =
                                crate::compiler::collect_eval_var_declared_names(&prog.body);
                            for name in &var_names {
                                if meta.forbidden_vars.contains(name) {
                                    return Err(self.make_error(
                                        "SyntaxError",
                                        format!("Identifier '{name}' has already been declared"),
                                    ));
                                }
                            }
                        }
                        let mode = if eval_is_strict || meta.in_function {
                            crate::compiler::EvalScopeMode::LocalAll
                        } else {
                            crate::compiler::EvalScopeMode::GlobalVars
                        };
                        let compiled = crate::compiler::compile_eval_program(&prog, mode)
                            .map_err(|e| self.make_error("SyntaxError", e.to_string()))?;
                        let caller_this = self.frames[frame_idx].this_val;
                        self.execute_program_with_this(compiled, caller_this)?
                    } else {
                        first
                    }
                };
                self.registers[a] = res;
            }
            Opcode::CallMethod => {
                let call_base = base + usize::from(inst.b());
                let ic_idx = self.resolve_ic_idx(inst, prog_id, func_idx, frame_idx);
                let argc =
                    usize::from(self.programs[prog_id].functions[func_idx].ic_slots[ic_idx].argc);
                let key = self.ic_key(prog_id, func_idx, ic_idx);
                let recv = self.registers[call_base];
                let method = self.get_prop_cached(recv, prog_id, func_idx, ic_idx, key)?;
                let args: Vec<JsValue> = (0..argc)
                    .map(|i| self.registers[call_base + 1 + i])
                    .collect();
                let res = self.call_value(method, recv, &args)?;
                self.registers[a] = res;
            }
            Opcode::CallWithThis => {
                let call_base = base + usize::from(inst.b());
                let argc = usize::from(inst.c());
                let callee = self.registers[call_base];
                let recv = self.registers[call_base + 1];
                let args: Vec<JsValue> = (0..argc)
                    .map(|i| self.registers[call_base + 2 + i])
                    .collect();
                let res = self.call_value(callee, recv, &args)?;
                self.registers[a] = res;
            }
            Opcode::CallSpread => {
                let call_base = base + usize::from(inst.b());
                let arr_val = self.registers[base + usize::from(inst.c())];
                let callee = self.registers[call_base];
                let recv = self.registers[call_base + 1];
                let args = self.collect_array_elements(arr_val);
                let res = self.call_value(callee, recv, &args)?;
                self.registers[a] = res;
            }
            Opcode::Construct => {
                let call_base = base + usize::from(inst.b());
                let argc = usize::from(inst.c());
                let callee = self.registers[call_base];
                let args: Vec<JsValue> = (0..argc)
                    .map(|i| self.registers[call_base + 1 + i])
                    .collect();
                let res = self.construct_value(callee, &args, callee)?;
                self.registers[a] = res;
            }
            Opcode::ConstructSpread => {
                let callee = self.registers[base + usize::from(inst.b())];
                let arr_val = self.registers[base + usize::from(inst.c())];
                let args = self.collect_array_elements(arr_val);
                let res = self.construct_value(callee, &args, callee)?;
                self.registers[a] = res;
            }
            Opcode::SuperConstruct => {
                let call_base = base + usize::from(inst.b());
                let argc = usize::from(inst.c());
                let args: Vec<JsValue> = (0..argc)
                    .map(|i| self.registers[call_base + 1 + i])
                    .collect();
                let callee_h = self.frames[frame_idx].callee;
                let this_val = self.frames[frame_idx].this_val;
                let new_target = self.frames[frame_idx].new_target;
                let super_ctor = match self.heap.get_object(callee_h).map(|o| &o.kind) {
                    Some(ObjectKind::Closure(c)) => c.super_ctor,
                    _ => None,
                };
                let mut actual_this = this_val;
                if let Some(sup) = super_ctor {
                    let sup_res =
                        self.call_handle(sup, this_val, &args, this_val.as_object(), new_target)?;
                    if let Some(res_h) = sup_res.as_object() {
                        if matches!(
                            self.heap.get_object(sup).map(|o| &o.kind),
                            Some(ObjectKind::NativeFunction(_))
                        ) {
                            if let Some(nt_proto) = new_target
                                .as_object()
                                .and_then(|nt| {
                                    self.get_property_by_key(nt, self.key_prototype).ok()
                                })
                                .and_then(|v| v.as_object())
                            {
                                if let Some(o) = self.heap.get_object_mut(res_h) {
                                    o.prototype = Some(nt_proto);
                                }
                            }
                        }
                        actual_this = sup_res;
                        self.frames[frame_idx].this_val = actual_this;
                        self.frames[frame_idx].construct_obj = Some(res_h);
                    }
                }
                if let Some(actual_obj) = actual_this.as_object() {
                    self.init_class_instance(callee_h, actual_obj)?;
                }
                self.registers[a] = actual_this;
            }
            Opcode::CollectRestArgs => {
                let first_idx = usize::from(inst.b());
                let arr = self.alloc_array();
                let (reg_count, param_count) = {
                    let f = &self.programs[prog_id].functions[func_idx];
                    (f.register_count as usize, f.param_count as usize)
                };
                let frame_end = self
                    .frames
                    .get(frame_idx + 1)
                    .map_or(self.registers.len(), |nf| nf.base_reg);
                let extra_start = base + reg_count.max(param_count + 1);
                for idx in extra_start..frame_end {
                    let v = self.registers[idx];
                    self.heap.object_push_element(arr, v);
                }
                let _ = first_idx;
                self.registers[a] = JsValue::from_object(arr);
            }
            Opcode::LoadArguments => {
                let args_obj = if let Some(cached) = self.frames[frame_idx].arguments_obj {
                    cached
                } else {
                    let call_args = self.frames[frame_idx].call_args.clone();
                    let obj = self.alloc_ordinary_object();
                    for (i, &val) in call_args.iter().enumerate() {
                        if let Some(o) = self.heap.get_object_mut(obj) {
                            o.elements.set(i as u32, val);
                        }
                        self.heap.write_barrier(obj, val);
                    }
                    self.heap.object_define_data(
                        obj,
                        self.key_length,
                        JsValue::from_i32(call_args.len() as i32),
                        PropertyAttrs::WRITABLE | PropertyAttrs::CONFIGURABLE,
                        &mut self.shapes,
                    );
                    let sym_iter_key = self.key_symbol_iterator;
                    if let Ok(arr_iter) = self.get_property_by_key(self.array_proto, sym_iter_key) {
                        if !arr_iter.is_undefined() {
                            self.heap.object_define_data(
                                obj,
                                sym_iter_key,
                                arr_iter,
                                PropertyAttrs::METHOD,
                                &mut self.shapes,
                            );
                        }
                    }
                    self.frames[frame_idx].arguments_obj = Some(obj);
                    obj
                };
                self.registers[a] = JsValue::from_object(args_obj);
            }
            // Binary arithmetic & bitwise ops
            Opcode::Add => {
                let lhs = self.registers[base + usize::from(inst.b())];
                let rhs = self.registers[base + usize::from(inst.c())];
                let res = self.op_add(lhs, rhs)?;
                self.registers[a] = res;
            }
            Opcode::Sub => {
                let lhs = self.registers[base + usize::from(inst.b())];
                let rhs = self.registers[base + usize::from(inst.c())];
                self.registers[a] = self.op_sub(lhs, rhs)?;
            }
            Opcode::Mul => {
                let lhs = self.registers[base + usize::from(inst.b())];
                let rhs = self.registers[base + usize::from(inst.c())];
                self.registers[a] = self.op_mul(lhs, rhs)?;
            }
            Opcode::Div => {
                let lhs = self.registers[base + usize::from(inst.b())];
                let rhs = self.registers[base + usize::from(inst.c())];
                self.registers[a] = self.op_numeric_bin(lhs, rhs, Opcode::Div)?;
            }
            Opcode::Rem => {
                let lhs = self.registers[base + usize::from(inst.b())];
                let rhs = self.registers[base + usize::from(inst.c())];
                self.registers[a] = self.op_numeric_bin(lhs, rhs, Opcode::Rem)?;
            }
            Opcode::Exp => {
                let lhs = self.registers[base + usize::from(inst.b())];
                let rhs = self.registers[base + usize::from(inst.c())];
                self.registers[a] = self.op_numeric_bin(lhs, rhs, Opcode::Exp)?;
            }
            Opcode::BitAnd => {
                let lhs = self.registers[base + usize::from(inst.b())];
                let rhs = self.registers[base + usize::from(inst.c())];
                self.registers[a] = self.op_numeric_bin(lhs, rhs, Opcode::BitAnd)?;
            }
            Opcode::BitOr => {
                let lhs = self.registers[base + usize::from(inst.b())];
                let rhs = self.registers[base + usize::from(inst.c())];
                self.registers[a] = self.op_numeric_bin(lhs, rhs, Opcode::BitOr)?;
            }
            Opcode::BitXor => {
                let lhs = self.registers[base + usize::from(inst.b())];
                let rhs = self.registers[base + usize::from(inst.c())];
                self.registers[a] = self.op_numeric_bin(lhs, rhs, Opcode::BitXor)?;
            }
            Opcode::Shl => {
                let lhs = self.registers[base + usize::from(inst.b())];
                let rhs = self.registers[base + usize::from(inst.c())];
                self.registers[a] = self.op_numeric_bin(lhs, rhs, Opcode::Shl)?;
            }
            Opcode::Shr => {
                let lhs = self.registers[base + usize::from(inst.b())];
                let rhs = self.registers[base + usize::from(inst.c())];
                self.registers[a] = self.op_numeric_bin(lhs, rhs, Opcode::Shr)?;
            }
            Opcode::UShr => {
                let lhs = self.registers[base + usize::from(inst.b())];
                let rhs = self.registers[base + usize::from(inst.c())];
                self.registers[a] = self.op_numeric_bin(lhs, rhs, Opcode::UShr)?;
            }
            // Comparisons
            Opcode::StrictEq => {
                let l = self.registers[base + usize::from(inst.b())];
                let r = self.registers[base + usize::from(inst.c())];
                self.registers[a] = JsValue::from_bool(self.strict_eq(l, r));
            }
            Opcode::StrictNotEq => {
                let l = self.registers[base + usize::from(inst.b())];
                let r = self.registers[base + usize::from(inst.c())];
                self.registers[a] = JsValue::from_bool(!self.strict_eq(l, r));
            }
            Opcode::Eq => {
                let l = self.registers[base + usize::from(inst.b())];
                let r = self.registers[base + usize::from(inst.c())];
                let eq = self.abstract_eq(l, r)?;
                self.registers[a] = JsValue::from_bool(eq);
            }
            Opcode::NotEq => {
                let l = self.registers[base + usize::from(inst.b())];
                let r = self.registers[base + usize::from(inst.c())];
                let eq = self.abstract_eq(l, r)?;
                self.registers[a] = JsValue::from_bool(!eq);
            }
            Opcode::Lt => {
                let l = self.registers[base + usize::from(inst.b())];
                let r = self.registers[base + usize::from(inst.c())];
                self.registers[a] = JsValue::from_bool(self.op_lt(l, r)?);
            }
            Opcode::LtEq => {
                let l = self.registers[base + usize::from(inst.b())];
                let r = self.registers[base + usize::from(inst.c())];
                self.registers[a] = JsValue::from_bool(self.op_lteq(l, r)?);
            }
            Opcode::Gt => {
                let l = self.registers[base + usize::from(inst.b())];
                let r = self.registers[base + usize::from(inst.c())];
                self.registers[a] = JsValue::from_bool(self.op_lt(r, l)?);
            }
            Opcode::GtEq => {
                let l = self.registers[base + usize::from(inst.b())];
                let r = self.registers[base + usize::from(inst.c())];
                self.registers[a] = JsValue::from_bool(self.op_lteq(r, l)?);
            }
            Opcode::In => {
                let key_val = self.registers[base + usize::from(inst.b())];
                let obj_val = self.registers[base + usize::from(inst.c())];
                let Some(obj) = obj_val.as_object() else {
                    return Err(self.type_error("Right-hand side of 'in' is not an object"));
                };
                let s = self.to_property_key(key_val)?;
                let key = self.heap.intern_key(&s);
                let has = self.has_property_by_key(obj, key)?;
                self.registers[a] = JsValue::from_bool(has);
            }
            Opcode::InstanceOf => {
                let lhs = self.registers[base + usize::from(inst.b())];
                let rhs = self.registers[base + usize::from(inst.c())];
                let res = self.op_instanceof(lhs, rhs)?;
                self.registers[a] = JsValue::from_bool(res);
            }
            // Unary ops
            Opcode::Neg => {
                let v = self.registers[base + usize::from(inst.b())];
                self.registers[a] = if let JsValueKind::Int32(i) = v.kind() {
                    if i != 0 && i != i32::MIN {
                        JsValue::from_i32(-i)
                    } else {
                        JsValue::from_f64(-f64::from(i))
                    }
                } else {
                    let nv = self.coerce_numeric(v)?;
                    if let Some(h) = nv.as_bigint() {
                        let bi = self
                            .heap
                            .get_bigint(h)
                            .cloned()
                            .unwrap_or_default()
                            .negate();
                        JsValue::from_bigint(self.heap.alloc_bigint(bi))
                    } else {
                        JsValue::from_f64(-self.to_number(nv)?)
                    }
                };
            }
            Opcode::Pos | Opcode::ToNumber => {
                let v = self.registers[base + usize::from(inst.b())];
                let n = self.to_number(v)?;
                self.registers[a] = JsValue::from_f64(n);
            }
            Opcode::ToString => {
                let v = self.registers[base + usize::from(inst.b())];
                let s = self.to_js_string(v)?;
                self.registers[a] = JsValue::from_string(self.heap.alloc_string(s));
            }
            Opcode::Not => {
                let v = self.registers[base + usize::from(inst.b())];
                self.registers[a] = JsValue::from_bool(!self.to_boolean(v));
            }
            Opcode::BitNot => {
                let raw = self.registers[base + usize::from(inst.b())];
                let nv = self.coerce_numeric(raw)?;
                self.registers[a] = if let Some(h) = nv.as_bigint() {
                    let bi = self
                        .heap
                        .get_bigint(h)
                        .cloned()
                        .unwrap_or_default()
                        .negate()
                        .sub(&JsBigInt::from_i64(1));
                    JsValue::from_bigint(self.heap.alloc_bigint(bi))
                } else {
                    JsValue::from_i32(!self.to_i32(nv)?)
                };
            }
            Opcode::TypeOf => {
                let v = self.registers[base + usize::from(inst.b())];
                let ty = self.typeof_value(v);
                self.registers[a] = JsValue::from_string(self.heap.alloc_string(ty));
            }
            Opcode::Inc => {
                let v = self.registers[base + usize::from(inst.b())];
                self.registers[a] = match v.kind() {
                    JsValueKind::Int32(i) => JsValue::add_i32_overflow(i, 1),
                    JsValueKind::BigInt(h) => {
                        let bi = self
                            .heap
                            .get_bigint(h)
                            .cloned()
                            .unwrap_or_default()
                            .add(&JsBigInt::from_i64(1));
                        JsValue::from_bigint(self.heap.alloc_bigint(bi))
                    }
                    _ => JsValue::from_f64(self.to_number(v)? + 1.0),
                };
            }
            Opcode::Dec => {
                let v = self.registers[base + usize::from(inst.b())];
                self.registers[a] = match v.kind() {
                    JsValueKind::Int32(i) => JsValue::sub_i32_overflow(i, 1),
                    JsValueKind::BigInt(h) => {
                        let bi = self
                            .heap
                            .get_bigint(h)
                            .cloned()
                            .unwrap_or_default()
                            .sub(&JsBigInt::from_i64(1));
                        JsValue::from_bigint(self.heap.alloc_bigint(bi))
                    }
                    _ => JsValue::from_f64(self.to_number(v)? - 1.0),
                };
            }
            // Control flow & cooperative OSR loop back-edge
            Opcode::Jump => {
                let cur_pc = self.frames[frame_idx].pc as isize;
                self.frames[frame_idx].pc = (cur_pc + isize::from(inst.sbx())) as usize;
            }
            Opcode::JumpIfFalse => {
                if !self.to_boolean(self.registers[a]) {
                    let cur_pc = self.frames[frame_idx].pc as isize;
                    self.frames[frame_idx].pc = (cur_pc + isize::from(inst.sbx())) as usize;
                }
            }
            Opcode::JumpIfTrue => {
                if self.to_boolean(self.registers[a]) {
                    let cur_pc = self.frames[frame_idx].pc as isize;
                    self.frames[frame_idx].pc = (cur_pc + isize::from(inst.sbx())) as usize;
                }
            }
            Opcode::JumpIfNullish => {
                let v = self.registers[a];
                if v.is_null() || v.is_undefined() {
                    let cur_pc = self.frames[frame_idx].pc as isize;
                    self.frames[frame_idx].pc = (cur_pc + isize::from(inst.sbx())) as usize;
                }
            }
            Opcode::JumpIfNotUndef => {
                if !self.registers[a].is_undefined() {
                    let cur_pc = self.frames[frame_idx].pc as isize;
                    self.frames[frame_idx].pc = (cur_pc + isize::from(inst.sbx())) as usize;
                }
            }
            Opcode::LoopBack => {
                self.loop_iterations = self.loop_iterations.saturating_add(1);
                if let Some(edges) = self.back_edge_counts.get_mut(prog_id) {
                    if let Some(cnt) = edges.get_mut(func_idx) {
                        *cnt = cnt.saturating_add(1);
                    }
                }
                if self.interrupt_flag.load(Ordering::Relaxed)
                    || self
                        .max_loop_iterations
                        .is_some_and(|limit| self.loop_iterations > limit)
                {
                    return Err(self.range_error("Execution interrupted: loop budget exceeded"));
                }
                if self.heap.should_collect_minor() {
                    self.collect_minor_gc();
                }
                let cur_pc = self.frames[frame_idx].pc as isize;
                let target_pc = (cur_pc + isize::from(inst.sbx())) as usize;
                self.frames[frame_idx].pc = target_pc;

                // Tier-2 On-Stack Replacement (OSR) at hot loop back-edges.
                if let Some(jit) = self.jit.clone() {
                    let back_cnt = self
                        .back_edge_counts
                        .get(prog_id)
                        .and_then(|v| v.get(func_idx))
                        .copied()
                        .unwrap_or(0);
                    if back_cnt >= jit.osr_threshold() {
                        let call_cnt = self
                            .call_counts
                            .get(prog_id)
                            .and_then(|v| v.get(func_idx))
                            .copied()
                            .unwrap_or(0);
                        match jit.try_enter(
                            self,
                            prog_id as u32,
                            func_idx as u32,
                            target_pc,
                            call_cnt,
                            back_cnt,
                        ) {
                            JitOutcome::Returned(val) => {
                                if let Some(top_ret) = self.pop_frame_with_value(val, stop_depth) {
                                    return Ok(StepOutcome::ReturnTop(top_ret));
                                }
                            }
                            JitOutcome::Threw(exc) => return Err(exc),
                            JitOutcome::Deopt { resume_pc } => {
                                self.frames[frame_idx].pc = resume_pc;
                            }
                            JitOutcome::Fallback => {}
                        }
                    }
                }
            }
            Opcode::Return => {
                let val = self.registers[a];
                if let Some(top_ret) = self.pop_frame_with_value(val, stop_depth) {
                    return Ok(StepOutcome::ReturnTop(top_ret));
                }
            }
            Opcode::Throw => {
                let val = self.registers[a];
                let msg = self.format_exception_value(val);
                return Err(JsException {
                    value: val,
                    message: msg,
                });
            }
            // Iterators
            Opcode::GetIterator => {
                let iterable = self.registers[base + usize::from(inst.b())];
                let iter_h = self.create_iterator(iterable)?;
                self.registers[a] = JsValue::from_object(iter_h);
            }
            Opcode::GetAsyncIterator => {
                let iterable = self.registers[base + usize::from(inst.b())];
                let iter_h = self.create_async_iterator(iterable)?;
                self.registers[a] = JsValue::from_object(iter_h);
            }
            Opcode::AsyncIteratorNext => {
                let iter_val = self.registers[base + usize::from(inst.b())];
                let res = self.step_async_iterator(iter_val)?;
                self.registers[a] = res;
            }
            Opcode::UnpackIterResult => {
                let res_val = self.registers[base + usize::from(inst.b())];
                let done_reg = base + usize::from(inst.c());
                let Some(res_h) = res_val.as_object() else {
                    return Err(self.type_error("Async iterator result is not an object"));
                };
                let done_val = self.get_property_by_key(res_h, self.key_done)?;
                let done = self.to_boolean(done_val);
                let val = self.get_property_by_key(res_h, self.key_value)?;
                self.registers[a] = val;
                self.registers[done_reg] = JsValue::from_bool(done);
            }
            Opcode::ForInKeys => {
                let obj_val = self.registers[base + usize::from(inst.b())];
                let mut keys = Vec::new();
                if let Some(mut cur) = obj_val.as_object() {
                    let mut seen = Vec::new();
                    loop {
                        let own = self.own_enumerable_keys_including_elements(cur);
                        for k in own {
                            if !seen.contains(&k) {
                                seen.push(k.clone());
                                keys.push(k);
                            }
                        }
                        match self.heap.get_object(cur).and_then(|o| o.prototype) {
                            Some(p) => cur = p,
                            None => break,
                        }
                    }
                }
                let handle = self.alloc_ordinary_object();
                self.iterators
                    .insert(handle, IteratorState::Keys { keys, index: 0 });
                self.registers[a] = JsValue::from_object(handle);
            }
            Opcode::IteratorNext => {
                let iter_val = self.registers[base + usize::from(inst.b())];
                let done_reg = base + usize::from(inst.c());
                let (val, done) = self.step_iterator(iter_val)?;
                self.registers[a] = val;
                self.registers[done_reg] = JsValue::from_bool(done);
            }
            Opcode::IteratorClose => {
                let abrupt = inst.b() != 0;
                if let Some(h) = self.registers[a].as_object() {
                    if let Some(IteratorState::Custom {
                        iter_obj,
                        exhausted: false,
                    }) = self.iterators.remove(&h)
                    {
                        self.iterator_close_custom(iter_obj, abrupt)?;
                    }
                }
            }
            Opcode::RequireObjectCoercible => {
                let v = self.registers[a];
                if v.is_null() || v.is_undefined() {
                    return Err(self.type_error("Cannot destructure null or undefined"));
                }
            }
            Opcode::GeneratorStart => {
                return Ok(StepOutcome::SuspendInitial);
            }
            Opcode::PushWith => {
                if let Some(h) = self.registers[a].as_object() {
                    self.with_stack.push(h);
                } else {
                    return Err(
                        self.type_error("Cannot convert null or undefined to object in 'with'")
                    );
                }
            }
            Opcode::PopWith => {
                self.with_stack.pop();
            }
            // Async / Generators / Module runtime hooks
            Opcode::Await => {
                let awaited = self.registers[base + usize::from(inst.b())];
                return Ok(StepOutcome::SuspendAwait {
                    dst: inst.a(),
                    awaited,
                });
            }
            Opcode::Yield => {
                let yielded = self.registers[base + usize::from(inst.b())];
                return Ok(StepOutcome::SuspendYield {
                    dst: inst.a(),
                    yielded,
                });
            }
            Opcode::YieldStar => {
                let iterable = self.registers[base + usize::from(inst.b())];
                return Ok(StepOutcome::SuspendYieldStar {
                    dst: inst.a(),
                    iterable,
                });
            }
            Opcode::DynamicImport => {
                let spec_val = self.registers[base + usize::from(inst.b())];
                let specifier = match self.to_js_string(spec_val) {
                    Ok(s) => s,
                    Err(exc) => {
                        let p = crate::promise::alloc_promise(self);
                        crate::promise::reject_promise(self, p, exc.value);
                        self.registers[a] = JsValue::from_object(p);
                        return Ok(StepOutcome::Continue);
                    }
                };
                let referrer = self
                    .module_envs
                    .get(&(prog_id as u32))
                    .map(|e| e.url.clone());
                let promise_h = if let Some(handler) = self.dynamic_import_handler.clone() {
                    handler(self, referrer, specifier)
                } else {
                    let p = crate::promise::alloc_promise(self);
                    let err =
                        self.type_error("Dynamic import() requires a registered ModuleLoader");
                    crate::promise::reject_promise(self, p, err.value);
                    p
                };
                self.registers[a] = JsValue::from_object(promise_h);
            }
            Opcode::GetModuleImport => {
                let import_idx = inst.bx();
                let val = crate::modules::vm_get_module_import(self, prog_id as u32, import_idx)?;
                self.registers[a] = val;
            }
            Opcode::SetModuleExport => {
                let export_idx = inst.bx();
                let val = self.registers[a];
                crate::modules::vm_set_module_export(self, prog_id as u32, export_idx, val);
            }
        }
        Ok(StepOutcome::Continue)
    }

    // -----------------------------------------------------------------------
    // Frame, Upvalue, Exception & GC Management
    // -----------------------------------------------------------------------

    fn pop_frame_with_value(&mut self, val: JsValue, stop_depth: usize) -> Option<JsValue> {
        let popped = self.frames.pop()?;
        self.close_upvalues_from(popped.base_reg);
        self.registers.truncate(popped.base_reg);
        let final_val = if let Some(inst_obj) = popped.construct_obj {
            if val.is_object() {
                val
            } else {
                JsValue::from_object(inst_obj)
            }
        } else {
            val
        };
        if self.frames.len() <= stop_depth {
            Some(final_val)
        } else {
            if let Some(dst) = popped.ret_dst {
                if dst < self.registers.len() {
                    self.registers[dst] = final_val;
                }
            }
            None
        }
    }

    fn handle_exception(
        &mut self,
        exc: JsException,
        stop_depth: usize,
    ) -> Result<JsValue, JsException> {
        while self.frames.len() > stop_depth {
            let f_idx = self.frames.len() - 1;
            let (prog_id, func_idx, base, faulting_pc) = {
                let f = &self.frames[f_idx];
                (
                    f.program_id as usize,
                    f.func_idx as usize,
                    f.base_reg,
                    f.pc.saturating_sub(1) as u32,
                )
            };
            let matched = self.programs[prog_id].functions[func_idx]
                .exception_handlers
                .iter()
                .find(|h| faulting_pc >= h.try_start && faulting_pc < h.try_end)
                .copied();
            if let Some(handler) = matched {
                self.registers[base + usize::from(handler.catch_reg)] = exc.value;
                self.frames[f_idx].pc = handler.handler_pc as usize;
                return Ok(JsValue::UNDEFINED);
            }
            let popped = self.frames.pop().expect("frame");
            self.close_upvalues_from(popped.base_reg);
            self.registers.truncate(popped.base_reg);
        }
        Err(exc)
    }

    fn capture_open_upvalue(&mut self, abs_reg: usize) -> HeapHandle {
        if let Some(&(_, cell)) = self.open_upvalues.iter().find(|&&(r, _)| r == abs_reg) {
            return cell;
        }
        let cell = self
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::UpvalueCell, None));
        let val = self
            .registers
            .get(abs_reg)
            .copied()
            .unwrap_or(JsValue::UNDEFINED);
        self.heap.object_set_slot(cell, 0, val);
        self.open_upvalues.push((abs_reg, cell));
        cell
    }

    fn close_upvalues_from(&mut self, cutoff_reg: usize) {
        let mut i = 0;
        while i < self.open_upvalues.len() {
            let (abs_reg, cell) = self.open_upvalues[i];
            if abs_reg >= cutoff_reg {
                let val = self
                    .registers
                    .get(abs_reg)
                    .copied()
                    .unwrap_or(JsValue::UNDEFINED);
                self.heap.object_set_slot(cell, 0, val);
                self.open_upvalues.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    fn frame_upvalue(
        &mut self,
        frame_idx: usize,
        uv_idx: usize,
    ) -> Result<HeapHandle, JsException> {
        let callee = self.frames[frame_idx].callee;
        let found = match self.heap.get_object(callee).map(|o| &o.kind) {
            Some(ObjectKind::Closure(c)) => c.upvalues.get(uv_idx).copied(),
            _ => return Err(self.reference_error("Active callee is not a closure")),
        };
        found.ok_or_else(|| self.reference_error("Invalid upvalue index"))
    }

    fn read_upvalue_cell(&self, cell: HeapHandle) -> JsValue {
        if let Some(&(abs_reg, _)) = self.open_upvalues.iter().find(|&&(_, c)| c == cell) {
            return self
                .registers
                .get(abs_reg)
                .copied()
                .unwrap_or(JsValue::UNDEFINED);
        }
        self.heap
            .get_object(cell)
            .and_then(|o| o.slots.first().copied())
            .unwrap_or(JsValue::UNDEFINED)
    }

    fn write_upvalue_cell(&mut self, cell: HeapHandle, val: JsValue) {
        if let Some(&(abs_reg, _)) = self.open_upvalues.iter().find(|&&(_, c)| c == cell) {
            if abs_reg < self.registers.len() {
                self.registers[abs_reg] = val;
            }
            return;
        }
        self.heap.object_set_slot(cell, 0, val);
    }

    /// Collects all live VM roots and runs a sub-millisecond generational minor GC.
    pub fn collect_minor_gc(&mut self) {
        let roots = self.collect_roots();
        let _ = self.heap.collect_minor(&roots);
    }

    // -----------------------------------------------------------------------
    // Inline-Cached Property & Element Access
    // -----------------------------------------------------------------------

    #[inline]
    fn ic_key(&self, prog_id: usize, func_idx: usize, ic_idx: usize) -> PropertyKey {
        let desc = self.programs[prog_id].functions[func_idx].ic_slots[ic_idx];
        self.const_keys[prog_id][func_idx][desc.key_const as usize]
            .expect("IC key constant must be string")
    }

    fn ic_key_and_name(
        &self,
        prog_id: usize,
        func_idx: usize,
        ic_idx: usize,
    ) -> (PropertyKey, String) {
        let desc: IcSlotDescriptor = self.programs[prog_id].functions[func_idx].ic_slots[ic_idx];
        let key = self.const_keys[prog_id][func_idx][desc.key_const as usize]
            .expect("IC key constant must be string");
        let name = self.const_string(prog_id, func_idx, desc.key_const as usize);
        (key, name)
    }

    #[inline]
    fn const_key(&self, prog_id: usize, func_idx: usize, const_idx: usize) -> PropertyKey {
        self.const_keys[prog_id][func_idx][const_idx].expect("constant must be string")
    }

    fn const_string(&self, prog_id: usize, func_idx: usize, const_idx: usize) -> String {
        match self.programs[prog_id].functions[func_idx]
            .constants
            .get(const_idx)
        {
            Some(ConstantEntry::String(s)) => s.clone(),
            _ => String::new(),
        }
    }

    fn materialize_constant(
        &mut self,
        prog_id: usize,
        func_idx: usize,
        const_idx: usize,
    ) -> Result<JsValue, JsException> {
        let entry = self.programs[prog_id].functions[func_idx]
            .constants
            .get(const_idx)
            .cloned()
            .ok_or_else(|| self.type_error("Constant index out of bounds"))?;
        Ok(match entry {
            ConstantEntry::Number(n) => JsValue::from_f64(n),
            ConstantEntry::Int32(i) => JsValue::from_i32(i),
            ConstantEntry::String(s) => JsValue::from_string(self.heap.alloc_string(s)),
            ConstantEntry::BigInt(raw) => {
                let bi = JsBigInt::parse_str(&raw).unwrap_or_default();
                JsValue::from_bigint(self.heap.alloc_bigint(bi))
            }
            ConstantEntry::RegExp { pattern, flags } => {
                JsValue::from_object(self.alloc_regexp(pattern, flags))
            }
            ConstantEntry::Function(_) | ConstantEntry::Class(_) | ConstantEntry::DirectEval(_) => {
                JsValue::UNDEFINED
            }
        })
    }

    fn get_global_cached(
        &mut self,
        prog_id: usize,
        func_idx: usize,
        ic_idx: usize,
        key: PropertyKey,
        name: &str,
    ) -> Result<JsValue, JsException> {
        let (shape, global_proto) = {
            let g = self.heap.get_object(self.global).unwrap();
            (g.shape(), g.prototype)
        };
        let epoch = self.shapes.prototype_epoch();
        if let Some(entry) = self.ic_tables[prog_id][func_idx].probe_load(
            ic_idx,
            shape,
            global_proto,
            key,
            epoch,
            &self.mega_ic,
            &mut self.ic_stats,
        ) {
            let holder = entry.holder.unwrap_or(self.global);
            if let Some(v) = self
                .heap
                .get_object(holder)
                .and_then(|o| o.slots.get(entry.offset as usize).copied())
            {
                return Ok(v);
            }
        }
        if !self.has_property_by_key(self.global, key)? {
            return Err(self.reference_error(format!("{name} is not defined")));
        }
        self.get_prop_slow(
            self.global,
            JsValue::from_object(self.global),
            prog_id,
            func_idx,
            ic_idx,
            key,
        )
    }

    fn get_prop_cached(
        &mut self,
        obj_val: JsValue,
        prog_id: usize,
        func_idx: usize,
        ic_idx: usize,
        key: PropertyKey,
    ) -> Result<JsValue, JsException> {
        if obj_val.is_null() || obj_val.is_undefined() {
            return Err(self.type_error("Cannot read properties of null or undefined"));
        }
        // Primitive string `.length` & prototype lookup.
        if let Some(str_h) = obj_val.as_string() {
            if key == self.key_length {
                let len = self.heap.get_string(str_h).map_or(0, |s| s.utf16_len()) as i32;
                return Ok(JsValue::from_i32(len));
            }
            return self.get_property_with_receiver(self.string_proto, key, obj_val);
        }
        let Some(obj) = obj_val.as_object() else {
            let proto = if obj_val.is_bool() {
                self.boolean_proto
            } else if obj_val.is_bigint() {
                self.bigint_proto
            } else if obj_val.is_symbol() {
                self.symbol_proto
            } else {
                self.number_proto
            };
            return self.get_property_with_receiver(proto, key, obj_val);
        };

        let Some((is_array, is_proxy, arr_len, shape, receiver_proto)) =
            self.heap.get_object(obj).map(|o| {
                (
                    matches!(o.kind, ObjectKind::Array),
                    matches!(o.kind, ObjectKind::Proxy(_)),
                    o.elements.len(),
                    o.shape(),
                    o.prototype,
                )
            })
        else {
            return Err(self.type_error("Dead object handle"));
        };

        // Fast-path Array `.length`.
        if is_array && key == self.key_length {
            return Ok(JsValue::from_f64(f64::from(arr_len)));
        }

        // Fast-path IC probe (skip for Proxy).
        if !is_proxy {
            let epoch = self.shapes.prototype_epoch();
            if let Some(entry) = self.ic_tables[prog_id][func_idx].probe_load(
                ic_idx,
                shape,
                receiver_proto,
                key,
                epoch,
                &self.mega_ic,
                &mut self.ic_stats,
            ) {
                let holder = entry.holder.unwrap_or(obj);
                if let Some(val) = self
                    .heap
                    .get_object(holder)
                    .and_then(|o| o.slots.get(entry.offset as usize).copied())
                {
                    return Ok(val);
                }
            }
        }

        self.get_prop_slow(obj, obj_val, prog_id, func_idx, ic_idx, key)
    }

    fn get_prop_slow(
        &mut self,
        obj: HeapHandle,
        receiver: JsValue,
        prog_id: usize,
        func_idx: usize,
        ic_idx: usize,
        key: PropertyKey,
    ) -> Result<JsValue, JsException> {
        if let Some(ObjectKind::Proxy(p)) = self.heap.get_object(obj).map(|o| o.kind.clone()) {
            return self.proxy_get(p, key, receiver);
        }
        let (receiver_shape, receiver_proto) = self
            .heap
            .get_object(obj)
            .map_or((ShapeId::DICTIONARY, None), |o| (o.shape(), o.prototype));
        let epoch = self.shapes.prototype_epoch();

        let mut cur = Some(obj);
        let idx_opt = self.key_as_array_index(key);
        while let Some(holder) = cur {
            if let Some(idx) = idx_opt {
                if let Some(o) = self.heap.get_object(holder) {
                    if let ObjectKind::TypedArray(ta) = o.kind {
                        let eff_len = crate::builtins_atomics::ta_effective_len(self, holder, ta)
                            .unwrap_or(0);
                        if idx < eff_len {
                            return Ok(crate::builtins_collections::typed_array_read_element(
                                self, ta, idx,
                            ));
                        }
                        return Ok(JsValue::UNDEFINED);
                    } else if let Some(val) = o.elements.get(idx) {
                        return Ok(val);
                    }
                }
            }
            if let Some(prop) = self.heap.object_get_own(holder, key, &self.shapes) {
                if prop.attrs.contains(PropertyAttrs::ACCESSOR) {
                    if let Some(getter) = prop.getter {
                        return self.call_value(JsValue::from_object(getter), receiver, &[]);
                    }
                    return Ok(JsValue::UNDEFINED);
                }
                // Populate IC if both receiver and holder are shaped!
                if !receiver_shape.is_dictionary() {
                    if let Some(h_obj) = self.heap.get_object(holder) {
                        if let Some(slot) = self.shapes.lookup(h_obj.shape(), key) {
                            let entry = if holder == obj {
                                IcEntry::own(receiver_shape, slot.offset)
                            } else {
                                IcEntry::proto(
                                    receiver_shape,
                                    receiver_proto,
                                    holder,
                                    slot.offset,
                                    epoch,
                                )
                            };
                            self.ic_tables[prog_id][func_idx].update(
                                ic_idx,
                                key,
                                entry,
                                false,
                                &mut self.mega_ic,
                            );
                        }
                    }
                }
                return Ok(prop.value);
            }
            cur = self.heap.get_object(holder).and_then(|o| o.prototype);
        }
        Ok(JsValue::UNDEFINED)
    }

    fn set_prop_cached(
        &mut self,
        obj: HeapHandle,
        prog_id: usize,
        func_idx: usize,
        ic_idx: usize,
        key: PropertyKey,
        val: JsValue,
    ) -> Result<(), JsException> {
        let Some((is_array, is_proxy, shape, is_proto, extensible)) =
            self.heap.get_object(obj).map(|o| {
                (
                    matches!(o.kind, ObjectKind::Array),
                    matches!(o.kind, ObjectKind::Proxy(_)),
                    o.shape(),
                    o.is_prototype,
                    o.extensible,
                )
            })
        else {
            return Err(self.type_error("Dead object handle"));
        };

        if is_array && key == self.key_length {
            let is_strict = self.programs[prog_id].functions[func_idx].is_strict;
            let ok = self.set_array_length_checked(obj, val)?;
            if !ok && is_strict {
                return Err(self.type_error("Cannot assign to read only property 'length'"));
            }
            return Ok(());
        }

        if !is_proxy {
            if let Some(entry) = self.ic_tables[prog_id][func_idx].probe_store(
                ic_idx,
                shape,
                key,
                &self.mega_ic,
                &mut self.ic_stats,
            ) {
                if entry.target_shape == entry.shape || extensible {
                    let target_idx = entry.offset as usize;
                    let obj_mut = self.heap.get_object_mut(obj).unwrap();
                    if entry.target_shape != entry.shape {
                        obj_mut.shape_id = entry.target_shape.0;
                        obj_mut.transition_count = obj_mut.transition_count.saturating_add(1);
                    }
                    if target_idx >= obj_mut.slots.len() {
                        obj_mut.slots.resize(target_idx + 1, JsValue::UNDEFINED);
                    }
                    obj_mut.slots[target_idx] = val;
                    if is_proto {
                        self.shapes.bump_prototype_epoch();
                    }
                    self.heap.write_barrier(obj, val);
                    return Ok(());
                }
            }
        }

        self.set_prop_slow(obj, prog_id, func_idx, ic_idx, key, val)
    }

    fn set_prop_slow(
        &mut self,
        obj: HeapHandle,
        prog_id: usize,
        func_idx: usize,
        ic_idx: usize,
        key: PropertyKey,
        val: JsValue,
    ) -> Result<(), JsException> {
        let is_strict = self.programs[prog_id].functions[func_idx].is_strict;
        if let Some(ObjectKind::Proxy(p)) = self.heap.get_object(obj).map(|o| o.kind.clone()) {
            let ok = self.proxy_set(p, key, val, JsValue::from_object(obj))?;
            if !ok && is_strict {
                return Err(self.type_error("'set' on proxy returned falsy"));
            }
            return Ok(());
        }

        // Check existing own property or inherited setter on prototype chain.
        let mut cur = Some(obj);
        while let Some(holder) = cur {
            if let Some(prop) = self.heap.object_get_own(holder, key, &self.shapes) {
                if prop.attrs.contains(PropertyAttrs::ACCESSOR) {
                    if let Some(setter) = prop.setter {
                        self.call_value(
                            JsValue::from_object(setter),
                            JsValue::from_object(obj),
                            &[val],
                        )?;
                        return Ok(());
                    }
                    if is_strict {
                        return Err(self.type_error("Cannot set property with only a getter"));
                    }
                    return Ok(());
                }
                if !prop.attrs.contains(PropertyAttrs::WRITABLE) {
                    if is_strict {
                        return Err(self.type_error("Cannot assign to read-only property"));
                    }
                    return Ok(());
                }
                if holder == obj {
                    if let Some((from_s, to_s, off)) =
                        self.heap
                            .object_define_data(obj, key, val, prop.attrs, &mut self.shapes)
                    {
                        self.ic_tables[prog_id][func_idx].update(
                            ic_idx,
                            key,
                            IcEntry::transition(from_s, to_s, off),
                            true,
                            &mut self.mega_ic,
                        );
                    }
                    return Ok(());
                }
                break;
            }
            cur = self.heap.get_object(holder).and_then(|o| o.prototype);
        }

        if !self.heap.get_object(obj).is_some_and(|o| o.extensible) {
            if is_strict {
                return Err(self.type_error("Cannot add property, object is not extensible"));
            }
            return Ok(());
        }

        if let Some((from_s, to_s, off)) =
            self.heap
                .object_define_data(obj, key, val, PropertyAttrs::DEFAULT, &mut self.shapes)
        {
            self.ic_tables[prog_id][func_idx].update(
                ic_idx,
                key,
                IcEntry::transition(from_s, to_s, off),
                true,
                &mut self.mega_ic,
            );
        }
        Ok(())
    }

    fn define_prop_cached(
        &mut self,
        obj: HeapHandle,
        prog_id: usize,
        func_idx: usize,
        ic_idx: usize,
        key: PropertyKey,
        val: JsValue,
    ) -> Result<(), JsException> {
        let shape = self
            .heap
            .get_object(obj)
            .map_or(ShapeId::DICTIONARY, |o| o.shape());
        let is_proto = self.heap.get_object(obj).is_some_and(|o| o.is_prototype);
        if let Some(entry) = self.ic_tables[prog_id][func_idx].probe_store(
            ic_idx,
            shape,
            key,
            &self.mega_ic,
            &mut self.ic_stats,
        ) {
            let idx = entry.offset as usize;
            let obj_mut = self.heap.get_object_mut(obj).unwrap();
            if entry.target_shape != entry.shape {
                obj_mut.shape_id = entry.target_shape.0;
                obj_mut.transition_count = obj_mut.transition_count.saturating_add(1);
            }
            if idx >= obj_mut.slots.len() {
                obj_mut.slots.resize(idx + 1, JsValue::UNDEFINED);
            }
            obj_mut.slots[idx] = val;
            if is_proto {
                self.shapes.bump_prototype_epoch();
            }
            self.heap.write_barrier(obj, val);
            return Ok(());
        }
        if let Some((from_s, to_s, off)) =
            self.heap
                .object_define_data(obj, key, val, PropertyAttrs::DEFAULT, &mut self.shapes)
        {
            self.ic_tables[prog_id][func_idx].update(
                ic_idx,
                key,
                IcEntry::transition(from_s, to_s, off),
                true,
                &mut self.mega_ic,
            );
        }
        Ok(())
    }

    pub fn get_property_by_key(
        &mut self,
        obj: HeapHandle,
        key: PropertyKey,
    ) -> Result<JsValue, JsException> {
        self.get_property_with_receiver(obj, key, JsValue::from_object(obj))
    }

    pub fn get_property_with_receiver(
        &mut self,
        obj: HeapHandle,
        key: PropertyKey,
        receiver: JsValue,
    ) -> Result<JsValue, JsException> {
        if let Some(ObjectKind::Proxy(p)) = self.heap.get_object(obj).map(|o| o.kind.clone()) {
            return self.proxy_get(p, key, receiver);
        }
        if matches!(
            self.heap.get_object(obj).map(|o| &o.kind),
            Some(ObjectKind::Array)
        ) && key == self.key_length
        {
            return Ok(JsValue::from_f64(f64::from(
                self.heap.get_object(obj).unwrap().elements.len(),
            )));
        }
        let mut cur = Some(obj);
        let idx_opt = self.key_as_array_index(key);
        while let Some(holder) = cur {
            if let Some(idx) = idx_opt {
                if let Some(o) = self.heap.get_object(holder) {
                    if let ObjectKind::TypedArray(ta) = o.kind {
                        let eff_len = crate::builtins_atomics::ta_effective_len(self, holder, ta)
                            .unwrap_or(0);
                        if idx < eff_len {
                            return Ok(crate::builtins_collections::typed_array_read_element(
                                self, ta, idx,
                            ));
                        }
                        return Ok(JsValue::UNDEFINED);
                    } else if let Some(val) = o.elements.get(idx) {
                        return Ok(val);
                    }
                }
            }
            if let Some(prop) = self.heap.object_get_own(holder, key, &self.shapes) {
                if prop.attrs.contains(PropertyAttrs::ACCESSOR) {
                    if let Some(getter) = prop.getter {
                        return self.call_value(JsValue::from_object(getter), receiver, &[]);
                    }
                    return Ok(JsValue::UNDEFINED);
                }
                return Ok(prop.value);
            }
            cur = self.heap.get_object(holder).and_then(|o| o.prototype);
        }
        Ok(JsValue::UNDEFINED)
    }

    pub fn set_property_by_key(
        &mut self,
        obj: HeapHandle,
        key: PropertyKey,
        val: JsValue,
    ) -> Result<bool, JsException> {
        if let Some(ObjectKind::Proxy(p)) = self.heap.get_object(obj).map(|o| o.kind.clone()) {
            return self.proxy_set(p, key, val, JsValue::from_object(obj));
        }
        if matches!(
            self.heap.get_object(obj).map(|o| &o.kind),
            Some(ObjectKind::Array)
        ) && key == self.key_length
        {
            return self.set_array_length_checked(obj, val);
        }
        let mut cur = Some(obj);
        while let Some(holder) = cur {
            if let Some(prop) = self.heap.object_get_own(holder, key, &self.shapes) {
                if prop.attrs.contains(PropertyAttrs::ACCESSOR) {
                    if let Some(setter) = prop.setter {
                        self.call_value(
                            JsValue::from_object(setter),
                            JsValue::from_object(obj),
                            &[val],
                        )?;
                        return Ok(true);
                    }
                    return Ok(false);
                }
                if !prop.attrs.contains(PropertyAttrs::WRITABLE) {
                    return Ok(false);
                }
                if holder == obj {
                    self.heap
                        .object_define_data(obj, key, val, prop.attrs, &mut self.shapes);
                    return Ok(true);
                }
                break;
            }
            cur = self.heap.get_object(holder).and_then(|o| o.prototype);
        }
        if !self.heap.get_object(obj).is_some_and(|o| o.extensible) {
            return Ok(false);
        }
        self.heap
            .object_define_data(obj, key, val, PropertyAttrs::DEFAULT, &mut self.shapes);
        Ok(true)
    }

    pub fn has_property_by_key(
        &mut self,
        obj: HeapHandle,
        key: PropertyKey,
    ) -> Result<bool, JsException> {
        if let Some(ObjectKind::Proxy(p)) = self.heap.get_object(obj).map(|o| o.kind.clone()) {
            if p.revoked {
                return Err(self.type_error("Cannot perform 'has' on a revoked Proxy"));
            }
            let trap = self.get_property_by_key(p.handler, self.key_has)?;
            if !trap.is_undefined() && !trap.is_null() {
                let k_str = self
                    .heap
                    .interner()
                    .get(key)
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let kv = JsValue::from_string(self.heap.alloc_string(k_str));
                let r = self.call_value(
                    trap,
                    JsValue::from_object(p.handler),
                    &[JsValue::from_object(p.target), kv],
                )?;
                return Ok(self.to_boolean(r));
            }
            return self.has_property_by_key(p.target, key);
        }
        if let Some(name) = self.heap.interner().get(key).map(|s| s.to_string()) {
            if let Ok(idx) = name.parse::<u32>() {
                if self
                    .heap
                    .get_object(obj)
                    .is_some_and(|o| o.elements.has(idx))
                {
                    return Ok(true);
                }
            }
        }
        let mut cur = Some(obj);
        while let Some(holder) = cur {
            if self
                .heap
                .object_get_own(holder, key, &self.shapes)
                .is_some()
            {
                return Ok(true);
            }
            cur = self.heap.get_object(holder).and_then(|o| o.prototype);
        }
        Ok(false)
    }

    pub fn delete_property_by_key(
        &mut self,
        obj: HeapHandle,
        key: PropertyKey,
    ) -> Result<bool, JsException> {
        if let Some(ObjectKind::Proxy(p)) = self.heap.get_object(obj).map(|o| o.kind.clone()) {
            if p.revoked {
                return Err(self.type_error("Cannot perform 'deleteProperty' on a revoked Proxy"));
            }
            let trap = self.get_property_by_key(p.handler, self.key_delete_property)?;
            if !trap.is_undefined() && !trap.is_null() {
                let k_str = self
                    .heap
                    .interner()
                    .get(key)
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let kv = JsValue::from_string(self.heap.alloc_string(k_str));
                let r = self.call_value(
                    trap,
                    JsValue::from_object(p.handler),
                    &[JsValue::from_object(p.target), kv],
                )?;
                return Ok(self.to_boolean(r));
            }
            return self.delete_property_by_key(p.target, key);
        }
        if let Some(prop) = self.heap.object_get_own(obj, key, &self.shapes) {
            if !prop.attrs.contains(PropertyAttrs::CONFIGURABLE) {
                return Ok(false);
            }
        }
        if matches!(
            self.heap.get_object(obj).map(|o| &o.kind),
            Some(ObjectKind::Array)
        ) && key == self.key_length
        {
            return Ok(false);
        }
        Ok(self.heap.object_delete_own(obj, key, &mut self.shapes))
    }

    fn proxy_get(
        &mut self,
        p: ProxyObject,
        key: PropertyKey,
        receiver: JsValue,
    ) -> Result<JsValue, JsException> {
        if p.revoked {
            return Err(self.type_error("Cannot perform 'get' on a revoked Proxy"));
        }
        let trap = self.get_property_by_key(p.handler, self.key_get)?;
        if !trap.is_undefined() && !trap.is_null() {
            let k_str = self
                .heap
                .interner()
                .get(key)
                .map(|s| s.to_string())
                .unwrap_or_default();
            let kv = JsValue::from_string(self.heap.alloc_string(k_str));
            return self.call_value(
                trap,
                JsValue::from_object(p.handler),
                &[JsValue::from_object(p.target), kv, receiver],
            );
        }
        self.get_property_with_receiver(p.target, key, receiver)
    }

    fn proxy_set(
        &mut self,
        p: ProxyObject,
        key: PropertyKey,
        val: JsValue,
        receiver: JsValue,
    ) -> Result<bool, JsException> {
        if p.revoked {
            return Err(self.type_error("Cannot perform 'set' on a revoked Proxy"));
        }
        let trap = self.get_property_by_key(p.handler, self.key_set)?;
        if !trap.is_undefined() && !trap.is_null() {
            let k_str = self
                .heap
                .interner()
                .get(key)
                .map(|s| s.to_string())
                .unwrap_or_default();
            let kv = JsValue::from_string(self.heap.alloc_string(k_str));
            let res = self.call_value(
                trap,
                JsValue::from_object(p.handler),
                &[JsValue::from_object(p.target), kv, val, receiver],
            )?;
            return Ok(self.to_boolean(res));
        }
        self.set_property_by_key(p.target, key, val)
    }

    pub fn get_elem(&mut self, obj_val: JsValue, key_val: JsValue) -> Result<JsValue, JsException> {
        if obj_val.is_null() || obj_val.is_undefined() {
            return Err(self.type_error("Cannot read properties of null or undefined"));
        }
        // Convert object keys via ECMA-262 `ToPropertyKey` before indexing.
        let key_val = if key_val.is_object() {
            let ks = self.to_property_key(key_val)?;
            JsValue::from_string(self.heap.alloc_string(ks))
        } else {
            key_val
        };
        // Fast string indexing `"hello"[1] === "e"`.
        if let Some(str_h) = obj_val.as_string() {
            if let Some(idx) = key_val.as_i32().and_then(|i| u32::try_from(i).ok()) {
                let s = self.heap.materialize_string(str_h).unwrap_or_default();
                if let Some(ch) = s.chars().nth(idx as usize) {
                    return Ok(JsValue::from_string(self.heap.alloc_string(ch.to_string())));
                }
                return Ok(JsValue::UNDEFINED);
            }
            let ks = self.to_property_key(key_val)?;
            let k = self.heap.intern_key(&ks);
            return self.get_property_with_receiver(self.string_proto, k, obj_val);
        }
        let Some(obj) = obj_val.as_object() else {
            return Ok(JsValue::UNDEFINED);
        };
        if let Some(ObjectKind::Proxy(p)) = self.heap.get_object(obj).map(|o| o.kind.clone()) {
            let ks = self.to_property_key(key_val)?;
            let k = self.heap.intern_key(&ks);
            return self.proxy_get(p, k, obj_val);
        }
        if let Some(idx) = self.as_array_index(key_val) {
            if let Some(ObjectKind::TypedArray(ta)) = self.heap.get_object(obj).map(|o| &o.kind) {
                let ta = *ta;
                let eff_len = crate::builtins_atomics::ta_effective_len(self, obj, ta).unwrap_or(0);
                if idx < eff_len {
                    return Ok(crate::builtins_collections::typed_array_read_element(
                        self, ta, idx,
                    ));
                }
                return Ok(JsValue::UNDEFINED);
            }
            if let Some(val) = self.heap.get_object(obj).and_then(|o| o.elements.get(idx)) {
                return Ok(val);
            }
        }
        let ks = self.to_property_key(key_val)?;
        let key = self.heap.intern_key(&ks);
        self.get_property_by_key(obj, key)
    }

    pub fn set_elem(
        &mut self,
        obj_val: JsValue,
        key_val: JsValue,
        val: JsValue,
    ) -> Result<(), JsException> {
        let _ = self.set_elem_checked(obj_val, key_val, val)?;
        Ok(())
    }

    pub fn set_elem_checked(
        &mut self,
        obj_val: JsValue,
        key_val: JsValue,
        val: JsValue,
    ) -> Result<bool, JsException> {
        let Some(obj) = obj_val.as_object() else {
            return Err(self.type_error("Cannot set element on non-object"));
        };
        let key_val = if key_val.is_object() {
            let ks = self.to_property_key(key_val)?;
            JsValue::from_string(self.heap.alloc_string(ks))
        } else {
            key_val
        };
        if let Some(ObjectKind::Proxy(p)) = self.heap.get_object(obj).map(|o| o.kind.clone()) {
            let ks = self.to_property_key(key_val)?;
            let k = self.heap.intern_key(&ks);
            return self.proxy_set(p, k, val, obj_val);
        }
        if let Some(idx) = self.as_array_index(key_val) {
            if let Some(ObjectKind::TypedArray(ta)) = self.heap.get_object(obj).map(|o| &o.kind) {
                let ta = *ta;
                crate::builtins_collections::typed_array_write_element_checked(
                    self, obj, ta, idx, val,
                )?;
                return Ok(true);
            }
            // Check if this numeric key is defined as an accessor or named property on obj/prototype.
            let has_named_idx = {
                let is_arr = matches!(
                    self.heap.get_object(obj).map(|o| &o.kind),
                    Some(ObjectKind::Array)
                );
                let elem_present = self
                    .heap
                    .get_object(obj)
                    .is_some_and(|o| o.elements.has(idx));
                if is_arr && elem_present {
                    false
                } else {
                    let ks = idx.to_string();
                    if let Some(k) = self.heap.interner().lookup(&ks) {
                        let mut cur = Some(obj);
                        let mut found = false;
                        while let Some(h) = cur {
                            if self.heap.object_get_own(h, k, &self.shapes).is_some() {
                                found = true;
                                break;
                            }
                            cur = self.heap.get_object(h).and_then(|o| o.prototype);
                        }
                        found
                    } else {
                        false
                    }
                }
            };
            if has_named_idx {
                let key = self.heap.intern_key(&idx.to_string());
                return self.set_property_by_key(obj, key, val);
            }
            let extensible = self.heap.get_object(obj).is_some_and(|o| o.extensible);
            let exists = self
                .heap
                .get_object(obj)
                .is_some_and(|o| o.elements.has(idx));
            if !exists && !extensible {
                return Ok(false);
            }
            if let Some(o) = self.heap.get_object_mut(obj) {
                o.elements.set(idx, val);
            }
            self.heap.write_barrier(obj, val);
            return Ok(true);
        }
        let ks = self.to_property_key(key_val)?;
        let key = self.heap.intern_key(&ks);
        self.set_property_by_key(obj, key, val)
    }

    fn delete_elem(&mut self, obj_val: JsValue, key_val: JsValue) -> Result<bool, JsException> {
        let Some(obj) = obj_val.as_object() else {
            return Ok(true);
        };
        let key_val = if key_val.is_object() {
            let ks = self.to_property_key(key_val)?;
            JsValue::from_string(self.heap.alloc_string(ks))
        } else {
            key_val
        };
        if let Some(idx) = self.as_array_index(key_val) {
            if self
                .heap
                .get_object(obj)
                .is_some_and(|o| o.elements.has(idx))
            {
                if let Some(o) = self.heap.get_object_mut(obj) {
                    return Ok(o.elements.delete(idx));
                }
            }
        }
        let ks = self.to_property_key(key_val)?;
        let key = self.heap.intern_key(&ks);
        self.delete_property_by_key(obj, key)
    }

    #[inline]
    fn key_as_array_index(&self, key: PropertyKey) -> Option<u32> {
        let s = self.heap.interner().get(key)?;
        let b0 = *s.as_bytes().first()?;
        if !b0.is_ascii_digit() || (s.len() > 1 && b0 == b'0') {
            return None;
        }
        s.parse::<u32>().ok().filter(|&n| n < u32::MAX)
    }

    #[inline]
    fn as_array_index(&self, val: JsValue) -> Option<u32> {
        match val.kind() {
            JsValueKind::Int32(i) if i >= 0 => Some(i as u32),
            JsValueKind::Float(f)
                if f.is_finite() && f >= 0.0 && f.fract() == 0.0 && f < f64::from(u32::MAX) =>
            {
                Some(f as u32)
            }
            JsValueKind::String(h) => {
                let s = self.heap.materialize_string(h)?;
                if s.is_empty() || (s.len() > 1 && s.starts_with('0')) {
                    return None;
                }
                s.parse::<u32>().ok().filter(|&n| n < u32::MAX)
            }
            _ => None,
        }
    }

    fn capture_function_upvalues(
        &mut self,
        prog_id: usize,
        child_fn_idx: u32,
        frame_idx: usize,
        base: usize,
    ) -> Result<Vec<HeapHandle>, JsException> {
        let uv_descs = self.programs[prog_id].functions[child_fn_idx as usize]
            .upvalues
            .clone();
        let mut captured = Vec::with_capacity(uv_descs.len());
        for desc in uv_descs {
            if desc.is_local {
                let abs_reg = base + usize::from(desc.index);
                let cell = self.capture_open_upvalue(abs_reg);
                captured.push(cell);
            } else {
                let parent_cell = self.frame_upvalue(frame_idx, usize::from(desc.index))?;
                captured.push(parent_cell);
            }
        }
        Ok(captured)
    }

    fn init_class_instance(
        &mut self,
        ctor_handle: HeapHandle,
        instance: HeapHandle,
    ) -> Result<(), JsException> {
        let Some(init) = self
            .heap
            .get_object(ctor_handle)
            .and_then(|o| o.rare.as_ref())
            .and_then(|r| r.class_instance_init)
        else {
            return Ok(());
        };
        // Copy private methods & private accessors defined on the class prototype onto the instance.
        let proto_privates: Vec<(PropertyKey, PrivateEntry)> = self
            .heap
            .get_object(init.proto)
            .and_then(|o| o.rare.as_ref())
            .map(|r| r.private_fields.iter().map(|(&k, &v)| (k, v)).collect())
            .unwrap_or_default();
        if !proto_privates.is_empty() {
            for (key, entry) in proto_privates {
                let already = self
                    .heap
                    .get_object(instance)
                    .and_then(|o| o.rare.as_ref())
                    .is_some_and(|r| r.private_fields.contains_key(&key));
                if already {
                    return Err(self.type_error(
                        "Cannot initialize private elements twice on the same object",
                    ));
                }
                if let Some(o) = self.heap.get_object_mut(instance) {
                    o.rare_mut().private_fields.insert(key, entry);
                }
                match entry {
                    PrivateEntry::Field(v) => self.heap.write_barrier(instance, v),
                    PrivateEntry::Method(h) => {
                        self.heap.write_barrier(instance, JsValue::from_object(h))
                    }
                    PrivateEntry::Accessor(g, s) => {
                        if let Some(h) = g {
                            self.heap.write_barrier(instance, JsValue::from_object(h));
                        }
                        if let Some(h) = s {
                            self.heap.write_barrier(instance, JsValue::from_object(h));
                        }
                    }
                }
            }
        }
        // Run instance field initializer closure (with already-captured upvalues).
        if let Some(field_init_fn) = init.field_init_fn {
            self.call_handle(
                field_init_fn,
                JsValue::from_object(instance),
                &[],
                None,
                JsValue::UNDEFINED,
            )?;
        }
        Ok(())
    }

    fn instantiate_class(
        &mut self,
        prog_id: usize,
        func_idx: usize,
        const_idx: usize,
        super_val: JsValue,
        frame_idx: usize,
        base: usize,
    ) -> Result<HeapHandle, JsException> {
        let bp = match self.programs[prog_id].functions[func_idx]
            .constants
            .get(const_idx)
        {
            Some(ConstantEntry::Class(bp)) => bp.clone(),
            _ => return Err(self.type_error("Expected Class constant")),
        };

        let super_ctor = if bp.has_super {
            match super_val.as_object() {
                Some(h) => Some(h),
                None if super_val.is_null() => None,
                None => {
                    return Err(self.type_error("Class extends value is not a constructor or null"))
                }
            }
        } else {
            None
        };

        let proto_parent = if let Some(sup) = super_ctor {
            self.get_property_by_key(sup, self.key_prototype)?
                .as_object()
                .unwrap_or(self.object_proto)
        } else {
            self.object_proto
        };

        let class_proto = self.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(proto_parent),
        ));
        self.heap.get_object_mut(class_proto).unwrap().is_prototype = true;

        // Pre-instantiate instance field initializer closure so any upvalues from the enclosing
        // lexical scope are captured at class definition time.
        let field_init_fn = if let Some(fi_idx) = bp.instance_field_init_idx {
            let upvalues = self.capture_function_upvalues(prog_id, fi_idx, frame_idx, base)?;
            let fn_h = self.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Closure(Box::new(ClosureObject {
                    program_id: prog_id as u32,
                    func_idx: fi_idx,
                    upvalues,
                    bound_this: None,
                    super_ctor,
                    field_init: None,
                })),
                Some(self.function_proto),
            ));
            Some(fn_h)
        } else {
            None
        };

        let ctor_length = bp
            .constructor_idx
            .map_or(0, |ci| self.programs[prog_id].functions[ci as usize].length);
        let ctor_handle = if let Some(ctor_idx) = bp.constructor_idx {
            let upvalues = self.capture_function_upvalues(prog_id, ctor_idx, frame_idx, base)?;
            let mut obj = JsObject::with_kind(
                ObjectKind::Closure(Box::new(ClosureObject {
                    program_id: prog_id as u32,
                    func_idx: ctor_idx,
                    upvalues,
                    bound_this: None,
                    super_ctor,
                    field_init: None,
                })),
                Some(super_ctor.unwrap_or(self.function_proto)),
            );
            obj.is_constructor = true;
            self.heap.alloc_object(obj)
        } else {
            // Synthesize default constructor invoking `super` + instance element init if derived.
            let sup_copy = super_ctor;
            let ctor_cell = Rc::new(std::cell::Cell::new(None::<HeapHandle>));
            let ctor_cell_clone = Rc::clone(&ctor_cell);
            let h = self.alloc_host_fn(
                bp.name.as_deref().unwrap_or(""),
                move |vm, this_val, args| {
                    if let Some(sup) = sup_copy {
                        let nt = ctor_cell_clone.get().map_or(this_val, JsValue::from_object);
                        let sup_res =
                            vm.call_handle(sup, this_val, args, this_val.as_object(), nt)?;
                        let actual_this = if let Some(res_h) = sup_res.as_object() {
                            if matches!(
                                vm.heap.get_object(sup).map(|o| &o.kind),
                                Some(ObjectKind::NativeFunction(_))
                            ) {
                                if let Some(tp) = this_val
                                    .as_object()
                                    .and_then(|t| vm.heap.get_object(t))
                                    .and_then(|o| o.prototype)
                                {
                                    if let Some(o) = vm.heap.get_object_mut(res_h) {
                                        o.prototype = Some(tp);
                                    }
                                }
                            }
                            sup_res
                        } else {
                            this_val
                        };
                        if let (Some(c_h), Some(actual_obj)) =
                            (ctor_cell_clone.get(), actual_this.as_object())
                        {
                            vm.init_class_instance(c_h, actual_obj)?;
                        }
                        return Ok(actual_this);
                    }
                    Ok(this_val)
                },
            );
            self.mark_constructor(h);
            ctor_cell.set(Some(h));
            if let Some(sup) = super_ctor {
                self.heap
                    .object_set_prototype(h, Some(sup), &mut self.shapes);
            }
            h
        };

        // Attach class instance initialization metadata to constructor (traced by GC).
        if let Some(o) = self.heap.get_object_mut(ctor_handle) {
            o.rare_mut().class_instance_init = Some(ClassInstanceInit {
                proto: class_proto,
                field_init_fn,
                is_derived: super_ctor.is_some(),
            });
        }
        self.heap
            .write_barrier(ctor_handle, JsValue::from_object(class_proto));
        if let Some(fi_h) = field_init_fn {
            self.heap
                .write_barrier(ctor_handle, JsValue::from_object(fi_h));
        }

        self.heap.object_define_data(
            ctor_handle,
            self.key_length,
            JsValue::from_i32(i32::from(ctor_length)),
            PropertyAttrs::CONFIGURABLE,
            &mut self.shapes,
        );
        let cls_name_str = bp.name.as_deref().unwrap_or("");
        let nv = JsValue::from_string(self.heap.alloc_string(cls_name_str));
        self.heap.object_define_data(
            ctor_handle,
            self.key_name,
            nv,
            PropertyAttrs::CONFIGURABLE,
            &mut self.shapes,
        );
        self.heap.object_define_data(
            ctor_handle,
            self.key_prototype,
            JsValue::from_object(class_proto),
            PropertyAttrs::empty(),
            &mut self.shapes,
        );
        self.heap.object_define_data(
            class_proto,
            self.key_constructor,
            JsValue::from_object(ctor_handle),
            PropertyAttrs::METHOD,
            &mut self.shapes,
        );

        for m in bp.methods {
            let (m_len, m_name) = {
                let f = &self.programs[prog_id].functions[m.func_idx as usize];
                (f.length, f.name.clone().unwrap_or_else(|| m.name.clone()))
            };
            let upvalues = self.capture_function_upvalues(prog_id, m.func_idx, frame_idx, base)?;
            let method_fn = self.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Closure(Box::new(ClosureObject {
                    program_id: prog_id as u32,
                    func_idx: m.func_idx,
                    upvalues,
                    bound_this: None,
                    super_ctor,
                    field_init: None,
                })),
                Some(self.function_proto),
            ));
            self.heap.object_define_data(
                method_fn,
                self.key_length,
                JsValue::from_i32(i32::from(m_len)),
                PropertyAttrs::CONFIGURABLE,
                &mut self.shapes,
            );
            let mnv = JsValue::from_string(self.heap.alloc_string(m_name));
            self.heap.object_define_data(
                method_fn,
                self.key_name,
                mnv,
                PropertyAttrs::CONFIGURABLE,
                &mut self.shapes,
            );
            let target = if m.is_static {
                ctor_handle
            } else {
                class_proto
            };
            let key = self.heap.intern_key(&m.name);
            if m.is_private {
                if let Some(o) = self.heap.get_object_mut(target) {
                    let map = &mut o.rare_mut().private_fields;
                    match m.kind {
                        SerializableMethodKind::Get => {
                            let existing_set = match map.get(&key) {
                                Some(PrivateEntry::Accessor(_, s)) => *s,
                                _ => None,
                            };
                            map.insert(key, PrivateEntry::Accessor(Some(method_fn), existing_set));
                        }
                        SerializableMethodKind::Set => {
                            let existing_get = match map.get(&key) {
                                Some(PrivateEntry::Accessor(g, _)) => *g,
                                _ => None,
                            };
                            map.insert(key, PrivateEntry::Accessor(existing_get, Some(method_fn)));
                        }
                        SerializableMethodKind::Method | SerializableMethodKind::Constructor => {
                            map.insert(key, PrivateEntry::Method(method_fn));
                        }
                    }
                }
                self.heap
                    .write_barrier(target, JsValue::from_object(method_fn));
                continue;
            }
            match m.kind {
                SerializableMethodKind::Get => {
                    self.heap.object_define_accessor(
                        target,
                        key,
                        Some(method_fn),
                        None,
                        PropertyAttrs::CONFIGURABLE,
                        &mut self.shapes,
                    );
                }
                SerializableMethodKind::Set => {
                    self.heap.object_define_accessor(
                        target,
                        key,
                        None,
                        Some(method_fn),
                        PropertyAttrs::CONFIGURABLE,
                        &mut self.shapes,
                    );
                }
                SerializableMethodKind::Method | SerializableMethodKind::Constructor => {
                    self.heap.object_define_data(
                        target,
                        key,
                        JsValue::from_object(method_fn),
                        PropertyAttrs::METHOD,
                        &mut self.shapes,
                    );
                }
            }
        }

        if let Some(static_fn_idx) = bp.static_init_idx {
            let upvalues =
                self.capture_function_upvalues(prog_id, static_fn_idx, frame_idx, base)?;
            let init_fn = self.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Closure(Box::new(ClosureObject {
                    program_id: prog_id as u32,
                    func_idx: static_fn_idx,
                    upvalues,
                    bound_this: None,
                    super_ctor,
                    field_init: None,
                })),
                Some(self.function_proto),
            ));
            self.call_handle(
                init_fn,
                JsValue::from_object(ctor_handle),
                &[],
                None,
                JsValue::UNDEFINED,
            )?;
        }

        Ok(ctor_handle)
    }

    // -----------------------------------------------------------------------
    // Iterators, Operators, Coercions & Errors
    // -----------------------------------------------------------------------

    fn set_function_name_for_key(
        &mut self,
        fn_h: HeapHandle,
        raw_key: JsValue,
        key_str: &str,
        prefix: &str,
    ) {
        let base_name = if let Some(sym_id) = raw_key.as_symbol() {
            match self.symbol_descriptions.get(&sym_id) {
                Some(desc) if !desc.is_empty() => format!("[{desc}]"),
                _ => String::new(),
            }
        } else {
            key_str.to_owned()
        };
        let full = if prefix.is_empty() {
            base_name
        } else {
            format!("{prefix}{base_name}")
        };
        let nv = JsValue::from_string(self.heap.alloc_string(full));
        self.heap.object_define_data(
            fn_h,
            self.key_name,
            nv,
            PropertyAttrs::CONFIGURABLE,
            &mut self.shapes,
        );
    }

    pub fn iterator_close_obj(
        &mut self,
        iter_obj: HeapHandle,
        abrupt: bool,
    ) -> Result<(), JsException> {
        self.iterator_close_custom(iter_obj, abrupt)
    }

    pub fn iterator_close_handle(
        &mut self,
        iter_h: HeapHandle,
        abrupt: bool,
    ) -> Result<(), JsException> {
        let state = self.iterators.remove(&iter_h);
        if let Some(
            IteratorState::Custom {
                iter_obj,
                exhausted: false,
                ..
            }
            | IteratorState::AsyncCustom { iter_obj },
        ) = state
        {
            return self.iterator_close_custom(iter_obj, abrupt);
        }
        Ok(())
    }

    fn iterator_close_custom(
        &mut self,
        iter_obj: HeapHandle,
        abrupt: bool,
    ) -> Result<(), JsException> {
        let ret_res = self.get_property_by_key(iter_obj, self.key_return);
        let ret_fn = match ret_res {
            Ok(v) => v,
            Err(e) => return if abrupt { Ok(()) } else { Err(e) },
        };
        if ret_fn.is_undefined() || ret_fn.is_null() {
            return Ok(());
        }
        let call_res = self.call_value(ret_fn, JsValue::from_object(iter_obj), &[]);
        match call_res {
            Ok(inner) => {
                if !abrupt && !inner.is_object() {
                    return Err(self.type_error("Iterator result is not an object"));
                }
                Ok(())
            }
            Err(e) => {
                if abrupt {
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }

    fn create_iterator(&mut self, iterable: JsValue) -> Result<HeapHandle, JsException> {
        if let Some(s_h) = iterable.as_string() {
            let chars: Vec<char> = self
                .heap
                .materialize_string(s_h)
                .unwrap_or_default()
                .chars()
                .collect();
            let h = self.alloc_ordinary_object();
            self.iterators
                .insert(h, IteratorState::String { chars, index: 0 });
            return Ok(h);
        }
        let Some(obj) = iterable.as_object() else {
            return Err(self.type_error("Value is not iterable"));
        };
        let is_unmodified_array = matches!(
            self.heap.get_object(obj).map(|o| &o.kind),
            Some(ObjectKind::Array)
        ) && self.heap.get_object(obj).and_then(|o| o.prototype)
            == Some(self.array_proto)
            && self
                .heap
                .object_get_own(obj, self.key_symbol_iterator, &self.shapes)
                .is_none()
            && self
                .heap
                .object_get_own(self.array_proto, self.key_symbol_iterator, &self.shapes)
                .is_some_and(|p| {
                    !p.attrs.contains(PropertyAttrs::ACCESSOR)
                        && p.value.as_object() == self.default_array_iter_fn
                });
        if is_unmodified_array {
            let h = self.alloc_ordinary_object();
            self.iterators
                .insert(h, IteratorState::Array { arr: obj, index: 0 });
            return Ok(h);
        }
        // Check `[Symbol.iterator]()` protocol (Map, Set, TypedArray, generators, user iterables).
        let iter_method = self.get_property_by_key(obj, self.key_symbol_iterator)?;
        if !iter_method.is_undefined() && !iter_method.is_null() {
            let iter_res = self.call_value(iter_method, JsValue::from_object(obj), &[])?;
            let Some(iter_obj) = iter_res.as_object() else {
                return Err(self.type_error("Result of Symbol.iterator method is not an object"));
            };
            let h = self.alloc_ordinary_object();
            self.iterators.insert(
                h,
                IteratorState::Custom {
                    iter_obj,
                    exhausted: false,
                },
            );
            return Ok(h);
        }
        Err(self.type_error("Object is not iterable"))
    }

    fn step_iterator(&mut self, iter_val: JsValue) -> Result<(JsValue, bool), JsException> {
        self.step_iterator_ex(iter_val, false)
    }

    fn step_iterator_ex(
        &mut self,
        iter_val: JsValue,
        read_value_on_done: bool,
    ) -> Result<(JsValue, bool), JsException> {
        let Some(h) = iter_val.as_object() else {
            return Ok((JsValue::UNDEFINED, true));
        };
        let state = self.iterators.get(&h).cloned();
        match state {
            Some(IteratorState::Array { arr, index }) => {
                let len = self.heap.get_object(arr).map_or(0, |o| o.elements.len());
                if index >= len {
                    return Ok((JsValue::UNDEFINED, true));
                }
                let val = self
                    .heap
                    .get_object(arr)
                    .and_then(|o| o.elements.get(index))
                    .unwrap_or(JsValue::UNDEFINED);
                if let Some(IteratorState::Array { index: idx, .. }) = self.iterators.get_mut(&h) {
                    *idx += 1;
                }
                Ok((val, false))
            }
            Some(IteratorState::String { chars, index }) => {
                if index >= chars.len() {
                    return Ok((JsValue::UNDEFINED, true));
                }
                let ch = chars[index];
                if let Some(IteratorState::String { index: idx, .. }) = self.iterators.get_mut(&h) {
                    *idx += 1;
                }
                Ok((
                    JsValue::from_string(self.heap.alloc_string(ch.to_string())),
                    false,
                ))
            }
            Some(IteratorState::Keys { keys, index }) => {
                if index >= keys.len() {
                    return Ok((JsValue::UNDEFINED, true));
                }
                let k = keys[index].clone();
                if let Some(IteratorState::Keys { index: idx, .. }) = self.iterators.get_mut(&h) {
                    *idx += 1;
                }
                Ok((JsValue::from_string(self.heap.alloc_string(k)), false))
            }
            Some(IteratorState::Custom {
                iter_obj,
                exhausted,
            }) => {
                if exhausted {
                    return Ok((JsValue::UNDEFINED, true));
                }
                let next_fn = self.get_property_by_key(iter_obj, self.key_next)?;
                let res = self.call_value(next_fn, JsValue::from_object(iter_obj), &[])?;
                let Some(res_h) = res.as_object() else {
                    return Err(self.type_error("Iterator result is not an object"));
                };
                let done_val = self.get_property_by_key(res_h, self.key_done)?;
                let done = self.to_boolean(done_val);
                if done {
                    if let Some(IteratorState::Custom { exhausted: ex, .. }) =
                        self.iterators.get_mut(&h)
                    {
                        *ex = true;
                    }
                    let val = if read_value_on_done {
                        self.get_property_by_key(res_h, self.key_value)?
                    } else {
                        JsValue::UNDEFINED
                    };
                    return Ok((val, true));
                }
                let val = self.get_property_by_key(res_h, self.key_value)?;
                Ok((val, false))
            }
            Some(IteratorState::AsyncCustom { iter_obj }) => {
                let next_fn = self.get_property_by_key(iter_obj, self.key_next)?;
                let res = self.call_value(next_fn, JsValue::from_object(iter_obj), &[])?;
                let Some(res_h) = res.as_object() else {
                    return Err(self.type_error("Iterator result is not an object"));
                };
                let done_val = self.get_property_by_key(res_h, self.key_done)?;
                let done = self.to_boolean(done_val);
                let val = self.get_property_by_key(res_h, self.key_value)?;
                Ok((val, done))
            }
            None => Ok((JsValue::UNDEFINED, true)),
        }
    }

    fn create_async_iterator(&mut self, iterable: JsValue) -> Result<HeapHandle, JsException> {
        if let Some(obj) = iterable.as_object() {
            let async_method = self.get_property_by_key(obj, self.key_symbol_async_iterator)?;
            if !async_method.is_undefined() && !async_method.is_null() {
                let iter_res = self.call_value(async_method, JsValue::from_object(obj), &[])?;
                let Some(iter_obj) = iter_res.as_object() else {
                    return Err(
                        self.type_error("Result of Symbol.asyncIterator method is not an object")
                    );
                };
                let h = self.alloc_ordinary_object();
                self.iterators
                    .insert(h, IteratorState::AsyncCustom { iter_obj });
                return Ok(h);
            }
        }
        // ECMA-262 §7.4.1 GetIterator(obj, async): fall back to sync iterator if no Symbol.asyncIterator.
        self.create_iterator(iterable)
    }

    fn step_async_iterator(&mut self, iter_val: JsValue) -> Result<JsValue, JsException> {
        let Some(h) = iter_val.as_object() else {
            return Ok(crate::promise::make_iter_result(
                self,
                JsValue::UNDEFINED,
                true,
            ));
        };
        if let Some(IteratorState::AsyncCustom { iter_obj }) = self.iterators.get(&h).cloned() {
            let next_fn = self.get_property_by_key(iter_obj, self.key_next)?;
            return self.call_value(next_fn, JsValue::from_object(iter_obj), &[]);
        }
        // Async-from-Sync Iterator adapter (ECMA-262 §27.1.4.2.1): unwrap awaited value into `{ value, done }`.
        let (val, done) = self.step_iterator(iter_val)?;
        let p = crate::promise::promise_resolve_value(self, val);
        let on_ful = self.alloc_host_fn("async_from_sync_next", move |vm, _this, args| {
            let v = args.first().copied().unwrap_or(JsValue::UNDEFINED);
            Ok(crate::promise::make_iter_result(vm, v, done))
        });
        let res_p = crate::promise::promise_then(self, p, Some(on_ful), None);
        Ok(JsValue::from_object(res_p))
    }

    pub fn collect_iterable(&mut self, iterable: JsValue) -> Result<Vec<JsValue>, JsException> {
        let iter = self.create_iterator(iterable)?;
        let iter_val = JsValue::from_object(iter);
        let mut out = Vec::new();
        let max_items = self.max_loop_iterations.unwrap_or(4_000_000).min(4_000_000) as usize;
        loop {
            if out.len() >= max_items {
                self.iterators.remove(&iter);
                return Err(self.range_error("Maximum iterable length exceeded"));
            }
            self.loop_iterations = self.loop_iterations.saturating_add(1);
            if let Some(limit) = self.max_loop_iterations {
                if self.loop_iterations > limit {
                    self.iterators.remove(&iter);
                    return Err(self.range_error("Script execution iteration limit exceeded"));
                }
            }
            let (v, done) = self.step_iterator(iter_val)?;
            if done {
                break;
            }
            out.push(v);
        }
        self.iterators.remove(&iter);
        Ok(out)
    }

    fn collect_array_elements(&self, arr_val: JsValue) -> Vec<JsValue> {
        let Some(h) = arr_val.as_object() else {
            return Vec::new();
        };
        let Some(obj) = self.heap.get_object(h) else {
            return Vec::new();
        };
        let len = obj.elements.len().min(1_000_000);
        (0..len)
            .map(|i| obj.elements.get(i).unwrap_or(JsValue::UNDEFINED))
            .collect()
    }

    pub fn own_enumerable_keys(&self, obj: HeapHandle) -> Vec<PropertyKey> {
        let Some(o) = self.heap.get_object(obj) else {
            return Vec::new();
        };
        if o.is_dictionary() {
            let Some(rare) = &o.rare else {
                return Vec::new();
            };
            return rare
                .dict_order
                .iter()
                .copied()
                .filter(|k| {
                    rare.dict_props
                        .get(k)
                        .is_some_and(|p| p.attrs.contains(PropertyAttrs::ENUMERABLE))
                })
                .collect();
        }
        let Some(shape) = self.shapes.get(o.shape()) else {
            return Vec::new();
        };
        shape
            .keys()
            .iter()
            .copied()
            .filter(|&k| {
                shape
                    .lookup(k)
                    .is_some_and(|s| s.attrs.contains(PropertyAttrs::ENUMERABLE))
            })
            .collect()
    }

    pub fn own_enumerable_keys_including_elements(&self, obj: HeapHandle) -> Vec<String> {
        let (mut idx_keys, mut str_keys) = (Vec::new(), Vec::new());
        if let Some(o) = self.heap.get_object(obj) {
            if let ObjectKind::TypedArray(ta) = o.kind {
                let eff = crate::builtins_atomics::ta_effective_len(self, obj, ta).unwrap_or(0);
                idx_keys.extend(0..eff);
            } else {
                idx_keys.extend(o.elements.present_indices());
            }
        }
        for k in self.own_enumerable_keys(obj) {
            let Some(s) = self.heap.interner().get(k) else {
                continue;
            };
            if is_symbol_key_str(s) {
                continue;
            }
            if let Some(idx) = parse_array_index_str(s) {
                if !idx_keys.contains(&idx) {
                    idx_keys.push(idx);
                }
            } else {
                str_keys.push(s.to_string());
            }
        }
        idx_keys.sort_unstable();
        idx_keys.dedup();
        idx_keys
            .into_iter()
            .map(|i| i.to_string())
            .chain(str_keys)
            .collect()
    }

    /// ECMA-262 §7.1.18 `ToObject`: boxes primitives and rejects `null`/`undefined`.
    pub fn to_object(&mut self, val: JsValue) -> Result<HeapHandle, JsException> {
        if val.is_null() || val.is_undefined() {
            return Err(self.type_error("Cannot convert undefined or null to object"));
        }
        if let Some(h) = val.as_object() {
            return Ok(h);
        }
        let proto = match val.kind() {
            JsValueKind::String(_) => self.string_proto,
            JsValueKind::Int32(_) | JsValueKind::Float(_) => self.number_proto,
            JsValueKind::Bool(_) => self.boolean_proto,
            JsValueKind::BigInt(_) => self.bigint_proto,
            JsValueKind::Symbol(_) => self.symbol_proto,
            _ => self.object_proto,
        };
        let h = self
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        if let Some(o) = self.heap.get_object_mut(h) {
            o.rare_mut().boxed_value = Some(val);
        }
        if let Some(s) = val
            .as_string()
            .and_then(|sh| self.heap.materialize_string(sh))
        {
            let units: Vec<u16> = s.encode_utf16().collect();
            let len = units.len() as f64;
            for (i, u) in units.into_iter().enumerate() {
                let ch = char::from_u32(u32::from(u))
                    .map_or_else(|| "\u{FFFD}".into(), |c| c.to_string());
                let cv = JsValue::from_string(self.heap.alloc_string(ch));
                let k = self.heap.intern_key(&i.to_string());
                self.heap
                    .object_define_data(h, k, cv, PropertyAttrs::ENUMERABLE, &mut self.shapes);
            }
            self.heap.object_define_data(
                h,
                self.key_length,
                JsValue::from_f64(len),
                PropertyAttrs::empty(),
                &mut self.shapes,
            );
        }
        self.heap.write_barrier(h, val);
        Ok(h)
    }

    /// ECMA-262 §7.1.5 `ToIntegerOrInfinity`.
    pub fn to_integer_or_infinity(&mut self, val: JsValue) -> Result<f64, JsException> {
        let n = self.to_number(val)?;
        Ok(if n.is_nan() || n == 0.0 {
            0.0
        } else if n.is_infinite() {
            n
        } else {
            n.trunc()
        })
    }

    /// ECMA-262 §7.1.22 `ToLength` (clamped to `0..=2^53 - 1`).
    pub fn to_length(&mut self, val: JsValue) -> Result<u64, JsException> {
        let n = self.to_integer_or_infinity(val)?;
        Ok(if n <= 0.0 {
            0
        } else {
            n.min(9_007_199_254_740_991.0) as u64
        })
    }

    /// ECMA-262 §10.4.2.4 `ArraySetLength` validation & truncation.
    pub fn set_array_length_checked(
        &mut self,
        obj: HeapHandle,
        val: JsValue,
    ) -> Result<bool, JsException> {
        let (new_len, num) = (self.to_u32(val)?, self.to_number(val)?);
        if f64::from(new_len) != num {
            return Err(self.range_error("Invalid array length"));
        }
        let cur_len = self.heap.get_object(obj).map_or(0, |o| o.elements.len());
        if self
            .heap
            .object_get_own(obj, self.key_length, &self.shapes)
            .is_some_and(|lp| !lp.attrs.contains(PropertyAttrs::WRITABLE))
        {
            return Ok(new_len == cur_len);
        }
        if new_len < cur_len {
            let (mut highest_non_cfg, mut to_delete) = (None::<u32>, Vec::new());
            if let Some(r) = self
                .heap
                .get_object(obj)
                .filter(|o| o.is_dictionary())
                .and_then(|o| o.rare.as_ref())
            {
                for (&k, p) in &r.dict_props {
                    if let Some(idx) = self
                        .heap
                        .interner()
                        .get(k)
                        .and_then(|s| parse_array_index_str(s))
                        .filter(|&i| i >= new_len)
                    {
                        if !p.attrs.contains(PropertyAttrs::CONFIGURABLE) {
                            highest_non_cfg = Some(highest_non_cfg.map_or(idx, |m| m.max(idx)));
                        } else {
                            to_delete.push((k, idx));
                        }
                    }
                }
            }
            if let Some(max_idx) = highest_non_cfg {
                let stop_len = max_idx.saturating_add(1);
                for (k, idx) in to_delete {
                    if idx >= stop_len {
                        self.heap.object_delete_own(obj, k, &mut self.shapes);
                    }
                }
                if let Some(o) = self.heap.get_object_mut(obj) {
                    o.elements.set_len(stop_len);
                }
                return Ok(false);
            }
            for (k, _) in to_delete {
                self.heap.object_delete_own(obj, k, &mut self.shapes);
            }
        }
        if let Some(o) = self.heap.get_object_mut(obj) {
            o.elements.set_len(new_len);
        }
        Ok(true)
    }

    /// ECMA-262 §7.2.2 `IsArray` (unwraps Proxy chains, throws `TypeError` on revoked Proxy).
    pub fn is_array_value(&self, val: JsValue) -> Result<bool, JsException> {
        let Some(mut h) = val.as_object() else {
            return Ok(false);
        };
        for _ in 0..64 {
            match self.heap.get_object(h).map(|o| &o.kind) {
                Some(ObjectKind::Array) => return Ok(true),
                Some(ObjectKind::Proxy(p)) if p.revoked => {
                    return Err(JsException {
                        value: JsValue::UNDEFINED,
                        message:
                            "TypeError: Cannot perform 'IsArray' on a proxy that has been revoked"
                                .into(),
                    });
                }
                Some(ObjectKind::Proxy(p)) => h = p.target,
                _ => return Ok(false),
            }
        }
        Ok(false)
    }

    /// ECMA-262 §10.4.2.3 `ArraySpeciesCreate(originalArray, length)`.
    pub fn array_species_create(
        &mut self,
        orig: HeapHandle,
        len: u64,
    ) -> Result<HeapHandle, JsException> {
        let is_arr = self
            .is_array_value(JsValue::from_object(orig))
            .map_err(|_| self.type_error("Proxy has been revoked"))?;
        let mut c = JsValue::UNDEFINED;
        if is_arr {
            c = self.get_property_by_key(orig, self.key_constructor)?;
            if let Some(ch) = c.as_object() {
                c = self.get_property_by_key(ch, self.key_symbol_species)?;
                if c.is_null() {
                    c = JsValue::UNDEFINED;
                }
            }
        }
        if c.is_undefined() {
            if len > u64::from(u32::MAX) {
                return Err(self.range_error("Invalid array length"));
            }
            let arr = self.alloc_array();
            if len > 0 {
                if let Some(o) = self.heap.get_object_mut(arr) {
                    o.elements.set_len(len as u32);
                }
            }
            return Ok(arr);
        }
        if !c.as_object().is_some_and(|h| self.is_constructor_handle(h)) {
            return Err(self.type_error("Species constructor is not a constructor"));
        }
        self.construct_value(c, &[JsValue::from_f64(len as f64)], c)?
            .as_object()
            .ok_or_else(|| self.type_error("Species constructor returned non-object"))
    }

    pub fn array_get_len(&mut self, obj: HeapHandle) -> Result<u64, JsException> {
        let v = self.get_property_by_key(obj, self.key_length)?;
        self.to_length(v)
    }

    pub fn array_has_index(&mut self, obj: HeapHandle, idx: u64) -> Result<bool, JsException> {
        if idx < u64::from(u32::MAX)
            && self
                .heap
                .get_object(obj)
                .is_some_and(|o| matches!(o.kind, ObjectKind::Array) && o.elements.has(idx as u32))
        {
            return Ok(true);
        }
        let key = self.heap.intern_key(&idx.to_string());
        self.has_property_by_key(obj, key)
    }

    pub fn array_get_index(&mut self, obj: HeapHandle, idx: u64) -> Result<JsValue, JsException> {
        if idx < u64::from(u32::MAX) {
            if let Some(v) = self
                .heap
                .get_object(obj)
                .filter(|o| matches!(o.kind, ObjectKind::Array))
                .and_then(|o| o.elements.get(idx as u32))
            {
                return Ok(v);
            }
        }
        self.get_elem(JsValue::from_object(obj), JsValue::from_f64(idx as f64))
    }

    pub fn array_set_index_strict(
        &mut self,
        obj: HeapHandle,
        idx: u64,
        val: JsValue,
    ) -> Result<(), JsException> {
        if !self.set_elem_checked(
            JsValue::from_object(obj),
            JsValue::from_f64(idx as f64),
            val,
        )? {
            return Err(self.type_error(format!("Cannot assign to read only property '{idx}'")));
        }
        Ok(())
    }

    pub fn array_create_data_prop(
        &mut self,
        obj: HeapHandle,
        idx: u64,
        val: JsValue,
    ) -> Result<(), JsException> {
        let key = self.heap.intern_key(&idx.to_string());
        let is_arr = matches!(
            self.heap.get_object(obj).map(|o| &o.kind),
            Some(ObjectKind::Array)
        );
        if let Some(cur) = self.heap.object_get_own(obj, key, &self.shapes) {
            if !cur.attrs.contains(PropertyAttrs::CONFIGURABLE) {
                return Err(self.type_error("Cannot redefine non-configurable property"));
            }
        } else if !self.heap.get_object(obj).is_some_and(|o| o.extensible) {
            return Err(self.type_error("Cannot define property on non-extensible object"));
        } else if is_arr && idx < u64::from(u32::MAX) {
            if let Some(o) = self.heap.get_object_mut(obj) {
                o.elements.set(idx as u32, val);
            }
            self.heap.write_barrier(obj, val);
            return Ok(());
        }
        self.heap
            .object_define_data(obj, key, val, PropertyAttrs::DEFAULT, &mut self.shapes);
        Ok(())
    }

    pub fn array_delete_index_strict(
        &mut self,
        obj: HeapHandle,
        idx: u64,
    ) -> Result<(), JsException> {
        if !self.delete_elem(JsValue::from_object(obj), JsValue::from_f64(idx as f64))? {
            return Err(self.type_error(format!("Cannot delete property '{idx}'")));
        }
        Ok(())
    }

    pub fn array_set_length(&mut self, obj: HeapHandle, new_len: u64) -> Result<(), JsException> {
        if !self.set_property_by_key(obj, self.key_length, JsValue::from_f64(new_len as f64))? {
            return Err(self.type_error("Cannot assign to read only property 'length'"));
        }
        Ok(())
    }

    /// Returns sorted present indices `< len` when `len > 65_536` and it is spec-safe to skip holes.
    pub fn sparse_present_indices_if_safe(&self, obj: HeapHandle, len: u64) -> Option<Vec<u64>> {
        if len <= 65_536 {
            return None;
        }
        let o = self.heap.get_object(obj)?;
        if matches!(o.kind, ObjectKind::Proxy(_)) {
            return None;
        }
        let has_idx_key = |k: PropertyKey| {
            self.heap
                .interner()
                .get(k)
                .is_some_and(|s| parse_array_index_str(s).is_some())
        };
        let mut cur = o.prototype;
        for _ in 0..64 {
            let Some(ph) = cur else { break };
            let po = self.heap.get_object(ph)?;
            if matches!(po.kind, ObjectKind::Proxy(_)) || !po.elements.present_indices().is_empty()
            {
                return None;
            }
            let proto_has = if po.is_dictionary() {
                po.rare
                    .as_ref()
                    .is_some_and(|r| r.dict_props.keys().copied().any(has_idx_key))
            } else {
                self.shapes
                    .get(po.shape())
                    .is_some_and(|sh| sh.keys().iter().copied().any(has_idx_key))
            };
            if proto_has {
                return None;
            }
            cur = po.prototype;
        }
        let mut out: Vec<u64> = o
            .elements
            .present_indices()
            .into_iter()
            .map(u64::from)
            .filter(|&i| i < len)
            .collect();
        let mut push_key = |k: PropertyKey| {
            if let Some(s) = self.heap.interner().get(k) {
                if let Some(i) = s
                    .parse::<u64>()
                    .ok()
                    .filter(|&i| i < len && i.to_string() == s.as_ref())
                {
                    out.push(i);
                }
            }
        };
        if o.is_dictionary() {
            if let Some(r) = &o.rare {
                r.dict_props.keys().copied().for_each(&mut push_key);
            }
        } else if let Some(sh) = self.shapes.get(o.shape()) {
            sh.keys().iter().copied().for_each(&mut push_key);
        }
        out.sort_unstable();
        out.dedup();
        Some(out)
    }

    fn op_add(&mut self, lhs: JsValue, rhs: JsValue) -> Result<JsValue, JsException> {
        match (lhs.kind(), rhs.kind()) {
            (JsValueKind::Int32(a), JsValueKind::Int32(b)) => {
                return Ok(JsValue::add_i32_overflow(a, b));
            }
            (JsValueKind::Float(a), JsValueKind::Float(b)) => return Ok(JsValue::from_f64(a + b)),
            (JsValueKind::Int32(a), JsValueKind::Float(b)) => {
                return Ok(JsValue::from_f64(f64::from(a) + b));
            }
            (JsValueKind::Float(a), JsValueKind::Int32(b)) => {
                return Ok(JsValue::from_f64(a + f64::from(b)));
            }
            (JsValueKind::String(a), JsValueKind::String(b)) => {
                let rope = self.heap.alloc_rope(a, b).unwrap_or(a);
                return Ok(JsValue::from_string(rope));
            }
            _ => {}
        }
        let lprim = if lhs.is_object() {
            self.to_primitive(lhs, PrimHint::Default)?
        } else {
            lhs
        };
        let rprim = if rhs.is_object() {
            self.to_primitive(rhs, PrimHint::Default)?
        } else {
            rhs
        };
        if lprim.is_string() || rprim.is_string() {
            let ls = self.to_js_string(lprim)?;
            let rs = self.to_js_string(rprim)?;
            return Ok(JsValue::from_string(
                self.heap.alloc_string(format!("{ls}{rs}")),
            ));
        }
        if let (Some(a), Some(b)) = (lprim.as_bigint(), rprim.as_bigint()) {
            let sum = self
                .heap
                .get_bigint(a)
                .unwrap()
                .add(self.heap.get_bigint(b).unwrap());
            return Ok(JsValue::from_bigint(self.heap.alloc_bigint(sum)));
        }
        if lprim.is_bigint() || rprim.is_bigint() {
            return Err(self.type_error("Cannot mix BigInt and other types"));
        }
        let l = self.to_number(lprim)?;
        let r = self.to_number(rprim)?;
        Ok(JsValue::from_f64(l + r))
    }

    fn coerce_numeric(&mut self, val: JsValue) -> Result<JsValue, JsException> {
        let p = if val.is_object() {
            self.to_primitive(val, PrimHint::Number)?
        } else {
            val
        };
        if p.as_bigint().is_some() {
            Ok(p)
        } else {
            Ok(JsValue::from_f64(self.to_number(p)?))
        }
    }

    fn op_numeric_bin(
        &mut self,
        lhs: JsValue,
        rhs: JsValue,
        op: Opcode,
    ) -> Result<JsValue, JsException> {
        let ln = self.coerce_numeric(lhs)?;
        let rn = self.coerce_numeric(rhs)?;
        match (ln.as_bigint(), rn.as_bigint()) {
            (Some(ah), Some(bh)) => {
                let a = self.heap.get_bigint(ah).cloned().unwrap_or_default();
                let b = self.heap.get_bigint(bh).cloned().unwrap_or_default();
                let res = match op {
                    Opcode::Div => a
                        .div_rem(&b)
                        .map(|(q, _)| q)
                        .ok_or_else(|| self.range_error("Division by zero"))?,
                    Opcode::Rem => a
                        .div_rem(&b)
                        .map(|(_, r)| r)
                        .ok_or_else(|| self.range_error("Division by zero"))?,
                    Opcode::Exp => a
                        .pow(&b)
                        .ok_or_else(|| self.range_error("Exponent must be positive"))?,
                    Opcode::BitAnd => a.bitwise_op(&b, 0),
                    Opcode::BitOr => a.bitwise_op(&b, 1),
                    Opcode::BitXor => a.bitwise_op(&b, 2),
                    Opcode::Shl => a
                        .shl(&b)
                        .ok_or_else(|| self.range_error("Maximum BigInt size exceeded"))?,
                    Opcode::Shr => a
                        .shr(&b)
                        .ok_or_else(|| self.range_error("Maximum BigInt size exceeded"))?,
                    Opcode::UShr => {
                        return Err(self.type_error("BigInts have no unsigned right shift"));
                    }
                    _ => unreachable!(),
                };
                Ok(JsValue::from_bigint(self.heap.alloc_bigint(res)))
            }
            (Some(_), None) | (None, Some(_)) => {
                Err(self.type_error("Cannot mix BigInt and other types"))
            }
            (None, None) => {
                let l = self.to_number(ln)?;
                let r = self.to_number(rn)?;
                Ok(match op {
                    Opcode::Div => JsValue::from_f64(l / r),
                    Opcode::Rem => JsValue::from_f64(l % r),
                    Opcode::Exp => {
                        // ECMA-262 §6.1.6.1.3: abs(base) == 1 && exp is ±Infinity => NaN.
                        if r.is_nan() || (l.abs() == 1.0 && r.is_infinite()) {
                            JsValue::NAN
                        } else if r == 0.0 {
                            JsValue::from_f64(1.0)
                        } else {
                            JsValue::from_f64(l.powf(r))
                        }
                    }
                    Opcode::BitAnd => JsValue::from_i32(f64_to_i32(l) & f64_to_i32(r)),
                    Opcode::BitOr => JsValue::from_i32(f64_to_i32(l) | f64_to_i32(r)),
                    Opcode::BitXor => JsValue::from_i32(f64_to_i32(l) ^ f64_to_i32(r)),
                    Opcode::Shl => {
                        JsValue::from_i32(f64_to_i32(l).wrapping_shl(f64_to_u32(r) & 0x1F))
                    }
                    Opcode::Shr => {
                        JsValue::from_i32(f64_to_i32(l).wrapping_shr(f64_to_u32(r) & 0x1F))
                    }
                    Opcode::UShr => JsValue::from_f64(f64::from(
                        f64_to_u32(l).wrapping_shr(f64_to_u32(r) & 0x1F),
                    )),
                    _ => unreachable!(),
                })
            }
        }
    }

    fn op_sub(&mut self, lhs: JsValue, rhs: JsValue) -> Result<JsValue, JsException> {
        if let (JsValueKind::Int32(a), JsValueKind::Int32(b)) = (lhs.kind(), rhs.kind()) {
            return Ok(JsValue::sub_i32_overflow(a, b));
        }
        let lp = self.coerce_numeric(lhs)?;
        let rp = self.coerce_numeric(rhs)?;
        if let (Some(a), Some(b)) = (lp.as_bigint(), rp.as_bigint()) {
            let diff = self
                .heap
                .get_bigint(a)
                .unwrap()
                .sub(self.heap.get_bigint(b).unwrap());
            return Ok(JsValue::from_bigint(self.heap.alloc_bigint(diff)));
        }
        if lp.as_bigint().is_some() || rp.as_bigint().is_some() {
            return Err(self.type_error("Cannot mix BigInt and other types"));
        }
        Ok(JsValue::from_f64(self.to_number(lp)? - self.to_number(rp)?))
    }

    fn op_mul(&mut self, lhs: JsValue, rhs: JsValue) -> Result<JsValue, JsException> {
        if let (JsValueKind::Int32(a), JsValueKind::Int32(b)) = (lhs.kind(), rhs.kind()) {
            return Ok(JsValue::mul_i32_overflow(a, b));
        }
        let lp = self.coerce_numeric(lhs)?;
        let rp = self.coerce_numeric(rhs)?;
        if let (Some(a), Some(b)) = (lp.as_bigint(), rp.as_bigint()) {
            let prod = self
                .heap
                .get_bigint(a)
                .unwrap()
                .mul(self.heap.get_bigint(b).unwrap());
            return Ok(JsValue::from_bigint(self.heap.alloc_bigint(prod)));
        }
        if lp.as_bigint().is_some() || rp.as_bigint().is_some() {
            return Err(self.type_error("Cannot mix BigInt and other types"));
        }
        Ok(JsValue::from_f64(self.to_number(lp)? * self.to_number(rp)?))
    }

    fn compare_bigint_f64(&self, bi_h: HeapHandle, n: f64) -> Option<std::cmp::Ordering> {
        if n.is_nan() {
            return None;
        }
        if n == f64::INFINITY {
            return Some(std::cmp::Ordering::Less);
        }
        if n == f64::NEG_INFINITY {
            return Some(std::cmp::Ordering::Greater);
        }
        let bi = self.heap.get_bigint(bi_h)?;
        let bi_f = bi.to_i64_lossy() as f64;
        if bi_f.abs() < 9_007_199_254_740_991.0 && n.abs() < 9_007_199_254_740_991.0 {
            bi_f.partial_cmp(&n)
        } else {
            let n_int = JsBigInt::from_i128(n.trunc() as i128);
            let ord = bi.cmp(&n_int);
            if ord != std::cmp::Ordering::Equal {
                Some(ord)
            } else if n.fract() > 0.0 {
                Some(std::cmp::Ordering::Less)
            } else if n.fract() < 0.0 {
                Some(std::cmp::Ordering::Greater)
            } else {
                Some(std::cmp::Ordering::Equal)
            }
        }
    }

    fn relational_cmp(
        &mut self,
        lhs: JsValue,
        rhs: JsValue,
    ) -> Result<Option<std::cmp::Ordering>, JsException> {
        if let (JsValueKind::Int32(a), JsValueKind::Int32(b)) = (lhs.kind(), rhs.kind()) {
            return Ok(Some(a.cmp(&b)));
        }
        let lp = if lhs.is_object() {
            self.to_primitive(lhs, PrimHint::Number)?
        } else {
            lhs
        };
        let rp = if rhs.is_object() {
            self.to_primitive(rhs, PrimHint::Number)?
        } else {
            rhs
        };
        if let (Some(a), Some(b)) = (lp.as_string(), rp.as_string()) {
            let sa = self.heap.materialize_string(a).unwrap_or_default();
            let sb = self.heap.materialize_string(b).unwrap_or_default();
            return Ok(Some(sa.encode_utf16().cmp(sb.encode_utf16())));
        }
        if let (Some(a), Some(b)) = (lp.as_bigint(), rp.as_bigint()) {
            let ba = self.heap.get_bigint(a).unwrap();
            let bb = self.heap.get_bigint(b).unwrap();
            return Ok(Some(ba.cmp(bb)));
        }
        if let Some(a) = lp.as_bigint() {
            if let Some(sh) = rp.as_string() {
                let s = self.heap.materialize_string(sh).unwrap_or_default();
                let Some(bb) = JsBigInt::parse_str(&s) else {
                    return Ok(None);
                };
                return Ok(Some(self.heap.get_bigint(a).unwrap().cmp(&bb)));
            }
            let r = self.to_number(rp)?;
            return Ok(self.compare_bigint_f64(a, r));
        }
        if let Some(b) = rp.as_bigint() {
            if let Some(sh) = lp.as_string() {
                let s = self.heap.materialize_string(sh).unwrap_or_default();
                let Some(ba) = JsBigInt::parse_str(&s) else {
                    return Ok(None);
                };
                return Ok(Some(ba.cmp(self.heap.get_bigint(b).unwrap())));
            }
            let l = self.to_number(lp)?;
            return Ok(self
                .compare_bigint_f64(b, l)
                .map(std::cmp::Ordering::reverse));
        }
        let l = self.to_number(lp)?;
        let r = self.to_number(rp)?;
        Ok(l.partial_cmp(&r))
    }

    fn op_lt(&mut self, lhs: JsValue, rhs: JsValue) -> Result<bool, JsException> {
        Ok(self.relational_cmp(lhs, rhs)? == Some(std::cmp::Ordering::Less))
    }

    fn op_lteq(&mut self, lhs: JsValue, rhs: JsValue) -> Result<bool, JsException> {
        Ok(matches!(
            self.relational_cmp(lhs, rhs)?,
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        ))
    }

    /// ECMA-262 §7.3.21 `OrdinaryHasInstance(C, O)`.
    pub fn ordinary_has_instance(
        &mut self,
        ctor: JsValue,
        val: JsValue,
    ) -> Result<bool, JsException> {
        let Some(mut ctor_h) = ctor.as_object() else {
            return Ok(false);
        };
        if !self.is_callable_handle(ctor_h) {
            return Ok(false);
        }
        while let Some(ObjectKind::BoundFunction(bf)) =
            self.heap.get_object(ctor_h).map(|o| o.kind.clone())
        {
            ctor_h = bf.target;
        }
        let Some(lhs_h) = val.as_object() else {
            return Ok(false);
        };
        let target_proto = self.get_property_by_key(ctor_h, self.key_prototype)?;
        let Some(target_proto_h) = target_proto.as_object() else {
            return Err(self.type_error("Function has non-object prototype in instanceof check"));
        };
        let mut cur = self.heap.get_object(lhs_h).and_then(|o| o.prototype);
        while let Some(p) = cur {
            if p == target_proto_h {
                return Ok(true);
            }
            cur = self.heap.get_object(p).and_then(|o| o.prototype);
        }
        Ok(false)
    }

    fn op_instanceof(&mut self, lhs: JsValue, rhs: JsValue) -> Result<bool, JsException> {
        let Some(rhs_h) = rhs.as_object() else {
            return Err(self.type_error("Right-hand side of 'instanceof' is not an object"));
        };
        let has_inst = self.get_property_by_key(rhs_h, self.key_symbol_has_instance)?;
        if !has_inst.is_undefined() && !has_inst.is_null() {
            let res = self.call_value(has_inst, rhs, &[lhs])?;
            return Ok(self.to_boolean(res));
        }
        if !self.is_callable_handle(rhs_h) {
            return Err(self.type_error("Right-hand side of 'instanceof' is not callable"));
        }
        self.ordinary_has_instance(rhs, lhs)
    }

    pub fn strict_eq(&self, a: JsValue, b: JsValue) -> bool {
        match (a.kind(), b.kind()) {
            (JsValueKind::String(ha), JsValueKind::String(hb)) => {
                self.heap.materialize_string(ha) == self.heap.materialize_string(hb)
            }
            (JsValueKind::BigInt(ha), JsValueKind::BigInt(hb)) => {
                self.heap.get_bigint(ha) == self.heap.get_bigint(hb)
            }
            _ => a.strict_eq_fast(b),
        }
    }

    pub fn abstract_eq(&mut self, a: JsValue, b: JsValue) -> Result<bool, JsException> {
        if (a.is_null() && b.is_undefined()) || (a.is_undefined() && b.is_null()) {
            return Ok(true);
        }
        if std::mem::discriminant(&a.kind()) == std::mem::discriminant(&b.kind())
            || (a.is_number() && b.is_number())
        {
            return Ok(self.strict_eq(a, b));
        }
        if a.is_bool() {
            return self.abstract_eq(JsValue::from_i32(i32::from(a.as_bool().unwrap())), b);
        }
        if b.is_bool() {
            return self.abstract_eq(a, JsValue::from_i32(i32::from(b.as_bool().unwrap())));
        }
        if a.is_object() && (b.is_number() || b.is_string() || b.is_symbol() || b.is_bigint()) {
            let ap = self.to_primitive(a, PrimHint::Default)?;
            return self.abstract_eq(ap, b);
        }
        if b.is_object() && (a.is_number() || a.is_string() || a.is_symbol() || a.is_bigint()) {
            let bp = self.to_primitive(b, PrimHint::Default)?;
            return self.abstract_eq(a, bp);
        }
        if a.is_number() && b.is_string() {
            let bn = self.to_number(b)?;
            return Ok(self.to_number(a)? == bn);
        }
        if a.is_string() && b.is_number() {
            let an = self.to_number(a)?;
            return Ok(an == self.to_number(b)?);
        }
        if let (Some(bh), Some(sh)) = (a.as_bigint(), b.as_string()) {
            let s = self.heap.materialize_string(sh).unwrap_or_default();
            return Ok(
                JsBigInt::parse_str(&s).is_some_and(|p| self.heap.get_bigint(bh) == Some(&p))
            );
        }
        if let (Some(sh), Some(bh)) = (a.as_string(), b.as_bigint()) {
            let s = self.heap.materialize_string(sh).unwrap_or_default();
            return Ok(
                JsBigInt::parse_str(&s).is_some_and(|p| self.heap.get_bigint(bh) == Some(&p))
            );
        }
        if let (Some(bh), true) = (a.as_bigint(), b.is_number()) {
            let n = self.to_number(b)?;
            return Ok(self.compare_bigint_f64(bh, n) == Some(std::cmp::Ordering::Equal));
        }
        if let (true, Some(bh)) = (a.is_number(), b.as_bigint()) {
            let n = self.to_number(a)?;
            return Ok(self.compare_bigint_f64(bh, n) == Some(std::cmp::Ordering::Equal));
        }
        Ok(false)
    }

    /// ECMA-262 §7.1.1 `ToPrimitive`.
    pub fn to_primitive(&mut self, val: JsValue, hint: PrimHint) -> Result<JsValue, JsException> {
        let Some(h) = val.as_object() else {
            return Ok(val);
        };
        let exotic = self.get_property_by_key(h, self.key_symbol_to_primitive)?;
        if !exotic.is_undefined() && !exotic.is_null() {
            let hint_str = match hint {
                PrimHint::Default => "default",
                PrimHint::Number => "number",
                PrimHint::String => "string",
            };
            let hs = JsValue::from_string(self.heap.alloc_string(hint_str));
            let res = self.call_value(exotic, val, &[hs])?;
            if res.is_object() {
                return Err(self.type_error("Cannot convert object to primitive value"));
            }
            return Ok(res);
        }
        let is_date = matches!(
            self.heap.get_object(h).map(|o| &o.kind),
            Some(ObjectKind::Date(_))
        );
        let order = if hint == PrimHint::String || (hint == PrimHint::Default && is_date) {
            [self.key_to_string, self.key_value_of]
        } else {
            [self.key_value_of, self.key_to_string]
        };
        for method_key in order {
            let m = self.get_property_by_key(h, method_key)?;
            if let Some(mh) = m.as_object() {
                if self.is_callable_handle(mh) {
                    let res = self.call_value(m, val, &[])?;
                    if !res.is_object() {
                        return Ok(res);
                    }
                }
            }
        }
        Err(self.type_error("Cannot convert object to primitive value"))
    }

    /// ECMA-262 §7.1.17 `ToString` (invokes `ToPrimitive(..., "string")` and throws on `Symbol`).
    pub fn to_js_string(&mut self, val: JsValue) -> Result<String, JsException> {
        match val.kind() {
            JsValueKind::Symbol(_) => {
                Err(self.type_error("Cannot convert a Symbol value to a string"))
            }
            JsValueKind::Object(_) => {
                let p = self.to_primitive(val, PrimHint::String)?;
                self.to_js_string(p)
            }
            _ => Ok(self.value_to_rust_string(val)),
        }
    }

    /// ECMA-262 §7.1.19 `ToPropertyKey` (preserves `Symbol` keys).
    pub fn to_property_key(&mut self, val: JsValue) -> Result<String, JsException> {
        let p = if val.is_object() {
            self.to_primitive(val, PrimHint::String)?
        } else {
            val
        };
        if p.is_symbol() {
            return Ok(self.value_to_rust_string(p));
        }
        self.to_js_string(p)
    }

    pub fn to_boolean(&self, val: JsValue) -> bool {
        match val.kind() {
            JsValueKind::Undefined | JsValueKind::Null | JsValueKind::Empty => false,
            JsValueKind::Bool(b) => b,
            JsValueKind::Int32(i) => i != 0,
            JsValueKind::Float(f) => f != 0.0 && !f.is_nan(),
            JsValueKind::String(h) => self.heap.get_string(h).is_some_and(|s| !s.is_empty()),
            JsValueKind::BigInt(h) => self.heap.get_bigint(h).is_some_and(|b| !b.is_zero()),
            JsValueKind::Object(_) | JsValueKind::Symbol(_) => true,
        }
    }

    pub fn to_number(&mut self, val: JsValue) -> Result<f64, JsException> {
        match val.kind() {
            JsValueKind::Int32(i) => Ok(f64::from(i)),
            JsValueKind::Float(f) => Ok(f),
            JsValueKind::Undefined | JsValueKind::Empty => Ok(f64::NAN),
            JsValueKind::Null => Ok(0.0),
            JsValueKind::Bool(b) => Ok(if b { 1.0 } else { 0.0 }),
            JsValueKind::String(h) => {
                let raw = self.heap.materialize_string(h).unwrap_or_default();
                let s = raw.trim();
                if s.is_empty() {
                    return Ok(0.0);
                }
                if s == "Infinity" || s == "+Infinity" {
                    return Ok(f64::INFINITY);
                }
                if s == "-Infinity" {
                    return Ok(f64::NEG_INFINITY);
                }
                if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                    return Ok(u128::from_str_radix(hex, 16).map_or(f64::NAN, |n| n as f64));
                }
                if let Some(bin) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
                    return Ok(u128::from_str_radix(bin, 2).map_or(f64::NAN, |n| n as f64));
                }
                if let Some(oct) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) {
                    return Ok(u128::from_str_radix(oct, 8).map_or(f64::NAN, |n| n as f64));
                }
                Ok(s.parse::<f64>().unwrap_or(f64::NAN))
            }
            JsValueKind::Symbol(_) | JsValueKind::BigInt(_) => {
                Err(self.type_error("Cannot convert Symbol or BigInt to number"))
            }
            JsValueKind::Object(_) => {
                let prim = self.to_primitive(val, PrimHint::Number)?;
                self.to_number(prim)
            }
        }
    }

    pub fn to_i32(&mut self, val: JsValue) -> Result<i32, JsException> {
        if let Some(i) = val.as_i32() {
            return Ok(i);
        }
        Ok(f64_to_i32(self.to_number(val)?))
    }

    pub fn to_u32(&mut self, val: JsValue) -> Result<u32, JsException> {
        Ok(self.to_i32(val)? as u32)
    }

    pub fn typeof_value(&self, val: JsValue) -> &'static str {
        match val.kind() {
            JsValueKind::Undefined | JsValueKind::Empty => "undefined",
            JsValueKind::Null => "object",
            JsValueKind::Bool(_) => "boolean",
            JsValueKind::Int32(_) | JsValueKind::Float(_) => "number",
            JsValueKind::BigInt(_) => "bigint",
            JsValueKind::String(_) => "string",
            JsValueKind::Symbol(_) => "symbol",
            JsValueKind::Object(h) => {
                if self.is_callable_handle(h) {
                    "function"
                } else {
                    "object"
                }
            }
        }
    }

    pub fn is_callable_handle(&self, h: HeapHandle) -> bool {
        match self.heap.get_object(h).map(|o| &o.kind) {
            Some(
                ObjectKind::Closure(_)
                | ObjectKind::NativeFunction(_)
                | ObjectKind::BoundFunction(_),
            ) => true,
            Some(ObjectKind::Proxy(p)) => self.is_callable_handle(p.target),
            _ => false,
        }
    }

    pub fn value_to_rust_string(&self, val: JsValue) -> String {
        let mut visited = Vec::new();
        self.value_to_rust_string_inner(val, &mut visited)
    }

    fn value_to_rust_string_inner(&self, val: JsValue, visited: &mut Vec<HeapHandle>) -> String {
        match val.kind() {
            JsValueKind::Undefined | JsValueKind::Empty => "undefined".into(),
            JsValueKind::Null => "null".into(),
            JsValueKind::Bool(true) => "true".into(),
            JsValueKind::Bool(false) => "false".into(),
            JsValueKind::Int32(i) => i.to_string(),
            JsValueKind::Float(f) => format_ecma_f64(f),
            JsValueKind::String(h) => self.heap.materialize_string(h).unwrap_or_default(),
            JsValueKind::BigInt(h) => self
                .heap
                .get_bigint(h)
                .map(|b| b.to_string())
                .unwrap_or_else(|| "0".into()),
            JsValueKind::Symbol(id) => format!("Symbol({})", id.0),
            JsValueKind::Object(h) => {
                if let Some(o) = self.heap.get_object(h) {
                    if matches!(o.kind, ObjectKind::Array) {
                        if visited.contains(&h) || visited.len() >= 64 {
                            return String::new();
                        }
                        visited.push(h);
                        let len = o.elements.len().min(65_536);
                        let parts: Vec<String> = (0..len)
                            .map(|i| match o.elements.get(i) {
                                Some(v) if !v.is_null() && !v.is_undefined() => {
                                    self.value_to_rust_string_inner(v, visited)
                                }
                                _ => String::new(),
                            })
                            .collect();
                        visited.pop();
                        return parts.join(",");
                    }
                    if let ObjectKind::TypedArray(ta) = o.kind {
                        let len = crate::builtins_atomics::ta_effective_len(self, h, ta)
                            .unwrap_or(0)
                            .min(65_536);
                        let parts: Vec<String> = (0..len)
                            .map(|i| {
                                crate::builtins_collections::typed_array_read_element_string(
                                    self, ta, i,
                                )
                            })
                            .collect();
                        return parts.join(",");
                    }
                }
                "[object Object]".into()
            }
        }
    }

    pub fn format_exception(&self, val: JsValue) -> String {
        self.format_exception_value(val)
    }

    fn format_exception_value(&self, val: JsValue) -> String {
        if let Some(h) = val.as_object() {
            let mut cur = Some(h);
            let mut name = None;
            let mut msg = None;
            let mut hops = 0u32;
            while let Some(obj_h) = cur {
                if hops > 64 {
                    break;
                }
                hops += 1;
                if name.is_none() {
                    name = self
                        .heap
                        .object_get_own(obj_h, self.key_name, &self.shapes)
                        .filter(|p| !p.value.is_undefined())
                        .map(|p| self.value_to_rust_string(p.value));
                }
                if msg.is_none() {
                    msg = self
                        .heap
                        .object_get_own(obj_h, self.key_message, &self.shapes)
                        .filter(|p| !p.value.is_undefined())
                        .map(|p| self.value_to_rust_string(p.value));
                }
                if name.is_some() && msg.is_some() {
                    break;
                }
                cur = self.heap.get_object(obj_h).and_then(|o| o.prototype);
            }
            if let Some(m) = msg {
                let n = name.unwrap_or_else(|| {
                    let mut c = Some(h);
                    for _ in 0..32 {
                        let Some(o) = c else { break };
                        if let Some(cp) =
                            self.heap
                                .object_get_own(o, self.key_constructor, &self.shapes)
                        {
                            if let Some(ch) = cp.value.as_object() {
                                if let Some(np) =
                                    self.heap.object_get_own(ch, self.key_name, &self.shapes)
                                {
                                    let cn = self.value_to_rust_string(np.value);
                                    if !cn.is_empty() && cn != "Object" {
                                        return cn;
                                    }
                                }
                            }
                        }
                        c = self.heap.get_object(o).and_then(|obj| obj.prototype);
                    }
                    "Error".to_string()
                });
                return if m.is_empty() { n } else { format!("{n}: {m}") };
            }
            if let Some(n) = name {
                return n;
            }
        }
        self.value_to_rust_string(val)
    }

    pub fn error_proto_for_name(&self, name: &str) -> HeapHandle {
        match name {
            "TypeError" => self.type_error_proto,
            "ReferenceError" => self.reference_error_proto,
            "RangeError" => self.range_error_proto,
            "SyntaxError" => self.syntax_error_proto,
            "URIError" => self.uri_error_proto,
            "EvalError" => self.eval_error_proto,
            "AggregateError" => self.aggregate_error_proto,
            _ => self.error_proto,
        }
    }

    pub fn make_error(&mut self, name: &str, message: impl Into<String>) -> JsException {
        let msg = message.into();
        let proto = self.error_proto_for_name(name);
        let err_obj = self
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        let is_known_builtin = matches!(
            name,
            "Error"
                | "TypeError"
                | "ReferenceError"
                | "RangeError"
                | "SyntaxError"
                | "URIError"
                | "EvalError"
                | "AggregateError"
        );
        if !is_known_builtin {
            let nv = JsValue::from_string(self.heap.alloc_string(name));
            self.heap.object_define_data(
                err_obj,
                self.key_name,
                nv,
                PropertyAttrs::METHOD,
                &mut self.shapes,
            );
        }
        if !msg.is_empty() {
            let mv = JsValue::from_string(self.heap.alloc_string(msg.as_str()));
            self.heap.object_define_data(
                err_obj,
                self.key_message,
                mv,
                PropertyAttrs::METHOD,
                &mut self.shapes,
            );
        }
        let formatted = if msg.is_empty() {
            name.to_owned()
        } else {
            format!("{name}: {msg}")
        };
        JsException {
            value: JsValue::from_object(err_obj),
            message: formatted,
        }
    }

    pub fn type_error(&mut self, msg: impl Into<String>) -> JsException {
        self.make_error("TypeError", msg)
    }

    pub fn reference_error(&mut self, msg: impl Into<String>) -> JsException {
        self.make_error("ReferenceError", msg)
    }

    pub fn range_error(&mut self, msg: impl Into<String>) -> JsException {
        self.make_error("RangeError", msg)
    }

    /// Exposes a compiled [`FunctionBytecode`] reference for Phase 3 JIT compilation.
    #[must_use]
    pub fn function_bytecode(&self, prog_id: u32, func_idx: u32) -> Option<&FunctionBytecode> {
        self.programs
            .get(prog_id as usize)?
            .functions
            .get(func_idx as usize)
    }

    /// Returns the base register offset of the active top call frame.
    #[must_use]
    pub fn jit_current_frame_base(&self) -> usize {
        self.frames.last().map_or(0, |f| f.base_reg)
    }

    /// Returns `this` of the active top call frame.
    #[must_use]
    pub fn jit_current_frame_this(&self) -> JsValue {
        self.frames
            .last()
            .map_or(JsValue::UNDEFINED, |f| f.this_val)
    }

    /// Updates `pc` on the active top call frame (used before throwing slow paths for exact unwinding).
    pub fn jit_set_current_frame_pc(&mut self, pc: usize) {
        if let Some(f) = self.frames.last_mut() {
            f.pc = pc;
        }
    }

    /// Mutable access to the flat register window slice starting at `base`.
    pub fn jit_registers_from_mut(&mut self, base: usize) -> &mut [JsValue] {
        &mut self.registers[base..]
    }

    /// Inspects an IC slot state for Tier-2/Tier-3 type & shape feedback.
    #[must_use]
    pub fn jit_ic_state(&self, prog_id: u32, func_idx: u32, ic_idx: usize) -> crate::ic::IcState {
        self.ic_tables
            .get(prog_id as usize)
            .and_then(|v| v.get(func_idx as usize))
            .map_or(crate::ic::IcState::Uninitialized, |t| t.state(ic_idx))
    }

    /// Returns the interned `PropertyKey` associated with an IC slot.
    #[must_use]
    pub fn jit_ic_key(&mut self, prog_id: u32, func_idx: u32, ic_idx: usize) -> PropertyKey {
        self.ic_key(prog_id as usize, func_idx as usize, ic_idx)
    }

    /// Materializes a constant pool entry into a `JsValue` (safe for JIT constant loading).
    pub fn jit_materialize_const(
        &mut self,
        prog_id: u32,
        func_idx: u32,
        idx: usize,
    ) -> Result<JsValue, JsException> {
        self.materialize_constant(prog_id as usize, func_idx as usize, idx)
    }

    /// Pure zero-allocation, zero-JS-reentry monomorphic own-slot `GetProp` fast path.
    /// Returns `Some(val)` on an own-data-property shape hit, or `None` if slow path is required.
    #[inline]
    pub fn jit_get_prop_ic_fast(
        &mut self,
        prog_id: u32,
        func_idx: u32,
        ic_idx: usize,
        obj_val: JsValue,
    ) -> Option<JsValue> {
        let obj = obj_val.as_object()?;
        let crate::ic::IcState::Monomorphic(entry) = self
            .ic_tables
            .get(prog_id as usize)?
            .get(func_idx as usize)?
            .state(ic_idx)
        else {
            return None;
        };
        if entry.holder.is_some() || entry.is_accessor {
            return None;
        }
        let cached_offset = entry.offset as usize;
        let o = self.heap.get_object(obj)?;
        if !matches!(o.kind, ObjectKind::Proxy(_))
            && ShapeId(o.shape_id) == entry.shape
            && cached_offset < o.slots.len()
        {
            self.ic_stats.monomorphic_hits += 1;
            return Some(o.slots[cached_offset]);
        }
        None
    }

    /// Pure zero-allocation, zero-JS-reentry monomorphic in-place `SetProp` fast path.
    /// Returns `true` on an own-data-property in-place write hit, or `false` if slow path is required.
    #[inline]
    pub fn jit_set_prop_ic_fast(
        &mut self,
        prog_id: u32,
        func_idx: u32,
        ic_idx: usize,
        obj_val: JsValue,
        val: JsValue,
    ) -> bool {
        let Some(obj) = obj_val.as_object() else {
            return false;
        };
        let Some(crate::ic::IcState::Monomorphic(entry)) = self
            .ic_tables
            .get(prog_id as usize)
            .and_then(|v| v.get(func_idx as usize))
            .map(|t| t.state(ic_idx))
        else {
            return false;
        };
        if entry.holder.is_some() || entry.is_accessor || entry.target_shape != entry.shape {
            return false;
        }
        let cached_offset = entry.offset as usize;
        let mut wrote = false;
        let mut is_proto = false;
        if let Some(o) = self.heap.get_object_mut(obj) {
            if !matches!(o.kind, ObjectKind::Proxy(_))
                && ShapeId(o.shape_id) == entry.shape
                && cached_offset < o.slots.len()
            {
                o.slots[cached_offset] = val;
                is_proto = o.is_prototype;
                wrote = true;
            }
        }
        if wrote {
            if is_proto {
                self.shapes.bump_prototype_epoch();
            }
            self.heap.write_barrier(obj, val);
            self.ic_stats.monomorphic_hits += 1;
        }
        wrote
    }

    /// Pure zero-JS-reentry dense array element read fast path.
    #[inline]
    pub fn jit_get_elem_fast(&self, obj_val: JsValue, key_val: JsValue) -> Option<JsValue> {
        let obj = obj_val.as_object()?;
        let idx = key_val.as_i32()?;
        if idx < 0 {
            return None;
        }
        let o = self.heap.get_object(obj)?;
        if matches!(o.kind, ObjectKind::Array) {
            return o.elements.get(idx as u32);
        }
        None
    }

    /// Pure zero-JS-reentry dense array in-bounds element write fast path.
    #[inline]
    pub fn jit_set_elem_fast(&mut self, obj_val: JsValue, key_val: JsValue, val: JsValue) -> bool {
        let Some(obj) = obj_val.as_object() else {
            return false;
        };
        let Some(idx) = key_val.as_i32() else {
            return false;
        };
        if idx < 0 {
            return false;
        }
        let mut wrote = false;
        if let Some(o) = self.heap.get_object_mut(obj) {
            if matches!(o.kind, ObjectKind::Array) && (idx as u32) < o.elements.len() {
                o.elements.set(idx as u32, val);
                wrote = true;
            }
        }
        if wrote {
            self.heap.write_barrier(obj, val);
        }
        wrote
    }

    /// Fast safe `GetProp` via the VM's Monomorphic/Polymorphic/Megamorphic IC table.
    #[inline]
    pub fn jit_get_prop_ic(
        &mut self,
        prog_id: u32,
        func_idx: u32,
        ic_idx: usize,
        obj_val: JsValue,
    ) -> Result<JsValue, JsException> {
        let p = prog_id as usize;
        let f = func_idx as usize;
        // Ultra-fast monomorphic direct own-slot hit before touching key lookup.
        if let Some(obj) = obj_val.as_object() {
            if let Some(crate::ic::IcState::Monomorphic(entry)) = self
                .ic_tables
                .get(p)
                .and_then(|v| v.get(f))
                .map(|t| t.state(ic_idx))
            {
                if entry.holder.is_none() && !entry.is_accessor {
                    let cached_offset = entry.offset as usize;
                    if let Some(o) = self.heap.get_object(obj) {
                        if !matches!(o.kind, ObjectKind::Proxy(_))
                            && ShapeId(o.shape_id) == entry.shape
                            && cached_offset < o.slots.len()
                        {
                            self.ic_stats.monomorphic_hits += 1;
                            return Ok(o.slots[cached_offset]);
                        }
                    }
                }
            }
        }
        let key = self.ic_key(p, f, ic_idx);
        self.get_prop_cached(obj_val, p, f, ic_idx, key)
    }

    /// Fast safe `SetProp` via the VM's Monomorphic/Polymorphic/Megamorphic IC table.
    #[inline]
    pub fn jit_set_prop_ic(
        &mut self,
        prog_id: u32,
        func_idx: u32,
        ic_idx: usize,
        obj_val: JsValue,
        val: JsValue,
    ) -> Result<(), JsException> {
        let p = prog_id as usize;
        let f = func_idx as usize;
        let Some(obj) = obj_val.as_object() else {
            return Err(self.type_error("Cannot set property on non-object"));
        };
        // Ultra-fast monomorphic in-place slot write when shape already matches!
        if let Some(crate::ic::IcState::Monomorphic(entry)) = self
            .ic_tables
            .get(p)
            .and_then(|v| v.get(f))
            .map(|t| t.state(ic_idx))
        {
            if entry.holder.is_none() && !entry.is_accessor && entry.target_shape == entry.shape {
                let cached_offset = entry.offset as usize;
                let mut wrote = false;
                let mut is_proto = false;
                if let Some(o) = self.heap.get_object_mut(obj) {
                    if !matches!(o.kind, ObjectKind::Proxy(_))
                        && ShapeId(o.shape_id) == entry.shape
                        && cached_offset < o.slots.len()
                    {
                        o.slots[cached_offset] = val;
                        is_proto = o.is_prototype;
                        wrote = true;
                    }
                }
                if wrote {
                    if is_proto {
                        self.shapes.bump_prototype_epoch();
                    }
                    self.heap.write_barrier(obj, val);
                    self.ic_stats.monomorphic_hits += 1;
                    return Ok(());
                }
            }
        }
        let key = self.ic_key(p, f, ic_idx);
        self.set_prop_cached(obj, p, f, ic_idx, key, val)
    }

    /// Fast safe dense array / object element load (`obj[key]`).
    #[inline]
    pub fn jit_get_elem(
        &mut self,
        obj_val: JsValue,
        key_val: JsValue,
    ) -> Result<JsValue, JsException> {
        if let (Some(obj), Some(idx)) = (obj_val.as_object(), key_val.as_i32()) {
            if idx >= 0 {
                if let Some(o) = self.heap.get_object(obj) {
                    if matches!(o.kind, ObjectKind::Array) {
                        if let Some(v) = o.elements.get(idx as u32) {
                            return Ok(v);
                        }
                    }
                }
            }
        }
        self.get_elem(obj_val, key_val)
    }

    /// Fast safe dense array / object element store (`obj[key] = val`).
    #[inline]
    pub fn jit_set_elem(
        &mut self,
        obj_val: JsValue,
        key_val: JsValue,
        val: JsValue,
    ) -> Result<(), JsException> {
        if let (Some(obj), Some(idx)) = (obj_val.as_object(), key_val.as_i32()) {
            if idx >= 0 {
                let mut wrote = false;
                if let Some(o) = self.heap.get_object_mut(obj) {
                    if matches!(o.kind, ObjectKind::Array) && (idx as u32) < o.elements.len() {
                        o.elements.set(idx as u32, val);
                        wrote = true;
                    }
                }
                if wrote {
                    self.heap.write_barrier(obj, val);
                    return Ok(());
                }
            }
        }
        self.set_elem(obj_val, key_val, val)
    }

    /// Arithmetic & comparison slow paths delegating to exact ES2024 VM semantics.
    pub fn jit_add(&mut self, a: JsValue, b: JsValue) -> Result<JsValue, JsException> {
        self.op_add(a, b)
    }
    pub fn jit_sub(&mut self, a: JsValue, b: JsValue) -> Result<JsValue, JsException> {
        self.op_sub(a, b)
    }
    pub fn jit_mul(&mut self, a: JsValue, b: JsValue) -> Result<JsValue, JsException> {
        self.op_mul(a, b)
    }
    pub fn jit_div(&mut self, a: JsValue, b: JsValue) -> Result<JsValue, JsException> {
        Ok(JsValue::from_number(
            self.to_number(a)? / self.to_number(b)?,
        ))
    }
    pub fn jit_rem(&mut self, a: JsValue, b: JsValue) -> Result<JsValue, JsException> {
        Ok(JsValue::from_number(
            self.to_number(a)? % self.to_number(b)?,
        ))
    }
    pub fn jit_lt(&mut self, a: JsValue, b: JsValue) -> Result<JsValue, JsException> {
        Ok(JsValue::from_bool(self.op_lt(a, b)?))
    }
    pub fn jit_le(&mut self, a: JsValue, b: JsValue) -> Result<JsValue, JsException> {
        Ok(JsValue::from_bool(self.op_lteq(a, b)?))
    }
    pub fn jit_gt(&mut self, a: JsValue, b: JsValue) -> Result<JsValue, JsException> {
        Ok(JsValue::from_bool(self.op_lt(b, a)?))
    }
    pub fn jit_ge(&mut self, a: JsValue, b: JsValue) -> Result<JsValue, JsException> {
        Ok(JsValue::from_bool(self.op_lteq(b, a)?))
    }
    pub fn jit_eq(&mut self, a: JsValue, b: JsValue) -> Result<JsValue, JsException> {
        Ok(JsValue::from_bool(self.abstract_eq(a, b)?))
    }
    pub fn jit_strict_eq(&self, a: JsValue, b: JsValue) -> JsValue {
        JsValue::from_bool(self.strict_eq(a, b))
    }

    /// Executes a single generic non-branch bytecode instruction at `pc` directly on the
    /// active top frame's live register window, returning the advanced `pc`.
    pub fn jit_step_generic(
        &mut self,
        prog_id: u32,
        func_idx: u32,
        pc: usize,
    ) -> Result<usize, JsException> {
        let frame_idx = self.frames.len() - 1;
        let base = self.frames[frame_idx].base_reg;
        let p = prog_id as usize;
        let f = func_idx as usize;
        let inst = self.programs[p].functions[f].code[pc];
        self.frames[frame_idx].pc = pc + 1;
        let Some(op) = inst.opcode() else {
            return Err(self.type_error("Invalid bytecode opcode"));
        };
        let stop_depth = self.frames.len();
        let _ = self.dispatch_op(op, inst, p, f, base, frame_idx, stop_depth)?;
        Ok(self.frames[frame_idx].pc)
    }

    /// Records monomorphic/polymorphic JS closure target feedback at `Opcode::Call`.
    #[inline]
    fn record_call_feedback(&mut self, prog_id: u32, func_idx: u32, pc: usize, callee: JsValue) {
        let key = (prog_id, func_idx, pc);
        let cur = self.call_feedback.get(&key).copied().unwrap_or_default();
        if matches!(cur, CallSiteFeedback::Polymorphic) {
            return;
        }
        let next = match callee.as_object().and_then(|h| {
            self.heap.get_object(h).and_then(|o| match &o.kind {
                ObjectKind::Closure(cl) if cl.upvalues.is_empty() => {
                    Some((h, cl.program_id, cl.func_idx))
                }
                _ => None,
            })
        }) {
            Some((h, cp, cf)) => match cur {
                CallSiteFeedback::Uninitialized => CallSiteFeedback::Monomorphic {
                    callee: h,
                    program_id: cp,
                    func_idx: cf,
                },
                CallSiteFeedback::Monomorphic {
                    callee: prev_h,
                    program_id: prev_p,
                    func_idx: prev_f,
                } if prev_h == h && prev_p == cp && prev_f == cf => cur,
                _ => CallSiteFeedback::Polymorphic,
            },
            None => CallSiteFeedback::Polymorphic,
        };
        self.call_feedback.insert(key, next);
    }

    /// Returns recorded Tier-3 call-site feedback for `(prog_id, func_idx, pc)`.
    #[must_use]
    pub fn jit_call_feedback(&self, prog_id: u32, func_idx: u32, pc: usize) -> CallSiteFeedback {
        self.call_feedback
            .get(&(prog_id, func_idx, pc))
            .copied()
            .unwrap_or_default()
    }

    /// Returns recorded Tier-3 array `ElementsKind` feedback for `(prog_id, func_idx, pc)`.
    #[must_use]
    pub fn jit_elem_feedback(
        &self,
        prog_id: u32,
        func_idx: u32,
        pc: usize,
    ) -> Option<crate::elements::ElementsKind> {
        self.elem_feedback.get(&(prog_id, func_idx, pc)).copied()
    }

    /// Snapshots all IC slot states for `(prog_id, func_idx)` for Tier-3 speculative compilation.
    #[must_use]
    pub fn jit_ic_snapshot(&self, prog_id: u32, func_idx: u32) -> Vec<crate::ic::IcState> {
        let Some(table) = self
            .ic_tables
            .get(prog_id as usize)
            .and_then(|v| v.get(func_idx as usize))
        else {
            return Vec::new();
        };
        (0..table.len()).map(|i| table.state(i)).collect()
    }

    /// Validates an object's `ShapeId` and returns a direct mutable slice over its dense own
    /// property slots for hoisted Tier-3 shape guards.
    ///
    /// Returns `None` (triggering Tier-3 deoptimization) if `obj_val` is not an ordinary object,
    /// is a `Proxy`, has a mismatched shape, has fewer than `min_slots`, or is a prototype object
    /// when `for_write` is true.
    #[inline]
    pub fn jit_guard_object_slots(
        &mut self,
        obj_val: JsValue,
        expected_shape: u32,
        min_slots: usize,
        for_write: bool,
    ) -> Option<&mut [JsValue]> {
        let obj = obj_val.as_object()?;
        let o = self.heap.get_object_mut(obj)?;
        if matches!(o.kind, ObjectKind::Proxy(_))
            || o.shape_id != expected_shape
            || o.slots.len() < min_slots
            || (for_write && o.is_prototype)
        {
            return None;
        }
        Some(o.slots.as_mut_slice())
    }

    /// Validates a prototype-chain load guard (`prototype_epoch` + receiver `ShapeId`) and reads
    /// the cached holder's slot directly. Returns `None` (deopt) if the prototype chain mutated
    /// or the receiver shape changed.
    #[inline]
    pub fn jit_guard_proto_prop(
        &self,
        obj_val: JsValue,
        expected_shape: u32,
        holder_val: JsValue,
        offset: usize,
        expected_epoch: u64,
    ) -> Option<JsValue> {
        if self.shapes.prototype_epoch() != expected_epoch {
            return None;
        }
        let obj = obj_val.as_object()?;
        let o = self.heap.get_object(obj)?;
        if matches!(o.kind, ObjectKind::Proxy(_)) || o.shape_id != expected_shape {
            return None;
        }
        let holder = holder_val.as_object()?;
        let ho = self.heap.get_object(holder)?;
        ho.slots.get(offset).copied()
    }

    /// Validates that `obj_val` is an `Array` backed by `ElementsStorage::PackedInt32` and returns
    /// direct slice access for Tier-3 unboxed array loops.
    #[inline]
    pub fn jit_guard_packed_i32(&mut self, obj_val: JsValue) -> Option<&mut [i32]> {
        let obj = obj_val.as_object()?;
        let o = self.heap.get_object_mut(obj)?;
        if !matches!(o.kind, ObjectKind::Array) {
            return None;
        }
        match &mut o.elements {
            crate::elements::ElementsStorage::PackedInt32(v) => Some(v.as_mut_slice()),
            _ => None,
        }
    }

    /// Validates that `obj_val` is an `Array` backed by `ElementsStorage::PackedFloat64`.
    #[inline]
    pub fn jit_guard_packed_f64(&mut self, obj_val: JsValue) -> Option<&mut [f64]> {
        let obj = obj_val.as_object()?;
        let o = self.heap.get_object_mut(obj)?;
        if !matches!(o.kind, ObjectKind::Array) {
            return None;
        }
        match &mut o.elements {
            crate::elements::ElementsStorage::PackedFloat64(v) => Some(v.as_mut_slice()),
            _ => None,
        }
    }

    /// Validates that `obj_val` is an `Array` backed by `ElementsStorage::PackedValues`.
    #[inline]
    pub fn jit_guard_packed_values(&mut self, obj_val: JsValue) -> Option<&mut [JsValue]> {
        let obj = obj_val.as_object()?;
        let o = self.heap.get_object_mut(obj)?;
        if !matches!(o.kind, ObjectKind::Array) {
            return None;
        }
        match &mut o.elements {
            crate::elements::ElementsStorage::PackedValues(v) => Some(v.as_mut_slice()),
            _ => None,
        }
    }

    /// Zero-allocation monomorphic own-slot `GetGlobal` fast path for JIT code.
    #[inline]
    pub fn jit_get_global_ic_fast(
        &mut self,
        prog_id: u32,
        func_idx: u32,
        ic_idx: usize,
    ) -> Option<JsValue> {
        let crate::ic::IcState::Monomorphic(entry) = self
            .ic_tables
            .get(prog_id as usize)?
            .get(func_idx as usize)?
            .state(ic_idx)
        else {
            return None;
        };
        if entry.holder.is_some() || entry.is_accessor {
            return None;
        }
        let cached_offset = entry.offset as usize;
        let g = self.heap.get_object(self.global)?;
        if ShapeId(g.shape_id) == entry.shape && cached_offset < g.slots.len() {
            self.ic_stats.monomorphic_hits += 1;
            return Some(g.slots[cached_offset]);
        }
        None
    }

    /// Zero-Vec-allocation fast call helper for JIT call sites.
    #[inline]
    pub fn jit_call_fast(
        &mut self,
        prog_id: u32,
        func_idx: u32,
        pc: usize,
        callee: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JsException> {
        self.record_call_feedback(prog_id, func_idx, pc, callee);
        self.call_value(callee, JsValue::UNDEFINED, args)
    }

    /// Generational write barrier invoked on cold heap-reference stores from Tier-3 JIT code.
    #[inline]
    pub fn jit_write_barrier(&mut self, owner_val: JsValue, val: JsValue) {
        if let Some(owner) = owner_val.as_object() {
            self.heap.write_barrier(owner, val);
        }
    }

    /// Cooperative interrupt + minor-GC safepoint executed at JIT loop back-edges.
    pub fn jit_loop_safepoint(&mut self, prog_id: u32, func_idx: u32) -> Result<(), JsException> {
        self.loop_iterations = self.loop_iterations.saturating_add(1);
        if let Some(edges) = self.back_edge_counts.get_mut(prog_id as usize) {
            if let Some(cnt) = edges.get_mut(func_idx as usize) {
                *cnt = cnt.saturating_add(1);
            }
        }
        if self.interrupt_flag.load(Ordering::Relaxed)
            || self
                .max_loop_iterations
                .is_some_and(|limit| self.loop_iterations > limit)
        {
            return Err(self.range_error("Execution interrupted: loop budget exceeded"));
        }
        if self.heap.should_collect_minor() {
            self.collect_minor_gc();
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimHint {
    Default,
    Number,
    String,
}

enum StepOutcome {
    Continue,
    ReturnTop(JsValue),
    SuspendAwait { dst: u8, awaited: JsValue },
    SuspendYield { dst: u8, yielded: JsValue },
    SuspendYieldStar { dst: u8, iterable: JsValue },
    SuspendInitial,
}

enum NativeInv {
    Static(NativeFnPtr),
    Closure(NativeClosure),
}

fn f64_to_u32(n: f64) -> u32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    n.trunc().rem_euclid(4_294_967_296.0) as u32
}

fn f64_to_i32(n: f64) -> i32 {
    f64_to_u32(n) as i32
}

pub(crate) fn is_symbol_key_str(s: &str) -> bool {
    s.strip_prefix("Symbol(")
        .and_then(|r| r.strip_suffix(')'))
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

pub(crate) fn parse_array_index_str(s: &str) -> Option<u32> {
    let idx = s.parse::<u32>().ok()?;
    if idx < u32::MAX && idx.to_string() == s {
        Some(idx)
    } else {
        None
    }
}

/// ECMA-262 §6.1.6.1.20 `Number::toString(x, 10)` decimal/scientific formatting.
fn format_ecma_f64(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f.is_sign_positive() {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    if f == 0.0 {
        return "0".into();
    }
    let abs = f.abs();
    if !(1e-6..1e21).contains(&abs) {
        let s = format!("{f:e}");
        if let Some((mant, exp)) = s.split_once('e') {
            return if exp.starts_with('-') {
                format!("{mant}e{exp}")
            } else {
                format!("{mant}e+{exp}")
            };
        }
        return s;
    }
    f.to_string()
}
