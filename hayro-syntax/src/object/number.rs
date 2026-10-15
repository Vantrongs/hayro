//! Numbers.

use crate::math::{powi_f64, trunc_f64};
use crate::object::macros::object;
use crate::object::{Object, ObjectLike};
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt, Skippable};
use crate::trivia::is_regular_character;
use core::fmt::{Debug, Display, Formatter};

#[rustfmt::skip]
static POWERS_OF_10: [f64; 20] = [
    1.0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9,
    1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19,
];

/// A number.
#[derive(Clone, Copy, Debug)]
pub struct Number(pub(crate) InternalNumber);

impl PartialEq for Number {
    /// Whether the two are the same number, as [`Number::as_f64`] and
    /// [`Number::is_integer`] tell.
    fn eq(&self, other: &Self) -> bool {
        match (self.0, other.0) {
            (InternalNumber::Real(a, _), InternalNumber::Real(b, _)) => a == b,
            (InternalNumber::Integer(a), InternalNumber::Integer(b)) => a == b,
            _ => false,
        }
    }
}

impl Number {
    /// The number zero.
    pub const ZERO: Self = Self::from_i32(0);
    /// The number one.
    pub const ONE: Self = Self::from_i32(1);

    /// Returns the number as a f64.
    pub fn as_f64(&self) -> f64 {
        match self.0 {
            InternalNumber::Real(r, _) => r,
            InternalNumber::Integer(i) => i as f64,
        }
    }

    /// Returns the number as a f32.
    pub fn as_f32(&self) -> f32 {
        match self.0 {
            InternalNumber::Real(r, _) => r as f32,
            InternalNumber::Integer(i) => i as f32,
        }
    }

    /// Returns the number as an i64.
    pub fn as_i64(&self) -> i64 {
        match self.0 {
            InternalNumber::Real(r, _) => {
                let res = r as i64;

                if !(trunc_f64(r) == r) {
                    debug!("float {r} was truncated to {res}");
                }

                res
            }
            InternalNumber::Integer(i) => i,
        }
    }

    /// Whether the number was written as an integer, without a decimal point.
    pub fn is_integer(&self) -> bool {
        matches!(self.0, InternalNumber::Integer(_))
    }

    /// The number as `MuPDF` reads it from the digits as written, which differs from
    /// [`Number::as_f64`] for a real (observed in `PyMuPDF` 1.28.2, through
    /// `pdf_to_real`): an integer is a 64-bit integer, converted to a 32-bit float.
    /// A real with up to nine characters before its point (sign and leading zeros
    /// included) is its first nine significant digits, the rest dropped, and its
    /// trailing zeros, times or divided by the power of ten that many places off
    /// held to 32 bits, rounded to the nearest 32-bit float. One with ten or more is the low 32 bits of its
    /// integer part, unsigned, as a 32-bit float, plus its fraction, whose digits are
    /// gathered into a numerator and a power of ten in 32-bit floats, one digit at a
    /// time, and divided, the sign applied last; so `4294967297.5` is 1.5.
    ///
    /// `None` where that is not established: an integer beyond 64 bits, a real whose
    /// first significant digit lies more than 37 places after its point (`MuPDF`
    /// reads some of these as 1), or whose digits make no number (`MuPDF` reads a
    /// fraction of 39 or more digits after a long integer part as NaN).
    pub fn viewer_f32(&self) -> Option<f32> {
        match self.0 {
            InternalNumber::Integer(i) => Some(i as f32),
            InternalNumber::Real(_, v) => (!v.is_nan()).then_some(v),
        }
    }

    /// Create a new `Number` from an f32 number.
    pub const fn from_f32(num: f32) -> Self {
        Self(InternalNumber::Real(num as f64, num))
    }

    /// Create a new `Number` from an i32 number.
    pub const fn from_i32(num: i32) -> Self {
        Self(InternalNumber::Integer(num as i64))
    }
}

impl Display for Number {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            InternalNumber::Real(value, _) => write!(f, "{value:?}"),
            InternalNumber::Integer(value) => write!(f, "{value}"),
        }
    }
}

