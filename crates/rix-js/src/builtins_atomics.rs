//! ECMAScript 2025 `Atomics` object and unified `ArrayBuffer` / `SharedArrayBuffer`
//! byte-buffer accessors (including resizable/growable buffers, detached checks,
//! and thread-safe FIFO `Atomics.wait` / `Atomics.notify` in 100% pure safe Rust).

use std::sync::Arc;
use std::time::Duration;

use crate::bigint::JsBigInt;
use crate::heap::{
    AtomicWaitResult, DataViewObject, ObjectKind, SharedBufferBacking, TypedArrayKind,
    TypedArrayObject,
};
use crate::shape::PropertyAttrs;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm, PrimHint};
use crate::vm_builtins::{define_global, define_method_len};

#[inline]
fn arg(args: &[JsValue], i: usize) -> JsValue {
    args.get(i).copied().unwrap_or(JsValue::UNDEFINED)
}

/// ECMA-262 §7.1.22 `ToIndex(value)`.
pub fn to_index(vm: &mut JsVm, val: JsValue) -> Result<usize, JsException> {
    if val.is_undefined() {
        return Ok(0);
    }
    let int = vm.to_integer_or_infinity(val)?;
    if int < 0.0 || int > 9_007_199_254_740_991.0 {
        return Err(vm.range_error("Index out of range"));
    }
    Ok(int as usize)
}

/// ECMA-262 §7.1.13 `ToBigInt(argument)` returning low 64 bits (signed two's complement).
pub fn to_bigint_i64(vm: &mut JsVm, val: JsValue) -> Result<i64, JsException> {
    let prim = vm.to_primitive(val, PrimHint::Number)?;
    if let Some(h) = prim.as_bigint() {
        return Ok(vm.heap.get_bigint(h).map_or(0, JsBigInt::to_i64_lossy));
    }
    if let Some(b) = prim.as_bool() {
        return Ok(i64::from(b));
    }
    if let Some(sh) = prim.as_string() {
        let s = vm.heap.materialize_string(sh).unwrap_or_default();
        let bi = JsBigInt::parse_str(&s).ok_or_else(|| {
            vm.make_error("SyntaxError", format!("Cannot convert {s} to a BigInt"))
        })?;
        return Ok(bi.to_i64_lossy());
    }
    Err(vm.type_error("Cannot convert value to a BigInt"))
}

#[inline]
pub fn ab_is_detached(vm: &JsVm, buf_h: HeapHandle) -> bool {
    vm.heap
        .get_object(buf_h)
        .and_then(|o| o.rare.as_ref())
        .is_some_and(|r| r.ab_detached)
}

#[inline]
pub fn ab_is_immutable(vm: &JsVm, buf_h: HeapHandle) -> bool {
    vm.heap
        .get_object(buf_h)
        .and_then(|o| o.rare.as_ref())
        .is_some_and(|r| r.ab_immutable)
}

#[inline]
pub fn ab_is_shared(vm: &JsVm, buf_h: HeapHandle) -> bool {
    vm.heap
        .get_object(buf_h)
        .and_then(|o| o.rare.as_ref())
        .is_some_and(|r| r.shared_mem.is_some())
}

#[inline]
pub fn ab_shared_backing(vm: &JsVm, buf_h: HeapHandle) -> Option<Arc<SharedBufferBacking>> {
    vm.heap
        .get_object(buf_h)
        .and_then(|o| o.rare.as_ref())
        .and_then(|r| r.shared_mem.clone())
}

#[inline]
pub fn ab_max_byte_length(vm: &JsVm, buf_h: HeapHandle) -> Option<usize> {
    vm.heap
        .get_object(buf_h)
        .and_then(|o| o.rare.as_ref())
        .and_then(|r| r.ab_max_byte_length)
}

/// Current byte length of an `ArrayBuffer` or `SharedArrayBuffer` (`0` if detached).
pub fn ab_byte_len(vm: &JsVm, buf_h: HeapHandle) -> usize {
    if ab_is_detached(vm, buf_h) {
        return 0;
    }
    if let Some(shared) = ab_shared_backing(vm, buf_h) {
        return shared.byte_len();
    }
    match vm.heap.get_object(buf_h).map(|o| &o.kind) {
        Some(ObjectKind::ArrayBuffer(b)) => b.len(),
        _ => 0,
    }
}

