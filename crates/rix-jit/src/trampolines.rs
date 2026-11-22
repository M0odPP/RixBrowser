//! Audited `extern "C"` runtime slow-path & Inline-Cache trampolines invoked by
//! Cranelift-compiled Baseline and Optimizing JIT code.
//!
//! ABI contract:
//! - Every normal ECMAScript `JsValue` has high 16 bits `<= 0xFFFE`.
//! - `JIT_EXCEPTION_SENTINEL` (`0xFFFF_FFFF_FFFF_FFFF`) signals that a JS exception
//!   was stored into `vm.pending_jit_exception` and the JIT frame must immediately unwind.
//! - `JIT_DEOPT_TAG_MASK` (`0xFFFF_0000_0000_0000`) encodes a clean deoptimization back
//!   to bytecode `resume_pc = (raw & 0xFFFF_FFFF) as usize`.
//! - Trampolines operate directly on the live `JsVm` register window at `base_reg`
//!   without ever allocating or copying shadow register arrays.

use rix_js::{JsValue, JsVm};

/// Sentinel returned by JIT trampolines and JIT entries when a JS exception was raised.
pub const JIT_EXCEPTION_SENTINEL: u64 = 0xFFFF_FFFF_FFFF_FFFF;

/// Sentinel returned by zero-flush IC fast-path helpers when a slow-path fallback is needed.
/// Cannot collide with any valid `JsValue` (whose high 16 bits are strictly `<= 0xFFFE`).
pub const JIT_MISS_SENTINEL: u64 = 0xFFFF_FFFE_0000_0000;

/// High-16-bit tag marking a deoptimization return:
/// `0xFFFF_0000_0000_0000 | ((reason_code & 0xFFFF) << 32) | (resume_pc & 0xFFFF_FFFF)`.
pub const JIT_DEOPT_TAG: u64 = 0xFFFF_0000_0000_0000;

/// Speculative guard failure categories encoded in Tier-3 deoptimization return words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum DeoptReason {
    ShapeGuard = 1,
    PrototypeEpoch = 2,
    Int32Type = 3,
    Int32Overflow = 4,
    ArrayGuard = 5,
    ArrayBounds = 6,
    CalleeGuard = 7,
}

impl DeoptReason {
    #[inline]
    #[must_use]
    pub const fn encode_deopt(self, resume_pc: u32) -> u64 {
        JIT_DEOPT_TAG | ((self as u64) << 32) | (resume_pc as u64)
    }

    #[inline]
    #[must_use]
    pub const fn from_raw(raw: u64) -> Option<(Self, usize)> {
        if (raw & 0xFFFF_0000_0000_0000) != JIT_DEOPT_TAG || raw == JIT_EXCEPTION_SENTINEL {
            return None;
        }
        let code = ((raw >> 32) & 0xFFFF) as u16;
        let pc = (raw & 0xFFFF_FFFF) as usize;
        let reason = match code {
            1 => Self::ShapeGuard,
            2 => Self::PrototypeEpoch,
            3 => Self::Int32Type,
            4 => Self::Int32Overflow,
            5 => Self::ArrayGuard,
            6 => Self::ArrayBounds,
            7 => Self::CalleeGuard,
            _ => return None,
        };
        Some((reason, pc))
    }
}

/// Helper storing a thrown `JsException` onto `vm` and returning [`JIT_EXCEPTION_SENTINEL`].
#[inline]
fn record_exc(vm: &mut JsVm, exc: rix_js::JsException, pc: u32) -> u64 {
    vm.jit_set_current_frame_pc(pc as usize + 1);
    vm.pending_jit_exception = Some(exc);
    JIT_EXCEPTION_SENTINEL
}

/// Returns the current live pointer to `vm.registers[base_reg]` after a call/slow-path
/// that may have grown the VM register stack.
///
/// # Safety
/// `vm` must be a valid, exclusively borrowed pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_regs_ptr(vm: *mut JsVm) -> *mut u64 {
    // SAFETY: Caller guarantees `vm` is valid and exclusively owned by the current thread.
    let vm = unsafe { &mut *vm };
    let base = vm.jit_current_frame_base();
    vm.jit_registers_from_mut(base).as_mut_ptr().cast::<u64>()
}

