//! Safe generational arena heap, tri-color incremental mark-sweep GC,
//! write barriers, root stack discipline, and leak-free weak handle tables
//! (`WeakHeapHandle` / `WeakTable`) ready for DOM wrappers.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::time::Instant;

use smallvec::SmallVec;

use crate::bigint::JsBigInt;
use crate::elements::ElementsStorage;
use crate::shape::{DictProperty, PropertyAttrs, ShapeId, ShapeTable, DICTIONARY_TRANSITION_LIMIT};
use crate::string::{JsString, PropertyKey, StringInterner};
use crate::value::{HeapHandle, JsValue, SymbolId};

/// Number of minor GC survivals before a Young object is promoted to Old generation.
const PROMOTE_AGE: u8 = 2;

/// Default nursery object budget before automatic minor collection.
pub const DEFAULT_NURSERY_LIMIT: usize = 4_096;

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

/// Captured closure payload stored inside a function [`JsObject`].
#[derive(Debug, Clone)]
pub struct ClosureObject {
    /// Compiled program identifier inside the VM.
    pub program_id: u32,
    /// Index into `CompiledProgram::functions`.
    pub func_idx: u32,
    /// Heap handles of [`JsObject`] upvalue cells (each holding its captured value in `slots[0]`).
    pub upvalues: Vec<HeapHandle>,
    /// Lexical `this` captured by arrow functions (`None` for ordinary functions).
    pub bound_this: Option<JsValue>,
    /// Class superclass constructor handle (if this function is a class constructor).
    pub super_ctor: Option<HeapHandle>,
    /// Optional instance field initializer `(program_id, func_idx)` for class constructors.
    pub field_init: Option<(u32, u32)>,
}

/// Bound function created by `Function.prototype.bind`.
#[derive(Debug, Clone)]
pub struct BoundFunctionObject {
    pub target: HeapHandle,
    pub bound_this: JsValue,
    pub bound_args: Vec<JsValue>,
}

/// ES6 `Proxy` wrapper (`new Proxy(target, handler)`).
#[derive(Debug, Clone, Copy)]
pub struct ProxyObject {
    pub target: HeapHandle,
    pub handler: HeapHandle,
    pub revoked: bool,
}

/// Out-of-line RegExp pattern, canonical flags, and cached compiled ECMAScript engine.
#[derive(Debug, Clone)]
pub struct RegExpObject {
    pub pattern: String,
    pub flags: String,
    pub compiled: Option<std::sync::Arc<regress::Regex>>,
}

/// Element type tag for ES6+ TypedArrays (including BigInt64, BigUint64, and Float16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TypedArrayKind {
    Int8 = 0,
    Uint8 = 1,
    Uint8Clamped = 2,
    Int16 = 3,
    Uint16 = 4,
    Int32 = 5,
    Uint32 = 6,
    Float32 = 7,
    Float64 = 8,
    BigInt64 = 9,
    BigUint64 = 10,
    Float16 = 11,
}

impl TypedArrayKind {
    pub const ALL: [Self; 12] = [
        Self::Int8,
        Self::Uint8,
        Self::Uint8Clamped,
        Self::Int16,
        Self::Uint16,
        Self::Int32,
        Self::Uint32,
        Self::Float32,
        Self::Float64,
        Self::BigInt64,
        Self::BigUint64,
        Self::Float16,
    ];

    #[inline]
    #[must_use]
    pub const fn bytes_per_element(self) -> usize {
        match self {
            Self::Int8 | Self::Uint8 | Self::Uint8Clamped => 1,
            Self::Int16 | Self::Uint16 | Self::Float16 => 2,
            Self::Int32 | Self::Uint32 | Self::Float32 => 4,
            Self::Float64 | Self::BigInt64 | Self::BigUint64 => 8,
        }
    }

    #[inline]
    #[must_use]
    pub const fn is_bigint(self) -> bool {
        matches!(self, Self::BigInt64 | Self::BigUint64)
    }

    #[inline]
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Int8 => "Int8Array",
            Self::Uint8 => "Uint8Array",
            Self::Uint8Clamped => "Uint8ClampedArray",
            Self::Int16 => "Int16Array",
            Self::Uint16 => "Uint16Array",
            Self::Int32 => "Int32Array",
            Self::Uint32 => "Uint32Array",
            Self::Float32 => "Float32Array",
            Self::Float64 => "Float64Array",
            Self::BigInt64 => "BigInt64Array",
            Self::BigUint64 => "BigUint64Array",
            Self::Float16 => "Float16Array",
        }
    }
}

/// `DataView` view over an [`ObjectKind::ArrayBuffer`].
#[derive(Debug, Clone, Copy)]
pub struct DataViewObject {
    pub buffer: HeapHandle,
    pub byte_offset: u32,
    pub byte_length: u32,
}

/// `TypedArray` view over an [`ObjectKind::ArrayBuffer`].
#[derive(Debug, Clone, Copy)]
pub struct TypedArrayObject {
    pub kind: TypedArrayKind,
    pub buffer: HeapHandle,
    pub byte_offset: u32,
    pub length: u32,
}

