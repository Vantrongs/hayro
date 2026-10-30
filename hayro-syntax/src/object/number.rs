//! Numbers: which tokens are numbers, and what a [`Number`] reads as.
//!
//! A token is a number by these rules.
//!
//! 1. A number starts with a digit, `+`, `-` or `.`: an optional sign, then a run
//!    of digits and minuses holding at most one `.`. It ends at white space, a
//!    delimiter or the end of the data; a regular character right after it makes the
//!    whole word no number (`1e3`, `0R`, `1.2.3`, `5+`, `-x`, `+abc`, `++5`).
//! 2. A sign followed by white space, a delimiter or the end of the data is a
//!    number of its own, the integer 0 (`-(`, `+<`, `-]`, `- `).
//! 3. The value is that of the sign and of the digits and `.` before the first
//!    minus after the sign; that minus and all after it count for nothing (`3-`,
//!    `3-1` and `3--` are 3, `1-.5` is 1, `1.5-3` is 1.5).
//! 4. A number is a real where a `.` stands among those and a digit does too, else
//!    an integer. With no digit there it is zero: a real zero with no 32-bit reading
//!    where a minus follows a minus sign (`--`, `--5`, `-.-5`), else the integer 0
//!    (`.`, `-.`, `+`, `+-5`, `.-5`).
//! 5. An integer is held in 64 bits, signed; `-0` is 0. One beyond them is a real,
//!    the nearest 64-bit float to its digits, with no 32-bit reading.
//! 6. A real's value ([`Number::as_f64`]): where its digits, leading zeros
//!    included, make an integer below 2^64, that integer as the nearest 64-bit float,
//!    divided by ten to the number of digits after the point (exact up to ten to the
//!    19th, beyond that as `f64::powi` computes it), and negated for a minus sign;
//!    else the nearest 64-bit float to the decimal. So a real of 17 to 19 digits may
//!    read one place off the nearest float to it.
//! 7. Skipping a number accepts what reading it does and ends where reading ends, so
//!    that checking a table, rebuilding it, reading an object and finding a
//!    reference meet the same tokens.
//!
//! A [`Number`] reads as other types by these rules.
//!
//! 8. An integer type reads [`Number::as_i64`] (a real's value toward zero,
//!    saturating at the ends of 64 bits) and is none where that is outside the
//!    type; `f32` and `f64` read the value, rounded to the nearest.
//! 9. Two numbers are equal where both are integers of one value or both reals of
//!    one value; an integer never equals a real.
//!
//! A number's 32-bit reading ([`Number::viewer_f32`]) is the one a viewer makes
//! from the digits as written, which for some reals is not the nearest 32-bit float
//! to them:
//!
//! 10. An integer reads as the nearest 32-bit float, ties to even.
//! 11. A real with up to nine characters before its point, its sign and leading
//!     zeros counted, reads from its first nine significant digits, the rest
//!     dropped, not rounded: those digits without their trailing zeros are an
//!     integer `m` and a power of ten `e`, and the reading is the 32-bit float
//!     nearest to `m` times (or divided by) ten to the `|e|`, ties to even, where
//!     that power of ten is first rounded to 32 significant bits (nearest, ties to
//!     even) if it needs more. All digits zero read as a zero of the real's sign.
//!     A real whose first significant digit lies more than 37 places after the
//!     point has no reading.
//! 12. A real with ten or more characters before its point, counted so, reads as
//!     the low 32 bits of its integer part, unsigned, as the nearest 32-bit float,
//!     plus its fraction, the sum rounded to 32 bits, and negated for a minus sign.
//!     The fraction is a numerator over a power of ten, both gathered one digit of
//!     the fraction at a time in 32-bit floats, each step rounded to the nearest:
//!     the numerator times ten plus the digit, the power times ten. Where both
//!     overflow, which takes 39 digits or more, the real has no reading.
//! 13. A number rule 4 or 5 gives no reading has none.
//!
//! A number's 32-bit integer reading ([`Number::viewer_i32`]) is an integer's low
//! 32 bits, signed; a real's 32-bit reading plus one half, the sum rounded to 32
//! bits, then truncated toward zero, and `i32::MIN` where that lies outside 32 bits;
//! none where the 32-bit reading is none.
//!
//! `PyMuPDF` 1.28.2 shows the same readings, through a page's media box and a
//! stream's predictor parameters, where these rules give one. Where they give none,
//! it reads `--5` as -5 and `--` and `-.-5` as 0, wraps an integer past 64 bits
//! (`9223372036854775808` reads as -2^63), reads 10^-38 and 10^-39 written out as
//! subnormal floats and 10^-46 as 1, and a real rule 12 gives none as NaN. It reads
//! a number with any `.` as a real (`1-.5`, `-5-.5`, `.`, `-.`). The tests in
//! `tests/numbers_and_objects.rs` pin these rules.

