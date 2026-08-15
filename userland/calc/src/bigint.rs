//! Arbitrary-precision integers: a sign and a little-endian vector of 32-bit
//! limbs.
//!
//! Everything above this module is exact or exactly rounded, so this is the one
//! place a carry can be dropped. It carries the operations the layers above ask
//! for and no others.

use core::cmp::Ordering;
use core::fmt;
use core::ops::{Add, Mul, Neg, Sub};

/// The base the limbs count in, as the `u64` the intermediates use.
const BASE: u64 = 1 << 32;

/// How many decimal digits one chunk of [`Int::to_decimal_string`] emits.
const CHUNK_DIGITS: u32 = 9;
/// `10^CHUNK_DIGITS`, the largest power of ten that fits a limb.
const CHUNK: u32 = 1_000_000_000;

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Int {
    /// Never true while `mag` is empty: zero has exactly one representation.
    neg: bool,
    /// Little-endian base-2^32 limbs with no trailing zero. Empty is zero.
    mag: Vec<u32>,
}

impl Int {
    pub fn zero() -> Int {
        Int { neg: false, mag: Vec::new() }
    }

    pub fn one() -> Int {
        Int { neg: false, mag: vec![1] }
    }

    pub fn from_u64(v: u64) -> Int {
        let mut mag = vec![v as u32, (v >> 32) as u32];
        trim(&mut mag);
        Int { neg: false, mag }
    }

    pub fn from_i64(v: i64) -> Int {
        let mut out = Int::from_u64(v.unsigned_abs());
        out.neg = v < 0 && !out.mag.is_empty();
        out
    }

    /// `1 << bits`.
    pub fn pow2(bits: usize) -> Int {
        let mut mag = vec![0u32; bits / 32 + 1];
        mag[bits / 32] = 1 << (bits % 32);
        Int { neg: false, mag }
    }

    /// `10^k`, by squaring.
    pub fn pow10(k: u32) -> Int {
        Int::from_u64(10).pow(k)
    }