/// Suspended register-window execution frame for `async` functions, generators,
/// and top-level-`await` ES modules.
#[derive(Debug, Clone)]
pub struct SuspendedFrame {
    pub program_id: u32,
    pub func_idx: u32,
    pub pc: usize,
    pub this_val: JsValue,
    pub callee: HeapHandle,
    pub saved_regs: Vec<JsValue>,
    /// Original invocation arguments preserved for lazy `arguments` materialization.
    pub call_args: Vec<JsValue>,
    /// Open upvalue cells `(relative_reg_offset, cell_handle)` detached on suspend
    /// and re-attached on resume so closures stay live across `await`/`yield`.
    pub saved_upvalues: Vec<(usize, HeapHandle)>,
    pub resume_dst: u8,
}

impl SuspendedFrame {
    pub fn trace_children(&self, mut visit: impl FnMut(HeapHandle)) {
        if let Some(h) = self.this_val.as_heap_handle() {
            visit(h);
        }
        visit(self.callee);
        for &v in &self.saved_regs {
            if let Some(h) = v.as_heap_handle() {
                visit(h);
            }
        }
        for &v in &self.call_args {
            if let Some(h) = v.as_heap_handle() {
                visit(h);
            }
        }
        for &(_, cell) in &self.saved_upvalues {
            visit(cell);
        }
    }
}

/// Resolution state of an ECMAScript `Promise`.
#[derive(Debug, Clone, Copy)]
pub enum PromiseState {
    Pending,
    Fulfilled(JsValue),
    Rejected(JsValue),
}

/// Reaction record queued on a pending [`PromiseObject`].
#[derive(Debug, Clone, Copy)]
pub struct PromiseReaction {
    pub on_fulfilled: Option<HeapHandle>,
    pub on_rejected: Option<HeapHandle>,
    pub result_promise: Option<HeapHandle>,
}

/// Backing storage for an ECMAScript `Promise` object.
#[derive(Debug, Clone)]
pub struct PromiseObject {
    pub state: PromiseState,
    pub reactions: Vec<PromiseReaction>,
    pub handled: bool,
}

impl Default for PromiseObject {
    fn default() -> Self {
        Self {
            state: PromiseState::Pending,
            reactions: Vec::new(),
            handled: false,
        }
    }
}

/// Lifecycle state of an ECMAScript `Generator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratorState {
    SuspendedStart,
    SuspendedYield,
    Executing,
    Completed,
}

/// Queued request kind for an ECMAScript `AsyncGenerator` (`async function*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncGeneratorRequestKind {
    Next,
    Return,
    Throw,
}

/// Queued step request waiting on an executing `AsyncGenerator`.
#[derive(Debug, Clone, Copy)]
pub struct AsyncGeneratorRequest {
    pub kind: AsyncGeneratorRequestKind,
    pub value: JsValue,
    pub promise: HeapHandle,
}

/// Backing storage for an ECMAScript `Generator` or `AsyncGenerator` object.
#[derive(Debug, Clone)]
pub struct GeneratorObject {
    pub state: GeneratorState,
    pub frame: Option<SuspendedFrame>,
    pub delegate_iter: Option<HeapHandle>,
    pub is_async: bool,
    pub pending_requests: VecDeque<AsyncGeneratorRequest>,
}

/// Getter/setter handle pair for accessor properties.
pub type AccessorPair = (Option<HeapHandle>, Option<HeapHandle>);

/// Private class element storage entry (ECMA-262 §7.3.26–7.3.30).
#[derive(Debug, Clone, Copy)]
pub enum PrivateEntry {
    Field(JsValue),
    Method(HeapHandle),
    Accessor(Option<HeapHandle>, Option<HeapHandle>),
}

/// Metadata attached to a class constructor for `InitializeInstanceElements`.
#[derive(Debug, Clone, Copy)]
pub struct ClassInstanceInit {
    pub proto: HeapHandle,
    pub field_init_fn: Option<HeapHandle>,
    pub is_derived: bool,
}

/// Per-waiter condvar signal cell for FIFO `Atomics.wait` / `Atomics.notify`.
pub type AtomicWaiterSignal = std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>;

/// Thread-safe pure-Rust backing storage and FIFO waiter queue for `SharedArrayBuffer` & `Atomics`.
#[derive(Debug)]
pub struct SharedBufferBacking {
    pub data: std::sync::Mutex<Vec<u8>>,
    pub max_byte_length: Option<usize>,
    pub waiters: std::sync::Mutex<std::collections::VecDeque<(usize, AtomicWaiterSignal)>>,
}

/// Outcome of an `Atomics.wait` operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomicWaitResult {
    Ok,
    NotEqual,
    TimedOut,
}

impl SharedBufferBacking {
    pub fn new(bytes: Vec<u8>, max_byte_length: Option<usize>) -> Self {
        Self {
            data: std::sync::Mutex::new(bytes),
            max_byte_length,
            waiters: std::sync::Mutex::new(std::collections::VecDeque::new()),
        }
    }

    pub fn byte_len(&self) -> usize {
        self.data.lock().map_or(0, |d| d.len())
    }