use crate::math::powi_f64;
use crate::object::macros::object;
use crate::object::{Object, ObjectLike};
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt, Skippable};
use crate::trivia::is_regular_character;
use core::fmt::{Debug, Display, Formatter};

/// A number.
// The [`Repr`] packed into two words, each made whole in a register and stored at
// once: a number stored field by field and then copied into an object, as reading
// one does, makes the copy's wider loads wait until the narrow stores reach the
// cache.
#[derive(Clone, Copy)]
pub struct Number {
    /// The kind in the low byte; for a decimal its sign and whether it is long in
    /// two bits above it and its scale in the high half; for a real its places in
    /// the 24 bits above the kind and the bits of its 32-bit reading in the high
    /// half.
    head: u64,
    /// The integer, the digits, or the bits of the value.
    body: u64,
}

const INTEGER_KIND: u8 = 0;
const DECIMAL_KIND: u8 = 1;
const REAL_KIND: u8 = 2;
const NEGATIVE_BIT: u64 = 1 << 8;
const LONG_BIT: u64 = 1 << 9;
/// The most places a real's 24 bits hold.
const MOST_PLACES: u32 = (1 << 24) - 1;

#[derive(Clone, Copy, Debug)]
enum Repr {
    /// An integer of 64 bits.
    Integer(i64),
    /// A real whose digits make an integer below 2^64: its value and 32-bit reading
    /// are made from them when asked for, so that reading one only gathers digits.
    Decimal {
        /// The digits, leading zeros and point dropped.
        digits: u64,
        /// How many of them stand after the point.
        scale: u32,
        negative: bool,
        /// Whether ten or more characters, sign and leading zeros counted, stand
        /// before the point (rule 12 of the module doc rather than rule 11).
        long: bool,
    },
    /// Any other real: its value, its 32-bit reading, NaN where there is none,
    /// and how many digits stand after its point, plus one, none where it has no
    /// point (at most `MOST_PLACES`).
    Real {
        value: f64,
        viewer: f32,
        places: u32,
    },
}

impl Number {
    #[inline(always)]
    const fn new(repr: Repr) -> Self {
        match repr {
            Repr::Integer(value) => Self {
                head: INTEGER_KIND as u64,
                body: value as u64,
            },
            Repr::Decimal {
                digits,
                scale,
                negative,
                long,
            } => Self {
                head: DECIMAL_KIND as u64
                    | if negative { NEGATIVE_BIT } else { 0 }
                    | if long { LONG_BIT } else { 0 }
                    | (scale as u64) << 32,
                body: digits,
            },
            Repr::Real {
                value,
                viewer,
                places,
            } => Self {
                head: REAL_KIND as u64
                    | (if places < MOST_PLACES {
                        places
                    } else {
                        MOST_PLACES
                    } as u64)
                        << 8
                    | (viewer.to_bits() as u64) << 32,
                body: value.to_bits(),
            },
        }
    }

