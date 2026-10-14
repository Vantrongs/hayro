//! Where the objects of a damaged file end, for the scan that rebuilds its table.
//!
//! The scan asks, at each object header and each `<<` it meets, whether an object
//! starts there and where it ends, reading the rest of the file as reading the
//! object does. Objects of a damaged file nest in each other's strings, overlap, or
//! never end, so reading each from its start reads much of the file again for each.
//! [`Scan`] answers each question as reading the object on the whole rest of the
//! file would, reading every part of the file a bounded number of times:
//!
//! - A literal string ends where the depth of its unescaped parentheses first falls
//!   below the depth after its `(`. One pass over the file finds that depth after
//!   each byte, kept as its least value in each block of the file in a tree of
//!   minima, so where any string ends is found by reading at most two blocks and a
//!   path of the tree, whichever object it belongs to. No string is read whole.
//! - A comment ends at the end of its line, a line holding a `%`: the first pass
//!   lists those ends, and a comment's end is looked up among them, not read.
//! - An array or dictionary is read once: its outcome is kept at its `[` or `<<` for
//!   a header's object and for each object the scan asks about, which are the
//!   places another question can start at.
//! - Two readings can only meet inside an array or dictionary at a byte where both
//!   leave a string (a string opened at an escaped `(` ends with the string around
//!   it) or a comment (a reading that left a string inside another's comment line
//!   reaches its end too). Those bytes, marked in the first pass, keep where the
//!   array or dictionary read from them ends, so a reading that meets another there
//!   takes its outcome instead of reading on, and so do the arrays and
//!   dictionaries around such a byte where both readings close them.
//! - Arrays and dictionaries nest at most [`MAX_DEPTH`] deep, so a reading keeps
//!   at most that many open: one that holds deeper ones fails at once, as reading
//!   it does, and its outcome is kept.
//!
//! So the work of a scan is linear in the file, but for a lookup logarithmic in it
//! for each string and comment: the first pass reads each byte once; each array or
//! dictionary is read once by a reading in each of the three states a reading can
//! be in where it starts (an item of an array, a key or a value of a dictionary);
//! and a reading that meets another reads on only to the next marked byte. The
//! tests count this work over families of damaged files (`opening_a_file_reads_it_
//! in_linear_time`) and check that each answer is what reading the object finds
//! (`scan_ends_objects_as_reading_them_does`).

use crate::object::dict::keys::{ROOT, TYPE};
use crate::object::name::skip_name_like;
use crate::object::{MAX_OBJECT_NESTING_DEPTH, Name, Null, Number, ObjRef};
use crate::reader::{Reader, ReaderExt, Skippable};
use crate::sync::FxHashMap;
use crate::trivia::{is_eol_character, is_white_space_character};
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

/// How deep arrays and dictionaries nest in an object that can be read.
const MAX_DEPTH: usize = MAX_OBJECT_NESTING_DEPTH;

/// The bytes of a block of [`Strings`].
const BLOCK: usize = 64;

// The work of a scan counts toward the work of opening the file, which the tests
// bound: bytes read, and blocks, tree nodes and outcomes looked at.
use super::opened as step;

/// A set of offsets in the file.
struct Bits(Vec<u64>);

impl Bits {
    fn new(len: usize) -> Self {
        Self(vec![0; len / 64 + 1])
    }

    fn set(&mut self, at: usize) {
        self.0[at / 64] |= 1 << (at % 64);
    }

    fn get(&self, at: usize) -> bool {
        self.0
            .get(at / 64)
            .is_some_and(|word| word & (1 << (at % 64)) != 0)
    }
}

/// Where each literal string of the file ends. A byte is escaped where an odd run of
/// backslashes precedes it, and the depth after a byte counts the unescaped `(` up to
/// it less the unescaped `)`. A string's bytes are escaped as the file's bytes are,
/// its `(` being no backslash, so the string whose `(` is at `o` ends after the first
/// byte past `o` after which the depth is below the depth after `o`.
struct Strings {
    /// For each block: the depth before its first byte, and whether that byte is
    /// escaped.
    starts: Vec<(i64, bool)>,
    /// The least depth after any byte of each block, as a tree: node `i` holds the
    /// least of nodes `2i` and `2i + 1`, the blocks are the leaves from `leaves`.
    mins: Vec<i64>,
    leaves: usize,
}

