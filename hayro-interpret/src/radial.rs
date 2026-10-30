//! How the two circles of a radial shading lie, decided on its coordinates as
//! written.

use hayro_syntax::object::Number;

/// The leading coefficient of the quadratic the painter solves for the circle of a
/// radial shading through a point, `|c1 - c0|² - (r1 - r0)²`, of its `/Coords` as
/// written in the file, with the sign of their exact value: positive where the
/// circles form a cone, zero where one touches the other inside, negative where
/// one holds the other apart from it. Every decision between these is made on
/// this sign, the painter's and the extension's alike.
///
/// No tolerance tells them apart, and no reading in fewer digits: circles a
/// document writes touching, such as `[10.3 0 0 50.7 0 40.4]`, touch, though in
/// `f32` a third of such draws are a cone an ulp wide, whose further circles sweep
/// the end's colour over its whole inside; and `[0 0 0 1000000.25 0 1000000]` is
/// a cone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lead(f64);

impl Lead {
    /// Of `/Coords [x0 y0 r0 x1 y1 r1]`. Each number with at most 19 significant
    /// digits is its exact decimal ([`Number::as_decimal`]), and the coefficient's
    /// sign exact ([`decimal_sum`]). A real of more digits, or an integer beyond 64
    /// bits, hayro-syntax keeps only as the nearest `f64`: then each coordinate is
    /// read as its nearest `f64`, the coefficient of those is found exactly
    /// ([`exact_sum`]), and circles it lies within the rounding of those readings
    /// of touching ([`READ_ROUNDING`]) are taken to touch, which only such
    /// coordinates may make of a cone or nested circles that close.
    pub(crate) fn of(coords: [Number; 6]) -> Self {
        let decimals = coords.map(|n| n.as_decimal());
        // The terms of the coefficient, each a product of two coordinates, those
        // counted twice listed twice: `x1² - 2 x0 x1 + x0²`, likewise for `y`, less
        // `r1² - 2 r0 r1 + r0²`.
        const TERMS: [(usize, usize, bool); 12] = [
            (3, 3, false),
            (0, 3, true),
            (0, 3, true),
            (0, 0, false),
            (4, 4, false),
            (1, 4, true),
            (1, 4, true),
            (1, 1, false),
            (5, 5, true),
            (2, 5, false),
            (2, 5, false),
            (2, 2, true),
        ];
        if let [Some(_), Some(_), Some(_), Some(_), Some(_), Some(_)] = decimals {
            let d = decimals.map(|d| d.expect("all are decimals"));
            let terms = TERMS.map(|(i, j, minus)| {
                let ((ni, di, si), (nj, dj, sj)) = (d[i], d[j]);
                Term {
                    negative: ni ^ nj ^ minus,
                    digits: u128::from(di) * u128::from(dj),
                    scale: u64::from(si) + u64::from(sj),
                }
            });
            return Self(decimal_sum(terms));
        }
        let v = coords.map(nearest_f64);
        let mut parts = Vec::with_capacity(2 * TERMS.len());
        let mut size = 0.0;
        for (i, j, minus) in TERMS {
            let sign = if minus { -1.0 } else { 1.0 };
            let product = v[i] * v[j];
            parts.push(sign * product);
            parts.push(sign * v[i].mul_add(v[j], -product));
            size += product.abs();
        }
        let a = exact_sum(&parts);
        if !a.is_finite() || a.abs() <= size * READ_ROUNDING {
            Self(0.0)
        } else {
            Self(a)
        }
    }

    /// The coefficient, rounded: zero exactly where it is, of its sign.
    pub(crate) fn value(self) -> f64 {
        self.0
    }

    /// Whether the circles form a cone: every point within the circle ending it
    /// then lies on a circle past that end too. Where they do not, one circle
    /// holds the other, and a point within them lies on one of the circles
    /// between.
    pub fn cone(self) -> bool {
        self.0 > 0.0
    }
}

/// How far the coefficient of coordinates each read as its nearest `f64` may lie
/// from the coefficient of the coordinates as written, as a share of the sum of
/// its terms' sizes: a product of two readings, each within `u` of its share of the
/// written number (`u = 2⁻⁵³`), lies within `2u + 3u²` of the product written, and
/// the sum of sizes is rounded by at most `12u` of itself.
const READ_ROUNDING: f64 = {
    let u = f64::EPSILON / 2.0;
    (2.0 * u + 3.0 * u * u) * (1.0 + 16.0 * u)
};