    #[inline(always)]
    fn repr(self) -> Repr {
        match self.head as u8 {
            INTEGER_KIND => Repr::Integer(self.body as i64),
            DECIMAL_KIND => Repr::Decimal {
                digits: self.body,
                scale: (self.head >> 32) as u32,
                negative: self.head & NEGATIVE_BIT != 0,
                long: self.head & LONG_BIT != 0,
            },
            _ => Repr::Real {
                value: f64::from_bits(self.body),
                viewer: f32::from_bits((self.head >> 32) as u32),
                places: (self.head >> 8) as u32 & MOST_PLACES,
            },
        }
    }
}

impl Default for Number {
    fn default() -> Self {
        Self::ZERO
    }
}

/// `Number(` and the [`Repr`] it holds.
impl Debug for Number {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("Number").field(&self.repr()).finish()
    }
}

impl PartialEq for Number {
    /// Whether the two are the same number, as [`Number::as_f64`] and
    /// [`Number::is_integer`] tell.
    fn eq(&self, other: &Self) -> bool {
        match (self.repr(), other.repr()) {
            (Repr::Integer(a), Repr::Integer(b)) => a == b,
            (Repr::Integer(_), _) | (_, Repr::Integer(_)) => false,
            _ => self.as_f64() == other.as_f64(),
        }
    }
}

/// Ten to the power of the index, each exact in 64 bits.
#[rustfmt::skip]
const POWERS_OF_TEN: [u64; 20] = [
    1, 10, 100, 1_000, 10_000, 100_000, 1_000_000, 10_000_000, 100_000_000,
    1_000_000_000, 10_000_000_000, 100_000_000_000, 1_000_000_000_000,
    10_000_000_000_000, 100_000_000_000_000, 1_000_000_000_000_000,
    10_000_000_000_000_000, 100_000_000_000_000_000, 1_000_000_000_000_000_000,
    10_000_000_000_000_000_000,
];

/// Ten to the power of the index as 64-bit floats, each exact.
#[rustfmt::skip]
const POWERS_OF_TEN_F64: [f64; 20] = [
    1.0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9,
    1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19,
];

/// A scale past which `powi` gives infinity for ten whatever the scale, so that a
/// longer one, which `powi`'s `i32` exponent cannot hold, divides alike.
const POWI_INFINITE: u32 = 1000;

/// How many places after the point the first significant digit of a real may stand
/// for it to have a 32-bit reading by nine digits (rule 11).
const MAX_PLACES: i64 = 37;

/// How many significant digits rule 11 reads.
const SIGNIFICANT: u32 = 9;

/// How many digits always make an integer below 2^64.
const SIMPLE_DIGITS: u32 = 19;

/// How many characters before the point make rule 12 read a real.
const LONG: usize = 10;

/// Fraction digits past which ten to their number is infinite in 32 bits however
/// it is rounded on the way, so that more leading zeros of a fraction change nothing
/// in rule 12.
const FRACTION_ZEROS: usize = 40;

/// The largest power of ten rule 11 divides by: nine digits whose first stands 37
/// places after the point end 45 places after it.
const MAX_POWER: usize = 46;

/// Ten to the power of the index, rounded to 32 significant bits (nearest, ties to
/// even) where it needs more: `(m, s)` for `m` times two to the `s`, `m` of exactly
/// 32 bits.
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

impl Number {
    /// The number zero.
    pub const ZERO: Self = Self::from_i32(0);
    /// The number one.
    pub const ONE: Self = Self::from_i32(1);

    /// Returns the number as a f64.
    #[inline]
    pub fn as_f64(&self) -> f64 {
        match self.repr() {
            Repr::Integer(value) => value as f64,
            Repr::Decimal {
                digits,
                scale,
                negative,
                ..
            } => {
                let mut value = digits as f64;
                if let Some(power) = POWERS_OF_TEN_F64.get(scale as usize) {
                    value /= power;
                } else {
                    value /= powi_f64(10.0, scale.min(POWI_INFINITE));
                }
                if negative { -value } else { value }
            }
            Repr::Real { value, .. } => value,
        }
    }