    pub fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }

    pub fn is_negative(&self) -> bool {
        self.neg
    }

    pub fn is_odd(&self) -> bool {
        self.mag.first().is_some_and(|l| l & 1 == 1)
    }

    /// `-1`, `0` or `1`.
    pub fn signum(&self) -> i32 {
        if self.is_zero() {
            0
        } else if self.neg {
            -1
        } else {
            1
        }
    }

    pub fn abs(&self) -> Int {
        Int { neg: false, mag: self.mag.clone() }
    }

    /// The position of the highest set bit, counted from one. Zero for zero.
    pub fn bit_len(&self) -> usize {
        match self.mag.last() {
            None => 0,
            Some(top) => self.mag.len() * 32 - top.leading_zeros() as usize,
        }
    }

    pub fn to_i64(&self) -> Option<i64> {
        if self.mag.len() > 2 {
            return None;
        }
        let mut v: u64 = 0;
        for (i, &l) in self.mag.iter().enumerate() {
            v |= (l as u64) << (32 * i);
        }
        if self.neg {
            if v > 1 << 63 {
                None
            } else {
                Some((v as i64).wrapping_neg())
            }
        } else if v > i64::MAX as u64 {
            None
        } else {
            Some(v as i64)
        }
    }

    pub fn to_u32(&self) -> Option<u32> {
        if self.neg || self.mag.len() > 1 {
            return None;
        }
        Some(self.mag.first().copied().unwrap_or(0))
    }

    /// `self * 10^k`.
    pub fn mul_pow10(&self, k: u32) -> Int {
        if self.is_zero() || k == 0 {
            return self.clone();
        }
        self * &Int::pow10(k)
    }

    pub fn pow(&self, mut exp: u32) -> Int {
        let mut base = self.clone();
        let mut acc = Int::one();
        while exp > 0 {
            if exp & 1 == 1 {
                acc = &acc * &base;
            }
            exp >>= 1;
            if exp > 0 {
                base = &base * &base;
            }
        }
        acc
    }

    /// Quotient truncated toward zero and a remainder carrying the dividend's
    /// sign, or `None` when the divisor is zero.
    pub fn div_rem(&self, other: &Int) -> Option<(Int, Int)> {
        if other.is_zero() {
            return None;
        }
        let (q, r) = mag_div_rem(&self.mag, &other.mag);
        let mut q = Int { neg: self.neg != other.neg, mag: q };
        let mut r = Int { neg: self.neg, mag: r };
        q.fix_sign();
        r.fix_sign();
        Some((q, r))
    }

    /// The non-negative greatest common divisor. `gcd(0, 0)` is zero.
    pub fn gcd(&self, other: &Int) -> Int {
        let mut a = self.abs();
        let mut b = other.abs();
        while !b.is_zero() {
            let r = a.div_rem(&b).expect("b is non-zero").1;
            a = b;
            b = r.abs();
        }
        a
    }

    /// The largest `r` with `r*r <= self`, or `None` for a negative number.
    pub fn isqrt(&self) -> Option<Int> {
        if self.neg {
            return None;
        }
        if self.is_zero() {
            return Some(Int::zero());
        }
        let mut x = Int::pow2(self.bit_len().div_ceil(2));
        loop {
            // `x` is never zero here: it starts above the root and the loop
            // stops as soon as an iteration fails to lower it.
            let next = &(&x + &self.div_rem(&x).expect("x is non-zero").0) >> 1;
            if next >= x {
                return Some(x);
            }
            x = next;
        }
    }

    /// The digits of `|self|` in base ten, most significant first, never empty.
    pub fn decimal_digits(&self) -> String {
        if self.is_zero() {
            return "0".to_string();
        }
        let mut chunks: Vec<u32> = Vec::new();
        let mut rest = self.mag.clone();
        while !rest.is_empty() {
            let (q, r) = mag_div_rem_small(&rest, CHUNK);
            chunks.push(r);
            rest = q;
        }
        let mut out = chunks.pop().expect("a non-zero number has a chunk").to_string();
        while let Some(c) = chunks.pop() {
            out.push_str(&format!("{c:0width$}", width = CHUNK_DIGITS as usize));
        }
        out
    }

    /// How many digits `|self|` has in base ten. One, for zero.
    pub fn decimal_len(&self) -> usize {
        self.decimal_digits().len()
    }

    /// Parse `digits` in `radix`, which must be 2, 8, 10 or 16. `None` when a
    /// character is not a digit of that radix or there are no digits at all.
    pub fn parse_radix(digits: &str, radix: u32) -> Option<Int> {
        if digits.is_empty() {
            return None;
        }
        let mut mag: Vec<u32> = Vec::new();
        for ch in digits.chars() {
            let d = ch.to_digit(radix)?;
            mag = mag_mul_small(&mag, radix);
            mag = mag_add_small(&mag, d);
        }
        trim(&mut mag);
        Some(Int { neg: false, mag })
    }

    fn fix_sign(&mut self) {
        if self.mag.is_empty() {
            self.neg = false;
        }
    }
}

impl fmt::Display for Int {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.neg {
            f.write_str("-")?;
        }
        f.write_str(&self.decimal_digits())
    }
}

impl Ord for Int {
    fn cmp(&self, other: &Int) -> Ordering {
        match (self.neg, other.neg) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => mag_cmp(&self.mag, &other.mag),
            (true, true) => mag_cmp(&other.mag, &self.mag),
        }
    }
}