// Reading and skipping a number are one reading: what one accepts, and where it ends,
// the other does too, so that checking a table, rebuilding it and reading an object
// find the same numbers, headers and references.
impl Skippable for Number {
    fn skip(r: &mut Reader<'_>, _: bool) -> Option<()> {
        lex::<false>(r).map(|_| ())
    }
}

impl Readable<'_> for Number {
    #[inline]
    fn read(r: &mut Reader<'_>, _: &ReaderContext<'_>) -> Option<Self> {
        let old_offset = r.offset();
        match lex::<true>(r) {
            Some(lexed) => Some(lexed.value(r)),
            None => {
                r.jump(old_offset);
                None
            }
        }
    }
}

/// What reading a number found, before its value is made.
struct Lexed {
    start: usize,
    negative: bool,
    mantissa: u64,
    overflowed: bool,
    has_dot: bool,
    /// Where the `.` is, when it is before `end`.
    dot_at: usize,
    decimal_shift: u32,
    has_digits: bool,
    /// Where the digits and `.` that make the value end: at the first minus after the
    /// sign, if any.
    end: Option<usize>,
}

/// Whether a number can start with `byte`, as [`lex`] reads one: with a sign, a digit,
/// a minus or a `.`. A header starts with one, so a rebuild looks for headers where
/// this holds.
#[inline(always)]
pub(crate) fn starts_number(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'+' | b'-' | b'.')
}

/// Reads a number: an optional sign, then a run of digits and minuses with at most
/// one `.` in or before it, not followed by a regular character. A sign alone is
/// zero where no regular character follows it (PDFJS-bug1753983; PDFJS-bug1953099
/// has one before a string). Its value is that of the digits and `.` before the first
/// minus after the sign: some PDFs have weird trailing minuses (`3-` is 3); with no
/// digits there, it is zero. `VALUE` says whether to gather what makes the value;
/// what is accepted and where it ends do not depend on it.
#[inline(always)]
fn lex<const VALUE: bool>(r: &mut Reader<'_>) -> Option<Lexed> {
    let first = r.peek_byte()?;
    if !starts_number(first) {
        return None;
    }
    let mut lexed = Lexed {
        start: r.offset(),
        negative: false,
        mantissa: 0,
        overflowed: false,
        has_dot: false,
        dot_at: 0,
        decimal_shift: 0,
        has_digits: false,
        end: None,
    };
    let signed = match first {
        b'-' => {
            lexed.negative = true;
            r.forward();
            true
        }
        b'+' => {
            r.forward();
            true
        }
        _ => false,
    };
    match r.peek_byte() {
        Some(b'0'..=b'9' | b'-' | b'.') => {}
        Some(b) if signed && !is_regular_character(b) => return Some(lexed),
        None if signed => return Some(lexed),
        _ => return None,
    }
    let mut dot = false;
    while let Some(b) = r.peek_byte() {
        match b {
            b'0'..=b'9' if VALUE && lexed.end.is_none() => lexed.digit(b - b'0'),
            b'0'..=b'9' => {}
            b'-' => {
                lexed.end.get_or_insert(r.offset());
            }
            b'.' if !dot => {
                dot = true;
                lexed.has_dot = VALUE && lexed.end.is_none();
                lexed.dot_at = r.offset();
            }
            _ => break,
        }
        r.forward();
    }

    // See issue 994. Don't accept numbers that are followed by a regular character
    // without any white space in-between.
    if r.peek_byte().is_some_and(is_regular_character) {
        return None;
    }

    Some(lexed)
}
impl Lexed {
    #[inline(always)]
    fn digit(&mut self, digit: u8) {
        self.has_digits = true;
        if self.has_dot {
            self.decimal_shift += 1;
        }
        if !self.overflowed {
            match self
                .mantissa
                .checked_mul(10)
                .and_then(|v| v.checked_add(digit as u64))
            {
                Some(value) => self.mantissa = value,
                None => self.overflowed = true,
            }
        }
    }

