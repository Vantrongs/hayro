use crate::object::macros::object;
use crate::object::{Object, ObjectLike};
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt, Skippable};
use crate::trivia::is_regular_character;
use core::fmt::{Debug, Display, Formatter};

#[rustfmt::skip]
static POWERS_OF_10: [f64; 23] = [
    1.0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9,
    1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19,
    1e20, 1e21, 1e22,
];

const MAX_EXACT_MANTISSA: u64 = 1 << 53;
const SIMPLE_DIGITS: usize = 18;

const MAX_PLACES: i64 = 37;
const SIGNIFICANT: u32 = 9;
const LONG: usize = 10;
const MAX_POWER: usize = MAX_PLACES as usize + SIGNIFICANT as usize - 1;

// The viewer divides by a power rounded to 32 significant bits, with ties to even.
const POWERS_OF_TEN_32: [(u64, i32); MAX_POWER + 1] = {
    let mut table = [(0, 0); MAX_POWER + 1];
    let mut k = 0;
    while k <= MAX_POWER {
        // Ten to the `k` is five to the `k` times two to the `k`.
        let five = 5_u128.pow(k as u32);
        let bits = 128 - five.leading_zeros();
        let (mut m, mut shift) = (five, k as i32);
        if bits > 32 {
            let drop = bits - 32;
            m = five >> drop;
            let rest = five & ((1 << drop) - 1);
            let half = 1 << (drop - 1);
            if rest > half || (rest == half && m & 1 == 1) {
                m += 1;
            }
            shift += drop as i32;
            if m == 1 << 32 {
                m >>= 1;
                shift += 1;
            }
        }
        let spare = m.leading_zeros() - 96;
        table[k] = ((m << spare) as u64, shift - spare as i32);
        k += 1;
    }
    table
};

/// A number.
#[derive(Clone, Copy, Debug)]
pub struct Number(pub(crate) InternalNumber);

impl Default for Number {
    fn default() -> Self {
        Self::ZERO
    }
}

impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        match (self.0, other.0) {
            (InternalNumber::Integer(a), InternalNumber::Integer(b)) => a == b,
            (InternalNumber::Integer(_), _) | (_, InternalNumber::Integer(_)) => false,
            _ => self.as_f64() == other.as_f64(),
        }
    }
}

impl Number {
    /// The number zero.
    pub const ZERO: Self = Self::from_i32(0);
    /// The number one.
    pub const ONE: Self = Self::from_i32(1);

    /// Whether this value was read as an integer.
    pub const fn is_integer(&self) -> bool {
        matches!(self.0, InternalNumber::Integer(_))
    }

    /// The sign, unsigned mantissa and decimal places when the exact decimal is retained.
    pub fn as_decimal(self) -> Option<(bool, u64, u32)> {
        match self.0 {
            InternalNumber::Integer(i) => Some((i < 0, i.unsigned_abs(), 0)),
            InternalNumber::Decimal {
                negative,
                digits,
                scale,
                ..
            } => Some((negative, digits, u32::from(scale))),
            InternalNumber::Real { .. } => None,
        }
    }

    /// Returns the number as a f64.
    pub fn as_f64(&self) -> f64 {
        match self.0 {
            InternalNumber::Real { value, .. } => value,
            InternalNumber::Decimal {
                digits,
                scale,
                negative,
                ..
            } => {
                let value = digits as f64 / POWERS_OF_10[usize::from(scale)];
                if negative { -value } else { value }
            }
            InternalNumber::Integer(i) => i as f64,
        }
    }

    /// Returns the number as a f32.
    pub fn as_f32(&self) -> f32 {
        match self.0 {
            InternalNumber::Integer(i) => i as f32,
            _ => self.as_f64() as f32,
        }
    }

    /// Returns the number as an i64.
    pub fn as_i64(&self) -> i64 {
        match self.0 {
            InternalNumber::Integer(i) => i,
            _ => self.as_f64() as i64,
        }
    }