impl PartialOrd for Int {
    fn partial_cmp(&self, other: &Int) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Neg for &Int {
    type Output = Int;
    fn neg(self) -> Int {
        let mut out = Int { neg: !self.neg, mag: self.mag.clone() };
        out.fix_sign();
        out
    }
}

impl Add for &Int {
    type Output = Int;
    fn add(self, other: &Int) -> Int {
        let mut out = if self.neg == other.neg {
            Int { neg: self.neg, mag: mag_add(&self.mag, &other.mag) }
        } else {
            match mag_cmp(&self.mag, &other.mag) {
                Ordering::Less => Int { neg: other.neg, mag: mag_sub(&other.mag, &self.mag) },
                _ => Int { neg: self.neg, mag: mag_sub(&self.mag, &other.mag) },
            }
        };
        out.fix_sign();
        out
    }
}

impl Sub for &Int {
    type Output = Int;
    fn sub(self, other: &Int) -> Int {
        self + &(-other)
    }
}

impl Mul for &Int {
    type Output = Int;
    fn mul(self, other: &Int) -> Int {
        let mut out = Int { neg: self.neg != other.neg, mag: mag_mul(&self.mag, &other.mag) };
        out.fix_sign();
        out
    }
}

/// Halving, used by the square root's Newton step. Truncates toward zero, which
/// is exact there because both operands are non-negative.
impl core::ops::Shr<u32> for &Int {
    type Output = Int;
    fn shr(self, bits: u32) -> Int {
        let mut mag = mag_shr(&self.mag, bits);
        trim(&mut mag);
        let mut out = Int { neg: self.neg, mag };
        out.fix_sign();
        out
    }
}

// --- magnitude helpers, all on trimmed little-endian slices ---

fn trim(mag: &mut Vec<u32>) {
    while mag.last() == Some(&0) {
        mag.pop();
    }
}

fn mag_cmp(a: &[u32], b: &[u32]) -> Ordering {
    if a.len() != b.len() {
        return a.len().cmp(&b.len());
    }
    for i in (0..a.len()).rev() {
        if a[i] != b[i] {
            return a[i].cmp(&b[i]);
        }
    }
    Ordering::Equal
}

fn mag_add(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(long.len() + 1);
    let mut carry = 0u64;
    for (i, &x) in long.iter().enumerate() {
        let sum = x as u64 + short.get(i).copied().unwrap_or(0) as u64 + carry;
        out.push(sum as u32);
        carry = sum >> 32;
    }
    if carry != 0 {
        out.push(carry as u32);
    }
    out
}

/// `a - b`, which every caller has already shown is non-negative.
fn mag_sub(a: &[u32], b: &[u32]) -> Vec<u32> {
    debug_assert!(mag_cmp(a, b) != Ordering::Less);
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for (i, &x) in a.iter().enumerate() {
        let d = x as i64 - b.get(i).copied().unwrap_or(0) as i64 - borrow;
        if d < 0 {
            out.push((d + BASE as i64) as u32);
            borrow = 1;
        } else {
            out.push(d as u32);
            borrow = 0;
        }
    }
    debug_assert_eq!(borrow, 0);
    trim(&mut out);
    out
}

fn mag_mul(a: &[u32], b: &[u32]) -> Vec<u32> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u32; a.len() + b.len()];
    for (i, &x) in a.iter().enumerate() {
        let mut carry = 0u64;
        for (j, &y) in b.iter().enumerate() {
            let t = x as u64 * y as u64 + out[i + j] as u64 + carry;
            out[i + j] = t as u32;
            carry = t >> 32;
        }
        out[i + b.len()] = carry as u32;
    }
    trim(&mut out);
    out
}

fn mag_mul_small(a: &[u32], m: u32) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len() + 1);
    let mut carry = 0u64;
    for &x in a {
        let t = x as u64 * m as u64 + carry;
        out.push(t as u32);
        carry = t >> 32;
    }
    if carry != 0 {
        out.push(carry as u32);
    }
    trim(&mut out);
    out
}

fn mag_add_small(a: &[u32], d: u32) -> Vec<u32> {
    let mut out = a.to_vec();
    let mut carry = d as u64;
    for limb in out.iter_mut() {
        if carry == 0 {
            break;
        }
        let t = *limb as u64 + carry;
        *limb = t as u32;
        carry = t >> 32;
    }
    if carry != 0 {
        out.push(carry as u32);
    }
    trim(&mut out);
    out
}

fn mag_div_rem_small(a: &[u32], d: u32) -> (Vec<u32>, u32) {
    let mut q = vec![0u32; a.len()];
    let mut rem = 0u64;
    for i in (0..a.len()).rev() {
        let cur = (rem << 32) | a[i] as u64;
        q[i] = (cur / d as u64) as u32;
        rem = cur % d as u64;
    }
    trim(&mut q);
    (q, rem as u32)
}