/// Copies `N` bytes starting at `abs_offset` from an `ArrayBuffer` or `SharedArrayBuffer`.
pub fn ab_read_bytes<const N: usize>(
    vm: &JsVm,
    buf_h: HeapHandle,
    abs_offset: usize,
) -> Option<[u8; N]> {
    if ab_is_detached(vm, buf_h) {
        return None;
    }
    if let Some(shared) = ab_shared_backing(vm, buf_h) {
        let guard = shared.data.lock().ok()?;
        if abs_offset.checked_add(N)? > guard.len() {
            return None;
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&guard[abs_offset..abs_offset + N]);
        return Some(out);
    }
    let ObjectKind::ArrayBuffer(buf) = vm.heap.get_object(buf_h).map(|o| &o.kind)? else {
        return None;
    };
    if abs_offset.checked_add(N)? > buf.len() {
        return None;
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&buf[abs_offset..abs_offset + N]);
    Some(out)
}

/// Writes `N` bytes starting at `abs_offset` into an `ArrayBuffer` or `SharedArrayBuffer`.
pub fn ab_write_bytes<const N: usize>(
    vm: &mut JsVm,
    buf_h: HeapHandle,
    abs_offset: usize,
    bytes: [u8; N],
) -> bool {
    if ab_is_detached(vm, buf_h) || ab_is_immutable(vm, buf_h) {
        return false;
    }
    if let Some(shared) = ab_shared_backing(vm, buf_h) {
        let Ok(mut guard) = shared.data.lock() else {
            return false;
        };
        if abs_offset
            .checked_add(N)
            .is_none_or(|end| end > guard.len())
        {
            return false;
        }
        guard[abs_offset..abs_offset + N].copy_from_slice(&bytes);
        return true;
    }
    let Some(ObjectKind::ArrayBuffer(buf)) = vm.heap.get_object_mut(buf_h).map(|o| &mut o.kind)
    else {
        return false;
    };
    if abs_offset.checked_add(N).is_none_or(|end| end > buf.len()) {
        return false;
    }
    buf[abs_offset..abs_offset + N].copy_from_slice(&bytes);
    true
}

/// Copies a dynamic slice `[start..end]` from an `ArrayBuffer` or `SharedArrayBuffer`.
pub fn ab_copy_slice(vm: &JsVm, buf_h: HeapHandle, start: usize, end: usize) -> Vec<u8> {
    if ab_is_detached(vm, buf_h) || end <= start {
        return Vec::new();
    }
    if let Some(shared) = ab_shared_backing(vm, buf_h) {
        return shared.data.lock().map_or_else(
            |_| Vec::new(),
            |g| g[start.min(g.len())..end.min(g.len())].to_vec(),
        );
    }
    match vm.heap.get_object(buf_h).map(|o| &o.kind) {
        Some(ObjectKind::ArrayBuffer(b)) => b[start.min(b.len())..end.min(b.len())].to_vec(),
        _ => Vec::new(),
    }
}

/// Effective element count of a `TypedArray`, or `None` if buffer is detached or out-of-bounds.
pub fn ta_effective_len(vm: &JsVm, ta_h: HeapHandle, ta: TypedArrayObject) -> Option<u32> {
    if ab_is_detached(vm, ta.buffer) {
        return None;
    }
    let buf_len = ab_byte_len(vm, ta.buffer);
    let off = ta.byte_offset as usize;
    let bpe = ta.kind.bytes_per_element();
    let auto = vm
        .heap
        .get_object(ta_h)
        .and_then(|o| o.rare.as_ref())
        .is_some_and(|r| r.view_auto_length);
    if auto {
        if off > buf_len {
            return None;
        }
        Some(((buf_len - off) / bpe) as u32)
    } else {
        let end = off.checked_add((ta.length as usize).checked_mul(bpe)?)?;
        if end > buf_len {
            None
        } else {
            Some(ta.length)
        }
    }
}

