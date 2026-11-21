//! 64-bit NaN-boxed [`JsValue`] backed by safe 48-bit generational [`HeapHandle`]s.
//!
//! Tag layout (Spec 05 §2.1):
//! - High 16 bits `< 0xFFF8`: IEEE-754 `f64` (all NaNs canonicalized to `0x7FF8_0000_0000_0000`)
//! - `0xFFF9`: `i32` fast signed integer (low 32 bits)
//! - `0xFFFA`: Special constants (`0` = `undefined`, `1` = `null`, `2` = `false`, `3` = `true`, `4` = `empty`)
//! - `0xFFFB`: `JsString` generational 48-bit [`HeapHandle`]
//! - `0xFFFC`: `JsObject` generational 48-bit [`HeapHandle`]
//! - `0xFFFD`: `Symbol` (`SymbolId(u32)` in low 32 bits)
//! - `0xFFFE`: `BigInt` generational 48-bit [`HeapHandle`]

use std::fmt;

use serde::{Deserialize, Serialize};

const TAG_MASK: u64 = 0xFFFF_0000_0000_0000;
const PAYLOAD_48_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
const PAYLOAD_32_MASK: u64 = 0x0000_0000_FFFF_FFFF;

/// Canonical positive quiet NaN (`high 16 = 0x7FF8 < 0xFFF8`).
pub const CANONICAL_NAN_BITS: u64 = 0x7FF8_0000_0000_0000;

const TAG_THRESHOLD: u64 = 0xFFF8_0000_0000_0000;
const TAG_INT32: u64 = 0xFFF9_0000_0000_0000;
const TAG_SPECIAL: u64 = 0xFFFA_0000_0000_0000;
const TAG_STRING: u64 = 0xFFFB_0000_0000_0000;
const TAG_OBJECT: u64 = 0xFFFC_0000_0000_0000;
const TAG_SYMBOL: u64 = 0xFFFD_0000_0000_0000;
const TAG_BIGINT: u64 = 0xFFFE_0000_0000_0000;

const SPECIAL_UNDEFINED: u64 = 0;
const SPECIAL_NULL: u64 = 1;
const SPECIAL_FALSE: u64 = 2;
const SPECIAL_TRUE: u64 = 3;
const SPECIAL_EMPTY: u64 = 4;

/// 48-bit generational handle into [`crate::heap::GcHeap`] (`32-bit index + 16-bit generation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HeapHandle {
    pub index: u32,
    pub generation: u16,
}

impl HeapHandle {
    #[inline]
    #[must_use]
    pub const fn new(index: u32, generation: u16) -> Self {
        Self { index, generation }
    }

    #[inline]
    #[must_use]
    pub const fn to_u48(self) -> u64 {
        ((self.generation as u64) << 32) | (self.index as u64)
    }

    #[inline]
    #[must_use]
    pub const fn from_u48(raw: u64) -> Self {
        let index = (raw & PAYLOAD_32_MASK) as u32;
        let generation = ((raw >> 32) & 0xFFFF) as u16;
        Self { index, generation }
    }
}

/// Dense identifier for an ECMAScript `Symbol`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolId(pub u32);

/// High-level discriminant for inspecting a [`JsValue`] in Rust match arms.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JsValueKind {
    Float(f64),
    Int32(i32),
    Undefined,
    Null,
    Bool(bool),
    Empty,
    String(HeapHandle),
    Object(HeapHandle),
    Symbol(SymbolId),
    BigInt(HeapHandle),
}

/// 8-byte transparent NaN-boxed ECMAScript value.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JsValue(u64);

const _: () = assert!(std::mem::size_of::<JsValue>() == 8);

impl Default for JsValue {
    #[inline]
    fn default() -> Self {
        Self::UNDEFINED
    }
}

