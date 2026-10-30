//! How the two circles of a radial shading lie, decided on its coordinates as
//! written.

use hayro_syntax::object::Number;

/// The leading coefficient of the quadratic the painter solves for the circle of a
/// radial shading through a point, `|c1 - c0|² - (r1 - r0)²`, of its `/Coords` as
/// written in the file, taken to the precision they are written to: zero where
/// one circle touches the other inside, which the coordinates say wherever some
/// values they round from make the coefficient zero; otherwise the coefficient of
/// the values as written, positive where the circles form a cone and negative
/// where one holds the other apart from it. Every decision between these is made
/// on this sign, the painter's and the extension's alike.
///
/// The array is written to the places of its longest number, `k` digits after
/// its point: a writer printing six places writes 0.500000 as `0.5` and zero as
/// `0`, dropping the zeros, not the precision. So each of the six, integers
/// among them, states its value to within half a unit of that place,
/// `±0.5 × 10^-k`; an array of integers only states its values exactly. So
/// `[-.223151 -.974784 0 0 0 1]`, a focal point a producer rounded to six places
/// onto the circle, touches, though as written it lies 2 × 10^-7 outside, where
/// a cone's further circles would sweep the end's colour over the whole inside;
/// and `[10.3 0 0 50.7 0 40.4]` touches, though in `f32` a third of such draws
/// are a cone an ulp wide. But `[0 0 0 1000000.25 0 1000000]`, which its digits
/// hold to within 10^4 of its coefficient of 5 × 10^5, is a cone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lead(f64);

