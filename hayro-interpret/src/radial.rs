//! How the two circles of a radial shading lie, decided on its coordinates as
//! written.

use hayro_syntax::object::Number;

/// The leading coefficient of the quadratic the painter solves for the circle of a
/// radial shading through a point, `|c1 - c0|² - (r1 - r0)²` of its `/Coords` as
/// written in the file, and whether the circles touch: positive where they form a
/// cone, negative where one holds the other apart from it, and touching where the
/// coordinates do not tell these apart. Every decision between the three is made
/// here, the painter's ([`coefficient`](Self::coefficient)) and the extension's
/// ([`cone`](Self::cone)) alike.
///
/// A real in PDF holds about five significant decimal digits in its fractional
/// part (ISO 32000-1, Annex C, Table C.1, "Number of significant decimal digits
/// of precision in fractional part (approximate)"), whatever digits a writer
/// prints. That is read two ways, and the circles touch only where neither
/// tells them from touching: absolutely, each of the six, integers among them,
/// states its value to within half a unit of the fifth place, `±0.5 × 10^-5`;
/// and relatively, to within `±0.5 × 10^-5` of its own size, as five
/// significant digits do. Under each, the circles touch wherever some values
/// within it of the coordinates make the coefficient zero. So
/// `[-.223151 -.974784 0 0 0 1]`, a focal point a producer rounded to six places
/// onto the circle, touches, though as written it lies 2 × 10^-7 outside, where a
/// cone's further circles would sweep the end's colour over the whole inside; but
/// `[0 0 0 1000000.25 0 1000000]`, whose coefficient of 5 × 10^5 the absolute
/// moves change by 40 at most, is a cone, and `[0 0 0 0 0 .1]` is a disc. The
/// relative reading alone does not change when the coordinates and the
/// shading's matrix trade a power of ten, so circles whose coefficient lies
/// beyond what it moves are decided alike at every scale:
/// `[0 0 0 .00002 0 .00001]` drawn ten million times larger is the cone
/// `[0 0 0 200 0 100]` is, though the absolute reading alone would take it for
/// touching. The bounds are the same for every writer, unlike the places one
/// prints, which a writer that drops trailing zeros does not show.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lead {
    /// The coefficient of the coordinates as written: of its exact sign, and
    /// zero only where it is zero.
    exact: f64,
    touching: bool,
}

/// A decimal as written: whether it is negative, its digits, and its places
/// after the point, `digits × 10^-scale`.
type Decimal = (bool, u64, u32);

/// Half a unit of the fifth place after the point, as a decimal: how far a
/// coordinate may lie from the value it states, read absolutely (`Lead`).
const HALF_UNIT: Decimal = (false, 5, 6);

/// How far the decimal `value` may lie from the value it states, read
/// absolutely: [`HALF_UNIT`].
fn absolute(_: Decimal) -> Decimal {
    HALF_UNIT
}

/// How far the decimal `value` may lie from the value it states, read
/// relatively: `0.5 × 10^-5` of its size, exactly; for a decimal of more than
/// 18 digits, which five times would not hold in 64 bits, half its digits
/// rounded up a place further, larger by less than 10^-18 of itself.
fn relative((_, digits, scale): Decimal) -> Decimal {
    match digits.checked_mul(5) {
        Some(five) => (false, five, scale + 6),
        None => (false, digits.div_ceil(2), scale + 5),
    }
}