    /// The 32-bit viewer reading of the original decimal spelling, when established.
    pub fn viewer_f32(&self) -> Option<f32> {
        match self.0 {
            InternalNumber::Integer(i) => Some(i as f32),
            InternalNumber::Real { viewer, .. } => (!viewer.is_nan()).then_some(viewer),
            InternalNumber::Decimal {
                digits,
                scale,
                negative,
                long: false,
            } => nine_digits_of_decimal(digits, u32::from(scale), negative),
            InternalNumber::Decimal {
                digits,
                scale,
                negative,
                long: true,
            } => {
                let power = 10_u64.pow(u32::from(scale));
                let mut fraction = digits % power;
                let mut places = [0; SIMPLE_DIGITS];
                for place in places[..usize::from(scale)].iter_mut().rev() {
                    *place = (fraction % 10) as u8;
                    fraction /= 10;
                }
                Some(long_reading(
                    (digits / power) as u32,
                    places[..usize::from(scale)].iter().copied(),
                    negative,
                ))
            }
        }
    }

    /// Create a new `Number` from an f32 number.
    pub const fn from_f32(num: f32) -> Self {
        Self(InternalNumber::Real {
            value: num as f64,
            viewer: num,
        })
    }

    /// Create a new `Number` from an i32 number.
    pub const fn from_i32(num: i32) -> Self {
        Self(InternalNumber::Integer(num as i64))
    }
}

impl Display for Number {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            InternalNumber::Integer(value) => write!(f, "{value}"),
            _ => write!(f, "{:?}", self.as_f64()),
        }
    }
}

impl Skippable for Number {
    fn skip(r: &mut Reader<'_>, _: bool) -> Option<()> {
        Self::read(r, ReaderContext::dummy_ref()).map(|_| ())
    }
}

impl Readable<'_> for Number {
    #[inline]
    fn read(r: &mut Reader<'_>, ctx: &ReaderContext<'_>) -> Option<Self> {
        let start = r.offset();
        Self::read_simple(r).or_else(|| {
            let long = r.offset() - start > SIMPLE_DIGITS;
            r.jump(start);
            if !long {
                return Self::read_fallback(r, false);
            }
            use crate::sync::MutexExt;
            let tail = r.tail()?;
            let cache = ctx.xref().storage().filter(|_| ctx.stored(tail));
            let key = (tail.as_ptr() as usize, tail.len());
            if let Some((len, value)) = cache.and_then(|c| c.numbers.get().get(&key).copied()) {
                r.skip_bytes(len)?;
                return Some(value);
            }
            let value = Self::read_fallback(r, false)?;
            if let Some(cache) = cache
                && r.offset() - start > SIMPLE_DIGITS
            {
                cache.numbers.get().insert(key, (r.offset() - start, value));
            }
            Some(value)
        })
    }
}

impl Number {
    #[inline]
    pub(crate) fn read_fallback(
        r: &mut Reader<'_>,
        allow_operator_after_dot: bool,
    ) -> Option<Self> {
        let start = r.offset();
        read_inner(r, allow_operator_after_dot).or_else(|| {
            r.jump(start);
            None
        })
    }

    #[inline(always)]
    pub(crate) fn read_simple(r: &mut Reader<'_>) -> Option<Self> {
        let start = r.offset();
        let negative = match r.peek_byte() {
            Some(b'-') => {
                r.forward();
                true
            }
            Some(b'+') => {
                r.forward();
                false
            }
            _ => false,
        };

        let mut mantissa: u64 = 0;
        let mut read_digits = |r: &mut Reader<'_>| {
            let start = r.offset();
            while let Some(d) = r
                .peek_byte()
                .map(|b| b.wrapping_sub(b'0'))
                .filter(|d| *d < 10)
            {
                if r.offset() - start > SIMPLE_DIGITS {
                    break;
                }
                mantissa = mantissa.wrapping_mul(10).wrapping_add(d as u64);
                r.forward();
            }
            r.offset() - start
        };

