//! What an entry of the table leads to: the body of an object, where it is
//! ([`Place`]) and what it holds ([`Shape`]), read once for every entry and every
//! read that leads to it, and why an entry's object may not be read ([`Unread`]).
//!
//! Every read of an object the table lists goes from its entry to its place, which
//! takes no reading of the file, then to its shape, which is read the first time any
//! read reaches the place and kept: a header is read once however many entries lead
//! to it, and a read of any type is the object the shape holds, cast to that type,
//! so that a read of another type than the object's costs nothing to repeat.

use crate::lost::Lost;
use crate::object::ObjectIdentifier;
use crate::object::parsed::{DictNode, Node};
use crate::reader::{Reader, ReaderContext};
use crate::sync::Arc;
#[cfg(test)]
use crate::sync::FxHashMap;
use crate::trivia::{is_eol_character, is_white_space_character};

/// Where the body of an object is: the bytes it is in and where it starts in them,
/// its header and all for an object of the file, and the object whose key decrypts
/// it ([`ReaderContext::decryption`]). Every entry that leads to one place reads
/// one object there, which is read once ([`Shape`]).
///
/// A body is read only within its slot: from its place up to the next place in the
/// same bytes, or their end ([`super::XRef::place`]), as reading it on all the bytes
/// reads it ([`Read::in_slot`]); a body that does not end in its slot cannot be
/// read. So the places of one buffer lex disjoint bytes, and reading every body of a
/// buffer, keeping what it holds, takes time and memory linear in the buffer, however
/// the objects of a damaged file overlap; the table's check reads a file's objects by
/// the same rule ([`super::check`]). A stream's data, which are not lexed, nor copied,
/// may reach past the slot ([`super::Layout::stream_data`]); those of an object
/// stream, which are decoded and kept, end at another stream's place
/// ([`Taken::Contained`]).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Place {
    pub(super) source: Source,
    pub(super) at: usize,
    pub(super) decryption: Option<ObjectIdentifier>,
}

/// The bytes an object is in.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Source {
    /// The file, where an object starts with its header.
    File,
    /// The decoded data of an object stream, by where it is kept
    /// ([`crate::data::Data::object_stream`]), where an object has no header.
    Stream(usize),
}

/// Why a read of an object the table lists finds none.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(super) enum Unread {
    /// The object is lost: the table lists it, but what it leads to cannot be read
    /// as the object. The object named is the first that reading it lost, which
    /// the read tells the computation being watched ([`crate::lost`]): this is the
    /// only way a read that fails says why, and the only one from such a failure to
    /// a read that finds nothing.
    Lost(ObjectIdentifier),
    /// The read's own chain of references rejects the object or one that reading
    /// it needs as a cycle: from another chain it may be read, so it is neither
    /// lost nor kept.
    Rejected,
}

/// How a read takes an object that is a stream.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(super) enum Taken {
    /// As its dictionary, as a read of any type but a stream or any object reads it.
    Dict,
    /// With its data: a stream whose data cannot be read is lost.
    Whole,
    /// With its data, or as its dictionary where they cannot be read, as loading
    /// the pages reads it, whose reads look at dictionaries, arrays and numbers.
    Either,
    /// As an object stream: with its data, cut at the next place that holds a
    /// stream, if they reach it. Its data are decoded and kept, so the data of the
    /// object streams of a file must be disjoint for what is kept to be linear in
    /// the file, and those of streams at different places are only where none
    /// reaches another's place.
    Contained,
}

/// Where the data of a stream are, from and up to, and where they are cut as an
/// object stream's ([`Taken::Contained`]).
pub(super) type DataAt = (usize, usize, usize);

/// Where the slot of a body ends in the bytes it is in ([`Place`]), and whether those
/// bytes go on, unknown, past their end: those of an object stream whose data are cut
/// ([`Taken::Contained`]), whose end is no end of a body.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(super) struct Slot {
    pub(super) end: usize,
    pub(super) open: bool,
}