    pub fn grow(&self, new_len: usize) -> Result<(), &'static str> {
        let Some(max) = self.max_byte_length else {
            return Err("not_growable");
        };
        let mut guard = self.data.lock().map_err(|_| "poisoned")?;
        if new_len < guard.len() || new_len > max {
            return Err("range");
        }
        guard.resize(new_len, 0);
        Ok(())
    }

    pub fn wait_at(
        &self,
        byte_offset: usize,
        check_eq: impl FnOnce(&[u8]) -> bool,
        timeout: Option<std::time::Duration>,
    ) -> AtomicWaitResult {
        let entry = {
            let data = self.data.lock().unwrap_or_else(|e| e.into_inner());
            if !check_eq(&data) {
                return AtomicWaitResult::NotEqual;
            }
            if timeout == Some(std::time::Duration::ZERO) {
                return AtomicWaitResult::TimedOut;
            }
            let e = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
            let mut w = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
            w.push_back((byte_offset, std::sync::Arc::clone(&e)));
            e
        };
        let (lock, cvar) = &*entry;
        let mut notified = lock.lock().unwrap_or_else(|e| e.into_inner());
        match timeout {
            None => {
                while !*notified {
                    notified = cvar.wait(notified).unwrap_or_else(|e| e.into_inner());
                }
                AtomicWaitResult::Ok
            }
            Some(dur) => {
                let deadline = std::time::Instant::now() + dur;
                while !*notified {
                    let Some(rem) = deadline.checked_duration_since(std::time::Instant::now())
                    else {
                        break;
                    };
                    let (g, res) = cvar
                        .wait_timeout(notified, rem)
                        .unwrap_or_else(|e| e.into_inner());
                    notified = g;
                    if res.timed_out() {
                        break;
                    }
                }
                if *notified {
                    return AtomicWaitResult::Ok;
                }
                drop(notified);
                let mut w = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
                let before = w.len();
                w.retain(|(_, e)| !std::sync::Arc::ptr_eq(e, &entry));
                if w.len() == before {
                    AtomicWaitResult::Ok
                } else {
                    AtomicWaitResult::TimedOut
                }
            }
        }
    }

    pub fn notify_at(&self, byte_offset: usize, count: usize) -> usize {
        if count == 0 {
            return 0;
        }
        let mut to_wake = Vec::new();
        {
            let mut w = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
            let mut i = 0usize;
            while i < w.len() && to_wake.len() < count {
                if w[i].0 == byte_offset {
                    if let Some((_, e)) = w.remove(i) {
                        to_wake.push(e);
                    }
                } else {
                    i += 1;
                }
            }
        }
        let woken = to_wake.len();
        for e in to_wake {
            let (lock, cvar) = &*e;
            let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
            *n = true;
            cvar.notify_one();
        }
        woken
    }
}

/// Resource entry registered inside a `DisposableStack` or `AsyncDisposableStack`.
#[derive(Debug, Clone, Copy)]
pub struct DisposableEntry {
    pub value: JsValue,
    pub method: Option<HeapHandle>,
    /// `0` = `use` (method.call(value)), `1` = `adopt` (method.call(undefined, value)),
    /// `2` = `defer` (method.call(undefined)), `3` = sync-method wrapped for async stack,
    /// `4` = null/undefined async tick entry.
    pub kind: u8,
}

/// Internal slots for `DisposableStack` (`is_async = false`) and `AsyncDisposableStack` (`is_async = true`).
#[derive(Debug, Clone)]
pub struct DisposableStackData {
    pub is_async: bool,
    pub disposed: bool,
    pub entries: Vec<DisposableEntry>,
}

/// Cold out-of-line object metadata (only allocated for dictionary-mode objects,
/// accessors, class constructors, or private fields so ordinary shaped objects stay compact in cache).
#[derive(Debug, Clone, Default)]
pub struct RareObjectData {
    pub dict_props: HashMap<PropertyKey, DictProperty>,
    pub dict_order: Vec<PropertyKey>,
    pub accessors: HashMap<PropertyKey, AccessorPair>,
    pub private_fields: HashMap<PropertyKey, PrivateEntry>,
    pub class_instance_init: Option<ClassInstanceInit>,
    /// Primitive slot (`[[NumberData]]`, `[[StringData]]`, `[[BooleanData]]`, `[[BigIntData]]`, `[[SymbolData]]`).
    pub boxed_value: Option<JsValue>,
    /// Resizable `ArrayBuffer` `[[ArrayBufferMaxByteLength]]` (`None` = fixed length).
    pub ab_max_byte_length: Option<usize>,
    /// `true` once an `ArrayBuffer` has been detached (`transfer` / `$262.detachArrayBuffer`).
    pub ab_detached: bool,
    /// `true` for immutable `ArrayBuffer` (`transferToImmutable` / `sliceToImmutable`).
    pub ab_immutable: bool,
    /// Shared cross-thread backing for `SharedArrayBuffer`.
    pub shared_mem: Option<std::sync::Arc<SharedBufferBacking>>,
    /// `true` when a `DataView` or `TypedArray` is length-tracking over a resizable/growable buffer.
    pub view_auto_length: bool,
    /// Explicit Resource Management stack slot (`[[DisposableState]]` / `[[DisposeCapability]]`).
    pub disposable_stack: Option<Box<DisposableStackData>>,
    /// `[[ErrorData]]` internal brand for `Error.isError`.
    pub is_error_data: bool,
    /// `[[IsRawJSON]]` internal brand for `JSON.rawJSON` / `JSON.isRawJSON`.
    pub is_raw_json: bool,
    /// Non-registered Symbol keys stored weakly in `WeakMap` (ECMA-262 ES2023).
    pub weak_sym_map: HashMap<crate::value::SymbolId, JsValue>,
    /// Non-registered Symbol entries stored weakly in `WeakSet` (ECMA-262 ES2023).
    pub weak_sym_set: HashSet<crate::value::SymbolId>,
    /// Non-registered Symbol target held by a `WeakRef` (ECMA-262 ES2023).
    pub weak_ref_sym: Option<crate::value::SymbolId>,
}