    /// The number's value; `r` is where it was read from.
    #[inline(always)]
    fn value(&self, r: &Reader<'_>) -> Number {
        if !self.has_digits {
            if self.negative && self.end.is_some() {
                // Zero, where `MuPDF` reads `--5` as one token, -5.
                return Number(InternalNumber::Real(0.0, f32::NAN));
            }
            return Number(InternalNumber::Integer(0));
        }
        // The sign, digits and `.` before the first minus after the sign.
        let token = r
            .range(self.start..self.end.unwrap_or_else(|| r.offset()))
            .unwrap_or_default();

        let integer_overflow = !self.has_dot && self.mantissa > i64::MAX as u64;

        if self.overflowed || integer_overflow {
            let number = core::str::from_utf8(token).unwrap_or_default();

            if !self.has_dot
                && let Ok(value) = number.parse::<i64>()
            {
                return Number(InternalNumber::Integer(value));
            }

            // Digits around at most one `.`, after an optional sign, always parse.
            let value = number.parse::<f64>().unwrap_or(f64::NAN);
            // An integer beyond 64 bits has no reading established.
            let viewer = if self.has_dot {
                viewer_real(token)
            } else {
                f32::NAN
            };
            return Number(InternalNumber::Real(value, viewer));
        }

        if !self.has_dot {
            let value = if self.negative {
                (self.mantissa as i64).wrapping_neg()
            } else {
                self.mantissa as i64
            };
            Number(InternalNumber::Integer(value))
        } else {
            let mut value = self.mantissa as f64;

            if self.decimal_shift > 0 {
                if self.decimal_shift < POWERS_OF_10.len() as u32 {
                    value /= POWERS_OF_10[self.decimal_shift as usize];
                } else {
                    value /= powi_f64(10.0, self.decimal_shift);
                }
            }

            if self.negative {
                value = -value;
            }

            // With up to nine digits, up to 13 after the point, and up to nine
            // characters before it, the reading is the nearest 32-bit float, which
            // the 64-bit quotient rounds to unless it lies just between two.
            let viewer = if self.mantissa < 1_000_000_000
                && self.decimal_shift <= 13
                && self.dot_at - self.start < 10
                && !halfway(value)
            {
                value as f32
            } else {
                viewer_real(token)
            };

            Number(InternalNumber::Real(value, viewer))
        }
    }
}

/// Whether `v`, a 64-bit float within the range of normal 32-bit floats or 0, lies
/// halfway between two 32-bit floats, where rounding it again may differ from
/// rounding the number it was rounded from.
fn halfway(v: f64) -> bool {
    // The 29 bits a 32-bit float drops, at half of its last place.
    v.to_bits() & 0x1FFF_FFFF == 0x1000_0000
}

/// [`Number::viewer_f32`] of a real written as `token`: a sign, digits, a point and
/// digits; NaN where no reading is established.
fn viewer_real(token: &[u8]) -> f32 {
    let (negative, digits) = match token.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, token),
    };
    let point = digits
        .iter()
        .position(|&b| b == b'.')
        .unwrap_or(digits.len());
    let (int, fraction) = (
        &digits[..point],
        digits.get(point + 1..).unwrap_or_default(),
    );
    let digit = |b: &u8| b.wrapping_sub(b'0');
    let v = if token.len() - digits.len() + int.len() >= 10 {
        let low = int.iter().fold(0_u32, |a, d| {
            a.wrapping_mul(10).wrapping_add(u32::from(digit(d)))
        });
        let (mut numerator, mut power) = (0.0_f32, 1.0_f32);
        for d in fraction {
            numerator = numerator * 10.0 + f32::from(digit(d));
            power *= 10.0;
        }
        low as f32 + numerator / power
    } else {
        // The first nine significant digits, `kept` of them from `lead` on.
        let all = || int.iter().chain(fraction);
        let Some(lead) = all().position(|&b| b != b'0') else {
            return if negative { -0.0 } else { 0.0 };
        };
        let (mut m, mut kept) = (0_u32, 0_i32);
        for d in all().skip(lead).take(9) {
            m = m * 10 + u32::from(digit(d));
            kept += 1;
        }
        // The power of ten of the first significant digit.
        let first = int.len() as i32 - lead as i32 - 1;
        if first < -37 {
            return f32::NAN;
        }
        nearest(m, first + 1 - kept)
    };
    if negative { -v } else { v }
}