/// Whether two numbers as written are one value: decimals compared exactly, any
/// other real by its nearest `f64`.
pub(crate) fn same(a: Number, b: Number) -> bool {
    match (a.as_decimal(), b.as_decimal()) {
        (Some(a), Some(b)) => normal(a) == normal(b),
        _ => nearest_f64(a) == nearest_f64(b),
    }
}

/// A decimal with no trailing zeros after its point, and zero without a sign.
fn normal((negative, mut digits, mut scale): (bool, u64, u32)) -> (bool, u64, u32) {
    while scale > 0 && digits % 10 == 0 {
        digits /= 10;
        scale -= 1;
    }
    if digits == 0 {
        (false, 0, 0)
    } else {
        (negative, digits, scale)
    }
}

/// The `f64` nearest to the number as written, which [`Number::as_f64`] may miss
/// by a place for a decimal of 17 to 19 digits.
fn nearest_f64(n: Number) -> f64 {
    match n.as_decimal() {
        Some((negative, digits, scale)) => {
            let sign = if negative { "-" } else { "" };
            format!("{sign}{digits}e-{scale}")
                .parse()
                .expect("digits and an exponent read as a float")
        }
        None => n.as_f64(),
    }
}

/// `±digits × 10^-scale`.
#[derive(Clone, Copy)]
struct Term {
    negative: bool,
    digits: u128,
    scale: u64,
}

/// The sum of `terms`, rounded: of the exact sum's sign, and zero only where it is
/// zero. The terms are added from the coarsest scale to the finest into an integer
/// in units of the scale reached, scaled by ten as each finer one comes; once that
/// integer is past `DECIDED`, the finer terms, each below 2^128 in its units and so
/// below them in the integer's, cannot change its sign or its first 53 bits, and
/// the sum is read from it. So the work does not grow with the scales, however far
/// apart they lie.
fn decimal_sum(mut terms: [Term; 12]) -> f64 {
    terms.sort_by_key(|t| t.scale);
    let mut sum = Signed::default();
    let mut unit = terms[0].scale;
    for term in terms {
        while unit < term.scale {
            if sum.magnitude.is_zero() {
                unit = term.scale;
                break;
            }
            if sum.magnitude.at_least_bit(DECIDED) {
                return sum.value(unit);
            }
            sum.magnitude.times_ten();
            unit += 1;
        }
        sum.add(term.negative, term.digits);
    }
    sum.value(unit)
}

/// The bit past which a sum of `decimal_sum` is decided: twelve terms below 2^128
/// stay below 2^132, which is 2^-53 of it.
const DECIDED: u32 = 185;

/// An integer of a sign and a magnitude.
#[derive(Default)]
struct Signed {
    negative: bool,
    magnitude: U256,
}

impl Signed {
    fn add(&mut self, negative: bool, value: u128) {
        let value = U256::from(value);
        if negative == self.negative || self.magnitude.is_zero() {
            self.negative = negative;
            self.magnitude = self.magnitude.plus(&value);
        } else if self.magnitude.at_least(&value) {
            self.magnitude = self.magnitude.minus(&value);
        } else {
            self.negative = negative;
            self.magnitude = value.minus(&self.magnitude);
        }
    }

    /// `self × 10^-unit` rounded: of its sign, and a nonzero one where it is not
    /// zero, the least `f64` where it is smaller.
    fn value(&self, unit: u64) -> f64 {
        if self.magnitude.is_zero() {
            return 0.0;
        }
        let mut value = self.magnitude.to_f64();
        let mut left = unit;
        while left > 0 && value != 0.0 {
            let step = left.min(300);
            value /= 10_f64.powi(step as i32);
            left -= step;
        }
        let value = value.max(f64::from_bits(1));
        if self.negative { -value } else { value }
    }
}

/// An unsigned integer of 256 bits, its lowest 64 first; the sums of
/// `decimal_sum` stay below 2^190.
#[derive(Clone, Copy, Default, PartialEq)]
struct U256([u64; 4]);

impl From<u128> for U256 {
    fn from(v: u128) -> Self {
        Self([v as u64, (v >> 64) as u64, 0, 0])
    }
}

