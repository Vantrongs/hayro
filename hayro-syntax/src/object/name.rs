//! Names.

use crate::filter::ascii_hex::decode_hex_digit;
use crate::object::macros::object;
use crate::object::{Bytes, Object};
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, Skippable};
use crate::trivia::is_regular_character;
use core::borrow::Borrow;
use core::fmt::{self, Debug, Display, Formatter};
use core::hash::{Hash, Hasher};
use core::ops::Deref;
use smallvec::SmallVec;

/// A PDF name object.
#[derive(Clone)]
pub struct Name<'a>(Bytes<'a>);

impl<'a> Deref for Name<'a> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

impl AsRef<[u8]> for Name<'_> {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl Borrow<[u8]> for Name<'_> {
    fn borrow(&self) -> &[u8] {
        self.as_ref()
    }
}

impl PartialEq for Name<'_> {
    /// Counts the bytes compared ([`crate::work::TOLD`]): a map compares a key it
    /// inserts with each key that hashes alike, which tests bound.
    fn eq(&self, other: &Self) -> bool {
        crate::work::told(1 + self.len().min(other.len()));
        self.as_ref() == other.as_ref()
    }
}

impl Eq for Name<'_> {}

impl Hash for Name<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_ref().hash(state);
    }
}

impl PartialOrd for Name<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Name<'_> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.as_ref().cmp(other.as_ref())
    }
}

impl<'a> Name<'a> {
    /// Create a new name from a sequence of bytes.
    #[inline]
    pub fn new(data: &'a [u8]) -> Option<Self> {
        if !data.contains(&b'#') {
            Some(Self::new_unescaped(data))
        } else {
            Self::new_escaped(data)
        }
    }

    /// Create a new name from an unescaped byte sequence.
    #[inline]
    pub fn new_unescaped(data: &'a [u8]) -> Self {
        Self(Bytes::Borrowed(data))
    }

    /// Create a new name from bytes that may contain escape sequences
    /// ([`unescape`]).
    #[inline]
    pub fn new_escaped(data: &'a [u8]) -> Option<Self> {
        let mut result = SmallVec::new();
        unescape(data, |b| result.push(b));
        Some(Self(Bytes::decoded(result)))
    }

    /// The name of `bytes`, decoded.
    pub(crate) fn from_bytes(bytes: Bytes<'a>) -> Self {
        Self(bytes)
    }

    /// The name's bytes, decoded.
    pub(crate) fn into_bytes(self) -> Bytes<'a> {
        self.0
    }

    /// The name, held without the data it was read from.
    pub(crate) fn to_static(&self) -> Name<'static> {
        Name(self.0.to_static())
    }

    /// Return a string representation of the name.
    ///
    /// Returns a placeholder in case the name is not UTF-8 encoded.
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_ref()).unwrap_or("{non-ascii key}")
    }
}

impl Debug for Name<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match core::str::from_utf8(self.as_ref()) {
            Ok(s) => <str as Debug>::fmt(s, f),
            Err(_) => <[u8] as Debug>::fmt(self.as_ref(), f),
        }
    }
}

impl Display for Name<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "/{}", self.as_str())
    }
}

object!(Name<'a>, Name);

// Skipping a name reads it as reading does, escapes included, without decoding them.
impl Skippable for Name<'_> {
    fn skip(r: &mut Reader<'_>, _: bool) -> Option<()> {
        lex_name_like(r, true).map(|_| ())
    }
}

impl<'a> Readable<'a> for Name<'a> {
    fn read(r: &mut Reader<'a>, _: &ReaderContext<'a>) -> Option<Self> {
        Self::new(lex_name_like(r, true)?)
    }
}

/// Reads a name, after its solidus where `solidus`, or an operator of a content
/// stream, which is read as a name without one: its bytes, whose `#` escapes are
/// decoded where they are read ([`unescape`]).
pub(crate) fn lex_name_like<'a>(r: &mut Reader<'a>, solidus: bool) -> Option<&'a [u8]> {
    let start = r.offset() + usize::from(solidus);
    skip_name_like(r, solidus)?;
    r.range(start..r.offset())
}

/// Decodes the `#` escapes of a name's bytes into `out`, as `MuPDF` reads them
/// (`PyMuPDF` 1.28.2): a `#` and two hex digits is the byte they make, but for 0,
/// which no name holds; any other `#` is itself (`/A#2`, `/A#G1` and `/A#00` are
/// `A#2`, `A#G1` and `A#00`, and `/A##41` is `A#A`).
#[inline]
fn unescape(data: &[u8], mut out: impl FnMut(u8)) {
    let mut at = 0;
    while let Some(&b) = data.get(at) {
        let byte = (b == b'#')
            .then(|| {
                let hex = data.get(at + 1..at + 3)?;
                let byte = decode_hex_digit(hex[0])? << 4 | decode_hex_digit(hex[1])?;
                (byte != 0).then_some(byte)
            })
            .flatten();
        match byte {
            Some(byte) => {
                out(byte);
                at += 3;
            }
            None => {
                out(b);
                at += 1;
            }
        }
    }
}

/// Skips the word `word`, a keyword such as `true` or `null`, only when it is the
/// whole token: `truefoo` and `nullable` are other words, which are left unread.
pub(crate) fn skip_keyword(r: &mut Reader<'_>, word: &[u8]) -> Option<()> {
    let start = r.offset();
    r.forward_while(is_regular_character);
    if r.range(start..r.offset()) == Some(word) {
        Some(())
    } else {
        r.jump(start);
        None
    }
}