/// `m` times ten to the `e`, for `m` of up to nine digits and `e` from -45 to 8, as
/// `MuPDF` reads it: `m` without its trailing zeros, times or divided by the power
/// of ten that many places off, held to 32 bits (rounded to nearest, ties to even),
/// then rounded to the nearest 32-bit float, ties to even.
fn nearest(mut m: u32, mut e: i32) -> f32 {
    if m == 0 {
        return 0.0;
    }
    while m.is_multiple_of(10) {
        m /= 10;
        e += 1;
    }
    // Ten to the `|e|` as `p` times two to the `p_exp`, `p` of 32 bits.
    let mut five: u128 = 1;
    for _ in 0..e.unsigned_abs() {
        five *= 5;
    }
    let bits = 128 - five.leading_zeros() as i32;
    let (mut p, mut p_exp) = if bits > 32 {
        let drop = bits - 32;
        let (kept, rest, half) = (five >> drop, five & ((1 << drop) - 1), 1_u128 << (drop - 1));
        let up = rest > half || (rest == half && kept & 1 == 1);
        (kept + u128::from(up), drop + e.abs())
    } else {
        (five << (32 - bits), e.abs() - (32 - bits))
    };
    if p >> 32 != 0 {
        p >>= 1;
        p_exp += 1;
    }
    // The exact quotient or product as `q` times two to the `q_exp`, and whether
    // anything is left below `q`.
    let (q, q_exp, inexact) = if e < 0 {
        let n = u128::from(m) << 64;
        (n / p, -64 - p_exp, n % p != 0)
    } else {
        (u128::from(m) * p, p_exp, false)
    };
    let bits = 128 - q.leading_zeros() as i32;
    let drop = bits - 24;
    let mantissa = if drop > 0 {
        let (kept, rest, half) = (q >> drop, q & ((1 << drop) - 1), 1_u128 << (drop - 1));
        let up = rest > half || (rest == half && (inexact || kept & 1 == 1));
        kept + u128::from(up)
    } else {
        q << -drop
    };
    let exp = q_exp + drop;
    // Exact: the mantissa has at most 25 bits and the power of two lies within
    // the range of 64-bit floats.
    let two = f64::from_bits(((exp + 1023) as u64) << 52);
    (mantissa as f64 * two) as f32
}

object!(Number, Number);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum InternalNumber {
    /// The value, and [`Number::viewer_f32`] of it, NaN where none is established.
    Real(f64, f32),
    Integer(i64),
}