/// Materializes a constant pool entry into a NaN-boxed `JsValue` raw `u64`.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_load_const(
    vm: *mut JsVm,
    prog_id: u32,
    func_idx: u32,
    const_idx: u32,
    pc: u32,
) -> u64 {
    // SAFETY: Valid active `JsVm` pointer passed from JIT prologue.
    let vm = unsafe { &mut *vm };
    match vm.jit_materialize_const(prog_id, func_idx, const_idx as usize) {
        Ok(v) => v.to_raw(),
        Err(e) => record_exc(vm, e, pc),
    }
}

/// Returns `this` of the active top call frame.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_load_this(vm: *mut JsVm) -> u64 {
    // SAFETY: Valid active `JsVm` pointer passed from JIT prologue.
    let vm = unsafe { &*vm };
    vm.jit_current_frame_this().to_raw()
}

/// Slow path for `+` (string concatenation, BigInt, object `ToPrimitive`).
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_add_slow(vm: *mut JsVm, a: u64, b: u64, pc: u32) -> u64 {
    // SAFETY: Valid active `JsVm` pointer passed from JIT prologue.
    let vm = unsafe { &mut *vm };
    match vm.jit_add(JsValue::from_raw(a), JsValue::from_raw(b)) {
        Ok(v) => v.to_raw(),
        Err(e) => record_exc(vm, e, pc),
    }
}

/// Slow path for `-` on non-numeric values.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_sub_slow(vm: *mut JsVm, a: u64, b: u64, pc: u32) -> u64 {
    // SAFETY: Valid active `JsVm` pointer passed from JIT prologue.
    let vm = unsafe { &mut *vm };
    match vm.jit_sub(JsValue::from_raw(a), JsValue::from_raw(b)) {
        Ok(v) => v.to_raw(),
        Err(e) => record_exc(vm, e, pc),
    }
}

/// Slow path for `*` on non-numeric values.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_mul_slow(vm: *mut JsVm, a: u64, b: u64, pc: u32) -> u64 {
    // SAFETY: Valid active `JsVm` pointer passed from JIT prologue.
    let vm = unsafe { &mut *vm };
    match vm.jit_mul(JsValue::from_raw(a), JsValue::from_raw(b)) {
        Ok(v) => v.to_raw(),
        Err(e) => record_exc(vm, e, pc),
    }
}

/// Slow path for `/` on non-numeric values.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_div_slow(vm: *mut JsVm, a: u64, b: u64, pc: u32) -> u64 {
    // SAFETY: Valid active `JsVm` pointer passed from JIT prologue.
    let vm = unsafe { &mut *vm };
    match vm.jit_div(JsValue::from_raw(a), JsValue::from_raw(b)) {
        Ok(v) => v.to_raw(),
        Err(e) => record_exc(vm, e, pc),
    }
}

/// Slow path for `%` on non-numeric values.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_rem_slow(vm: *mut JsVm, a: u64, b: u64, pc: u32) -> u64 {
    // SAFETY: Valid active `JsVm` pointer passed from JIT prologue.
    let vm = unsafe { &mut *vm };
    match vm.jit_rem(JsValue::from_raw(a), JsValue::from_raw(b)) {
        Ok(v) => v.to_raw(),
        Err(e) => record_exc(vm, e, pc),
    }
}

/// Slow path for relational `<`, `<=`, `>`, `>=` (`op`: 0=Lt, 1=Le, 2=Gt, 3=Ge, 4=Eq, 5=StrictEq).
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_cmp_slow(vm: *mut JsVm, op: u32, a: u64, b: u64, pc: u32) -> u64 {
    // SAFETY: Valid active `JsVm` pointer passed from JIT prologue.
    let vm = unsafe { &mut *vm };
    let va = JsValue::from_raw(a);
    let vb = JsValue::from_raw(b);
    let res = match op {
        0 => vm.jit_lt(va, vb),
        1 => vm.jit_le(va, vb),
        2 => vm.jit_gt(va, vb),
        3 => vm.jit_ge(va, vb),
        4 => vm.jit_eq(va, vb),
        5 => Ok(vm.jit_strict_eq(va, vb)),
        _ => Ok(JsValue::FALSE),
    };
    match res {
        Ok(v) => v.to_raw(),
        Err(e) => record_exc(vm, e, pc),
    }
}

/// Evaluates ECMAScript `ToBoolean` for strings/floats/BigInts (returns `1` or `0`).
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_to_bool(vm: *mut JsVm, val: u64) -> u64 {
    // SAFETY: Valid active `JsVm` pointer passed from JIT prologue.
    let vm = unsafe { &*vm };
    u64::from(vm.to_boolean(JsValue::from_raw(val)))
}

