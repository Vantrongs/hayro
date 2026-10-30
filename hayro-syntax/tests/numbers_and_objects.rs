//! How numbers are read, and how arrays and dictionaries are read where a token in
//! them is bad, observed only through the crate's public API: `Object::from_bytes`
//! and the other `FromBytes` readers, objects read from a file through
//! `XRef::get` and `XRef::written`, the accessors of `Number`, `Array`, `Dict` and
//! `Written`, and the operands of `content::UntypedIter`. The rules these tests pin
//! are stated in the module docs of `hayro_syntax::object` (tokens, arrays,
//! dictionaries) and of its `number` module (`Number`'s readings).
//!
//! An object is shown by its `Display`: an integer as its digits (`5`), a real as a
//! 64-bit float (`5.0`, `1e20`), `null`, a reference as `N G R`, a string in quotes,
//! a dictionary with its keys sorted. Every object a test reads is read three ways,
//! which must agree ([`read`]): from its bytes, as the body of an object a file's
//! table lists, and as the body of an object found by the scan that rebuilds a file
//! without a table.
//!
//! Where a test says `PyMuPDF` 1.28.2 shows the same, it was checked with
//! `probe.py`, kept outside this repository: `xref_object` and `xref_get_key` for
//! arrays, dictionaries and the kind of a number, the page's media box for a
//! number's 32-bit reading, a predictor's /Columns for its integer reading, and
//! `get_drawings` for what a content stream draws after a bad token. Where it reads
//! a case otherwise, the test names the difference and says whether it is
//! deliberate (a decision recorded in this crate or its history) or a suspected
//! defect; a difference it says neither of is pinned as it is, undecided.

use hayro_syntax::Pdf;
use hayro_syntax::content::UntypedIter;
use hayro_syntax::object::{
    Array, Dict, FromBytes, MaybeRef, Number, Object, ObjectIdentifier, Written,
};

const CATALOG: &str = "<< /Type /Catalog /Pages 2 0 R >>";
const PAGES: &str = "<< /Type /Pages /Kids [] /Count 0 >>";
const OBJECT: ObjectIdentifier = ObjectIdentifier {
    obj_number: 3,
    gen_number: 0,
};

/// A file whose body holds `objects`, each a number and a body, and whose table
/// lists each at its place.
fn listed(objects: &[(i32, &str)]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut entries = vec![];
    for (number, body) in objects {
        entries.push((*number, out.len()));
        out.extend(format!("{number} 0 obj\n{body}\nendobj\n").bytes());
    }
    let xref = out.len();
    out.extend(b"xref\n0 1\n0000000000 65535 f \n");
    for (number, offset) in entries {
        out.extend(format!("{number} 1\n{offset:010} 00000 n \n").bytes());
    }
    let size = objects.len() + 1;
    out.extend(
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").bytes(),
    );
    out
}

/// A file without a table, whose objects are found by scanning it.
fn unlisted(objects: &[(i32, &str)]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    for (number, body) in objects {
        out.extend(format!("{number} 0 obj\n{body}\nendobj\n").bytes());
    }
    out.extend(b"trailer\n<< /Root 1 0 R >>\n%%EOF\n");
    out
}

/// A file with object 3 `body`; `table` says whether it has a table.
fn file(body: &str, table: bool) -> Pdf {
    let objects = [(1, CATALOG), (2, PAGES), (3, body)];
    let data = if table {
        listed(&objects)
    } else {
        unlisted(&objects)
    };
    Pdf::new(data).unwrap()
}

