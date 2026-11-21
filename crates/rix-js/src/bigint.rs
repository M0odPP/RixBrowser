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
        if self.is_zero() {
            return "0".to_string();
        }
        let mut work = self.limbs.clone();
        let mut digits = Vec::new();
        while !work.is_empty() {
            let rem = div_rem_limb(&mut work, 10);
            digits.push(char::from(b'0' + rem as u8));
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
