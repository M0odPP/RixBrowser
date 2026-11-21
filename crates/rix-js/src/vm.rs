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
use crate::heap::{ClosureObject, GcHeap, JsObject, ObjectKind, ProxyObject, RegExpObject};
use crate::ic::{IcEntry, IcStats, IcTable, MegamorphicCache};
use crate::shape::{PropertyAttrs, ShapeId, ShapeTable};
use crate::string::PropertyKey;
use crate::value::{HeapHandle, JsValue, JsValueKind, SymbolId};
use std::collections::HashMap;
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
}

#[derive(Debug, Clone)]
enum IteratorState {
    Array { arr: HeapHandle, index: u32 },
    String { chars: Vec<char>, index: usize },
    Keys { keys: Vec<String>, index: usize },
    Custom { iter_obj: HeapHandle },
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
    pub string_proto: HeapHandle,
    pub number_proto: HeapHandle,
    pub boolean_proto: HeapHandle,
    pub regexp_proto: HeapHandle,
    pub date_proto: HeapHandle,
    pub symbol_registry: HashMap<String, SymbolId>,
    pub symbol_descriptions: HashMap<SymbolId, String>,
    pub rng_state: u64,
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
    /// Shared atomic flag checked on every `Opcode::LoopBack`.
    pub interrupt_flag: Arc<AtomicBool>,
    /// Per-function invocation counters (used for Tier-2/3 JIT tier-up in Phase 3).
    pub call_counts: Vec<Vec<u32>>,
    /// Per-function loop back-edge counters (used for Tier-2 OSR in Phase 3).
    pub back_edge_counts: Vec<Vec<u32>>,
    /// Frequently interned well-known property keys.
    pub key_length: PropertyKey,
    pub key_prototype: PropertyKey,
    pub key_constructor: PropertyKey,
    pub key_name: PropertyKey,
    pub key_message: PropertyKey,
    pub key_to_string: PropertyKey,
    pub key_value_of: PropertyKey,
    pub key_next: PropertyKey,
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

        let function_proto = heap.alloc_empty_object();
        heap.object_set_prototype(function_proto, Some(object_proto), &mut shapes);
        heap.get_object_mut(function_proto).unwrap().is_prototype = true;

        let array_proto = heap.alloc_empty_object();
        heap.object_set_prototype(array_proto, Some(object_proto), &mut shapes);
        heap.get_object_mut(array_proto).unwrap().is_prototype = true;

        let error_proto = heap.alloc_empty_object();
        heap.object_set_prototype(error_proto, Some(object_proto), &mut shapes);
        heap.get_object_mut(error_proto).unwrap().is_prototype = true;

        let string_proto = heap.alloc_empty_object();
        heap.object_set_prototype(string_proto, Some(object_proto), &mut shapes);
        heap.get_object_mut(string_proto).unwrap().is_prototype = true;

        let number_proto = heap.alloc_empty_object();
        heap.object_set_prototype(number_proto, Some(object_proto), &mut shapes);
        heap.get_object_mut(number_proto).unwrap().is_prototype = true;

        let boolean_proto = heap.alloc_empty_object();
        heap.object_set_prototype(boolean_proto, Some(object_proto), &mut shapes);
        heap.get_object_mut(boolean_proto).unwrap().is_prototype = true;

        let regexp_proto = heap.alloc_empty_object();
        heap.object_set_prototype(regexp_proto, Some(object_proto), &mut shapes);
        heap.get_object_mut(regexp_proto).unwrap().is_prototype = true;

        let date_proto = heap.alloc_empty_object();
        heap.object_set_prototype(date_proto, Some(object_proto), &mut shapes);
        heap.get_object_mut(date_proto).unwrap().is_prototype = true;

        let global = heap.alloc_empty_object();
        heap.object_set_prototype(global, Some(object_proto), &mut shapes);

