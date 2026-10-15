//! Where the objects of a damaged file end, for the scan that rebuilds its table.
//!
//! The scan asks, at each object header and each `<<` it meets, whether an object
//! starts there and where it ends, reading the rest of the file as reading the
//! object does. Objects of a damaged file nest in each other's strings, overlap, or
//! never end, so reading each from its start reads much of the file again for each.
//! [`Scan`] answers each question as reading the object on the whole rest of the
//! file would, reading every part of the file a bounded number of times:
//!
//! - An object reads as the reader reads it: a token that is no array or dictionary
//!   is read by the reader itself, and an array or dictionary, at any depth, reads
//!   its items and keys as the one reading of them that reading and skipping share
//!   (`parse_dict_with` in `object::dict`): a name whose `#` escapes are not hex is
//!   no key, and is skipped as an object.
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
use crate::object::{MAX_OBJECT_NESTING_DEPTH, Name, ObjRef, Object, Skip, leaf};
use crate::reader::{Reader, ReaderContext, ReaderExt};
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
    /// arrays and dictionaries in it nest, and `keys` the keys of it that the
    /// scan's callers ask about; kept for a byte it was read on from, how deep those
    /// after the byte nest, and those keys after the byte.
    Ends { end: usize, nest: u8, keys: Keys },
    /// It cannot be read: a token in it is none, or it never ends.
    Fails,
}

/// Whether a dictionary, or a part of one, has /Root and /Type keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Keys {
    root: bool,
    kind: bool,
}