impl Slot {
    /// A slot that ends at `end`, where the bytes it is in go on if they do.
    pub(super) fn to(end: usize) -> Self {
        Self { end, open: false }
    }
}

/// What the body at a place holds, parsed once with every array and dictionary in
/// it ([`crate::object::parsed`]), which every read of the place, and of anything in
/// it, makes an object of without lexing the body again.
#[derive(Clone)]
pub(super) enum Shape {
    /// Nothing that can be read as an object: a header or a body that does not
    /// parse in its slot.
    Unreadable,
    /// An object that is no stream.
    Value(Node),
    /// A stream: its dictionary, where the dictionary ends, and where its data are,
    /// with what finding them lost, once found the same way from every chain of
    /// references (the data are found by /Length, which may be an object).
    Stream {
        dict: Arc<DictNode>,
        end: usize,
        data: Option<(Option<DataAt>, Lost)>,
    },
}

impl core::fmt::Debug for Shape {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Unreadable => "Unreadable",
            Self::Value(_) => "Value",
            Self::Stream { .. } => "Stream",
        })
    }
}

#[cfg(test)]
std::thread_local! {
    /// The places whose bodies were read on this thread, how often each, which the
    /// tests bound to show that each is read once however it is asked for.
    pub(crate) static BODIES: core::cell::RefCell<FxHashMap<Place, usize>> =
        core::cell::RefCell::new(FxHashMap::default());
    /// The places whose streams' data were found on this thread, how often each.
    pub(crate) static DATA_FOUND: core::cell::RefCell<FxHashMap<Place, usize>> =
        core::cell::RefCell::new(FxHashMap::default());
}

impl Shape {
    /// What the body at `place` of `data`, whose slot ends at `slot`, holds, after a
    /// header in the file, read in `ctx` as an entry leading to it reads it, which
    /// resolves nothing ([`Read::in_slot`]).
    pub(super) fn read<'a>(
        data: &'a [u8],
        slot: Slot,
        place: Place,
        ctx: &ReaderContext<'a>,
    ) -> Self {
        #[cfg(test)]
        BODIES.with_borrow_mut(|bodies| *bodies.entry(place).or_default() += 1);
        let (read, looked) = Read::in_slot(
            data,
            place.at,
            slot,
            place.source == Source::File,
            |r| DictNode::read(r, data, ctx),
            |r| Node::read(r, data, ctx),
        );
        crate::work::read_bodies(looked);
        match read.map(|(read, _)| read) {
            None => Self::Unreadable,
            Some(Read::Object(node)) => Self::Value(node),
            Some(Read::Dict(dict)) => Self::Value(Node::Dict(Arc::new(dict))),
            Some(Read::Stream { dict, end }) => Self::Stream {
                dict: Arc::new(dict),
                end,
                data: None,
            },
        }
    }
}

/// What reading the body of an object finds, whatever entry it is read for, a
/// dictionary as `D`, another object as `O`.
pub(super) enum Read<O, D> {
    /// An object that is no stream nor dictionary.
    Object(O),
    /// A dictionary.
    Dict(D),
    /// A stream's dictionary, and where the dictionary ends: the stream's data are
    /// found by its /Length, which may be an object, read from each chain of
    /// references.
    Stream { dict: D, end: usize },
}

