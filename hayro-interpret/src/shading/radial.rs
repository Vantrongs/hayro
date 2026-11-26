use hayro_syntax::object::Number;
use kurbo::Point;

/// Local geometry of a radial shading's two circles.
#[derive(Clone, Copy, Debug)]
pub struct RadialCircles {
    pub(crate) p1: Point,
    pub(crate) r: Point,
    pub(crate) dr: f64,
    pub(crate) a: f64,
}

impl RadialCircles {
    pub(super) fn new(coords: [Number; 6]) -> Self {
        let deltas = [0, 1, 2].map(|i| decimal_difference(coords[i + 3], coords[i]));
        let [dx, dy, dr] = [0, 1, 2].map(|i| match deltas[i] {
            Some((n, places)) => n as f64 / 10_f64.powi(places as i32),
            None => coords[i + 3].as_f64() - coords[i].as_f64(),
        });
        let major = dx.abs().max(dy.abs());
        let minor = dx.abs().min(dy.abs());
        let radius = dr.abs();
        let scale = major.max(radius);
        let mut a = (major - radius).mul_add(major + radius, minor * minor);
        if a.abs() <= 8.0 * f64::EPSILON * scale * scale
            && let Some(exact) = decimal_coefficient(deltas)
        {
            a = exact;
        }
        // Treat rounded tangencies as linear only in both authored and normalized coordinates.
        const TANGENCY: f64 = 1e-4;
        if major > 0.0 && radius > 0.0 && a.abs() < TANGENCY && (a / scale / scale).abs() < TANGENCY
        {
            a = 0.0;
        }
        Self {
            p1: Point::new(dx, dy),
            r: Point::new(coords[2].as_f64(), coords[5].as_f64()),
            dr,
            a,
        }
    }
}

fn decimal_difference(end: Number, start: Number) -> Option<(i128, u32)> {
    let end = end.as_decimal()?;
    let start = start.as_decimal()?;
    let places = end.2.max(start.2);
    let aligned = |(negative, digits, scale): (bool, u64, u32)| {
        let n = i128::from(digits);
        let signed = if negative { -n } else { n };
        signed.checked_mul(10_i128.checked_pow(places - scale)?)
    };
    Some((aligned(end)?.checked_sub(aligned(start)?)?, places))
}

fn decimal_coefficient(deltas: [Option<(i128, u32)>; 3]) -> Option<f64> {
    let [Some(x), Some(y), Some(r)] = deltas else {
        return None;
    };
    let places = x.1.max(y.1).max(r.1);
    let square = |(n, p): (i128, u32)| {
        n.checked_mul(10_i128.checked_pow(places - p)?)?
            .checked_pow(2)
    };
    let a = square(x)?
        .checked_sub(square(r)?)?
        .checked_add(square(y)?)?;
    Some(a as f64 / 10_f64.powi((2 * places) as i32))
}
