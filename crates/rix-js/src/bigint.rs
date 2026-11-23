//! Pure-Rust arbitrary-precision signed ECMAScript `BigInt` (`#![deny(unsafe_code)]`).

use std::cmp::Ordering;
use std::fmt;

/// Arbitrary-precision signed integer stored in base-2^32 little-endian magnitude limbs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JsBigInt {
    negative: bool,
    /// Little-endian base 2^32 limbs, normalized with no trailing zero limbs (0 = empty).
    limbs: Vec<u32>,
}

impl Default for JsBigInt {
    fn default() -> Self {
        Self::zero()
    }
}

impl JsBigInt {
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            negative: false,
            limbs: Vec::new(),
        }
    }

    #[must_use]
    pub fn from_i64(n: i64) -> Self {
        if n == 0 {
            return Self::zero();
        }
        let negative = n < 0;
        let mag = n.unsigned_abs();
        let lo = mag as u32;
        let hi = (mag >> 32) as u32;
        let limbs = if hi == 0 { vec![lo] } else { vec![lo, hi] };
        Self { negative, limbs }
    }

    #[must_use]
    pub fn from_i128(n: i128) -> Self {
        if n == 0 {
            return Self::zero();
        }
        let negative = n < 0;
        let mut mag = n.unsigned_abs();
        let mut limbs = Vec::with_capacity(4);
        while mag > 0 {
            limbs.push(mag as u32);
            mag >>= 32;
        }
        Self { negative, limbs }
    }

    /// Parses decimal, `0x` hex, `0o` octal, or `0b` binary BigInt strings (optional leading sign).
    pub fn parse_str(input: &str) -> Option<Self> {
        let trimmed = input.trim().trim_end_matches('n');
        if trimmed.is_empty() {
            return Some(Self::zero());
        }
        let (negative, rest) = if let Some(r) = trimmed.strip_prefix('-') {
            (true, r)
        } else if let Some(r) = trimmed.strip_prefix('+') {
            (false, r)
        } else {
            (false, trimmed)
        };

        let (radix, digits) =
            if let Some(h) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
                (16u32, h)
            } else if let Some(o) = rest.strip_prefix("0o").or_else(|| rest.strip_prefix("0O")) {
                (8u32, o)
            } else if let Some(b) = rest.strip_prefix("0b").or_else(|| rest.strip_prefix("0B")) {
                (2u32, b)
            } else {
                (10u32, rest)
            };

        if digits.is_empty() {
            return None;
        }
        let mut limbs: Vec<u32> = Vec::new();
        for ch in digits.chars() {
            if ch == '_' {
                continue;
            }
            let d = ch.to_digit(radix)?;
            mul_add_limb(&mut limbs, radix, d);
        }
        let mut out = Self { negative, limbs };
        out.normalize();
        Some(out)
    }

    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    #[must_use]
    pub fn is_negative(&self) -> bool {
        self.negative && !self.limbs.is_empty()
    }

    #[must_use]
    pub fn to_i64_lossy(&self) -> i64 {
        let lo = u64::from(self.limbs.first().copied().unwrap_or(0));
        let hi = u64::from(self.limbs.get(1).copied().unwrap_or(0));
        let mag = lo | (hi << 32);
        if self.negative {
            (mag.wrapping_neg()) as i64
        } else {
            mag as i64
        }
    }

    #[must_use]
    pub fn to_f64(&self) -> f64 {
        let mut acc = 0.0f64;
        for &limb in self.limbs.iter().rev() {
            acc = acc * 4_294_967_296.0 + f64::from(limb);
        }
        if self.negative {
            -acc
        } else {
            acc
        }
    }

    #[must_use]
    pub fn negate(&self) -> Self {
        let mut copy = self.clone();
        if !copy.is_zero() {
            copy.negative = !copy.negative;
        }
        copy
    }

    #[must_use]
    pub fn add(&self, rhs: &Self) -> Self {
        if self.negative == rhs.negative {
            let mut out = Self {
                negative: self.negative,
                limbs: add_mag(&self.limbs, &rhs.limbs),
            };
            out.normalize();
            out
        } else {
            match cmp_mag(&self.limbs, &rhs.limbs) {
                Ordering::Equal => Self::zero(),
                Ordering::Greater => {
                    let mut out = Self {
                        negative: self.negative,
                        limbs: sub_mag(&self.limbs, &rhs.limbs),
                    };
                    out.normalize();
                    out
                }
                Ordering::Less => {
                    let mut out = Self {
                        negative: rhs.negative,
                        limbs: sub_mag(&rhs.limbs, &self.limbs),
                    };
                    out.normalize();
                    out
                }
            }
        }
    }

    #[must_use]
    pub fn sub(&self, rhs: &Self) -> Self {
        self.add(&rhs.negate())
    }

    #[must_use]
    pub fn mul(&self, rhs: &Self) -> Self {
        if self.is_zero() || rhs.is_zero() {
            return Self::zero();
        }
        let mut out = vec![0u32; self.limbs.len() + rhs.limbs.len()];
        for (i, &a) in self.limbs.iter().enumerate() {
            let mut carry = 0u64;
            for (j, &b) in rhs.limbs.iter().enumerate() {
                let prod = u64::from(a) * u64::from(b) + u64::from(out[i + j]) + carry;
                out[i + j] = prod as u32;
                carry = prod >> 32;
            }
            out[i + rhs.limbs.len()] = carry as u32;
        }
        let mut res = Self {
            negative: self.negative ^ rhs.negative,
            limbs: out,
        };
        res.normalize();
        res
    }

    /// Formats the BigInt in base 10 without the trailing `n`.
    #[must_use]
    pub fn to_decimal_string(&self) -> String {
        self.to_radix_string(10)
    }

    /// Formats the BigInt in the given radix (`2..=36`) without the trailing `n`.
    #[must_use]
    pub fn to_radix_string(&self, radix: u32) -> String {
        debug_assert!((2..=36).contains(&radix));
        if self.is_zero() {
            return "0".to_string();
        }
        const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let mut work = self.limbs.clone();
        let mut digits = Vec::new();
        while !work.is_empty() {
            let rem = div_rem_limb(&mut work, radix);
            digits.push(char::from(DIGITS[rem as usize]));
            while work.last() == Some(&0) {
                work.pop();
            }
        }
        if self.negative {
            digits.push('-');
        }
        digits.into_iter().rev().collect()
    }

    fn normalize(&mut self) {
        while self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
        if self.limbs.is_empty() {
            self.negative = false;
        }
    }

    /// Truncating division and remainder (ECMA-262 `BigInt::divide` / `BigInt::remainder`).
    pub fn div_rem(&self, rhs: &Self) -> Option<(Self, Self)> {
        if rhs.is_zero() {
            return None;
        }
        let cmp = cmp_mag(&self.limbs, &rhs.limbs);
        if cmp == Ordering::Less {
            return Some((Self::zero(), self.clone()));
        }
        let (q_limbs, r_limbs) = if rhs.limbs.len() == 1 {
            let mut q = self.limbs.clone();
            let r = div_rem_limb(&mut q, rhs.limbs[0]);
            (q, if r == 0 { Vec::new() } else { vec![r] })
        } else {
            let total_bits = self.limbs.len() * 32;
            let mut q = vec![0u32; self.limbs.len()];
            let mut rem: Vec<u32> = Vec::new();
            for bit in (0..total_bits).rev() {
                // rem = (rem << 1) | bit_i(self)
                let b = (self.limbs[bit / 32] >> (bit % 32)) & 1;
                let mut carry = b;
                for limb in &mut rem {
                    let next = *limb >> 31;
                    *limb = (*limb << 1) | carry;
                    carry = next;
                }
                if carry != 0 {
                    rem.push(carry);
                }
                while rem.last() == Some(&0) {
                    rem.pop();
                }
                if cmp_mag(&rem, &rhs.limbs) != Ordering::Less {
                    rem = sub_mag(&rem, &rhs.limbs);
                    while rem.last() == Some(&0) {
                        rem.pop();
                    }
                    q[bit / 32] |= 1 << (bit % 32);
                }
            }
            (q, rem)
        };
        let mut quo = Self {
            negative: self.negative ^ rhs.negative,
            limbs: q_limbs,
        };
        let mut rem = Self {
            negative: self.negative,
            limbs: r_limbs,
        };
        quo.normalize();
        rem.normalize();
        Some((quo, rem))
    }

    /// Exponentiation (`self ** exp`); returns `None` on negative exponent or size overflow.
    pub fn pow(&self, exp: &Self) -> Option<Self> {
        if exp.is_negative() {
            return None;
        }
        if exp.is_zero() {
            return Some(Self::from_i64(1));
        }
        if self.is_zero() {
            return Some(Self::zero());
        }
        if self.limbs.len() == 1 && self.limbs[0] == 1 {
            let odd = (exp.limbs[0] & 1) == 1;
            return Some(Self::from_i64(if self.negative && odd { -1 } else { 1 }));
        }
        let e = if exp.limbs.len() > 1 || exp.limbs[0] > 1_000_000 {
            return None;
        } else {
            exp.limbs[0]
        };
        let mut base = self.clone();
        let mut res = Self::from_i64(1);
        let mut n = e;
        while n > 0 {
            if (n & 1) == 1 {
                res = res.mul(&base);
            }
            n >>= 1;
            if n > 0 {
                base = base.mul(&base);
                if base.limbs.len() > 32_768 {
                    return None;
                }
            }
        }
        Some(res)
    }

    pub fn shl(&self, shift: &Self) -> Option<Self> {
        if shift.is_negative() {
            return self.shr(&shift.negate());
        }
        if self.is_zero() || shift.is_zero() {
            return Some(self.clone());
        }
        if shift.limbs.len() > 1 || shift.limbs[0] > 1_048_576 {
            return None;
        }
        let bits = shift.limbs[0] as usize;
        let words = bits / 32;
        let rem = (bits % 32) as u32;
        let mut out = vec![0u32; words];
        let mut carry = 0u32;
        for &limb in &self.limbs {
            if rem == 0 {
                out.push(limb);
            } else {
                out.push((limb << rem) | carry);
                carry = limb >> (32 - rem);
            }
        }
        if carry != 0 {
            out.push(carry);
        }
        let mut res = Self {
            negative: self.negative,
            limbs: out,
        };
        res.normalize();
        Some(res)
    }

    pub fn shr(&self, shift: &Self) -> Option<Self> {
        if shift.is_negative() {
            return self.shl(&shift.negate());
        }
        if self.is_zero() || shift.is_zero() {
            return Some(self.clone());
        }
        if shift.limbs.len() > 1 || (shift.limbs[0] as usize) >= self.limbs.len() * 32 {
            return Some(if self.negative {
                Self::from_i64(-1)
            } else {
                Self::zero()
            });
        }
        let bits = shift.limbs[0] as usize;
        let words = bits / 32;
        let rem = (bits % 32) as u32;
        let mut dropped_nonzero = self.limbs[..words].iter().any(|&l| l != 0);
        if rem != 0 && (self.limbs[words] & ((1u32 << rem) - 1)) != 0 {
            dropped_nonzero = true;
        }
        let mut out = Vec::with_capacity(self.limbs.len() - words);
        for i in words..self.limbs.len() {
            let lo = self.limbs[i] >> rem;
            let hi = if rem == 0 || i + 1 >= self.limbs.len() {
                0
            } else {
                self.limbs[i + 1] << (32 - rem)
            };
            out.push(lo | hi);
        }
        let mut res = Self {
            negative: self.negative,
            limbs: out,
        };
        res.normalize();
        if self.negative && dropped_nonzero {
            res = res.sub(&Self::from_i64(1));
        }
        Some(res)
    }

    pub fn bitwise_op(&self, rhs: &Self, op: u8) -> Self {
        let n = self.limbs.len().max(rhs.limbs.len()) + 1;
        let to_twos = |bi: &Self| -> Vec<u32> {
            let mut v = vec![0u32; n];
            for (i, &l) in bi.limbs.iter().enumerate() {
                v[i] = l;
            }
            if bi.negative {
                let mut carry = 1u64;
                for limb in &mut v {
                    let s = u64::from(!*limb) + carry;
                    *limb = s as u32;
                    carry = s >> 32;
                }
            }
            v
        };
        let a = to_twos(self);
        let b = to_twos(rhs);
        let mut out = vec![0u32; n];
        for i in 0..n {
            out[i] = match op {
                0 => a[i] & b[i],
                1 => a[i] | b[i],
                _ => a[i] ^ b[i],
            };
        }
        let neg = (out[n - 1] & 0x8000_0000) != 0;
        if neg {
            let mut carry = 1u64;
            for limb in &mut out {
                let s = u64::from(!*limb) + carry;
                *limb = s as u32;
                carry = s >> 32;
            }
        }
        let mut res = Self {
            negative: neg,
            limbs: out,
        };
        res.normalize();
        res
    }
}