impl Lead {
    /// Of `/Coords [x0 y0 r0 x1 y1 r1]`. Each number with at most 19 significant
    /// digits is its exact decimal ([`Number::as_decimal`]), and the coefficient's
    /// sign exact ([`decimal_sum`]), as are the signs of the least and the most it
    /// reaches over the values within either reading of the coordinates'
    /// precision ([`absolute`], [`relative`]) of them ([`touching`]). A real of more digits, or an integer beyond 64 bits,
    /// hayro-syntax keeps only as the nearest `f64`: then each coordinate is read as
    /// its nearest `f64`, the coefficient of those is found exactly ([`exact_sum`]),
    /// and circles it lies within the rounding of those readings
    /// ([`READ_ROUNDING`]) and the precision of PDF reals of touching are taken to
    /// touch. The two are one interval but for its sides: of these readings it is
    /// taken as wide on either side as on its wider one.
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
            let exact = decimal_sum(&terms);
            return Self {
                exact,
                touching: exact == 0.0
                    || touching(&d, exact > 0.0, absolute) && touching(&d, exact > 0.0, relative),
            };
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
        let exact = exact_sum(&parts);
        // How far the coefficient moves over the values the coordinates state:
        // `(S + e)² - S²` along each axis, `S` the distance and `e` its precision,
        // read absolutely and relatively.
        let half = nearest_f64_of(HALF_UNIT);
        let moves = |e: &dyn Fn(usize, usize) -> f64| -> f64 {
            [(0, 3), (1, 4), (2, 5)]
                .into_iter()
                .map(|(i, j)| (2.0 * (v[j] - v[i]).abs() + e(i, j)) * e(i, j))
                .sum()
        };
        let within =
            |written: f64| exact.abs() <= size * READ_ROUNDING + written * (1.0 + WRITTEN_ROUNDING);
        let absolutely = moves(&|_, _| 2.0 * half);
        let relatively = moves(&|i, j| half * (v[i].abs() + v[j].abs()));
        Self {
            exact,
            touching: !exact.is_finite() || within(absolutely) && within(relatively),
        }
    }

    /// The leading coefficient the painter solves with at a point where the
    /// linear one is `b`: zero where the circles touch, so the one root of the
    /// line through them is taken, as the coordinates mean; but where `b` is zero
    /// too, as at every point of concentric circles from a point, that line holds
    /// no root, and the coefficient as written is taken, so a point a root of it
    /// paints is painted.
    pub(crate) fn coefficient(self, b: f64) -> f64 {
        if self.touching && b != 0.0 {
            0.0
        } else {
            self.exact
        }
    }

    /// Whether the circles form a cone: every point within the circle ending it
    /// then lies on a circle past that end too. Where they do not, one circle
    /// holds the other, or they touch, and a point within them lies on one of the
    /// circles between.
    pub fn cone(self) -> bool {
        !self.touching && self.exact > 0.0
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
/// `f64` readings moves over the precision of PDF reals.
const WRITTEN_ROUNDING: f64 = 16.0 * f64::EPSILON;

/// Whether the coefficient of circles whose coordinates are the decimals `d`,
/// each stating its value to within `bound` of it, is zero for some values they
/// state: the least it reaches is at most zero where the coefficient as written is
/// positive (`positive`), the most at least zero where it is negative. Along each
/// axis the distance `s = v1 - v0` lies within `e = bound(v0) + bound(v1)` of its value, so
/// its square ranges from `(|s| - e)²`, or zero where `|s| <= e`, to `(|s| + e)²`;
/// the coefficient adds those of the centres' axes and subtracts the radii's.
/// Each bound is a sum of products of two decimals, its sign exact.
fn touching(d: &[Decimal; 6], positive: bool, bound: fn(Decimal) -> Decimal) -> bool {
    let mut terms = Vec::with_capacity(3 * 16);
    for (axis, (i, j)) in [(0, 3), (1, 4), (2, 5)].into_iter().enumerate() {
        let radius = axis == 2;
        let negate = |(negative, digits, scale): Decimal| (!negative, digits, scale);
        let difference = [d[j], negate(d[i])];
        // `|s|`: the difference, negated where it is negative.
        let s = if decimal_sum(&difference.map(Term::of)) < 0.0 {
            difference.map(negate)
        } else {
            difference
        };
        // The bound needs `|s| + e` where the centres' distance is most and the
        // radii's least, so where `positive` the radii's most, and the other way.
        let most = radius == positive;
        let mut side: Vec<Decimal> = s.to_vec();
        for h in [bound(d[i]), bound(d[j])] {
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
fn nearest_f64_of((negative, digits, scale): Decimal) -> f64 {
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
fn normal((negative, mut digits, mut scale): Decimal) -> Decimal {
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
    fn of((negative, digits, scale): Decimal) -> Self {
        Self {
            negative,
            digits: u128::from(digits),
            scale: u64::from(scale),
        }
    }

    fn times((n0, d0, s0): Decimal, (n1, d1, s1): Decimal) -> Self {
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

    /// How the circles lie: `Equal` where they touch, else the coefficient's sign.
    fn sign(lead: Lead) -> Ordering {
        if lead.touching {
            Ordering::Equal
        } else {
            lead.exact.partial_cmp(&0.0).expect("a number")
        }
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

    /// How the circles lie agrees with exact integer arithmetic on the values
    /// within half a unit of the fifth place of the coordinates, near touching
    /// and far from it: circles written touching along an axis and on a 3-4-5
    /// diagonal, as integers or at up to eight places and eleven digits, and moved
    /// apart by one in the last place, which the precision of PDF reals does not
    /// tell from touching, or by more, each way; about half the numbers written
    /// with their trailing zeros dropped, which changes nothing.
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
        let mut undecided = 0;
        for _ in 0..20_000 {
            let places = next(9) as u32;
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
            // In units of the tenth of a millionth or of the last of `places`,
            // the finer: half a unit of the fifth place is five tenths of a
            // millionth.
            let fine = places.max(6);
            let [x0, y0, r0, x1, y1, r1] =
                coords.map(|c| i128::from(c) * 10_i128.pow(fine - places));
            let half = 5 * 10_i128.pow(fine - 6);
            let (least, most) = reach([x1 - x0, y1 - y0, r1 - r0], [2 * half; 3]);
            let exact = (x1 - x0).pow(2) + (y1 - y0).pow(2) - (r1 - r0).pow(2);
            let Some(relatively) = relatively_touching([x0, y0, r0, x1, y1, r1]) else {
                undecided += 1;
                continue;
            };
            let expected = if least <= 0 && most >= 0 && relatively {
                0
            } else {
                exact.signum()
            };
            let got = lead(written.each_ref().map(String::as_str));
            assert_eq!(sign(got), expected.cmp(&0), "{written:?}");
            assert_eq!(got.cone(), expected > 0, "{written:?}");
            assert_eq!(
                got.exact.partial_cmp(&0.0),
                Some(exact.cmp(&0)),
                "{written:?}"
            );
            seen[(expected + 1) as usize] += 1;
            if expected == 0 && exact != 0 {
                seen[3] += 1;
            }
        }
        assert!(seen.iter().all(|&n| n > 1000), "{seen:?}");
        assert!(undecided < 20, "{undecided}");
    }

    /// Whether the coefficient of circles of the integer coordinates `c` is zero
    /// for some values within `0.5 × 10^-5` of each coordinate's size, found in
    /// `f64`; `None` where its rounding could change the answer. Each axis's
    /// distance `s` moves by `e = (|c0| + |c1|) / 200000`, so its square, times
    /// 200000², ranges over `(200000 |s| ∓ (|c0| + |c1|))²`.
    fn relatively_touching(c: [i128; 6]) -> Option<bool> {
        let mut least = 0.0_f64;
        let mut most = 0.0_f64;
        let mut size = 0.0_f64;
        for (axis, (i, j)) in [(0, 3), (1, 4), (2, 5)].into_iter().enumerate() {
            let s = 200_000.0 * (c[j] - c[i]).abs() as f64;
            let e = (c[i].abs() + c[j].abs()) as f64;
            let (low, high) = (if s > e { (s - e).powi(2) } else { 0.0 }, (s + e).powi(2));
            size += high;
            if axis == 2 {
                least -= high;
                most -= low;
            } else {
                least += low;
                most += high;
            }
        }
        let margin = size * 1e-12;
        if least.abs() <= margin || most.abs() <= margin {
            return None;
        }
        Some(least <= 0.0 && most >= 0.0)
    }

    /// Circles whose coefficient lies beyond what moves within `0.5 × 10^-5` of
    /// each coordinate's size change, a cone or nested, are decided alike when
    /// their coordinates are written ten to the `k` times larger, as a shading
    /// whose matrix is ten to the `k` times smaller draws them, for `k` from
    /// -6 to 6: never touching, of the coefficient's sign. The absolute reading
    /// alone would take the smallest of them for touching.
    #[test]
    fn circles_beyond_the_relative_precision_lie_alike_at_every_scale() {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move |below: i64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % below as u64) as i64
        };
        let mut checked = 0;
        let mut small = 0;
        for _ in 0..4000 {
            let size = 10_i64.pow(1 + next(6) as u32);
            let places = next(4) as u32;
            let c: [i64; 6] = [
                next(2 * size) - size,
                next(2 * size) - size,
                next(size),
                next(2 * size) - size,
                next(2 * size) - size,
                next(size),
            ];
            let wide = c.map(i128::from);
            if relatively_touching(wide) != Some(false) {
                continue;
            }
            let exact = (wide[3] - wide[0]).pow(2) + (wide[4] - wide[1]).pow(2)
                - (wide[5] - wide[2]).pow(2);
            for k in -6_i32..=6 {
                // `c × 10^(k - places)`, written out.
                let shift = places as i32 - k;
                let written = c.map(|v| {
                    if shift >= 0 {
                        decimal(v, shift as u32)
                    } else {
                        decimal(v * 10_i64.pow((-shift) as u32), 0)
                    }
                });
                let got = lead(written.each_ref().map(String::as_str));
                assert_eq!(sign(got), exact.cmp(&0), "{written:?}");
                if shift >= 6 {
                    small += 1;
                }
                checked += 1;
            }
        }
        assert!(checked > 20_000 && small > 1000, "{checked} {small}");
    }

    /// Coordinates of very different scales are decided exactly too, in steps
    /// that do not grow with the scales: centres 10^-100 off touching touch, and
    /// their coefficient as written keeps its sign and size; circles 10^-3 apart
    /// whose other numbers have a hundred places are a cone or nested by the
    /// side they lie on.
    #[test]
    fn far_apart_scales_are_decided_exactly() {
        let tiny = format!("0.{}10", "0".repeat(99));
        let minus_tiny = format!("-{tiny}");
        assert_eq!(sign(lead(["0", "0", "0", "5", "0", "5"])), Ordering::Equal);
        let touching = lead([&minus_tiny, "0", "0", "5", "0", "5"]);
        assert_eq!(sign(touching), Ordering::Equal);
        assert!((touching.exact / 1e-99 - 1.0).abs() < 1e-12, "{touching:?}");
        assert!(lead([&tiny, "0", "0", "5", "0", "5"]).exact < 0.0);
        assert_eq!(
            sign(lead([&tiny, "0", "0", "5.001", "0", "5"])),
            Ordering::Greater
        );
        assert_eq!(
            sign(lead([&tiny, "0", &tiny, "5", "0", "5.001"])),
            Ordering::Less
        );
        let a = lead(["0", "0", "0", "1000000.25", "0", "1000000"]).exact;
        assert!((a - 500_000.062_5).abs() < 1e-9, "{a}");
    }

    /// Circles touch where moves of each coordinate within half a unit of the
    /// fifth place, and within half of 10^-5 of its size, both make them touch,
    /// however many places they are written to: focal points a producer rounded
    /// to six to eight places onto the circle, either side of it, touch; circles
    /// of two places a hundredth off, a cone `1000000.25` from `1000000`, and the
    /// same digits as integers, lie as written, as do discs of short numbers,
    /// `[0 0 0 0 0 .1]`, which the precision of its digits alone would take for
    /// touching, and of a millionth, which the absolute reading alone would; and
    /// `[0 0 0 .00002 0 .00001]` is the cone `[0 0 0 200 0 100]` is.
    #[test]
    fn circles_touch_within_the_precision_of_pdf_reals() {
        let cases: [([&str; 6], Ordering); 25] = [
            (
                ["-.223151", "-.974784", "0", "0", "0", "1"],
                Ordering::Equal,
            ),
            (["-.28201", "-.959412", "0", "0", "0", "1"], Ordering::Equal),
            (["-.891111", ".453785", "0", "0", "0", "1"], Ordering::Equal),
            (["-.9999", ".0141175", "0", "0", "0", "1"], Ordering::Equal),
            (
                ["-.999995", "-.00323254", "0", "0", "0", "1"],
                Ordering::Equal,
            ),
            (
                ["-.999999", "-.00148365", "0", "0", "0", "1"],
                Ordering::Equal,
            ),
            (["0", "0", "0", "3", "4", "5"], Ordering::Equal),
            (["10.3", "0", "0", "50.7", "0", "40.4"], Ordering::Equal),
            (["0", "0", "0", "3.000001", "4", "5"], Ordering::Equal),
            (["0", "0", "0", "3.01", "4", "5.01"], Ordering::Less),
            (["0", "0", "0", "3.01", "4", "5"], Ordering::Greater),
            (["0", "0", "0", "3", "4", "4.99"], Ordering::Greater),
            (["0", "0", "0", "2.99", "4", "5"], Ordering::Less),
            (
                ["-262.42", "0", "0", "-181.921951", "0", "80.49"],
                Ordering::Greater,
            ),
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
            (["0", "0", "0", "0", "0", ".1"], Ordering::Less),
            (["0", "0", "0", "0", "0", "1"], Ordering::Less),
            (["0", "0", "0", "0", "0", ".000001"], Ordering::Less),
            ([".1", "0", "0", "0", "0", ".2"], Ordering::Less),
            (["0", "0", "0", ".00002", "0", ".00001"], Ordering::Greater),
            (["0", "0", "0", "200", "0", "100"], Ordering::Greater),
            (["0", "0", "1", "0", "0", "1.000001"], Ordering::Equal),
        ];
        for (coords, expected) in cases {
            assert_eq!(sign(lead(coords)), expected, "{coords:?}");
        }
    }

    /// The painter's coefficient is zero for touching circles, but the one as
    /// written where the line through them holds no root: concentric circles
    /// whose radii lie closer than the precision of PDF reals touch, and at each
    /// point the coefficient is theirs; nested and cone circles keep theirs.
    #[test]
    fn touching_circles_with_no_line_keep_their_coefficient() {
        let disc = lead(["0", "0", "1", "0", "0", "1.000001"]);
        assert_eq!(disc.coefficient(1.0), 0.0);
        assert!((disc.coefficient(0.0) + 1e-12).abs() < 1e-24, "{disc:?}");
        let nested = lead(["0", "0", "0", "0", "0", ".1"]);
        assert!((nested.coefficient(0.0) + 0.01).abs() < 1e-15);
        assert!((nested.coefficient(1.0) + 0.01).abs() < 1e-15);
        let exactly = lead(["0", "0", "0", "3", "4", "5"]);
        assert_eq!(
            (exactly.coefficient(0.0), exactly.coefficient(1.0)),
            (0.0, 0.0)
        );
    }

    /// A real of more digits than 64 bits hold is read as its nearest `f64`, and
    /// circles within the rounding of those readings and the precision of PDF
    /// reals of touching are taken to touch; farther apart they are a cone or
    /// nested.
    #[test]
    fn long_reals_touch_within_their_reading() {
        let near = "5.0000000000000000000000001";
        assert_eq!(sign(lead(["0", "0", "0", near, "0", "5"])), Ordering::Equal);
        let within = "5.0000010000000000000000001";
        assert_eq!(
            sign(lead(["0", "0", "0", within, "0", "5"])),
            Ordering::Equal
        );
        let off = "5.0010000000000000000000001";
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