/// Direct IC-accelerated property load (`GetProp`).
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_get_prop_ic(
    vm: *mut JsVm,
    obj_bits: u64,
    prog_id: u32,
    func_idx: u32,
    ic_idx: u32,
    pc: u32,
) -> u64 {
    // SAFETY: Valid active `JsVm` pointer passed from JIT prologue.
    let vm = unsafe { &mut *vm };
    match vm.jit_get_prop_ic(
        prog_id,
        func_idx,
        ic_idx as usize,
        JsValue::from_raw(obj_bits),
    ) {
        Ok(v) => v.to_raw(),
        Err(e) => record_exc(vm, e, pc),
    }
}

/// Direct IC-accelerated property store (`SetProp`). Returns `0` on success or `JIT_EXCEPTION_SENTINEL`.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_set_prop_ic(
    vm: *mut JsVm,
    obj_bits: u64,
    val_bits: u64,
    prog_id: u32,
    func_idx: u32,
    ic_idx: u32,
    pc: u32,
) -> u64 {
    // SAFETY: Valid active `JsVm` pointer passed from JIT prologue.
    let vm = unsafe { &mut *vm };
    match vm.jit_set_prop_ic(
        prog_id,
        func_idx,
        ic_idx as usize,
        JsValue::from_raw(obj_bits),
        JsValue::from_raw(val_bits),
    ) {
        Ok(()) => 0,
        Err(e) => record_exc(vm, e, pc),
    }
}

/// Zero-flush monomorphic own-slot `GetProp` fast path.
/// Returns the property value raw `u64`, or [`JIT_MISS_SENTINEL`] if slow path is required.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_get_prop_ic_fast(
    vm: *mut JsVm,
    obj_bits: u64,
    prog_id: u32,
    func_idx: u32,
    ic_idx: u32,
) -> u64 {
    // SAFETY: Valid active `JsVm` pointer; fast path never reallocates `vm.registers` or runs JS.
    let vm = unsafe { &mut *vm };
    match vm.jit_get_prop_ic_fast(
        prog_id,
        func_idx,
        ic_idx as usize,
        JsValue::from_raw(obj_bits),
    ) {
        Some(v) => v.to_raw(),
        None => JIT_MISS_SENTINEL,
    }
}

/// Zero-flush monomorphic in-place `SetProp` fast path.
/// Returns `0` on hit or `1` when slow path is required.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_set_prop_ic_fast(
    vm: *mut JsVm,
    obj_bits: u64,
    val_bits: u64,
    prog_id: u32,
    func_idx: u32,
    ic_idx: u32,
) -> u64 {
    // SAFETY: Valid active `JsVm` pointer; fast path never reallocates `vm.registers` or runs JS.
    let vm = unsafe { &mut *vm };
    if vm.jit_set_prop_ic_fast(
        prog_id,
        func_idx,
        ic_idx as usize,
        JsValue::from_raw(obj_bits),
        JsValue::from_raw(val_bits),
    ) {
        0
    } else {
        1
    }
}

/// Zero-flush dense-array `GetElem` fast path.
/// Returns element raw `u64`, or [`JIT_MISS_SENTINEL`] on miss.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_get_elem_fast(vm: *mut JsVm, obj_bits: u64, key_bits: u64) -> u64 {
    // SAFETY: Valid active `JsVm` pointer; read-only fast path never runs JS or GC.
    let vm = unsafe { &*vm };
    match vm.jit_get_elem_fast(JsValue::from_raw(obj_bits), JsValue::from_raw(key_bits)) {
        Some(v) => v.to_raw(),
        None => JIT_MISS_SENTINEL,
    }
}

/// Zero-flush dense-array `SetElem` fast path.
/// Returns `0` on hit or `1` on miss.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_set_elem_fast(
    vm: *mut JsVm,
    obj_bits: u64,
    key_bits: u64,
    val_bits: u64,
) -> u64 {
    // SAFETY: Valid active `JsVm` pointer; fast path never reallocates `vm.registers` or runs JS.
    let vm = unsafe { &mut *vm };
    if vm.jit_set_elem_fast(
        JsValue::from_raw(obj_bits),
        JsValue::from_raw(key_bits),
        JsValue::from_raw(val_bits),
    ) {
        0
    } else {
        1
    }
}

