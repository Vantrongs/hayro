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
//!   (`parse_dict_with` in `object::dict`): a token in a key's place that is no
//!   name is skipped as an object.
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
//! (`rebuilding_and_checking_a_table_read_objects_as_reading_them_does`).

use crate::hash;
use crate::object::dict::keys::{ROOT, TYPE};
use crate::object::{MAX_OBJECT_NESTING_DEPTH, Name, ObjRef, Object, Skip, leaf};
use crate::reader::{Reader, ReaderContext, ReaderExt};
use crate::trivia::{is_eol_character, is_white_space_character};
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
#[cfg(feature = "unsafe")]
use fearless_simd::{Simd, SimdBase, u8x64};

/// How deep arrays and dictionaries nest in an object that can be read.
const MAX_DEPTH: usize = MAX_OBJECT_NESTING_DEPTH;

/// The bytes of a block of [`Strings`].
const BLOCK: usize = 64;

// The work of a scan counts toward the work of opening the file, which the tests
// bound: bytes of white space and strings read, and blocks, tree nodes and outcomes
// looked at; the bytes its readers lex, they count (`crate::work::EXAMINED`).
use super::opened as step;

/// Whether the first pass of a scan reads `b` for more than ending an escape: a byte
/// that opens or closes a string, escapes, ends a line, or starts a comment.
#[inline(always)]
fn is_mark(b: u8) -> bool {
    (b == b'(') | (b == b')') | (b == b'\\') | (b == b'\r') | (b == b'\n') | (b == b'%')
}

/// The marks of `block` ([`is_mark`]), as bits: bit `i` is set where `block[i]` is
/// one. The bytes are compared all at once, and each 8 of the flags made into 8 bits
/// by one multiplication, rather than one byte at a time: the first pass's work on
/// a byte that is no mark, most of a file, in streams.
fn marked(block: &[u8; BLOCK]) -> u64 {
    let mut flags = [0_u8; BLOCK];
    for (flag, &b) in flags.iter_mut().zip(block) {
        *flag = u8::from(is_mark(b));
    }
    let mut marks = 0;
    for (i, eight) in flags.as_chunks::<8>().0.iter().enumerate() {
        let eight = u64::from_le_bytes(*eight);
        // Flag `j`, at bit `8 j`, moves to bit `56 + j`; no two flags add up.
        marks |= (eight.wrapping_mul(0x0102_0408_1020_4080) >> 56) << (8 * i);
    }
    marks
}

/// The bytes of a block that the first pass of a scan reads, by kind, as bits: bit
/// `i` is set where byte `i` of the block is one of the kind. Any other byte only ends
/// an escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Kinds {
    open: u64,
    close: u64,
    backslash: u64,
    percent: u64,
    eol: u64,
}

impl Kinds {
    /// The kinds of `bytes`, at most a block: those of its marks ([`is_mark`]),
    /// found first.
    fn of(bytes: &[u8]) -> Self {
        let mut marks = match <&[u8; BLOCK]>::try_from(bytes) {
            Ok(block) => marked(block),
            Err(_) => bytes
                .iter()
                .enumerate()
                .filter(|(_, b)| is_mark(**b))
                .fold(0, |marks, (i, _)| marks | 1 << i),
        };
        let mut kinds = Self::default();
        while marks != 0 {
            let i = marks.trailing_zeros();
            marks &= marks - 1;
            let (bit, b) = (1 << i, bytes[i as usize]);
            kinds.open |= bit * u64::from(b == b'(');
            kinds.close |= bit * u64::from(b == b')');
            kinds.backslash |= bit * u64::from(b == b'\\');
            kinds.percent |= bit * u64::from(b == b'%');
            kinds.eol |= bit * u64::from(is_eol_character(b));
        }
        kinds
    }