impl JsValue {
    pub const UNDEFINED: Self = Self(TAG_SPECIAL | SPECIAL_UNDEFINED);
    pub const NULL: Self = Self(TAG_SPECIAL | SPECIAL_NULL);
    pub const FALSE: Self = Self(TAG_SPECIAL | SPECIAL_FALSE);
    pub const TRUE: Self = Self(TAG_SPECIAL | SPECIAL_TRUE);
    pub const EMPTY: Self = Self(TAG_SPECIAL | SPECIAL_EMPTY);
    pub const NAN: Self = Self(CANONICAL_NAN_BITS);

    #[inline]
    #[must_use]
    pub const fn undefined() -> Self {
        Self::UNDEFINED
    }

    #[inline]
    #[must_use]
    pub const fn null() -> Self {
        Self::NULL
    }

    #[inline]
    #[must_use]
    pub const fn empty() -> Self {
        Self::EMPTY
    }

    #[inline]
    #[must_use]
    pub const fn from_bool(b: bool) -> Self {
        if b {
            Self::TRUE
        } else {
            Self::FALSE
        }
    }

    #[inline]
    #[must_use]
    pub const fn from_i32(n: i32) -> Self {
        Self(TAG_INT32 | (n as u32 as u64))
    }

    /// Encodes any IEEE-754 `f64`, canonicalizing all NaN bit patterns to positive quiet NaN
    /// so negative NaNs can never collide with tagged pointer/special space (`>= 0xFFF8`).
    #[inline]
    #[must_use]
    pub fn from_f64(n: f64) -> Self {
        if n.is_nan() {
            return Self::NAN;
        }
        let bits = n.to_bits();
        debug_assert!(bits < TAG_THRESHOLD);
        Self(bits)
    }

    /// Encodes a numeric value using fast `Int32` when representable without loss
    /// (preserving `-0.0` strictly as IEEE-754 `f64`).
    #[inline]
    #[must_use]
    pub fn from_number(n: f64) -> Self {
        if n.is_nan() {
            return Self::NAN;
        }
        if n == 0.0 && n.is_sign_negative() {
            return Self::from_f64(n);
        }
        let i = n as i32;
        if f64::from(i) == n {
            Self::from_i32(i)
        } else {
            Self::from_f64(n)
        }
    }

    #[inline]
    #[must_use]
    pub const fn from_string(handle: HeapHandle) -> Self {
        Self(TAG_STRING | handle.to_u48())
    }

    #[inline]
    #[must_use]
    pub const fn from_object(handle: HeapHandle) -> Self {
        Self(TAG_OBJECT | handle.to_u48())
    }

    #[inline]
    #[must_use]
    pub const fn from_symbol(sym: SymbolId) -> Self {
        Self(TAG_SYMBOL | (sym.0 as u64))
    }

    #[inline]
    #[must_use]
    pub const fn from_bigint(handle: HeapHandle) -> Self {
        Self(TAG_BIGINT | handle.to_u48())
    }

    /// Raw 64-bit representation for interpreter/JIT ABI boundary.
    #[inline]
    #[must_use]
    pub const fn to_raw(self) -> u64 {
        self.0
    }

    /// Reconstructs a `JsValue` from raw NaN-boxed 64-bit ABI bits.
    #[inline]
    #[must_use]
    pub const fn from_raw(bits: u64) -> Self {
        Self(bits)
    }

    #[inline]
    #[must_use]
    pub const fn is_f64(self) -> bool {
        self.0 < TAG_THRESHOLD
    }

    #[inline]
    #[must_use]
    pub const fn is_i32(self) -> bool {
        (self.0 & TAG_MASK) == TAG_INT32
    }

    #[inline]
    #[must_use]
    pub const fn is_number(self) -> bool {
        self.is_f64() || self.is_i32()
    }

    #[inline]
    #[must_use]
    pub const fn is_undefined(self) -> bool {
        self.0 == Self::UNDEFINED.0
    }

    #[inline]
    #[must_use]
    pub const fn is_null(self) -> bool {
        self.0 == Self::NULL.0
    }