/// Direct dense-array / object element load (`GetElem`).
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_get_elem(vm: *mut JsVm, obj_bits: u64, key_bits: u64, pc: u32) -> u64 {
    // SAFETY: Valid active `JsVm` pointer passed from JIT prologue.
    let vm = unsafe { &mut *vm };
    match vm.jit_get_elem(JsValue::from_raw(obj_bits), JsValue::from_raw(key_bits)) {
        Ok(v) => v.to_raw(),
        Err(e) => record_exc(vm, e, pc),
    }
}

/// Direct dense-array / object element store (`SetElem`). Returns `0` on success or `JIT_EXCEPTION_SENTINEL`.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_set_elem(
    vm: *mut JsVm,
    obj_bits: u64,
    key_bits: u64,
    val_bits: u64,
    pc: u32,
) -> u64 {
    // SAFETY: Valid active `JsVm` pointer passed from JIT prologue.
    let vm = unsafe { &mut *vm };
    match vm.jit_set_elem(
        JsValue::from_raw(obj_bits),
        JsValue::from_raw(key_bits),
        JsValue::from_raw(val_bits),
    ) {
        Ok(()) => 0,
        Err(e) => record_exc(vm, e, pc),
    }
}

/// Executes any generic non-branch/non-return ES2024 opcode directly on the live frame window.
/// Returns `0` on success or `JIT_EXCEPTION_SENTINEL` on thrown exception.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`], and live SSA registers must have
/// been flushed to `vm.registers[base..]` before invocation.
pub unsafe extern "C" fn rt_step_generic(
    vm: *mut JsVm,
    prog_id: u32,
    func_idx: u32,
    pc: u32,
) -> u64 {
    // SAFETY: Valid active `JsVm` pointer with flushed frame register window.
    let vm = unsafe { &mut *vm };
    match vm.jit_step_generic(prog_id, func_idx, pc as usize) {
        Ok(_) => 0,
        Err(e) => record_exc(vm, e, pc),
    }
}

/// Loop back-edge cooperative interrupt & minor-GC safepoint.
/// Returns `0` to continue or `JIT_EXCEPTION_SENTINEL` when interrupted.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`], and live SSA registers must have
/// been flushed before calling when minor GC may run.
pub unsafe extern "C" fn rt_loop_safepoint(
    vm: *mut JsVm,
    prog_id: u32,
    func_idx: u32,
    pc: u32,
) -> u64 {
    // SAFETY: Valid active `JsVm` pointer passed from JIT loop back-edge.
    let vm = unsafe { &mut *vm };
    match vm.jit_loop_safepoint(prog_id, func_idx) {
        Ok(()) => 0,
        Err(e) => record_exc(vm, e, pc),
    }
}

/// Explicit JS `throw` from a JIT frame.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_throw(vm: *mut JsVm, val_bits: u64, pc: u32) -> u64 {
    // SAFETY: Valid active `JsVm` pointer passed from JIT frame.
    let vm = unsafe { &mut *vm };
    let val = JsValue::from_raw(val_bits);
    let msg = vm.value_to_rust_string(val);
    record_exc(
        vm,
        rix_js::JsException {
            value: val,
            message: msg,
        },
        pc,
    )
}

/// Tier-3 hoisted shape guard: verifies `obj` has `expected_shape` (`!Proxy`, `slots.len() >= min_slots`,
/// and not a prototype when `for_write != 0`), returning a raw pointer to `obj.slots[0]`, or null on fail.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`]. The returned pointer remains valid only
/// while the object stays rooted and no operation reallocates `obj.slots`.
pub unsafe extern "C" fn rt_guard_object_slots(
    vm: *mut JsVm,
    obj_bits: u64,
    expected_shape: u32,
    min_slots: u32,
    for_write: u32,
) -> *mut u64 {
    // SAFETY: Valid active `JsVm` pointer passed from Tier-3 JIT code.
    let vm = unsafe { &mut *vm };
    match vm.jit_guard_object_slots(
        JsValue::from_raw(obj_bits),
        expected_shape,
        min_slots as usize,
        for_write != 0,
    ) {
        Some(slice) => slice.as_mut_ptr().cast::<u64>(),
        None => std::ptr::null_mut(),
    }
}