impl Lead {
    /// Of `/Coords [x0 y0 r0 x1 y1 r1]`. Each number with at most 19 significant
    /// digits is its exact decimal ([`Number::as_decimal`]), and the coefficient's
    /// sign exact ([`decimal_sum`]), as are the signs of the least and the most it
    /// reaches over the values the coordinates round from ([`touching`]). A real of
    /// more digits, or an integer beyond 64 bits, hayro-syntax keeps only as the
    /// nearest `f64`: then each coordinate is read as its nearest `f64`, the
    /// coefficient of those is found exactly ([`exact_sum`]), and circles it lies
    /// within the rounding of those readings ([`READ_ROUNDING`]) and the precision
    /// of the array of touching are taken to touch. The two are one interval but
    /// for its sides: of these readings it is taken as wide on either side as on
    /// its wider one.
    pub(crate) fn of(coords: [Number; 6]) -> Self {
        let decimals = coords.map(|n| n.as_decimal());
        let halves = [half_unit(&coords); 6];
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
            let a = decimal_sum(&terms);
            return Self(if a != 0.0 && touching(&d, &halves, a > 0.0) {
                0.0
            } else {
                a
            });
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
        // How far the coefficient moves over the values the decimals round from:
        // `(S + e)² - S²` along each axis, `S` the distance and `e` its precision.
        let written: f64 = [(0, 3), (1, 4), (2, 5)]
            .into_iter()
            .map(|(i, j)| {
                let half = |k: usize| halves[k].map_or(0.0, nearest_f64_of);
                let e = half(i) + half(j);
                (2.0 * (v[j] - v[i]).abs() + e) * e
            })
            .sum();
        if !a.is_finite() || a.abs() <= size * READ_ROUNDING + written * (1.0 + WRITTEN_ROUNDING) {
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

/// The relative rounding of the dozen operations finding how far a coefficient of
/// `f64` readings moves over the precision of its decimals.
const WRITTEN_ROUNDING: f64 = 16.0 * f64::EPSILON;

/// Half a unit of the last place an array of numbers is written to,
/// `0.5 × 10^-k` for its longest number's `k` digits after the point, as a
/// decimal: the most each of its values may lie from the value it was rounded
/// from. `None` for an array of integers, which is written exactly.
fn half_unit(numbers: &[Number]) -> Option<(bool, u64, u32)> {
    let places = numbers.iter().filter_map(Number::fraction_digits).max()?;
    Some((false, 5, places.saturating_add(1)))
}

/// Whether the coefficient of circles whose coordinates are the decimals `d`,
/// each written to within `halves` of its value, is zero for some values they
/// round from: the least it reaches is at most zero where the coefficient as
/// written is positive (`positive`), the most at least zero where it is negative.
/// Along each axis the distance `s = v1 - v0` lies within `e = h0 + h1` of its
/// value, so its square ranges from `(|s| - e)²`, or zero where `|s| <= e`, to
/// `(|s| + e)²`; the coefficient adds those of the centres' axes and subtracts
/// the radii's. Each bound is a sum of products of two decimals, its sign exact.
fn touching(
    d: &[(bool, u64, u32); 6],
    halves: &[Option<(bool, u64, u32)>; 6],
    positive: bool,
) -> bool {
    let mut terms = Vec::with_capacity(3 * 16);
    for (axis, (i, j)) in [(0, 3), (1, 4), (2, 5)].into_iter().enumerate() {
        let radius = axis == 2;
        let negate = |(negative, digits, scale): (bool, u64, u32)| (!negative, digits, scale);
        let difference = [d[j], negate(d[i])];
        // `|s|`: the difference, negated where it is negative.
        let s = if decimal_sum(&difference.map(Term::of)) < 0.0 {
            difference.map(negate)
        } else {
            difference
        };
        let e = [halves[i], halves[j]];
        // The bound needs `|s| + e` where the centres' distance is most and the
        // radii's least, so where `positive` the radii's most, and the other way.
        let most = radius == positive;
        let mut side: Vec<(bool, u64, u32)> = s.to_vec();
        for h in e.into_iter().flatten() {
            side.push(if most { h } else { negate(h) });
        }
        if !most {
            // `|s| - e`, which counts only where it is not below zero.
            let rest = decimal_sum(&side.iter().map(|&x| Term::of(x)).collect::<Vec<_>>());
            if rest <= 0.0 {
                continue;
            }
        }
        for &p in &side {
            for &q in &side {
                let mut term = Term::times(p, q);
                term.negative ^= radius;
                terms.push(term);
            }
        }
    }
    let bound = decimal_sum(&terms);
    if positive { bound <= 0.0 } else { bound >= 0.0 }
}

/// The `f64` nearest a decimal.
fn nearest_f64_of((negative, digits, scale): (bool, u64, u32)) -> f64 {
    let sign = if negative { "-" } else { "" };
    format!("{sign}{digits}e-{scale}")
        .parse()
        .expect("digits and an exponent read as a float")
}

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
    n.as_decimal().map_or_else(|| n.as_f64(), nearest_f64_of)
}

/// `±digits × 10^-scale`.
#[derive(Clone, Copy)]
struct Term {
    negative: bool,
    digits: u128,
    scale: u64,
}

impl Term {
    fn of((negative, digits, scale): (bool, u64, u32)) -> Self {
        Self {
            negative,
            digits: u128::from(digits),
            scale: u64::from(scale),
        }
    }

    fn times((n0, d0, s0): (bool, u64, u32), (n1, d1, s1): (bool, u64, u32)) -> Self {
        Self {
            negative: n0 ^ n1,
            digits: u128::from(d0) * u128::from(d1),
            scale: u64::from(s0) + u64::from(s1),
        }
    }
}

/// The sum of `terms`, rounded: of the exact sum's sign, and zero only where it is
/// zero. The terms are added from the coarsest scale to the finest into an integer
/// in units of the scale reached, scaled by ten as each finer one comes; once that
/// integer is past `DECIDED`, the finer terms, each below 2^128 in its units and so
/// below them in the integer's, cannot change its sign or its first 53 bits, and
/// the sum is read from it. So the work does not grow with the scales, however far
/// apart they lie.
fn decimal_sum(terms: &[Term]) -> f64 {
    assert!(terms.len() <= MOST_TERMS, "at most {MOST_TERMS} terms");
    if terms.is_empty() {
        return 0.0;
    }
    let mut terms = terms.to_vec();
    terms.sort_by_key(|t| t.scale);
    let mut sum = Signed::default();
    let mut unit = terms[0].scale;
    for term in &terms {
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

/// The most terms `decimal_sum` adds: three squares of four.
const MOST_TERMS: usize = 64;

/// The bit past which a sum of `decimal_sum` is decided: `MOST_TERMS` terms below
/// 2^128 stay below 2^134, which is 2^-53 of it.
const DECIDED: u32 = 187;

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
/// `decimal_sum` stay below 2^192.
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
        debug_assert!(!carry, "sums stay below 2^192");
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
        debug_assert_eq!(carry, 0, "sums stay below 2^192");
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

    /// The coefficient's least and most over values `e` either way of `s` along
    /// each of the centres' axes and the radii's, in integers.
    fn reach(s: [i128; 3], e: [i128; 3]) -> (i128, i128) {
        let square = |s: i128, e: i128| {
            let s = s.abs();
            (if s > e { (s - e).pow(2) } else { 0 }, (s + e).pow(2))
        };
        let [x, y, r] = [0, 1, 2].map(|i| square(s[i], e[i]));
        (x.0 + y.0 - r.1, x.1 + y.1 - r.0)
    }

    /// The sign of the coefficient of circles written as decimals agrees with
    /// exact integer arithmetic on the values they round from, near touching and
    /// far from it: circles written touching along an axis and on a 3-4-5
    /// diagonal, as integers or at up to six places and nine digits, and moved
    /// apart by one in the last place, which their digits do not tell from
    /// touching, or by more, each way; about half the numbers written with their
    /// trailing zeros dropped, as writers drop them, so the array's places are
    /// its longest number's. The `f32` reading of these takes a third of the
    /// touching ones for cones.
    #[test]
    fn the_circles_of_decimals_lie_as_exact_arithmetic_says() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |below: i64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % below as u64) as i64
        };
        // Nested, touching, a cone, and touching though not as written.
        let mut seen = [0; 4];
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
            let written = coords.map(|c| {
                let full = decimal(c, places);
                if full.contains('.') && next(2) == 0 {
                    full.trim_end_matches('0').trim_end_matches('.').to_string()
                } else {
                    full
                }
            });
            let longest = written
                .iter()
                .filter_map(|t| t.split_once('.').map(|(_, f)| f.len() as u32))
                .max();
            // In half units of the last of `places`, each coordinate's precision
            // half a unit of the longest number's last place, none where all are
            // integers.
            let [x0, y0, r0, x1, y1, r1] = coords.map(|c| 2 * i128::from(c));
            let half = longest.map_or(0, |k| 10_i128.pow(places - k));
            let (least, most) = reach([x1 - x0, y1 - y0, r1 - r0], [2 * half; 3]);
            let exact = (x1 - x0).pow(2) + (y1 - y0).pow(2) - (r1 - r0).pow(2);
            let expected = if least <= 0 && most >= 0 {
                0
            } else {
                exact.signum()
            };
            let got = lead(written.each_ref().map(String::as_str));
            assert_eq!(sign(got), expected.cmp(&0), "{written:?}");
            assert_eq!(got.cone(), expected > 0, "{written:?}");
            seen[(expected + 1) as usize] += 1;
            if expected == 0 && exact != 0 {
                seen[3] += 1;
            }
        }
        assert!(seen.iter().all(|&n| n > 1000), "{seen:?}");
    }

    /// Coordinates of very different scales are decided exactly too, in steps
    /// that do not grow with the scales: a centre 10^-100 off touching, written to
    /// a place finer, is nested or a cone by the side it lies on, one apart by
    /// 10^-60 from another number apart by as much touches, and the coefficient's
    /// size follows the gap.
    #[test]
    fn far_apart_scales_are_decided_exactly() {
        let tiny = format!("0.{}10", "0".repeat(99));
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

    /// Circles touch where their coordinates' digits do not tell them from
    /// touching, each to half a unit of the last place of the array's longest
    /// number: focal points a producer rounded to five and six places onto the
    /// circle, either side of it, and circles of two places a hundredth off,
    /// touch, the integers among them as imprecise as the rest; where the
    /// longest number has seven or eight places, the same focal points 10^-7 off
    /// lie as written, as do circles of two places five hundredths off. An array
    /// of integers is exact, so the same digits as integers are a cone or nested
    /// as written; and `1000000.25` from `1000000` is a cone, two places moving
    /// the coefficient of 5 × 10^5 by 4 × 10^4 at most. A point after a whole
    /// number states it to half a unit.
    #[test]
    fn circles_touch_within_the_precision_they_are_written_to() {
        let cases: [([&str; 6], Ordering); 17] = [
            (
                ["-.223151", "-.974784", "0", "0", "0", "1"],
                Ordering::Equal,
            ),
            (["-.28201", "-.959412", "0", "0", "0", "1"], Ordering::Equal),
            (["-.891111", ".453785", "0", "0", "0", "1"], Ordering::Equal),
            (["-.9999", ".0141175", "0", "0", "0", "1"], Ordering::Less),
            (
                ["-.999995", "-.00323254", "0", "0", "0", "1"],
                Ordering::Greater,
            ),
            (
                ["-.999999", "-.00148365", "0", "0", "0", "1"],
                Ordering::Greater,
            ),
            (["0", "0", "0", "3.01", "4", "5.01"], Ordering::Equal),
            (["0", "0", "0", "3", "4.01", "5.01"], Ordering::Equal),
            (["0", "0", "0", "3.01", "4", "5"], Ordering::Equal),
            (["0", "0", "0", "3", "4", "4.99"], Ordering::Equal),
            (["0", "0", "0", "3.05", "4", "5"], Ordering::Greater),
            (["0", "0", "0", "2.95", "4", "5"], Ordering::Less),
            (["0", "0", "0", "3", "4", "5.6"], Ordering::Less),
            (
                ["-223151", "-974784", "0", "0", "0", "1000000"],
                Ordering::Greater,
            ),
            (
                ["-28201", "959412", "0", "0", "0", "1000000"],
                Ordering::Less,
            ),
            (
                ["0", "0", "0", "1000000.25", "0", "1000000"],
                Ordering::Greater,
            ),
            (
                ["0", "0", "0", "1000000", "0", "1000000.25"],
                Ordering::Less,
            ),
        ];
        for (coords, expected) in cases {
            assert_eq!(sign(lead(coords)), expected, "{coords:?}");
        }
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