        let mut digits = read_digits(r);
        let whole_len = r.offset() - start;
        let mut decimal_shift = None;
        if r.peek_byte() == Some(b'.') {
            r.forward();
            let fraction_digits = read_digits(r);
            decimal_shift = Some(fraction_digits as u32);
            digits += fraction_digits;
        }

        if digits == 0 || digits > SIMPLE_DIGITS || r.peek_byte().is_some_and(is_regular_character)
        {
            return None;
        }

        Self::from_parts(negative, mantissa, decimal_shift, whole_len >= LONG)
    }

    #[inline(always)]
    fn from_parts(
        negative: bool,
        mantissa: u64,
        decimal_shift: Option<u32>,
        long: bool,
    ) -> Option<Self> {
        match decimal_shift {
            None => {
                let value = mantissa as i64;
                Some(Self(InternalNumber::Integer(if negative {
                    value.wrapping_neg()
                } else {
                    value
                })))
            }
            Some(shift) => {
                if mantissa > MAX_EXACT_MANTISSA || shift > SIMPLE_DIGITS as u32 {
                    return None;
                }

                Some(Self(InternalNumber::Decimal {
                    digits: mantissa,
                    scale: shift as u8,
                    negative,
                    long,
                }))
            }
        }
    }
}

#[inline(always)]
fn read_inner(r: &mut Reader<'_>, allow_operator_after_dot: bool) -> Option<Number> {
    let start_offset = r.offset();
    let (has_sign, negative) = match r.peek_byte()? {
        b'-' => {
            r.forward();
            (true, true)
        }
        b'+' => {
            r.forward();
            (true, false)
        }
        _ => (false, false),
    };

    let mut mantissa: u64 = 0;
    let mut point = None;
    let mut decimal_shift: u32 = 0;
    let mut has_digits = false;
    let mut overflowed = false;
    let mut number_end = None;

    loop {
        match r.peek_byte() {
            Some(b'0'..=b'9') => {
                let d = r.read_byte().unwrap();
                if !overflowed {
                    if let Some(value) = mantissa
                        .checked_mul(10)
                        .and_then(|v| v.checked_add((d - b'0') as u64))
                    {
                        mantissa = value;
                    } else {
                        overflowed = true;
                    }
                }
                has_digits = true;
                if point.is_some() {
                    decimal_shift = decimal_shift.saturating_add(1);
                }
            }
            Some(b'.') if point.is_none() => {
                point = Some(r.offset());
                r.forward();
            }
            // Some weird PDFs have trailing minus in the fraction of number.
            Some(b'-') if has_digits => {
                number_end = Some(r.offset());
                r.forward();
                r.forward_while(is_digit_or_minus);
                break;
            }
            _ => break,
        }
    }

    if !has_digits {
        if (point.is_some() && allow_operator_after_dot)
            || ((point.is_some() || has_sign) && !r.peek_byte().is_some_and(is_regular_character))
        {
            // Treat numbers like just `-`, `+` or `-.` as zero.
            return Some(Number(InternalNumber::Integer(0)));
        }
        return None;
    }

    // See issue 994. Don't accept numbers that are followed by a regular character.
    if r.peek_byte().is_some_and(is_regular_character) {
        return None;
    }

    let integer_overflow = point.is_none() && mantissa > i64::MAX as u64;

    if !overflowed
        && !integer_overflow
        && let Some(number) = Number::from_parts(
            negative,
            mantissa,
            point.map(|_| decimal_shift),
            point.is_some_and(|at| at - start_offset >= LONG),
        )
    {
        return Some(number);
    }

    let end_offset = number_end.unwrap_or_else(|| r.offset());
    let number = core::str::from_utf8(r.range(start_offset..end_offset)?).ok()?;

    if point.is_none()
        && let Ok(value) = number.parse::<i64>()
    {
        return Some(Number(InternalNumber::Integer(value)));
    }

    let value = number.parse::<f64>().ok()?;
    let viewer = point.map_or(f32::NAN, |point| {
        let whole = &r.data[start_offset + usize::from(has_sign)..point];
        let fraction = &r.data[point + 1..end_offset];
        if point - start_offset >= LONG {
            let low = whole.iter().fold(0_u32, |n, d| {
                n.wrapping_mul(10).wrapping_add(u32::from(d - b'0'))
            });
            long_reading(low, fraction.iter().map(|d| d - b'0'), negative)
        } else {
            nine_digits_of_written(whole, fraction, negative).unwrap_or(f32::NAN)
        }
    });
    Some(Number(InternalNumber::Real { value, viewer }))
}