impl Strings {
    /// Where the string whose `(` is at `at` ends, after its `)`.
    fn end(&self, data: &[u8], at: usize) -> Option<usize> {
        // The rest of the `(`'s block, read from the byte after it, which is not
        // escaped.
        let block_end = (at / BLOCK + 1) * BLOCK;
        let mut depth = 0_i64;
        let mut escaped = false;
        for (i, &b) in data.iter().enumerate().take(block_end).skip(at + 1) {
            step(1);
            if !escaped {
                if b == b'(' {
                    depth += 1;
                } else if b == b')' {
                    depth -= 1;
                    if depth < 0 {
                        return Some(i + 1);
                    }
                }
            }
            escaped = !escaped && b == b'\\';
        }
        // The first later block where the depth falls far enough.
        if block_end >= data.len() {
            return None;
        }
        let (start, _) = self.starts[block_end / BLOCK];
        let target = start - depth - 1;
        let block = self.first_at_most(block_end / BLOCK, target)?;
        let (mut depth, mut escaped) = self.starts[block];
        let from = block * BLOCK;
        for (i, &b) in data.iter().enumerate().take(from + BLOCK).skip(from) {
            step(1);
            if !escaped {
                if b == b'(' {
                    depth += 1;
                } else if b == b')' {
                    depth -= 1;
                    if depth <= target {
                        return Some(i + 1);
                    }
                }
            }
            escaped = !escaped && b == b'\\';
        }
        None
    }

    /// The first block from `from` with a depth of at most `target`.
    fn first_at_most(&self, from: usize, target: i64) -> Option<usize> {
        let mut i = from + self.leaves;
        if *self.mins.get(i)? > target {
            // Up past the subtrees to the right that hold no such depth...
            loop {
                while i & 1 == 1 {
                    i >>= 1;
                    step(1);
                }
                if i == 0 {
                    return None;
                }
                i += 1;
                step(1);
                if self.mins[i] <= target {
                    break;
                }
            }
            // ...and down to the first block of the one that does.
            while i < self.leaves {
                i *= 2;
                step(1);
                if self.mins[i] > target {
                    i += 1;
                }
            }
        }
        Some(i - self.leaves)
    }
}

/// What reading an array or dictionary finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// It ends at `end`. Kept for its `[` or `<<`, `nest` is how deep it and the
    /// arrays and dictionaries in it nest; kept for a byte it was read on from, how
    /// deep those after the byte nest.
    Ends { end: usize, nest: u8 },
    /// It cannot be read: a token in it is none, or it never ends.
    Fails,
}

impl Outcome {
    fn end(self) -> Option<usize> {
        match self {
            Self::Ends { end, .. } => Some(end),
            Self::Fails => None,
        }
    }
}

/// What a reading expects next in an open array or dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum State {
    Item,
    Key,
    Value,
}

/// How the dictionary a reading starts at reads its keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Keys {
    /// As skipping it does: a `/` and the regular characters after it are a key.
    Skipped,
    /// As reading it does: a key whose `#` escapes are not hex is skipped as an
    /// object, as other tokens that are no key are.
    Read,
}

/// What the scan finds of an object it is asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Found {
    /// Where the object ends.
    pub(crate) end: usize,
    /// For a dictionary, whether a key of it is /Root, and whether one is /Type.
    pub(crate) root: bool,
    pub(crate) kind: bool,
}

/// What a reading finds of the keys of the dictionary it starts at.
#[derive(Debug, Clone, Copy, Default)]
struct TopKeys {
    root: bool,
    kind: bool,
    /// Whether a key's `#` escapes are not hex, which the two ways of reading keys
    /// read differently.
    escaped: bool,
}

/// The last object the scan was asked about: the scan that rebuilds a table asks
/// about a header's object, then, where it is a dictionary, about it again with
/// its keys read, which finds the same where no key is escaped.
#[derive(Debug, Clone, Copy)]
struct Last {
    at: usize,
    keys: Keys,
    found: Option<Found>,
    escaped: bool,
}