/// Tier-3 prototype-chain guard: verifies `shapes.prototype_epoch() == expected_epoch` and
/// receiver `shape_id == expected_shape`, returning the holder slot value or [`JIT_MISS_SENTINEL`].
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_guard_proto_prop(
    vm: *mut JsVm,
    obj_bits: u64,
    expected_shape: u32,
    holder_bits: u64,
    offset: u32,
    expected_epoch: u64,
) -> u64 {
    // SAFETY: Valid active `JsVm` pointer passed from Tier-3 JIT code.
    let vm = unsafe { &*vm };
    match vm.jit_guard_proto_prop(
        JsValue::from_raw(obj_bits),
        expected_shape,
        JsValue::from_raw(holder_bits),
        offset as usize,
        expected_epoch,
    ) {
        Some(v) => v.to_raw(),
        None => JIT_MISS_SENTINEL,
    }
}

/// Tier-3 hoisted packed array guard: verifies `obj` is an `Array` with `expected_kind`
/// (`0 = PackedInt32`, `1 = PackedFloat64`, `2 = PackedValues`), writes its length to `*out_len`,
/// and returns its raw data buffer pointer, or null on guard failure.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`]; `out_len` must be a valid writable `*mut u32`.
pub unsafe extern "C" fn rt_guard_packed_array(
    vm: *mut JsVm,
    obj_bits: u64,
    expected_kind: u32,
    out_len: *mut u32,
) -> *mut u8 {
    // SAFETY: Valid active `JsVm` pointer and stack-allocated `out_len` slot passed from Tier-3 JIT.
    let vm = unsafe { &mut *vm };
    let val = JsValue::from_raw(obj_bits);
    let (ptr, len) = match expected_kind {
        0 => match vm.jit_guard_packed_i32(val) {
            Some(s) => (s.as_mut_ptr().cast::<u8>(), s.len() as u32),
            None => return std::ptr::null_mut(),
        },
        1 => match vm.jit_guard_packed_f64(val) {
            Some(s) => (s.as_mut_ptr().cast::<u8>(), s.len() as u32),
            None => return std::ptr::null_mut(),
        },
        2 => match vm.jit_guard_packed_values(val) {
            Some(s) => (s.as_mut_ptr().cast::<u8>(), s.len() as u32),
            None => return std::ptr::null_mut(),
        },
        _ => return std::ptr::null_mut(),
    };
    // SAFETY: Caller passed a valid stack-slot pointer for `out_len`.
    unsafe {
        *out_len = len;
    }
    ptr
}

/// Tier-3 cold generational write barrier called when storing a heap-tagged value into a raw slot.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_write_barrier(vm: *mut JsVm, owner_bits: u64, val_bits: u64) -> u64 {
    // SAFETY: Valid active `JsVm` pointer passed from Tier-3 JIT code.
    let vm = unsafe { &mut *vm };
    vm.jit_write_barrier(JsValue::from_raw(owner_bits), JsValue::from_raw(val_bits));
    0
}

/// Zero-flush monomorphic `GetGlobal` fast path.
/// Returns the global value raw `u64`, or [`JIT_MISS_SENTINEL`] on miss.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`].
pub unsafe extern "C" fn rt_get_global_ic_fast(
    vm: *mut JsVm,
    prog_id: u32,
    func_idx: u32,
    ic_idx: u32,
) -> u64 {
    // SAFETY: Valid active `JsVm` pointer; read-only monomorphic hit never runs JS or GC.
    let vm = unsafe { &mut *vm };
    match vm.jit_get_global_ic_fast(prog_id, func_idx, ic_idx as usize) {
        Some(v) => v.to_raw(),
        None => JIT_MISS_SENTINEL,
    }
}

/// Fast zero-Vec-allocation direct call trampoline for 0..=2 argument `Opcode::Call` sites.
///
/// # Safety
/// `vm` must be a valid pointer to the active [`JsVm`], and live SSA registers must have
/// been flushed before invocation.
pub unsafe extern "C" fn rt_call_fast2(
    vm: *mut JsVm,
    callee_bits: u64,
    a0: u64,
    a1: u64,
    packed_meta: u64,
    pc: u32,
) -> u64 {
    // SAFETY: Valid active `JsVm` pointer with flushed frame registers.
    let vm = unsafe { &mut *vm };
    let prog_id = (packed_meta >> 32) as u32;
    let func_idx = ((packed_meta >> 8) & 0x00FF_FFFF) as u32;
    let argc = (packed_meta & 0xFF) as usize;
    let args = [JsValue::from_raw(a0), JsValue::from_raw(a1)];
    match vm.jit_call_fast(
        prog_id,
        func_idx,
        pc as usize,
        JsValue::from_raw(callee_bits),
        &args[..argc.min(2)],
    ) {
        Ok(v) => v.to_raw(),
        Err(e) => record_exc(vm, e, pc),
    }
}