/// Effective byte length of a `DataView`, or `None` if buffer is detached or out-of-bounds.
pub fn dv_effective_byte_len(vm: &JsVm, dv_h: HeapHandle, dv: DataViewObject) -> Option<u32> {
    if ab_is_detached(vm, dv.buffer) {
        return None;
    }
    let buf_len = ab_byte_len(vm, dv.buffer);
    let off = dv.byte_offset as usize;
    let auto = vm
        .heap
        .get_object(dv_h)
        .and_then(|o| o.rare.as_ref())
        .is_some_and(|r| r.view_auto_length);
    if auto {
        if off > buf_len {
            None
        } else {
            Some((buf_len - off) as u32)
        }
    } else {
        let end = off.checked_add(dv.byte_length as usize)?;
        if end > buf_len {
            None
        } else {
            Some(dv.byte_length)
        }
    }
}

// ---------------------------------------------------------------------------
// Atomics (ECMA-262 §25.4)
// ---------------------------------------------------------------------------

fn is_atomic_integer_kind(kind: TypedArrayKind) -> bool {
    matches!(
        kind,
        TypedArrayKind::Int8
            | TypedArrayKind::Uint8
            | TypedArrayKind::Int16
            | TypedArrayKind::Uint16
            | TypedArrayKind::Int32
            | TypedArrayKind::Uint32
            | TypedArrayKind::BigInt64
            | TypedArrayKind::BigUint64
    )
}

/// ECMA-262 §25.4.3.1 `ValidateIntegerTypedArray(typedArray, waitable)`.
fn validate_integer_typed_array(
    vm: &mut JsVm,
    val: JsValue,
    waitable: bool,
) -> Result<(HeapHandle, TypedArrayObject, u32), JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Atomics target must be an integer TypedArray"));
    };
    let ta = match vm.heap.get_object(h).map(|o| &o.kind) {
        Some(ObjectKind::TypedArray(t)) => *t,
        _ => return Err(vm.type_error("Atomics target must be an integer TypedArray")),
    };
    if waitable {
        if !matches!(ta.kind, TypedArrayKind::Int32 | TypedArrayKind::BigInt64) {
            return Err(vm.type_error("Atomics wait/notify requires Int32Array or BigInt64Array"));
        }
        if !ab_is_shared(vm, ta.buffer) {
            return Err(
                vm.type_error("Atomics wait/notify requires a SharedArrayBuffer-backed TypedArray")
            );
        }
    } else if !is_atomic_integer_kind(ta.kind) {
        return Err(vm.type_error("Atomics target must be an integer TypedArray"));
    }
    let Some(len) = ta_effective_len(vm, h, ta) else {
        return Err(vm.type_error("Atomics TypedArray buffer is detached or out of bounds"));
    };
    Ok((h, ta, len))
}

/// ECMA-262 §25.4.3.2 `ValidateAtomicAccess(taRecord, requestIndex)`.
fn validate_atomic_access(
    vm: &mut JsVm,
    ta: TypedArrayObject,
    len: u32,
    index_val: JsValue,
) -> Result<(u32, usize), JsException> {
    let idx = to_index(vm, index_val)?;
    if idx >= len as usize {
        return Err(vm.range_error("Atomics index out of bounds"));
    }
    let byte_pos = ta.byte_offset as usize + idx * ta.kind.bytes_per_element();
    Ok((idx as u32, byte_pos))
}

fn coerce_atomic_operand(
    vm: &mut JsVm,
    kind: TypedArrayKind,
    val: JsValue,
) -> Result<i64, JsException> {
    if kind.is_bigint() {
        to_bigint_i64(vm, val)
    } else {
        let n = vm.to_integer_or_infinity(val)?;
        if !n.is_finite() || n == 0.0 {
            Ok(0)
        } else {
            Ok((n.rem_euclid(4_294_967_296.0) as u32) as i32 as i64)
        }
    }
}

fn bytes_to_raw_i64(kind: TypedArrayKind, b: [u8; 8]) -> i64 {
    match kind {
        TypedArrayKind::Int8 => i64::from(b[0] as i8),
        TypedArrayKind::Uint8 => i64::from(b[0]),
        TypedArrayKind::Int16 => i64::from(i16::from_ne_bytes([b[0], b[1]])),
        TypedArrayKind::Uint16 => i64::from(u16::from_ne_bytes([b[0], b[1]])),
        TypedArrayKind::Int32 => i64::from(i32::from_ne_bytes([b[0], b[1], b[2], b[3]])),
        TypedArrayKind::Uint32 => i64::from(u32::from_ne_bytes([b[0], b[1], b[2], b[3]])),
        TypedArrayKind::BigInt64 => i64::from_ne_bytes(b),
        TypedArrayKind::BigUint64 => u64::from_ne_bytes(b) as i64,
        _ => 0,
    }
}

