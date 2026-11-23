//! Classification of the two circles of a radial shading.

use hayro_syntax::object::Number;

/// Local geometry and degree of a radial shading.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Circles {
    pub(crate) p1: kurbo::Point,
    pub(crate) r: kurbo::Point,
    pub(crate) dr: f64,
    pub(crate) a: f64,
}

impl Circles {
    pub(crate) fn of(coords: [Number; 6]) -> Self {
        let [dx, dy, dr] = [0, 1, 2].map(|i| difference(coords[i + 3], coords[i]));
        let major = dx.abs().max(dy.abs());
        let minor = dx.abs().min(dy.abs());
        let radius = dr.abs();
        let coefficient = |x: f64, y: f64, r: f64| (x - r).mul_add(x + r, y * y);
        let mut a = coefficient(major, minor, radius);
        let scale = major.max(radius);
        if a.abs() <= 8.0 * f64::EPSILON * scale * scale
            && let Some(exact) = decimal_coefficient(coords)
        {
            a = exact;
        }
        if major > 0.0 && radius > 0.0 && a.abs() < 1e-4 && (a / scale / scale).abs() < 1e-4 {
            a = 0.0;
        }
        Self {
            p1: kurbo::Point::new(dx, dy),
            r: kurbo::Point::new(nearest_f64(coords[2]), nearest_f64(coords[5])),
            dr,
            a,
        }
    }

    /// Whether later extended circles can paint over the end circle.
    pub fn cone(self) -> bool {
        self.a > 0.0
    }
}

fn decimal_coefficient(coords: [Number; 6]) -> Option<f64> {
    let decimals = coords.map(|n| n.as_decimal());
    let scale = decimals.iter().flatten().map(|d| d.2).max()?;
    let mut integers = [0_i128; 6];
    for (value, decimal) in integers.iter_mut().zip(decimals) {
        let (negative, digits, places) = decimal?;
        *value = i128::from(digits).checked_mul(10_i128.checked_pow(scale - places)?)?;
        if negative {
            *value = -*value;
        }
    }
    let squared = |i: usize| integers[i + 3].checked_sub(integers[i])?.checked_pow(2);
    let coefficient = squared(0)?
        .checked_sub(squared(2)?)?
        .checked_add(squared(1)?)?;
    Some(coefficient as f64 / 10.0_f64.powi((2 * scale) as i32))
}

fn difference(a: Number, b: Number) -> f64 {
    if let (Some((na, da, sa)), Some((nb, db, sb))) = (a.as_decimal(), b.as_decimal()) {
        let scale = sa.max(sb);
        let aligned = |negative: bool, digits: u64, places: u32| {
            let signed = if negative {
                -i128::from(digits)
            } else {
                i128::from(digits)
            };
            signed.checked_mul(10_i128.checked_pow(scale - places)?)
        };
        if let Some(delta) = aligned(na, da, sa)
            .zip(aligned(nb, db, sb))
            .and_then(|(a, b)| a.checked_sub(b))
        {
            if scale <= 22 && delta.unsigned_abs() <= 1 << 53 {
                return delta as f64 / 10.0_f64.powi(scale as i32);
            }
            return format!("{delta}e-{scale}")
                .parse()
                .expect("decimal difference");
        }
    }
    nearest_f64(a) - nearest_f64(b)
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

#[cfg(test)]
mod tests {
    use super::{Circles, same};
    use hayro_syntax::object::{FromBytes, Number};
    use std::cmp::Ordering;

    fn circles(coords: [&str; 6]) -> Circles {
        Circles::of(coords.map(|c| Number::from_bytes(c.as_bytes()).expect("a number")))
    }

    fn sign(circles: Circles) -> Ordering {
        circles.a.partial_cmp(&0.0).expect("a number")
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
            let [x0, y0, r0, x1, y1, r1] = coords.map(i128::from);
            let dc2 = (x1 - x0).pow(2) + (y1 - y0).pow(2);
            let dr2 = (r1 - r0).pow(2);
            let exact = dc2 - dr2;
            let expected = if exact.abs() * 10000 < 10_i128.pow(2 * places)
                && exact.abs() * 10000
                    < (x1 - x0)
                        .abs()
                        .max((y1 - y0).abs())
                        .max((r1 - r0).abs())
                        .pow(2)
            {
                0
            } else {
                exact.signum()
            };
            let got = circles(written.each_ref().map(String::as_str));
            assert_eq!(sign(got), expected.cmp(&0), "{written:?}");
            assert_eq!(got.cone(), expected > 0, "{written:?}");
            seen[(expected + 1) as usize] += 1;
            if expected == 0 && exact != 0 {
                seen[3] += 1;
            }
        }
        assert!(seen.iter().all(|&n| n > 0), "{seen:?}");
    }

    #[test]
    fn radial_classification_in_authored_and_unit_coordinates() {
        for coords in [
            ["0", "0", "0", "1000", "1", "1000"],
            ["0", "0", "0", "1000000.25", "0", "1000000"],
            ["0", "0", "0", "0.002", "0", "0.001"],
        ] {
            assert!(circles(coords).cone());
        }
        for radius in ["0.1", "0.100"] {
            assert_eq!(
                sign(circles(["0", "0", "0", "0", "0", radius])),
                Ordering::Less
            );
        }
        for coords in [
            ["-.223151", "-.974784", "0", "0", "0", "1"],
            ["-.28201", "-.959412", "0", "0", "0", "1"],
            [
                "1000000000000000000",
                "0",
                "0",
                "1000000000000000001",
                "0",
                "1",
            ],
        ] {
            assert_eq!(sign(circles(coords)), Ordering::Equal);
        }
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
