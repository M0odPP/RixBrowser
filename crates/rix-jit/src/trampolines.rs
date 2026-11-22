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

/// High-16-bit tag marking a deoptimization return (`0xFFFF_0000_0000_0000 | resume_pc`).
pub const JIT_DEOPT_TAG: u64 = 0xFFFF_0000_0000_0000;

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