/// `body` read as an object, shown by its `Display`, or `none` where it cannot be
/// read: from its bytes, as object 3 of a file with a table, and as object 3 of a
/// file without one, which must agree.
fn read(body: &str) -> String {
    let show = |object: Option<Object<'_>>| object.map_or("none".to_owned(), |o| o.to_string());
    let from_bytes = show(Object::from_bytes(body.as_bytes()));
    for table in [true, false] {
        let pdf = file(body, table);
        let in_file = show(pdf.xref().get::<Object<'_>>(OBJECT));
        assert_eq!(in_file, from_bytes, "{body:?}, table: {table}");
    }
    from_bytes
}

/// Asserts that each body reads as shown ([`read`]).
fn reads(cases: &[(&str, &str)]) {
    for (body, shown) in cases {
        assert_eq!(read(body), *shown, "{body:?}");
    }
}

/// The number `token` is, read alone; `None` where it is no number.
fn number(token: &str) -> Option<Number> {
    Number::from_bytes(token.as_bytes())
}

/// The bits of [`Number::viewer_f32`] of `token`.
fn viewer(token: &str) -> Option<u32> {
    number(token).unwrap().viewer_f32().map(f32::to_bits)
}

/// The instructions of `content`, each its operator and operands.
fn content(content: &str) -> String {
    let mut instructions = UntypedIter::new(content.as_bytes());
    let mut out = vec![];
    while let Some(instruction) = instructions.next() {
        let operands: Vec<String> = instruction.operands().map(|o| o.to_string()).collect();
        out.push(format!("{}({})", instruction.operator, operands.join(" ")));
    }
    out.join(" ")
}

/// A number is an optional sign, then digits, minuses and at most one point. It
/// ends at white space, a delimiter or the end of the data; a regular character
/// right after it makes the whole word no number. `PyMuPDF` 1.28.2 shows the same
/// split of each of these arrays into items.
#[test]
fn where_a_number_ends() {
    reads(&[
        // Glued to delimiters: each delimiter starts the next token.
        (
            "[5/N 5(s) 5[1] 5<<>> 5<41> 5%c\n 6]",
            r#"[5 /N 5 "s" 5 [1] 5 <<>> 5 "A" 5 6]"#,
        ),
        ("<</A 5/B 6>>", "<< /A 5 /B 6 >>"),
        ("[1 2]", "[1 2]"),
        // A sign alone before a delimiter, white space or the end is zero.
        ("[-(a) +<41> -/N -]", r#"[0 "a" 0 "A" 0 /N 0]"#),
        ("[- + -%c\n 1]", "[0 0 0 1]"),
        // A regular character after the digits makes the word no number: null.
        (
            "[1e3 1E3 0R 1x 1.0x .x -x +abc 5+]",
            "[null null null null null null null null null]",
        ),
        // A second point, or a sign after the first character but for a minus.
        (
            "[1.2.3 5.. .5. 1.5-. ++5 -+5]",
            "[null null null null null null]",
        ),
    ]);
    for token in ["-", "+", "-0", "5"] {
        assert!(number(token).is_some(), "{token}");
    }
    for token in ["1e3", "0R", "1.2.3", "-x", "+abc", "", "x"] {
        assert!(number(token).is_none(), "{token}");
    }
}

/// A number's value is that of its sign and the digits and point before the first
/// minus after the sign; minuses there and everything after them count for nothing.
/// It is a real when a point is among those, else an integer; with no digits there
/// it is zero, a real with no 32-bit reading where the sign is a minus followed by
/// another minus, else the integer 0.
///
/// `PyMuPDF` 1.28.2 reads the same values but for `--5`, which it reads as -5 (a
/// difference `Written` records, kept deliberately), and it reads `.`, `-.`, `+.`,
/// `.-5`, `-.-5`, `1-.5` and `-5-.5` as reals, because they hold a point, and `--`
/// as the integer 0.
#[test]
fn signs_points_and_minuses() {
    for (token, shown, integer) in [
        ("0", "0", true),
        ("-0", "0", true),
        ("+32", "32", true),
        ("-32", "-32", true),
        ("003245", "3245", true),
        ("5.", "5.0", false),
        ("5.0", "5.0", false),
        ("-0.", "-0.0", false),
        ("-0.0", "-0.0", false),
        ("-.345", "-0.345", false),
        ("+.5", "0.5", false),
        (".5", "0.5", false),
        // No digits: zero.
        (".", "0", true),
        ("-.", "0", true),
        ("+.", "0", true),
        ("-", "0", true),
        ("+", "0", true),
        ("+-5", "0", true),
        (".-5", "0", true),
        ("--", "0.0", false),
        ("--5", "0.0", false),
        ("-.-5", "0.0", false),
        // Trailing minuses.
        ("3-", "3", true),
        ("3--", "3", true),
        ("3-1", "3", true),
        ("1-2-3", "1", true),
        ("-1-2", "-1", true),
        ("1-.5", "1", true),
        ("-5-.5", "-5", true),
        ("1.5-", "1.5", false),
        ("1.5-3", "1.5", false),
        ("0-", "0", true),
        (".-", "0", true),
        ("12345678901234567890123.", "1.2345678901234568e22", false),
    ] {
        let n = number(token).unwrap();
        assert_eq!(
            (n.to_string(), n.is_integer()),
            (shown.to_owned(), integer),
            "{token}"
        );
        assert_eq!(read(token), shown, "{token}");
        // As an item, which reading the array's bytes finds by skipping it.
        assert_eq!(
            read(&format!("[{token} /N]")),
            format!("[{shown} /N]"),
            "{token}"
        );
    }
    for token in ["--", "--5", "-.-5"] {
        assert_eq!(number(token).unwrap().viewer_f32(), None, "{token}");
    }
}

/// An integer is held in 64 bits. Past them, an integer is a real: the nearest
/// 64-bit float to its digits, with no 32-bit reading.
///
/// `PyMuPDF` 1.28.2 keeps 64 bits too (its media box shows `9223372036854775807`
/// as 2^63), and wraps past them: `9223372036854775808` is -2^63 there. That an
/// integer past 64 bits has no 32-bit reading here is deliberate (`viewer_f32`
/// gives a reading only where one is established).
#[test]
fn integers_past_32_and_64_bits() {
    for (token, shown, integer, i64_value) in [
        ("4294967297", "4294967297", true, 4_294_967_297),
        ("-4294967297", "-4294967297", true, -4_294_967_297),
        ("9223372036854775807", "9223372036854775807", true, i64::MAX),
        (
            "-9223372036854775808",
            "-9223372036854775808",
            true,
            i64::MIN,
        ),
        (
            "9223372036854775808",
            "9.223372036854776e18",
            false,
            i64::MAX,
        ),
        (
            "-9223372036854775809",
            "-9.223372036854776e18",
            false,
            i64::MIN,
        ),
        ("99999999999999999999", "1e20", false, i64::MAX),
        (
            "-1234567890123456789012345",
            "-1.2345678901234568e24",
            false,
            i64::MIN,
        ),
        (
            "1234567890123456789012345-123",
            "1.2345678901234568e24",
            false,
            i64::MAX,
        ),
    ] {
        let n = number(token).unwrap();
        assert_eq!(n.to_string(), shown, "{token}");
        assert_eq!(n.is_integer(), integer, "{token}");
        assert_eq!(n.as_i64(), i64_value, "{token}");
        assert_eq!(read(token), shown, "{token}");
        // As an item, which reading the array's bytes finds by skipping it.
        assert_eq!(
            read(&format!("[{token} /N]")),
            format!("[{shown} /N]"),
            "{token}"
        );
    }
    assert_eq!(number("9999999999999999999").unwrap().as_f64(), 1e19);
    for token in [
        "9223372036854775808",
        "-9223372036854775809",
        "99999999999999999999",
    ] {
        assert_eq!(number(token).unwrap().viewer_f32(), None, "{token}");
    }
    // An integer's 32-bit reading is the nearest 32-bit float.
    assert_eq!(viewer("4294967297"), Some(0x4F80_0000));
    assert_eq!(viewer("9223372036854775807"), Some(0x5F00_0000));
    assert_eq!(viewer("-9223372036854775808"), Some(0xDF00_0000));
    assert_eq!(viewer("16777217"), Some(0x4B80_0000));
}

/// A real's 64-bit value: where all its digits, leading zeros included, make an
/// integer of 64 bits unsigned, that integer as the nearest 64-bit float divided by
/// ten to the number of digits after the point; else the nearest 64-bit float to
/// the decimal. The first differs from the nearest to the decimal for some reals of
/// 17 digits or more.
#[test]
fn a_real_as_a_64_bit_float() {
    for (token, value) in [
        ("-32.01", -32.01),
        ("-.00143", -0.00143),
        ("98349.432534", 98_349.432_534),
        ("-34534656.34", -34_534_656.34),
        // 18 digits: the quotient, which is not the nearest float to the decimal
        // (6.128819645975627).
        ("6.12881964597562642", 6.128_819_645_975_626),
        ("706151680334.106163", 706_151_680_334.106_1),
        ("190.5000076293945313", 190.500_007_629_394_53),
        // 19 digits, which still make an integer of 64 bits: the quotient, where the
        // nearest float to the decimal is 9.674830585100175.
        ("9.674830585100174214", 9.674_830_585_100_173),
        // 20 digits overflow 64 bits: the nearest float to the decimal.
        ("190.50000762939453225", 190.500_007_629_394_53),
        ("12345678901234567890123.456", 1.234_567_890_123_456_8e22),
        // More than 19 digits after the point: ten to their number as `f64::powi`
        // computes it, which here is not the nearest float to 1e30.
        (
            "0.000000000000000000000000000001",
            9.999_999_999_999_999e-31,
        ),
        ("0.0000000000000000000000000000001234", 1.234e-31),
    ] {
        assert_eq!(number(token).unwrap().as_f64(), value, "{token}");
    }
    // A float32 read is the 64-bit value rounded.
    assert_eq!(
        f32::from_bytes(b"340282346638528859811704183484516925440"),
        Some(f32::MAX)
    );
    assert_eq!(f32::from_bytes(b"-32.01"), Some(-32.01));
    assert_eq!(f32::from_bytes(b"98349.432534"), Some(98_349.43));
    assert_eq!(number("98349.432534").unwrap().as_f32(), 98_349.43);
    assert_eq!(f64::from_bytes(b"-.345"), Some(-0.345));
}

/// An integer type reads a number's 64-bit value toward zero (saturating for a
/// real beyond 64 bits) and is none where that is outside the type; a float type
/// reads the 64-bit value rounded. A reference `N G R` is read from the integers so
/// read, which is why `1.5 0 R` is a reference to object 1.
#[test]
fn typed_reads() {
    let dict = Dict::from_bytes(
        b"<< /I 4294967297 /R 3.7 /N -1.5 /B 300 /M -1 /L 9223372036854775808 \
          /S 2147483647 /T 2147483648 /F 1e3 /Z 0 >>",
    )
    .unwrap();
    assert_eq!(dict.get::<i32>(b"I"), None);
    assert_eq!(dict.get::<i64>(b"I"), Some(4_294_967_297));
    assert_eq!(dict.get::<u32>(b"I"), None);
    assert_eq!(dict.get::<usize>(b"I"), Some(4_294_967_297));
    assert_eq!(dict.get::<i32>(b"R"), Some(3));
    assert_eq!(dict.get::<i32>(b"N"), Some(-1));
    assert_eq!(dict.get::<u8>(b"B"), None);
    assert_eq!(dict.get::<u16>(b"B"), Some(300));
    assert_eq!(dict.get::<u32>(b"M"), None);
    assert_eq!(dict.get::<usize>(b"M"), None);
    assert_eq!(dict.get::<i64>(b"L"), Some(i64::MAX));
    assert_eq!(dict.get::<i32>(b"S"), Some(i32::MAX));
    assert_eq!(dict.get::<i32>(b"T"), None);
    assert_eq!(dict.get::<u32>(b"T"), Some(2_147_483_648));
    assert_eq!(dict.get::<f32>(b"I"), Some(4_294_967_296.0));
    assert_eq!(dict.get::<f64>(b"R"), Some(3.7));
    assert_eq!(dict.get::<i32>(b"F"), None);
    assert_eq!(dict.get::<Number>(b"F"), None);
    assert!(dict.contains_key(b"F"));
    assert_eq!(dict.get::<u8>(b"Z"), Some(0));

    fn object(t: &[u8]) -> Object<'_> {
        Object::from_bytes(t).unwrap()
    }
    assert_eq!(object(b"3.7").into_i32(), Some(3));
    assert_eq!(object(b"-3.7").into_i32(), Some(-3));
    assert_eq!(object(b"4294967297").into_i32(), None);
    assert_eq!(object(b"255.9").into_u8(), Some(255));
    assert_eq!(object(b"65536").into_u16(), None);
    assert_eq!(object(b"0.1").into_f32(), Some(0.1));
    assert_eq!(object(b"(1)").into_i32(), None);
    assert_eq!(i32::from_bytes(b"+32"), Some(32));
    assert_eq!(i32::from_bytes(b"-32.9"), Some(-32));
    assert_eq!(u32::from_bytes(b"4294966260"), Some(4_294_966_260));
    assert_eq!(i64::from_bytes(b"38359922"), Some(38_359_922));
    assert_eq!(f32::from_bytes(b"+abc"), None);

    let array = Array::from_bytes(b"[1 4294967297 3.7 null 1e3 x (s) -2.5]").unwrap();
    assert_eq!(array.iter::<i32>().collect::<Vec<_>>(), [1, 3, -2]);
    assert_eq!(
        array.items::<i32>().collect::<Vec<_>>(),
        [Some(1), None, Some(3), None, None, None, None, Some(-2)]
    );
}

/// Two numbers are equal where both are integers of one value or both reals of one
/// 64-bit value; an integer never equals a real.
/// A number's exact decimal is its sign, digits and places after the point as
/// written, where the digits make an integer below 2^64; any other real has none.
#[test]
fn a_number_as_its_exact_decimal() {
    for (token, decimal) in [
        ("7", Some((false, 7, 0))),
        ("-12", Some((true, 12, 0))),
        ("-1.50", Some((true, 150, 2))),
        ("+.25", Some((false, 25, 2))),
        ("1000000.25", Some((false, 100_000_025, 2))),
        ("0.000000000000000000000000000001", Some((false, 1, 30))),
        ("18446744073709551615.0", None),
        ("5.0000000000000000000000001", None),
        ("99999999999999999999", None),
    ] {
        assert_eq!(number(token).unwrap().as_decimal(), decimal, "{token}");
    }
}

#[test]
fn equality() {
    let n = |t: &str| number(t).unwrap();
    assert_eq!(n("5.0"), Number::from_f32(5.0));
    assert_ne!(n("5.0"), Number::from_i32(5));
    assert_eq!(n("5"), Number::from_i32(5));
    assert_eq!(n("+5"), n("005"));
    assert_eq!(n("--5"), Number::from_f32(0.0));
    assert_eq!(n("-"), Number::ZERO);
    assert_eq!(n("1"), Number::ONE);
    assert_ne!(n("0.1"), Number::from_f32(0.1));
    assert_eq!(Number::from_f32(0.1).viewer_f32(), Some(0.1));
    assert_eq!(Number::from_i32(-7).viewer_i32(), Some(-7));
    assert_eq!(Number::from_f32(10.5).to_string(), "10.5");
    assert_eq!(Number::from_i32(10).to_string(), "10");
}

/// A real with up to nine characters before its point, sign and leading zeros
/// counted, is read in 32 bits from its first nine significant digits, the rest
/// dropped: the nearest 32-bit float to those digits times their power of ten, the
/// power of ten held to 32 significant bits first where it needs more. All digits
/// zero is a zero of the real's sign; a first significant digit more than 37 places
/// after the point has no reading. `PyMuPDF` 1.28.2 shows each of these bits.
#[test]
fn a_real_in_32_bits_from_nine_digits() {
    for (token, bits) in [
        ("0.1", 0x3DCC_CCCD_u32),
        ("3.7", 0x406C_CCCD),
        ("-1000.000703157", 0xC47A_000B),
        (".7719685138", 0x3F45_9FBA),
        ("16777217.5", 0x4B80_0001),
        ("4194304.75", 0x4A80_0002),
        // Halfway between two 32-bit floats: the even one.
        ("16777217.0", 0x4B80_0000),
        ("16777219.0", 0x4B80_0002),
        ("33554434.0", 0x4C00_0000),
        ("33554438.0", 0x4C00_0002),
        // Digits past the ninth are dropped, not rounded.
        ("16777217.001", 0x4B80_0000),
        ("190.50000762939453225", 0x433E_8000),
        // Just above the midpoint of two 32-bit floats, but not within nine digits:
        // the float below, where the nearest to the decimal is the one above.
        ("600.000030517579125", 0x4416_0000),
        ("0.30000002682309014892578125", 0x3E99_999A),
        ("0.002000000211410224437713623046875", 0x3B03_126F),
        ("1.700000107289360595703125", 0x3FD9_999A),
        ("7.2500002384195791015625", 0x40E8_0000),
        // Nine characters before the point; the tenth would be a sign.
        ("016777217.5", 0x4B80_0001),
        ("-0000000.1234567891", 0xBDFC_D6EA),
        // The power of ten held to 32 bits: one place off the nearest float.
        ("0.000000152703727", 0x3423_F6E1),
        ("0.0000000000037878188", 0x2C85_45A3),
        ("0.000000000000763653111", 0x2B56_F302),
        ("0.000000000000000634991775", 0x2637_061F),
        ("0.000000000000000000000000000001", 0x0DA2_4260),
        (
            "0.000000000000000000000000000000000000123456789",
            0x0228_0A6A,
        ),
        // Zeros keep the sign.
        ("-0.000", 0x8000_0000),
        ("0.0", 0x0000_0000),
        ("-.", 0x0000_0000),
        // A trailing minus ends the digits read.
        ("1.5-3", 0x3FC0_0000),
    ] {
        assert_eq!(viewer(token), Some(bits), "{token}");
    }
    // `PyMuPDF` 1.28.2 reads the first as 1 and the others as subnormal floats.
    for token in [
        "0.0000000000000000000000000000000000000000000001",
        "0.000000000000000000000000000000000000001",
        "0.00000000000000000000000000000000000001",
    ] {
        assert_eq!(viewer(token), None, "{token}");
    }
}

/// A real with ten or more characters before its point, sign and leading zeros
/// counted, is read in 32 bits as the low 32 bits of its integer part, unsigned, as
/// the nearest 32-bit float, plus its fraction, the sign applied last. The fraction
/// is its digits gathered one at a time in 32-bit floats (times ten plus the digit)
/// over ten to the number of its digits, gathered likewise, each step rounded to
/// the nearest; where both overflow, the real has no reading. `PyMuPDF` 1.28.2 shows
/// each of these bits, and NaN where this has no reading.
#[test]
fn a_real_in_32_bits_from_ten_characters_before_its_point() {
    let zeros = "0".repeat(38);
    let cases = [
        ("4294967297.5".to_owned(), Some(0x3FC0_0000_u32)),
        ("4503599627370496.25".to_owned(), Some(0x3E80_0000)),
        ("4294967296.4999999".to_owned(), Some(0x3EFF_FFFD)),
        ("4294967297.782".to_owned(), Some(0x3FE4_1894)),
        (
            "-695679881199.73806956042304114426".to_owned(),
            Some(0xCF79_C090),
        ),
        ("2147483647.5".to_owned(), Some(0x4F00_0000)),
        ("-4294967296.".to_owned(), Some(0x8000_0000)),
        ("-0000000000.0".to_owned(), Some(0x8000_0000)),
        ("0000000001.5".to_owned(), Some(0x3FC0_0000)),
        ("000000000016777217.5".to_owned(), Some(0x4B80_0000)),
        // A sign is a character before the point: compare `016777217.5` above.
        ("-016777217.5".to_owned(), Some(0xCB80_0000)),
        ("+016777217.5".to_owned(), Some(0x4B80_0000)),
        ("100000000000000000000.0".to_owned(), Some(0x4EC6_2000)),
        ("18446744073709551615.5".to_owned(), Some(0x4F80_0000)),
        ("18446744073709551616.5".to_owned(), Some(0x3F00_0000)),
        ("12345678901234567890123.456".to_owned(), Some(0x4EE2_848A)),
        // A fraction of 39 digits: the power overflows alone, the fraction is 0.
        (format!("1000000000.0{zeros}"), Some(0x4E6E_6B28)),
        (format!("1000000000.1{zeros}"), Some(0x4E6E_6B28)),
        (format!("1000000000.{zeros}1"), Some(0x4E6E_6B28)),
        // Both overflow.
        (format!("1000000000.4{zeros}"), None),
        (
            "1000000000.1444119979317587707861942590889050111873".to_owned(),
            None,
        ),
    ];
    for (token, bits) in cases {
        assert_eq!(viewer(&token), bits, "{token}");
    }
}

/// A number read as a 32-bit integer the way a viewer reads one: an integer's low
/// 32 bits, signed; a real's 32-bit reading plus one half in 32 bits, truncated
/// toward zero, and `i32::MIN` where that lies outside 32 bits; none where the
/// 32-bit reading is none. `PyMuPDF` 1.28.2 reads a predictor's /Columns so: 2.4 as
/// 2, 2.5 and 3.4 as 3, 3.5 as 4, 4294967297 as 1 and 4294967299 as 3.
#[test]
fn a_number_as_a_32_bit_integer() {
    for (token, value) in [
        ("3", Some(3)),
        ("4294967297", Some(1)),
        ("4294967299", Some(3)),
        ("9223372036854775807", Some(-1)),
        ("-9223372036854775808", Some(0)),
        ("2.4", Some(2)),
        ("2.5", Some(3)),
        ("3.4", Some(3)),
        ("3.5", Some(4)),
        ("-2.5", Some(-2)),
        ("-3.7", Some(-3)),
        ("-0.7", Some(0)),
        // Plus one half rounds in 32 bits.
        ("16777215.0", Some(16_777_216)),
        ("2147483520.0", Some(2_147_483_520)),
        ("2147483647.0", Some(i32::MIN)),
        ("-2147483648.0", Some(i32::MIN)),
        ("-2147483904.0", Some(i32::MIN)),
        ("100000000000000000000.0", Some(1_661_992_960)),
        ("1.5-", Some(2)),
        ("--5", None),
        ("99999999999999999999", None),
    ] {
        assert_eq!(number(token).unwrap().viewer_i32(), value, "{token}");
    }
}

/// In an array or a dictionary, a token that starts like a number but is none, a
/// word that is not `true`, `false` or `null` as a whole, and a keyword such as `R`
/// or `obj` that makes no reference, are each one `null` item or value, up to the
/// next white space or delimiter: the rest of the object is kept. `PyMuPDF` 1.28.2
/// shows the same.
#[test]
fn bad_tokens_are_null() {
    reads(&[
        ("[8 0R 1e3 1.2.3 5]", "[8 null null null 5]"),
        ("[truefoo 1 nullx falsey]", "[null 1 null null]"),
        ("[truefoo falsefoo true false]", "[null null true false]"),
        (
            "[two t nul fals bar trust nullfoo]",
            "[null null null null null null null]",
        ),
        ("[1 0 obj]", "[1 0 null]"),
        ("[1 0 -R]", "[1 0 null]"),
        ("[R obj endobj stream]", "[null null null null]"),
        ("<< /A 1e3 /B 2 >>", "<< /A null /B 2 >>"),
        (
            "<< /A truefoo /B 0R /C true >>",
            "<< /A null /B null /C true >>",
        ),
        ("<< /Q two /A 1 >>", "<< /A 1 /Q null >>"),
    ]);
    let dict = Dict::from_bytes(b"<< /A truefoo /B 1 /C true >>").unwrap();
    assert_eq!(dict.get::<bool>(b"A"), None);
    assert!(dict.contains_key(b"A"));
    assert_eq!(dict.get::<i32>(b"B"), Some(1));
    assert_eq!(dict.get::<bool>(b"C"), Some(true));
    // `PyMuPDF` 1.28.2 fails to read the object for these; hayro reads them as
    // null, as in an array.
    reads(&[("1e3", "null"), ("0R", "null"), ("truefoo", "null")]);
}

/// A reference in an array or a dictionary is two numbers that read as 32-bit
/// integers (toward zero) and `R`, separated by white space only; else its tokens
/// are items of their own. `PyMuPDF` 1.28.2 shows the same but where a comment
/// stands between the numbers (a reference there: a suspected defect here, as a
/// comment counts as white space between tokens), where a number is outside 32
/// bits (a reference to its low 32 bits there) and where the first is a real or
/// `R` follows a reference (the object fails to read there). After `R` the next
/// token starts at once: `1 0 Rx` is a reference and `x`, where `PyMuPDF` reads
/// `Rx` as a word and holds `[1 0 null]`; a suspected defect, pinned.
#[test]
fn references() {
    reads(&[
        ("[1 0 R 2 0 R]", "[1 0 R 2 0 R]"),
        ("[1 0 R/N]", "[1 0 R /N]"),
        ("[1 2 3 R]", "[1 2 3 R]"),
        ("[0 0 R]", "[0 0 R]"),
        ("[-1 0 R]", "[-1 0 R]"),
        ("[+1 0 R]", "[1 0 R]"),
        ("[1 -0 R]", "[1 0 R]"),
        ("[3- 0 R]", "[3 0 R]"),
        ("<< /A 1 2 R /B 3 >>", "<< /A 1 2 R /B 3 >>"),
        // Differences from `PyMuPDF`.
        ("[1.5 0 R]", "[1 0 R]"),
        ("[1 0 R 0 R]", "[1 0 R 0 null]"),
        ("[1 %c\n 0 R]", "[1 0 null]"),
        ("[4294967297 0 R]", "[4294967297 0 null]"),
        ("[1 4294967297 R]", "[1 4294967297 null]"),
        ("[1 0 Rx 2]", "[1 0 R null 2]"),
    ]);
    let array = Array::from_bytes(b"[1 0 R 1.5 0 R]").unwrap();
    let refs: Vec<_> = array.raw_iter().filter_map(|i| i.as_obj_ref()).collect();
    assert_eq!(refs.len(), 2);
    assert!(matches!(array.raw_iter().next(), Some(MaybeRef::Ref(_))));
}

/// A dictionary's key is a name. Another token in a key's place is read as one
/// object and passed over, and the next token is read as a key; `PyMuPDF` 1.28.2
/// fails to read such a dictionary; passing it over is deliberate (upstream hayro's
/// leniency for garbage between entries). A later entry of
/// a key replaces an earlier one, as `PyMuPDF` 1.28.2 shows.
#[test]
fn tokens_in_a_keys_place() {
    reads(&[
        (
            "<< /ProcSet [ /PDF /Text ] /ExtGState << /GS2 14 0 R\n2000\n /GS3 15 0 R >> >>",
            "<< /ExtGState << /GS2 14 0 R /GS3 15 0 R >> /ProcSet [/PDF /Text] >>",
        ),
        ("<< 2000 /B 2 >>", "<< /B 2 >>"),
        ("<< (k) 1 /B 2 >>", "<< /B 2 >>"),
        ("<< 1e3 /B 2 >>", "<< /B 2 >>"),
        ("<< [1 2] true /B 2 >>", "<< /B 2 >>"),
        ("<< /A 1 2 >>", "<< /A 1 >>"),
        ("<< /A 1 /A 2 >>", "<< /A 2 >>"),
        // `PyMuPDF` 1.28.2 reads /A as null: `0R` ends its value there.
        ("<< /A 1 0R /B 2 >>", "<< /A 1 /B 2 >>"),
    ]);
}

/// An array or a dictionary that is not closed, or where a delimiter that starts
/// no object (`)`, `}`, `{`, a lone `>`, or `]` in a dictionary) stands for an item
/// or a value, fails to read whole, as does one holding a value that fails to read
/// (an unterminated string, a hex string with a character that is neither a hex
/// digit nor white space). An array ends at its first `]`. `PyMuPDF` 1.28.2 fails
/// on the same unclosed objects and on a dictionary missing a value; it reads such
/// a delimiter as a null item or value, and the hex string as `<00>`: the
/// delimiters are a suspected defect, pinned (one bad token loses the object), the
/// hex string deliberate.
#[test]
fn what_loses_the_object() {
    reads(&[
        ("[1 ) 2]", "none"),
        ("[1 } 2]", "none"),
        ("[1 { 2]", "none"),
        ("[1 > 2]", "none"),
        ("[1 >> 2]", "none"),
        ("[1 <zz> 2]", "none"),
        ("[1 (abc 2]", "none"),
        ("[1 2", "none"),
        ("[ << /A 1 ] 2 ]", "none"),
        ("<< /A ) /B 2 >>", "none"),
        ("<< /A ] /B 2 >>", "none"),
        ("<< /A } /B 2 >>", "none"),
        ("<< ] /B 2 >>", "none"),
        ("<< } /B 2 >>", "none"),
        ("<< /A >>", "none"),
        ("<< /A 1 /B >>", "none"),
        ("<< /A /B /C >>", "none"),
        ("<< /A [1 2 >>", "none"),
        ("<< /A 1", "none"),
        ("[1 2]]", "[1 2]"),
        ("[1 ] ]", "[1]"),
        ("<< /A 1 >> >>", "<< /A 1 >>"),
    ]);
}

/// Arrays and dictionaries nest up to 64 deep; deeper, the outermost fails to read.
#[test]
fn nesting() {
    let arrays = |n: usize| format!("{}0{}", "[".repeat(n), "]".repeat(n));
    let dicts = |n: usize| format!("{}0{}", "<< /X ".repeat(n), " >>".repeat(n));
    assert_ne!(read(&arrays(64)), "none");
    assert_eq!(read(&arrays(65)), "none");
    assert_ne!(read(&dicts(64)), "none");
    assert_eq!(read(&dicts(65)), "none");
    assert_eq!(read(&arrays(5000)), "none");
    assert_eq!(read(&dicts(5000)), "none");
}

/// An object of its own is read up to its end and no further: a body `4 0 R` is
/// the number 4, whatever object 4 is. `PyMuPDF` 1.28.2 reads it as a reference and
/// follows it (a box item given so reads 90 there, not 4).
#[test]
fn a_body_that_is_a_reference() {
    for table in [true, false] {
        let objects = [(1, CATALOG), (2, PAGES), (3, "4 0 R"), (4, "90")];
        let data = if table {
            listed(&objects)
        } else {
            unlisted(&objects)
        };
        let pdf = Pdf::new(data).unwrap();
        assert_eq!(pdf.xref().get::<Number>(OBJECT), Some(Number::from_i32(4)));
        assert_eq!(
            pdf.xref().get::<Object<'_>>(OBJECT).unwrap().to_string(),
            "4"
        );
    }
    assert_eq!(read("4 0 R"), "4");
}

/// A number is written as the bytes it is read from, trailing minuses and all,
/// wherever it is: an item, a value, or an object of its own.
#[test]
fn numbers_are_written_as_their_bytes() {
    let token = |w: Written<'_>| match w {
        Written::Number(token) => String::from_utf8(token.to_vec()).unwrap(),
        other => panic!("not a number: {other:?}"),
    };
    let body = "<< /A 4294967297 /B [--5 % c\n 5.0 1 0 R 3- -] /C -.50 >>";
    let check = |dict: &Dict<'_>| {
        assert_eq!(token(dict.get_written(b"A").unwrap()), "4294967297");
        assert_eq!(token(dict.get_written(b"C").unwrap()), "-.50");
        let Written::Array(array) = dict.get_written(b"B").unwrap() else {
            panic!("not an array");
        };
        let items: Vec<_> = array.written().collect();
        assert_eq!(token(items[0].clone()), "--5");
        assert_eq!(token(items[1].clone()), "5.0");
        assert!(matches!(items[2], Written::Ref(_)));
        assert_eq!(token(items[3].clone()), "3-");
        assert_eq!(token(items[4].clone()), "-");
    };
    check(&Dict::from_bytes(body.as_bytes()).unwrap());
    for table in [true, false] {
        let pdf = file(body, table);
        check(&pdf.xref().get::<Dict<'_>>(OBJECT).unwrap());
        assert!(matches!(pdf.xref().written(OBJECT), Some(Written::Dict(_))));
        let pdf = file("  -.50", table);
        assert_eq!(token(pdf.xref().written(OBJECT).unwrap()), "-.50");
    }
}