/// An array or dictionary a reading has opened.
struct Frame {
    /// Its `[` or `<<`.
    opener: usize,
    dict: bool,
    /// For a dictionary, whether a key was read and its value was not yet.
    value: bool,
    /// How many arrays and dictionaries of the reading hold it.
    height: usize,
    /// How deep the arrays and dictionaries in it read so far nest, itself included.
    nest: u8,
    /// Whether its outcome is kept for a reading that starts at it.
    kept: bool,
    /// The bytes in it where another reading may meet this one, each with the state
    /// there and how deep the arrays and dictionaries read since nest.
    points: Vec<(usize, State, u8)>,
}

impl Frame {
    fn state(&self) -> State {
        match (self.dict, self.value) {
            (false, _) => State::Item,
            (true, false) => State::Key,
            (true, true) => State::Value,
        }
    }

    /// Notes an item read that holds arrays and dictionaries `nest` deep.
    fn holds(&mut self, nest: u8) {
        self.nest = self.nest.max(nest.saturating_add(1));
        for point in self.points.iter_mut().rev() {
            if point.2 >= nest {
                break;
            }
            point.2 = nest;
        }
    }
}

/// A token of regular characters that is no other object, which reading an object
/// skips (`skip_name_like` without a solidus).
struct Keyword;

impl Skippable for Keyword {
    fn skip(r: &mut Reader<'_>, _: bool) -> Option<()> {
        skip_name_like(r, false)
    }
}

/// What a token at an offset is.
enum Token {
    /// A token that ends at the offset.
    Ends(usize),
    /// A string, which ends at the offset.
    String(usize),
    /// An array or dictionary, whose `[` or `<<` ends at the offset.
    Opens(usize, bool),
    Fails,
}

/// The objects of a file, read for the scan that rebuilds its table.
pub(crate) struct Scan<'a> {
    data: &'a [u8],
    strings: Strings,
    /// The bytes after a string's `)` that ends a string opened at an escaped `(`
    /// too, and the ends of lines holding a `%`.
    meets: Bits,
    /// The ends of the lines holding a `%`, in order, where any comment ends.
    comment_ends: Vec<usize>,
    /// The `[` and `<<` of arrays and dictionaries that cannot be read.
    fails: Bits,
    /// The outcomes kept for readings that start at a `[` or `<<` after a header,
    /// read as skipping them does. A reading starts at the `[` or `<<` it is asked
    /// about after all readings that started before it, and only reads on from
    /// where it starts, so another can only meet one it starts at inside another
    /// object, after its header.
    kept: FxHashMap<usize, Outcome>,
    /// The outcomes of the arrays and dictionaries read on from a byte in `meets`,
    /// or from where one such closes, in a state and reading keys in a way: only a
    /// reading's outermost dictionary reads them as the reader does, and the rest
    /// of it may then end where skipping it fails.
    met: FxHashMap<(usize, State, Keys), Outcome>,
    last: Option<Last>,
    /// How the reading being read reads the keys of the dictionary it starts at, and
    /// what it found of them.
    keys: Keys,
    top: TopKeys,
}