fn nine_digits_of_decimal(digits: u64, scale: u32, negative: bool) -> Option<f32> {
    if digits == 0 {
        return Some(signed_zero(negative));
    }
    let count = digits.ilog10() + 1;
    if i64::from(scale) - i64::from(count) + 1 > MAX_PLACES {
        return None;
    }
    let (m, power) = if count > SIGNIFICANT {
        let dropped = count - SIGNIFICANT;
        (
            digits / 10_u64.pow(dropped),
            i64::from(dropped) - i64::from(scale),
        )
    } else {
        (digits, -i64::from(scale))
    };
    Some(nine_digits(m, power, negative))
}

fn nine_digits_of_written(whole: &[u8], fraction: &[u8], negative: bool) -> Option<f32> {
    let before = whole.len() as i64;
    let mut significant = whole
        .iter()
        .chain(fraction)
        .map(|d| d - b'0')
        .enumerate()
        .skip_while(|&(_, d)| d == 0);
    let Some((first, d)) = significant.next() else {
        return Some(signed_zero(negative));
    };
    // The places after the point the first significant digit stands at.
    if first as i64 + 1 - before > MAX_PLACES {
        return None;
    }
    let mut m = u64::from(d);
    let mut last = first;
    for (at, d) in significant.take(SIGNIFICANT as usize - 1) {
        m = m * 10 + u64::from(d);
        last = at;
    }
    Some(nine_digits(m, before - 1 - last as i64, negative))
}

fn nine_digits(mut m: u64, mut power: i64, negative: bool) -> f32 {
    if m == 0 {
        return signed_zero(negative);
    }
    while m.is_multiple_of(10) {
        m /= 10;
        power += 1;
    }
    let reading = if power >= 0 {
        // At most nine characters before the point: below 10^9, exact in 64 bits.
        (m * 10_u64.pow(power as u32)) as f32
    } else {
        let (p, shift) = POWERS_OF_TEN_32[power.unsigned_abs() as usize];
        divided(m, p, shift)
    };
    if negative { -reading } else { reading }
}

// Two guard bits beyond a float keep the division from rounding twice.
const WIDEN: i32 = 58;

fn divided(m: u64, p: u64, shift: i32) -> f32 {
    let numerator = u128::from(m) << WIDEN;
    let quotient = numerator / u128::from(p);
    let inexact = !numerator.is_multiple_of(u128::from(p));
    // A remainder held in the lowest bit, below where rounding to 24 bits looks,
    // rounds the widened quotient as it rounds the exact one.
    let quotient = quotient as u64 | u64::from(inexact);
    let scale = f64::from_bits(((1023 - WIDEN - shift) as u64) << 52);
    // Rounded once from the integer; scaling by a power of two is exact.
    ((quotient as f32) as f64 * scale) as f32
}

// For a long integer part, the viewer wraps it to 32 bits and accumulates the fraction in f32.
fn long_reading(low: u32, fraction: impl Iterator<Item = u8>, negative: bool) -> f32 {
    let mut numerator = 0.0_f32;
    let mut power = 1.0_f32;
    for digit in fraction {
        numerator = numerator * 10.0 + f32::from(digit);
        power *= 10.0;
    }
    let reading = low as f32 + numerator / power;
    if negative { -reading } else { reading }
}