    /// Returns the number as a f32.
    #[inline]
    pub fn as_f32(&self) -> f32 {
        match self.repr() {
            Repr::Integer(value) => value as f32,
            _ => self.as_f64() as f32,
        }
    }

    /// Returns the number as an i64.
    #[inline]
    pub fn as_i64(&self) -> i64 {
        match self.repr() {
            Repr::Integer(value) => value,
            _ => self.as_f64() as i64,
        }
    }

    /// Whether the number was written as an integer, without a decimal point.
    pub fn is_integer(&self) -> bool {
        matches!(self.repr(), Repr::Integer(_))
    }

    /// The number's value as written, exactly, where its digits make an integer
    /// below 2^64: whether it is negative, its digits, and how many of them stand
    /// after the point (`-1.50` is `(true, 150, 2)`, `7` is `(false, 7, 0)`).
    /// `None` for any other real, whose value is the nearest 64-bit float to its
    /// digits ([`Number::as_f64`], rules 5 and 6 of the module doc).
    pub fn as_decimal(&self) -> Option<(bool, u64, u32)> {
        match self.repr() {
            Repr::Integer(value) => Some((value < 0, value.unsigned_abs(), 0)),
            Repr::Decimal {
                digits,
                scale,
                negative,
                ..
            } => Some((negative, digits, scale)),
            Repr::Real { .. } => None,
        }
    }

    /// How many digits the number is written with after its point (`0.500` has
    /// three, `5.` none); `None` where it is written without a point, an integer
    /// of any size, or made rather than read ([`Number::from_f32`]).
    pub fn fraction_digits(&self) -> Option<u32> {
        match self.repr() {
            Repr::Integer(_) => None,
            Repr::Decimal { scale, .. } => Some(scale),
            Repr::Real { places, .. } => places.checked_sub(1),
        }
    }

    /// The number in 32 bits as a viewer reads it from the digits as written, which
    /// for a real can differ from [`Number::as_f64`] rounded: `4294967297.5` reads as
    /// 1.5 (rules 10 to 13 of the module doc). `None` where no reading is
    /// established.
    pub fn viewer_f32(&self) -> Option<f32> {
        let reading = match self.repr() {
            Repr::Integer(value) => value as f32,
            Repr::Decimal {
                digits,
                scale,
                negative,
                long: false,
            } => nine_digits_of_decimal(digits, scale, negative)?,
            Repr::Decimal {
                digits,
                scale,
                negative,
                long: true,
            } => long_of_decimal(digits, scale, negative),
            Repr::Real { viewer, .. } => viewer,
        };
        (!reading.is_nan()).then_some(reading)
    }

    /// The number as a 32-bit integer as a viewer reads one: an integer's low 32
    /// bits, a real's [`Number::viewer_f32`] plus one half in 32 bits, truncated
    /// toward zero, and `i32::MIN` where that lies outside 32 bits. `None` where
    /// `viewer_f32` is.
    pub fn viewer_i32(&self) -> Option<i32> {
        if let Repr::Integer(value) = self.repr() {
            return Some(value as i32);
        }
        let rounded = self.viewer_f32()? + 0.5;
        // Below -2^31 the cast saturates to `i32::MIN` by itself.
        Some(if rounded >= 2_147_483_648.0 {
            i32::MIN
        } else {
            rounded as i32
        })
    }

    /// Create a new `Number` from an f32 number.
    pub const fn from_f32(num: f32) -> Self {
        Self::new(Repr::Real {
            value: num as f64,
            viewer: num,
            places: 0,
        })
    }

    /// Create a new `Number` from an i32 number.
    pub const fn from_i32(num: i32) -> Self {
        Self::new(Repr::Integer(num as i64))
    }