fn raw_i64_to_bytes(kind: TypedArrayKind, v: i64) -> ([u8; 8], usize) {
    let mut out = [0u8; 8];
    match kind {
        TypedArrayKind::Int8 | TypedArrayKind::Uint8 => {
            out[0] = v as u8;
            (out, 1)
        }
        TypedArrayKind::Int16 | TypedArrayKind::Uint16 => {
            out[..2].copy_from_slice(&(v as u16).to_ne_bytes());
            (out, 2)
        }
        TypedArrayKind::Int32 | TypedArrayKind::Uint32 => {
            out[..4].copy_from_slice(&(v as u32).to_ne_bytes());
            (out, 4)
        }
        TypedArrayKind::BigInt64 | TypedArrayKind::BigUint64 => {
            out.copy_from_slice(&v.to_ne_bytes());
            (out, 8)
        }
        _ => (out, 0),
    }
}

fn raw_i64_to_js(vm: &mut JsVm, kind: TypedArrayKind, v: i64) -> JsValue {
    match kind {
        TypedArrayKind::Int8 => JsValue::from_i32(i32::from(v as i8)),
        TypedArrayKind::Uint8 => JsValue::from_i32(i32::from(v as u8)),
        TypedArrayKind::Int16 => JsValue::from_i32(i32::from(v as i16)),
        TypedArrayKind::Uint16 => JsValue::from_i32(i32::from(v as u16)),
        TypedArrayKind::Int32 => JsValue::from_i32(v as i32),
        TypedArrayKind::Uint32 => JsValue::from_f64(f64::from(v as u32)),
        TypedArrayKind::BigInt64 => {
            JsValue::from_bigint(vm.heap.alloc_bigint(JsBigInt::from_i64(v)))
        }
        TypedArrayKind::BigUint64 => JsValue::from_bigint(
            vm.heap
                .alloc_bigint(JsBigInt::from_i128(i128::from(v as u64))),
        ),
        _ => JsValue::UNDEFINED,
    }
}

fn atomic_rmw_bytes(
    vm: &mut JsVm,
    ta_h: HeapHandle,
    ta: TypedArrayObject,
    idx: u32,
    byte_pos: usize,
    is_write: bool,
    op: impl FnOnce(i64) -> i64,
) -> Result<i64, JsException> {
    // Re-validate buffer after user-visible coercions (could have detached/shrunk!).
    let Some(cur_len) = ta_effective_len(vm, ta_h, ta) else {
        return Err(vm.type_error("Atomics TypedArray buffer was detached or shrunk"));
    };
    if idx >= cur_len {
        return Err(vm.type_error("Atomics TypedArray buffer was shrunk out of bounds"));
    }
    if is_write && ab_is_immutable(vm, ta.buffer) {
        return Err(vm.type_error("Cannot perform atomic write on immutable ArrayBuffer"));
    }
    let bpe = ta.kind.bytes_per_element();
    let apply_slice = |slice: &mut [u8]| -> Option<i64> {
        if byte_pos + bpe > slice.len() {
            return None;
        }
        let mut raw = [0u8; 8];
        raw[..bpe].copy_from_slice(&slice[byte_pos..byte_pos + bpe]);
        let old = bytes_to_raw_i64(ta.kind, raw);
        if is_write {
            let new_val = op(old);
            let (nb, n) = raw_i64_to_bytes(ta.kind, new_val);
            slice[byte_pos..byte_pos + n].copy_from_slice(&nb[..n]);
        }
        Some(old)
    };
    let old = if let Some(shared) = ab_shared_backing(vm, ta.buffer) {
        let mut guard = shared.data.lock().unwrap_or_else(|e| e.into_inner());
        apply_slice(&mut guard)
    } else if let Some(ObjectKind::ArrayBuffer(buf)) =
        vm.heap.get_object_mut(ta.buffer).map(|o| &mut o.kind)
    {
        apply_slice(buf)
    } else {
        None
    };
    old.ok_or_else(|| vm.type_error("Atomics TypedArray buffer is detached or out of bounds"))
}

#[derive(Clone, Copy)]
enum AtomicBinOp {
    Add,
    Sub,
    And,
    Or,
    Xor,
    Exchange,
}