fn signed_zero(negative: bool) -> f32 {
    if negative { -0.0 } else { 0.0 }
}

object!(Number, Number);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum InternalNumber {
    Real {
        value: f64,
        viewer: f32,
    },
    Decimal {
        digits: u64,
        scale: u8,
        long: bool,
        negative: bool,
    },
    Integer(i64),
}

macro_rules! int_num {
    ($i:ident) => {
        impl Skippable for $i {
            fn skip(r: &mut Reader<'_>, _: bool) -> Option<()> {
                r.forward_if(|b| b == b'+' || b == b'-');
                r.forward_while_1(is_digit)?;

                if r.peek_byte() == Some(b'.') {
                    return None;
                }

                // See issue 994. Don't accept numbers that are followed by a regular character.
                if r.peek_byte().is_some_and(is_regular_character) {
                    return None;
                }

                Some(())
            }
        }

        impl<'a> Readable<'a> for $i {
            fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<$i> {
                r.read::<Number>(ctx)
                    .map(|n| n.as_i64())
                    .and_then(|n| n.try_into().ok())
            }
        }

        impl TryFrom<Object<'_>> for $i {
            type Error = ();

            fn try_from(value: Object<'_>) -> core::result::Result<Self, Self::Error> {
                match value {
                    Object::Number(n) => n.as_i64().try_into().ok().ok_or(()),
                    _ => Err(()),
                }
            }
        }

        impl<'a> ObjectLike<'a> for $i {}
    };
}

int_num!(i32);
int_num!(i64);
int_num!(u32);
int_num!(u16);
int_num!(usize);
int_num!(u8);

impl Skippable for f32 {
    fn skip(r: &mut Reader<'_>, is_content_stream: bool) -> Option<()> {
        r.skip::<Number>(is_content_stream).map(|_| {})
    }
}

impl<'a> Readable<'a> for f32 {
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        r.read::<Number>(ctx).map(|n| n.as_f64() as Self)
    }
}

impl TryFrom<Object<'_>> for f32 {
    type Error = ();

    fn try_from(value: Object<'_>) -> Result<Self, Self::Error> {
        match value {
            Object::Number(n) => Ok(n.as_f64() as Self),
            _ => Err(()),
        }
    }
}

impl ObjectLike<'_> for f32 {}

impl Skippable for f64 {
    fn skip(r: &mut Reader<'_>, is_content_stream: bool) -> Option<()> {
        r.skip::<Number>(is_content_stream).map(|_| {})
    }
}

impl<'a> Readable<'a> for f64 {
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        r.read::<Number>(ctx).map(|n| n.as_f64())
    }
}

impl TryFrom<Object<'_>> for f64 {
    type Error = ();

    fn try_from(value: Object<'_>) -> Result<Self, Self::Error> {
        match value {
            Object::Number(n) => Ok(n.as_f64()),
            _ => Err(()),
        }
    }
}

impl ObjectLike<'_> for f64 {}

pub(crate) fn is_digit(byte: u8) -> bool {
    byte.is_ascii_digit()
}

pub(crate) fn is_digit_or_minus(byte: u8) -> bool {
    is_digit(byte) || byte == b'-'
}

#[cfg(test)]
mod tests {
    use crate::object::Number;
    use crate::reader::Reader;
    use crate::reader::ReaderExt;