impl Keys {
    fn or(self, other: Self) -> Self {
        Self {
            root: self.root || other.root,
            kind: self.kind || other.kind,
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

/// What the scan finds of an object it is asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Found {
    /// Where the object ends.
    pub(crate) end: usize,
    /// For a dictionary, whether a key of it is /Root, and whether one is /Type.
    pub(crate) root: bool,
    pub(crate) kind: bool,
}

impl Found {
    fn new(end: usize, keys: Keys) -> Self {
        Self {
            end,
            root: keys.root,
            kind: keys.kind,
        }
    }
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
    /// The keys read in it, and for each, how many of `points` were before the last
    /// one read: those points have it after them.
    keys: Keys,
    before: [usize; 2],
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

    /// Notes `keys`, read after every point so far.
    fn read_keys(&mut self, keys: Keys) {
        self.keys = self.keys.or(keys);
        let points = self.points.len();
        if keys.root {
            self.before[0] = points;
        }
        if keys.kind {
            self.before[1] = points;
        }
    }

    /// The keys read after the `i`-th point.
    fn keys_after(&self, i: usize) -> Keys {
        Keys {
            root: i < self.before[0],
            kind: i < self.before[1],
        }
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
    /// The outcomes kept for readings that start at a `[` or `<<` that the scan was
    /// asked about, or that is after a header. A reading starts at the `[` or `<<` it
    /// is asked about after all readings that started before it, and only reads on
    /// from where it starts, so another can only meet one it starts at inside
    /// another object, after its header, or one asked about before.
    kept: FxHashMap<usize, Outcome>,
    /// The outcomes of the arrays and dictionaries read on from a byte in `meets`,
    /// or from where one such closes, in a state.
    met: FxHashMap<(usize, State), Outcome>,
    /// The last object the scan was asked about, and what it found: the scan that
    /// rebuilds a table asks about a header's object, then, where it is a dictionary,
    /// about it again for its keys.
    last: Option<(usize, Option<Found>)>,
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
        }
    }

    /// What the scan finds of the object at `at`, after white space and comments,
    /// read as reading an object there does (`read_object` in the parent module):
    /// `None` where that fails.
    pub(crate) fn object(&mut self, at: usize) -> Option<Found> {
        let at = self.space(at, &mut false);
        if let Some((last, found)) = self.last
            && last == at
        {
            return found;
        }
        let found = match self.data.get(at) {
            Some(b'(') => self
                .strings
                .end(self.data, at)
                .map(|end| Found::new(end, Keys::default())),
            Some(b'[') => self.container(at, at + 1, false),
            Some(b'<') if self.data[at..].starts_with(b"<<") => self.container(at, at + 2, true),
            _ => {
                // A name, string, number, keyword, `null` or boolean: read by the
                // reader, which reads a name with escapes that are not hex as none.
                let mut r = Reader::new(self.data);
                r.jump(at);
                let read = r.read_with_context::<Object<'_>>(&ReaderContext::dummy());
                step(r.offset() - at + 1);
                read.map(|_| Found::new(r.offset(), Keys::default()))
            }
        };
        self.last = Some((at, found));
        found
    }

    /// What the scan finds of the array or dictionary whose `[` or `<<` is at `at` and
    /// ends at `after`.
    fn container(&mut self, at: usize, after: usize, dict: bool) -> Option<Found> {
        match self.known(at) {
            Some(Outcome::Ends { end, keys, .. }) => Some(Found::new(end, keys)),
            Some(Outcome::Fails) => None,
            None => {
                let top = self.frame(at, dict, 0, false);
                self.read(after, top)
            }
        }
    }

    /// The token at `at`, read as an item of an array or a dictionary's value where
    /// `reference`, which may be a reference, or as an object: as skipping it reads
    /// it, but for a literal string, whose end is looked up.
    fn token(&self, at: usize, reference: bool) -> Token {
        if reference && let Some(end) = self.skipped(at, |r| r.skip::<ObjRef>(false)) {
            return Token::Ends(end);
        }
        match self.data.get(at) {
            None => Token::Fails,
            Some(b'<') if self.data[at..].starts_with(b"<<") => Token::Opens(at + 2, true),
            Some(b'[') => Token::Opens(at + 1, false),
            Some(b'(') => self
                .strings
                .end(self.data, at)
                .map_or(Token::Fails, Token::String),
            Some(_) => self
                .skipped(at, |r| leaf(r, false, Skip))
                .map_or(Token::Fails, Token::Ends),
        }
    }

    /// The name at `at`, read as reading a dictionary reads a key, and where it ends.
    fn key(&self, at: usize) -> Option<(Name<'a>, usize)> {
        let mut r = Reader::new(self.data);
        r.jump(at);
        let key = r.read_without_context::<Name<'a>>();
        step(r.offset() - at + 1);
        key.map(|key| (key, r.offset()))
    }

    /// Where what `skip` reads at `at` ends, if it reads one, counting the bytes it
    /// reads.
    fn skipped<T>(
        &self,
        at: usize,
        skip: impl FnOnce(&mut Reader<'a>) -> Option<T>,
    ) -> Option<usize> {
        let mut r = Reader::new(self.data);
        r.jump(at);
        let skipped = skip(&mut r);
        step(r.offset() - at + 1);
        skipped.map(|_| r.offset())
    }

    /// The outcome known for the `[` or `<<` at `at`.
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
            keys: Keys::default(),
            before: [0; 2],
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
    fn read(&mut self, mut at: usize, top: Frame) -> Option<Found> {
        let mut stack = VecDeque::from([top]);
        // The outcome of the object the reading started at, once known.
        let mut outcome = None;
        while let Some(frame) = stack.back() {
            let state = frame.state();
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
                // As reading a dictionary reads it, a name whose escapes are not hex
                // is no key, and is skipped as an object.
                State::Key if let Some((key, end)) = self.key(at) => {
                    let frame = stack.back_mut().unwrap();
                    frame.read_keys(Keys {
                        root: key.as_ref() == ROOT,
                        kind: key.as_ref() == TYPE,
                    });
                    frame.value = true;
                    at = end;
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
                        Some(Outcome::Ends { end, nest, .. }) => {
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
        outcome: &mut Option<Option<Found>>,
    ) -> bool {
        let Some(frame) = stack.back_mut() else {
            return false;
        };
        let (state, height) = (frame.state(), frame.height);
        step(1);
        match self.met.get(&(*at, state)).copied() {
            Some(Outcome::Fails) => {
                self.fail(stack, outcome);
                true
            }
            Some(Outcome::Ends { end, nest, keys }) => {
                frame.holds(nest);
                frame.read_keys(keys);
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
        outcome: &mut Option<Option<Found>>,
    ) {
        let frame = stack.pop_back().unwrap();
        *at = end;
        let nest = frame.nest;
        if frame.kept {
            let keys = frame.keys;
            self.kept
                .insert(frame.opener, Outcome::Ends { end, nest, keys });
        }
        for (i, &(point, state, nest)) in frame.points.iter().enumerate() {
            let keys = frame.keys_after(i);
            self.met
                .insert((point, state), Outcome::Ends { end, nest, keys });
        }
        if frame.height == 0 {
            *outcome = Some(Some(Found::new(end, frame.keys)));
        }
        // Where a reading met another in the frame, the two close it alike and go on
        // from its end, where they meet again if their frames around it are alike.
        // Where the frame around it was dropped, nothing is left to read.
        if let Some(around) = stack.back_mut() {
            around.holds(nest);
            around.value = false;
            if met || !frame.points.is_empty() {
                self.meet(stack, at, outcome);
            }
        }
    }

    /// Drops the frames that hold arrays or dictionaries too deep, now that one is at
    /// `deepest`.
    fn deepest(
        &mut self,
        stack: &mut VecDeque<Frame>,
        deepest: usize,
        outcome: &mut Option<Option<Found>>,
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
    fn fail(&mut self, stack: &mut VecDeque<Frame>, outcome: &mut Option<Option<Found>>) {
        while let Some(frame) = stack.pop_back() {
            self.failed(&frame, outcome);
        }
    }

    fn failed(&mut self, frame: &Frame, outcome: &mut Option<Option<Found>>) {
        if frame.kept {
            self.kept.insert(frame.opener, Outcome::Fails);
        }
        self.fails.set(frame.opener);
        for &(point, state, _) in &frame.points {
            self.met.insert((point, state), Outcome::Fails);
        }
        if frame.height == 0 {
            *outcome = Some(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::body::Read;
    use super::super::{object_in, read_object, rebuilt_header};
    use super::*;
    use crate::object::Dict;
    use crate::object::ObjectIdentifier;
    use crate::object::indirect::IndirectObject;
    use crate::object::parsed::{DictNode, Node};
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
    /// open, close and nest in every way, and the tokens reading reads leniently:
    /// names whose escapes are not hex, which are no keys, bad numbers, keywords,
    /// brackets that do not match.
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
            "/T#79pe",
            "/B#zz",
            "/B#4",
            "/K#41",
            "#",
            "1",
            "0",
            "R",
            "5 0 obj ",
            "6 0 obj",
            "obj",
            "endobj",
            "stream",
            "endstream",
            "<< /Length 1 >> stream\n",
            "null",
            "true",
            "fals",
            "1.5",
            "1.2.3",
            "--5",
            "+-",
            "-.x",
            "5-.5",
            "1e3",
            "-",
            "+",
            ".",
            "<ab>",
            "<a b",
            "<a#>",
            "{",
            "}",
            "x",
            "1 0 R",
            "12 0 R ",
            "0 R",
            "<< /B#zz /A >>",
            "<< /B#zz >>",
            "<< /B#zz /Root 1 0 R >>",
            "<< /A ( /Root \\( ) /Root 1 0 R >>",
            // Numbers and headers with signs where a number ends, and other tokens
            // that start like numbers or names.
            "3-",
            "0-",
            "3- 0 obj",
            "3 0- obj",
            "+3 -0 obj",
            "3.0 0 obj",
            "-1 0 obj",
            "99999999999 0 obj",
            "4- 0 R",
            "4 0- R",
            "1.0 0 R",
            "-x",
            "+ ",
            "-(",
            "+<",
            "1.5-",
            "1-.5",
            "--",
            ".-",
            "/a#zz",
            "/a#4",
            "/",
            "#zz",
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

    /// What reading the object at `at` of `data` finds, as an entry's object is read
    /// (`body::Read::from`): where it ends, and for a dictionary whether it has a
    /// /Root and a /Type key.
    fn entry_read(data: &[u8], at: usize) -> Option<Found> {
        let mut r = Reader::new(data);
        r.jump(at);
        let ctx = ReaderContext::dummy();
        let syntax = Read::from(
            &mut r,
            false,
            |r| DictNode::read(r, data, &ctx),
            |r| Node::read(r, data, &ctx),
        )?;
        let dict = match &syntax {
            Read::Dict(dict) | Read::Stream { dict, .. } => Some(dict),
            Read::Object(_) => None,
        };
        Some(Found {
            end: r.offset(),
            root: dict.is_some_and(|dict| dict.get(ROOT).is_some()),
            kind: dict.is_some_and(|dict| dict.get(TYPE).is_some()),
        })
    }

    /// What reading the object at `at` of `data` after its header finds, as an
    /// entry's object is read (`body::Read::from`) and as `XRef::get_with` reads
    /// one that is not a stream (`IndirectObject`), which agree: where the object
    /// ends.
    fn headed_read(data: &[u8], at: usize) -> Option<usize> {
        let mut r = Reader::new(data);
        r.jump(at);
        let ctx = ReaderContext::dummy();
        let syntax = Read::from(
            &mut r,
            true,
            |r| DictNode::read(r, data, &ctx),
            |r| Node::read(r, data, &ctx),
        );
        let end = r.offset();
        if !matches!(syntax, Some(Read::Stream { .. })) {
            let mut indirect = Reader::new(data);
            indirect.jump(at);
            let read =
                indirect.read_with_context::<IndirectObject<Object<'_>>>(&ReaderContext::dummy());
            assert_eq!(read.is_some(), syntax.is_some(), "at {at}");
        }
        syntax.map(|_| end)
    }

    /// The header at `at` of `data` and whether its object is read, as rebuilding a
    /// table finds them (`rebuilt_header`).
    fn headed_rebuilt(data: &[u8], scan: &mut Scan<'_>, at: usize) -> Option<ObjectIdentifier> {
        let mut r = Reader::new(data);
        r.jump(at);
        let (header, object) = rebuilt_header(&mut r, scan).flatten()?;
        object.then_some(header)
    }

    /// Reads every item of `object`, an array or dictionary, and of those in it.
    fn read_items(object: Object<'_>) {
        match object {
            Object::Array(array) => array.raw_iter().for_each(|item| {
                if let crate::object::MaybeRef::NotRef(item) = item {
                    read_items(item);
                }
            }),
            Object::Dict(dict) => dict.entries().for_each(|(_, item)| {
                if let crate::object::MaybeRef::NotRef(item) = item {
                    read_items(item);
                }
            }),
            _ => {}
        }
    }

    /// Where the object at `at` of `data` ends as checking a table reads it, which
    /// skips a dictionary.
    fn checked(data: &[u8], at: usize) -> Option<usize> {
        let mut r = Reader::new(data);
        r.jump(at);
        read_object(
            &mut r,
            false,
            |r| r.skip::<Dict<'_>>(false),
            |r| r.read_with_context::<Object<'_>>(&ReaderContext::dummy()),
        )
        .map(|_| r.offset())
    }

    // What the rebuild of a table finds of an object, through a scan, and what its
    // check finds, which skips a dictionary, are what reading the object as an entry's
    // object finds: wherever it starts, whatever the scan was asked before, and on the
    // whole rest of the file or only up to the next offset an entry names.
    #[test]
    fn rebuilding_and_checking_a_table_read_objects_as_reading_them_does() {
        const CASES: usize = 3000;
        let workers = std::thread::available_parallelism().map_or(1, usize::from);
        std::thread::scope(|scope| {
            for worker in 0..workers {
                scope.spawn(move || {
                    for case in (worker..CASES).step_by(workers) {
                        compare(case);
                    }
                });
            }
        });
    }

    /// Compares, for `case`'s file, what the scan and the check find with reading.
    fn compare(case: usize) {
        let mut random = Random(0x9e37_79b9_7f4a_7c15 ^ (case as u64 + 1) << 20);
        let len = 1 + random.below(120);
        let data = file(&mut random, len);
        let shown = || String::from_utf8_lossy(&data).into_owned();
        let mut scan = Scan::new(&data);
        let mut positions: Vec<usize> = (0..data.len()).collect();
        for i in (1..positions.len()).rev() {
            positions.swap(i, random.below(i + 1));
        }
        for at in positions {
            let read = entry_read(&data, at);
            // The scan, asked twice, as the scan that rebuilds a table asks about a
            // header's object and then about a dictionary there.
            for _ in 0..2 {
                let found = scan.object(at);
                assert_eq!(
                    found,
                    read,
                    "case {case}, scan at {at}: {:?} of {:?}",
                    String::from_utf8_lossy(&data[at..]),
                    shown()
                );
            }
            // The check on the whole rest of the file, and up to an offset where
            // the next object may start.
            let whole = checked(&data, at);
            assert_eq!(
                whole,
                read.map(|read| read.end),
                "case {case}, check at {at} of {:?}",
                shown()
            );
            let next = at + random.below(data.len() - at + 1);
            let checked = checked(&data[..next], at);
            assert_eq!(
                checked,
                entry_read(&data[..next], at).map(|read| read.end),
                "case {case}, check at {at} to {next} of {:?}",
                shown()
            );
            // What reads to its end before that offset is read there.
            if let Some(read) = read
                && read.end <= next
            {
                assert_eq!(checked, Some(read.end), "case {case}");
            }
            // The items of an array and the entries of a dictionary read, at any
            // depth, as skipping the array or dictionary read them.
            let mut r = Reader::new(&data);
            r.jump(at);
            if let Some(object) = r.read_with_context::<Object<'_>>(&ReaderContext::dummy()) {
                // And parsing the object finds what reading it does, to the same end.
                let end = r.offset();
                let mut parse = Reader::new(&data);
                parse.jump(at);
                let ctx = ReaderContext::dummy();
                let node = Node::read(&mut parse, &data, &ctx);
                assert_eq!(
                    parse.offset(),
                    end,
                    "case {case}, parse at {at} of {:?}",
                    shown()
                );
                let parsed = node.map(|node| format!("{}", node.object(&data, &ctx)));
                assert_eq!(
                    parsed,
                    Some(format!("{object}")),
                    "case {case}, parse at {at} of {:?}",
                    shown()
                );
                read_items(object);
            }
            // An object after a header: its header and object are read alike by
            // checking the table, on the whole rest of the file and up to an offset,
            // by rebuilding it, and by reading the object.
            let read = headed_read(&data, at);
            let header = Reader::new(&data[at..]).read_without_context::<ObjectIdentifier>();
            assert_eq!(
                object_in(&data[at..]),
                read.and(header),
                "case {case}, header check at {at} of {:?}",
                shown()
            );
            assert_eq!(
                object_in(&data[at..next]),
                headed_read(&data[..next], at).and(header),
                "case {case}, header check at {at} to {next} of {:?}",
                shown()
            );
            assert_eq!(
                headed_rebuilt(&data, &mut scan, at),
                read.and(header),
                "case {case}, header scan at {at} of {:?}",
                shown()
            );
        }
    }
}