fn atomics_rmw(vm: &mut JsVm, args: &[JsValue], bin: AtomicBinOp) -> Result<JsValue, JsException> {
    let (ta_h, ta, len) = validate_integer_typed_array(vm, arg(args, 0), false)?;
    let (idx, byte_pos) = validate_atomic_access(vm, ta, len, arg(args, 1))?;
    let v = coerce_atomic_operand(vm, ta.kind, arg(args, 2))?;
    let old = atomic_rmw_bytes(vm, ta_h, ta, idx, byte_pos, true, |cur| match bin {
        AtomicBinOp::Add => cur.wrapping_add(v),
        AtomicBinOp::Sub => cur.wrapping_sub(v),
        AtomicBinOp::And => cur & v,
        AtomicBinOp::Or => cur | v,
        AtomicBinOp::Xor => cur ^ v,
        AtomicBinOp::Exchange => v,
    })?;
    Ok(raw_i64_to_js(vm, ta.kind, old))
}

fn atomics_load(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let (ta_h, ta, len) = validate_integer_typed_array(vm, arg(args, 0), false)?;
    let (idx, byte_pos) = validate_atomic_access(vm, ta, len, arg(args, 1))?;
    let old = atomic_rmw_bytes(vm, ta_h, ta, idx, byte_pos, false, |cur| cur)?;
    Ok(raw_i64_to_js(vm, ta.kind, old))
}

fn atomics_store(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let (ta_h, ta, len) = validate_integer_typed_array(vm, arg(args, 0), false)?;
    let (idx, byte_pos) = validate_atomic_access(vm, ta, len, arg(args, 1))?;
    let (v, ret_val) = if ta.kind.is_bigint() {
        let prim = vm.to_primitive(arg(args, 2), PrimHint::Number)?;
        let bi = if let Some(h) = prim.as_bigint() {
            vm.heap.get_bigint(h).cloned().unwrap_or_default()
        } else if let Some(b) = prim.as_bool() {
            JsBigInt::from_i64(i64::from(b))
        } else if let Some(sh) = prim.as_string() {
            let s = vm.heap.materialize_string(sh).unwrap_or_default();
            JsBigInt::parse_str(&s).ok_or_else(|| {
                vm.make_error("SyntaxError", format!("Cannot convert {s} to a BigInt"))
            })?
        } else {
            return Err(vm.type_error("Cannot convert value to a BigInt"));
        };
        let raw = bi.to_i64_lossy();
        let ret = JsValue::from_bigint(vm.heap.alloc_bigint(bi));
        (raw, ret)
    } else {
        let n = vm.to_integer_or_infinity(arg(args, 2))?;
        let raw = if !n.is_finite() || n == 0.0 {
            0
        } else {
            (n.rem_euclid(4_294_967_296.0) as u32) as i32 as i64
        };
        let ret = if n == 0.0 {
            JsValue::from_i32(0)
        } else {
            JsValue::from_f64(n)
        };
        (raw, ret)
    };
    let _ = atomic_rmw_bytes(vm, ta_h, ta, idx, byte_pos, true, |_| v)?;
    Ok(ret_val)
}

fn atomics_compare_exchange(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let (ta_h, ta, len) = validate_integer_typed_array(vm, arg(args, 0), false)?;
    let (idx, byte_pos) = validate_atomic_access(vm, ta, len, arg(args, 1))?;
    let expected = coerce_atomic_operand(vm, ta.kind, arg(args, 2))?;
    let replacement = coerce_atomic_operand(vm, ta.kind, arg(args, 3))?;
    let (exp_b, exp_n) = raw_i64_to_bytes(ta.kind, expected);
    let expected_norm = bytes_to_raw_i64(ta.kind, exp_b);
    let _ = exp_n;
    let old = atomic_rmw_bytes(vm, ta_h, ta, idx, byte_pos, true, |cur| {
        if cur == expected_norm {
            replacement
        } else {
            cur
        }
    })?;
    Ok(raw_i64_to_js(vm, ta.kind, old))
}

fn atomics_is_lock_free(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let n = vm.to_integer_or_infinity(arg(args, 0))?;
    Ok(JsValue::from_bool(
        matches!(n as i64, 1 | 2 | 4 | 8) && n.fract() == 0.0,
    ))
}