/// Specialized runtime tag for a [`JsObject`].
#[derive(Debug, Clone, Default)]
pub enum ObjectKind {
    #[default]
    Ordinary,
    Array,
    Closure(Box<ClosureObject>),
    NativeFunction(u32),
    BoundFunction(Box<BoundFunctionObject>),
    Proxy(ProxyObject),
    UpvalueCell,
    RegExp(Box<RegExpObject>),
    Date(f64),
    Map(Box<Vec<(JsValue, JsValue)>>),
    Set(Box<Vec<JsValue>>),
    WeakMap(Box<HashMap<HeapHandle, JsValue>>),
    WeakSet(Box<HashSet<HeapHandle>>),
    WeakRef(HeapHandle),
    ArrayBuffer(Box<Vec<u8>>),
    DataView(DataViewObject),
    TypedArray(TypedArrayObject),
    Promise(Box<PromiseObject>),
    Generator(Box<GeneratorObject>),
    Host,
}

/// Mutable JS object backing storage inside the GC arena.
#[derive(Debug, Clone)]
pub struct JsObject {
    pub shape_id: u32,
    pub transition_count: u16,
    pub is_prototype: bool,
    pub extensible: bool,
    pub is_constructor: bool,
    pub is_global: bool,
    pub prototype: Option<HeapHandle>,
    pub slots: Vec<JsValue>,
    pub elements: ElementsStorage,
    pub kind: ObjectKind,
    pub rare: Option<Box<RareObjectData>>,
    pub extra_refs: SmallVec<[HeapHandle; 2]>,
    pub host_tag: Option<u64>,
}