/// A content stream's operands are numbers read as in objects. Outside an array or
/// a dictionary, a token that starts like a number but is none is passed over with
/// the operands before it. Inside one, a word is null as in objects, but a bad
/// number or a delimiter that starts no object ends the reading of the stream:
/// `PyMuPDF` 1.28.2 goes on and draws the fill after `BT [1 1e3 2] TJ ET`,
/// `BT [(a) 0R 1] TJ ET`, `BT [1 ) 2] TJ ET` and `<< /A 1e3 /B 2 >> BDC`; a
/// suspected defect, pinned.
#[test]
fn content_operands() {
    for (stream, instructions) in [
        ("1 2 m", "m(1 2)"),
        (
            "--5 3- 1.5- 1-.5 . - 4294967297 m",
            "m(0.0 3 1.5 1 0 0 4294967297)",
        ),
        (".5 -.5 +.5 m", "m(0.5 -0.5 0.5)"),
        ("1.5-3 w", "w(1.5)"),
        ("+ w", "w(0)"),
        ("-(a) Tj", r#"Tj(0 "a")"#),
        ("- (a) Tj", r#"Tj(0 "a")"#),
        ("5/F1 Tf", "Tf(5 /F1)"),
        ("1e3 2 m", "m(2)"),
        ("1 2e 3 m", "m(3)"),
        ("0R 2 w", "w(2)"),
        ("1 2 3.4.5 m 6 w", "m() w(6)"),
        ("1 0 R 2 w", "R(1 0) w(2)"),
        ("[(a) trust 1] TJ 4 w", r#"TJ(["a" null 1]) w(4)"#),
        ("BT [1 1e3 2] TJ ET 4 w", "BT()"),
        ("BT [(a) 0R 1] TJ ET 4 w", "BT()"),
        ("BT [1 ) 2] TJ ET 4 w", "BT()"),
        ("<< /A 1e3 /B 2 >> BDC 4 w", ""),
    ] {
        assert_eq!(content(stream), instructions, "{stream:?}");
    }
}