    /// The kinds of a whole block, its bytes compared all at once.
    #[cfg(feature = "unsafe")]
    #[inline(always)]
    fn of_block<S: Simd>(simd: S, block: &[u8; BLOCK]) -> Self {
        let bytes = u8x64::from_slice(simd, block);
        let of = |b| simd.to_bitmask_mask8x64(simd.simd_eq_u8x64(bytes, simd.splat_u8x64(b)));
        Self {
            open: of(b'('),
            close: of(b')'),
            backslash: of(b'\\'),
            percent: of(b'%'),
            eol: of(b'\r') | of(b'\n'),
        }
    }
}

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
    /// The depth before the first byte of each block.
    starts: Vec<i64>,
    /// The blocks whose first byte is escaped.
    escaped: Bits,
    /// The least depth after any byte of each block, as a tree: node `i` holds the
    /// least of nodes `2i` and `2i + 1`, the blocks are the leaves from `blocks`.
    /// Their count is not rounded up to a power of two, so a node whose leaves are
    /// not all at one level holds no range of blocks; [`Self::first_at_most`] only
    /// looks at nodes that do.
    mins: Vec<i64>,
    blocks: usize,
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
        let target = self.starts[block_end / BLOCK] - depth - 1;
        let block = self.first_at_most(block_end / BLOCK, target)?;
        let mut depth = self.starts[block];
        let mut escaped = self.escaped.get(block);
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
        // The nodes that hold the blocks from `from` to the last, in order: those
        // met on the left edge, then those on the right one, last met first.
        let (mut left, mut right) = (from + self.blocks, 2 * self.blocks);
        let mut rights = [0; usize::BITS as usize];
        let mut met = 0;
        let mut found = None;
        while left < right && found.is_none() {
            step(1);
            if left & 1 == 1 {
                if self.mins[left] <= target {
                    found = Some(left);
                }
                left += 1;
            }
            if right & 1 == 1 {
                right -= 1;
                rights[met] = right;
                met += 1;
            }
            left >>= 1;
            right >>= 1;
        }
        let mut i = found.or_else(|| {
            rights[..met].iter().rev().copied().find(|&i| {
                step(1);
                self.mins[i] <= target
            })
        })?;
        // Down to the first block of it with such a depth.
        while i < self.blocks {
            i *= 2;
            step(1);
            if self.mins[i] > target {
                i += 1;
            }
        }
        Some(i - self.blocks)
    }
}

/// What the first pass of a scan has found in the blocks it has read, and the state
/// it reads the next one in.
struct FirstPass {
    /// The depth after the last byte read.
    depth: i64,
    /// Whether the next byte is escaped: right after an unescaped backslash.
    escaped: bool,
    /// Whether the line of the next byte holds a `%` before it.
    percent: bool,
    /// The depths at which strings opened at an escaped `(` have not ended yet, in
    /// rising order: each ends where the depth falls below it.
    escaped_opens: Vec<i64>,
    /// As [`Scan::meets`] and [`Scan::comment_ends`].
    meets: Bits,
    comment_ends: Vec<usize>,
}

impl FirstPass {
    /// Reads `data` into `strings`, with the kinds of each block from `kinds_of`.
    #[inline(always)]
    fn read(&mut self, data: &[u8], strings: &mut Strings, kinds_of: impl Fn(&[u8]) -> Kinds) {
        for (block, bytes) in data.chunks(BLOCK).enumerate() {
            strings.starts.push(self.depth);
            if self.escaped {
                strings.escaped.set(block);
            }
            strings.mins[strings.blocks + block] = self.block(block * BLOCK, kinds_of(bytes));
        }
    }