impl<O, D> Read<O, D> {
    /// The body at `at` of `data`, after its header where `header`, a dictionary read
    /// by `dict`, another object by `object` ([`super::read_object`]), and where it
    /// ends, where that is in its slot, `slot`; and how many bytes reading it looked
    /// at.
    ///
    /// A body is what reading it on the whole of `data` finds, where that ends in the
    /// slot, or nothing: the slot's end is no end of a token, nor of the white space
    /// before `stream`. The body is read on the bytes up to the slot's end and one
    /// more, so a token reaching the end goes on past it, as it does on all the bytes,
    /// and the body is not read, its end being past the slot: a number, a name or a
    /// keyword cut by the end is not taken for a shorter one; where the bytes go on
    /// unknown past the slot's end, which is theirs ([`Slot::open`]), a body that
    /// reaches it is not read. The bytes in the slot and the next decide anything
    /// else reading the body looks at, but whether a dictionary is a stream's
    /// ([`stream_follows`]). So reading every body of a buffer looks at each byte a
    /// bounded number of times.
    pub(super) fn in_slot<'a>(
        data: &'a [u8],
        at: usize,
        slot: Slot,
        header: bool,
        dict: impl FnOnce(&mut Reader<'a>) -> Option<D>,
        object: impl FnOnce(&mut Reader<'a>) -> Option<O>,
    ) -> (Option<(Self, usize)>, usize) {
        let view = &data[..data.len().min(slot.end.saturating_add(1))];
        if at > view.len() {
            return (None, 0);
        }
        let mut r = Reader::new(view);
        r.jump(at);
        let read = super::read_object(&mut r, header, dict, object);
        let end = r.offset();
        let looked = end.saturating_sub(at);
        let ends = end <= slot.end && !(slot.open && end >= data.len());
        let Some((_, read)) = read.filter(|_| ends) else {
            return (None, looked);
        };
        match read {
            // As `Object::read_indirect` reads an indirect object; an object stream
            // holds none, but reads one so too.
            Ok(dict) => match stream_follows(data, end, slot) {
                Some((true, past)) => (Some((Self::Stream { dict, end }, end)), past - at),
                Some((false, past)) => (Some((Self::Dict(dict), end)), past - at),
                None => (None, slot.end - at),
            },
            Err(object) => (Some((Self::Object(object), end)), looked),
        }
    }
}

/// Whether the keyword `stream` follows, after white space and comments, a
/// dictionary that ends at `end` of `data` in its slot, `slot`, as reading on the
/// whole of `data` finds; and where looking for it stopped. `None` where that is not
/// known from the slot and the white space after it: a comment runs past the slot's
/// end or starts after it, or the bytes end, going on unknown, before it is known.
///
/// Past the slot, only white space is read, which the dictionary of no other body
/// reads, as a run of white space after one dictionary's end holds no other's end:
/// so looking for the keyword after every dictionary of a buffer reads each byte a
/// bounded number of times. In a file, a slot ends at a header, so the keyword is
/// looked for in the slot; in an object stream, the next object may start after
/// white space past it.
pub(super) fn stream_follows(data: &[u8], end: usize, slot: Slot) -> Option<(bool, usize)> {
    let (open, slot) = (slot.open, slot.end);
    let mut at = end;
    while at < slot {
        match data[at] {
            // A comment ends where its line does, at the slot's end at the latest.
            b'%' => match data[at..data.len().min(slot + 1)]
                .iter()
                .position(|b| is_eol_character(*b))
            {
                Some(line) => at += line,
                None if slot >= data.len() && !open => at = slot,
                None => return None,
            },
            b if is_white_space_character(b) => at += 1,
            _ => break,
        }
    }
    if at >= slot {
        while data.get(at).is_some_and(|b| is_white_space_character(*b)) {
            at += 1;
        }
        if data.get(at) == Some(&b'%') {
            return None;
        }
    }
    // Bytes that end where they go on unknown, which may be the keyword's start.
    let rest = data.get(at..).unwrap_or_default();
    if open && rest.len() < b"stream".len() && b"stream".starts_with(rest) {
        return None;
    }
    let stream = data
        .get(at..)
        .is_some_and(|rest| rest.starts_with(b"stream"));
    Some((stream, at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Dict, Object};
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;

    /// A small deterministic generator.
    struct Random(u64);

    impl Random {
        fn below(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) % n as u64) as usize
        }

        fn pick<'s>(&mut self, of: &[&'s str]) -> &'s str {
            of[self.below(of.len())]
        }
    }

    const TOKENS: &[&str] = &[
        "12",
        "-3",
        "+4",
        "1.5",
        ".5",
        "4.",
        "12-3",
        "0",
        "007",
        "99999999999999999999",
        "/A",
        "/Name",
        "/A#42",
        "/A#4",
        "/",
        "true",
        "false",
        "null",
        "R",
        "obj",
        "endobj",
        "stream",
        "endstream",
        "abc",
        "(a)",
        "(a(b)c)",
        "(\\))",
        "(x",
        "<41>",
        "<4 1>",
        "<>",
        "<4",
        "[",
        "]",
        "<<",
        ">>",
        "{",
        "}",
        "5 0 R",
        "5 0",
        "% note\n",
        "%x\r",
        "%",
    ];
    const SPACES: &[&str] = &["", " ", "  ", "\n", "\r\n", " % c\n", "\t"];

    /// An object, as a writer or a damaged file holds one.
    fn object(random: &mut Random, depth: usize) -> String {
        let space = |random: &mut Random| random.pick(SPACES).to_owned();
        match random.below(if depth > 2 { 4 } else { 7 }) {
            0..=2 => random.pick(TOKENS).to_owned(),
            3 => format!("{}{}", random.pick(TOKENS), random.pick(TOKENS)),
            4 => {
                let n = random.below(4);
                let items: Vec<String> = (0..n)
                    .map(|_| format!("{}{}", space(random), object(random, depth + 1)))
                    .collect();
                format!("[{}{}]", items.concat(), space(random))
            }
            _ => {
                let n = random.below(4);
                let entries: Vec<String> = (0..n)
                    .map(|_| {
                        let key = random.pick(&["/K", "/Length", "/Type", "/A#20"]);
                        let s = space(random);
                        format!("{key}{s}{}{}", object(random, depth + 1), space(random))
                    })
                    .collect();
                let after = random.pick(&["", " stream\nxy", "\nendobj", " %c\nstream\n"]);
                format!("<<{}{}>>{after}", space(random), entries.concat())
            }
        }
    }

    /// Whether whether a dictionary that ends at `end` of `data` is a stream's depends,
    /// for a slot that ends at `slot`, on what follows a comment the slot does not
    /// hold: after white space and comments, a comment runs past the slot's end or
    /// starts at or after it.
    fn depends(data: &[u8], end: usize, slot: usize) -> bool {
        let mut at = end;
        while let Some(&b) = data.get(at) {
            if b == b'%' {
                let eol = data[at..]
                    .iter()
                    .position(|b| *b == b'\n' || *b == b'\r')
                    .map_or(data.len(), |line| at + line);
                if at >= slot || eol > slot {
                    return true;
                }
                at = eol;
            } else if is_white_space_character(b) {
                at += 1;
            } else {
                break;
            }
        }
        false
    }

    /// What reading a body finds, made comparable: its kind, the object, and where
    /// it ends.
    fn shown(read: &Read<Node, Arc<DictNode>>, end: usize, data: &[u8]) -> String {
        let ctx = ReaderContext::dummy();
        let dict = |dict: &Arc<DictNode>| {
            let dict = Dict::parsed(data, dict.clone(), &ctx);
            format!("{}", Object::Dict(dict))
        };
        match read {
            Read::Object(node) => format!("object {} to {end}", node.object(data, &ctx)),
            Read::Dict(d) => format!("dict {} to {end}", dict(d)),
            Read::Stream { dict: d, end: e } => format!("stream {} to {e}", dict(d)),
        }
    }

    // A body read in its slot is what reading it on all the bytes finds, where that
    // ends in the slot, or nothing: however the slot's end cuts a token (a number, a
    // name, a keyword, a string, a reference), the white space or a comment before
    // `stream`, a body never reads as another value. And where reading on all the
    // bytes ends in the slot, so does reading in the slot, but where whether a
    // dictionary is a stream's depends on what follows a comment past the slot.
    #[test]
    fn a_body_read_in_its_slot_is_the_body_read_whole_or_nothing() {
        let mut random = Random(0x5eed);
        let mut read_in_slot = 0;
        let mut cut = 0;
        for case in 0..6000 {
            let header = random.below(2) == 0;
            let mut text = String::new();
            if header {
                text += random.pick(&["7 0 obj", "7 0 obj ", "7 0 obj\n", "7 0 obj%c\n"]);
            }
            for _ in 0..1 + random.below(3) {
                text += &object(&mut random, 0);
                text += random.pick(SPACES);
            }
            let data = text.as_bytes();
            let at = if header { 0 } else { random.below(data.len()) };
            let ctx = ReaderContext::dummy();
            let read = |slot: usize| {
                Read::in_slot(
                    data,
                    at,
                    Slot::to(slot),
                    header,
                    |r| DictNode::read(r, data, &ctx).map(Arc::new),
                    |r| Node::read(r, data, &ctx),
                )
                .0
            };
            let whole = read(data.len());
            for _ in 0..8 {
                let slot = at + random.below(data.len() - at + 1);
                let what = || format!("case {case}, at {at}, slot {slot} of {text:?}");
                match (read(slot), &whole) {
                    (Some((part, end)), Some((whole, whole_end))) => {
                        assert!(*whole_end <= slot, "{}", what());
                        assert_eq!(
                            shown(&part, end, data),
                            shown(whole, *whole_end, data),
                            "{}",
                            what()
                        );
                        read_in_slot += 1;
                    }
                    (Some((part, end)), None) => {
                        panic!(
                            "{}: {} read, nothing whole",
                            what(),
                            shown(&part, end, data)
                        )
                    }
                    (None, Some((whole, end))) if *end <= slot => {
                        let dict = !matches!(whole, Read::Object(_));
                        assert!(
                            dict && depends(data, *end, slot),
                            "{}: {} whole, nothing in the slot",
                            what(),
                            shown(whole, *end, data)
                        );
                    }
                    (None, _) => cut += 1,
                }
            }
        }
        // Both outcomes are met often.
        assert!(
            read_in_slot > 5000 && cut > 5000,
            "{read_in_slot} read, {cut} cut"
        );
    }

    // Where the bytes end going on unknown, as the data of an object stream cut at
    // another stream do, a body read in a slot up to their end is what reading it
    // finds whatever bytes follow, or nothing.
    #[test]
    fn a_body_read_up_to_an_open_end_is_the_body_whatever_follows_or_nothing() {
        let mut random = Random(0x0be7);
        let mut read_open = 0;
        for case in 0..6000 {
            let mut text = String::new();
            for _ in 0..1 + random.below(3) {
                text += &object(&mut random, 0);
                text += random.pick(SPACES);
            }
            let end = 1 + random.below(text.len());
            let at = random.below(end);
            let ctx = ReaderContext::dummy();
            let read = |data: &[u8], slot: Slot| {
                Read::in_slot(
                    data,
                    at,
                    slot,
                    false,
                    |r| DictNode::read(r, data, &ctx).map(Arc::new),
                    |r| Node::read(r, data, &ctx),
                )
                .0
                .map(|(read, end)| shown(&read, end, data))
            };
            let cut = &text.as_bytes()[..end];
            let Some(open) = read(cut, Slot { end, open: true }) else {
                continue;
            };
            read_open += 1;
            for after in [
                &text.as_bytes()[end..],
                random.pick(TOKENS).as_bytes(),
                b"stream\n".as_slice(),
                b"0 R".as_slice(),
            ] {
                let whole = [cut, after].concat();
                assert_eq!(
                    read(&whole, Slot::to(whole.len())).as_ref(),
                    Some(&open),
                    "case {case}, at {at} of {:?} then {:?}",
                    String::from_utf8_lossy(cut),
                    String::from_utf8_lossy(after)
                );
            }
        }
        assert!(read_open > 1000, "{read_open} read");
    }
}