impl U256 {
    fn is_zero(&self) -> bool {
        self.0 == [0; 4]
    }

    fn at_least(&self, other: &Self) -> bool {
        for i in (0..4).rev() {
            if self.0[i] != other.0[i] {
                return self.0[i] > other.0[i];
            }
        }
        true
    }

    fn at_least_bit(&self, bit: u32) -> bool {
        let (limb, within) = ((bit / 64) as usize, bit % 64);
        self.0[limb] >> within != 0 || self.0[limb + 1..].iter().any(|&l| l != 0)
    }

    fn plus(&self, other: &Self) -> Self {
        let mut out = [0; 4];
        let mut carry = false;
        for (i, limb) in out.iter_mut().enumerate() {
            let (s, c1) = self.0[i].overflowing_add(other.0[i]);
            let (s, c2) = s.overflowing_add(u64::from(carry));
            *limb = s;
            carry = c1 || c2;
        }
        debug_assert!(!carry, "sums stay below 2^190");
        Self(out)
    }

    /// `self - other`, `self` at least `other`.
    fn minus(&self, other: &Self) -> Self {
        let mut out = [0; 4];
        let mut borrow = false;
        for (i, limb) in out.iter_mut().enumerate() {
            let (d, b1) = self.0[i].overflowing_sub(other.0[i]);
            let (d, b2) = d.overflowing_sub(u64::from(borrow));
            *limb = d;
            borrow = b1 || b2;
        }
        debug_assert!(!borrow, "the larger less the smaller");
        Self(out)
    }

    fn times_ten(&mut self) {
        let mut carry = 0_u128;
        for limb in &mut self.0 {
            let product = u128::from(*limb) * 10 + carry;
            *limb = product as u64;
            carry = product >> 64;
        }
        debug_assert_eq!(carry, 0, "sums stay below 2^190");
    }

    fn to_f64(self) -> f64 {
        self.0
            .iter()
            .rev()
            .fold(0.0, |value, &limb| value * 2_f64.powi(64) + limb as f64)
    }
}

/// The sum of `terms`, of its exact sign and zero only where it is exactly:
/// the terms are added into an expansion, parts that do not overlap whose sum is
/// exact (Shewchuk, "Adaptive Precision Floating-Point Arithmetic and Fast
/// Robust Geometric Predicates", 1997, `GROW-EXPANSION`), and its two largest
/// parts are added: the smaller lies below the larger's lowest bit, so their sum
/// rounds to a number of the larger's sign, and the parts below them change it
/// by less than its rounding. Finite terms whose sum does not overflow.
fn exact_sum(terms: &[f64]) -> f64 {
    let mut parts: Vec<f64> = Vec::with_capacity(terms.len());
    for &term in terms {
        let mut q = term;
        let mut next = Vec::with_capacity(parts.len() + 1);
        for &part in &parts {
            let (sum, error) = two_sum(q, part);
            if error != 0.0 {
                next.push(error);
            }
            q = sum;
        }
        if q != 0.0 {
            next.push(q);
        }
        parts = next;
    }
    match parts[..] {
        [] => 0.0,
        [only] => only,
        [.., below, top] => top + below,
    }
}

/// `a + b` rounded and its rounding error, exactly (Knuth's `TwoSum`).
fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let sum = a + b;
    let b_part = sum - a;
    let a_part = sum - b_part;
    (sum, (a - a_part) + (b - b_part))
}

#[cfg(test)]
mod tests {
    use super::{Lead, same};
    use hayro_syntax::object::{FromBytes, Number};
    use std::cmp::Ordering;

    fn lead(coords: [&str; 6]) -> Lead {
        Lead::of(coords.map(|c| Number::from_bytes(c.as_bytes()).expect("a number")))
    }

    fn sign(lead: Lead) -> Ordering {
        lead.value().partial_cmp(&0.0).expect("a number")
    }

    /// `±digits / 10^places` written out.
    fn decimal(value: i64, places: u32) -> String {
        let digits = value.unsigned_abs().to_string();
        let digits = format!("{digits:0>width$}", width = places as usize + 1);
        let (whole, fraction) = digits.split_at(digits.len() - places as usize);
        let sign = if value < 0 { "-" } else { "" };
        if places == 0 {
            format!("{sign}{whole}")
        } else {
            format!("{sign}{whole}.{fraction}")
        }
    }