    /// Reads the block of the bytes from `from`, of `kinds`; returns the least depth
    /// after any of its bytes.
    ///
    /// A block is read as bits, not a byte at a time: in a stream, marks are random
    /// bytes, and branches on them, mispredicted, would cost the most of the pass.
    /// What is rare in any file, a string opened at an escaped `(` that has not
    /// ended, is read a mark at a time.
    #[inline(always)]
    fn block(&mut self, from: usize, kinds: Kinds) -> i64 {
        // The escaped bytes: those after an odd run of backslashes. Where a run
        // starts at an even bit, the run's bits plus its first bit carry past its
        // end, so the bit after it is flipped from the even bits that mark one after
        // a run starting at an odd bit (the escape scan of simdjson).
        const EVEN: u64 = 0x5555_5555_5555_5555;
        let before = u64::from(self.escaped);
        let backslash = kinds.backslash & !before;
        let follows = backslash << 1 | before;
        let odd_starts = backslash & !EVEN & !follows;
        let (even_runs, carried) = odd_starts.overflowing_add(backslash);
        let escaped = (EVEN ^ (even_runs << 1)) & follows;
        self.escaped = carried;

        let opens = kinds.open & !escaped;
        let closes = kinds.close & !escaped;
        let escaped_open = kinds.open & escaped;
        let start = self.depth;
        let after_first = start + i64::from(opens & 1 == 1) - i64::from(closes & 1 == 1);
        let mut least = after_first;
        if self.escaped_opens.is_empty() && escaped_open == 0 {
            // Only a `)` lowers the depth: the least is after the first byte or one.
            // The first `)` is read without a branch, as a block of a stream mostly
            // has none or one; with none, the bytes up to it are all, after which the
            // depth is no less than after the first.
            let depth_upto = |upto: u64| {
                start + i64::from((opens & upto).count_ones())
                    - i64::from((closes & upto).count_ones())
            };
            least = least.min(depth_upto(closes ^ closes.wrapping_sub(1)));
            let mut rest = closes & closes.wrapping_sub(1);
            while rest != 0 {
                least = least.min(depth_upto(rest ^ (rest - 1)));
                rest &= rest - 1;
            }
            self.depth = start + i64::from(opens.count_ones()) - i64::from(closes.count_ones());
        } else {
            let mut marks = opens | closes | escaped_open;
            while marks != 0 {
                let i = marks.trailing_zeros() as usize;
                marks &= marks - 1;
                let bit = 1 << i;
                if closes & bit != 0 {
                    if self.escaped_opens.last() == Some(&self.depth) {
                        self.escaped_opens.pop();
                        self.meets.set(from + i + 1);
                    }
                    self.depth -= 1;
                } else if opens & bit != 0 {
                    self.depth += 1;
                } else if self.escaped_opens.last() != Some(&self.depth) {
                    self.escaped_opens.push(self.depth);
                }
                least = least.min(self.depth);
            }
        }

        // The ends of lines holding a `%`: the first `%` or end of line after each
        // `%`, where it is an end of line. Adding a bit after each `%` to the bytes
        // that are neither carries it to the first that is one; a `%` before the
        // block carries in, and a `%` after the block's last end of line out.
        let gaps = !(kinds.percent | kinds.eol);
        let (sum, over) = gaps.overflowing_add(kinds.percent << 1);
        let (sum, carried) = sum.overflowing_add(u64::from(self.percent));
        self.percent = over | carried | (kinds.percent >> 63 == 1);
        let mut ends = sum & kinds.eol;
        while ends != 0 {
            let at = from + ends.trailing_zeros() as usize;
            ends &= ends - 1;
            self.meets.set(at);
            self.comment_ends.push(at);
        }
        least
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
    kept: hash::Map<usize, Outcome>,
    /// The outcomes of the arrays and dictionaries read on from a byte in `meets`,
    /// or from where one such closes, in a state.
    met: hash::Map<(usize, State), Outcome>,
    /// The last object the scan was asked about, and what it found: the scan that
    /// rebuilds a table asks about a header's object, then, where it is a dictionary,
    /// about it again for its keys.
    last: Option<(usize, Option<Found>)>,
}

impl<'a> Scan<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        let blocks = data.len().div_ceil(BLOCK).max(1);
        let mut strings = Strings {
            starts: Vec::with_capacity(blocks),
            escaped: Bits::new(blocks),
            mins: vec![0; 2 * blocks],
            blocks,
        };
        let mut pass = FirstPass {
            depth: 0,
            escaped: false,
            percent: false,
            escaped_opens: vec![],
            meets: Bits::new(data.len() + 1),
            comment_ends: vec![],
        };
        #[cfg(feature = "unsafe")]
        fearless_simd::dispatch!(fearless_simd::Level::new(), simd => pass.read(
            data,
            &mut strings,
            |bytes| match <&[u8; BLOCK]>::try_from(bytes) {
                Ok(block) => Kinds::of_block(simd, block),
                Err(_) => Kinds::of(bytes),
            },
        ));
        #[cfg(not(feature = "unsafe"))]
        pass.read(data, &mut strings, Kinds::of);
        if pass.percent {
            pass.comment_ends.push(data.len());
        }
        step(data.len());
        let mins = &mut strings.mins;
        for i in (1..blocks).rev() {
            mins[i] = mins[2 * i].min(mins[2 * i + 1]);
        }