impl Ord for JsBigInt {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.is_negative(), other.is_negative()) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => cmp_mag(&self.limbs, &other.limbs),
            (true, true) => cmp_mag(&other.limbs, &self.limbs),
        }
    }
}

impl PartialOrd for JsBigInt {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for JsBigInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_decimal_string())
    }
}

fn cmp_mag(a: &[u32], b: &[u32]) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| {
        for (&x, &y) in a.iter().rev().zip(b.iter().rev()) {
            let ord = x.cmp(&y);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    })
}

fn add_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let n = a.len().max(b.len());
    let mut out = Vec::with_capacity(n + 1);
    let mut carry = 0u64;
    for i in 0..n {
        let sum = u64::from(a.get(i).copied().unwrap_or(0))
            + u64::from(b.get(i).copied().unwrap_or(0))
            + carry;
        out.push(sum as u32);
        carry = sum >> 32;
    }
    if carry > 0 {
        out.push(carry as u32);
    }
    out
}

fn sub_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for (i, &limb) in a.iter().enumerate() {
        let av = i64::from(limb);
        let bv = i64::from(b.get(i).copied().unwrap_or(0));
        let mut diff = av - bv - borrow;
        if diff < 0 {
            diff += 1i64 << 32;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out.push(diff as u32);
    }
    out
}

fn mul_add_limb(limbs: &mut Vec<u32>, mul: u32, add: u32) {
    let mut carry = u64::from(add);
    for limb in limbs.iter_mut() {
        let val = u64::from(*limb) * u64::from(mul) + carry;
        *limb = val as u32;
        carry = val >> 32;
    }
    if carry > 0 {
        limbs.push(carry as u32);
    }
}

fn div_rem_limb(limbs: &mut [u32], divisor: u32) -> u32 {
    let mut rem = 0u64;
    for limb in limbs.iter_mut().rev() {
        let cur = (rem << 32) | u64::from(*limb);
        *limb = (cur / u64::from(divisor)) as u32;
        rem = cur % u64::from(divisor);
    }
    rem as u32
}