// This method is shared by `Name` and the parser for content stream operators (which behave like
// names, except that they aren't preceded by a solidus.
pub(crate) fn skip_name_like(r: &mut Reader<'_>, solidus: bool) -> Option<()> {
    // Note that we are not validating hex escape sequences here
    // (since this method can lie on the hot path), so it's possible
    // this method will yield invalid names. Validation needs to happen during actual
    // actual reading.
    if solidus {
        r.forward_tag(b"/")?;
        r.forward_while(is_regular_character);
    } else {
        r.forward_while_1(is_regular_character)?;
    }

    Some(())
}

#[cfg(test)]
mod tests {
    use crate::object::Name;
    use crate::reader::Reader;
    use crate::reader::ReaderExt;
    use std::ops::Deref;

    #[test]
    fn display() {
        let name = Reader::new(b"/Test")
            .read_without_context::<Name<'_>>()
            .unwrap();

        assert_eq!(format!("{name}"), "/Test");
    }

    #[test]
    fn name_1() {
        assert_eq!(
            Reader::new("/".as_bytes())
                .read_without_context::<Name<'_>>()
                .unwrap()
                .deref(),
            b""
        );
    }

    #[test]
    fn name_2() {
        assert!(
            Reader::new("dfg".as_bytes())
                .read_without_context::<Name<'_>>()
                .is_none()
        );
    }

    // A `#` that is not an escape is itself, as `MuPDF` reads it (`PyMuPDF` 1.28.2).
    #[test]
    fn name_3() {
        for (written, read) in [
            ("/AB#FG", &b"AB#FG"[..]),
            ("/A#2", b"A#2"),
            ("/A#", b"A#"),
            ("/A#G1", b"A#G1"),
            ("/A#4G", b"A#4G"),
            ("/A##41", b"A#A"),
            ("/#", b"#"),
            ("/A#00", b"A#00"),
            ("/A#g1", b"A#g1"),
            ("/A#61", b"Aa"),
            ("/A#2F", b"A/"),
        ] {
            assert_eq!(
                Reader::new(written.as_bytes())
                    .read_without_context::<Name<'_>>()
                    .unwrap()
                    .deref(),
                read,
                "{written}"
            );
        }
    }

    #[test]
    fn name_4() {
        assert_eq!(
            Reader::new("/Name1".as_bytes())
                .read_without_context::<Name<'_>>()
                .unwrap()
                .deref(),
            b"Name1"
        );
    }

    #[test]
    fn name_5() {
        assert_eq!(
            Reader::new("/ASomewhatLongerName".as_bytes())
                .read_without_context::<Name<'_>>()
                .unwrap()
                .deref(),
            b"ASomewhatLongerName"
        );
    }

    #[test]
    fn name_6() {
        assert_eq!(
            Reader::new("/A;Name_With-Various***Characters?".as_bytes())
                .read_without_context::<Name<'_>>()
                .unwrap()
                .deref(),
            b"A;Name_With-Various***Characters?"
        );
    }

    #[test]
    fn name_7() {
        assert_eq!(
            Reader::new("/1.2".as_bytes())
                .read_without_context::<Name<'_>>()
                .unwrap()
                .deref(),
            b"1.2"
        );
    }

    #[test]
    fn name_8() {
        assert_eq!(
            Reader::new("/$$".as_bytes())
                .read_without_context::<Name<'_>>()
                .unwrap()
                .deref(),
            b"$$"
        );
    }

    #[test]
    fn name_9() {
        assert_eq!(
            Reader::new("/@pattern".as_bytes())
                .read_without_context::<Name<'_>>()
                .unwrap()
                .deref(),
            b"@pattern"
        );
    }

    #[test]
    fn name_10() {
        assert_eq!(
            Reader::new("/.notdef".as_bytes())
                .read_without_context::<Name<'_>>()
                .unwrap()
                .deref(),
            b".notdef"
        );
    }

    #[test]
    fn name_11() {
        assert_eq!(
            Reader::new("/lime#20Green".as_bytes())
                .read_without_context::<Name<'_>>()
                .unwrap()
                .deref(),
            b"lime Green"
        );
    }

    #[test]
    fn name_12() {
        assert_eq!(
            Reader::new("/paired#28#29parentheses".as_bytes())
                .read_without_context::<Name<'_>>()
                .unwrap()
                .deref(),
            b"paired()parentheses"
        );
    }

    #[test]
    fn name_13() {
        assert_eq!(
            Reader::new("/The_Key_of_F#23_Minor".as_bytes())
                .read_without_context::<Name<'_>>()
                .unwrap()
                .deref(),
            b"The_Key_of_F#_Minor"
        );
    }

    #[test]
    fn name_14() {
        assert_eq!(
            Reader::new("/A#42".as_bytes())
                .read_without_context::<Name<'_>>()
                .unwrap()
                .deref(),
            b"AB"
        );
    }

    #[test]
    fn name_15() {
        assert_eq!(
            Reader::new("/A#3b".as_bytes())
                .read_without_context::<Name<'_>>()
                .unwrap()
                .deref(),
            b"A;"
        );
    }

    #[test]
    fn name_16() {
        assert_eq!(
            Reader::new("/A#3B".as_bytes())
                .read_without_context::<Name<'_>>()
                .unwrap()
                .deref(),
            b"A;"
        );
    }

    #[test]
    fn name_17() {
        assert_eq!(
            Reader::new("/k1  ".as_bytes())
                .read_without_context::<Name<'_>>()
                .unwrap()
                .deref(),
            b"k1"
        );
    }
}