    #[test]
    fn viewer_numbers_keep_the_written_fraction() {
        for (spelling, expected) in [
            ("4294967297.4999999", Some(1.4999999)),
            ("600.000030517579125", Some(f32::from_bits(0x4416_0000))),
            ("0.000000152703727", Some(f32::from_bits(0x3423_F6E1))),
            ("4503599627370497.25", Some(1.25)),
            ("-9007199254740993.75", Some(-1.75)),
            ("9223372036854775808.0", Some(0.0)),
            ("9223372036854775808", None),
            ("1000000000000000000000000000000000000000.0", Some(0.0)),
            ("0.00000000000000000000000000000000000001", None),
            (
                "0000000001.9999999999999999999999999999999999999999999",
                None,
            ),
        ] {
            let n = Reader::new(spelling.as_bytes())
                .read_without_context::<Number>()
                .unwrap();
            assert_eq!(n.viewer_f32(), expected, "{spelling}");
        }
    }

    #[test]
    fn display() {
        assert_eq!(format!("{}", Number::from_i32(10)), "10");
        assert_eq!(format!("{}", Number::from_f32(10.0)), "10.0");
        assert_eq!(format!("{}", Number::from_f32(10.5)), "10.5");
    }

    #[test]
    fn int_1() {
        assert_eq!(
            Reader::new("0".as_bytes())
                .read_without_context::<i32>()
                .unwrap(),
            0
        );
    }

    #[test]
    fn int_3() {
        assert_eq!(
            Reader::new("+32".as_bytes())
                .read_without_context::<i32>()
                .unwrap(),
            32
        );
    }

    #[test]
    fn int_4() {
        assert_eq!(
            Reader::new("-32".as_bytes())
                .read_without_context::<i32>()
                .unwrap(),
            -32
        );
    }

    #[test]
    fn int_6() {
        assert_eq!(
            Reader::new("98349".as_bytes())
                .read_without_context::<i32>()
                .unwrap(),
            98349
        );
    }

    #[test]
    fn int_7() {
        assert_eq!(
            Reader::new("003245".as_bytes())
                .read_without_context::<i32>()
                .unwrap(),
            3245
        );
    }

    #[test]
    fn int_min_stays_integer() {
        let number = Reader::new("-9223372036854775808".as_bytes())
            .read_without_context::<Number>()
            .unwrap();
        assert_eq!(number.0, super::InternalNumber::Integer(i64::MIN));
        assert_eq!(number.as_i64(), i64::MIN);
    }