    /// The integer of the sign and the digits, which make `digits` below 2^64; a real
    /// with no 32-bit reading where 64 bits signed do not hold it (rule 5).
    #[inline(always)]
    fn integer(negative: bool, digits: u64) -> Self {
        // A minus sign holds one more: -2^63.
        if digits <= i64::MAX as u64 + u64::from(negative) {
            let value = digits as i64;
            Self::new(Repr::Integer(if negative {
                value.wrapping_neg()
            } else {
                value
            }))
        } else {
            let value = digits as f64;
            Self::unread(if negative { -value } else { value })
        }
    }

    /// The number of a token of an optional sign and the digits from `from` to `to`
    /// with the point among them at `point`, which make `digits`, where they are 1
    /// to 19; `None` where they are none or more.
    #[inline(always)]
    fn simple(
        negative: bool,
        signed: bool,
        from: usize,
        point: Option<usize>,
        to: usize,
        digits: u64,
    ) -> Option<Self> {
        let count = to - from - usize::from(point.is_some());
        if count.wrapping_sub(1) >= SIMPLE_DIGITS as usize {
            return None;
        }
        Some(match point {
            None => Self::integer(negative, digits),
            Some(point) => Self::new(Repr::Decimal {
                digits,
                // At most 19.
                scale: (to - point - 1) as u32,
                negative,
                long: point - from + usize::from(signed) >= LONG,
            }),
        })
    }

    /// A real of `value` with no 32-bit reading.
    fn unread(value: f64) -> Self {
        Self::new(Repr::Real {
            value,
            viewer: f32::NAN,
            places: 0,
        })
    }
}

/// An integer as its decimal digits (`10`); a real as its 64-bit value is written
/// by `{:?}` (`10.0`, `10.5`).
impl Display for Number {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        match self.repr() {
            Repr::Integer(value) => write!(f, "{value}"),
            _ => write!(f, "{:?}", self.as_f64()),
        }
    }
}

/// A number's token as [`scan`] finds it.
struct Token {
    negative: bool,
    /// Whether a sign stands first.
    signed: bool,
    /// Where the digits and point after the sign start.
    from: usize,
    /// Where they end: at the first minus after the sign, or the end of the token.
    to: usize,
    /// Where the point among them is, if one is.
    point: Option<usize>,
    /// Whether a minus ends them.
    minus: bool,
    /// The integer of those digits, kept to its low 64 bits.
    wrapped: u64,
}

/// Gathers the run of digits that starts with `byte`, the byte at `r`, into
/// `digits`, kept to their low 64 bits; returns the byte after the run, at which `r`
/// then stands.
#[inline(always)]
fn gather(r: &mut Reader<'_>, mut byte: Option<u8>, digits: &mut u64) -> Option<u8> {
    while let Some(b) = byte {
        let digit = b.wrapping_sub(b'0');
        if digit > 9 {
            break;
        }
        *digits = digits.wrapping_mul(10).wrapping_add(u64::from(digit));
        r.forward();
        byte = r.peek_byte();
    }
    byte
}

/// Reads the number token at `r`, which then stands after it (rules 1 and 3 of the
/// module doc); `None` where no number starts there or a regular character follows
/// the token. Reading and skipping a number both scan it here.
#[inline(always)]
fn scan(r: &mut Reader<'_>) -> Option<Token> {
    // On a copy, whose position stays in a register: moving `r` byte by byte stores
    // each position, and a copy of the number made just after waits for those
    // stores to reach the cache.
    let mut cur = r.clone();
    let token = scan_on(&mut cur);
    r.jump(cur.offset());
    token
}

