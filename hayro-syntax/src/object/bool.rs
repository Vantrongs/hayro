//! Booleans.

use crate::object::Object;
use crate::object::macros::object;
use crate::object::name::skip_keyword;
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt, Skippable};

impl Skippable for bool {
    fn skip(r: &mut Reader<'_>, _: bool) -> Option<()> {
        match r.peek_byte()? {
            b't' => skip_keyword(r, b"true"),
            b'f' => skip_keyword(r, b"false"),
            _ => None,
        }
    }
}

impl Readable<'_> for bool {
    fn read(r: &mut Reader<'_>, _: &ReaderContext<'_>) -> Option<Self> {
        match r.skip::<Self>(true)? {
            b"true" => Some(true),
            b"false" => Some(false),
            _ => None,
        }
    }
}

object!(bool, Boolean);

#[cfg(test)]
mod tests {
    use crate::reader::Reader;
    use crate::reader::ReaderExt;

    #[test]
    fn bool_true() {
        assert!(
            Reader::new("true".as_bytes())
                .read_without_context::<bool>()
                .unwrap()
        );
    }

    #[test]
    fn bool_false() {
        assert!(
            !Reader::new("false".as_bytes())
                .read_without_context::<bool>()
                .unwrap()
        );
    }

    /// A boolean is the whole word, as in `MuPDF`: `trueabdf` is another word.
    #[test]
    fn bool_trailing() {
        for word in ["trueabdf", "falsefoo", "trust", "tru", "true_"] {
            assert_eq!(
                Reader::new(word.as_bytes()).read_without_context::<bool>(),
                None,
                "{word}"
            );
        }
        for (word, value) in [("true]", true), ("false/A", false), ("true%c", true)] {
            assert_eq!(
                Reader::new(word.as_bytes()).read_without_context::<bool>(),
                Some(value),
                "{word}"
            );
        }
    }
}