    #[test]
    fn real_1() {
        assert_eq!(
            Reader::new("3".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            3.0
        );
    }

    #[test]
    fn real_3() {
        assert_eq!(
            Reader::new("+32".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            32.0
        );
    }

    #[test]
    fn real_4() {
        assert_eq!(
            Reader::new("-32".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            -32.0
        );
    }

    #[test]
    fn real_5() {
        assert_eq!(
            Reader::new("-32.01".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            -32.01
        );
    }

    #[test]
    fn real_6() {
        assert_eq!(
            Reader::new("-.345".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            -0.345
        );
    }

    #[test]
    fn real_7() {
        assert_eq!(
            Reader::new("-.00143".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            -0.00143
        );
    }

    #[test]
    fn real_8() {
        assert_eq!(
            Reader::new("-12.0013".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            -12.0013
        );
    }

    #[test]
    fn real_9() {
        assert_eq!(
            Reader::new("98349.432534".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            98_349.43
        );
    }

    #[test]
    fn real_10() {
        assert_eq!(
            Reader::new("-34534656.34".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            -34534656.34
        );
    }

    #[test]
    fn skip_number() {
        let mut reader = Reader::new(b"--2");
        assert!(reader.skip::<Number>(false).is_none());
        assert_eq!(reader.offset(), 0);
    }

    #[test]
    fn lone_sign() {
        for input in [
            "+", "-", "+ ", "+(", "+<", "+]", "+/", "- ", "-(", "-<", "-]", "-/",
        ] {
            assert_eq!(
                Reader::new(input.as_bytes()).read_without_context::<i32>(),
                Some(0),
                "{input:?}"
            );
        }

        for input in ["+abc", "-abc"] {
            assert_eq!(
                Reader::new(input.as_bytes()).read_without_context::<i32>(),
                None,
                "{input:?}"
            );
        }
    }

    #[test]
    fn real_failing() {
        assert!(
            Reader::new("+abc".as_bytes())
                .read_without_context::<f32>()
                .is_none()
        );
    }

    #[test]
    fn number_1() {
        assert_eq!(
            Reader::new("+32".as_bytes())
                .read_without_context::<Number>()
                .unwrap()
                .as_f64() as f32,
            32.0
        );
    }

    #[test]
    fn number_2() {
        assert_eq!(
            Reader::new("-32.01".as_bytes())
                .read_without_context::<Number>()
                .unwrap()
                .as_f64() as f32,
            -32.01
        );
    }

    #[test]
    fn number_3() {
        assert_eq!(
            Reader::new("-.345".as_bytes())
                .read_without_context::<Number>()
                .unwrap()
                .as_f64() as f32,
            -0.345
        );
    }

    #[test]
    fn large_number() {
        assert_eq!(
            Reader::new("38359922".as_bytes())
                .read_without_context::<Number>()
                .unwrap()
                .as_i64(),
            38359922
        );
    }

    #[test]
    fn large_number_2() {
        assert_eq!(
            Reader::new("4294966260".as_bytes())
                .read_without_context::<u32>()
                .unwrap(),
            4294966260
        );
    }

    #[test]
    fn twenty_digit_real_does_not_wrap() {
        let got = Reader::new("190.50000762939453225".as_bytes())
            .read_without_context::<Number>()
            .unwrap()
            .as_f64();
        assert_eq!(got, 190.500_007_629_394_53);
    }

    #[test]
    fn nineteen_digit_real_stays_exact() {
        let got = Reader::new("190.5000076293945313".as_bytes())
            .read_without_context::<Number>()
            .unwrap()
            .as_f64();
        assert_eq!(got, 190.500_007_629_394_53);
    }

    #[test]
    fn oversized_integer_uses_float_fallback() {
        let n = Reader::new("9999999999999999999".as_bytes())
            .read_without_context::<Number>()
            .unwrap();
        assert_eq!(n.as_f64(), 9_999_999_999_999_999_999_u64 as f64);
        assert_eq!(n.as_i64(), i64::MAX);
    }

    #[test]
    fn oversized_negative_integer_uses_float_fallback() {
        let n = Reader::new("-1234567890123456789012345".as_bytes())
            .read_without_context::<Number>()
            .unwrap();
        assert_eq!(n.as_f64(), -1.234_567_890_123_456_8e24);
        assert_eq!(n.as_i64(), i64::MIN);
    }

    #[test]
    fn integer_overflow_then_fraction_uses_float_fallback() {
        let got = Reader::new("12345678901234567890123.456".as_bytes())
            .read_without_context::<Number>()
            .unwrap()
            .as_f64();
        assert_eq!(got, 1.234_567_890_123_456_8e22);
    }

    #[test]
    fn f32_max_uses_float_fallback() {
        let got = Reader::new("340282346638528859811704183484516925440".as_bytes())
            .read_without_context::<f32>()
            .unwrap();
        assert_eq!(got, f32::MAX);
    }

    #[test]
    fn float_fallback_ignores_trailing_minus() {
        let input = "1234567890123456789012345-123".as_bytes();
        let mut reader = Reader::new(input);
        let got = reader.read_without_context::<Number>().unwrap().as_f64();
        assert_eq!(got, 1.234_567_890_123_456_8e24);
        assert_eq!(reader.offset(), input.len());
    }

    #[test]
    fn reals_are_correctly_rounded() {
        let tiny = format!("0.{}1", "0".repeat(309));

        for input in ["519188.22107137204", "-0.000123", &tiny] {
            let got = Reader::new(input.as_bytes())
                .read_without_context::<Number>()
                .unwrap()
                .as_f64();
            assert_eq!(got, input.parse::<f64>().unwrap());
        }
    }
}