#[inline(always)]
fn scan_on(r: &mut Reader<'_>) -> Option<Token> {
    let (negative, signed) = match r.peek_byte()? {
        b'-' => (true, true),
        b'+' => (false, true),
        b'0'..=b'9' | b'.' => (false, false),
        _ => return None,
    };
    if signed {
        r.forward();
    }
    let from = r.offset();
    let mut point = None;
    let mut wrapped = 0_u64;
    if gather(r, r.peek_byte(), &mut wrapped) == Some(b'.') {
        point = Some(r.offset());
        r.forward();
        gather(r, r.peek_byte(), &mut wrapped);
    }
    let to = r.offset();
    let minus = r.peek_byte() == Some(b'-');
    if minus {
        // The rest of the run counts for nothing, but holds at most one point too.
        let mut dotted = point.is_some();
        while let Some(byte) = r.peek_byte() {
            match byte {
                b'0'..=b'9' | b'-' => {}
                b'.' if !dotted => dotted = true,
                _ => break,
            }
            r.forward();
        }
    }
    if r.peek_byte().is_some_and(is_regular_character) {
        return None;
    }
    Some(Token {
        negative,
        signed,
        from,
        to,
        point,
        minus,
        wrapped,
    })
}

impl Token {
    /// The integer of the digits in `data`, more than 19 of them, where it is below
    /// 2^64, leading zeros and all.
    fn digits(&self, data: &[u8]) -> Option<u64> {
        data[self.from..self.to]
            .iter()
            .filter(|&&b| b != b'.')
            .try_fold(0_u64, |digits, &b| {
                digits.checked_mul(10)?.checked_add(u64::from(b - b'0'))
            })
    }

    /// The number the token in `data` is (rules 3 to 5 of the module doc).
    #[inline(always)]
    fn number(&self, data: &[u8]) -> Number {
        Number::simple(
            self.negative,
            self.signed,
            self.from,
            self.point,
            self.to,
            self.wrapped,
        )
        .unwrap_or_else(|| self.unusual(data))
    }

    /// The number of a token of no digit or more than 19.
    #[cold]
    #[inline(never)]
    fn unusual(&self, data: &[u8]) -> Number {
        if self.to - self.from == usize::from(self.point.is_some()) {
            // A minus after a minus sign makes a real zero with no reading (rule 4).
            return if self.negative && self.minus {
                Number::unread(0.0)
            } else {
                Number::ZERO
            };
        }
        let Some(point) = self.point else {
            return match self.digits(data) {
                Some(digits) => Number::integer(self.negative, digits),
                None => Number::unread(self.nearest(data)),
            };
        };
        let before = point - self.from + usize::from(self.signed);
        let long = before >= LONG;
        let scale = u32::try_from(self.to - point - 1).unwrap_or(u32::MAX);
        if let Some(digits) = self.digits(data) {
            return Number::new(Repr::Decimal {
                digits,
                scale,
                negative: self.negative,
                long,
            });
        }
        let whole = &data[self.from..point];
        let fraction = &data[point + 1..self.to];
        let viewer = if long {
            let low = whole.iter().fold(0_u32, |low, &d| {
                low.wrapping_mul(10).wrapping_add(u32::from(d - b'0'))
            });
            long_reading(low, fraction.iter().map(|d| d - b'0'), self.negative)
        } else {
            nine_digits_of_written(whole, fraction, self.negative).unwrap_or(f32::NAN)
        };
        Number::new(Repr::Real {
            value: self.nearest(data),
            viewer,
            places: scale.saturating_add(1),
        })
    }

    /// The nearest 64-bit float to the sign and the digits and point.
    fn nearest(&self, data: &[u8]) -> f64 {
        // Digits with at most one point, at least one digit among them, are what
        // `f64`'s parser reads, and it reads any number of them.
        let value = core::str::from_utf8(&data[self.from..self.to])
            .ok()
            .and_then(|text| text.parse::<f64>().ok())
            .expect("digits with at most one point parse as a float");
        if self.negative { -value } else { value }
    }
}

/// The digits of `value` in `out`, most significant first, none for zero.
fn decimal_digits(mut value: u64, out: &mut [u8; 20]) -> &[u8] {
    let mut at = out.len();
    while value > 0 {
        at -= 1;
        out[at] = (value % 10) as u8;
        value /= 10;
    }
    &out[at..]
}