    /// The sign of the coefficient of circles written as decimals agrees with
    /// exact integer arithmetic, near touching and far from it: circles written
    /// touching along an axis and on a 3-4-5 diagonal, at up to six places and
    /// nine digits, and moved apart by one in the last place or by more, each way.
    /// The `f32` reading of these takes a third of the touching ones for cones.
    #[test]
    fn the_circles_of_decimals_lie_as_exact_arithmetic_says() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |below: i64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % below as u64) as i64
        };
        let mut seen = [0; 3];
        for _ in 0..20_000 {
            let places = next(7) as u32;
            let size = 10_i64.pow(next(10) as u32).max(10);
            let x0 = next(2 * size) - size;
            let y0 = next(2 * size) - size;
            let r0 = if next(2) == 0 { 0 } else { next(size) };
            let unit = next(size).max(1);
            let (dx, dy, dr) = if next(2) == 0 {
                (unit, 0, unit)
            } else {
                (3 * unit, 4 * unit, 5 * unit)
            };
            let nudge = [0, 1, -1, 7, -7, unit / 3, -unit / 3][next(7) as usize];
            let coords = [x0, y0, r0, x0 + dx + nudge, y0 + dy, r0 + dr];
            let [x0, y0, r0, x1, y1, r1] = coords.map(i128::from);
            let exact = (x1 - x0).pow(2) + (y1 - y0).pow(2) - (r1 - r0).pow(2);
            let written = coords.map(|c| decimal(c, places));
            let got = lead(written.each_ref().map(String::as_str));
            assert_eq!(sign(got), exact.cmp(&0), "{written:?}");
            assert_eq!(got.cone(), exact > 0, "{written:?}");
            seen[(exact.signum() + 1) as usize] += 1;
        }
        assert!(seen.iter().all(|&n| n > 1000), "{seen:?}");
    }

    /// Coordinates of very different scales are decided exactly too, in steps
    /// that do not grow with the scales: a centre 10^-100 off touching is nested
    /// or a cone by the side it lies on, one apart by 10^-60 from another number
    /// apart by as much touches, and the coefficient's size follows the gap.
    #[test]
    fn far_apart_scales_are_decided_exactly() {
        let tiny = format!("0.{}1", "0".repeat(99));
        let minus_tiny = format!("-{tiny}");
        assert_eq!(sign(lead(["0", "0", "0", "5", "0", "5"])), Ordering::Equal);
        assert_eq!(sign(lead([&tiny, "0", "0", "5", "0", "5"])), Ordering::Less);
        assert_eq!(
            sign(lead([&minus_tiny, "0", "0", "5", "0", "5"])),
            Ordering::Greater
        );
        let small = format!("0.{}3", "0".repeat(59));
        assert_eq!(
            sign(lead([&small, "0", &small, "5", "0", "5"])),
            Ordering::Equal
        );
        let a = lead(["0", "0", "0", "1000000.25", "0", "1000000"]).value();
        assert!((a - 500_000.062_5).abs() < 1e-9, "{a}");
        let a = lead([&minus_tiny, "0", "0", "5", "0", "5"]).value();
        assert!((a / 1e-99 - 1.0).abs() < 1e-12, "{a}");
    }

    /// A real of more digits than 64 bits hold is read as its nearest `f64`, and
    /// circles within the rounding of those readings of touching are taken to
    /// touch; farther apart they are a cone.
    #[test]
    fn long_reals_touch_within_their_reading() {
        let near = "5.0000000000000000000000001";
        assert_eq!(sign(lead(["0", "0", "0", near, "0", "5"])), Ordering::Equal);
        let off = "5.0000000000010000000000001";
        assert_eq!(
            sign(lead(["0", "0", "0", off, "0", "5"])),
            Ordering::Greater
        );
        assert_eq!(sign(lead(["0", "0", "0", "5", "0", off])), Ordering::Less);
    }

    /// Numbers are one value where their decimals are, however written.
    #[test]
    fn same_values() {
        let n = |s: &str| Number::from_bytes(s.as_bytes()).expect("a number");
        assert!(same(n("1"), n("1.000")));
        assert!(same(n("-0"), n("0.0")));
        assert!(!same(n("1000000000"), n("1000000001")));
        assert!(!same(n("0.1"), n("0.10000000000000001")));
    }
}