impl Default for JsObject {
    fn default() -> Self {
        Self {
            shape_id: ShapeId::ROOT.0,
            transition_count: 0,
            is_prototype: false,
            extensible: true,
            is_constructor: false,
            is_global: false,
            prototype: None,
            slots: Vec::new(),
            elements: ElementsStorage::default(),
            kind: ObjectKind::Ordinary,
            rare: None,
            extra_refs: SmallVec::new(),
            host_tag: None,
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

    #[must_use]
    pub fn with_kind(kind: ObjectKind, prototype: Option<HeapHandle>) -> Self {
        Self {
            kind,
            prototype,
            ..Self::default()
        }
    }

    #[inline]
    #[must_use]
    pub fn shape(&self) -> ShapeId {
        ShapeId(self.shape_id)
    }

    #[inline]
    #[must_use]
    pub fn is_dictionary(&self) -> bool {
        self.shape().is_dictionary()
    }

    #[inline]
    pub fn rare_mut(&mut self) -> &mut RareObjectData {
        self.rare.get_or_insert_with(Default::default)
    }

    /// Converts a shaped object into per-object dictionary mode preserving
    /// insertion order, values, attributes, and accessor pairs.
    pub fn to_dictionary_mode(&mut self, shapes: &ShapeTable) {
        if self.is_dictionary() {
            return;
        }
        let mut dict = HashMap::new();
        let mut order = Vec::new();
        if let Some(shape) = shapes.get(self.shape()) {
            for &key in shape.keys() {
                if let Some(slot) = shape.lookup(key) {
                    let val = self
                        .slots
                        .get(slot.offset as usize)
                        .copied()
                        .unwrap_or(JsValue::UNDEFINED);
                    let (getter, setter) = if slot.attrs.contains(PropertyAttrs::ACCESSOR) {
                        self.rare
                            .as_ref()
                            .and_then(|r| r.accessors.get(&key).copied())
                            .unwrap_or((None, None))
                    } else {
                        (None, None)
                    };
                    dict.insert(
                        key,
                        DictProperty {
                            value: val,
                            attrs: slot.attrs,
                            getter,
                            setter,
                        },
                    );
                    order.push(key);
                }
            }
        }
        self.slots.clear();
        let rare = self.rare_mut();
        rare.accessors.clear();
        rare.dict_props = dict;
        rare.dict_order = order;
        self.shape_id = ShapeId::DICTIONARY.0;
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
        self.elements.trace_children(&mut visit);
        if let Some(rare) = &self.rare {
            for prop in rare.dict_props.values() {
                if let Some(h) = prop.value.as_heap_handle() {
                    visit(h);
                }
                if let Some(g) = prop.getter {
                    visit(g);
                }
                if let Some(s) = prop.setter {
                    visit(s);
                }
            }
            for &(g, s) in rare.accessors.values() {
                if let Some(h) = g {
                    visit(h);
                }
                if let Some(h) = s {
                    visit(h);
                }
            }
            for entry in rare.private_fields.values() {
                match *entry {
                    PrivateEntry::Field(val) => {
                        if let Some(h) = val.as_heap_handle() {
                            visit(h);
                        }
                    }
                    PrivateEntry::Method(h) => visit(h),
                    PrivateEntry::Accessor(g, s) => {
                        if let Some(h) = g {
                            visit(h);
                        }
                        if let Some(h) = s {
                            visit(h);
                        }
                    }
                }
            }
            if let Some(init) = rare.class_instance_init {
                visit(init.proto);
                if let Some(f) = init.field_init_fn {
                    visit(f);
                }
            }
            if let Some(bv) = rare.boxed_value {
                if let Some(h) = bv.as_heap_handle() {
                    visit(h);
                }
            }
            if let Some(ds) = &rare.disposable_stack {
                for entry in &ds.entries {
                    if let Some(h) = entry.value.as_heap_handle() {
                        visit(h);
                    }
                    if let Some(m) = entry.method {
                        visit(m);
                    }
                }
            }
            for &val in rare.weak_sym_map.values() {
                if let Some(h) = val.as_heap_handle() {
                    visit(h);
                }
            }
        }
        match &self.kind {
            ObjectKind::Closure(c) => {
                for &uv in &c.upvalues {
                    visit(uv);
                }
                if let Some(h) = c.bound_this.and_then(JsValue::as_heap_handle) {
                    visit(h);
                }
                if let Some(sup) = c.super_ctor {
                    visit(sup);
                }
            }
            ObjectKind::BoundFunction(b) => {
                visit(b.target);
                if let Some(h) = b.bound_this.as_heap_handle() {
                    visit(h);
                }
                for &arg in &b.bound_args {
                    if let Some(h) = arg.as_heap_handle() {
                        visit(h);
                    }
                }
            }
            ObjectKind::Proxy(p) => {
                visit(p.target);
                visit(p.handler);
            }
            ObjectKind::Map(entries) => {
                for &(k, v) in entries.iter() {
                    if let Some(h) = k.as_heap_handle() {
                        visit(h);
                    }
                    if let Some(h) = v.as_heap_handle() {
                        visit(h);
                    }
                }
            }
            ObjectKind::Set(entries) => {
                for &v in entries.iter() {
                    if let Some(h) = v.as_heap_handle() {
                        visit(h);
                    }
                }
            }
            ObjectKind::DataView(dv) => {
                visit(dv.buffer);
            }
            ObjectKind::TypedArray(ta) => {
                visit(ta.buffer);
            }
            ObjectKind::Promise(p) => {
                match p.state {
                    PromiseState::Fulfilled(v) | PromiseState::Rejected(v) => {
                        if let Some(h) = v.as_heap_handle() {
                            visit(h);
                        }
                    }
                    PromiseState::Pending => {}
                }
                for r in &p.reactions {
                    if let Some(h) = r.on_fulfilled {
                        visit(h);
                    }
                    if let Some(h) = r.on_rejected {
                        visit(h);
                    }
                    if let Some(h) = r.result_promise {
                        visit(h);
                    }
                }
            }
            ObjectKind::Generator(g) => {
                if let Some(ref f) = g.frame {
                    f.trace_children(&mut visit);
                }
                if let Some(d) = g.delegate_iter {
                    visit(d);
                }
                for req in &g.pending_requests {
                    if let Some(h) = req.value.as_heap_handle() {
                        visit(h);
                    }
                    visit(req.promise);
                }
            }
            // WeakMap, WeakSet, and WeakRef do NOT strongly trace weak keys or targets.
            // WeakMap ephemeron values are traced during GC ephemeron resolution when keys are live.
            ObjectKind::Ordinary
            | ObjectKind::Array
            | ObjectKind::NativeFunction(_)
            | ObjectKind::UpvalueCell
            | ObjectKind::RegExp(_)
            | ObjectKind::Date(_)
            | ObjectKind::WeakMap(_)
            | ObjectKind::WeakSet(_)
            | ObjectKind::WeakRef(_)
            | ObjectKind::ArrayBuffer(_)
            | ObjectKind::Host => {}
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
    weak_collections: Vec<HeapHandle>,
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
            weak_collections: Vec::new(),
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

    #[inline]
    #[must_use]
    pub fn interner(&self) -> &StringInterner {
        &self.interner
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
        let track_weak = matches!(obj.kind, ObjectKind::WeakMap(_) | ObjectKind::WeakSet(_));
        let h = self.alloc_cell(HeapCell::Object(obj), Generation::Young);
        if track_weak {
            self.weak_collections.push(h);
        }
        h
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

    /// Sets `owner.[[Prototype]] = proto`, marking `proto.is_prototype = true`,
    /// bumping the prototype-chain epoch, and invoking the write barrier.
    pub fn object_set_prototype(
        &mut self,
        owner: HeapHandle,
        proto: Option<HeapHandle>,
        shapes: &mut ShapeTable,
    ) -> bool {
        if let Some(p) = proto {
            if let Some(proto_obj) = self.get_object_mut(p) {
                proto_obj.is_prototype = true;
            }
        }
        let Some(obj) = self.get_object_mut(owner) else {
            return false;
        };
        obj.prototype = proto;
        shapes.bump_prototype_epoch();
        if let Some(p) = proto {
            self.write_barrier(owner, JsValue::from_object(p));
        }
        true
    }

    /// Reads an own property value or descriptor from `owner` (shape or dictionary mode).
    #[must_use]
    pub fn object_get_own(
        &self,
        owner: HeapHandle,
        key: PropertyKey,
        shapes: &ShapeTable,
    ) -> Option<DictProperty> {
        let obj = self.get_object(owner)?;
        if obj.is_dictionary() {
            return obj.rare.as_ref()?.dict_props.get(&key).copied();
        }
        let slot = shapes.lookup(obj.shape(), key)?;
        let value = obj
            .slots
            .get(slot.offset as usize)
            .copied()
            .unwrap_or(JsValue::UNDEFINED);
        let (getter, setter) = if slot.attrs.contains(PropertyAttrs::ACCESSOR) {
            obj.rare
                .as_ref()
                .and_then(|r| r.accessors.get(&key).copied())
                .unwrap_or((None, None))
        } else {
            (None, None)
        };
        Some(DictProperty {
            value,
            attrs: slot.attrs,
            getter,
            setter,
        })
    }

    /// Defines or updates an own data property on `owner`, advancing its Shape
    /// transition tree or falling back automatically to dictionary mode when
    /// `transition_count >= DICTIONARY_TRANSITION_LIMIT`.
    ///
    /// Returns `Some((prev_shape, new_shape, slot_offset))` when the object
    /// remained in shaped mode (usable to populate an Inline Cache), or `None`
    /// if stored in dictionary mode or rejected (`!extensible`).
    pub fn object_define_data(
        &mut self,
        owner: HeapHandle,
        key: PropertyKey,
        value: JsValue,
        attrs: PropertyAttrs,
        shapes: &mut ShapeTable,
    ) -> Option<(ShapeId, ShapeId, u32)> {
        let is_proto = self.get_object(owner)?.is_prototype;
        let cur_shape = self.get_object(owner)?.shape();
        let extensible = self.get_object(owner)?.extensible;

        if cur_shape.is_dictionary() {
            let obj = self.get_object_mut(owner)?;
            let rare = obj.rare_mut();
            if let Some(entry) = rare.dict_props.get_mut(&key) {
                entry.value = value;
                entry.attrs = attrs;
                entry.getter = None;
                entry.setter = None;
            } else {
                if !extensible {
                    return None;
                }
                rare.dict_props
                    .insert(key, DictProperty::data(value, attrs));
                rare.dict_order.push(key);
            }
            if is_proto {
                shapes.bump_prototype_epoch();
            }
            self.write_barrier(owner, value);
            return None;
        }

        // Existing shaped property update.
        if let Some(existing) = shapes.lookup(cur_shape, key) {
            let new_shape = if existing.attrs == attrs {
                cur_shape
            } else {
                let (ns, _) = shapes.transition_add(cur_shape, key, attrs);
                ns
            };
            let obj = self.get_object_mut(owner)?;
            obj.shape_id = new_shape.0;
            let idx = existing.offset as usize;
            if idx >= obj.slots.len() {
                obj.slots.resize(idx + 1, JsValue::UNDEFINED);
            }
            obj.slots[idx] = value;
            if let Some(rare) = &mut obj.rare {
                rare.accessors.remove(&key);
            }
            if is_proto {
                shapes.bump_prototype_epoch();
            }
            self.write_barrier(owner, value);
            return Some((cur_shape, new_shape, existing.offset));
        }

        // Adding a new own property.
        if !extensible {
            return None;
        }
        let (tc, is_global) = {
            let o = self.get_object(owner)?;
            (o.transition_count, o.is_global)
        };
        let limit = if is_global {
            1024
        } else {
            DICTIONARY_TRANSITION_LIMIT
        };
        if tc >= limit {
            let obj = self.get_object_mut(owner)?;
            obj.to_dictionary_mode(shapes);
            let rare = obj.rare_mut();
            rare.dict_props
                .insert(key, DictProperty::data(value, attrs));
            rare.dict_order.push(key);
            if is_proto {
                shapes.bump_prototype_epoch();
            }
            self.write_barrier(owner, value);
            return None;
        }

        let (new_shape, offset) = shapes.transition_add(cur_shape, key, attrs);
        let obj = self.get_object_mut(owner)?;
        obj.shape_id = new_shape.0;
        obj.transition_count = obj.transition_count.saturating_add(1);
        let idx = offset as usize;
        if idx >= obj.slots.len() {
            obj.slots.resize(idx + 1, JsValue::UNDEFINED);
        }
        obj.slots[idx] = value;
        if is_proto {
            shapes.bump_prototype_epoch();
        }
        self.write_barrier(owner, value);
        Some((cur_shape, new_shape, offset))
    }

    /// Defines an own getter and/or setter accessor property on `owner`.
    pub fn object_define_accessor(
        &mut self,
        owner: HeapHandle,
        key: PropertyKey,
        getter: Option<HeapHandle>,
        setter: Option<HeapHandle>,
        attrs: PropertyAttrs,
        shapes: &mut ShapeTable,
    ) -> bool {
        let Some(obj_ref) = self.get_object(owner) else {
            return false;
        };
        let is_proto = obj_ref.is_prototype;
        let cur_shape = obj_ref.shape();
        let acc_attrs = attrs.union(PropertyAttrs::ACCESSOR);

        if cur_shape.is_dictionary() {
            let Some(obj) = self.get_object_mut(owner) else {
                return false;
            };
            let rare = obj.rare_mut();
            let entry = rare
                .dict_props
                .entry(key)
                .or_insert_with(|| DictProperty::accessor(None, None, acc_attrs));
            entry.attrs = acc_attrs;
            if getter.is_some() {
                entry.getter = getter;
            }
            if setter.is_some() {
                entry.setter = setter;
            }
            if !rare.dict_order.contains(&key) {
                rare.dict_order.push(key);
            }
        } else {
            let (new_shape, offset) = shapes.transition_add(cur_shape, key, acc_attrs);
            let Some(obj) = self.get_object_mut(owner) else {
                return false;
            };
            obj.shape_id = new_shape.0;
            let idx = offset as usize;
            if idx >= obj.slots.len() {
                obj.slots.resize(idx + 1, JsValue::UNDEFINED);
            }
            obj.slots[idx] = JsValue::UNDEFINED;
            let rare = obj.rare_mut();
            let pair = rare.accessors.entry(key).or_insert((None, None));
            if getter.is_some() {
                pair.0 = getter;
            }
            if setter.is_some() {
                pair.1 = setter;
            }
        }

        if is_proto {
            shapes.bump_prototype_epoch();
        }
        if let Some(g) = getter {
            self.write_barrier(owner, JsValue::from_object(g));
        }
        if let Some(s) = setter {
            self.write_barrier(owner, JsValue::from_object(s));
        }
        true
    }

    /// Deletes own property `key` from `owner` (respecting `CONFIGURABLE`).
    /// Uses fast tail rollback if deleting the last added property, or transitions
    /// cleanly into dictionary mode otherwise.
    pub fn object_delete_own(
        &mut self,
        owner: HeapHandle,
        key: PropertyKey,
        shapes: &mut ShapeTable,
    ) -> bool {
        let Some(obj_ref) = self.get_object(owner) else {
            return true;
        };
        let is_proto = obj_ref.is_prototype;
        let cur_shape = obj_ref.shape();

        if cur_shape.is_dictionary() {
            let Some(obj) = self.get_object_mut(owner) else {
                return true;
            };
            if let Some(rare) = &mut obj.rare {
                if let Some(prop) = rare.dict_props.get(&key) {
                    if !prop.attrs.contains(PropertyAttrs::CONFIGURABLE) {
                        return false;
                    }
                    rare.dict_props.remove(&key);
                    rare.dict_order.retain(|&k| k != key);
                    if is_proto {
                        shapes.bump_prototype_epoch();
                    }
                }
            }
            return true;
        }

        let Some(slot) = shapes.lookup(cur_shape, key) else {
            return true;
        };
        if !slot.attrs.contains(PropertyAttrs::CONFIGURABLE) {
            return false;
        }

        if let Some((parent_shape, offset)) = shapes.try_tail_delete(cur_shape, key) {
            if let Some(obj) = self.get_object_mut(owner) {
                obj.shape_id = parent_shape.0;
                obj.transition_count = obj.transition_count.saturating_add(1);
                if (offset as usize) + 1 == obj.slots.len() {
                    obj.slots.pop();
                } else if (offset as usize) < obj.slots.len() {
                    obj.slots[offset as usize] = JsValue::UNDEFINED;
                }
                if let Some(rare) = &mut obj.rare {
                    rare.accessors.remove(&key);
                }
            }
        } else if let Some(obj) = self.get_object_mut(owner) {
            obj.to_dictionary_mode(shapes);
            if let Some(rare) = &mut obj.rare {
                rare.dict_props.remove(&key);
                rare.dict_order.retain(|&k| k != key);
            }
        }

        if is_proto {
            shapes.bump_prototype_epoch();
        }
        true
    }

    /// Replaces an accessor property's getter and setter verbatim (allowing `None` to clear).
    pub fn object_replace_accessor(
        &mut self,
        owner: HeapHandle,
        key: PropertyKey,
        getter: Option<HeapHandle>,
        setter: Option<HeapHandle>,
        attrs: PropertyAttrs,
        shapes: &mut ShapeTable,
    ) -> bool {
        let Some(obj_ref) = self.get_object(owner) else {
            return false;
        };
        let is_proto = obj_ref.is_prototype;
        let cur_shape = obj_ref.shape();
        let acc_attrs = attrs.union(PropertyAttrs::ACCESSOR);

        if cur_shape.is_dictionary() {
            let Some(obj) = self.get_object_mut(owner) else {
                return false;
            };
            let rare = obj.rare_mut();
            let entry = rare
                .dict_props
                .entry(key)
                .or_insert_with(|| DictProperty::accessor(None, None, acc_attrs));
            entry.attrs = acc_attrs;
            entry.getter = getter;
            entry.setter = setter;
            if !rare.dict_order.contains(&key) {
                rare.dict_order.push(key);
            }
        } else {
            let (new_shape, offset) = shapes.transition_add(cur_shape, key, acc_attrs);
            let Some(obj) = self.get_object_mut(owner) else {
                return false;
            };
            obj.shape_id = new_shape.0;
            let idx = offset as usize;
            if idx >= obj.slots.len() {
                obj.slots.resize(idx + 1, JsValue::UNDEFINED);
            }
            obj.slots[idx] = JsValue::UNDEFINED;
            let rare = obj.rare_mut();
            rare.accessors.insert(key, (getter, setter));
        }

        if is_proto {
            shapes.bump_prototype_epoch();
        }
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

        // 3. Trace transitive Young survivors + WeakMap ephemerons.
        loop {
            while let Some(idx) = worklist.pop() {
                scratch.clear();
                if let Some(cell) = &self.slots[idx as usize].cell {
                    cell.trace_children(|h| scratch.push(h));
                }
                for &child in &scratch {
                    self.mark_if_young(child, epoch, &mut worklist);
                }
            }
            if self.weak_collections.is_empty() {
                break;
            }
            let mut ephem_marked = false;
            for wi in 0..self.weak_collections.len() {
                let wh = self.weak_collections[wi];
                let Some(s) = self.slots.get(wh.index as usize) else {
                    continue;
                };
                if s.cell.is_none()
                    || s.generation != wh.generation
                    || (s.gen_space == Generation::Young && s.mark_epoch != epoch)
                {
                    continue;
                }
                let pairs: Vec<(HeapHandle, JsValue)> = match &s.cell {
                    Some(HeapCell::Object(o)) => match &o.kind {
                        ObjectKind::WeakMap(m) => m.iter().map(|(&k, &v)| (k, v)).collect(),
                        _ => continue,
                    },
                    _ => continue,
                };
                for (k, v) in pairs {
                    let key_live = self.slots.get(k.index as usize).is_some_and(|ks| {
                        ks.cell.is_some()
                            && ks.generation == k.generation
                            && (ks.gen_space == Generation::Old || ks.mark_epoch == epoch)
                    });
                    if key_live {
                        if let Some(vh) = v.as_heap_handle() {
                            let before = worklist.len();
                            self.mark_if_young(vh, epoch, &mut worklist);
                            if worklist.len() > before {
                                ephem_marked = true;
                            }
                        }
                    }
                }
            }
            if !ephem_marked {
                break;
            }
        }

        // 4. Sweep ONLY the nursery in-place (zero allocation): promote aged survivors, reclaim dead slots.
        let mut write_pos = 0usize;
        let nursery_len = self.nursery.len();
        let mut reclaimed = 0usize;
        let mut promoted = 0usize;

        for read_pos in 0..nursery_len {
            let idx = self.nursery[read_pos];
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
                    self.nursery[write_pos] = idx;
                    write_pos += 1;
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

        self.nursery.truncate(write_pos);
        self.remembered_set = kept_remembered;
        self.prune_dead_weak_collections();
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

        // Ephemeron fixed-point loop for live WeakMaps: if both the WeakMap and a key
        // are marked live in this major epoch, mark the entry's value too.
        while !self.weak_collections.is_empty() {
            let mut ephem_marked = false;
            for wi in 0..self.weak_collections.len() {
                let wh = self.weak_collections[wi];
                let Some(s) = self.slots.get(wh.index as usize) else {
                    continue;
                };
                if s.cell.is_none() || s.generation != wh.generation || s.mark_epoch != epoch {
                    continue;
                }
                let pairs: Vec<(HeapHandle, JsValue)> = match &s.cell {
                    Some(HeapCell::Object(o)) => match &o.kind {
                        ObjectKind::WeakMap(m) => m.iter().map(|(&k, &v)| (k, v)).collect(),
                        _ => continue,
                    },
                    _ => continue,
                };
                for (k, v) in pairs {
                    let key_marked = self.slots.get(k.index as usize).is_some_and(|ks| {
                        ks.cell.is_some() && ks.generation == k.generation && ks.mark_epoch == epoch
                    });
                    if key_marked {
                        if let Some(vh) = v.as_heap_handle() {
                            let before = self.gray_worklist.len();
                            self.mark_any(vh, epoch);
                            if self.gray_worklist.len() > before {
                                ephem_marked = true;
                            }
                        }
                    }
                }
            }
            if !ephem_marked {
                break;
            }
            while !self.step_major_marking(usize::MAX) {}
        }

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
        self.prune_dead_weak_collections();
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

    fn prune_dead_weak_collections(&mut self) {
        if self.weak_collections.is_empty() {
            return;
        }
        let handles = std::mem::take(&mut self.weak_collections);
        let mut kept = Vec::with_capacity(handles.len());
        for wh in handles {
            if !self.is_alive(wh) {
                continue;
            }
            kept.push(wh);
            // Collect dead keys first to satisfy borrow checker.
            let dead_keys: Vec<HeapHandle> = match self.get_object(wh).map(|o| &o.kind) {
                Some(ObjectKind::WeakMap(m)) => {
                    m.keys().copied().filter(|&k| !self.is_alive(k)).collect()
                }
                Some(ObjectKind::WeakSet(s)) => {
                    s.iter().copied().filter(|&k| !self.is_alive(k)).collect()
                }
                _ => Vec::new(),
            };
            if !dead_keys.is_empty() {
                if let Some(obj) = self.get_object_mut(wh) {
                    match &mut obj.kind {
                        ObjectKind::WeakMap(m) => {
                            for k in dead_keys {
                                m.remove(&k);
                            }
                        }
                        ObjectKind::WeakSet(s) => {
                            for k in dead_keys {
                                s.remove(&k);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        self.weak_collections = kept;
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