/// Rule 11 for a real whose digits make `digits`, `scale` of them after the point.
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
            digits / POWERS_OF_TEN[dropped as usize],
            i64::from(dropped) - i64::from(scale),
        )
    } else {
        (digits, -i64::from(scale))
    };
    Some(nine_digits(m, power, negative))
}

/// Rule 11 for a real written with the digits `whole` before its point and
/// `fraction` after it.
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

/// The nearest 32-bit float to `m` (at most nine digits) times ten to the `power`,
/// the power of ten held to 32 significant bits first (rule 11).
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
        (m * POWERS_OF_TEN[power as usize]) as f32
    } else {
        let (p, shift) = POWERS_OF_TEN_32[power.unsigned_abs() as usize];
        divided(m, p, shift)
    };
    if negative { -reading } else { reading }
}

/// The bits the quotient of `divided` is widened by: `m` below 2^30 over `p` of 32
/// bits leaves it 26 bits at least, two more than a 32-bit float holds, and 57 at
/// most.
const WIDEN: i32 = 58;

/// The nearest 32-bit float, ties to even, to `m / (p * 2^shift)`, a normal float
/// for the `m`, `p` and `shift` rule 11 gives.
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

/// Rule 12 for a real whose digits make `digits`, `scale` of them after the point.
fn long_of_decimal(digits: u64, scale: u32, negative: bool) -> f32 {
    let (whole, fraction) = match POWERS_OF_TEN.get(scale as usize) {
        Some(&power) => (digits / power, digits % power),
        None => (0, digits),
    };
    let mut buffer = [0; 20];
    let written = decimal_digits(fraction, &mut buffer);
    let zeros = (scale as usize - written.len()).min(FRACTION_ZEROS);
    let fraction = core::iter::repeat_n(0, zeros).chain(written.iter().copied());
    long_reading(whole as u32, fraction, negative)
}

/// The low 32 bits `low` of a real's integer part as the nearest 32-bit float plus
/// its fraction, gathered digit by digit in 32 bits, the sign applied last; NaN where
/// both the numerator and the power of the fraction overflow (rule 12).
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

// Reading and skipping a number are one reading: what one accepts, and where it ends,
// the other does too, so that checking a table, rebuilding it and reading an object
// find the same numbers, headers and references.
impl Skippable for Number {
    #[inline]
    fn skip(r: &mut Reader<'_>, _: bool) -> Option<()> {
        scan(r).map(|_| ())
    }
}

impl Readable<'_> for Number {
    #[inline(always)]
    fn read(r: &mut Reader<'_>, _: &ReaderContext<'_>) -> Option<Self> {
        let token = scan(r)?;
        Some(token.number(r.data))
    }
}

impl Number {
    /// The number [`Readable::read`] reads at `r`, for the common numbers the content
    /// reader meets, which it tries first; `None`, with `r` moved, for any other
    /// token, which the caller then reads with [`Readable::read`].
    #[inline(always)]
    pub(crate) fn read_simple(r: &mut Reader<'_>) -> Option<Self> {
        // An optional sign, then at most 19 digits holding at most one point, ended
        // by white space, a delimiter or the end: what `scan` reads with no minus
        // after the sign and digits that fit 64 bits. Each byte is looked at once.
        let first = r.peek_byte()?;
        let negative = first == b'-';
        let signed = negative || first == b'+';
        let mut byte = Some(first);
        if signed {
            r.forward();
            byte = r.peek_byte();
        }
        let from = r.offset();
        let mut digits = 0_u64;
        let mut point = None;
        byte = gather(r, byte, &mut digits);
        if byte == Some(b'.') {
            point = Some(r.offset());
            r.forward();
            byte = gather(r, r.peek_byte(), &mut digits);
        }
        // A minus, a second point or any regular character: the full reading decides,
        // as it does for no digit or too many for 64 bits.
        if byte.is_some_and(is_regular_character) {
            return None;
        }
        Self::simple(negative, signed, from, point, r.offset(), digits)
    }
}