        Self {
            data,
            strings,
            meets: pass.meets,
            comment_ends: pass.comment_ends,
            fails: Bits::new(data.len()),
            kept: hash::Map::default(),
            met: hash::Map::default(),
            last: None,
        }
    }

    /// What the scan finds of the object at `at`, after white space and comments,
    /// read as reading an object there does (`body::SlotReader::object` in the
    /// parent module): `None` where that fails.
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
                // reader.
                let mut r = Reader::new(self.data);
                r.jump(at);
                let read = r.read_with_context::<Object<'_>>(&ReaderContext::dummy());
                step(1);
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
        step(1);
        key.map(|key| (key, r.offset()))
    }

    /// Where what `skip` reads at `at` ends, if it reads one.
    fn skipped<T>(
        &self,
        at: usize,
        skip: impl FnOnce(&mut Reader<'a>) -> Option<T>,
    ) -> Option<usize> {
        let mut r = Reader::new(self.data);
        r.jump(at);
        let skipped = skip(&mut r);
        step(1);
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
                // As reading a dictionary reads it, a token in a key's place that is
                // no name is skipped as an object.
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
    use super::super::body::{Read, Slot};
    use super::super::{Checked, object_in, rebuilt_header};
    use super::*;
    use crate::object::Dict;
    use crate::object::ObjectIdentifier;
    use crate::object::indirect::IndirectObject;
    use crate::object::parsed::{Top, TopDict};
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
    /// names whose escapes are not hex, bad numbers, keywords,
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
    /// in a slot that ends at `slot` (`body::Read::in_slot`): where it ends, and for
    /// a dictionary whether it has a /Root and a /Type key.
    fn entry_read(data: &[u8], at: usize, slot: usize) -> Option<Found> {
        let ctx = ReaderContext::dummy();
        let (syntax, end) = Read::in_slot(
            data,
            at,
            Slot::to(slot),
            false,
            |r| TopDict::read(r, data, &ctx),
            |r| Top::read(r, data, &ctx),
        )
        .0?;
        let dict = match &syntax {
            Read::Dict(dict) | Read::Stream { dict, .. } => Some(dict.dict(data, &ctx)),
            Read::Object(_) => None,
        };
        let has = |key: &[u8]| dict.as_ref().is_some_and(|dict| dict.contains_key(key));
        Some(Found {
            end,
            root: has(ROOT),
            kind: has(TYPE),
        })
    }

    /// What reading the object at `at` of `data` after its header finds, as an
    /// entry's object is read in a slot that ends at `slot` (`body::Read::in_slot`),
    /// and, on all of `data`, as `XRef::get_with` reads one that is not a stream
    /// (`IndirectObject`), which agree: where the object ends.
    fn headed_read(data: &[u8], at: usize, slot: usize) -> Option<usize> {
        let ctx = ReaderContext::dummy();
        let syntax = Read::in_slot(
            data,
            at,
            Slot::to(slot),
            true,
            |r| TopDict::read(r, data, &ctx),
            |r| Top::read(r, data, &ctx),
        )
        .0;
        if slot == data.len() && !matches!(syntax, Some((Read::Stream { .. }, _))) {
            let mut indirect = Reader::new(data);
            indirect.jump(at);
            let read =
                indirect.read_with_context::<IndirectObject<Object<'_>>>(&ReaderContext::dummy());
            assert_eq!(read.is_some(), syntax.is_some(), "at {at}");
        }
        syntax.map(|(_, end)| end)
    }

    /// The header at `at` of `data` and whether its object is read, as rebuilding a
    /// table finds them (`rebuilt_header`).
    fn headed_rebuilt(data: &[u8], scan: &mut Scan<'_>, at: usize) -> Option<ObjectIdentifier> {
        let mut r = Reader::new(data);
        r.jump(at);
        let (header, object) = rebuilt_header(&mut r, scan).flatten()?;
        object.map(|_| header)
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

    /// Where the object at `at` of `data` ends as checking a table reads it, in a slot
    /// that ends at `slot`, which skips a dictionary.
    fn checked(data: &[u8], at: usize, slot: usize) -> Option<usize> {
        Read::in_slot(
            data,
            at,
            Slot::to(slot),
            false,
            |r| r.skip::<Dict<'_>>(false),
            |r| r.read_with_context::<Object<'_>>(&ReaderContext::dummy()),
        )
        .0
        .map(|(_, end)| end)
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

    #[test]
    fn the_tree_finds_the_first_block_with_a_depth_of_at_most_a_target() {
        let mut random = Random(0x2545_f491_4f6c_dd1d);
        for blocks in 1..=130 {
            let leaves: Vec<i64> = (0..blocks).map(|_| random.below(9) as i64).collect();
            let mut mins = vec![0; 2 * blocks];
            mins[blocks..].copy_from_slice(&leaves);
            for i in (1..blocks).rev() {
                mins[i] = mins[2 * i].min(mins[2 * i + 1]);
            }
            let strings = Strings {
                starts: vec![],
                escaped: Bits::new(0),
                mins,
                blocks,
            };
            for from in 0..blocks {
                for target in -1..9 {
                    assert_eq!(
                        strings.first_at_most(from, target),
                        (from..blocks).find(|&b| leaves[b] <= target),
                        "{blocks} blocks, from {from}, target {target}"
                    );
                }
            }
        }
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
            let read = entry_read(&data, at, data.len());
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
            // The check on the whole rest of the file, and in a slot up to an offset
            // where the next object may start.
            let whole = checked(&data, at, data.len());
            assert_eq!(
                whole,
                read.map(|read| read.end),
                "case {case}, check at {at} of {:?}",
                shown()
            );
            let next = at + random.below(data.len() - at + 1);
            let checked = checked(&data, at, next);
            assert_eq!(
                checked,
                entry_read(&data, at, next).map(|read| read.end),
                "case {case}, check at {at} to {next} of {:?}",
                shown()
            );
            // What reads to its end before that offset is read there, but a
            // dictionary after which a comment runs past the offset, or starts after
            // white space up to it and past it, as whether it is a stream's depends on
            // what follows the comment.
            if let Some(read) = read
                && read.end <= next
            {
                let dict = matches!(
                    Read::in_slot(
                        &data,
                        at,
                        Slot::to(data.len()),
                        false,
                        |r| r.skip::<Dict<'_>>(false),
                        |r| r.skip::<Object<'_>>(false),
                    )
                    .0,
                    Some((Read::Dict(_) | Read::Stream { .. }, _))
                );
                let depends = dict
                    && super::super::body::stream_follows(&data, read.end, Slot::to(next))
                        .is_none();
                if !depends {
                    assert_eq!(checked, Some(read.end), "case {case}");
                }
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
                let node = Top::read(&mut parse, &data, &ctx);
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
            let read = headed_read(&data, at, data.len());
            let mut whole = Reader::new(&data);
            whole.jump(at);
            let header = whole.read_without_context::<ObjectIdentifier>();
            // The header, and whether the object after it is read, or whether the
            // slot cuts the start of a header.
            let checked = |next: usize| match object_in(&data, at, next) {
                Checked::Object(header) => (Some(header), true, false),
                Checked::Unfinished(Some((header, _))) => (Some(header), false, false),
                Checked::Unfinished(None) => (None, false, true),
                Checked::Nothing => (None, false, false),
            };
            assert_eq!(
                checked(data.len()),
                (header, read.is_some() && header.is_some(), false),
                "case {case}, header check at {at} of {:?}",
                shown()
            );
            // In a slot, the header is the one read on all the bytes, if it ends in
            // the slot; one that does not is cut. Where there is none, the slot may
            // cut what starts like one.
            let (in_slot, object, cut) = checked(next);
            let what = || {
                format!(
                    "case {case}, header check at {at} to {next} of {:?}",
                    shown()
                )
            };
            match header {
                Some(header) if whole.offset() <= next => assert_eq!(
                    (in_slot, object, cut),
                    (Some(header), headed_read(&data, at, next).is_some(), false),
                    "{}",
                    what()
                ),
                Some(_) => assert_eq!((in_slot, object, cut), (None, false, true), "{}", what()),
                None => assert_eq!((in_slot, object), (None, false), "{}", what()),
            }
            assert_eq!(
                headed_rebuilt(&data, &mut scan, at),
                read.and(header),
                "case {case}, header scan at {at} of {:?}",
                shown()
            );
        }
    }

    #[test]
    fn the_first_pass_reads_the_bytes_that_open_close_or_end_strings_and_comments() {
        for b in 0..=u8::MAX {
            assert_eq!(
                is_mark(b),
                b"()\\%".contains(&b) || is_eol_character(b),
                "{b}"
            );
        }
        // A block's marks are the bits of the bytes that are marks, however many.
        let mut random = Random(0x2545_f491_4f6c_dd1d);
        for case in 0..20_000 {
            let mut block = [0_u8; BLOCK];
            for b in &mut block {
                let pick = |bytes: &[u8], random: &mut Random| bytes[random.below(bytes.len())];
                *b = match case % 3 {
                    0 => pick(b"()\\\r\n%a\x80", &mut random),
                    1 => random.below(256) as u8,
                    _ => pick(b"(\\", &mut random),
                };
            }
            let expected = (0..BLOCK)
                .filter(|i| is_mark(block[*i]))
                .fold(0_u64, |marks, i| marks | 1 << i);
            assert_eq!(marked(&block), expected, "{block:?}");
        }
    }

    /// What the first pass finds, read a byte at a time: the starts and escapes of
    /// the blocks, their least depths, the bytes in `meets`, and the comment ends.
    type Passed = (Vec<i64>, Vec<bool>, Vec<i64>, Vec<usize>, Vec<usize>);

    fn passed_byte_by_byte(data: &[u8]) -> Passed {
        let (mut starts, mut escapes, mut leasts) = (vec![], vec![], vec![]);
        let (mut meets, mut comment_ends) = (vec![], vec![]);
        let (mut depth, mut escaped, mut percent) = (0_i64, false, false);
        let mut escaped_opens: Vec<i64> = vec![];
        for (block, bytes) in data.chunks(BLOCK).enumerate() {
            starts.push(depth);
            escapes.push(escaped);
            let mut least = i64::MAX;
            for (i, &b) in bytes.iter().enumerate() {
                let at = block * BLOCK + i;
                if !escaped && b == b')' {
                    if escaped_opens.last() == Some(&depth) {
                        escaped_opens.pop();
                        meets.push(at + 1);
                    }
                    depth -= 1;
                } else if !escaped && b == b'(' {
                    depth += 1;
                } else if escaped && b == b'(' && escaped_opens.last() != Some(&depth) {
                    escaped_opens.push(depth);
                }
                least = least.min(depth);
                if is_eol_character(b) {
                    if percent {
                        meets.push(at);
                        comment_ends.push(at);
                    }
                    percent = false;
                } else if b == b'%' {
                    percent = true;
                }
                escaped = !escaped && b == b'\\';
            }
            leasts.push(least);
        }
        if percent {
            comment_ends.push(data.len());
        }
        meets.sort_unstable();
        meets.dedup();
        (starts, escapes, leasts, meets, comment_ends)
    }

    #[test]
    fn the_first_pass_finds_what_reading_a_byte_at_a_time_does() {
        let mut random = Random(0x9e37_79b9_7f4a_7c15);
        for case in 0..20_000 {
            let len = random.below(5 * BLOCK);
            let bytes: &[u8] = match case % 3 {
                0 => b"()\\\r\n%a",
                1 => b"(\\\\\\)a",
                _ => b"()\\%\n aaaaaaaaaaaaaaaaaaaaaa",
            };
            let data: Vec<u8> = (0..len).map(|_| bytes[random.below(bytes.len())]).collect();
            let scan = Scan::new(&data);
            let strings = &scan.strings;
            let blocks = data.len().div_ceil(BLOCK);
            let passed = (
                strings.starts.clone(),
                (0..blocks).map(|b| strings.escaped.get(b)).collect(),
                strings.mins[strings.blocks..][..blocks].to_vec(),
                (0..=data.len()).filter(|&at| scan.meets.get(at)).collect(),
                scan.comment_ends.clone(),
            );
            assert_eq!(
                passed,
                passed_byte_by_byte(&data),
                "{:?}",
                String::from_utf8_lossy(&data)
            );
        }
    }
}
