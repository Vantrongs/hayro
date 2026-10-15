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
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Number(pub(crate) InternalNumber);

impl Number {
    /// The number zero.
    pub const ZERO: Self = Self::from_i32(0);
    /// The number one.
    pub const ONE: Self = Self::from_i32(1);

    /// Returns the number as a f64.
    pub fn as_f64(&self) -> f64 {
        match self.0 {
            InternalNumber::Real(r) => r,
            InternalNumber::Integer(i) => i as f64,
        }
    }

    /// Returns the number as a f32.
    pub fn as_f32(&self) -> f32 {
        match self.0 {
            InternalNumber::Real(r) => r as f32,
            InternalNumber::Integer(i) => i as f32,
        }
    }

    /// Returns the number as an i64.
    pub fn as_i64(&self) -> i64 {
        match self.0 {
            InternalNumber::Real(r) => {
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

    /// Create a new `Number` from an f32 number.
    pub const fn from_f32(num: f32) -> Self {
        Self(InternalNumber::Real(num as f64))
    }

    /// Create a new `Number` from an i32 number.
    pub const fn from_i32(num: i32) -> Self {
        Self(InternalNumber::Integer(num as i64))
    }
}

impl Display for Number {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            InternalNumber::Real(value) => write!(f, "{value:?}"),
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
            return Number(InternalNumber::Integer(0));
        }

        let integer_overflow = !self.has_dot && self.mantissa > i64::MAX as u64;

        if self.overflowed || integer_overflow {
            // The sign, digits and `.` before the first minus after the sign.
            let number = r
                .range(self.start..self.end.unwrap_or_else(|| r.offset()))
                .and_then(|number| core::str::from_utf8(number).ok())
                .unwrap_or_default();

            if !self.has_dot
                && let Ok(value) = number.parse::<i64>()
            {
                return Number(InternalNumber::Integer(value));
            }

            // Digits around at most one `.`, after an optional sign, always parse.
            let value = number.parse::<f64>().unwrap_or(f64::NAN);
            return Number(InternalNumber::Real(value));
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

            Number(InternalNumber::Real(value))
        }
    }
}

object!(Number, Number);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum InternalNumber {
    Real(f64),
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