fn mag_shl(a: &[u32], bits: u32) -> Vec<u32> {
    if bits == 0 {
        return a.to_vec();
    }
    let mut out = Vec::with_capacity(a.len() + 1);
    let mut carry = 0u32;
    for &x in a {
        out.push((x << bits) | carry);
        carry = x >> (32 - bits);
    }
    if carry != 0 {
        out.push(carry);
    }
    out
}

fn mag_shr(a: &[u32], bits: u32) -> Vec<u32> {
    let words = (bits / 32) as usize;
    let bits = bits % 32;
    if words >= a.len() {
        return Vec::new();
    }
    let a = &a[words..];
    if bits == 0 {
        return a.to_vec();
    }
    let mut out = vec![0u32; a.len()];
    let mut carry = 0u32;
    for i in (0..a.len()).rev() {
        out[i] = (a[i] >> bits) | carry;
        carry = a[i] << (32 - bits);
    }
    out
}

/// Knuth's algorithm D, in the formulation that keeps every intermediate in a
/// `u64`. The caller has already refused a zero divisor.
fn mag_div_rem(a: &[u32], b: &[u32]) -> (Vec<u32>, Vec<u32>) {
    debug_assert!(!b.is_empty());
    if mag_cmp(a, b) == Ordering::Less {
        return (Vec::new(), a.to_vec());
    }
    if b.len() == 1 {
        let (q, r) = mag_div_rem_small(a, b[0]);
        return (q, if r == 0 { Vec::new() } else { vec![r] });
    }

    let n = b.len();
    let m = a.len() - n;
    let shift = b[n - 1].leading_zeros();
    let v = mag_shl(b, shift);
    debug_assert_eq!(v.len(), n);
    let mut u = mag_shl(a, shift);
    u.resize(a.len() + 1, 0);

    let mut q = vec![0u32; m + 1];
    for j in (0..=m).rev() {
        let num = ((u[j + n] as u64) << 32) | u[j + n - 1] as u64;
        let mut qhat = num / v[n - 1] as u64;
        let mut rhat = num - qhat * v[n - 1] as u64;
        while qhat >= BASE || qhat * v[n - 2] as u64 > (rhat << 32) + u[j + n - 2] as u64 {
            qhat -= 1;
            rhat += v[n - 1] as u64;
            if rhat >= BASE {
                break;
            }
        }

        // Multiply and subtract; `k` carries both the product's high half and
        // the running borrow, so it stays inside an `i64`.
        let mut k: i64 = 0;
        for i in 0..n {
            let p = qhat * v[i] as u64;
            let t = u[i + j] as i64 - k - ((p & 0xFFFF_FFFF) as i64);
            u[i + j] = t as u32;
            k = (p >> 32) as i64 - (t >> 32);
        }
        let t = u[j + n] as i64 - k;
        u[j + n] = t as u32;

        if t < 0 {
            // `qhat` was one too large, which algorithm D allows; give it back.
            qhat -= 1;
            let mut carry = 0u64;
            for i in 0..n {
                let s = u[i + j] as u64 + v[i] as u64 + carry;
                u[i + j] = s as u32;
                carry = s >> 32;
            }
            u[j + n] = (u[j + n] as u64 + carry) as u32;
        }
        q[j] = qhat as u32;
    }

    trim(&mut q);
    let mut r = mag_shr(&u[..n], shift);
    trim(&mut r);
    (q, r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(s: &str) -> Int {
        let (neg, digits) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let mut v = Int::parse_radix(digits, 10).unwrap();
        v.neg = neg && !v.is_zero();
        v
    }

    #[test]
    fn zero_has_one_representation() {
        assert_eq!(Int::from_i64(0), Int::zero());
        assert_eq!(&Int::from_i64(5) - &Int::from_i64(5), Int::zero());
        assert!(!(&Int::from_i64(5) - &Int::from_i64(5)).is_negative());
        assert_eq!((&Int::from_i64(-3) * &Int::zero()).to_string(), "0");
    }

    #[test]
    fn addition_crosses_the_limb_boundary() {
        let a = int("4294967295");
        assert_eq!((&a + &Int::one()).to_string(), "4294967296");
        assert_eq!((&Int::pow2(64) - &Int::one()).to_string(), "18446744073709551615");
        assert_eq!((&int("-7") + &int("3")).to_string(), "-4");
        assert_eq!((&int("7") + &int("-3")).to_string(), "4");
        assert_eq!((&int("-7") - &int("-3")).to_string(), "-4");
    }

    #[test]
    fn multiplication_matches_a_known_product() {
        let a = int("123456789012345678901234567890");
        let b = int("987654321098765432109876543210");
        assert_eq!(
            (&a * &b).to_string(),
            "121932631137021795226185032733622923332237463801111263526900"
        );
        assert_eq!((&a * &int("-1")).to_string(), "-123456789012345678901234567890");
    }

    #[test]
    fn division_is_exact_over_many_shapes() {
        // Multi-limb divisors exercise the qhat correction; the identity
        // a == q*b + r with |r| < |b| is what algorithm D promises.
        let cases: &[(&str, &str)] = &[
            ("100", "7"),
            ("18446744073709551616", "4294967297"),
            ("121932631137021795226185032733622923332237463801111263526900", "987654321098765432109876543210"),
            ("999999999999999999999999999999999", "1000000000000000"),
            ("-100", "7"),
            ("100", "-7"),
            ("-100", "-7"),
            ("5", "1000000000000000000000"),
        ];
        for (a, b) in cases {
            let (a, b) = (int(a), int(b));
            let (q, r) = a.div_rem(&b).unwrap();
            assert_eq!(&(&q * &b) + &r, a, "a={a} b={b} q={q} r={r}");
            assert!(r.abs() < b.abs(), "remainder {r} is not smaller than {b}");
            if !r.is_zero() {
                assert_eq!(r.is_negative(), a.is_negative(), "remainder takes the dividend's sign");
            }
        }
        assert!(Int::one().div_rem(&Int::zero()).is_none());
    }

    #[test]
    fn powers_and_roots_agree() {
        assert_eq!(int("2").pow(64).to_string(), "18446744073709551616");
        assert_eq!(int("10").pow(0).to_string(), "1");
        assert_eq!(int("-3").pow(3).to_string(), "-27");
        let n = int("152415787532388367501905199875019052100");
        assert_eq!(n.isqrt().unwrap().to_string(), "12345678901234567890");
        assert_eq!((&n - &Int::one()).isqrt().unwrap().to_string(), "12345678901234567889");
        assert_eq!(Int::zero().isqrt().unwrap().to_string(), "0");
        assert!(int("-1").isqrt().is_none());
    }

    #[test]
    fn gcd_is_non_negative_and_divides_both() {
        assert_eq!(int("-48").gcd(&int("18")).to_string(), "6");
        assert_eq!(int("0").gcd(&int("0")).to_string(), "0");
        assert_eq!(int("0").gcd(&int("-5")).to_string(), "5");
        let a = int("123456789012345678901234567890");
        let b = int("987654321098765432109876543210");
        let g = a.gcd(&b);
        assert!(a.div_rem(&g).unwrap().1.is_zero());
        assert!(b.div_rem(&g).unwrap().1.is_zero());
    }

    #[test]
    fn radix_parsing_and_lengths() {
        assert_eq!(Int::parse_radix("ffff", 16).unwrap().to_string(), "65535");
        assert_eq!(Int::parse_radix("1010", 2).unwrap().to_string(), "10");
        assert!(Int::parse_radix("12", 2).is_none());
        assert!(Int::parse_radix("", 10).is_none());
        assert_eq!(Int::zero().decimal_len(), 1);
        assert_eq!(Int::pow10(40).decimal_len(), 41);
        assert_eq!(int("-1000").decimal_len(), 4);
    }

    #[test]
    fn conversions_refuse_what_does_not_fit() {
        assert_eq!(Int::from_i64(i64::MIN).to_i64(), Some(i64::MIN));
        assert_eq!(Int::from_i64(i64::MAX).to_i64(), Some(i64::MAX));
        assert_eq!((&Int::from_i64(i64::MAX) + &Int::one()).to_i64(), None);
        assert_eq!((&Int::from_i64(i64::MIN) - &Int::one()).to_i64(), None);
        assert_eq!(Int::from_u64(u64::MAX).to_u32(), None);
        assert_eq!(int("-1").to_u32(), None);
    }
}