impl<'a> Scan<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        let blocks = data.len().div_ceil(BLOCK).max(1);
        let leaves = blocks.next_power_of_two();
        let mut starts = Vec::with_capacity(blocks);
        let mut mins = vec![i64::MAX; 2 * leaves];
        let mut meets = Bits::new(data.len() + 1);
        let mut comment_ends = vec![];

        let mut depth = 0_i64;
        let mut escaped = false;
        let mut percent = false;
        // The depths at which strings opened at an escaped `(` have not ended yet, in
        // rising order: each ends where the depth falls below it.
        let mut escaped_opens: Vec<i64> = vec![];
        for (block, bytes) in data.chunks(BLOCK).enumerate() {
            starts.push((depth, escaped));
            let mut least = i64::MAX;
            for (i, &b) in bytes.iter().enumerate() {
                let at = block * BLOCK + i;
                if !escaped {
                    if b == b'(' {
                        depth += 1;
                    } else if b == b')' {
                        if escaped_opens.last() == Some(&depth) {
                            escaped_opens.pop();
                            meets.set(at + 1);
                        }
                        depth -= 1;
                    }
                } else if b == b'(' && escaped_opens.last() != Some(&depth) {
                    escaped_opens.push(depth);
                }
                escaped = !escaped && b == b'\\';
                least = least.min(depth);
                if is_eol_character(b) {
                    if percent {
                        meets.set(at);
                        comment_ends.push(at);
                    }
                    percent = false;
                } else if b == b'%' {
                    percent = true;
                }
            }
            mins[leaves + block] = least;
        }
        if percent {
            comment_ends.push(data.len());
        }
        step(data.len());
        for i in (1..leaves).rev() {
            mins[i] = mins[2 * i].min(mins[2 * i + 1]);
        }

        Self {
            data,
            strings: Strings {
                starts,
                mins,
                leaves,
            },
            meets,
            comment_ends,
            fails: Bits::new(data.len()),
            kept: FxHashMap::default(),
            met: FxHashMap::default(),
            last: None,
            keys: Keys::Skipped,
            top: TopKeys::default(),
        }
    }

    /// What the scan finds of the object at `at`, read as skipping it does; a
    /// dictionary at `at` reads its keys as `keys` says.
    pub(crate) fn object(&mut self, at: usize, keys: Keys) -> Option<Found> {
        if let Some(last) = self.last
            && last.at == at
            && (last.keys == keys || !last.escaped)
        {
            return last.found;
        }
        let at_end = |end| Found {
            end,
            root: false,
            kind: false,
        };
        let (found, escaped) = match self.token(at, false) {
            Token::Ends(end) | Token::String(end) => (Some(at_end(end)), false),
            Token::Fails => (None, false),
            Token::Opens(after, dict) => match (keys, self.known(at)) {
                // Kept without its keys, which are found again if asked for.
                (Keys::Skipped, Some(outcome)) => (outcome.end().map(at_end), true),
                _ => {
                    (self.keys, self.top) = (keys, TopKeys::default());
                    let top = self.frame(at, dict, 0, false);
                    let end = self.read(after, top);
                    let TopKeys {
                        root,
                        kind,
                        escaped,
                    } = self.top;
                    let found = end.map(|end| Found { end, root, kind });
                    (found, escaped)
                }
            },
        };
        self.last = Some(Last {
            at,
            keys,
            found,
            escaped,
        });
        found
    }

    /// The token at `at`, read as an item of an array or a dictionary's value where
    /// `reference`, which may be a reference, or as an object.
    fn token(&self, at: usize, reference: bool) -> Token {
        if reference && let Some(end) = self.skipped::<ObjRef>(at) {
            return Token::Ends(end);
        }
        let skipped = match self.data.get(at) {
            None => None,
            Some(b'n') => self.skipped::<Null>(at),
            Some(b't' | b'f') => self.skipped::<bool>(at),
            Some(b'/') => self.skipped::<Name<'_>>(at),
            Some(b'<') if self.data[at..].starts_with(b"<<") => return Token::Opens(at + 2, true),
            Some(b'<') => self.skipped::<crate::object::String<'_>>(at),
            Some(b'(') => {
                return self
                    .strings
                    .end(self.data, at)
                    .map_or(Token::Fails, Token::String);
            }
            Some(b'[') => return Token::Opens(at + 1, false),
            Some(b'.' | b'+' | b'-' | b'0'..=b'9') => self
                .skipped::<Number>(at)
                .or_else(|| self.skipped::<Keyword>(at)),
            Some(_) => self.skipped::<Keyword>(at),
        };
        skipped.map_or(Token::Fails, Token::Ends)
    }

    /// Where a `T` at `at` ends, if one is there, counting the bytes skipping it reads.
    fn skipped<T: Skippable>(&self, at: usize) -> Option<usize> {
        let mut r = Reader::new(self.data);
        r.jump(at);
        let skipped = T::skip(&mut r, false);
        step(r.offset() - at + 1);
        skipped.map(|()| r.offset())
    }

    /// The outcome known for the `[` or `<<` at `at`, read as skipping it does.
    fn known(&self, at: usize) -> Option<Outcome> {
        step(1);
        if self.fails.get(at) {
            return Some(Outcome::Fails);
        }
        self.kept.get(&at).copied()
    }

    fn frame(&self, opener: usize, dict: bool, height: usize, kept: bool) -> Frame {
        Frame {
            opener,
            dict,
            value: false,
            height,
            nest: 1,
            kept,
            points: vec![],
        }
    }

    /// Whether the `[` or `<<` at `at` is a header's object, which a reading starts
    /// at, so that its outcome is kept.
    fn after_header(&self, at: usize) -> bool {
        let before = &self.data[..at];
        let end = before
            .iter()
            .rposition(|b| !is_white_space_character(*b))
            .map_or(0, |i| i + 1);
        step(at - end + 1);
        before[..end].ends_with(b"obj")
    }

    /// Reads on from `at` in the array or dictionary `top`, a reading's outermost,
    /// to where it ends.
    fn read(&mut self, mut at: usize, top: Frame) -> Option<usize> {
        let mut stack = VecDeque::from([top]);
        // The outcome of the object the reading started at, once known.
        let mut outcome = None;
        while let Some(frame) = stack.back() {
            let (state, top) = (frame.state(), frame.height == 0);
            let mut met = false;
            at = self.space(at, &mut met);
            if met && self.meet(&mut stack, &mut at, &mut outcome) {
                continue;
            }
            let token = match state {
                State::Item if self.data.get(at) == Some(&b']') => {
                    step(1);
                    let end = at + 1;
                    self.close(&mut stack, &mut at, end, false, &mut outcome);
                    continue;
                }
                State::Key if self.data[at..].starts_with(b">>") => {
                    step(2);
                    let end = at + 2;
                    self.close(&mut stack, &mut at, end, false, &mut outcome);
                    continue;
                }
                State::Key if self.data.get(at) == Some(&b'/') => {
                    let mut r = Reader::new(self.data);
                    r.jump(at);
                    let name = r.skip::<Name<'_>>(false).unwrap_or_default();
                    step(name.len());
                    let mut key = true;
                    if top {
                        match Name::new(&name[1..]) {
                            Some(name) => {
                                self.top.root |= name.as_ref() == ROOT;
                                self.top.kind |= name.as_ref() == TYPE;
                            }
                            // Read as the reader reads keys, a name whose escapes
                            // are not hex is no key, and is skipped as an object.
                            None => {
                                self.top.escaped = true;
                                key = self.keys == Keys::Skipped;
                            }
                        }
                    }
                    stack.back_mut().unwrap().value = key;
                    at = r.offset();
                    continue;
                }
                State::Key => self.token(at, false),
                State::Item | State::Value => self.token(at, true),
            };
            stack.back_mut().unwrap().value = false;
            match token {
                Token::Ends(end) => at = end,
                Token::String(end) => {
                    at = end;
                    if self.meets.get(end) {
                        self.meet(&mut stack, &mut at, &mut outcome);
                    }
                }
                Token::Fails => self.fail(&mut stack, &mut outcome),
                Token::Opens(after, dict) => {
                    let height = stack.back().unwrap().height + 1;
                    match self.known(at) {
                        Some(Outcome::Fails) => self.fail(&mut stack, &mut outcome),
                        Some(Outcome::Ends { end, nest }) => {
                            self.deepest(&mut stack, height + nest as usize - 1, &mut outcome);
                            if let Some(frame) = stack.back_mut() {
                                frame.holds(nest);
                            }
                            at = end;
                        }
                        None => {
                            let kept = self.after_header(at);
                            stack.push_back(self.frame(at, dict, height, kept));
                            self.deepest(&mut stack, height, &mut outcome);
                            at = after;
                        }
                    }
                }
            }
        }
        outcome.flatten()
    }

    /// Skips white space and comments from `at`; `met` where it passes a marked end
    /// of line.
    fn space(&self, mut at: usize, met: &mut bool) -> usize {
        while let Some(&b) = self.data.get(at) {
            if is_white_space_character(b) {
                step(1);
                *met |= is_eol_character(b) && self.meets.get(at);
                at += 1;
            } else if b == b'%' {
                // A search among the ends of the lines.
                step(1 + (usize::BITS - self.comment_ends.len().leading_zeros()) as usize);
                at = self.comment_end(at);
            } else {
                break;
            }
        }
        at
    }

    /// Where the comment whose `%` is at `at` ends: at the end of its line, which
    /// holds a `%`, found among those lines' ends rather than by reading the line, as
    /// readings that leave strings in a line enter a comment at each `%` of it.
    fn comment_end(&self, at: usize) -> usize {
        let ends = &self.comment_ends;
        ends[ends.partition_point(|end| *end < at)]
    }

    /// At a byte where another reading may have met this one: takes the outcome
    /// kept for the innermost frame read on from it, if there is one, and returns
    /// whether it did; otherwise marks the byte, for the outcome to be kept.
    fn meet(
        &mut self,
        stack: &mut VecDeque<Frame>,
        at: &mut usize,
        outcome: &mut Option<Option<usize>>,
    ) -> bool {
        let Some(frame) = stack.back_mut() else {
            return false;
        };
        let (state, height) = (frame.state(), frame.height);
        let keys = self.keys_of(height);
        step(1);
        match self.met.get(&(*at, state, keys)).copied() {
            Some(Outcome::Fails) => {
                self.fail(stack, outcome);
                true
            }
            Some(Outcome::Ends { end, nest }) => {
                frame.holds(nest);
                self.deepest(stack, height + nest as usize, outcome);
                if !stack.is_empty() {
                    self.close(stack, at, end, true, outcome);
                }
                true
            }
            None => {
                frame.points.push((*at, state, 0));
                false
            }
        }
    }

    /// Closes the innermost frame at `end`, where the reading goes on in the frame
    /// around it; `met` where its outcome was taken from another reading's.
    fn close(
        &mut self,
        stack: &mut VecDeque<Frame>,
        at: &mut usize,
        end: usize,
        met: bool,
        outcome: &mut Option<Option<usize>>,
    ) {
        let frame = stack.pop_back().unwrap();
        *at = end;
        if frame.kept {
            let nest = frame.nest;
            self.kept.insert(frame.opener, Outcome::Ends { end, nest });
        }
        let keys = self.keys_of(frame.height);
        for &(point, state, nest) in &frame.points {
            self.met
                .insert((point, state, keys), Outcome::Ends { end, nest });
        }
        if frame.height == 0 {
            *outcome = Some(Some(end));
        }
        // Where a reading met another in the frame, the two close it alike and go on
        // from its end, where they meet again if their frames around it are alike.
        // Where the frame around it was dropped, nothing is left to read.
        if let Some(around) = stack.back_mut() {
            around.holds(frame.nest);
            around.value = false;
            if met || !frame.points.is_empty() {
                self.meet(stack, at, outcome);
            }
        }
    }

    /// How a frame `height` deep in the reading reads its keys.
    fn keys_of(&self, height: usize) -> Keys {
        if height == 0 {
            self.keys
        } else {
            Keys::Skipped
        }
    }

    /// Drops the frames that hold arrays or dictionaries too deep, now that one is at
    /// `deepest`.
    fn deepest(
        &mut self,
        stack: &mut VecDeque<Frame>,
        deepest: usize,
        outcome: &mut Option<Option<usize>>,
    ) {
        while let Some(frame) = stack.front() {
            if deepest - frame.height < MAX_DEPTH {
                break;
            }
            let frame = stack.pop_front().unwrap();
            self.failed(&frame, outcome);
        }
    }

    /// Fails every open frame: the innermost cannot be read, nor can those holding it.
    fn fail(&mut self, stack: &mut VecDeque<Frame>, outcome: &mut Option<Option<usize>>) {
        while let Some(frame) = stack.pop_back() {
            self.failed(&frame, outcome);
        }
    }

    fn failed(&mut self, frame: &Frame, outcome: &mut Option<Option<usize>>) {
        if frame.kept {
            self.kept.insert(frame.opener, Outcome::Fails);
        }
        // Only an outermost frame reads its keys as the reader does, which fails
        // as skipping it does where no key is escaped.
        if frame.height > 0 || self.keys == Keys::Skipped || !self.top.escaped {
            self.fails.set(frame.opener);
        }
        let keys = self.keys_of(frame.height);
        for &(point, state, _) in &frame.points {
            self.met.insert((point, state, keys), Outcome::Fails);
        }
        if frame.height == 0 {
            *outcome = Some(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Dict, Object};
    use crate::reader::{ReaderContext, ReaderExt};
    use alloc::string::String;

    /// A generator of pseudo-random numbers, the same in every run.
    struct Random(u64);

    impl Random {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
    }

    /// A file of `len` pieces of tokens, strings, escapes, comments and headers that
    /// open, close and nest in every way.
    fn file(random: &mut Random, len: usize) -> Vec<u8> {
        const PIECES: &[&str] = &[
            "(",
            ")",
            "\\(",
            "\\)",
            "\\",
            "\\\\",
            "[",
            "]",
            "<<",
            ">>",
            "<",
            ">",
            "%",
            "\n",
            "\r",
            " ",
            " ",
            " ",
            "/A",
            "/Root",
            "/Type",
            "/R#6Fot",
            "/B#zz",
            "/K#41",
            "1",
            "0",
            "R",
            "5 0 obj ",
            "6 0 obj",
            "obj",
            "null",
            "true",
            "fals",
            "1.5",
            "-",
            "+",
            "<ab>",
            "<a b",
            "endobj",
            "{",
            "}",
            "x",
            "1 0 R",
            "12 0 R ",
            "0 R",
            "#",
            ".",
            "<< /B#zz /A >>",
            "<< /B#zz >>",
        ];
        let mut out = String::new();
        for _ in 0..len {
            match random.below(40) {
                // Containers nested as deep as reading allows, and deeper.
                0 => out.push_str(&"[ ".repeat(60 + random.below(8))),
                1 => out.push_str(&"<< /A ".repeat(60 + random.below(8))),
                2 => out.push_str(&"] ".repeat(60 + random.below(8))),
                _ => out.push_str(PIECES[random.below(PIECES.len())]),
            }
        }
        out.into_bytes()
    }

    /// Where the object at `at` ends as skipping it on the whole rest of the file
    /// finds it.
    fn skipped(data: &[u8], at: usize) -> Option<usize> {
        let mut r = Reader::new(data);
        r.jump(at);
        r.skip::<Object<'_>>(false).map(|_| r.offset())
    }

    /// What reading the dictionary at `at` on the whole rest of the file finds.
    fn read(data: &[u8], at: usize) -> Option<Found> {
        let mut r = Reader::new(data);
        r.jump(at);
        let dict = r.read_with_context::<Dict<'_>>(&ReaderContext::dummy())?;
        Some(Found {
            end: r.offset(),
            root: dict.contains_key(ROOT),
            kind: dict.contains_key(TYPE),
        })
    }

    // A scan finds where each object ends as reading it on the whole rest of the file
    // does, whatever it was asked before.
    #[test]
    fn scan_ends_objects_as_reading_them_does() {
        let mut random = Random(0x9e37_79b9_7f4a_7c15);
        for case in 0..3000 {
            let len = 1 + random.below(120);
            let data = file(&mut random, len);
            let mut scan = Scan::new(&data);
            let mut positions: Vec<usize> = (0..data.len()).collect();
            for i in (1..positions.len()).rev() {
                positions.swap(i, random.below(i + 1));
            }
            for at in positions {
                // Both ways of reading keys in turn, as the scan that rebuilds a
                // table asks about a header's dictionary, in either order.
                let mut modes = vec![Keys::Skipped];
                if data[at..].starts_with(b"<<") {
                    modes.push(Keys::Read);
                }
                if random.below(2) == 0 {
                    modes.reverse();
                }
                for keys in modes {
                    let found = scan.object(at, keys);
                    let expected = match keys {
                        Keys::Skipped => skipped(&data, at),
                        Keys::Read => read(&data, at).map(|read| read.end),
                    };
                    assert_eq!(
                        found.map(|found| found.end),
                        expected,
                        "case {case}, {keys:?} at {at} of {:?}",
                        String::from_utf8_lossy(&data)
                    );
                    if keys == Keys::Read {
                        let keys = |found: Option<Found>| found.map(|f| (f.root, f.kind));
                        assert_eq!(keys(found), keys(read(&data, at)), "case {case} at {at}");
                    }
                }
            }
        }
    }
}