/// Whether a number can start with `byte` (rule 1 of the module doc). A header starts
/// with one, so a rebuild looks for headers where this holds.
#[inline(always)]
pub(crate) fn starts_number(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'+' | b'-' | b'.')
}

object!(Number, Number);

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
    use super::{MAX_PLACES, Number, POWERS_OF_TEN_32, Repr, divided};
    use crate::reader::{Reader, ReaderExt};

    /// Tokens of every shape the content reader meets: signs, points, digits up to
    /// past 64 bits, trailing minuses and regular characters, each before white
    /// space, a delimiter, or the end.
    fn tokens() -> Vec<String> {
        let mut bodies = vec![];
        for digits in [
            "",
            "0",
            "7",
            "05",
            "123",
            "4294967297",
            "1234567890123456789",
        ] {
            for more in ["", "9", "99999"] {
                let digits = format!("{digits}{more}");
                for at in 0..=digits.len() {
                    bodies.push(format!("{}.{}", &digits[..at], &digits[at..]));
                }
                bodies.push(digits);
            }
        }
        let mut tokens = vec![];
        for sign in ["", "-", "+"] {
            for body in &bodies {
                for tail in ["", "-", "-5", ".", "x"] {
                    for end in ["", " ", "/", "]", "%"] {
                        tokens.push(format!("{sign}{body}{tail}{end}"));
                    }
                }
            }
        }
        tokens
    }

    fn same(a: Number, b: Number) -> bool {
        match (a.repr(), b.repr()) {
            (Repr::Integer(a), Repr::Integer(b)) => a == b,
            _ => {
                a.is_integer() == b.is_integer()
                    && a.as_f64().to_bits() == b.as_f64().to_bits()
                    && a.viewer_f32().map(f32::to_bits) == b.viewer_f32().map(f32::to_bits)
            }
        }
    }

    /// Where the fast path reads a number, it is the number the full reading reads,
    /// ending where that ends.
    #[test]
    fn read_simple_agrees_with_read() {
        let mut simple = 0;
        for token in tokens() {
            let mut fast = Reader::new(token.as_bytes());
            let Some(number) = Number::read_simple(&mut fast) else {
                continue;
            };
            simple += 1;
            let mut full = Reader::new(token.as_bytes());
            let read = full.read_without_context::<Number>();
            assert!(read.is_some_and(|read| same(read, number)), "{token:?}");
            assert_eq!(fast.offset(), full.offset(), "{token:?}");
        }
        assert!(simple > 1000, "{simple}");
    }

    /// The quotient of rule 11 is the nearest 32-bit float to the exact one: one
    /// division in 64 bits, which rounds to 53 bits, rounds alike wherever its
    /// result is no midpoint of 32-bit floats.
    #[test]
    fn nine_digit_quotients_round_once() {
        let mut checked = 0;
        for (k, &(p, shift)) in POWERS_OF_TEN_32.iter().enumerate().skip(1) {
            let denominator = p as f64 * 2_f64.powi(shift);
            for i in 0..2000_u64 {
                let m = (i * 499_979 + k as u64 * 7_919) % 999_999_999 + 1;
                // Rule 11 reads no real whose first significant digit stands more
                // than 37 places after the point.
                if k as i64 - i64::from(m.ilog10()) > MAX_PLACES {
                    continue;
                }
                let wide = m as f64 / denominator;
                if wide.to_bits() & 0x1FFF_FFFF == 0x1000_0000 {
                    continue;
                }
                assert_eq!(divided(m, p, shift), wide as f32, "{m} / 10^{k}");
                checked += 1;
            }
        }
        assert!(checked > 50_000, "{checked}");
    }
}