    #[inline]
    #[must_use]
    pub const fn is_nullish(self) -> bool {
        self.is_undefined() || self.is_null()
    }

    #[inline]
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == Self::EMPTY.0
    }

    #[inline]
    #[must_use]
    pub const fn is_bool(self) -> bool {
        self.0 == Self::TRUE.0 || self.0 == Self::FALSE.0
    }

    #[inline]
    #[must_use]
    pub const fn is_string(self) -> bool {
        (self.0 & TAG_MASK) == TAG_STRING
    }

    #[inline]
    #[must_use]
    pub const fn is_object(self) -> bool {
        (self.0 & TAG_MASK) == TAG_OBJECT
    }

    #[inline]
    #[must_use]
    pub const fn is_symbol(self) -> bool {
        (self.0 & TAG_MASK) == TAG_SYMBOL
    }

    #[inline]
    #[must_use]
    pub const fn is_bigint(self) -> bool {
        (self.0 & TAG_MASK) == TAG_BIGINT
    }

    #[inline]
    #[must_use]
    pub const fn is_heap_ref(self) -> bool {
        self.is_string() || self.is_object() || self.is_bigint()
    }

    #[inline]
    #[must_use]
    pub fn as_f64(self) -> Option<f64> {
        if self.is_f64() {
            Some(f64::from_bits(self.0))
        } else {
            None
        }
    }

    #[inline]
    #[must_use]
    pub const fn as_i32(self) -> Option<i32> {
        if self.is_i32() {
            Some((self.0 & PAYLOAD_32_MASK) as u32 as i32)
        } else {
            None
        }
    }

    /// Converts either `Int32` or `f64` to `f64`.
    #[inline]
    #[must_use]
    pub fn to_number_f64(self) -> Option<f64> {
        if let Some(i) = self.as_i32() {
            Some(f64::from(i))
        } else {
            self.as_f64()
        }
    }

    #[inline]
    #[must_use]
    pub const fn as_bool(self) -> Option<bool> {
        if self.0 == Self::TRUE.0 {
            Some(true)
        } else if self.0 == Self::FALSE.0 {
            Some(false)
        } else {
            None
        }
    }

    #[inline]
    #[must_use]
    pub const fn as_heap_handle(self) -> Option<HeapHandle> {
        if self.is_heap_ref() {
            Some(HeapHandle::from_u48(self.0 & PAYLOAD_48_MASK))
        } else {
            None
        }
    }

    #[inline]
    #[must_use]
    pub const fn as_object_handle(self) -> Option<HeapHandle> {
        if self.is_object() {
            Some(HeapHandle::from_u48(self.0 & PAYLOAD_48_MASK))
        } else {
            None
        }
    }

    #[inline]
    #[must_use]
    pub const fn as_string_handle(self) -> Option<HeapHandle> {
        if self.is_string() {
            Some(HeapHandle::from_u48(self.0 & PAYLOAD_48_MASK))
        } else {
            None
        }
    }

    #[inline]
    #[must_use]
    pub const fn as_bigint_handle(self) -> Option<HeapHandle> {
        if self.is_bigint() {
            Some(HeapHandle::from_u48(self.0 & PAYLOAD_48_MASK))
        } else {
            None
        }
    }

    #[inline]
    #[must_use]
    pub const fn as_symbol(self) -> Option<SymbolId> {
        if self.is_symbol() {
            Some(SymbolId((self.0 & PAYLOAD_32_MASK) as u32))
        } else {
            None
        }
    }

    #[inline]
    #[must_use]
    pub fn kind(self) -> JsValueKind {
        if self.is_f64() {
            return JsValueKind::Float(f64::from_bits(self.0));
        }
        match self.0 & TAG_MASK {
            TAG_INT32 => JsValueKind::Int32((self.0 & PAYLOAD_32_MASK) as u32 as i32),
            TAG_SPECIAL => match self.0 & PAYLOAD_48_MASK {
                SPECIAL_UNDEFINED => JsValueKind::Undefined,
                SPECIAL_NULL => JsValueKind::Null,
                SPECIAL_FALSE => JsValueKind::Bool(false),
                SPECIAL_TRUE => JsValueKind::Bool(true),
                _ => JsValueKind::Empty,
            },
            TAG_STRING => JsValueKind::String(HeapHandle::from_u48(self.0 & PAYLOAD_48_MASK)),
            TAG_OBJECT => JsValueKind::Object(HeapHandle::from_u48(self.0 & PAYLOAD_48_MASK)),
            TAG_SYMBOL => JsValueKind::Symbol(SymbolId((self.0 & PAYLOAD_32_MASK) as u32)),
            TAG_BIGINT => JsValueKind::BigInt(HeapHandle::from_u48(self.0 & PAYLOAD_48_MASK)),
            _ => JsValueKind::Undefined,
        }
    }

    /// Checked fast `Int32` addition overflowing smoothly into IEEE-754 `f64`.
    #[inline]
    #[must_use]
    pub fn add_i32_overflow(a: i32, b: i32) -> Self {
        match a.checked_add(b) {
            Some(sum) => Self::from_i32(sum),
            None => Self::from_f64(f64::from(a) + f64::from(b)),
        }
    }

    /// Checked fast `Int32` subtraction overflowing smoothly into IEEE-754 `f64`.
    #[inline]
    #[must_use]
    pub fn sub_i32_overflow(a: i32, b: i32) -> Self {
        match a.checked_sub(b) {
            Some(diff) => Self::from_i32(diff),
            None => Self::from_f64(f64::from(a) - f64::from(b)),
        }
    }

    /// Checked fast `Int32` multiplication preserving `-0.0` and overflowing cleanly to `f64`.
    #[inline]
    #[must_use]
    pub fn mul_i32_overflow(a: i32, b: i32) -> Self {
        if (a == 0 && b < 0) || (b == 0 && a < 0) {
            return Self::from_f64(-0.0);
        }
        match a.checked_mul(b) {
            Some(prod) => Self::from_i32(prod),
            None => Self::from_f64(f64::from(a) * f64::from(b)),
        }
    }

    /// ECMAScript `SameValue` (`Object.is`) for non-string/non-bigint primitives & object identity.
    #[must_use]
    pub fn same_value_fast(self, other: Self) -> bool {
        if let (Some(a), Some(b)) = (self.to_number_f64(), other.to_number_f64()) {
            if a.is_nan() && b.is_nan() {
                return true;
            }
            if a == 0.0 && b == 0.0 {
                return a.is_sign_negative() == b.is_sign_negative();
            }
            return a == b;
        }
        self.0 == other.0
    }

    /// ECMAScript `===` for numbers, booleans, null, undefined, symbols, and object identity.
    #[must_use]
    pub fn strict_eq_fast(self, other: Self) -> bool {
        if let (Some(a), Some(b)) = (self.to_number_f64(), other.to_number_f64()) {
            return a == b;
        }
        self.0 == other.0
    }
}

impl fmt::Debug for JsValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind() {
            JsValueKind::Float(n) => write!(f, "JsValue::Float({n:?})"),
            JsValueKind::Int32(n) => write!(f, "JsValue::Int32({n})"),
            JsValueKind::Undefined => f.write_str("JsValue::Undefined"),
            JsValueKind::Null => f.write_str("JsValue::Null"),
            JsValueKind::Bool(b) => write!(f, "JsValue::Bool({b})"),
            JsValueKind::Empty => f.write_str("JsValue::Empty"),
            JsValueKind::String(h) => write!(f, "JsValue::String({h:?})"),
            JsValueKind::Object(h) => write!(f, "JsValue::Object({h:?})"),
            JsValueKind::Symbol(s) => write!(f, "JsValue::Symbol({s:?})"),
            JsValueKind::BigInt(h) => write!(f, "JsValue::BigInt({h:?})"),
        }
    }
}