macro_rules! int_num {
    ($i:ident) => {
        // As reading one reads it.
        impl Skippable for $i {
            fn skip(r: &mut Reader<'_>, _: bool) -> Option<()> {
                r.read_without_context::<$i>().map(|_| ())
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

impl Readable<'_> for f32 {
    fn read(r: &mut Reader<'_>, _: &ReaderContext<'_>) -> Option<Self> {
        r.read_without_context::<Number>()
            .map(|n| n.as_f64() as Self)
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

impl Readable<'_> for f64 {
    fn read(r: &mut Reader<'_>, _: &ReaderContext<'_>) -> Option<Self> {
        r.read_without_context::<Number>().map(|n| n.as_f64())
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

#[cfg(test)]
mod tests {
    use crate::object::Number;
    use crate::reader::Reader;
    use crate::reader::ReaderExt;

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

    /// Numbers as `MuPDF` reads them, observed in `PyMuPDF` 1.28.2 through
    /// `pdf_to_real`, by 32-bit float bits.
    #[test]
    fn viewer_reading() {
        let read = |t: &str| {
            Reader::new(t.as_bytes())
                .read_without_context::<Number>()
                .unwrap()
                .viewer_f32()
        };
        for (token, bits) in [
            ("0.1", 0x3DCC_CCCD_u32),
            ("16777217.5", 0x4B80_0001),
            // Nine significant digits, the rest dropped.
            ("16777217.001", 0x4B80_0000),
            ("-1000.000703157", 0xC47A_000B),
            (".7719685138", 0x3F45_9FBA),
            ("000000000016777217.5", 0x4B80_0000),
            // Ten characters before the point: the low 32 bits, then the fraction.
            ("4294967297.5", 0x3FC0_0000),
            ("4503599627370496.25", 0x3E80_0000),
            ("4294967296.4999999", 0x3EFF_FFFD),
            ("4294967297.782", 0x3FE4_1894),
            ("-695679881199.73806956042304114426", 0xCF79_C090),
            ("2147483647.5", 0x4F00_0000),
            ("-4294967296.", 0x8000_0000),
            ("0000000001.5", 0x3FC0_0000),
            ("1.5-3", 0x3FC0_0000),
            ("-.", 0x0000_0000),
            ("4194304.75", 0x4A80_0002),
            (
                "0.000000000000000000000000000000000000123456789",
                0x0228_0A6A,
            ),
            ("9223372036854775807", 0x5F00_0000),
            ("-9223372036854775808", 0xDF00_0000),
        ] {
            assert_eq!(read(token).map(f32::to_bits), Some(bits), "{token}");
        }
        for token in [
            // A fraction of 39 digits or more after a long integer part: NaN.
            "1000000000.1444119979317587707861942590889050111873",
            // Read as 1, or not as the nearest float, below 1e-37.
            "0.0000000000000000000000000000000000000000000001",
            "0.000000000000000000000000000000000000001",
            "9223372036854775808",
            "--5",
        ] {
            assert_eq!(read(token), None, "{token}");
        }
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

    // Skipping a number, or an integer, reads what reading it does, to the same end.
    #[test]
    fn skipping_a_number_reads_it_as_reading_does() {
        for token in [
            "3",
            "3-",
            "0-",
            "3--",
            "3-1",
            "1.5-",
            "1-.5",
            "--5",
            "-",
            "+",
            "-.",
            ".",
            ".-",
            "+ ",
            "- ",
            "-(",
            "+<",
            "-]",
            "-/",
            "-x",
            "+abc",
            "1.2.3",
            "1e3",
            "3.0",
            "3.7",
            "-1-2",
            "99999999999",
            "12345678901234567890123.",
            "-.5",
            "+.5",
            "0",
            "-0",
        ] {
            let read = |skip: bool| {
                let mut r = Reader::new(token.as_bytes());
                let found = if skip {
                    r.skip::<Number>(false).is_some()
                } else {
                    r.read_without_context::<Number>().is_some()
                };
                (found, r.offset())
            };
            assert_eq!(read(true), read(false), "{token:?}");
            let read = |skip: bool| {
                let mut r = Reader::new(token.as_bytes());
                let found = if skip {
                    r.skip::<i32>(false).is_some()
                } else {
                    r.read_without_context::<i32>().is_some()
                };
                (found, r.offset())
            };
            assert_eq!(read(true), read(false), "{token:?} as i32");
        }
    }

    // A number's value is that of the digits before the first minus after its sign;
    // a sign alone is zero.
    #[test]
    fn values_of_numbers_with_odd_minuses_and_signs() {
        for (token, value) in [
            ("3-", 3.0),
            ("3--", 3.0),
            ("1-.5", 1.0),
            ("1.5-", 1.5),
            ("--5", 0.0),
            ("+", 0.0),
            ("- ", 0.0),
            ("-(", 0.0),
            (".", 0.0),
        ] {
            let number = Reader::new(token.as_bytes())
                .read_without_context::<Number>()
                .unwrap();
            assert_eq!(number.as_f64(), value, "{token:?}");
        }
    }

    #[test]
    fn float_fallback_ignores_trailing_minus() {
        let input = "1234567890123456789012345-123".as_bytes();
        let mut reader = Reader::new(input);
        let got = reader.read_without_context::<Number>().unwrap().as_f64();
        assert_eq!(got, 1.234_567_890_123_456_8e24);
        assert_eq!(reader.offset(), input.len());
    }
}