        let key_length = heap.intern_key("length");
        let key_prototype = heap.intern_key("prototype");
        let key_constructor = heap.intern_key("constructor");
        let key_name = heap.intern_key("name");
        let key_message = heap.intern_key("message");
        let key_to_string = heap.intern_key("toString");
        let key_value_of = heap.intern_key("valueOf");
        let key_next = heap.intern_key("next");
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
            string_proto,
            number_proto,
            boolean_proto,
            regexp_proto,
            date_proto,
            symbol_registry: HashMap::new(),
            symbol_descriptions: HashMap::new(),
            rng_state: 0x9E37_79B9_7F4A_7C15,
            programs: Vec::new(),
            ic_tables: Vec::new(),
            const_keys: Vec::new(),
            native_fns: Vec::new(),
            registers: Vec::with_capacity(1024),
            frames: Vec::with_capacity(64),
            open_upvalues: Vec::new(),
            with_stack: Vec::new(),
            iterators: HashMap::new(),
            max_loop_iterations: None,
            loop_iterations: 0,
            interrupt_flag: Arc::new(AtomicBool::new(false)),
            call_counts: Vec::new(),
            back_edge_counts: Vec::new(),
            key_length,
            key_prototype,
            key_constructor,
            key_name,
            key_message,
            key_to_string,
            key_value_of,
            key_next,
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
        vm
    }

    /// Allocates a `RegExp` object with `RegExp.prototype` and standard metadata properties.
    pub fn alloc_regexp(&mut self, pattern: String, flags: String) -> HeapHandle {
        let re = self.heap.alloc_object(JsObject::with_kind(
            ObjectKind::RegExp(Box::new(RegExpObject {
                pattern: pattern.clone(),
                flags: flags.clone(),
            })),
            Some(self.regexp_proto),
        ));
        let k_src = self.heap.intern_key("source");
        let k_flags = self.heap.intern_key("flags");
        let k_global = self.heap.intern_key("global");
        let k_icase = self.heap.intern_key("ignoreCase");
        let k_multi = self.heap.intern_key("multiline");
        let k_last = self.heap.intern_key("lastIndex");
        let src_v = JsValue::from_string(self.heap.alloc_string(pattern));
        let flg_v = JsValue::from_string(self.heap.alloc_string(flags.clone()));
        self.heap.object_define_data(
            re,
            k_src,
            src_v,
            PropertyAttrs::CONFIGURABLE,
            &mut self.shapes,
        );
        self.heap.object_define_data(
            re,
            k_flags,
            flg_v,
            PropertyAttrs::CONFIGURABLE,
            &mut self.shapes,
        );
        self.heap.object_define_data(
            re,
            k_global,
            JsValue::from_bool(flags.contains('g')),
            PropertyAttrs::CONFIGURABLE,
            &mut self.shapes,
        );
        self.heap.object_define_data(
            re,
            k_icase,
            JsValue::from_bool(flags.contains('i')),
            PropertyAttrs::CONFIGURABLE,
            &mut self.shapes,
        );
        self.heap.object_define_data(
            re,
            k_multi,
            JsValue::from_bool(flags.contains('m')),
            PropertyAttrs::CONFIGURABLE,
            &mut self.shapes,
        );
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

    /// Registers a static native function and returns its callable [`HeapHandle`].
    pub fn alloc_native_fn(&mut self, name: &str, f: NativeFnPtr) -> HeapHandle {
        let id = self.native_fns.len() as u32;
        self.native_fns.push(NativeCallable::Static(f));
        let obj = self.heap.alloc_object(JsObject::with_kind(
            ObjectKind::NativeFunction(id),
            Some(self.function_proto),
        ));
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
        let global_val = JsValue::from_object(self.global);
        self.call_value(JsValue::from_object(entry_closure), global_val, &[])
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

        // Determine instance `[[Prototype]]` from `callee.prototype`.
        let proto_val = self.get_property_by_key(callee, self.key_prototype)?;
        let proto_handle = proto_val.as_object().unwrap_or(self.object_proto);
        let instance = self.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(proto_handle),
        ));
        if let Some(p_obj) = self.heap.get_object_mut(proto_handle) {
            p_obj.is_prototype = true;
        }

        // Run instance class field initializer if attached to constructor.
        if let Some(ObjectKind::Closure(c)) = self.heap.get_object(callee).map(|o| o.kind.clone()) {
            if let Some((prog_id, field_fn)) = c.field_init {
                let init_fn = self.heap.alloc_object(JsObject::with_kind(
                    ObjectKind::Closure(Box::new(ClosureObject {
                        program_id: prog_id,
                        func_idx: field_fn,
                        upvalues: Vec::new(),
                        bound_this: None,
                        super_ctor: None,
                        field_init: None,
                    })),
                    Some(self.function_proto),
                ));
                self.call_handle(
                    init_fn,
                    JsValue::from_object(instance),
                    &[],
                    None,
                    JsValue::UNDEFINED,
                )?;
            }
        }

        let res = self.call_handle(
            callee,
            JsValue::from_object(instance),
            args,
            Some(instance),
            new_target,
        )?;
        if res.is_object() {
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
        let kind = self
            .heap
            .get_object(callee)
            .map(|o| o.kind.clone())
            .ok_or_else(|| self.type_error("Attempted to call dead object handle"))?;

        match kind {
            ObjectKind::NativeFunction(id) => {
                // Execute native Rust callback cleanly via temporary borrow split.
                let callable = match self.native_fns.get(id as usize) {
                    Some(NativeCallable::Static(fp)) => NativeInv::Static(*fp),
                    Some(NativeCallable::Closure(rc)) => NativeInv::Closure(Rc::clone(rc)),
                    None => return Err(self.type_error("Invalid native function ID")),
                };
                match callable {
                    NativeInv::Static(fp) => fp(self, this_val, args),
                    NativeInv::Closure(rc) => rc(self, this_val, args),
                }
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
                let entry_depth = self.frames.len();
                let effective_this = closure.bound_this.unwrap_or_else(|| {
                    let is_strict = self.programs[closure.program_id as usize].functions
                        [closure.func_idx as usize]
                        .is_strict;
                    if !is_strict && (this_val.is_undefined() || this_val.is_null()) {
                        JsValue::from_object(self.global)
                    } else {
                        this_val
                    }
                });
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
    fn push_closure_frame(
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
        });
        Ok(())
    }

    /// Core register-window bytecode interpreter dispatch loop.
    fn run_loop(&mut self, stop_depth: usize) -> Result<JsValue, JsException> {
        loop {
            if self.frames.len() <= stop_depth {
                return Ok(JsValue::UNDEFINED);
            }
            let frame_idx = self.frames.len() - 1;
            let (prog_id, func_idx, base, pc) = {
                let f = &self.frames[frame_idx];
                (f.program_id as usize, f.func_idx as usize, f.base_reg, f.pc)
            };
            let inst = match self.programs[prog_id].functions[func_idx].code.get(pc) {
                Some(&i) => i,
                None => {
                    // Implicit return undefined.
                    if let Some(ret) = self.pop_frame_with_value(JsValue::UNDEFINED, stop_depth) {
                        return Ok(ret);
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
                Ok(StepOutcome::ReturnTop(val)) => return Ok(val),
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
                self.heap.object_define_data(
                    self.global,
                    key,
                    val,
                    PropertyAttrs::DEFAULT,
                    &mut self.shapes,
                );
            }
            Opcode::GetUpvalue => {
                let uv_idx = usize::from(inst.bx());
                let cell = self.frame_upvalue(frame_idx, uv_idx)?;
                let val = self.read_upvalue_cell(cell);
                self.registers[a] = val;
            }
            Opcode::SetUpvalue => {
                let uv_idx = usize::from(inst.bx());
                let cell = self.frame_upvalue(frame_idx, uv_idx)?;
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
                let uv_descs = self.programs[prog_id].functions[child_fn_idx as usize]
                    .upvalues
                    .clone();
                let is_arrow = self.programs[prog_id].functions[child_fn_idx as usize].is_arrow;
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
                let bound_this = is_arrow.then_some(self.frames[frame_idx].this_val);
                let closure_obj = self.heap.alloc_object(JsObject::with_kind(
                    ObjectKind::Closure(Box::new(ClosureObject {
                        program_id: prog_id as u32,
                        func_idx: child_fn_idx,
                        upvalues: captured,
                        bound_this,
                        super_ctor: None,
                        field_init: None,
                    })),
                    Some(self.function_proto),
                ));
                if !is_arrow {
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
                let cls_obj = self.instantiate_class(prog_id, func_idx, const_idx, super_val)?;
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
                let Some(obj) = obj_val.as_object() else {
                    return Err(self.type_error("Cannot set property on non-object"));
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
            Opcode::GetElem => {
                let obj_val = self.registers[base + usize::from(inst.b())];
                let key_val = self.registers[base + usize::from(inst.c())];
                let val = self.get_elem(obj_val, key_val)?;
                self.registers[a] = val;
            }
            Opcode::SetElem => {
                let obj_val = self.registers[a];
                let key_val = self.registers[base + usize::from(inst.b())];
                let val = self.registers[base + usize::from(inst.c())];
                self.set_elem(obj_val, key_val, val)?;
            }
            Opcode::DeleteProp | Opcode::DeleteElem => {
                let obj_val = self.registers[base + usize::from(inst.b())];
                let key_val = self.registers[base + usize::from(inst.c())];
                let deleted = self.delete_elem(obj_val, key_val)?;
                self.registers[a] = JsValue::from_bool(deleted);
            }
            Opcode::DefineGetter | Opcode::DefineSetter => {
                let key_val = self.registers[base + usize::from(inst.b())];
                let fn_val = self.registers[base + usize::from(inst.c())];
                if let (Some(obj), Some(fn_h)) = (self.registers[a].as_object(), fn_val.as_object())
                {
                    let s = self.value_to_rust_string(key_val);
                    let key = self.heap.intern_key(&s);
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
            Opcode::GetPrivate => {
                let obj_val = self.registers[base + usize::from(inst.b())];
                let ic_idx = self.resolve_ic_idx(inst, prog_id, func_idx, frame_idx);
                let key = self.ic_key(prog_id, func_idx, ic_idx);
                let Some(obj) = obj_val.as_object() else {
                    return Err(self.type_error("Cannot read private member from non-object"));
                };
                let val = self
                    .heap
                    .get_object(obj)
                    .and_then(|o| o.rare.as_ref())
                    .and_then(|r| r.private_fields.get(&key).copied())
                    .ok_or_else(|| {
                        self.type_error("Cannot read private member from object without it")
                    })?;
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
                if let Some(o) = self.heap.get_object_mut(obj) {
                    o.rare_mut().private_fields.insert(key, val);
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
                let args: Vec<JsValue> = (0..argc)
                    .map(|i| self.registers[call_base + 1 + i])
                    .collect();
                let res = self.call_value(callee, JsValue::UNDEFINED, &args)?;
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
                let super_ctor = match self.heap.get_object(callee_h).map(|o| &o.kind) {
                    Some(ObjectKind::Closure(c)) => c.super_ctor,
                    _ => None,
                };
                if let Some(sup) = super_ctor {
                    self.call_handle(sup, this_val, &args, this_val.as_object(), this_val)?;
                }
                self.registers[a] = this_val;
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
                let l = self.to_number(lhs)?;
                let r = self.to_number(rhs)?;
                self.registers[a] = JsValue::from_f64(l / r);
            }
            Opcode::Rem => {
                let lhs = self.registers[base + usize::from(inst.b())];
                let rhs = self.registers[base + usize::from(inst.c())];
                let l = self.to_number(lhs)?;
                let r = self.to_number(rhs)?;
                self.registers[a] = JsValue::from_f64(l % r);
            }
            Opcode::Exp => {
                let lhs = self.registers[base + usize::from(inst.b())];
                let rhs = self.registers[base + usize::from(inst.c())];
                let l = self.to_number(lhs)?;
                let r = self.to_number(rhs)?;
                self.registers[a] = JsValue::from_f64(l.powf(r));
            }
            Opcode::BitAnd => {
                let l = self.to_i32(self.registers[base + usize::from(inst.b())])?;
                let r = self.to_i32(self.registers[base + usize::from(inst.c())])?;
                self.registers[a] = JsValue::from_i32(l & r);
            }
            Opcode::BitOr => {
                let l = self.to_i32(self.registers[base + usize::from(inst.b())])?;
                let r = self.to_i32(self.registers[base + usize::from(inst.c())])?;
                self.registers[a] = JsValue::from_i32(l | r);
            }
            Opcode::BitXor => {
                let l = self.to_i32(self.registers[base + usize::from(inst.b())])?;
                let r = self.to_i32(self.registers[base + usize::from(inst.c())])?;
                self.registers[a] = JsValue::from_i32(l ^ r);
            }
            Opcode::Shl => {
                let l = self.to_i32(self.registers[base + usize::from(inst.b())])?;
                let r = self.to_u32(self.registers[base + usize::from(inst.c())])? & 0x1F;
                self.registers[a] = JsValue::from_i32(l.wrapping_shl(r));
            }
            Opcode::Shr => {
                let l = self.to_i32(self.registers[base + usize::from(inst.b())])?;
                let r = self.to_u32(self.registers[base + usize::from(inst.c())])? & 0x1F;
                self.registers[a] = JsValue::from_i32(l.wrapping_shr(r));
            }
            Opcode::UShr => {
                let l = self.to_u32(self.registers[base + usize::from(inst.b())])?;
                let r = self.to_u32(self.registers[base + usize::from(inst.c())])? & 0x1F;
                let res = l.wrapping_shr(r);
                self.registers[a] = JsValue::from_f64(f64::from(res));
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
                let s = self.value_to_rust_string(key_val);
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
                self.registers[a] = match v.kind() {
                    JsValueKind::Int32(i) if i != 0 && i != i32::MIN => JsValue::from_i32(-i),
                    _ => JsValue::from_f64(-self.to_number(v)?),
                };
            }
            Opcode::Pos | Opcode::ToNumber => {
                let v = self.registers[base + usize::from(inst.b())];
                let n = self.to_number(v)?;
                self.registers[a] = JsValue::from_f64(n);
            }
            Opcode::ToString => {
                let v = self.registers[base + usize::from(inst.b())];
                let s = self.value_to_rust_string(v);
                self.registers[a] = JsValue::from_string(self.heap.alloc_string(s));
            }
            Opcode::Not => {
                let v = self.registers[base + usize::from(inst.b())];
                self.registers[a] = JsValue::from_bool(!self.to_boolean(v));
            }
            Opcode::BitNot => {
                let v = self.to_i32(self.registers[base + usize::from(inst.b())])?;
                self.registers[a] = JsValue::from_i32(!v);
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
                    _ => JsValue::from_f64(self.to_number(v)? + 1.0),
                };
            }
            Opcode::Dec => {
                let v = self.registers[base + usize::from(inst.b())];
                self.registers[a] = match v.kind() {
                    JsValueKind::Int32(i) => JsValue::sub_i32_overflow(i, 1),
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
                self.frames[frame_idx].pc = (cur_pc + isize::from(inst.sbx())) as usize;
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
            Opcode::GetIterator | Opcode::GetAsyncIterator => {
                let iterable = self.registers[base + usize::from(inst.b())];
                let iter_h = self.create_iterator(iterable)?;
                self.registers[a] = JsValue::from_object(iter_h);
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
                if let Some(h) = self.registers[a].as_object() {
                    self.iterators.remove(&h);
                }
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
            // Async / Generators / Module runtime hooks (expanded in Issue #15)
            Opcode::Await | Opcode::Yield | Opcode::YieldStar | Opcode::DynamicImport => {
                let src = self.registers[base + usize::from(inst.b())];
                self.registers[a] = src;
            }
            Opcode::GetModuleImport => {
                self.registers[a] = JsValue::UNDEFINED;
            }
            Opcode::SetModuleExport => {}
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
        let mut roots = Vec::with_capacity(self.registers.len() + self.frames.len() * 3 + 16);
        roots.push(JsValue::from_object(self.global));
        roots.push(JsValue::from_object(self.object_proto));
        roots.push(JsValue::from_object(self.function_proto));
        roots.push(JsValue::from_object(self.array_proto));
        roots.push(JsValue::from_object(self.error_proto));
        roots.push(JsValue::from_object(self.string_proto));
        roots.push(JsValue::from_object(self.number_proto));
        roots.push(JsValue::from_object(self.boolean_proto));
        roots.push(JsValue::from_object(self.regexp_proto));
        roots.push(JsValue::from_object(self.date_proto));
        roots.extend_from_slice(&self.registers);
        for f in &self.frames {
            roots.push(f.this_val);
            roots.push(JsValue::from_object(f.callee));
            roots.push(f.new_target);
            if let Some(c) = f.construct_obj {
                roots.push(JsValue::from_object(c));
            }
        }
        for &(_, cell) in &self.open_upvalues {
            roots.push(JsValue::from_object(cell));
        }
        for &w in &self.with_stack {
            roots.push(JsValue::from_object(w));
        }
        for (&h, state) in &self.iterators {
            roots.push(JsValue::from_object(h));
            match state {
                IteratorState::Array { arr, .. } => roots.push(JsValue::from_object(*arr)),
                IteratorState::Custom { iter_obj } => roots.push(JsValue::from_object(*iter_obj)),
                IteratorState::String { .. } | IteratorState::Keys { .. } => {}
            }
        }
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
            ConstantEntry::Function(_) | ConstantEntry::Class(_) => JsValue::UNDEFINED,
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
        let shape = self.heap.get_object(self.global).unwrap().shape();
        let epoch = self.shapes.prototype_epoch();
        if let Some(entry) = self.ic_tables[prog_id][func_idx].probe_load(
            ic_idx,
            shape,
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
            } else {
                self.number_proto
            };
            return self.get_property_with_receiver(proto, key, obj_val);
        };

        let Some((is_array, is_proxy, arr_len, shape)) = self.heap.get_object(obj).map(|o| {
            (
                matches!(o.kind, ObjectKind::Array),
                matches!(o.kind, ObjectKind::Proxy(_)),
                o.elements.len(),
                o.shape(),
            )
        }) else {
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
        let receiver_shape = self
            .heap
            .get_object(obj)
            .map_or(ShapeId::DICTIONARY, |o| o.shape());
        let epoch = self.shapes.prototype_epoch();

        let mut cur = Some(obj);
        while let Some(holder) = cur {
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
                                IcEntry::proto(receiver_shape, holder, slot.offset, epoch)
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
        let Some((is_array, is_proxy, shape, is_proto)) = self.heap.get_object(obj).map(|o| {
            (
                matches!(o.kind, ObjectKind::Array),
                matches!(o.kind, ObjectKind::Proxy(_)),
                o.shape(),
                o.is_prototype,
            )
        }) else {
            return Err(self.type_error("Dead object handle"));
        };

        if is_array && key == self.key_length {
            let new_len = self.to_u32(val)?;
            if let Some(o) = self.heap.get_object_mut(obj) {
                o.elements.set_len(new_len);
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
        if let Some(ObjectKind::Proxy(p)) = self.heap.get_object(obj).map(|o| o.kind.clone()) {
            self.proxy_set(p, key, val, JsValue::from_object(obj))?;
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
                    }
                    return Ok(());
                }
                if !prop.attrs.contains(PropertyAttrs::WRITABLE) {
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
        while let Some(holder) = cur {
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
            let new_len = self.to_u32(val)?;
            if let Some(o) = self.heap.get_object_mut(obj) {
                o.elements.set_len(new_len);
            }
            return Ok(true);
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
        // Fast string indexing `"hello"[1] === "e"`.
        if let Some(str_h) = obj_val.as_string() {
            if let Some(idx) = key_val.as_i32().and_then(|i| u32::try_from(i).ok()) {
                let s = self.heap.materialize_string(str_h).unwrap_or_default();
                if let Some(ch) = s.chars().nth(idx as usize) {
                    return Ok(JsValue::from_string(self.heap.alloc_string(ch.to_string())));
                }
                return Ok(JsValue::UNDEFINED);
            }
            let ks = self.value_to_rust_string(key_val);
            let k = self.heap.intern_key(&ks);
            return self.get_property_with_receiver(self.string_proto, k, obj_val);
        }
        let Some(obj) = obj_val.as_object() else {
            return Ok(JsValue::UNDEFINED);
        };
        if let Some(ObjectKind::Proxy(p)) = self.heap.get_object(obj).map(|o| o.kind.clone()) {
            let ks = self.value_to_rust_string(key_val);
            let k = self.heap.intern_key(&ks);
            return self.proxy_get(p, k, obj_val);
        }
        if let Some(idx) = self.as_array_index(key_val) {
            if let Some(val) = self.heap.get_object(obj).and_then(|o| o.elements.get(idx)) {
                return Ok(val);
            }
        }
        let ks = self.value_to_rust_string(key_val);
        let key = self.heap.intern_key(&ks);
        self.get_property_by_key(obj, key)
    }

    pub fn set_elem(
        &mut self,
        obj_val: JsValue,
        key_val: JsValue,
        val: JsValue,
    ) -> Result<(), JsException> {
        let Some(obj) = obj_val.as_object() else {
            return Err(self.type_error("Cannot set element on non-object"));
        };
        if let Some(ObjectKind::Proxy(p)) = self.heap.get_object(obj).map(|o| o.kind.clone()) {
            let ks = self.value_to_rust_string(key_val);
            let k = self.heap.intern_key(&ks);
            self.proxy_set(p, k, val, obj_val)?;
            return Ok(());
        }
        if let Some(idx) = self.as_array_index(key_val) {
            if let Some(o) = self.heap.get_object_mut(obj) {
                o.elements.set(idx, val);
            }
            self.heap.write_barrier(obj, val);
            return Ok(());
        }
        let ks = self.value_to_rust_string(key_val);
        let key = self.heap.intern_key(&ks);
        self.set_property_by_key(obj, key, val)?;
        Ok(())
    }

    fn delete_elem(&mut self, obj_val: JsValue, key_val: JsValue) -> Result<bool, JsException> {
        let Some(obj) = obj_val.as_object() else {
            return Ok(true);
        };
        if let Some(idx) = self.as_array_index(key_val) {
            if let Some(o) = self.heap.get_object_mut(obj) {
                return Ok(o.elements.delete(idx));
            }
        }
        let ks = self.value_to_rust_string(key_val);
        let key = self.heap.intern_key(&ks);
        self.delete_property_by_key(obj, key)
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

    fn instantiate_class(
        &mut self,
        prog_id: usize,
        func_idx: usize,
        const_idx: usize,
        super_val: JsValue,
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

        let ctor_handle = if let Some(ctor_idx) = bp.constructor_idx {
            self.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Closure(Box::new(ClosureObject {
                    program_id: prog_id as u32,
                    func_idx: ctor_idx,
                    upvalues: Vec::new(),
                    bound_this: None,
                    super_ctor,
                    field_init: bp.instance_field_init_idx.map(|f| (prog_id as u32, f)),
                })),
                Some(super_ctor.unwrap_or(self.function_proto)),
            ))
        } else {
            // Synthesize default constructor invoking `super` if derived.
            let sup_copy = super_ctor;
            let field_init = bp.instance_field_init_idx.map(|f| (prog_id as u32, f));
            let h = self.alloc_host_fn(
                bp.name.as_deref().unwrap_or(""),
                move |vm, this_val, args| {
                    if let Some(sup) = sup_copy {
                        vm.call_handle(sup, this_val, args, this_val.as_object(), this_val)?;
                    }
                    let _ = field_init;
                    Ok(this_val)
                },
            );
            if let Some(sup) = super_ctor {
                self.heap
                    .object_set_prototype(h, Some(sup), &mut self.shapes);
            }
            h
        };

        if let Some(ref cls_name) = bp.name {
            let nv = JsValue::from_string(self.heap.alloc_string(cls_name.as_str()));
            self.heap.object_define_data(
                ctor_handle,
                self.key_name,
                nv,
                PropertyAttrs::CONFIGURABLE,
                &mut self.shapes,
            );
        }
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
            let method_fn = self.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Closure(Box::new(ClosureObject {
                    program_id: prog_id as u32,
                    func_idx: m.func_idx,
                    upvalues: Vec::new(),
                    bound_this: None,
                    super_ctor,
                    field_init: None,
                })),
                Some(self.function_proto),
            ));
            let target = if m.is_static {
                ctor_handle
            } else {
                class_proto
            };
            let key = self.heap.intern_key(&m.name);
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
            let init_fn = self.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Closure(Box::new(ClosureObject {
                    program_id: prog_id as u32,
                    func_idx: static_fn_idx,
                    upvalues: Vec::new(),
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
        if matches!(
            self.heap.get_object(obj).map(|o| &o.kind),
            Some(ObjectKind::Array)
        ) {
            let h = self.alloc_ordinary_object();
            self.iterators
                .insert(h, IteratorState::Array { arr: obj, index: 0 });
            return Ok(h);
        }
        if self.has_property_by_key(obj, self.key_next)? {
            let h = self.alloc_ordinary_object();
            self.iterators
                .insert(h, IteratorState::Custom { iter_obj: obj });
            return Ok(h);
        }
        Err(self.type_error("Object is not iterable"))
    }

    fn step_iterator(&mut self, iter_val: JsValue) -> Result<(JsValue, bool), JsException> {
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
            Some(IteratorState::Custom { iter_obj }) => {
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

    fn collect_iterable(&mut self, iterable: JsValue) -> Result<Vec<JsValue>, JsException> {
        let iter = self.create_iterator(iterable)?;
        let iter_val = JsValue::from_object(iter);
        let mut out = Vec::new();
        loop {
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
        let len = obj.elements.len();
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
        let mut out = Vec::new();
        if let Some(o) = self.heap.get_object(obj) {
            for idx in o.elements.present_indices() {
                out.push(idx.to_string());
            }
        }
        for k in self.own_enumerable_keys(obj) {
            if let Some(s) = self.heap.interner().get(k) {
                out.push(s.to_string());
            }
        }
        out
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
            (JsValueKind::BigInt(a), JsValueKind::BigInt(b)) => {
                let sum = self
                    .heap
                    .get_bigint(a)
                    .unwrap()
                    .add(self.heap.get_bigint(b).unwrap());
                return Ok(JsValue::from_bigint(self.heap.alloc_bigint(sum)));
            }
            (JsValueKind::String(a), JsValueKind::String(b)) => {
                let rope = self.heap.alloc_rope(a, b).unwrap_or(a);
                return Ok(JsValue::from_string(rope));
            }
            _ => {}
        }
        if lhs.is_string() || rhs.is_string() {
            let ls = self.value_to_rust_string(lhs);
            let rs = self.value_to_rust_string(rhs);
            return Ok(JsValue::from_string(
                self.heap.alloc_string(format!("{ls}{rs}")),
            ));
        }
        let l = self.to_number(lhs)?;
        let r = self.to_number(rhs)?;
        Ok(JsValue::from_f64(l + r))
    }

    fn op_sub(&mut self, lhs: JsValue, rhs: JsValue) -> Result<JsValue, JsException> {
        match (lhs.kind(), rhs.kind()) {
            (JsValueKind::Int32(a), JsValueKind::Int32(b)) => Ok(JsValue::sub_i32_overflow(a, b)),
            (JsValueKind::BigInt(a), JsValueKind::BigInt(b)) => {
                let diff = self
                    .heap
                    .get_bigint(a)
                    .unwrap()
                    .sub(self.heap.get_bigint(b).unwrap());
                Ok(JsValue::from_bigint(self.heap.alloc_bigint(diff)))
            }
            _ => Ok(JsValue::from_f64(
                self.to_number(lhs)? - self.to_number(rhs)?,
            )),
        }
    }

    fn op_mul(&mut self, lhs: JsValue, rhs: JsValue) -> Result<JsValue, JsException> {
        match (lhs.kind(), rhs.kind()) {
            (JsValueKind::Int32(a), JsValueKind::Int32(b)) => Ok(JsValue::mul_i32_overflow(a, b)),
            (JsValueKind::BigInt(a), JsValueKind::BigInt(b)) => {
                let prod = self
                    .heap
                    .get_bigint(a)
                    .unwrap()
                    .mul(self.heap.get_bigint(b).unwrap());
                Ok(JsValue::from_bigint(self.heap.alloc_bigint(prod)))
            }
            _ => Ok(JsValue::from_f64(
                self.to_number(lhs)? * self.to_number(rhs)?,
            )),
        }
    }

    fn op_lt(&mut self, lhs: JsValue, rhs: JsValue) -> Result<bool, JsException> {
        match (lhs.kind(), rhs.kind()) {
            (JsValueKind::Int32(a), JsValueKind::Int32(b)) => return Ok(a < b),
            (JsValueKind::String(a), JsValueKind::String(b)) => {
                let sa = self.heap.materialize_string(a).unwrap_or_default();
                let sb = self.heap.materialize_string(b).unwrap_or_default();
                return Ok(sa < sb);
            }
            _ => {}
        }
        let l = self.to_number(lhs)?;
        let r = self.to_number(rhs)?;
        Ok(l < r)
    }

    fn op_lteq(&mut self, lhs: JsValue, rhs: JsValue) -> Result<bool, JsException> {
        match (lhs.kind(), rhs.kind()) {
            (JsValueKind::Int32(a), JsValueKind::Int32(b)) => return Ok(a <= b),
            (JsValueKind::String(a), JsValueKind::String(b)) => {
                let sa = self.heap.materialize_string(a).unwrap_or_default();
                let sb = self.heap.materialize_string(b).unwrap_or_default();
                return Ok(sa <= sb);
            }
            _ => {}
        }
        let l = self.to_number(lhs)?;
        let r = self.to_number(rhs)?;
        Ok(l <= r)
    }

    fn op_instanceof(&mut self, lhs: JsValue, rhs: JsValue) -> Result<bool, JsException> {
        let Some(rhs_h) = rhs.as_object() else {
            return Err(self.type_error("Right-hand side of 'instanceof' is not an object"));
        };
        let Some(lhs_h) = lhs.as_object() else {
            return Ok(false);
        };
        let target_proto = self.get_property_by_key(rhs_h, self.key_prototype)?;
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
        if a.is_number() && b.is_string() {
            let bn = self.to_number(b)?;
            return Ok(self.to_number(a)? == bn);
        }
        if a.is_string() && b.is_number() {
            let an = self.to_number(a)?;
            return Ok(an == self.to_number(b)?);
        }
        Ok(false)
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
                    return Ok(u64::from_str_radix(hex, 16).map_or(f64::NAN, |n| n as f64));
                }
                Ok(s.parse::<f64>().unwrap_or(f64::NAN))
            }
            JsValueKind::Symbol(_) | JsValueKind::BigInt(_) => {
                Err(self.type_error("Cannot convert Symbol or BigInt to number"))
            }
            JsValueKind::Object(h) => {
                let vo = self.get_property_by_key(h, self.key_value_of)?;
                if vo.is_object() {
                    let prim = self.call_value(vo, val, &[])?;
                    if !prim.is_object() {
                        return self.to_number(prim);
                    }
                }
                let s = self.value_to_rust_string(val);
                let trimmed = s.trim();
                Ok(if trimmed.is_empty() {
                    0.0
                } else {
                    trimmed.parse::<f64>().unwrap_or(f64::NAN)
                })
            }
        }
    }

    pub fn to_i32(&mut self, val: JsValue) -> Result<i32, JsException> {
        if let Some(i) = val.as_i32() {
            return Ok(i);
        }
        let n = self.to_number(val)?;
        if !n.is_finite() || n == 0.0 {
            return Ok(0);
        }
        let int = n.trunc().rem_euclid(4_294_967_296.0) as u32;
        Ok(int as i32)
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
        match val.kind() {
            JsValueKind::Undefined | JsValueKind::Empty => "undefined".into(),
            JsValueKind::Null => "null".into(),
            JsValueKind::Bool(true) => "true".into(),
            JsValueKind::Bool(false) => "false".into(),
            JsValueKind::Int32(i) => i.to_string(),
            JsValueKind::Float(f) => {
                if f.is_nan() {
                    "NaN".into()
                } else if f.is_infinite() {
                    if f.is_sign_positive() {
                        "Infinity".into()
                    } else {
                        "-Infinity".into()
                    }
                } else if f == 0.0 {
                    "0".into()
                } else {
                    f.to_string()
                }
            }
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
                        let len = o.elements.len();
                        let parts: Vec<String> = (0..len)
                            .map(|i| match o.elements.get(i) {
                                Some(v) if !v.is_null() && !v.is_undefined() => {
                                    self.value_to_rust_string(v)
                                }
                                _ => String::new(),
                            })
                            .collect();
                        return parts.join(",");
                    }
                }
                "[object Object]".into()
            }
        }
    }

    fn format_exception_value(&self, val: JsValue) -> String {
        if let Some(h) = val.as_object() {
            let name = self
                .heap
                .object_get_own(h, self.key_name, &self.shapes)
                .map(|p| self.value_to_rust_string(p.value));
            let msg = self
                .heap
                .object_get_own(h, self.key_message, &self.shapes)
                .map(|p| self.value_to_rust_string(p.value));
            if let (Some(n), Some(m)) = (name, msg) {
                return format!("{n}: {m}");
            }
        }
        self.value_to_rust_string(val)
    }

    pub fn make_error(&mut self, name: &str, message: impl Into<String>) -> JsException {
        let msg = message.into();
        let err_obj = self.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(self.error_proto),
        ));
        let nv = JsValue::from_string(self.heap.alloc_string(name));
        let mv = JsValue::from_string(self.heap.alloc_string(msg.as_str()));
        self.heap.object_define_data(
            err_obj,
            self.key_name,
            nv,
            PropertyAttrs::METHOD,
            &mut self.shapes,
        );
        self.heap.object_define_data(
            err_obj,
            self.key_message,
            mv,
            PropertyAttrs::METHOD,
            &mut self.shapes,
        );
        JsException {
            value: JsValue::from_object(err_obj),
            message: format!("{name}: {msg}"),
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
}

enum StepOutcome {
    Continue,
    ReturnTop(JsValue),
}

enum NativeInv {
    Static(NativeFnPtr),
    Closure(NativeClosure),
}