fn parse_wait_timeout(vm: &mut JsVm, val: JsValue) -> Result<Option<Duration>, JsException> {
    let q = vm.to_number(val)?;
    if q.is_nan() || q == f64::INFINITY {
        Ok(None)
    } else if q <= 0.0 {
        Ok(Some(Duration::ZERO))
    } else {
        let ms = q.min(8.64e12);
        Ok(Some(Duration::from_secs_f64(ms / 1000.0)))
    }
}

fn atomics_wait(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let (_ta_h, ta, len) = validate_integer_typed_array(vm, arg(args, 0), true)?;
    let (_idx, byte_pos) = validate_atomic_access(vm, ta, len, arg(args, 1))?;
    let expected = if ta.kind == TypedArrayKind::BigInt64 {
        to_bigint_i64(vm, arg(args, 2))?
    } else {
        i64::from(vm.to_i32(arg(args, 2))?)
    };
    let timeout = parse_wait_timeout(vm, arg(args, 3))?;
    let Some(shared) = ab_shared_backing(vm, ta.buffer) else {
        return Err(vm.type_error("Atomics.wait requires a SharedArrayBuffer"));
    };
    if ab_is_immutable(vm, ta.buffer) {
        return Err(vm.type_error("Cannot perform atomic write on immutable ArrayBuffer"));
    }
    let bpe = ta.kind.bytes_per_element();
    let res = shared.wait_at(
        byte_pos,
        |data| {
            if byte_pos + bpe > data.len() {
                return false;
            }
            let mut raw = [0u8; 8];
            raw[..bpe].copy_from_slice(&data[byte_pos..byte_pos + bpe]);
            bytes_to_raw_i64(ta.kind, raw) == expected
        },
        timeout,
    );
    let label = match res {
        AtomicWaitResult::Ok => "ok",
        AtomicWaitResult::NotEqual => "not-equal",
        AtomicWaitResult::TimedOut => "timed-out",
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(label)))
}

fn atomics_wait_async(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let (_ta_h, ta, len) = validate_integer_typed_array(vm, arg(args, 0), true)?;
    let (_idx, byte_pos) = validate_atomic_access(vm, ta, len, arg(args, 1))?;
    let expected = if ta.kind == TypedArrayKind::BigInt64 {
        to_bigint_i64(vm, arg(args, 2))?
    } else {
        i64::from(vm.to_i32(arg(args, 2))?)
    };
    let timeout = parse_wait_timeout(vm, arg(args, 3))?;
    let Some(shared) = ab_shared_backing(vm, ta.buffer) else {
        return Err(vm.type_error("Atomics.waitAsync requires a SharedArrayBuffer"));
    };
    if ab_is_immutable(vm, ta.buffer) {
        return Err(vm.type_error("Cannot perform atomic write on immutable ArrayBuffer"));
    }
    let bpe = ta.kind.bytes_per_element();
    let equal = {
        let data = shared.data.lock().unwrap_or_else(|e| e.into_inner());
        byte_pos + bpe <= data.len() && {
            let mut raw = [0u8; 8];
            raw[..bpe].copy_from_slice(&data[byte_pos..byte_pos + bpe]);
            bytes_to_raw_i64(ta.kind, raw) == expected
        }
    };
    let out = vm.alloc_ordinary_object();
    let k_async = vm.heap.intern_key("async");
    if !equal || timeout == Some(Duration::ZERO) {
        let label = if !equal { "not-equal" } else { "timed-out" };
        vm.heap.object_define_data(
            out,
            k_async,
            JsValue::FALSE,
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        let lv = JsValue::from_string(vm.heap.alloc_string(label));
        vm.heap.object_define_data(
            out,
            vm.key_value,
            lv,
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        return Ok(JsValue::from_object(out));
    }
    let promise = crate::promise::alloc_promise(vm);
    let ptr_id = Arc::as_ptr(&shared) as usize;
    vm.atomic_async_waiters.push((ptr_id, byte_pos, promise));
    vm.heap.object_define_data(
        out,
        k_async,
        JsValue::TRUE,
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        out,
        vm.key_value,
        JsValue::from_object(promise),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    Ok(JsValue::from_object(out))
}

fn atomics_notify(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let (_ta_h, ta, len) = validate_integer_typed_array(vm, arg(args, 0), false)?;
    if !matches!(ta.kind, TypedArrayKind::Int32 | TypedArrayKind::BigInt64) {
        return Err(vm.type_error("Atomics.notify requires Int32Array or BigInt64Array"));
    }
    let (_idx, byte_pos) = validate_atomic_access(vm, ta, len, arg(args, 1))?;
    let count_arg = arg(args, 2);
    let count = if count_arg.is_undefined() {
        usize::MAX
    } else {
        let c = vm.to_integer_or_infinity(count_arg)?;
        if c <= 0.0 {
            0
        } else if c.is_infinite() {
            usize::MAX
        } else {
            c.min(usize::MAX as f64) as usize
        }
    };
    let mut woken = 0usize;
    if let Some(shared) = ab_shared_backing(vm, ta.buffer) {
        woken += shared.notify_at(byte_pos, count);
        let rem = count.saturating_sub(woken);
        if rem > 0 {
            let ptr_id = Arc::as_ptr(&shared) as usize;
            let mut resolved = Vec::new();
            let mut idx = 0usize;
            while idx < vm.atomic_async_waiters.len() && resolved.len() < rem {
                let (pid, off, prom) = vm.atomic_async_waiters[idx];
                if pid == ptr_id && off == byte_pos {
                    vm.atomic_async_waiters.remove(idx);
                    resolved.push(prom);
                } else {
                    idx += 1;
                }
            }
            woken += resolved.len();
            for prom in resolved {
                let ok_v = JsValue::from_string(vm.heap.alloc_string("ok"));
                crate::promise::resolve_promise(vm, prom, ok_v);
            }
        }
    }
    Ok(JsValue::from_f64(woken as f64))
}

/// ES2025 `Atomics.pause([N])`.
fn atomics_pause(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let n_arg = arg(args, 0);
    if !n_arg.is_undefined() {
        let Some(n) = n_arg.as_number() else {
            return Err(
                vm.type_error("Atomics.pause argument must be an integral Number or undefined")
            );
        };
        if !n.is_finite() || n.fract() != 0.0 {
            return Err(vm.type_error("Atomics.pause argument must be an integral Number"));
        }
        let iters = (n.clamp(0.0, 64.0)) as u32;
        for _ in 0..iters {
            std::hint::spin_loop();
        }
    } else {
        std::hint::spin_loop();
    }
    Ok(JsValue::UNDEFINED)
}

/// Installs the global `Atomics` namespace object (ECMA-262 §25.4).
pub fn install_atomics(vm: &mut JsVm) {
    let atomics = vm.alloc_ordinary_object();
    define_method_len(vm, atomics, "load", 2, atomics_load);
    define_method_len(vm, atomics, "store", 3, atomics_store);
    define_method_len(vm, atomics, "add", 3, |vm, _, a| {
        atomics_rmw(vm, a, AtomicBinOp::Add)
    });
    define_method_len(vm, atomics, "sub", 3, |vm, _, a| {
        atomics_rmw(vm, a, AtomicBinOp::Sub)
    });
    define_method_len(vm, atomics, "and", 3, |vm, _, a| {
        atomics_rmw(vm, a, AtomicBinOp::And)
    });
    define_method_len(vm, atomics, "or", 3, |vm, _, a| {
        atomics_rmw(vm, a, AtomicBinOp::Or)
    });
    define_method_len(vm, atomics, "xor", 3, |vm, _, a| {
        atomics_rmw(vm, a, AtomicBinOp::Xor)
    });
    define_method_len(vm, atomics, "exchange", 3, |vm, _, a| {
        atomics_rmw(vm, a, AtomicBinOp::Exchange)
    });
    define_method_len(vm, atomics, "compareExchange", 4, atomics_compare_exchange);
    define_method_len(vm, atomics, "isLockFree", 1, atomics_is_lock_free);
    define_method_len(vm, atomics, "wait", 4, atomics_wait);
    define_method_len(vm, atomics, "waitAsync", 4, atomics_wait_async);
    define_method_len(vm, atomics, "notify", 3, atomics_notify);
    define_method_len(vm, atomics, "pause", 0, atomics_pause);
    let tag_v = JsValue::from_string(vm.heap.alloc_string("Atomics"));
    let k_tag = vm.key_symbol_to_string_tag;
    vm.heap.object_define_data(
        atomics,
        k_tag,
        tag_v,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    define_global(vm, "Atomics", JsValue::from_object(atomics));
}
